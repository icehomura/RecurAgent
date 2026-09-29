#![forbid(unsafe_code)]

use serde_json::Value;
use std::collections::HashSet;
use std::path::PathBuf;

const CONTRACT_PATH: &str =
    "docs/contracts/proof-carrying-swarm-test-fabric-closeout-gate-contract.json";
const EVIDENCE_PATH: &str = "docs/evidence/proof-carrying-swarm-test-fabric-closeout-gate.json";
const RUNBOOK_PATH: &str = "docs/swarm-operations-runbook.md";
const README_PATH: &str = "README.md";
const EXPECTED_CONTRACT_SCHEMA: &str =
    "ra.swarm.proof_carrying_test_fabric.closeout_gate_contract.v1";
const EXPECTED_EVIDENCE_SCHEMA: &str = "ra.swarm.proof_carrying_test_fabric.closeout_gate.v1";
const EXPECTED_PURPOSE: &str =
    "prompt_to_artifact_proof_carrying_swarm_test_fabric_closeout_gate_not_source_of_truth";

type TestResult<T = ()> = Result<T, String>;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn load_json(path: &str) -> TestResult<Value> {
    let full_path = repo_root().join(path);
    let raw = std::fs::read_to_string(&full_path)
        .map_err(|err| format!("failed to read {}: {err}", full_path.display()))?;
    serde_json::from_str(&raw)
        .map_err(|err| format!("failed to parse {} as JSON: {err}", full_path.display()))
}

fn load_text(path: &str) -> TestResult<String> {
    let full_path = repo_root().join(path);
    std::fs::read_to_string(&full_path)
        .map_err(|err| format!("failed to read {}: {err}", full_path.display()))
}

fn require(condition: bool, message: impl Into<String>) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}

fn require_lazy(condition: bool, message: impl FnOnce() -> String) -> TestResult {
    if condition { Ok(()) } else { Err(message()) }
}

fn pointer<'a>(value: &'a Value, path: &str) -> TestResult<&'a Value> {
    value
        .pointer(path)
        .ok_or_else(|| format!("missing JSON pointer {path}"))
}

fn pointer_mut<'a>(value: &'a mut Value, path: &str) -> TestResult<&'a mut Value> {
    value
        .pointer_mut(path)
        .ok_or_else(|| format!("missing mutable JSON pointer {path}"))
}

fn pointer_str<'a>(value: &'a Value, path: &str) -> TestResult<&'a str> {
    pointer(value, path)?
        .as_str()
        .ok_or_else(|| format!("{path} must be a string"))
}

fn pointer_bool(value: &Value, path: &str) -> TestResult<bool> {
    pointer(value, path)?
        .as_bool()
        .ok_or_else(|| format!("{path} must be a bool"))
}

fn pointer_array<'a>(value: &'a Value, path: &str) -> TestResult<&'a [Value]> {
    pointer(value, path)?
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| format!("{path} must be an array"))
}

fn pointer_array_mut<'a>(value: &'a mut Value, path: &str) -> TestResult<&'a mut Vec<Value>> {
    pointer_mut(value, path)?
        .as_array_mut()
        .ok_or_else(|| format!("{path} must be an array"))
}

fn string_set<'a>(value: &'a Value, path: &str) -> TestResult<HashSet<&'a str>> {
    let mut entries = HashSet::new();
    for entry in pointer_array(value, path)? {
        let raw = entry
            .as_str()
            .ok_or_else(|| format!("{path} entries must be strings"))?;
        let normalized = raw.trim();
        require(
            !normalized.is_empty(),
            format!("{path} entries must be non-empty"),
        )?;
        entries.insert(normalized);
    }
    Ok(entries)
}

