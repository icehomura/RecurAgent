//! `dag` 工具（host-coupled）：模型在一次调用中给出完整图，
//! 本实例内由 [`DagScheduler`] 三层调度执行——无子进程、无新实例。
//!
//! 结果经 [`ToolOutput`] → `record_tool_result` 回传主 Agent（D9），
//! 失败/跳过节点整理在聚合输出里，由主 Agent 决定下一步。
//!
//! **修复回路（resume + patch）**：每次调用把图（节点定义 + 已成功输出）存进
//! 会话级 [`GraphStore`]。图跑完若有失败，模型可再发一次 `dag` 调用，带
//! `resume: "<graphId>"` 与 `patch` 增删改节点/连线；**已 Succeeded 且定义
//! 未变的节点直接复用旧输出，不重跑**，只有被改/新增/失败节点及其下游会执行。

use std::collections::{HashMap, HashSet, VecDeque};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::dag_scheduler::{DagScheduler, NodeExecutor, ScheduleError, TaskOutputStore, text_of};
use crate::error::{Error, Result};
use crate::model::{ContentBlock, TextContent};
use crate::task_dag::{
    FORBIDDEN_NODE_TOOLS, MAX_DAG_DEPTH, MAX_DAG_NODES, MAX_LAYER_WIDTH, TaskGraph, TaskNode,
    TaskNodeId, TaskNodeState,
};
use crate::tools::{SharedToolRegistry, SharedToolRegistryInner, Tool, ToolOutput, ToolUpdate};

/// Shared per-call emit plumbing for the three `ra.dag.*` schemas.
type DagEmit = Arc<dyn Fn(ToolUpdate) + Send + Sync>;

/// One retained graph: node definitions (post-patch) plus the outputs of nodes
/// that already succeeded **under the current definition**.
#[derive(Debug, Default, Clone)]
struct StoredGraph {
    nodes: Vec<TaskNode>,
    outputs: HashMap<TaskNodeId, Arc<ToolOutput>>,
}

/// Session-level `graphId → graph` table backing `resume`.
///
/// Bounded FIFO: the oldest graph is evicted once `keep` is exceeded, so a
/// long session cannot grow the table without bound.
#[derive(Debug, Default)]
struct GraphStore {
    order: VecDeque<String>,
    by_id: HashMap<String, StoredGraph>,
    /// Max retained graphs (from `dag.keepGraphs`).
    keep: usize,
}

impl GraphStore {
    fn with_capacity(keep: usize) -> Self {
        Self {
            order: VecDeque::new(),
            by_id: HashMap::new(),
            keep: keep.max(1),
        }
    }

    /// Insert or replace a graph, evicting the oldest beyond `keep`.
    fn put(&mut self, id: String, graph: StoredGraph) {
        if self.by_id.insert(id.clone(), graph).is_none() {
            self.order.push_back(id);
        }
        while self.order.len() > self.keep {
            if let Some(oldest) = self.order.pop_front() {
                self.by_id.remove(&oldest);
            }
        }
    }

    fn get(&self, id: &str) -> Option<&StoredGraph> {
        self.by_id.get(id)
    }
}

/// `dag.patch`: structural edits applied to a resumed graph.
///
/// Every list is optional (`#[serde(default)]`), so a patch may add nodes
/// without touching edges, or rewire edges without touching nodes.
#[derive(Debug, Default, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct DagPatch {
    /// Add a node, or replace the definition of an existing id.
    upsert: Vec<TaskNode>,
    /// Delete nodes by id. Deleting a node also drops every edge into it.
    remove: Vec<u32>,
    /// Add an edge `(child, parent)` — `parent` must finish before `child`.
    add_depends_on: Vec<(u32, u32)>,
    /// Remove the edge `(child, parent)`.
    remove_depends_on: Vec<(u32, u32)>,
}

/// What a [`DagPatch`] changed.
///
/// Two distinct questions, deliberately separated:
/// - [`Self::dirty`] — whose **execution** is invalidated (tool / args / edges
///   changed, or a dependency was removed). These re-run, along with every
///   transitive dependent.
/// - [`Self::touched`] — whose **definition** changed at all, including a
///   display-only rename. These need a `node_update.v1` so the frontend
///   redraws the box, but a rename alone must not re-execute anything.
#[derive(Debug, Default)]
struct PatchEffect {
    dirty: HashSet<u32>,
    touched: HashSet<u32>,
}

/// Apply `patch` to `nodes` in place; report what changed.
///
/// Uses **camelCase JSON keys** and validates that edge endpoints exist, so a
/// typo fails loudly instead of silently producing a graph that ignores it.
fn apply_patch(nodes: &mut Vec<TaskNode>, patch: &DagPatch) -> Result<PatchEffect> {
    let mut effect = PatchEffect::default();

    if !patch.remove.is_empty() {
        let removed: HashSet<u32> = patch.remove.iter().copied().collect();
        for id in &removed {
            effect.touched.insert(*id);
        }
        nodes.retain(|node| !removed.contains(&node.id.value()));
        // Deleting a node drops every edge into it — a dangling `depends_on`
        // would be rejected by `TaskGraph::build`. A node that lost a
        // dependency is dirty: its inputs changed, so it must re-run rather
        // than reuse an output computed against the deleted parent.
        for node in nodes.iter_mut() {
            let before = node.depends_on.len();
            node.depends_on
                .retain(|dep| !removed.contains(&dep.value()));
            if node.depends_on.len() != before {
                effect.dirty.insert(node.id.value());
                effect.touched.insert(node.id.value());
            }
        }
    }

    for upsert in &patch.upsert {
        let id = upsert.id.value();
        if let Some(existing) = nodes.iter_mut().find(|node| node.id.value() == id) {
            let exec_changed = existing.tool_name != upsert.tool_name
                || existing.args != upsert.args
                || existing.depends_on != upsert.depends_on;
            let view_changed = exec_changed || existing.name != upsert.name;
            if view_changed {
                *existing = upsert.clone();
                effect.touched.insert(id);
            }
            if exec_changed {
                effect.dirty.insert(id);
            }
        } else {
            nodes.push(upsert.clone());
            effect.dirty.insert(id);
            effect.touched.insert(id);
        }
    }

    for &(child, parent) in &patch.add_depends_on {
        let Some(node) = nodes.iter_mut().find(|node| node.id.value() == child) else {
            return Err(Error::validation(format!(
                "patch.addDependsOn references unknown child node {child}"
            )));
        };
        let parent_id = TaskNodeId::new(parent);
        if !node.depends_on.contains(&parent_id) {
            node.depends_on.push(parent_id);
            effect.dirty.insert(child);
            effect.touched.insert(child);
        }
    }

    for &(child, parent) in &patch.remove_depends_on {
        let Some(node) = nodes.iter_mut().find(|node| node.id.value() == child) else {
            return Err(Error::validation(format!(
                "patch.removeDependsOn references unknown child node {child}"
            )));
        };
        let parent_id = TaskNodeId::new(parent);
        let before = node.depends_on.len();
        node.depends_on.retain(|dep| *dep != parent_id);
        if node.depends_on.len() != before {
            effect.dirty.insert(child);
            effect.touched.insert(child);
        }
    }

    Ok(effect)
}

