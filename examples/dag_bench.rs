#![recursion_limit = "256"]
//! `dag_bench` — quantifies how much wall-clock time the `dag` tool saves over
//! issuing the same tool calls one at a time.
//!
//! The comparison is *single-invocation*: the sequential baseline and the DAG
//! run execute in the same process, through the same [`DagScheduler`], differing
//! only in the concurrency limit. `concurrency = 1` reproduces the agent main
//! loop's strictly-sequential tool latency (one tool finishes before the next
//! starts); `concurrency = C` is what the `dag` tool does.
//!
//! Node latency is simulated with an async [`Delay`] future so independent nodes
//! genuinely overlap on a `current_thread` runtime — a blocking `sleep` inside a
//! node would serialize on the single runtime thread and hide the effect.
//!
//! Run with:
//!
//! ```sh
//! cargo run --release --example dag_bench
//! cargo run --release --example dag_bench -- --json
//! cargo run --release --example dag_bench -- --latency-ms 100 --concurrency 16
//! ```
//!
//! Reported metrics per scenario:
//!
//! * `serial_ms` — concurrency 1 (the agent-loop baseline for the tool leg).
//! * `dag_ms` — concurrency C.
//! * `speedup` — `serial_ms / dag_ms`.
//! * `peak` — measured peak simultaneous node execution (proves real overlap).
//! * `*_tot_ms` — the tool leg plus the model round-trip cost `R`: a sequential
//!   agent pays `R` per tool call (`N·R`), while `dag` pays `R` once for the
//!   whole graph. This term usually dominates, because the model cannot issue
//!   the next call until the previous tool result comes back.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use ra::dag_scheduler::{DagScheduler, NodeExecutor};
use ra::model::{ContentBlock, TextContent};
use ra::task_dag::{TaskGraph, TaskNode, TaskNodeId};
use ra::tools::{ToolEffects, ToolOutput};
use serde_json::{Value, json};

/// `usize` -> `f64` for reporting only; every count here is < 2^53 so the
/// conversion is exact. Kept explicit because `cast_precision_loss` is a
/// default-on pedantic lint for this crate.
#[allow(clippy::cast_precision_loss)]
fn to_f64(n: usize) -> f64 {
    n as f64
}

/// `u64` -> `f64` for reporting only (scenario shape units, always < 2^53).
#[allow(clippy::cast_precision_loss)]
fn units_to_f64(n: u64) -> f64 {
    n as f64
}

// ============================================================================
// Async delay: overlaps independent nodes without blocking the runtime thread.
// ============================================================================

struct Delay {
    dur: Duration,
    started: bool,
    done: Arc<AtomicU64>,
    waker: Arc<std::sync::Mutex<Option<Waker>>>,
}

impl Delay {
    fn new(dur: Duration) -> Self {
        Self {
            dur,
            started: false,
            done: Arc::new(AtomicU64::new(0)),
            waker: Arc::new(std::sync::Mutex::new(None)),
        }
    }
}

impl Future for Delay {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.dur.is_zero() || self.done.load(Ordering::SeqCst) == 1 {
            return Poll::Ready(());
        }
        {
            let mut slot = self.waker.lock().expect("waker lock");
            *slot = Some(cx.waker().clone());
        }
        if self.done.load(Ordering::SeqCst) == 1 {
            return Poll::Ready(());
        }
        if !self.started {
            self.started = true;
            let done = Arc::clone(&self.done);
            let waker = Arc::clone(&self.waker);
            let dur = self.dur;
            std::thread::spawn(move || {
                std::thread::sleep(dur);
                done.store(1, Ordering::SeqCst);
                let pending = waker.lock().expect("waker lock").take();
                if let Some(waker) = pending {
                    waker.wake();
                }
            });
        }
        Poll::Pending
    }
}

// ============================================================================
// Counting executor
// ============================================================================

#[derive(Default)]
struct Counters {
    current: AtomicUsize,
    peak: AtomicUsize,
    started: AtomicU64,
    service_ns: AtomicU64,
}

impl Counters {
    fn enter(&self) {
        let now = self.current.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        self.started.fetch_add(1, Ordering::SeqCst);
    }

    fn exit(&self, elapsed: Duration) {
        self.current.fetch_sub(1, Ordering::SeqCst);
        let nanos = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
        self.service_ns.fetch_add(nanos, Ordering::SeqCst);
    }

    fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
}

struct TimedExecutor {
    latency: Duration,
    counters: Arc<Counters>,
}

impl NodeExecutor for TimedExecutor {
    fn execute(
        &self,
        node: TaskNode,
        _resolved_args: Value,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, String>> + Send>> {
        let counters = Arc::clone(&self.counters);
        let latency = self.latency;
        Box::pin(async move {
            counters.enter();
            let started = Instant::now();
            Delay::new(latency).await;
            counters.exit(started.elapsed());
            Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(format!(
                    "node-{}",
                    node.id.value()
                )))],
                details: Some(json!({ "value": node.id.value() })),
                is_error: false,
            })
        })
    }
}