fn is_hex_commit(value: &str) -> bool {
    value.len() == 40 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn is_hex_commitish(value: &str) -> bool {
    (7..=40).contains(&value.len()) && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn require_hex_commit(value: &Value, path: &str) -> TestResult {
    let commit = pointer_str(value, path)?;
    require(
        is_hex_commit(commit),
        format!("{path} must be a 40-character hex commit, got {commit}"),
    )
}

fn require_hex_commitish(value: &Value, path: &str) -> TestResult {
    let commit = pointer_str(value, path)?;
    require(
        is_hex_commitish(commit),
        format!("{path} must be a hex commit or short commit, got {commit}"),
    )
}

fn require_non_empty_array(value: &Value, path: &str) -> TestResult {
    require(
        !pointer_array(value, path)?.is_empty(),
        format!("{path} must not be empty"),
    )
}

fn require_existing_paths(row: &Value, path: &str) -> TestResult {
    for entry in pointer_array(row, path)? {
        let relative_path = entry
            .as_str()
            .ok_or_else(|| format!("{path} entries must be strings"))?;
        require_lazy(repo_root().join(relative_path).exists(), || {
            format!("{path} entry does not exist: {relative_path}")
        })?;
    }
    Ok(())
}

fn checklist_row<'a>(evidence: &'a Value, id: &str) -> TestResult<&'a Value> {
    pointer_array(evidence, "/checklist")?
        .iter()
        .find(|row| row.pointer("/id").and_then(Value::as_str) == Some(id))
        .ok_or_else(|| format!("missing checklist row {id}"))
}

fn checklist_row_mut<'a>(evidence: &'a mut Value, id: &str) -> TestResult<&'a mut Value> {
    pointer_array_mut(evidence, "/checklist")?
        .iter_mut()
        .find(|row| row.pointer("/id").and_then(Value::as_str) == Some(id))
        .ok_or_else(|| format!("missing mutable checklist row {id}"))
}

fn quality_gate_row_mut<'a>(evidence: &'a mut Value, id: &str) -> TestResult<&'a mut Value> {
    pointer_array_mut(evidence, "/quality_gate_results")?
        .iter_mut()
        .find(|row| row.pointer("/id").and_then(Value::as_str) == Some(id))
        .ok_or_else(|| format!("missing mutable quality gate row {id}"))
}

fn checklist_ids(contract: &Value) -> TestResult<HashSet<&str>> {
    string_set(contract, "/required_check_ids")
}

fn child_ids(contract: &Value) -> TestResult<HashSet<&str>> {
    string_set(contract, "/required_child_bead_ids")
}

fn quality_gate_ids(contract: &Value) -> TestResult<HashSet<&str>> {
    string_set(contract, "/required_quality_gate_ids")
}

fn quality_gate_requires_rch(id: &str) -> bool {
    matches!(
        id,
        "proof_carrying_swarm_test_fabric_closeout_gate_contract_rch"
            | "cargo_check_all_targets_rch"
            | "cargo_clippy_all_targets_rch"
    )
}

fn verify_identity_and_required_keys(contract: &Value, evidence: &Value) -> TestResult {
    require(
        pointer_str(contract, "/schema")? == EXPECTED_CONTRACT_SCHEMA,
        "contract schema mismatch",
    )?;
    require(
        pointer_str(contract, "/decision_gate_schema")? == EXPECTED_EVIDENCE_SCHEMA,
        "contract decision_gate_schema mismatch",
    )?;
    require(
        pointer_str(contract, "/purpose")? == EXPECTED_PURPOSE,
        "contract purpose mismatch",
    )?;
    require(
        pointer_str(evidence, "/schema")? == EXPECTED_EVIDENCE_SCHEMA,
        "evidence schema mismatch",
    )?;
    require(
        pointer_str(evidence, "/purpose")? == EXPECTED_PURPOSE,
        "evidence purpose mismatch",
    )?;
    require(
        pointer_str(evidence, "/status")? == "pass",
        "evidence status must be pass",
    )?;
    require(
        pointer_str(evidence, "/parent_epic/id")? == "bd-zeccr",
        "parent epic id mismatch",
    )?;
    require(
        pointer_str(evidence, "/final_gate_bead/id")? == "bd-zeccr.6",
        "final gate bead id mismatch",
    )?;
    require(
        pointer_bool(evidence, "/epic_can_close_after_this_commit")?,
        "passing closeout gate must allow parent close after this commit lands",
    )?;

    for key in string_set(contract, "/required_top_level_keys")? {
        require_lazy(evidence.get(key).is_some(), || {
            format!("evidence missing required top-level key {key}")
        })?;
    }
    Ok(())
}

