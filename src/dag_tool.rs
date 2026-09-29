//! `dag` 工具（host-coupled）：模型在一次调用中给出完整图，
//! 本实例内由 [`DagScheduler`] 三层调度执行——无子进程、无新实例。
//!
//! 结果经 [`ToolOutput`] → `record_tool_result` 回传主 Agent（D9），
//! 失败/跳过节点整理在聚合输出里，由主 Agent 决定下一步。

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

use crate::dag_scheduler::{DagScheduler, NodeExecutor, ScheduleError, text_of};
use crate::error::{Error, Result};
use crate::model::{ContentBlock, TextContent};
use crate::task_dag::{
    FORBIDDEN_NODE_TOOLS, MAX_DAG_DEPTH, MAX_DAG_NODES, MAX_LAYER_WIDTH, TaskGraph, TaskNode,
    TaskNodeId, TaskNodeState,
};
use crate::tools::{SharedToolRegistry, SharedToolRegistryInner, Tool, ToolOutput, ToolUpdate};

/// Shared per-call emit plumbing for the three `ra.dag.*` schemas.
type DagEmit = Arc<dyn Fn(ToolUpdate) + Send + Sync>;

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
}

impl DagTool {
    #[must_use]
    pub fn new(registry: &SharedToolRegistry) -> Self {
        Self {
            registry: registry.downgrade(),
        }
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
         optional `args`, and optional `dependsOn` (ids that must succeed first). \
         Independent nodes run concurrently (bounded by the configured limit); tools with \
         write/append/process side effects are serialized automatically. When a node fails, \
         its downstream nodes are skipped and the whole report is returned for you to decide \
         the next step. Nodes may not invoke `dag` itself."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "required": ["nodes"],
            "properties": {
                "nodes": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["id", "toolName"],
                        "properties": {
                            "id": {"type": "integer", "minimum": 0},
                            "toolName": {"type": "string"},
                            "args": {},
                            "dependsOn": {
                                "type": "array",
                                "items": {"type": "integer", "minimum": 0}
                            }
                        },
                        "additionalProperties": false
                    }
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
        let call_id = tool_call_id.to_string();
        Self::run_dag(weak, call_id, input, on_update).await
    }
    // effects() 走默认 write()：dag 自身是 barrier，主循环天然串行它。
}

impl DagTool {
    /// 实际执行逻辑：inherent async fn + owned 参数（见 `execute` 内注释）。
    async fn run_dag(
        weak: std::sync::Weak<SharedToolRegistryInner>,
        tool_call_id: String,
        input: Value,
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let on_update: Option<DagEmit> = on_update.map(Arc::from);
        let seq = Arc::new(AtomicU64::new(0));
        // 1. 解析 wire 输入（camelCase 对应 TaskNode serde）。
        let nodes_value = input
            .get("nodes")
            .cloned()
            .ok_or_else(|| Error::validation("dag requires a `nodes` array"))?;
        let mut nodes: Vec<TaskNode> = serde_json::from_value(nodes_value)
            .map_err(|err| Error::validation(format!("dag nodes are malformed: {err}")))?;

        // 2. 递归防护优先（明确报错），再从 registry 解析副作用集。
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

        // 3. 构建期硬门禁：ID 唯一 / 依赖存在 / 环 / 深度 100 / 节点数 / 层宽。
        let node_meta: Vec<TaskNode> = nodes.clone();
        let graph = TaskGraph::build(nodes)
            .map_err(|err| Error::validation(format!("dag graph rejected: {err}")))?;

        // id → 拓扑层号（node_state 消息需要；在 graph 移交 scheduler 前算好）。
        let layer_of: HashMap<TaskNodeId, usize> = graph
            .layers()
            .iter()
            .enumerate()
            .flat_map(|(layer, ids)| ids.iter().map(move |id| (*id, layer)))
            .collect();

        // 首帧即拓扑（M4：ra.dag.topology.v1），保证前端先拿到图形状。
        if let Some(emit) = &on_update {
            emit_topology(emit, &tool_call_id, &graph, &layer_of);
        }

        // 4. 本实例三层调度执行。
        let executor = Arc::new(RegistryExecutor {
            registry,
            dag_call_id: tool_call_id.clone(),
            on_update: on_update.clone(),
            seq: Arc::clone(&seq),
        });
        let mut scheduler = DagScheduler::with_default_concurrency(graph, executor);
        // 节点状态迁移 → ra.dag.node_state.v1（低频，同样带共享 seq）。
        {
            let emit = on_update.clone();
            let seq = Arc::clone(&seq);
            let graph_id = tool_call_id.clone();
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

        // 5. 聚合 per-node 报告（在消费 scheduler 之前收集状态与失败原因）。
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
        Ok(aggregate(&report, &outputs))
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
fn aggregate(
    report: &[(TaskNodeId, String, Option<TaskNodeState>, Option<String>)],
    outputs: &crate::dag_scheduler::TaskOutputStore,
) -> ToolOutput {
    let mut lines = Vec::new();
    let mut node_results = Vec::new();
    let mut failed = 0usize;
    let mut skipped = 0usize;
    let mut succeeded = 0usize;
    let mut cancelled = 0usize;

    for (id, tool_name, state, failure) in report {
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
        } else {
            lines.push(format!("node {} ({tool_name}): {status}", id.value()));
        }

        if let Some(output) = outputs.get(*id) {
            let text = text_of(output);
            if !text.is_empty() {
                entry["output"] = Value::String(text.clone());
                if status == "succeeded" {
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
    lines.insert(0, summary);
    let is_error = failed > 0 || skipped > 0 || cancelled > 0;
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(lines.join("\n")))],
        details: Some(json!({
            "schema": "ra.dag.result.v1",
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
    fn tool_over(shared: &SharedToolRegistry) -> DagTool {
        DagTool::new(shared)
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
}