/// Expand `dirty` to every transitive dependent: changing a node invalidates
/// everything that consumed it, since their inputs changed.
fn dirty_closure(nodes: &[TaskNode], dirty: &HashSet<u32>) -> HashSet<u32> {
    let mut closure = dirty.clone();
    loop {
        let mut grew = false;
        for node in nodes {
            let id = node.id.value();
            if closure.contains(&id) {
                continue;
            }
            if node
                .depends_on
                .iter()
                .any(|dep| closure.contains(&dep.value()))
            {
                closure.insert(id);
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    closure
}

/// 节点执行器：走本实例 `ToolRegistry` 快照调用已注册工具。
struct RegistryExecutor {
    registry: SharedToolRegistry,
    dag_call_id: String,
    /// 上游 dag 调用的进度回调；节点工具的流式更新转发为 `ra.dag.node_output.v1`。
    on_update: Option<DagEmit>,
    /// state/output 共享的单调序号。
    seq: Arc<AtomicU64>,
}

impl NodeExecutor for RegistryExecutor {
    fn execute(
        &self,
        node: TaskNode,
        resolved_args: Value,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<ToolOutput, String>> + Send>> {
        let registry = self.registry.clone();
        let dag_call_id = self.dag_call_id.clone();
        let node_id = node.id.value();
        let on_update = self.on_update.clone();
        let seq = Arc::clone(&self.seq);
        Box::pin(async move {
            // 快照持有整个查找 + 执行期，保证工具句柄存活（snapshot 文档约定）。
            let snapshot = registry.snapshot();
            let tool = snapshot.get(&node.tool_name).ok_or_else(|| {
                format!(
                    "dag node {} references unknown tool `{}`",
                    node.id.value(),
                    node.tool_name
                )
            })?;
            let node_call_id = format!("{dag_call_id}#n{node_id}");
            // 节点工具的增量输出 → dag 的 node_output 消息（带共享 seq）。
            let forward: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>> =
                on_update.as_ref().map(|emit| {
                    let emit = Arc::clone(emit);
                    let seq = Arc::clone(&seq);
                    let graph_id = dag_call_id.clone();
                    Box::new(move |update: ToolUpdate| {
                        let text = update
                            .content
                            .iter()
                            .filter_map(|block| match block {
                                ContentBlock::Text(t) => Some(t.text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        if text.is_empty() {
                            return;
                        }
                        let n = seq.fetch_add(1, Ordering::SeqCst);
                        emit(ToolUpdate {
                            content: vec![],
                            details: Some(json!({
                                "schema": "ra.dag.node_output.v1",
                                "graphId": graph_id,
                                "nodeId": node_id,
                                "delta": text,
                                "seq": n,
                            })),
                        });
                    }) as Box<dyn Fn(ToolUpdate) + Send + Sync>
                });
            tool.execute(&node_call_id, resolved_args, forward)
                .await
                .map_err(|err| format!("dag node {} failed: {err}", node.id.value()))
        })
    }
}

/// 对外是一个普通工具，对内是三层调度器（计划 §3.5）。
///
/// 持有 registry 的 `Weak`：`extend_tools` 会把本工具发布进同一 registry，
/// 强引用会形成 Arc 环（registry → tool → registry）。
pub struct DagTool {
    registry: std::sync::Weak<SharedToolRegistryInner>,
    /// `dag` 段配置（重试/并发/保留图数）。默认值即 `DagSettings::default()`。
    settings: crate::config::DagSettings,
    /// 会话级图表：`graphId → {nodes, outputs}`，供 `resume` 复用。
    graphs: Arc<Mutex<GraphStore>>,
}

impl DagTool {
    #[must_use]
    pub fn new(registry: &SharedToolRegistry) -> Self {
        Self {
            registry: registry.downgrade(),
            settings: crate::config::DagSettings::default(),
            graphs: Arc::new(Mutex::new(GraphStore::default())),
        }
    }

    /// 注入 `dag` 段配置（`settings.json` 的 `dag` 键）。`None` → 全默认。
    #[must_use]
    pub fn with_settings(mut self, settings: Option<crate::config::DagSettings>) -> Self {
        self.settings = settings.unwrap_or_default();
        self.graphs = Arc::new(Mutex::new(GraphStore::with_capacity(
            self.settings.keep_graphs(),
        )));
        self
    }
}

#[async_trait::async_trait]
impl Tool for DagTool {
    fn name(&self) -> &'static str {
        "dag"
    }

    fn label(&self) -> &'static str {
        "dag"
    }

    fn description(&self) -> &'static str {
        "Execute a dependency DAG of tool calls in parallel within this session. \
         Provide the complete graph in one call: each node has `id` (u32), `toolName`, \
         optional `args`, and optional `dependsOn` (ids that must succeed first). An \
         optional `name` gives a step a short human label. Independent nodes run \
         concurrently (bounded by the configured limit); tools with write/append/process \
         side effects are serialized automatically. Read-only nodes that fail are retried \
         automatically; write/append/process nodes are NEVER retried, because replaying \
         them would duplicate the side effect. When a node still fails, its downstream \
         nodes are skipped and the whole report — including the `graphId` — is returned. \
         To repair, call `dag` again with `resume: \"<graphId>\"` and a `patch` \
         (upsert/remove nodes, addDependsOn/removeDependsOn edges): already-succeeded \
         nodes whose definition is unchanged are reused, not re-run. Nodes may not \
         invoke `dag` itself."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "nodes": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["id", "toolName"],
                        "properties": {
                            "id": {"type": "integer", "minimum": 0},
                            "toolName": {"type": "string"},
                            "name": {"type": "string"},
                            "args": {},
                            "dependsOn": {
                                "type": "array",
                                "items": {"type": "integer", "minimum": 0}
                            }
                        },
                        "additionalProperties": false
                    }
                },
                "resume": {
                    "type": "string",
                    "description": "graphId returned by a previous dag call; resume and \
                                    patch that graph instead of creating a new one."
                },
                "patch": {
                    "type": "object",
                    "description": "Structural edits to a resumed graph. Requires `resume`.",
                    "properties": {
                        "upsert": {
                            "type": "array",
                            "description": "Add a node, or replace an existing id's tool/args/edges.",
                            "items": {
                                "type": "object",
                                "required": ["id", "toolName"],
                                "properties": {
                                    "id": {"type": "integer", "minimum": 0},
                                    "toolName": {"type": "string"},
                                    "name": {"type": "string"},
                                    "args": {},
                                    "dependsOn": {
                                        "type": "array",
                                        "items": {"type": "integer", "minimum": 0}
                                    }
                                },
                                "additionalProperties": false
                            }
                        },
                        "remove": {
                            "type": "array",
                            "description": "Delete nodes by id.",
                            "items": {"type": "integer", "minimum": 0}
                        },
                        "addDependsOn": {
                            "type": "array",
                            "description": "Add edges as [child, parent] pairs.",
                            "items": {
                                "type": "array",
                                "items": {"type": "integer", "minimum": 0}
                            }
                        },
                        "removeDependsOn": {
                            "type": "array",
                            "description": "Remove edges as [child, parent] pairs.",
                            "items": {
                                "type": "array",
                                "items": {"type": "integer", "minimum": 0}
                            }
                        }
                    },
                    "additionalProperties": false
                }
            },
            "additionalProperties": false
        })
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        input: Value,
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        // 被 await 的 future 只持有 owned 数据、不借用 `&self`：
        // 绕开 nightly async_trait 的 rust-lang/rust#100013 误报。
        let weak = self.registry.clone();
        let settings = self.settings.clone();
        let graphs = Arc::clone(&self.graphs);
        let call_id = tool_call_id.to_string();
        Self::run_dag(weak, settings, graphs, call_id, input, on_update).await
    }
    // effects() 走默认 write()：dag 自身是 barrier，主循环天然串行它。
}