fn verify_child_artifact_map(contract: &Value, evidence: &Value) -> TestResult {
    let required_children = child_ids(contract)?;
    let child_map = pointer_array(evidence, "/child_artifact_map")?;
    require(
        child_map.len() == required_children.len(),
        "child_artifact_map must have exactly one row per required child",
    )?;

    let mut observed = HashSet::new();
    for row in child_map {
        let bead = pointer_str(row, "/bead_id")?;
        require_lazy(required_children.contains(bead), || {
            format!("unexpected child bead mapping {bead}")
        })?;
        require_lazy(observed.insert(bead), || {
            format!("duplicate child bead mapping {bead}")
        })?;
        require_lazy(pointer_str(row, "/status")? == "closed", || {
            format!("{bead} must be closed")
        })?;
        require_lazy(
            !pointer_str(row, "/close_reason")?.trim().is_empty(),
            || format!("{bead} close_reason must be non-empty"),
        )?;
        require_hex_commitish(row, "/commit")?;
        require_existing_paths(row, "/code_paths")?;
        require_existing_paths(row, "/test_paths")?;
        require_existing_paths(row, "/docs_or_evidence_paths")?;
        require_non_empty_array(row, "/validation_commands")?;
        require(
            !pointer_str(row, "/negative_control/id")?.trim().is_empty(),
            format!("{bead} negative_control id must be non-empty"),
        )?;
        require_non_empty_array(row, "/negative_control/evidence")?;
        require(
            !pointer_str(row, "/claim_boundary_text")?.trim().is_empty(),
            format!("{bead} claim_boundary_text must be non-empty"),
        )?;
    }

    require(
        observed == required_children,
        "child_artifact_map ids must exactly match required child bead ids",
    )
}

fn verify_closeout_outcome(evidence: &Value, required_checks: &HashSet<&str>) -> TestResult {
    require(
        string_set(evidence, "/required_checks")?.eq(required_checks),
        "required_checks must exactly match the contract",
    )?;
    require(
        pointer_array(evidence, "/missing_checks")?.is_empty(),
        "missing_checks must be empty for a passing gate",
    )?;
    require(
        pointer_array(evidence, "/remaining_follow_ups")?.is_empty(),
        "remaining_follow_ups must be empty for a passing gate",
    )?;
    require(
        !pointer_bool(evidence, "/follow_up_required")?,
        "follow_up_required must be false for a passing gate",
    )?;
    require(
        pointer_array(evidence, "/follow_up_beads")?.is_empty(),
        "follow_up_beads must be empty for a passing gate",
    )
}

fn verify_known_limitations(evidence: &Value) -> TestResult {
    for required_fragment in [
        "Agent Mail",
        "advisory operator evidence",
        "strict drop-in",
        "does not authorize",
    ] {
        require_lazy(
            pointer_array(evidence, "/known_limitations")?
                .iter()
                .any(|entry| {
                    entry
                        .as_str()
                        .is_some_and(|text| text.contains(required_fragment))
                }),
            || format!("known_limitations must contain {required_fragment:?}"),
        )?;
    }
    Ok(())
}