// ============================================================================
// Graph builders
// ============================================================================

fn node(id: u32, deps: &[u32], effects: ToolEffects) -> TaskNode {
    TaskNode {
        id: TaskNodeId::new(id),
        tool_name: if effects.reads() { "read" } else { "bash" }.to_string(),
        name: String::new(),
        args: json!({}),
        depends_on: deps.iter().copied().map(TaskNodeId::new).collect(),
        effects,
    }
}

/// `n` independent nodes of the given effect, ids `0..n`.
fn wide(n: u32, effects: ToolEffects) -> Vec<TaskNode> {
    (0..n).map(|id| node(id, &[], effects)).collect()
}

/// `1 -> w (parallel) -> 1` diamond; ids 0, then 1..=w, then w+1.
fn diamond(w: u32, effects: ToolEffects) -> Vec<TaskNode> {
    let mut nodes = vec![node(0, &[], effects)];
    for id in 1..=w {
        nodes.push(node(id, &[0], effects));
    }
    let middle: Vec<u32> = (1..=w).collect();
    nodes.push(node(w + 1, &middle, effects));
    nodes
}

/// A chain of `n` nodes; critical path length `n` — parallelism cannot help.
fn chain(n: u32, effects: ToolEffects) -> Vec<TaskNode> {
    (0..n)
        .map(|id| node(id, if id == 0 { &[] } else { &[id - 1] }, effects))
        .collect()
}

fn scenarios(read: ToolEffects, process: ToolEffects) -> Vec<Scenario> {
    vec![
        Scenario {
            name: "wide_read_8",
            nodes: wide(8, read),
            ideal_units_at_c: 1, // one wave of 8 at C=8
        },
        Scenario {
            name: "wide_read_64",
            nodes: wide(64, read),
            ideal_units_at_c: 8, // ceil(64/8) waves
        },
        Scenario {
            name: "wide_barrier_8",
            nodes: wide(8, process),
            ideal_units_at_c: 8, // barriers never overlap: fully serial by design
        },
        Scenario {
            name: "mixed_4read_4barrier",
            nodes: {
                let mut nodes = wide(4, read);
                for source in wide(4, process) {
                    nodes.push(node(source.id.value() + 4, &[], process));
                }
                nodes
            },
            ideal_units_at_c: 5, // 4 barriers serialize + 1 wave of 4 reads
        },
        Scenario {
            name: "diamond_w8",
            nodes: diamond(8, read),
            ideal_units_at_c: 3, // 1 -> 8 -> 1
        },
        Scenario {
            name: "chain_8",
            nodes: chain(8, read),
            ideal_units_at_c: 8, // critical path
        },
    ]
}

// ============================================================================
// Harness
// ============================================================================

struct Scenario {
    name: &'static str,
    nodes: Vec<TaskNode>,
    /// Ideal wall time at concurrency `C` in units of `latency`.
    ideal_units_at_c: u64,
}

struct Row {
    name: &'static str,
    nodes: usize,
    concurrency: usize,
    serial_ms: f64,
    dag_ms: f64,
    peak: usize,
    ideal_ms: f64,
    serial_total_ms: f64,
    dag_total_ms: f64,
}

fn run_once(
    rt: &asupersync::runtime::Runtime,
    executor: Arc<TimedExecutor>,
    graph: TaskGraph,
    concurrency: usize,
) -> Duration {
    let dyn_executor: Arc<dyn NodeExecutor> = executor;
    let mut scheduler = DagScheduler::new(graph, dyn_executor, concurrency);
    let started = Instant::now();
    rt.block_on(async { scheduler.run().await })
        .expect("benchmark graph must succeed");
    started.elapsed()
}

