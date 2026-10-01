//! Search backend comparison benches (`bd-cv653.1.5`).
//!
//! Engineering measurement (not a release claim): compares the in-process
//! grep/find backends against the external `rg`/`fd` escape hatch on a
//! synthetic source tree. The external lanes are skipped silently when the
//! binaries are not installed.
//!
//! It also prices the structural `ast_grep` backend against `grep` on the same
//! tree. That lane is deliberately a *full-scan, zero-match* query: ripgrep and
//! tree-sitter do the same amount of file walking, no result set is serialized
//! by either, and `ast_grep`'s 1000-match hard cap cannot make it stop early
//! while grep keeps going. Any matched-pattern lane would not be comparable,
//! because `ast_grep` returns at most `HARD_MATCH_LIMIT` matches and aborts the
//! scan on reaching it, so it would do strictly less work than grep.
//!
//! Run with: `cargo bench --bench search_backends`

#[path = "bench_env.rs"]
mod bench_env;

use criterion::{Criterion, criterion_group, criterion_main};
use ra::config::Config;
use ra::tools::ToolRegistry;
use std::fmt::Write as _;
use std::path::Path;

/// Lay out a synthetic tree: `dirs` directories x `files_per_dir` files, each
/// with `lines` lines, a needle on one line per file, plus a `.gitignore`
/// excluding one subtree.
fn build_fixture_tree(root: &Path, dirs: usize, files_per_dir: usize, lines: usize) {
    std::fs::write(root.join(".gitignore"), "ignored-dir/\n").expect("write gitignore");
    let ignored = root.join("ignored-dir");
    std::fs::create_dir_all(&ignored).expect("create ignored dir");
    std::fs::write(ignored.join("skip.rs"), "needle should not appear\n").expect("write ignored");

    for dir_index in 0..dirs {
        let dir = root.join(format!("mod_{dir_index:03}"));
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        for file_index in 0..files_per_dir {
            let mut content = String::with_capacity(lines * 24);
            for line_index in 0..lines {
                if line_index == lines / 2 {
                    let _ = writeln!(content, "    let needle_{file_index} = {line_index};");
                } else {
                    let _ = writeln!(content, "    let filler_{line_index} = {line_index};");
                }
            }
            std::fs::write(dir.join(format!("file_{file_index:03}.rs")), content)
                .expect("write fixture file");
        }
    }
}

fn registry_for(root: &Path, backend: &str) -> ToolRegistry {
    let config: Config = serde_json::from_value(serde_json::json!({
        "search_backend": backend,
    }))
    .expect("backend config");
    ToolRegistry::new(&["grep", "find", "ast_grep"], root, Some(&config))
}

/// Runs the tool once with a per-iteration `limit` nudge so the tool-output
/// cache key changes every call — the scan is what's being measured, not the
/// cache hit path.
fn run_tool(
    registry: &ToolRegistry,
    name: &str,
    mut input: serde_json::Value,
    iteration: &mut u64,
) {
    *iteration += 1;
    input["limit"] = serde_json::Value::Number(serde_json::Number::from(5000 + *iteration));
    let tool = registry.get(name).expect("tool registered");
    asupersync::test_utils::run_test(|| async {
        tool.execute("bench", input.clone(), None)
            .await
            .expect("tool run");
    });
}

/// `ast_grep` rejects any `limit` above its own `HARD_MATCH_LIMIT` (1000), so it
/// cannot share [`run_tool`]'s `5000 + iteration` cache-busting nudge, which the
/// tool would reject as out of range. This runner still varies the limit — so the
/// tool-output cache never answers twice — but stays inside the accepted range.
fn run_ast_tool(registry: &ToolRegistry, mut input: serde_json::Value, iteration: &mut u64) {
    *iteration += 1;
    input["limit"] = serde_json::Value::Number(serde_json::Number::from(1_000 - (*iteration % 10)));
    let tool = registry.get("ast_grep").expect("ast_grep registered");
    asupersync::test_utils::run_test(|| async {
        tool.execute("bench", input.clone(), None)
            .await
            .expect("ast_grep run");
    });
}

fn external_binaries_present() -> bool {
    let have = |names: &[&str]| {
        names.iter().any(|name| {
            std::process::Command::new(name)
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok()
        })
    };
    have(&["rg"]) && have(&["fd", "fdfind"])
}

fn bench_search_backends(c: &mut Criterion) {
    let tmp = tempfile::tempdir().expect("fixture tempdir");
    // ~2k files, ~60 lines each — big enough to exercise walking + matching,
    // small enough for CI.
    build_fixture_tree(tmp.path(), 40, 50, 60);
    let root = tmp.path();

    let grep_input = serde_json::json!({ "pattern": "needle_[0-9]+" });
    let find_input = serde_json::json!({ "pattern": "*.rs" });
    let mut iteration = 0u64;

    let mut group = c.benchmark_group("search_backends");
    group.sample_size(10);

    let inproc = registry_for(root, "inproc");
    group.bench_function("grep_inproc_2k_files", |b| {
        b.iter(|| run_tool(&inproc, "grep", grep_input.clone(), &mut iteration));
    });
    group.bench_function("find_inproc_2k_files", |b| {
        b.iter(|| run_tool(&inproc, "find", find_input.clone(), &mut iteration));
    });

    if external_binaries_present() {
        let external = registry_for(root, "external");
        group.bench_function("grep_external_2k_files", |b| {
            b.iter(|| run_tool(&external, "grep", grep_input.clone(), &mut iteration));
        });
        group.bench_function("find_external_2k_files", |b| {
            b.iter(|| run_tool(&external, "find", find_input.clone(), &mut iteration));
        });
    }

    // Head-to-head with the structural backend. Both patterns match nothing, so
    // each tool walks the same 2k files, reads the same bytes, and serializes an
    // empty result — the only difference left is the matcher itself (ripgrep's
    // literal/regex scan vs. tree-sitter parsing every file). `$$$ARGS` keeps the
    // pattern a syntactically valid Rust call expression, which is the shape
    // `ast_grep` requires.
    let nomatch_grep = serde_json::json!({ "pattern": "zzz_absent_symbol" });
    let nomatch_ast = serde_json::json!({ "pattern": "zzz_absent_symbol($$$ARGS)" });
    group.bench_function("grep_inproc_nomatch_2k_files", |b| {
        b.iter(|| run_tool(&inproc, "grep", nomatch_grep.clone(), &mut iteration));
    });
    group.bench_function("ast_grep_inproc_nomatch_2k_files", |b| {
        b.iter(|| run_ast_tool(&inproc, nomatch_ast.clone(), &mut iteration));
    });

    group.finish();
}

criterion_group!(
    name = benches;
    config = bench_env::criterion_config();
    targets = bench_search_backends
);
criterion_main!(benches);