fn verify_negative_control_coverage(evidence: &Value) -> TestResult {
    for (id, required_fragment) in [
        ("no_mock_lifecycle_e2e", "fails closed"),
        ("cross_surface_conformance_matrix", "negative"),
        ("golden_operator_artifacts", "negative"),
        ("structured_input_fuzz_harness", "unsupported"),
        ("metamorphic_replay_equivalence", "negative"),
    ] {
        let row = checklist_row(evidence, id)?;
        let requirement = pointer_str(row, "/requirement")?;
        require_lazy(requirement.contains(required_fragment), || {
            format!("checklist row {id} must describe {required_fragment} coverage")
        })?;
    }
    Ok(())
}

fn verify_checklist_rows(evidence: &Value, required_checks: &HashSet<&str>) -> TestResult {
    let mut observed = HashSet::new();
    for row in pointer_array(evidence, "/checklist")? {
        let id = pointer_str(row, "/id")?;
        require_lazy(required_checks.contains(id), || {
            format!("unexpected checklist id {id}")
        })?;
        require_lazy(pointer_str(row, "/status")? == "pass", || {
            format!("checklist row {id} must pass")
        })?;
        require_non_empty_array(row, "/evidence")?;
        observed.insert(id);
    }
    require(
        observed == *required_checks,
        "checklist ids must exactly match required checks",
    )
}

fn verify_quality_gate_rows(
    evidence: &Value,
    required_quality_gates: &HashSet<&str>,
) -> TestResult {
    let mut observed = HashSet::new();
    for gate in pointer_array(evidence, "/quality_gate_results")? {
        let id = pointer_str(gate, "/id")?;
        let command = pointer_str(gate, "/command")?;
        require_lazy(required_quality_gates.contains(id), || {
            format!("unexpected quality gate id {id}")
        })?;
        require_lazy(pointer_str(gate, "/status")? == "pass", || {
            format!("quality gate {id} must pass")
        })?;
        require_lazy(!command.trim().is_empty(), || {
            format!("quality gate {id} command must be non-empty")
        })?;
        if quality_gate_requires_rch(id) {
            require_lazy(command.contains("rch exec --"), || {
                format!("quality gate {id} must prove RCH execution")
            })?;
        }
        observed.insert(id);
    }
    require(
        observed == *required_quality_gates,
        "quality gate ids must exactly match required quality gates",
    )
}

fn verify_docs_links() -> TestResult {
    let readme = load_text(README_PATH)?;
    let runbook = load_text(RUNBOOK_PATH)?;
    for required in [CONTRACT_PATH, EVIDENCE_PATH] {
        require_lazy(readme.contains(required), || {
            format!("README must link {required}")
        })?;
        require_lazy(runbook.contains(required), || {
            format!("runbook must link {required}")
        })?;
    }
    require(
        runbook.contains(EXPECTED_EVIDENCE_SCHEMA),
        "runbook must document proof-carrying test-fabric closeout schema",
    )
}

fn verify_checklist_quality_gates_and_docs(contract: &Value, evidence: &Value) -> TestResult {
    let required_checks = checklist_ids(contract)?;
    let required_quality_gates = quality_gate_ids(contract)?;
    verify_closeout_outcome(evidence, &required_checks)?;
    verify_known_limitations(evidence)?;
    verify_checklist_rows(evidence, &required_checks)?;
    verify_negative_control_coverage(evidence)?;
    verify_quality_gate_rows(evidence, &required_quality_gates)?;
    verify_docs_links()
}