/// Best-of-`repeats` run of one scenario: serial (concurrency 1) vs DAG.
///
/// Min is the right estimator for a latency floor: a slower run only ever adds
/// scheduler/OS noise.
fn run_scenario(
    rt: &asupersync::runtime::Runtime,
    scenario: &Scenario,
    latency: Duration,
    concurrency: usize,
    repeats: usize,
    roundtrip_ms: f64,
) -> Row {
    let mut serial = Duration::MAX;
    let mut dag = Duration::MAX;
    let mut peak = 0usize;

    for _ in 0..repeats {
        let serial_counters = Arc::new(Counters::default());
        let serial_executor = Arc::new(TimedExecutor {
            latency,
            counters: Arc::clone(&serial_counters),
        });
        let graph = TaskGraph::build(scenario.nodes.clone()).expect("graph builds");
        serial = serial.min(run_once(rt, serial_executor, graph, 1));

        let dag_counters = Arc::new(Counters::default());
        let dag_executor = Arc::new(TimedExecutor {
            latency,
            counters: Arc::clone(&dag_counters),
        });
        let graph = TaskGraph::build(scenario.nodes.clone()).expect("graph builds");
        dag = dag.min(run_once(rt, dag_executor, graph, concurrency));
        peak = peak.max(dag_counters.peak());
    }

    let node_count = scenario.nodes.len();
    let serial_ms = serial.as_secs_f64() * 1000.0;
    let dag_ms = dag.as_secs_f64() * 1000.0;
    let ideal_ms = units_to_f64(scenario.ideal_units_at_c) * latency.as_secs_f64() * 1000.0;
    // Model round-trip leg: serial needs one model turn per tool call, DAG
    // needs exactly one turn for the whole graph.
    let serial_total_ms = serial_ms + to_f64(node_count) * roundtrip_ms;
    let dag_total_ms = dag_ms + roundtrip_ms;

    Row {
        name: scenario.name,
        nodes: node_count,
        concurrency,
        serial_ms,
        dag_ms,
        peak,
        ideal_ms,
        serial_total_ms,
        dag_total_ms,
    }
}

fn row_json(r: &Row) -> Value {
    json!({
        "scenario": r.name,
        "nodes": r.nodes,
        "concurrency": r.concurrency,
        "serialMs": r.serial_ms,
        "dagMs": r.dag_ms,
        "speedup": r.serial_ms / r.dag_ms,
        "peakConcurrency": r.peak,
        "idealMs": r.ideal_ms,
        "serialTotalMs": r.serial_total_ms,
        "dagTotalMs": r.dag_total_ms,
        "endToEndSpeedup": r.serial_total_ms / r.dag_total_ms,
    })
}

fn print_table(
    rows: &[Row],
    latency_ms: f64,
    concurrency: usize,
    repeats: usize,
    roundtrip_ms: f64,
) {
    println!(
        "dag_bench  latency={latency_ms}ms  concurrency={concurrency}  \
         repeats={repeats}  model-round-trip={roundtrip_ms}ms\n"
    );
    println!(
        "{:<22} {:>5} {:>10} {:>10} {:>9} {:>6} {:>9} {:>14} {:>14} {:>10}",
        "scenario",
        "nodes",
        "serial_ms",
        "dag_ms",
        "speedup",
        "peak",
        "ideal_ms",
        "serial_tot_ms",
        "dag_tot_ms",
        "e2e_x"
    );
    for r in rows {
        println!(
            "{:<22} {:>5} {:>10.1} {:>10.1} {:>8.2}x {:>6} {:>9.1} {:>14.1} {:>14.1} {:>9.2}x",
            r.name,
            r.nodes,
            r.serial_ms,
            r.dag_ms,
            r.serial_ms / r.dag_ms,
            r.peak,
            r.ideal_ms,
            r.serial_total_ms,
            r.dag_total_ms,
            r.serial_total_ms / r.dag_total_ms,
        );
    }
    println!(
        "\nserial_ms = tool leg at concurrency 1 (agent-loop baseline)   \
         dag_ms = tool leg at concurrency C\n\
         peak     = measured peak simultaneous nodes (proves real overlap)\n\
         *_tot_ms = tool leg + model round-trips (serial: N turns, dag: 1 turn)\n\
         e2e_x    = end-to-end speedup including model round-trips"
    );
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let flag = |name: &str| argv.iter().any(|a| a == name);
    let value = |name: &str, default: u64| -> u64 {
        argv.iter()
            .position(|a| a == name)
            .and_then(|i| argv.get(i + 1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };

    let json_mode = flag("--json");
    let latency = Duration::from_millis(value("--latency-ms", 60));
    let concurrency = usize::try_from(value("--concurrency", 8)).unwrap_or(8);
    let repeats = usize::try_from(value("--repeats", 3).max(1)).unwrap_or(3);
    let roundtrip_ms = to_f64(usize::try_from(value("--roundtrip-ms", 800)).unwrap_or(800));

    let rt = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("current_thread runtime");

    let rows: Vec<Row> = scenarios(ToolEffects::read(), ToolEffects::process())
        .iter()
        .map(|scenario| run_scenario(&rt, scenario, latency, concurrency, repeats, roundtrip_ms))
        .collect();

    if json_mode {
        let payload: Vec<Value> = rows.iter().map(row_json).collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema": "ra.dag.bench.v1",
                "latencyMs": latency.as_secs_f64() * 1000.0,
                "concurrency": concurrency,
                "repeats": repeats,
                "modelRoundTripMs": roundtrip_ms,
                "rows": payload,
            }))
            .expect("json")
        );
        return;
    }

    print_table(
        &rows,
        latency.as_secs_f64() * 1000.0,
        concurrency,
        repeats,
        roundtrip_ms,
    );
}