impl DagTool {
    /// 实际执行逻辑：inherent async fn + owned 参数（见 `execute` 内注释）。
    ///
    /// Long by necessity: it is one linear pipeline (assemble → patch → resolve
    /// effects → build → seed → run → aggregate → store) whose stages share a
    /// lot of local state; splitting them would thread eight parameters through
    /// each helper for no readability gain.
    #[allow(clippy::too_many_lines)]
    async fn run_dag(
        weak: std::sync::Weak<SharedToolRegistryInner>,
        settings: crate::config::DagSettings,
        graphs: Arc<Mutex<GraphStore>>,
        tool_call_id: String,
        input: Value,
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let on_update: Option<DagEmit> = on_update.map(Arc::from);
        let seq = Arc::new(AtomicU64::new(0));

        // 1. 组装节点定义：`resume` 从会话表取回旧图再打 patch；否则用 `nodes`。
        let resume_id = input
            .get("resume")
            .and_then(Value::as_str)
            .map(str::to_string);
        if input.get("patch").is_some() && resume_id.is_none() {
            return Err(Error::validation("dag `patch` requires `resume`"));
        }
        // Every event this call emits keys on the **graph** id, not the tool-call
        // id: a resume must fold back into the card the original call opened
        // (same key ⇒ in-place redraw), so the stable id is the resumed one.
        let graph_id = resume_id.clone().unwrap_or_else(|| tool_call_id.clone());

        // 1. 组装节点定义：`resume` 从会话表取回旧图再打 patch；否则用 `nodes`。
        let mut nodes: Vec<TaskNode> = if let Some(graph_id) = &resume_id {
            let store = graphs
                .lock()
                .map_err(|_| Error::tool("dag", "graph store poisoned"))?;
            let stored = store.get(graph_id).ok_or_else(|| {
                Error::validation(format!(
                    "dag resume: unknown or evicted graphId `{graph_id}`"
                ))
            })?;
            // Clone out and release the lock before any further work: the
            // store is shared across dag calls and must not be held across
            // the whole pipeline.
            let nodes = stored.nodes.clone();
            drop(store);
            nodes
        } else {
            let nodes_value = input
                .get("nodes")
                .cloned()
                .ok_or_else(|| Error::validation("dag requires a `nodes` array (or `resume`)"))?;
            serde_json::from_value(nodes_value)
                .map_err(|err| Error::validation(format!("dag nodes are malformed: {err}")))?
        };

        // 2. 结构性修补（仅 resume 允许）。`dirty` 驱动重跑，`touched` 驱动视图刷新。
        let effect = if let Some(value) = input.get("patch") {
            let patch: DagPatch = serde_json::from_value(value.clone())
                .map_err(|err| Error::validation(format!("dag patch is malformed: {err}")))?;
            apply_patch(&mut nodes, &patch)?
        } else {
            PatchEffect::default()
        };
        let (dirty, touched) = (effect.dirty, effect.touched);
        if nodes.is_empty() {
            return Err(Error::validation("dag graph has no nodes"));
        }

        // 3. 递归防护优先（明确报错），再从 registry 解析副作用集。
        let registry = SharedToolRegistry::upgrade(&weak)
            .ok_or_else(|| Error::tool("dag", "tool registry has been dropped"))?;
        let snapshot = registry.snapshot();
        for node in &mut nodes {
            if FORBIDDEN_NODE_TOOLS.contains(&node.tool_name.as_str()) {
                return Err(Error::validation(format!(
                    "dag node {} may not invoke the `{}` tool (recursive dag)",
                    node.id.value(),
                    node.tool_name
                )));
            }
            let tool = snapshot.get(&node.tool_name).ok_or_else(|| {
                Error::validation(format!(
                    "dag node {} references unknown tool `{}`",
                    node.id.value(),
                    node.tool_name
                ))
            })?;
            node.effects = tool.effects();
        }

        // 4. 构建期硬门禁：ID 唯一 / 依赖存在 / 环 / 深度 100 / 节点数 / 层宽。
        let node_meta: Vec<TaskNode> = nodes.clone();
        let graph = TaskGraph::build(nodes)
            .map_err(|err| Error::validation(format!("dag graph rejected: {err}")))?;

        // 5. resume 播种：变更节点及其**全部下游**都要重跑（输入变了）；
        //    只有「定义未变且上次 Succeeded」的节点复用旧输出。
        let (seed_outputs, seed_succeeded) = if let Some(graph_id) = &resume_id {
            let closure = dirty_closure(&node_meta, &dirty);
            let mut outputs = Vec::new();
            let mut succeeded = Vec::new();
            {
                let store = graphs
                    .lock()
                    .map_err(|_| Error::tool("dag", "graph store poisoned"))?;
                if let Some(stored) = store.get(graph_id) {
                    for node in &node_meta {
                        if closure.contains(&node.id.value()) {
                            continue;
                        }
                        if let Some(output) = stored.outputs.get(&node.id) {
                            outputs.push((node.id, Arc::clone(output)));
                            succeeded.push(node.id);
                        }
                    }
                }
                // Release the shared lock before the scheduler runs.
                drop(store);
            }
            (outputs, succeeded)
        } else {
            (Vec::new(), Vec::new())
        };
        let reused: HashSet<u32> = seed_succeeded.iter().map(|id| id.value()).collect();

        // id → 拓扑层号（node_state 消息需要；在 graph 移交 scheduler 前算好）。
        let layer_of: HashMap<TaskNodeId, usize> = graph
            .layers()
            .iter()
            .enumerate()
            .flat_map(|(layer, ids)| ids.iter().map(move |id| (*id, layer)))
            .collect();

        // 首帧即拓扑（M4：ra.dag.topology.v1），保证前端先拿到图形状。
        // resume 复用同一 graphId → 前端就地重画那张卡片，不新开卡。
        if let Some(emit) = &on_update {
            emit_topology(emit, &graph_id, &graph, &layer_of);
            // Patch 改过的节点再逐条发 node_update：支持"只改那个方框"的
            // 增量前端，不必整图重建。**视图变化**（含纯改名）都发，但只有
            // 执行相关的变化才进 `dirty`、才会重跑。
            for node in &node_meta {
                if !touched.contains(&node.id.value()) {
                    continue;
                }
                let n = seq.fetch_add(1, Ordering::SeqCst);
                emit(ToolUpdate {
                    content: vec![],
                    details: Some(json!({
                        "schema": "ra.dag.node_update.v1",
                        "graphId": graph_id,
                        "nodeId": node.id.value(),
                        "name": node.name,
                        "toolName": node.tool_name,
                        "dependsOn": node.depends_on.iter().map(|d| d.value()).collect::<Vec<_>>(),
                        "layer": layer_of.get(&node.id).copied().unwrap_or(0),
                        "revision": n,
                    })),
                });
            }
            // 被复用的节点不再触发 state 事件，直接补发终态。
            for id in &seed_succeeded {
                let n = seq.fetch_add(1, Ordering::SeqCst);
                emit(ToolUpdate {
                    content: vec![],
                    details: Some(json!({
                        "schema": "ra.dag.node_state.v1",
                        "graphId": graph_id,
                        "nodeId": id.value(),
                        "state": "succeeded",
                        "seq": n,
                        "layer": layer_of.get(id).copied().unwrap_or(0),
                        "reused": true,
                    })),
                });
            }
        }

        // 6. 本实例三层调度执行。
        let executor = Arc::new(RegistryExecutor {
            registry,
            dag_call_id: graph_id.clone(),
            on_update: on_update.clone(),
            seq: Arc::clone(&seq),
        });
        let mut scheduler = DagScheduler::with_default_concurrency(graph, executor)
            .with_retry(settings.retry_attempts(), settings.retry_backoff())
            .with_seed(seed_outputs, &seed_succeeded);
        // 节点状态迁移 → ra.dag.node_state.v1（低频，同样带共享 seq）。
        {
            let emit = on_update.clone();
            let seq = Arc::clone(&seq);
            let graph_id = graph_id.clone();
            let layer_of = layer_of.clone();
            scheduler = scheduler.on_state(move |id, state| {
                let Some(emit) = &emit else { return };
                let n = seq.fetch_add(1, Ordering::SeqCst);
                emit(ToolUpdate {
                    content: vec![],
                    details: Some(json!({
                        "schema": "ra.dag.node_state.v1",
                        "graphId": graph_id,
                        "nodeId": id.value(),
                        "state": state,
                        "seq": n,
                        "layer": layer_of.get(&id).copied().unwrap_or(0),
                    })),
                });
            });
        }
        if let Err(ScheduleError::Stalled { pending }) = scheduler.run().await {
            return Err(Error::validation(format!(
                "dag scheduler stalled with {pending} pending nodes"
            )));
        }

        // 7. 聚合 per-node 报告（在消费 scheduler 之前收集状态与失败原因）。
        let report: Vec<(TaskNodeId, String, Option<TaskNodeState>, Option<String>)> = node_meta
            .iter()
            .map(|node| {
                (
                    node.id,
                    node.tool_name.clone(),
                    scheduler.state(node.id),
                    scheduler.failure(node.id).map(str::to_string),
                )
            })
            .collect();
        let outputs = scheduler.into_outputs();

        // 8. 存回会话表：图定义 + 本轮成功的输出（含复用），供后续 resume。
        //    只有 Succeeded 才有可复用的输出；Failed/Skipped 不能污染输出表。
        //    resume 时沿用**原 graphId**，这样反复修复都用同一个 id。
        let store_key = graph_id.clone();
        let succeeded_ids: HashSet<TaskNodeId> = report
            .iter()
            .filter(|(_, _, state, _)| *state == Some(TaskNodeState::Succeeded))
            .map(|(id, _, _, _)| *id)
            .collect();
        let stored_outputs: HashMap<TaskNodeId, Arc<ToolOutput>> = node_meta
            .iter()
            .filter(|node| succeeded_ids.contains(&node.id))
            .filter_map(|node| outputs.get(node.id).map(|out| (node.id, Arc::clone(out))))
            .collect();
        if let Ok(mut store) = graphs.lock() {
            store.put(
                store_key.clone(),
                StoredGraph {
                    nodes: node_meta.clone(),
                    outputs: stored_outputs,
                },
            );
        }

        Ok(aggregate(&store_key, &report, &outputs, &reused))
    }
}

