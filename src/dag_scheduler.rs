//! DAG 三层调度：依赖就绪集 → 副作用切批 → 并发限流。
//!
//! 第 1 层由 [`TaskGraph`] 的显式 `depends_on` 决定谁就绪；第 2 层复用
//! [`crate::agent::plan_tool_effect_batches`] 对就绪子集切批（barrier 独占批次）；
//! 第 3 层在 `max_concurrency` 个动态槽位内并发：完成一个立刻补一个，慢节点
//! 不阻塞其余槽位。批间串行，因此全局并发峰值恒 ≤ `max_concurrency`，
//! barrier 批永不与他者重叠。
//!
//! **重试**（[`DagScheduler::with_retry`]）只作用于 **parallel-safe** 节点
//! （只读/联网）：这类节点重跑无副作用，失败后按 `attempts` 重试。带
//! write/append/process 副作用的节点**永不重试**——重放 `bash`/`write` 会把
//! 副作用做两遍，比原始失败更糟。这是固定语义，不提供配置开关。

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use serde_json::Value;
use thiserror::Error;

use crate::agent::plan_tool_effect_batches;
use crate::model::ContentBlock;
use crate::task_dag::{TaskGraph, TaskNode, TaskNodeId, TaskNodeState};
use crate::tools::ToolOutput;

/// 节点执行抽象：M3 接入 `ToolRegistry`，测试注入 mock。
///
/// 返回 boxed `'static` future（入参 owned）以保持对象安全，并避开
/// nightly 上 `+ 'a` dyn 界限触发的 rust-lang/rust#100013 推断失败。
pub trait NodeExecutor: Send + Sync {
    /// 执行一个节点。`resolved_args` 已完成 `{{node.*}}` 占位符解析。
    fn execute(
        &self,
        node: TaskNode,
        resolved_args: Value,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, String>> + Send>>;
}

/// 占位符/信息流解析错误。
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ResolveError {
    /// 引用的节点在图里不存在，或其输出尚未写入黑板。
    #[error("reference to node {node:?} has no output")]
    UnknownNode { node: TaskNodeId },
    /// 引用了 `depends_on` 范围之外的节点（越权读取）。
    #[error("node {node:?} references {referenced:?} outside depends_on")]
    OutOfScope {
        node: TaskNodeId,
        referenced: TaskNodeId,
    },
    /// 模板语法无法解析。
    #[error("invalid node template: {0}")]
    InvalidTemplate(String),
    /// `data.<path>` 在上游 details 中不存在。
    #[error("path not found in upstream details")]
    PathMissing,
}

/// 调度失败。
#[derive(Debug, Error)]
pub enum ScheduleError {
    /// 有剩余节点但无就绪节点（构建期已排除环，理论上仅防御性触发）。
    #[error("scheduler stalled: {pending} pending nodes with no ready set")]
    Stalled { pending: usize },
    /// 图跑完后存在失败/跳过/取消节点，聚合回传给调用方。
    #[error("graph finished with {failed} failed, {skipped} skipped, {cancelled} cancelled")]
    Aggregate {
        failed: usize,
        skipped: usize,
        cancelled: usize,
    },
}

/// 任务间共享黑板：节点成功后写入，下游按 `depends_on` 范围读取。
#[derive(Debug, Default)]
pub struct TaskOutputStore {
    outputs: HashMap<TaskNodeId, Arc<ToolOutput>>,
}

impl TaskOutputStore {
    /// 写入一个节点的输出（内部使用；DAG 语义下同批节点互不依赖）。
    pub fn insert(&mut self, id: TaskNodeId, out: Arc<ToolOutput>) {
        self.outputs.insert(id, out);
    }

    /// 读取节点输出。
    #[must_use]
    pub fn get(&self, id: TaskNodeId) -> Option<&Arc<ToolOutput>> {
        self.outputs.get(&id)
    }

    /// 节点数（已产出输出的节点）。
    #[must_use]
    pub fn len(&self) -> usize {
        self.outputs.len()
    }

    /// 是否无输出。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.outputs.is_empty()
    }

    /// 解析节点 args 中的 `{{node.<id>.content}}` / `{{node.<id>.data.<path>}}`。
    ///
    /// **只允许解析 `node.depends_on` 范围内的引用**——越权引用返回
    /// [`ResolveError::OutOfScope`]，绝不静默置空。
    ///
    /// - 整串恰好是一个模板时，内嵌原始 JSON 值（结构化注入）；
    /// - 否则按字符串拼接替换。
    pub fn resolve_args(&self, node: &TaskNode, args: &Value) -> Result<Value, ResolveError> {
        self.resolve_value(node, args)
    }