fn verify_source_boundaries_claims_and_push(contract: &Value, evidence: &Value) -> TestResult {
    let required_boundary_ids = string_set(contract, "/required_source_boundary_ids")?;
    let source_boundaries = pointer_array(evidence, "/source_boundary_checks")?;
    require(
        source_boundaries.len() == required_boundary_ids.len(),
        "source_boundary_checks must exactly cover required source boundaries",
    )?;

    let mut observed = HashSet::new();
    for row in source_boundaries {
        let id = pointer_str(row, "/id")?;
        require_lazy(required_boundary_ids.contains(id), || {
            format!("unexpected source boundary check {id}")
        })?;
        require_lazy(pointer_str(row, "/status")? == "pass", || {
            format!("source boundary {id} must pass")
        })?;
        require_non_empty_array(row, "/evidence")?;
        observed.insert(id);
    }
    require(
        observed == required_boundary_ids,
        "source boundary ids must exactly match the contract",
    )?;

    for claim_path in [
        "/claim_boundaries/strict_dropin_or_release_claim_authorized",
        "/claim_boundaries/performance_or_capacity_claim_authorized",
        "/claim_boundaries/closeout_replaces_source_artifacts",
        "/claim_boundaries/closeout_mutates_agent_mail_rch_or_runtime",
        "/claim_boundaries/closeout_mutates_sources",
        "/claim_boundaries/closeout_runs_live_provider_or_network_calls",
        "/claim_boundaries/closeout_authorizes_file_deletion",
    ] {
        require(
            !pointer_bool(evidence, claim_path)?,
            format!("{claim_path} must be false"),
        )?;
    }

    let pushed = checklist_row(evidence, "pushed_commits")?;
    let snapshot = pointer_array(pushed, "/evidence")?
        .first()
        .ok_or_else(|| "pushed_commits evidence must not be empty".to_string())?;
    require_hex_commit(snapshot, "/head_before_closeout_commit")?;
    require_hex_commit(snapshot, "/origin_main_before_closeout_commit")?;
    require_hex_commit(snapshot, "/origin_legacy_mirror_before_closeout_commit")?;

    let head = pointer_str(snapshot, "/head_before_closeout_commit")?;
    require(
        pointer_str(snapshot, "/origin_main_before_closeout_commit")? == head,
        "origin/main snapshot must match HEAD before closeout",
    )?;
    require(
        pointer_str(snapshot, "/origin_legacy_mirror_before_closeout_commit")? == head,
        "legacy mirror snapshot must match HEAD before closeout",
    )?;
    require(
        pointer_bool(snapshot, "/pushed_remote_refs_equal_head")?,
        "push snapshot must explicitly report remote refs equal HEAD",
    )?;

    let child_commits = pointer_array(snapshot, "/child_commits")?;
    require(
        child_commits.len() == child_ids(contract)?.len(),
        "pushed snapshot must list one commit per implementation child",
    )?;
    for commit in child_commits {
        let commit = commit
            .as_str()
            .ok_or_else(|| "child commit entries must be strings".to_string())?;
        require(
            is_hex_commitish(commit),
            "child commits must be hex commit hashes",
        )?;
    }
    Ok(())
}

fn expect_error(result: TestResult, expected_fragment: &str) -> TestResult {
    match result {
        Ok(()) => Err(format!(
            "expected verifier error containing {expected_fragment:?}"
        )),
        Err(message) => require(
            message.contains(expected_fragment),
            format!("expected error containing {expected_fragment:?}, got {message:?}"),
        ),
    }
}

#[test]
fn proof_carrying_test_fabric_closeout_identity_and_required_keys_pass() -> TestResult {
    let contract = load_json(CONTRACT_PATH)?;
    let evidence = load_json(EVIDENCE_PATH)?;
    verify_identity_and_required_keys(&contract, &evidence)
}

#[test]
fn proof_carrying_test_fabric_closeout_child_artifact_map_is_complete() -> TestResult {
    let contract = load_json(CONTRACT_PATH)?;
    let evidence = load_json(EVIDENCE_PATH)?;
    verify_child_artifact_map(&contract, &evidence)
}

#[test]
fn proof_carrying_test_fabric_closeout_checklist_quality_gates_and_docs_are_complete() -> TestResult
{
    let contract = load_json(CONTRACT_PATH)?;
    let evidence = load_json(EVIDENCE_PATH)?;
    verify_checklist_quality_gates_and_docs(&contract, &evidence)
}