/// 首帧拓扑：图形状一次下发，`layers` 让前端不必自算拓扑排序。
fn emit_topology(
    emit: &DagEmit,
    graph_id: &str,
    graph: &TaskGraph,
    layer_of: &HashMap<TaskNodeId, usize>,
) {
    emit(ToolUpdate {
        content: vec![],
        details: Some(json!({
            "schema": "ra.dag.topology.v1",
            "graphId": graph_id,
            "nodes": graph.nodes().iter().map(|n| json!({
                "id": n.id.value(),
                "toolName": n.tool_name,
                "name": n.name,
                "dependsOn": n.depends_on.iter().map(|d| d.value()).collect::<Vec<_>>(),
                "layer": layer_of.get(&n.id).copied().unwrap_or(0),
            })).collect::<Vec<_>>(),
            "layers": graph.layers().iter().map(|l|
                l.iter().map(|id| id.value()).collect::<Vec<_>>()
            ).collect::<Vec<_>>(),
            "maxDepth": MAX_DAG_DEPTH,
            "limits": {"nodes": MAX_DAG_NODES, "layerWidth": MAX_LAYER_WIDTH},
        })),
    });
}

/// D9：每节点状态/失败原因/输出聚合成一份 `ToolOutput`；
/// 存在失败/跳过 → `is_error=true`，由主 Agent 决策下一步。
///
/// `graphId` 与失败节点一并回传，模型据此发起 `resume` + `patch` 修复；
/// `reused` 标记从会话表复用、本轮**没有重跑**的节点。
fn aggregate(
    graph_id: &str,
    report: &[(TaskNodeId, String, Option<TaskNodeState>, Option<String>)],
    outputs: &TaskOutputStore,
    reused: &HashSet<u32>,
) -> ToolOutput {
    let mut lines = Vec::new();
    let mut node_results = Vec::new();
    let mut failed = 0usize;
    let mut skipped = 0usize;
    let mut succeeded = 0usize;
    let mut cancelled = 0usize;

    for (id, tool_name, state, failure) in report {
        let is_reused = reused.contains(&id.value());
        let status = match state {
            Some(TaskNodeState::Succeeded) => "succeeded",
            Some(TaskNodeState::Failed) => "failed",
            Some(TaskNodeState::Skipped) => "skipped",
            Some(TaskNodeState::Cancelled) => "cancelled",
            Some(TaskNodeState::Running | TaskNodeState::Pending) | None => "pending",
        };
        match *state {
            Some(TaskNodeState::Succeeded) => succeeded += 1,
            Some(TaskNodeState::Failed) => failed += 1,
            Some(TaskNodeState::Skipped) => skipped += 1,
            Some(TaskNodeState::Cancelled) => cancelled += 1,
            _ => {}
        }

        let mut entry = json!({
            "id": id.value(),
            "toolName": tool_name,
            "status": status,
        });
        if is_reused {
            entry["reused"] = Value::Bool(true);
        }
        if let Some(reason) = failure {
            entry["error"] = Value::String(reason.clone());
            lines.push(format!(
                "node {} ({tool_name}): {status} — {reason}",
                id.value()
            ));
        } else if status == "skipped" {
            lines.push(format!(
                "node {} ({tool_name}): skipped (dependency did not succeed)",
                id.value()
            ));
        } else if is_reused {
            lines.push(format!(
                "node {} ({tool_name}): succeeded (reused)",
                id.value()
            ));
        } else {
            lines.push(format!("node {} ({tool_name}): {status}", id.value()));
        }

        if let Some(output) = outputs.get(*id) {
            let text = text_of(output);
            if !text.is_empty() {
                entry["output"] = Value::String(text.clone());
                if status == "succeeded" && !is_reused {
                    lines.push(text);
                }
            }
            if let Some(details) = &output.details {
                entry["details"] = details.clone();
            }
        }
        node_results.push(entry);
    }

    let summary = format!(
        "DAG finished: {succeeded} succeeded, {failed} failed, \
         {skipped} skipped, {cancelled} cancelled"
    );
    lines.insert(0, format!("graphId: {graph_id}"));
    lines.insert(1, summary);
    if failed > 0 || skipped > 0 || cancelled > 0 {
        lines.push(format!(
            "to repair: call dag again with {{\"resume\": \"{graph_id}\", \"patch\": {{...}}}} \
             — patch.upsert/remove/addDependsOn/removeDependsOn may change nodes, tools, args \
             and edges. Already-succeeded nodes whose definition is unchanged are reused, not re-run."
        ));
    }
    let is_error = failed > 0 || skipped > 0 || cancelled > 0;
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(lines.join("\n")))],
        details: Some(json!({
            "schema": "ra.dag.result.v1",
            "graphId": graph_id,
            "nodes": node_results,
        })),
        is_error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolRegistry;
    use std::sync::Mutex;

    fn rt() -> asupersync::runtime::Runtime {
        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
    }

    fn registry(names: &[&str]) -> SharedToolRegistry {
        let dir = tempfile::tempdir().expect("tempdir");
        SharedToolRegistry::new(ToolRegistry::new(names, dir.path(), None))
    }

    /// DagTool 只持 Weak；测试必须保留一个 owning handle 否则立即失效。
    ///
    /// Default to **no retry** so a failing-node test does not pay the backoff
    /// (the retry path has its own tests, with `attempts` set explicitly).
    fn tool_over(shared: &SharedToolRegistry) -> DagTool {
        tool_over_with(shared, 1)
    }

    /// A `DagTool` with an explicit retry budget and zero backoff.
    fn tool_over_with(shared: &SharedToolRegistry, attempts: u32) -> DagTool {
        DagTool::new(shared).with_settings(Some(crate::config::DagSettings {
            retry: Some(crate::config::DagRetrySettings {
                attempts: Some(attempts),
                backoff_ms: Some(0),
            }),
            max_concurrency: None,
            keep_graphs: None,
        }))
    }

    fn node_json(id: u32, tool: &str, deps: &[u32]) -> Value {
        json!({
            "id": id,
            "toolName": tool,
            "args": {},
            "dependsOn": deps,
        })
    }

    #[test]
    fn dag_end_to_end_executes_registered_tools() {
        let shared = registry(&["current_time"]);
        let tool = tool_over(&shared);
        let input = json!({
            "nodes": [
                node_json(1, "current_time", &[]),
                node_json(2, "current_time", &[1]),
            ]
        });
        let runtime = rt();
        let output = runtime
            .block_on(tool.execute("call-dag-1", input, None))
            .expect("dag should succeed");

        assert!(!output.is_error);
        let text = match &output.content[0] {
            ContentBlock::Text(t) => t.text.clone(),
            other => panic!("unexpected content {other:?}"),
        };
        assert!(text.contains("2 succeeded"), "summary: {text}");
        assert!(text.contains("node 1 (current_time): succeeded"));
        assert!(text.contains("node 2 (current_time): succeeded"));

        let details = output.details.expect("details present");
        assert_eq!(details["schema"], "ra.dag.result.v1");
        assert_eq!(details["nodes"][0]["status"], "succeeded");
        assert_eq!(details["nodes"][1]["status"], "succeeded");
    }

    #[test]
    fn dag_rejects_recursive_self_reference() {
        // registry 未挂 dag 也必须先报递归（不落入 unknown tool 分支）。
        let shared = registry(&[]);
        let tool = tool_over(&shared);
        let input = json!({ "nodes": [node_json(1, "dag", &[])] });
        let runtime = rt();
        let err = runtime
            .block_on(tool.execute("call-dag-2", input, None))
            .expect_err("recursive dag must be rejected");
        assert!(
            err.to_string().contains("recursive dag"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn dag_unknown_tool_rejected() {
        let shared = registry(&["current_time"]);
        let tool = tool_over(&shared);
        let input = json!({ "nodes": [node_json(1, "no_such_tool", &[])] });
        let runtime = rt();
        let err = runtime
            .block_on(tool.execute("call-dag-3", input, None))
            .expect_err("unknown tool must be rejected");
        assert!(
            err.to_string().contains("unknown tool"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn dag_failed_node_skips_downstream() {
        let shared = registry(&["read", "current_time"]);
        let tool = tool_over(&shared);
        let input = json!({
            "nodes": [
                {
                    "id": 1,
                    "toolName": "read",
                    "args": {"path": "this-file-does-not-exist-dag-test.txt"},
                    "dependsOn": [],
                },
                node_json(2, "current_time", &[1]),
            ]
        });
        let runtime = rt();
        let output = runtime
            .block_on(tool.execute("call-dag-4", input, None))
            .expect("aggregate result is Ok even with failures");

        assert!(output.is_error, "failures must surface as is_error");
        let details = output.details.expect("details present");
        assert_eq!(details["nodes"][0]["status"], "failed");
        assert_eq!(details["nodes"][1]["status"], "skipped");
        let text = match &output.content[0] {
            ContentBlock::Text(t) => t.text.clone(),
            other => panic!("unexpected content {other:?}"),
        };
        assert!(text.contains("1 failed"), "summary: {text}");
        assert!(text.contains("skipped (dependency did not succeed)"));
    }

    #[test]
    fn dag_emits_topology_then_node_states() {
        let shared = registry(&["current_time"]);
        let tool = tool_over(&shared);
        let input = json!({
            "nodes": [
                node_json(1, "current_time", &[]),
                node_json(2, "current_time", &[1]),
            ]
        });
        let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let runtime = rt();
        let _output = runtime
            .block_on(tool.execute(
                "call-dag-evt",
                input,
                Some(Box::new(move |update: ToolUpdate| {
                    if let Some(details) = update.details {
                        sink.lock().expect("sink lock").push(details);
                    }
                })),
            ))
            .expect("dag should succeed");

        let seen = seen.lock().expect("sink lock");
        assert!(!seen.is_empty(), "no DAG events emitted");
        // 首帧必须是拓扑。
        assert_eq!(seen[0]["schema"], "ra.dag.topology.v1");
        assert_eq!(seen[0]["graphId"], "call-dag-evt");
        assert_eq!(seen[0]["nodes"].as_array().map(Vec::len), Some(2));
        assert_eq!(seen[0]["layers"], json!([[1], [2]]));
        assert_eq!(seen[0]["maxDepth"], MAX_DAG_DEPTH);

        // 其后是状态迁移，seq 单调递增，含终态。clone 成 owned 后尽早释放锁。
        let states: Vec<Value> = seen
            .iter()
            .filter(|d| d["schema"] == "ra.dag.node_state.v1")
            .cloned()
            .collect();
        drop(seen);
        assert!(!states.is_empty(), "no node_state events");
        let mut last_seq = -1i64;
        for state in &states {
            assert_eq!(state["graphId"], "call-dag-evt");
            let seq = state["seq"].as_i64().expect("seq present");
            assert!(seq > last_seq, "seq must increase: {seq} <= {last_seq}");
            last_seq = seq;
        }
        assert!(
            states
                .iter()
                .any(|s| s["state"] == "running" && s["nodeId"] == 1),
            "missing running state"
        );
        assert!(
            states
                .iter()
                .any(|s| s["state"] == "succeeded" && s["nodeId"] == 2),
            "missing terminal state"
        );
    }

    #[test]
    fn dag_input_requires_nodes() {
        let shared = registry(&[]);
        let tool = tool_over(&shared);
        let runtime = rt();
        let err = runtime
            .block_on(tool.execute("call-dag-5", json!({}), None))
            .expect_err("missing nodes must be rejected");
        assert!(err.to_string().contains("nodes"));
    }

    #[test]
    fn dag_cycle_rejected_by_build_gate() {
        let shared = registry(&["current_time"]);
        let tool = tool_over(&shared);
        let input = json!({
            "nodes": [
                node_json(1, "current_time", &[2]),
                node_json(2, "current_time", &[1]),
            ]
        });
        let runtime = rt();
        let err = runtime
            .block_on(tool.execute("call-dag-6", input, None))
            .expect_err("cycle must be rejected");
        assert!(
            err.to_string().contains("rejected"),
            "unexpected error: {err}"
        );
    }

    // ---- resume / patch ----

    /// The result carries the `graphId` the model must echo back to repair.
    #[test]
    fn dag_result_reports_graph_id() {
        let shared = registry(&["current_time"]);
        let tool = tool_over(&shared);
        let runtime = rt();
        let out = runtime
            .block_on(tool.execute(
                "call-graphid",
                json!({"nodes": [node_json(1, "current_time", &[])]}),
                None,
            ))
            .expect("dag should succeed");
        let details = out.details.expect("details");
        assert_eq!(details["graphId"], "call-graphid");
    }

    #[test]
    fn dag_patch_without_resume_is_rejected() {
        let shared = registry(&["current_time"]);
        let tool = tool_over(&shared);
        let runtime = rt();
        let err = runtime
            .block_on(tool.execute("call-patch-nors", json!({"patch": {"remove": [1]}}), None))
            .expect_err("patch without resume must be rejected");
        assert!(
            err.to_string().contains("requires `resume`"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn dag_resume_unknown_graph_is_rejected() {
        let shared = registry(&["current_time"]);
        let tool = tool_over(&shared);
        let runtime = rt();
        let err = runtime
            .block_on(tool.execute(
                "call-unknown",
                json!({"resume": "no-such-graph", "patch": {}}),
                None,
            ))
            .expect_err("resume of an unknown graph must be rejected");
        assert!(
            err.to_string().contains("unknown or evicted"),
            "unexpected error: {err}"
        );
    }

    /// resume + patch swaps the failing node's tool; the already-succeeded
    /// upstream node is **reused**, not re-executed.
    #[test]
    fn dag_resume_reuses_unchanged_succeeded_nodes() {
        let shared = registry(&["current_time", "read"]);
        let tool = tool_over(&shared);
        let runtime = rt();

        // 第一次：2 依赖 1，但 read 的文件不存在 → 1 成功、2 失败。
        let first = json!({
            "nodes": [
                node_json(1, "current_time", &[]),
                {
                    "id": 2,
                    "toolName": "read",
                    "args": {"path": "definitely-missing-dag-resume.txt"},
                    "dependsOn": [1],
                },
            ]
        });
        let out1 = runtime
            .block_on(tool.execute("call-resume-1", first, None))
            .expect("aggregate result is Ok even with failures");
        assert!(out1.is_error, "node 2 must fail");
        let d1 = out1.details.expect("details");
        assert_eq!(d1["graphId"], "call-resume-1");
        assert_eq!(d1["nodes"][1]["status"], "failed");

        // 第二次：resume + patch 把节点 2 换成 current_time。
        let second = json!({
            "resume": "call-resume-1",
            "patch": {"upsert": [
                {"id": 2, "toolName": "current_time", "args": {}, "dependsOn": [1]}
            ]}
        });
        let out2 = runtime
            .block_on(tool.execute("call-resume-2", second, None))
            .expect("resume should run");
        assert!(!out2.is_error, "patched graph should succeed");
        let d2 = out2.details.expect("details");
        assert_eq!(
            d2["graphId"], "call-resume-1",
            "graphId stays stable across resumes"
        );
        assert_eq!(d2["nodes"][0]["status"], "succeeded");
        assert_eq!(
            d2["nodes"][0]["reused"], true,
            "unchanged upstream node must be reused, not re-run"
        );
        assert_eq!(d2["nodes"][1]["status"], "succeeded");
        assert!(
            d2["nodes"][1].get("reused").is_none(),
            "the patched node must actually run"
        );
    }

    /// Removing a node drops the edges into it and re-runs its former
    /// dependents (their inputs changed).
    #[test]
    fn dag_patch_remove_reruns_former_dependents() {
        let shared = registry(&["current_time"]);
        let tool = tool_over(&shared);
        let runtime = rt();

        let first = json!({"nodes": [
            node_json(1, "current_time", &[]),
            node_json(2, "current_time", &[1]),
        ]});
        let out1 = runtime
            .block_on(tool.execute("call-del-1", first, None))
            .expect("dag should succeed");
        assert!(!out1.is_error);

        let second = json!({
            "resume": "call-del-1",
            "patch": {"remove": [1]}
        });
        let out2 = runtime
            .block_on(tool.execute("call-del-2", second, None))
            .expect("resume should run");
        assert!(!out2.is_error);
        let d2 = out2.details.expect("details");
        let nodes = d2["nodes"].as_array().expect("nodes array");
        assert_eq!(nodes.len(), 1, "removed node must be gone");
        assert_eq!(nodes[0]["id"], 2);
        assert!(
            nodes[0].get("reused").is_none(),
            "a node whose dependency was removed must re-run"
        );
    }

    /// A `name`-only change is presentational: nothing re-runs.
    #[test]
    fn dag_patch_name_only_change_reuses_everything() {
        let shared = registry(&["current_time"]);
        let tool = tool_over(&shared);
        let runtime = rt();

        let first = json!({"nodes": [
            node_json(1, "current_time", &[]),
            node_json(2, "current_time", &[1]),
        ]});
        let out1 = runtime
            .block_on(tool.execute("call-name-1", first, None))
            .expect("dag should succeed");
        assert!(!out1.is_error);

        let second = json!({
            "resume": "call-name-1",
            "patch": {"upsert": [
                {"id": 2, "toolName": "current_time", "name": "重新打标签", "args": {}, "dependsOn": [1]}
            ]}
        });
        let out2 = runtime
            .block_on(tool.execute("call-name-2", second, None))
            .expect("resume should run");
        let d2 = out2.details.expect("details");
        assert_eq!(d2["nodes"][0]["reused"], true);
        assert_eq!(
            d2["nodes"][1]["reused"], true,
            "renaming a node must not invalidate its output"
        );
    }

    /// A patch emits `node_update.v1` for exactly the changed nodes, so an
    /// incremental frontend can repaint one box instead of rebuilding the graph.
    #[test]
    fn dag_resume_emits_node_update_for_changed_nodes_only() {
        let shared = registry(&["current_time"]);
        let tool = tool_over(&shared);
        let runtime = rt();

        let first = json!({"nodes": [
            node_json(1, "current_time", &[]),
            node_json(2, "current_time", &[1]),
        ]});
        let _ = runtime
            .block_on(tool.execute("call-upd-1", first, None))
            .expect("dag should succeed");

        let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let second = json!({
            "resume": "call-upd-1",
            "patch": {"upsert": [
                {"id": 2, "toolName": "current_time", "name": "改过的节点", "args": {}, "dependsOn": [1]}
            ]}
        });
        let _ = runtime
            .block_on(tool.execute(
                "call-upd-2",
                second,
                Some(Box::new(move |update: ToolUpdate| {
                    if let Some(details) = update.details {
                        sink.lock().expect("sink lock").push(details);
                    }
                })),
            ))
            .expect("resume should run");

        let seen = seen.lock().expect("sink lock");
        let updates: Vec<&Value> = seen
            .iter()
            .filter(|d| d["schema"] == "ra.dag.node_update.v1")
            .collect();
        assert_eq!(updates.len(), 1, "only the patched node may be updated");
        assert_eq!(updates[0]["graphId"], "call-upd-1");
        assert_eq!(updates[0]["nodeId"], 2);
        assert_eq!(updates[0]["name"], "改过的节点");
        assert_eq!(updates[0]["dependsOn"], json!([1]));
    }

    // ---- stack safety ----

    /// A dag node naming `run_code` must resolve its effects without recursing.
    ///
    /// `SharedToolRegistry` binds every tool to the live registry, `run_code`
    /// included, and `run_code::effects()` unions the registry's effects. With
    /// no self-skip that re-entered `effects()` until the thread stack
    /// overflowed, so a dag containing a `run_code` node aborted before any
    /// node ran. Step 3 of `run_dag` resolves `tool.effects()` for every node,
    /// which is the path pinned here.
    #[test]
    fn dag_run_code_node_effects_resolve_without_recursing() {
        let shared = registry(&["run_code"]);
        let tool = tool_over(&shared);
        let input = json!({
            "nodes": [{
                "id": 1,
                "toolName": "run_code",
                "args": {"code": "return 1;", "timeoutMs": 30_000},
                "dependsOn": [],
            }]
        });
        let runtime = rt();
        let output = runtime
            .block_on(tool.execute("call-dag-run-code", input, None))
            .expect("a dag with a run_code node must complete, not overflow");
        assert!(!output.is_error, "{output:?}");
        let text = match &output.content[0] {
            ContentBlock::Text(t) => t.text.clone(),
            other => panic!("unexpected content {other:?}"),
        };
        assert!(text.contains("1 succeeded"), "summary: {text}");
    }

    /// TEMP measurement probe (removed after the sweep): drives the real dag
    /// tool at N nodes on a caller thread with an explicit stack, mirroring the
    /// FTUI driver's `RuntimeBuilder::new().thread_stack_size(..)` + `block_on`.
    #[test]
    #[ignore = "measurement probe: DAG_PROBE_STACK_BYTES + DAG_PROBE_N"]
    fn probe_dag_stack_scaling() {
        let stack: usize = std::env::var("DAG_PROBE_STACK_BYTES")
            .expect("DAG_PROBE_STACK_BYTES")
            .parse()
            .expect("stack bytes");
        let n: usize = std::env::var("DAG_PROBE_N")
            .expect("DAG_PROBE_N")
            .parse()
            .expect("node count");
        let handle = std::thread::Builder::new()
            .name("dag-probe".into())
            .stack_size(stack)
            .spawn(move || {
                let runtime = asupersync::runtime::RuntimeBuilder::new()
                    .thread_stack_size(stack)
                    .build()
                    .expect("runtime");
                let shared = registry(&["current_time"]);
                let tool = tool_over(&shared);
                let nodes: Vec<Value> = (1..=n)
                    .map(|id| {
                        // 64-wide layers, depth = ceil(n / 64): stays inside the
                        // build gates (width 64, depth 100) at every N probed.
                        let deps: Vec<u32> = if id > 64 { vec![(id - 64) as u32] } else { vec![] };
                        node_json(id as u32, "current_time", &deps)
                    })
                    .collect();
                let out = runtime
                    .block_on(tool.execute("probe-dag", json!({"nodes": nodes}), None))
                    .expect("probe dag completes");
                assert!(!out.is_error, "probe dag failed: {out:?}");
            })
            .expect("spawn probe");
        handle.join().expect("probe thread must not abort");
    }
}