    fn resolve_value(&self, node: &TaskNode, value: &Value) -> Result<Value, ResolveError> {
        match value {
            Value::String(s) => self.resolve_string(node, s),
            Value::Array(items) => items
                .iter()
                .map(|item| self.resolve_value(node, item))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array),
            Value::Object(map) => {
                let mut out = serde_json::Map::with_capacity(map.len());
                for (key, item) in map {
                    out.insert(key.clone(), self.resolve_value(node, item)?);
                }
                Ok(Value::Object(out))
            }
            other => Ok(other.clone()),
        }
    }

    fn resolve_string(&self, node: &TaskNode, raw: &str) -> Result<Value, ResolveError> {
        // 整串单模板 → 结构化内嵌。
        if let Some(inner) = single_template(raw) {
            let parsed = parse_template(inner)?;
            let value = self.lookup(node, &parsed)?;
            return Ok(value);
        }

        // 部分模板 → 字符串拼接替换。
        let mut out = String::with_capacity(raw.len());
        let mut rest = raw;
        while let Some(start) = rest.find("{{node.") {
            out.push_str(&rest[..start]);
            let close_rel = rest[start + 7..]
                .find("}}")
                .ok_or_else(|| ResolveError::InvalidTemplate(rest.to_string()))?;
            let inner = &rest[start + 7..start + 7 + close_rel];
            let parsed = parse_template(inner)?;
            let value = self.lookup(node, &parsed)?;
            out.push_str(&stringify(&value));
            rest = &rest[start + 7 + close_rel + 2..];
        }
        out.push_str(rest);
        Ok(Value::String(out))
    }

    fn lookup(
        &self,
        node: &TaskNode,
        target: &(TaskNodeId, TemplateKind),
    ) -> Result<Value, ResolveError> {
        let referenced = target.0;
        // 信息流边界：只允许读 depends_on 范围内（先判范围，再查输出）。
        if !node.depends_on.contains(&referenced) {
            return Err(ResolveError::OutOfScope {
                node: node.id,
                referenced,
            });
        }
        let output = self
            .outputs
            .get(&referenced)
            .ok_or(ResolveError::UnknownNode { node: referenced })?;
        match &target.1 {
            TemplateKind::Content => Ok(Value::String(text_of(output))),
            TemplateKind::Data(path) => {
                let mut current = output.details.as_ref().ok_or(ResolveError::PathMissing)?;
                for segment in path {
                    current = current
                        .get(segment.as_str())
                        .ok_or(ResolveError::PathMissing)?;
                }
                Ok(current.clone())
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TemplateKind {
    Content,
    Data(Vec<String>),
}

/// 若整串恰好是一个 `{{node.…}}` 模板，返回其内部内容。
fn single_template(raw: &str) -> Option<&str> {
    let rest = raw.strip_prefix("{{node.")?;
    let end = rest.find("}}")?;
    if end + 2 != rest.len() {
        return None;
    }
    Some(&rest[..end])
}

/// 解析 `\<id\>.content` 或 `\<id\>.data.<path>`。
fn parse_template(inner: &str) -> Result<(TaskNodeId, TemplateKind), ResolveError> {
    let (id_str, rest) = inner
        .split_once('.')
        .ok_or_else(|| ResolveError::InvalidTemplate(inner.to_string()))?;
    let id: u32 = id_str
        .parse()
        .map_err(|_| ResolveError::InvalidTemplate(inner.to_string()))?;
    let tid = TaskNodeId::new(id);
    if rest == "content" {
        return Ok((tid, TemplateKind::Content));
    }
    let data = rest
        .strip_prefix("data.")
        .ok_or_else(|| ResolveError::InvalidTemplate(inner.to_string()))?;
    if data.is_empty() {
        return Err(ResolveError::InvalidTemplate(inner.to_string()));
    }
    Ok((
        tid,
        TemplateKind::Data(data.split('.').map(String::from).collect()),
    ))
}

fn stringify(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// 拼接 `ToolOutput` 的全部文本块。
pub(crate) fn text_of(output: &ToolOutput) -> String {
    output
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

type StateCallback = Box<dyn Fn(TaskNodeId, TaskNodeState) + Send + Sync>;
type OutputCallback = Box<dyn Fn(TaskNodeId, &ToolOutput) + Send + Sync>;

/// 三层 DAG 调度器。
pub struct DagScheduler {
    graph: TaskGraph,
    outputs: TaskOutputStore,
    states: HashMap<TaskNodeId, TaskNodeState>,
    /// 节点失败原因（供调用方整理回传 LLM）。
    failures: HashMap<TaskNodeId, String>,
    executor: Arc<dyn NodeExecutor>,
    max_concurrency: usize,
    /// parallel-safe 节点的总尝试次数（含首次）；`1` = 不重试。
    retry_attempts: u32,
    /// 两次尝试之间的固定退避。
    retry_backoff: Duration,
    on_state: Option<StateCallback>,
    on_output: Option<OutputCallback>,
}

impl DagScheduler {
    /// 创建调度器，所有节点初始为 `Pending`，默认不重试。
    #[must_use]
    pub fn new(graph: TaskGraph, executor: Arc<dyn NodeExecutor>, max_concurrency: usize) -> Self {
        let states = graph
            .nodes()
            .iter()
            .map(|node| (node.id, TaskNodeState::Pending))
            .collect();
        Self {
            graph,
            outputs: TaskOutputStore::default(),
            states,
            failures: HashMap::new(),
            executor,
            max_concurrency: max_concurrency.max(1),
            retry_attempts: 1,
            retry_backoff: Duration::ZERO,
            on_state: None,
            on_output: None,
        }
    }

    /// 默认并发上限：复用主循环的 compatible-tool 并发配置。
    #[must_use]
    pub fn with_default_concurrency(graph: TaskGraph, executor: Arc<dyn NodeExecutor>) -> Self {
        Self::new(
            graph,
            executor,
            crate::agent::compatible_tool_parallelism_limit(),
        )
    }

    /// 设置 parallel-safe 节点的重试策略。
    ///
    /// `attempts` 含首次执行（`1` = 不重试；`0` 视作 `1`）。带副作用的节点
    /// （write/append/process）不受影响，**永不重试**。
    #[must_use]
    pub fn with_retry(mut self, attempts: u32, backoff: Duration) -> Self {
        self.retry_attempts = attempts.max(1);
        self.retry_backoff = backoff;
        self
    }

    /// 播种既有结果：`resume` 时已 `Succeeded` 的节点不重跑。
    ///
    /// `outputs` 同时写入黑板（下游占位符可解析）与状态机（节点直接终态，
    /// 不再进入就绪集）。不在图中的 id 会被忽略——避免写入幽灵节点。
    #[must_use]
    pub fn with_seed(
        mut self,
        outputs: Vec<(TaskNodeId, Arc<ToolOutput>)>,
        succeeded: &[TaskNodeId],
    ) -> Self {
        for (id, output) in outputs {
            if self.graph.node(id).is_some() {
                self.outputs.insert(id, output);
            }
        }
        for &id in succeeded {
            if self.graph.node(id).is_some() {
                self.states.insert(id, TaskNodeState::Succeeded);
            }
        }
        self
    }

    /// 注册状态迁移回调（M4 事件层挂这里）。
    #[must_use]
    pub fn on_state(
        mut self,
        cb: impl Fn(TaskNodeId, TaskNodeState) + Send + Sync + 'static,
    ) -> Self {
        self.on_state = Some(Box::new(cb));
        self
    }

    /// 注册节点产出回调（M4 事件层用它发 `node_output` delta）。
    ///
    /// 在黑板写入之后、`Succeeded` 状态迁移之前调用。
    #[must_use]
    pub fn on_output(
        mut self,
        cb: impl Fn(TaskNodeId, &ToolOutput) + Send + Sync + 'static,
    ) -> Self {
        self.on_output = Some(Box::new(cb));
        self
    }

    /// 当前节点状态。
    #[must_use]
    pub fn state(&self, id: TaskNodeId) -> Option<TaskNodeState> {
        self.states.get(&id).copied()
    }

    /// 节点失败原因（仅 Failed 节点有值）。
    #[must_use]
    pub fn failure(&self, id: TaskNodeId) -> Option<&str> {
        self.failures.get(&id).map(String::as_str)
    }

    /// 消费调度器，取出黑板（`run()` 之后调用）。
    #[must_use]
    pub fn into_outputs(self) -> TaskOutputStore {
        self.outputs
    }

    /// 三层调度主循环。
    ///
    /// 全部节点 settle 后：存在 `Failed`/`Skipped`/`Cancelled` →
    /// [`ScheduleError::Aggregate`]（由调用方整理回传 LLM，不做自动 replan）。
    pub async fn run(&mut self) -> Result<(), ScheduleError> {
        loop {
            self.cascade();
            let pending = self.pending_nodes();
            if pending.is_empty() {
                break;
            }

            // 第 1 层：依赖就绪集（依赖全部 Succeeded）。
            let ready = self.ready_set(&pending);
            if ready.is_empty() {
                return Err(ScheduleError::Stalled {
                    pending: pending.len(),
                });
            }

            // 就绪节点互不依赖（否则其一不会 ready），可安全按副作用重排：
            // 非 barrier 前置成一个大并发批，barrier 按原序落在其后。
            // 这样既复用连续切批函数，又恢复被位置相邻性误伤的并行度。
            let (mut safe, mut barriers): (Vec<TaskNodeId>, Vec<TaskNodeId>) =
                ready.into_iter().partition(|id| {
                    self.graph
                        .node(*id)
                        .is_some_and(|node| node.effects.parallel_safe())
                });
            safe.append(&mut barriers);

            let effects: Vec<_> = safe
                .iter()
                .filter_map(|id| self.graph.node(*id))
                .map(|node| node.effects)
                .collect();
            let batches = plan_tool_effect_batches(&effects);

            for batch in batches {
                let ids = &safe[batch.start..batch.end];
                for &id in ids {
                    self.set_state(id, TaskNodeState::Running);
                }

                // 第 3 层：批内并发受限；批间串行（barrier 语义由切批保证）。
                //
                // 动态槽位：维护至多 `max_concurrency` 个在飞 future，**谁先
                // 完成谁立刻补位**——慢节点不再拖住整块（旧的 `chunks +
                // join_all` 会让一个慢节点阻塞同块其余槽位）。
                //
                // 注：原设计用 `buffer_unordered`，nightly rustc 1.100 对该组合
                // 的 opaque future Send 推断触发 rust-lang/rust#100013（误报
                // lifetime bound not satisfied）。这里改用显式的
                // `FuturesUnordered` 手动补位：future 类型具名、不经过
                // `buffer_unordered` 的 opaque 包装，同样的语义与限流保证，
                // 且能在不满足 `Send` 推断时**编译期直接暴露**（哨兵测试
                // `run_future_is_send` 会先报警），不会悄悄退化为串行。
                let mut results: Vec<(TaskNodeId, Result<ToolOutput, String>)> =
                    Vec::with_capacity(ids.len());
                // Scope the in-flight set so its borrow of `&self` ends before
                // the mutable bookkeeping loop below.
                {
                    let mut pending = ids.iter();
                    let mut in_flight: FuturesUnordered<_> = pending
                        .by_ref()
                        .take(self.max_concurrency)
                        .map(|&id| self.execute_with_retry(id))
                        .collect();
                    while let Some((id, result)) = in_flight.next().await {
                        results.push((id, result));
                        if let Some(&next) = pending.next() {
                            in_flight.push(self.execute_with_retry(next));
                        }
                    }
                }

                for (id, result) in results {
                    match result {
                        Ok(output) if !output.is_error => {
                            let output = Arc::new(output);
                            self.outputs.insert(id, Arc::clone(&output));
                            if let Some(cb) = &self.on_output {
                                cb(id, &output);
                            }
                            self.set_state(id, TaskNodeState::Succeeded);
                        }
                        Ok(output) => {
                            // 工具返回 is_error=true：记失败原因，输出留存黑板供排查。
                            let reason = text_of(&output);
                            self.failures.insert(
                                id,
                                if reason.is_empty() {
                                    "tool reported an error".to_string()
                                } else {
                                    reason
                                },
                            );
                            self.outputs.insert(id, Arc::new(output));
                            self.set_state(id, TaskNodeState::Failed);
                        }
                        Err(err) => {
                            self.failures.insert(id, err);
                            self.set_state(id, TaskNodeState::Failed);
                        }
                    }
                }
            }
        }

        let count = |state: TaskNodeState| {
            self.states
                .values()
                .filter(|current| **current == state)
                .count()
        };
        let failed = count(TaskNodeState::Failed);
        let skipped = count(TaskNodeState::Skipped);
        let cancelled = count(TaskNodeState::Cancelled);
        if failed > 0 || skipped > 0 || cancelled > 0 {
            return Err(ScheduleError::Aggregate {
                failed,
                skipped,
                cancelled,
            });
        }
        Ok(())
    }

    fn set_state(&mut self, id: TaskNodeId, state: TaskNodeState) {
        self.states.insert(id, state);
        if let Some(cb) = &self.on_state {
            cb(id, state);
        }
    }

    fn pending_nodes(&self) -> Vec<TaskNodeId> {
        self.graph
            .nodes()
            .iter()
            .filter(|node| self.states.get(&node.id) == Some(&TaskNodeState::Pending))
            .map(|node| node.id)
            .collect()
    }

    /// 第 1 层就绪集：自身 Pending 且全部 `depends_on` 已 `Succeeded`。
    fn ready_set(&self, pending: &[TaskNodeId]) -> Vec<TaskNodeId> {
        pending
            .iter()
            .copied()
            .filter(|id| {
                self.graph.node(*id).is_some_and(|node| {
                    node.depends_on
                        .iter()
                        .all(|dep| self.states.get(dep) == Some(&TaskNodeState::Succeeded))
                })
            })
            .collect()
    }

    /// 失败传播（三条规则，级联到不动点）：
    /// 上游 `Failed`/`Skipped` → 下游 `Skipped`；上游 `Cancelled` → 下游 `Cancelled`。
    fn cascade(&mut self) {
        loop {
            let mut changed = false;
            let pending = self.pending_nodes();
            for id in pending {
                let Some(node) = self.graph.node(id) else {
                    continue;
                };
                let mut next = None;
                for dep in &node.depends_on {
                    match self.states.get(dep) {
                        Some(TaskNodeState::Cancelled) => {
                            next = Some(TaskNodeState::Cancelled);
                            break;
                        }
                        Some(TaskNodeState::Failed | TaskNodeState::Skipped) => {
                            next = Some(TaskNodeState::Skipped);
                        }
                        _ => {}
                    }
                }
                if let Some(state) = next {
                    self.set_state(id, state);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }

    /// 单节点执行：先解析占位符（越权 → 失败且不调 executor），再调执行器。
    async fn execute_one(&self, id: TaskNodeId) -> (TaskNodeId, Result<ToolOutput, String>) {
        let Some(node) = self.graph.node(id) else {
            return (id, Err(format!("missing node {id:?}")));
        };
        let resolved = match self.outputs.resolve_args(node, &node.args) {
            Ok(value) => value,
            Err(err) => return (id, Err(format!("resolve failed: {err}"))),
        };
        (id, self.executor.execute(node.clone(), resolved).await)
    }

    /// 执行一个节点，**只对 parallel-safe 节点**按 `retry_attempts` 重试。
    ///
    /// barrier 节点（write/append/process）恒为一次：重跑会重放副作用。
    /// 解析失败（越权占位符等）是确定性错误，重试也拦不住——但重试只发生在
    /// parallel-safe 节点上，成本有界，故不额外区分。
    async fn execute_with_retry(&self, id: TaskNodeId) -> (TaskNodeId, Result<ToolOutput, String>) {
        let attempts = match self.graph.node(id) {
            Some(node) if node.effects.parallel_safe() => self.retry_attempts,
            _ => 1,
        };
        let mut result = self.execute_one(id).await;
        let mut attempt = 1;
        while attempt < attempts && is_failure(&result.1) {
            if !self.retry_backoff.is_zero() {
                asupersync::time::sleep(asupersync::time::wall_now(), self.retry_backoff).await;
            }
            result = self.execute_one(id).await;
            attempt += 1;
        }
        result
    }
}

/// 执行结果是否算失败（`Err`，或工具自报 `is_error`）。
fn is_failure(result: &Result<ToolOutput, String>) -> bool {
    result.as_ref().map_or(true, |output| output.is_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TextContent;
    use crate::tools::ToolEffects;
    use std::collections::HashSet;
    use std::future::Future;
    use std::sync::Mutex;
    use std::task::{Context, Poll};

    /// 让出一次调度权，使同批 future 能真正交叠执行。
    struct YieldOnce(bool);

    impl Future for YieldOnce {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.0 {
                Poll::Ready(())
            } else {
                self.0 = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    fn node(id: u32, tool: &str, deps: &[u32], effects: ToolEffects) -> TaskNode {
        TaskNode {
            id: TaskNodeId::new(id),
            tool_name: tool.to_string(),
            name: String::new(),
            args: Value::Null,
            depends_on: deps.iter().copied().map(TaskNodeId::new).collect(),
            effects,
        }
    }

    #[derive(Debug, Default)]
    struct MockState {
        started: HashSet<u32>,
        recorded_args: HashMap<u32, Value>,
        /// (node_id, is_enter) 时间线。
        timeline: Vec<(u32, bool)>,
        current: usize,
        peak: usize,
    }

    struct MockExecutor {
        fail: Arc<HashSet<u32>>,
        state: Arc<Mutex<MockState>>,
    }

    impl MockExecutor {
        fn new(fail: &[u32]) -> Self {
            Self {
                fail: Arc::new(fail.iter().copied().collect()),
                state: Arc::new(Mutex::new(MockState::default())),
            }
        }

        fn snapshot(&self) -> MockState {
            let guard = self.state.lock().expect("mock state lock");
            MockState {
                started: guard.started.clone(),
                recorded_args: guard.recorded_args.clone(),
                timeline: guard.timeline.clone(),
                current: guard.current,
                peak: guard.peak,
            }
        }
    }

    impl NodeExecutor for MockExecutor {
        fn execute(
            &self,
            node: TaskNode,
            resolved_args: Value,
        ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, String>> + Send>> {
            let state = Arc::clone(&self.state);
            let fail = Arc::clone(&self.fail);
            Box::pin(async move {
                let id = node.id.value();
                {
                    let mut guard = state.lock().expect("mock state lock");
                    guard.started.insert(id);
                    guard.recorded_args.insert(id, resolved_args);
                    guard.timeline.push((id, true));
                    guard.current += 1;
                    guard.peak = guard.peak.max(guard.current);
                }
                YieldOnce(false).await;
                let failed = fail.contains(&id);
                {
                    let mut guard = state.lock().expect("mock state lock");
                    guard.timeline.push((id, false));
                    guard.current -= 1;
                }
                if failed {
                    return Err(format!("node {id} failed"));
                }
                Ok(ToolOutput {
                    content: vec![ContentBlock::Text(TextContent::new(format!("out-{id}")))],
                    details: Some(serde_json::json!({ "value": id * 10 })),
                    is_error: false,
                })
            })
        }
    }

    fn run_with(executor: &Arc<MockExecutor>, graph: TaskGraph, n: usize) -> TaskOutputStore {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let concrete = Arc::clone(executor);
        let executor: Arc<dyn NodeExecutor> = concrete;
        let mut scheduler = DagScheduler::new(graph, executor, n);
        rt.block_on(async { scheduler.run().await })
            .expect("run should succeed");
        scheduler.into_outputs()
    }

    /// 断言 barrier 节点的 enter/exit 区间内没有其他任何节点的事件。
    fn assert_barrier_exclusive(timeline: &[(u32, bool)], barrier_ids: &[u32]) {
        for &barrier in barrier_ids {
            let enter = timeline
                .iter()
                .position(|&(id, enter)| id == barrier && enter)
                .unwrap_or_else(|| panic!("barrier {barrier} never entered"));
            let exit = timeline
                .iter()
                .position(|&(id, enter)| id == barrier && !enter)
                .unwrap_or_else(|| panic!("barrier {barrier} never exited"));
            for &(other, _) in &timeline[enter + 1..exit] {
                assert_eq!(
                    other, barrier,
                    "barrier {barrier} overlapped with node {other}"
                );
            }
        }
    }

    /// `run()` 的 opaque future 必须 `Send`（async_trait 装箱要求）——
    /// 这是 nightly #100013 回归的哨兵测试。
    #[test]
    fn run_future_is_send() {
        fn assert_send<T: Send>(_: &T) {}
        fn assert_sync<T: Sync>(_: &T) {}
        let graph = TaskGraph::build(vec![node(1, "read", &[], ToolEffects::read())]).unwrap();
        let executor = Arc::new(MockExecutor::new(&[]));
        let scheduler = DagScheduler::new(graph, executor, 1);
        assert_send(&scheduler);
        assert_sync(&scheduler);
        let mut scheduler = scheduler;
        let fut = scheduler.run();
        assert_send(&fut);
    }

    #[test]
    fn diamond_concurrency_and_topological_order() {
        let mut n4 = node(4, "read", &[2, 3], ToolEffects::read());
        n4.args = serde_json::json!({
            "a": "{{node.2.content}}",
            "b": "{{node.2.data.value}}",
        });
        let graph = TaskGraph::build(vec![
            node(1, "read", &[], ToolEffects::read()),
            node(2, "read", &[1], ToolEffects::read()),
            node(3, "read", &[1], ToolEffects::read()),
            n4,
        ])
        .expect("valid graph");

        let executor = Arc::new(MockExecutor::new(&[]));
        let outputs = run_with(&executor, graph, 4);
        let state = executor.snapshot();

        // 2、3 并发重叠：一个 enter 夹在另一个 enter/exit 之间。
        let enter2 = state.timeline.iter().position(|&(id, e)| id == 2 && e);
        let exit2 = state.timeline.iter().position(|&(id, e)| id == 2 && !e);
        let enter3 = state.timeline.iter().position(|&(id, e)| id == 3 && e);
        let exit3 = state.timeline.iter().position(|&(id, e)| id == 3 && !e);
        assert!(
            (enter2 < enter3 && enter3 < exit2) || (enter3 < enter2 && enter2 < exit3),
            "nodes 2 and 3 did not overlap: {:?}",
            state.timeline
        );

        // 4 在 2、3 都 exit 之后才 enter。
        let enter4 = state
            .timeline
            .iter()
            .position(|&(id, e)| id == 4 && e)
            .expect("4 enter");
        assert!(enter4 > exit2.expect("2 exit") && enter4 > exit3.expect("3 exit"));
        assert!(
            state.peak >= 2,
            "expected real overlap, peak={}",
            state.peak
        );

        // 下游拿到上游真实输出。
        let n4 = TaskNodeId::new(4);
        let args = state.recorded_args.get(&n4.value()).expect("4 executed");
        assert_eq!(args["a"], "out-2");
        assert_eq!(args["b"], 20);
        assert!(outputs.get(TaskNodeId::new(2)).is_some());
        assert!(outputs.get(n4).is_some());
    }

    #[test]
    fn ten_node_islands_recover_parallelism() {
        // §2.2.1 用例：unknown 按仓内惯例映射为 write（barrier）。
        let effects = [
            ToolEffects::read(),
            ToolEffects::network(),
            ToolEffects::write(),
            ToolEffects::read(),
            ToolEffects::append(),
            ToolEffects::network(),
            ToolEffects::process(),
            ToolEffects::read(),
            ToolEffects::write(),
            ToolEffects::network(),
        ];
        let nodes: Vec<TaskNode> = [
            "read", "network", "write", "read", "append", "network", "process", "read", "unknown",
            "network",
        ]
        .iter()
        .enumerate()
        .map(|(i, name)| {
            node(
                u32::try_from(i).expect("index fits u32"),
                name,
                &[],
                effects[i],
            )
        })
        .collect();
        let graph = TaskGraph::build(nodes).expect("valid graph");

        let executor = Arc::new(MockExecutor::new(&[]));
        run_with(&executor, graph, 2);
        let state = executor.snapshot();

        // (a) 无依赖的 read/network 节点恢复并发，不再退化为单元素批次。
        assert!(
            state.peak >= 2,
            "ready-set islands must run concurrently, peak={}",
            state.peak
        );
        // (b) barrier 节点（write/append/process，下标 2/4/6，及 unknown→write 8）
        //     始终独占执行窗口。
        assert_barrier_exclusive(&state.timeline, &[2, 4, 6, 8]);
        assert_eq!(state.started.len(), 10, "all nodes should execute");
    }

    #[test]
    fn concurrency_never_exceeds_n() {
        let nodes: Vec<TaskNode> = (0..8)
            .map(|i| node(i, "read", &[], ToolEffects::read()))
            .collect();
        let graph = TaskGraph::build(nodes).expect("valid graph");

        let executor = Arc::new(MockExecutor::new(&[]));
        run_with(&executor, graph, 2);
        let state = executor.snapshot();
        assert!(
            state.peak <= 2,
            "peak concurrency {} exceeded N=2",
            state.peak
        );
        assert_eq!(state.started.len(), 8);
    }

    #[test]
    fn n_equals_one_matches_sequential_oracle() {
        let nodes: Vec<TaskNode> = (0..6)
            .map(|i| node(i, "read", &[], ToolEffects::read()))
            .collect();
        let graph = TaskGraph::build(nodes).expect("valid graph");

        let executor = Arc::new(MockExecutor::new(&[]));
        let outputs = run_with(&executor, graph, 1);
        let state = executor.snapshot();

        assert_eq!(state.peak, 1, "N=1 must be strictly sequential");
        // 顺序 oracle：每个节点输出确定，黑板全量且一致。
        for i in 0..6u32 {
            let out = outputs
                .get(TaskNodeId::new(i))
                .unwrap_or_else(|| panic!("missing output for {i}"));
            assert_eq!(out.content.len(), 1);
            match &out.content[0] {
                ContentBlock::Text(t) => assert_eq!(t.text, format!("out-{i}")),
                other => panic!("unexpected content {other:?}"),
            }
            assert_eq!(out.details, Some(serde_json::json!({ "value": i * 10 })));
        }
    }

    #[test]
    fn upstream_failure_skips_downstream() {
        let graph = TaskGraph::build(vec![
            node(1, "read", &[], ToolEffects::read()),
            node(2, "read", &[1], ToolEffects::read()),
            node(4, "read", &[2], ToolEffects::read()),
        ])
        .expect("valid graph");

        let executor = Arc::new(MockExecutor::new(&[2]));
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let mut scheduler = DagScheduler::new(graph, executor.clone(), 2);
        let result = rt.block_on(async { scheduler.run().await });

        assert!(matches!(
            result,
            Err(ScheduleError::Aggregate {
                failed: 1,
                skipped: 1,
                cancelled: 0
            })
        ));
        let state = executor.snapshot();
        assert!(!state.started.contains(&4), "skipped node must not run");
        assert_eq!(
            scheduler.state(TaskNodeId::new(4)),
            Some(TaskNodeState::Skipped)
        );
        assert_eq!(
            scheduler.state(TaskNodeId::new(2)),
            Some(TaskNodeState::Failed)
        );
        // 黑板无 4 的键。
        assert!(scheduler.into_outputs().get(TaskNodeId::new(4)).is_none());
    }

    #[test]
    fn out_of_scope_reference_rejected() {
        let mut n2 = node(2, "read", &[], ToolEffects::read());
        // 引用节点 1，但不在 depends_on 内 → 越权。
        n2.args = serde_json::json!({ "a": "{{node.1.content}}" });
        let graph = TaskGraph::build(vec![node(1, "read", &[], ToolEffects::read()), n2])
            .expect("valid graph");

        let executor = Arc::new(MockExecutor::new(&[]));
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let mut scheduler = DagScheduler::new(graph, executor.clone(), 2);
        let result = rt.block_on(async { scheduler.run().await });

        assert!(result.is_err());
        let state = executor.snapshot();
        assert!(
            !state.started.contains(&2),
            "out-of-scope node must fail before execution"
        );
        assert_eq!(
            scheduler.state(TaskNodeId::new(2)),
            Some(TaskNodeState::Failed)
        );
    }

    #[test]
    fn placeholder_injects_upstream_output() {
        let mut n2 = node(2, "read", &[1], ToolEffects::read());
        n2.args = serde_json::json!({
            "a": "{{node.1.content}}",
            "b": "{{node.1.data.value}}",
            "c": "pre-{{node.1.content}}-post",
        });
        let graph = TaskGraph::build(vec![node(1, "read", &[], ToolEffects::read()), n2])
            .expect("valid graph");

        let executor = Arc::new(MockExecutor::new(&[]));
        run_with(&executor, graph, 2);
        let state = executor.snapshot();

        let args = state
            .recorded_args
            .get(&2)
            .expect("node 2 executed with resolved args");
        // 整串 content 模板 → 字符串；data 模板 → 结构化数字；混合 → 拼接。
        assert_eq!(args["a"], "out-1");
        assert_eq!(args["b"], 10);
        assert_eq!(args["c"], "pre-out-1-post");
    }

    // ---- 重试语义 ----

    /// 计数型执行器：指定节点前 `fail_first` 次失败，之后成功。
    #[derive(Default)]
    struct FlakyState {
        attempts: HashMap<u32, u32>,
    }

    struct FlakyExecutor {
        fail_first: HashMap<u32, u32>,
        state: Arc<Mutex<FlakyState>>,
    }

    impl FlakyExecutor {
        fn new(fail_first: &[(u32, u32)]) -> Self {
            Self {
                fail_first: fail_first.iter().copied().collect(),
                state: Arc::new(Mutex::new(FlakyState::default())),
            }
        }

        fn attempts(&self, id: u32) -> u32 {
            self.state
                .lock()
                .expect("flaky state lock")
                .attempts
                .get(&id)
                .copied()
                .unwrap_or(0)
        }
    }

    impl NodeExecutor for FlakyExecutor {
        fn execute(
            &self,
            node: TaskNode,
            _resolved_args: Value,
        ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, String>> + Send>> {
            let id = node.id.value();
            let fail_first = self.fail_first.get(&id).copied().unwrap_or(0);
            let state = Arc::clone(&self.state);
            Box::pin(async move {
                let n = {
                    let mut guard = state.lock().expect("flaky state lock");
                    let counter = guard.attempts.entry(id).or_insert(0);
                    *counter += 1;
                    *counter
                };
                if n <= fail_first {
                    return Err(format!("node {id} attempt {n} failed"));
                }
                Ok(ToolOutput {
                    content: vec![ContentBlock::Text(TextContent::new(format!("out-{id}")))],
                    details: None,
                    is_error: false,
                })
            })
        }
    }

    fn run_with_retry(
        executor: Arc<FlakyExecutor>,
        graph: TaskGraph,
        n: usize,
        attempts: u32,
    ) -> Result<(), ScheduleError> {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let executor: Arc<dyn NodeExecutor> = executor;
        let mut scheduler =
            DagScheduler::new(graph, executor, n).with_retry(attempts, Duration::ZERO);
        rt.block_on(async { scheduler.run().await })
    }

    /// 只读节点失败后重试，直到成功。
    #[test]
    fn read_node_retries_until_success() {
        let graph =
            TaskGraph::build(vec![node(1, "read", &[], ToolEffects::read())]).expect("valid graph");
        let executor = Arc::new(FlakyExecutor::new(&[(1, 2)])); // 前两次失败
        let result = run_with_retry(Arc::clone(&executor), graph, 1, 5);
        assert!(
            result.is_ok(),
            "read node should succeed within the retry budget"
        );
        assert_eq!(executor.attempts(1), 3, "third attempt should succeed");
    }

    /// 重试用尽仍失败 → Failed，下游级联 Skipped，不越权执行。
    #[test]
    fn read_node_exhausts_retries_then_skips_downstream() {
        let graph = TaskGraph::build(vec![
            node(1, "read", &[], ToolEffects::read()),
            node(2, "read", &[1], ToolEffects::read()),
        ])
        .expect("valid graph");
        let executor = Arc::new(FlakyExecutor::new(&[(1, 99)])); // 恒失败
        let result = run_with_retry(Arc::clone(&executor), graph, 1, 3);
        assert!(matches!(
            result,
            Err(ScheduleError::Aggregate {
                failed: 1,
                skipped: 1,
                cancelled: 0
            })
        ));
        assert_eq!(
            executor.attempts(1),
            3,
            "attempts must equal the configured limit"
        );
        assert_eq!(executor.attempts(2), 0, "skipped downstream must not run");
    }

    /// barrier 节点永不重试：一次即定，避免重放副作用。
    #[test]
    fn barrier_node_is_never_retried() {
        let graph = TaskGraph::build(vec![node(1, "bash", &[], ToolEffects::process())])
            .expect("valid graph");
        let executor = Arc::new(FlakyExecutor::new(&[(1, 99)])); // 恒失败
        let result = run_with_retry(Arc::clone(&executor), graph, 1, 5);
        assert!(result.is_err());
        assert_eq!(
            executor.attempts(1),
            1,
            "a write/process node must run exactly once even with retries configured"
        );
    }

    // ---- 动态槽位 ----

    /// Yields `yields[id]` times before completing, recording enter/exit.
    #[derive(Default)]
    struct YieldsState {
        timeline: Vec<(u32, bool)>,
    }

    struct YieldsExecutor {
        yields: HashMap<u32, usize>,
        state: Arc<Mutex<YieldsState>>,
    }

    impl NodeExecutor for YieldsExecutor {
        fn execute(
            &self,
            node: TaskNode,
            _resolved_args: Value,
        ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, String>> + Send>> {
            let id = node.id.value();
            let count = self.yields.get(&id).copied().unwrap_or(1);
            let state = Arc::clone(&self.state);
            Box::pin(async move {
                state.lock().expect("yields lock").timeline.push((id, true));
                for _ in 0..count {
                    YieldOnce(false).await;
                }
                state
                    .lock()
                    .expect("yields lock")
                    .timeline
                    .push((id, false));
                Ok(ToolOutput {
                    content: vec![ContentBlock::Text(TextContent::new(format!("out-{id}")))],
                    details: None,
                    is_error: false,
                })
            })
        }
    }

    /// A slow node must not hold the batch: a fast node queued behind it starts
    /// as soon as a slot frees. The old `chunks + join_all` would have started
    /// node 2 only after node 0 finished (a whole chunk later).
    #[test]
    fn slow_node_does_not_block_the_next_slot() {
        let graph = TaskGraph::build(vec![
            node(0, "read", &[], ToolEffects::read()),
            node(1, "read", &[], ToolEffects::read()),
            node(2, "read", &[], ToolEffects::read()),
        ])
        .expect("valid graph");
        // 0 is slow (many yields); 1 and 2 finish almost immediately.
        let state = Arc::new(Mutex::new(YieldsState::default()));
        let executor = Arc::new(YieldsExecutor {
            yields: HashMap::from([(0, 50), (1, 1), (2, 1)]),
            state: Arc::clone(&state),
        });
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let executor: Arc<dyn NodeExecutor> = executor;
        let mut scheduler = DagScheduler::new(graph, executor, 2);
        rt.block_on(async { scheduler.run().await })
            .expect("run should succeed");

        let timeline = state.lock().expect("yields lock").timeline.clone();
        let enter2 = timeline
            .iter()
            .position(|&(id, enter)| id == 2 && enter)
            .expect("node 2 must start");
        let exit0 = timeline
            .iter()
            .position(|&(id, enter)| id == 0 && !enter)
            .expect("node 0 must finish");
        assert!(
            enter2 < exit0,
            "node 2 must start while slow node 0 is still running (dynamic slot reclaim): {timeline:?}"
        );
    }
}