#[test]
fn proof_carrying_test_fabric_closeout_source_boundaries_claims_and_push_pass() -> TestResult {
    let contract = load_json(CONTRACT_PATH)?;
    let evidence = load_json(EVIDENCE_PATH)?;
    verify_source_boundaries_claims_and_push(&contract, &evidence)
}

#[test]
fn proof_carrying_test_fabric_closeout_rejects_missing_open_or_weak_child_evidence() -> TestResult {
    let contract = load_json(CONTRACT_PATH)?;
    let evidence = load_json(EVIDENCE_PATH)?;

    let mut missing_child = evidence.clone();
    pointer_array_mut(&mut missing_child, "/child_artifact_map")?.pop();
    expect_error(
        verify_child_artifact_map(&contract, &missing_child),
        "exactly one row per required child",
    )?;

    let mut open_child = evidence.clone();
    *pointer_mut(&mut open_child, "/child_artifact_map/0/status")? =
        Value::String("open".to_string());
    expect_error(
        verify_child_artifact_map(&contract, &open_child),
        "must be closed",
    )?;

    let mut weak_child = evidence.clone();
    pointer_array_mut(&mut weak_child, "/child_artifact_map/0/validation_commands")?.clear();
    expect_error(
        verify_child_artifact_map(&contract, &weak_child),
        "validation_commands",
    )?;

    let mut missing_claim_boundary = evidence;
    *pointer_mut(
        &mut missing_claim_boundary,
        "/child_artifact_map/0/claim_boundary_text",
    )? = Value::String(String::new());
    expect_error(
        verify_child_artifact_map(&contract, &missing_claim_boundary),
        "claim_boundary_text",
    )
}

#[test]
fn proof_carrying_test_fabric_closeout_rejects_unpushed_or_unmirrored_snapshot() -> TestResult {
    let contract = load_json(CONTRACT_PATH)?;
    let evidence = load_json(EVIDENCE_PATH)?;

    let mut unpushed = evidence.clone();
    *pointer_mut(
        checklist_row_mut(&mut unpushed, "pushed_commits")?,
        "/evidence/0/origin_main_before_closeout_commit",
    )? = Value::String("0000000000000000000000000000000000000000".to_string());
    expect_error(
        verify_source_boundaries_claims_and_push(&contract, &unpushed),
        "origin/main snapshot must match HEAD",
    )?;

    let mut unmirrored = evidence;
    *pointer_mut(
        checklist_row_mut(&mut unmirrored, "pushed_commits")?,
        "/evidence/0/pushed_remote_refs_equal_head",
    )? = Value::Bool(false);
    expect_error(
        verify_source_boundaries_claims_and_push(&contract, &unmirrored),
        "remote refs equal HEAD",
    )
}

#[test]
fn proof_carrying_test_fabric_closeout_rejects_missing_quality_gates_or_claim_drift() -> TestResult
{
    let contract = load_json(CONTRACT_PATH)?;
    let evidence = load_json(EVIDENCE_PATH)?;

    let mut failed_ubs = evidence.clone();
    *pointer_mut(
        quality_gate_row_mut(&mut failed_ubs, "staged_ubs")?,
        "/status",
    )? = Value::String("fail".to_string());
    expect_error(
        verify_checklist_quality_gates_and_docs(&contract, &failed_ubs),
        "staged_ubs",
    )?;

    let mut empty_self_test_command = evidence.clone();
    *pointer_mut(
        quality_gate_row_mut(&mut empty_self_test_command, "runpack_self_test")?,
        "/command",
    )? = Value::String(String::new());
    expect_error(
        verify_checklist_quality_gates_and_docs(&contract, &empty_self_test_command),
        "command must be non-empty",
    )?;

    let mut claim_drift = evidence;
    *pointer_mut(
        &mut claim_drift,
        "/claim_boundaries/closeout_authorizes_file_deletion",
    )? = Value::Bool(true);
    expect_error(
        verify_source_boundaries_claims_and_push(&contract, &claim_drift),
        "closeout_authorizes_file_deletion",
    )
}
