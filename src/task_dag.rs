//! 任务 DAG 的数据结构与构建期硬门禁（计划 §3.2 / §3.6，里程碑 M0）。
//!
//! `TaskGraph::build` 在图进入执行前完成全部校验：ID 唯一、依赖存在、
//! 无环、规模上限、递归防护。任何一项不满足都返回 `Err`，
//! 绝不静默截断或修正（D7/D8）。

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::tools::ToolEffects;

/// 最大拓扑层数（D8：用户指定）。
pub const MAX_DAG_DEPTH: usize = 100;
/// 最大节点总数。
pub const MAX_DAG_NODES: usize = 256;
/// 单层最大宽度。
pub const MAX_LAYER_WIDTH: usize = 64;
/// 节点禁止声明的工具名（防 `dag -> dag` 无限递归）。
pub const FORBIDDEN_NODE_TOOLS: &[&str] = &["dag"];

/// 强类型节点 ID：杜绝数字重号被静默覆盖。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskNodeId(u32);

impl TaskNodeId {
    /// 构造一个节点 ID。
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// 取出底层数值。
    #[must_use]
    pub const fn value(self) -> u32 {
        self.0
    }
}

/// 单个 DAG 节点：一次工具调用声明。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskNode {
    pub id: TaskNodeId,
    pub tool_name: String,
    /// 节点入参；省略时为 `Null`（无参工具无需声明）。
    #[serde(default)]
    pub args: serde_json::Value,
    /// 前置依赖；省略时为叶子节点。
    #[serde(default)]
    pub depends_on: Vec<TaskNodeId>,
    /// 由 tool registry 解析后的副作用集，不进 wire 格式。
    /// `ToolEffects` 无 `Default`，反序列化占位取 `read()`；
    /// M3 接入 registry 后会重新解析覆盖。
    #[serde(skip, default = "default_effects")]
    pub effects: ToolEffects,
}

const fn default_effects() -> ToolEffects {
    ToolEffects::read()
}

/// 节点执行状态机。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskNodeState {
    Pending,
    Running,
    Succeeded,
    Failed,
    /// 依赖失败/被取消导致未执行 —— 显式失败语义，不留悬空占位符。
    Skipped,
    Cancelled,
}

impl TaskNodeState {
    #[must_use]
    pub const fn is_pending(self) -> bool {
        matches!(self, Self::Pending)
    }

    #[must_use]
    pub const fn is_running(self) -> bool {
        matches!(self, Self::Running)
    }

    /// 是否处于不可逆终态。
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Skipped | Self::Cancelled
        )
    }

    #[must_use]
    pub const fn is_success(self) -> bool {
        matches!(self, Self::Succeeded)
    }

    #[must_use]
    pub const fn is_failure(self) -> bool {
        matches!(self, Self::Failed)
    }
}

/// 构建期校验失败。全部显式返回 `Err`，不 panic、不静默截断。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GraphError {
    #[error("duplicate node id {id:?}")]
    DuplicateId { id: TaskNodeId },

    #[error("node {node:?} depends on missing node {missing:?}")]
    MissingDependency {
        node: TaskNodeId,
        missing: TaskNodeId,
    },

    #[error("dependency cycle: {}", path.iter().map(|id| id.value().to_string()).collect::<Vec<_>>().join(" -> "))]
    Cycle { path: Vec<TaskNodeId> },

    #[error("graph depth {depth} exceeds max {max}")]
    TooDeep { depth: usize, max: usize },

    #[error("node count {count} exceeds max {max}")]
    TooManyNodes { count: usize, max: usize },

    #[error("layer {layer} width {width} exceeds max {max}")]
    LayerTooWide {
        layer: usize,
        width: usize,
        max: usize,
    },

    #[error("node {node:?} declares forbidden recursive tool")]
    RecursiveNode { node: TaskNodeId },

    #[error("node {node:?} declares unknown tool {tool:?}")]
    UnknownTool { node: TaskNodeId, tool: String },
}

/// 已通过全部构建期校验的任务图。
///
/// `layers` / `dependents` 在 `build` 时一次算好（Kahn 分层），
/// 调度器与前端直接消费，不再重复排序。
#[derive(Debug)]
pub struct TaskGraph {
    nodes: Vec<TaskNode>,
    /// 拓扑分层（构建期算好，供前端一次拿到 DAG 形状）。
    layers: Vec<Vec<TaskNodeId>>,
    /// adjacency：`a -> b` 表示 a 完成后 b 才就绪。
    dependents: HashMap<TaskNodeId, Vec<TaskNodeId>>,
}

impl TaskGraph {
    /// 构建并校验一张任务图。
    ///
    /// 校验顺序：ID 唯一 → 依赖存在 → 环检测（Kahn）→ 规模上限 → 递归防护。
    pub fn build(nodes: Vec<TaskNode>) -> Result<Self, GraphError> {
        // 1. ID 唯一。
        let mut id_set: HashSet<TaskNodeId> = HashSet::with_capacity(nodes.len());
        for node in &nodes {
            if !id_set.insert(node.id) {
                return Err(GraphError::DuplicateId { id: node.id });
            }
        }

        // 2. 依赖存在（悬空依赖直接拒绝）。
        for node in &nodes {
            for dep in &node.depends_on {
                if !id_set.contains(dep) {
                    return Err(GraphError::MissingDependency {
                        node: node.id,
                        missing: *dep,
                    });
                }
            }
        }

        // 3. Kahn 分层 + 环检测：一轮只摘出入度为 0 的节点作为一层，
        //    全部摘完则无环且得到拓扑分层。
        let (layers, processed) = kahn_layer(&nodes, &id_set);
        if processed < nodes.len() {
            // `layers` 只含已处理（无环）节点；未处理者即环上节点。
            let done: HashSet<TaskNodeId> = layers.iter().flatten().copied().collect();
            let remaining: HashSet<TaskNodeId> = id_set
                .iter()
                .copied()
                .filter(|id| !done.contains(id))
                .collect();
            let path = find_cycle_path(&remaining, &nodes)
                .unwrap_or_else(|| remaining.iter().copied().collect());
            return Err(GraphError::Cycle { path });
        }

        // 4. 规模硬上限（不做静默截断）。
        let depth = layers.len();
        if depth > MAX_DAG_DEPTH {
            return Err(GraphError::TooDeep {
                depth,
                max: MAX_DAG_DEPTH,
            });
        }
        let count = nodes.len();
        if count > MAX_DAG_NODES {
            return Err(GraphError::TooManyNodes {
                count,
                max: MAX_DAG_NODES,
            });
        }
        for (layer, ids) in layers.iter().enumerate() {
            if ids.len() > MAX_LAYER_WIDTH {
                return Err(GraphError::LayerTooWide {
                    layer,
                    width: ids.len(),
                    max: MAX_LAYER_WIDTH,
                });
            }
        }

        // 5. 递归防护：节点不得声明 `dag` 工具。
        for node in &nodes {
            if FORBIDDEN_NODE_TOOLS.contains(&node.tool_name.as_str()) {
                return Err(GraphError::RecursiveNode { node: node.id });
            }
        }

        // 合法图：构建邻接表（依赖边反向即就绪边）。
        let mut dependents: HashMap<TaskNodeId, Vec<TaskNodeId>> = HashMap::new();
        for node in &nodes {
            for dep in &node.depends_on {
                dependents.entry(*dep).or_default().push(node.id);
            }
        }

        Ok(Self {
            nodes,
            layers,
            dependents,
        })
    }

    /// 全部节点（输入顺序）。
    #[must_use]
    pub fn nodes(&self) -> &[TaskNode] {
        &self.nodes
    }

    /// 拓扑分层：同层节点互不依赖，可并行进入就绪集。
    #[must_use]
    pub fn layers(&self) -> &[Vec<TaskNodeId>] {
        &self.layers
    }

    /// 邻接表：`dep -> [下游节点]`。
    #[must_use]
    pub const fn dependents(&self) -> &HashMap<TaskNodeId, Vec<TaskNodeId>> {
        &self.dependents
    }

    /// 按 ID 查找节点。
    #[must_use]
    pub fn node(&self, id: TaskNodeId) -> Option<&TaskNode> {
        self.nodes.iter().find(|node| node.id == id)
    }

    /// 节点数。
    #[must_use]
    pub const fn len(&self) -> usize {
        self.nodes.len()
    }

    /// 图是否为空。
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

/// Kahn 分层：返回 `(每层节点, 成功处理的节点数)`。
/// 处理数 < 总数即存在环。
fn kahn_layer(nodes: &[TaskNode], id_set: &HashSet<TaskNodeId>) -> (Vec<Vec<TaskNodeId>>, usize) {
    let mut in_degree: HashMap<TaskNodeId, usize> = HashMap::with_capacity(nodes.len());
    let mut dependents: HashMap<TaskNodeId, Vec<TaskNodeId>> = HashMap::new();

    for node in nodes {
        in_degree.insert(node.id, node.depends_on.len());
        for dep in &node.depends_on {
            // 悬空依赖已在第 2 步拒绝，这里只需跳过自环的
            // map 初始化顺序问题（自环仍会正确保留在环集中）。
            if id_set.contains(dep) {
                dependents.entry(*dep).or_default().push(node.id);
            }
        }
    }

    let mut frontier: Vec<TaskNodeId> = in_degree
        .iter()
        .filter_map(|(id, &deg)| if deg == 0 { Some(*id) } else { None })
        .collect();
    // 稳定输出：同一层按节点输入顺序。
    let input_order: HashMap<TaskNodeId, usize> = nodes
        .iter()
        .enumerate()
        .map(|(idx, node)| (node.id, idx))
        .collect();
    frontier.sort_by_key(|id| input_order[id]);

    let mut layers: Vec<Vec<TaskNodeId>> = Vec::new();
    let mut processed = 0usize;

    while !frontier.is_empty() {
        let layer = std::mem::take(&mut frontier);
        processed += layer.len();

        let mut unlocked: Vec<TaskNodeId> = Vec::new();
        for id in &layer {
            if let Some(nexts) = dependents.get(id) {
                for next in nexts {
                    if let Some(deg) = in_degree.get_mut(next) {
                        *deg -= 1;
                        if *deg == 0 {
                            unlocked.push(*next);
                        }
                    }
                }
            }
        }
        unlocked.sort_by_key(|id| input_order[id]);
        frontier = unlocked;
        layers.push(layer);
    }

    // 空图合法：占一个空层，与计划 §3.2 的 `[[]]` 约定一致。
    if layers.is_empty() && nodes.is_empty() {
        layers.push(Vec::new());
    }

    (layers, processed)
}

/// 在剩余节点诱导子图中沿就绪边找一条环路径（DFS）。
/// 返回 `None` 仅表示理论上的意外情况，调用方会退回剩余节点列表。
fn find_cycle_path(remaining: &HashSet<TaskNodeId>, nodes: &[TaskNode]) -> Option<Vec<TaskNodeId>> {
    fn dfs(
        id: TaskNodeId,
        dependents: &HashMap<TaskNodeId, Vec<TaskNodeId>>,
        color: &mut HashMap<TaskNodeId, u8>,
        path: &mut Vec<TaskNodeId>,
    ) -> Option<Vec<TaskNodeId>> {
        color.insert(id, 1);
        path.push(id);
        if let Some(nexts) = dependents.get(&id) {
            for &next in nexts {
                match color.get(&next).copied().unwrap_or(0) {
                    1 => {
                        let start = path.iter().position(|&p| p == next)?;
                        return Some(path[start..].to_vec());
                    }
                    0 => {
                        if let Some(cycle) = dfs(next, dependents, color, path) {
                            return Some(cycle);
                        }
                    }
                    _ => {}
                }
            }
        }
        path.pop();
        color.insert(id, 2);
        None
    }

    let mut dependents: HashMap<TaskNodeId, Vec<TaskNodeId>> = HashMap::new();
    for node in nodes {
        if !remaining.contains(&node.id) {
            continue;
        }
        for dep in &node.depends_on {
            if remaining.contains(dep) {
                dependents.entry(*dep).or_default().push(node.id);
            }
        }
    }

    let mut color: HashMap<TaskNodeId, u8> = HashMap::new();
    let mut path: Vec<TaskNodeId> = Vec::new();

    let mut start: Vec<TaskNodeId> = remaining.iter().copied().collect();
    start.sort_by_key(|id| id.value());
    for id in start {
        if color.get(&id).copied().unwrap_or(0) == 0
            && let Some(cycle) = dfs(id, &dependents, &mut color, &mut path)
        {
            return Some(cycle);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(id: u32, deps: &[u32]) -> TaskNode {
        TaskNode {
            id: TaskNodeId::new(id),
            tool_name: "read".to_string(),
            args: json!({}),
            depends_on: deps.iter().copied().map(TaskNodeId::new).collect(),
            effects: ToolEffects::read(),
        }
    }

    fn layer_ids(graph: &TaskGraph) -> Vec<Vec<u32>> {
        graph
            .layers()
            .iter()
            .map(|layer| layer.iter().map(|id| id.value()).collect())
            .collect()
    }

    #[test]
    fn diamond_graph_layers_correctly() {
        // 1 -> {2, 3} -> 4
        let graph = TaskGraph::build(vec![
            node(1, &[]),
            node(2, &[1]),
            node(3, &[1]),
            node(4, &[2, 3]),
        ])
        .expect("diamond graph must be valid");

        assert_eq!(layer_ids(&graph), vec![vec![1], vec![2, 3], vec![4]]);
        assert_eq!(graph.len(), 4);
        assert_eq!(
            graph.dependents().get(&TaskNodeId::new(1)).unwrap().len(),
            2
        );
        assert!(graph.node(TaskNodeId::new(3)).is_some());
        assert!(graph.node(TaskNodeId::new(99)).is_none());
    }

    #[test]
    fn empty_and_single_node_graphs_are_valid() {
        let empty = TaskGraph::build(vec![]).expect("empty graph must be valid");
        assert!(empty.is_empty());
        assert_eq!(empty.layers().len(), 1);
        assert!(empty.layers()[0].is_empty());

        let single = TaskGraph::build(vec![node(7, &[])]).expect("single node valid");
        assert_eq!(layer_ids(&single), vec![vec![7]]);
    }

    #[test]
    fn duplicate_id_is_rejected() {
        let result = TaskGraph::build(vec![node(1, &[]), node(1, &[])]);
        assert!(matches!(
            result,
            Err(GraphError::DuplicateId { id }) if id == TaskNodeId::new(1)
        ));
    }

    #[test]
    fn missing_dependency_is_rejected() {
        let result = TaskGraph::build(vec![node(1, &[42])]);
        assert!(matches!(
            result,
            Err(GraphError::MissingDependency { node, missing })
                if node == TaskNodeId::new(1) && missing == TaskNodeId::new(42)
        ));
    }

    #[test]
    fn self_cycle_is_rejected() {
        let result = TaskGraph::build(vec![node(1, &[1])]);
        match result {
            Err(GraphError::Cycle { path }) => assert_eq!(path, vec![TaskNodeId::new(1)]),
            other => panic!("expected Cycle, got {other:?}"),
        }
    }

    #[test]
    fn multi_node_cycle_is_rejected() {
        // 1 -> 2 -> 3 -> 1（depends_on 方向成环）
        let result = TaskGraph::build(vec![node(1, &[3]), node(2, &[1]), node(3, &[2])]);
        match result {
            Err(GraphError::Cycle { path }) => {
                assert_eq!(path.len(), 3);
                assert!(path.contains(&TaskNodeId::new(1)));
                assert!(path.contains(&TaskNodeId::new(2)));
                assert!(path.contains(&TaskNodeId::new(3)));
            }
            other => panic!("expected Cycle, got {other:?}"),
        }
    }

    #[test]
    fn depth_101_is_too_deep() {
        // 101 层链：1 <- 2 <- ... <- 101
        let nodes: Vec<TaskNode> = (1..=101)
            .map(|id| {
                if id == 1 {
                    node(id, &[])
                } else {
                    node(id, &[id - 1])
                }
            })
            .collect();
        let result = TaskGraph::build(nodes);
        assert!(matches!(
            result,
            Err(GraphError::TooDeep {
                depth: 101,
                max: 100
            })
        ));
    }

    #[test]
    fn node_count_257_is_too_many_nodes() {
        let nodes: Vec<TaskNode> = (1..=257).map(|id| node(id, &[])).collect();
        let result = TaskGraph::build(nodes);
        assert!(matches!(
            result,
            Err(GraphError::TooManyNodes {
                count: 257,
                max: 256
            })
        ));
    }

    #[test]
    fn layer_width_65_is_too_wide() {
        // 单层 65 个互不依赖的节点（节点数 65 未超 256）。
        let nodes: Vec<TaskNode> = (1..=65).map(|id| node(id, &[])).collect();
        let result = TaskGraph::build(nodes);
        assert!(matches!(
            result,
            Err(GraphError::LayerTooWide {
                layer: 0,
                width: 65,
                max: 64
            })
        ));
    }

    #[test]
    fn dag_tool_node_is_recursive_and_rejected() {
        let mut recursive = node(1, &[]);
        recursive.tool_name = "dag".to_string();
        let result = TaskGraph::build(vec![recursive]);
        assert!(matches!(
            result,
            Err(GraphError::RecursiveNode { node }) if node == TaskNodeId::new(1)
        ));
    }

    #[test]
    fn state_predicates_match_variants() {
        assert!(TaskNodeState::Pending.is_pending());
        assert!(TaskNodeState::Running.is_running());
        assert!(TaskNodeState::Succeeded.is_terminal());
        assert!(TaskNodeState::Failed.is_terminal());
        assert!(TaskNodeState::Skipped.is_terminal());
        assert!(TaskNodeState::Cancelled.is_terminal());
        assert!(!TaskNodeState::Pending.is_terminal());
        assert!(TaskNodeState::Succeeded.is_success());
        assert!(TaskNodeState::Failed.is_failure());
    }

    #[test]
    fn task_node_wire_format_skips_effects() {
        let raw = serde_json::to_value(node(1, &[])).expect("serialize node");
        assert!(raw.get("effects").is_none());
        assert_eq!(raw.get("toolName"), Some(&json!("read")));
        assert_eq!(raw.get("dependsOn"), Some(&json!([])));

        let parsed: TaskNode = serde_json::from_value(raw).expect("deserialize node");
        assert_eq!(parsed.id, TaskNodeId::new(1));
        assert!(parsed.effects.parallel_safe());
    }
}
