#!/usr/bin/env python3
"""Check completion-audit JSON as a lightweight closeout gate.

The gate is read-only. It consumes a completion-audit artifact that was already
created by scripts/build_completion_audit.py and emits stable remediation JSON.
It does not run cargo, mutate Beads, send Agent Mail, launch RCH, or touch live
provider state.
"""

from __future__ import annotations

import argparse
import difflib
import json
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


GATE_SCHEMA = "ra.completion_audit.closeout_gate.v1"
FIXTURE_SCHEMA = "ra.completion_audit.closeout_gate_fixtures.v1"
SELF_TEST_SCHEMA = "ra.completion_audit.closeout_gate_self_test.v1"
FIXTURE_PATH = Path("tests/fixtures/completion_audit_gate/scenarios.json")
GOLDEN_DIR = Path("tests/fixtures/completion_audit_gate/goldens")
GOLDEN_GENERATED_AT = "[GENERATED_AT]"
BLOCKING_STATUSES = {"missing", "failed", "proxy_only", "contradiction", "uncertain"}


class GateError(Exception):
    """Raised when gate inputs or fixture expectations are invalid."""


def utc_now_iso() -> str:
    return datetime.now(timezone.utc).replace(microsecond=0).isoformat()


def json_dumps(value: Any) -> str:
    return json.dumps(value, indent=2, sort_keys=True) + "\n"


def load_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError as exc:
        raise GateError(f"missing JSON file: {path}") from exc
    except json.JSONDecodeError as exc:
        raise GateError(f"malformed JSON file {path}: {exc}") from exc


def as_object(value: Any, *, field: str) -> dict[str, Any]:
    if value is None:
        return {}
    if not isinstance(value, dict):
        raise GateError(f"{field} must be an object")
    return value


def as_list(value: Any, *, field: str) -> list[Any]:
    if value is None:
        return []
    if not isinstance(value, list):
        raise GateError(f"{field} must be an array")
    return value


def bool_or_false(value: Any) -> bool:
    return value is True


def normalize_requirement(item: dict[str, Any]) -> dict[str, Any]:
    refs = item.get("evidence_refs", item.get("refs", []))
    return {
        "id": str(item.get("id") or "unknown"),
        "kind": str(item.get("kind") or "unknown"),
        "status": str(item.get("evidence_status") or item.get("status") or "unknown"),
        "issue": str(item.get("issue") or ""),
        "refs": [str(ref) for ref in as_list(refs, field="requirement.refs")],
        "text": str(item.get("text") or ""),
    }


def classify_blocker(requirement: dict[str, Any]) -> str:
    status = requirement["status"]
    kind = requirement["kind"]
    issue = requirement["issue"].lower()
    refs = " ".join(requirement["refs"]).lower()
    if status == "proxy_only":
        return "proxy_only_evidence"
    if kind in {"push", "commit_push"} and ("push" in issue or "push" in refs):
        return "missing_push"
    if kind == "command" and status == "failed":
        return "failed_command"
    if kind == "artifact_bundle" or "artifact" in issue:
        return "missing_artifact"
    if status == "missing":
        return "missing_evidence"
    if status == "contradiction":
        return "contradictory_evidence"
    return "blocked_requirement"


def remediation_for(kind: str) -> str:
    return {
        "missing_push": "Push the closeout commit, then regenerate completion-audit evidence.",
        "failed_command": "Fix or rerun the failed command and include its passing transcript.",
        "proxy_only_evidence": "Replace proxy-only evidence with direct command, artifact, git, or Beads evidence.",
        "missing_artifact": "Create or correct the missing artifact path, then regenerate the audit.",
        "missing_evidence": "Attach direct evidence for the requirement or file an owner bead for the gap.",
        "contradictory_evidence": "Resolve contradictory green status before admitting closeout.",
        "unresolved_gap": "Close the gap or create an active owner bead before closeout.",
        "source_missing": "Provide a completion-audit JSON artifact before using this gate.",
    }.get(kind, "Resolve the blocked requirement before admitting closeout.")


def command_statuses(evidence: dict[str, Any]) -> list[dict[str, Any]]:
    values = evidence.get("commands", evidence.get("command_statuses", []))
    return [item for item in as_list(values, field="evidence.commands") if isinstance(item, dict)]


def artifact_statuses(evidence: dict[str, Any]) -> list[dict[str, Any]]:
    values = evidence.get("artifacts", evidence.get("artifact_statuses", []))
    return [item for item in as_list(values, field="evidence.artifacts") if isinstance(item, dict)]


def build_gate(audit: dict[str, Any], *, generated_at: str) -> dict[str, Any]:
    if not isinstance(audit, dict):
        raise GateError("audit must be a JSON object")
    source_schema = str(audit.get("schema") or "")
    source_present = bool(audit)
    requirements = [
        normalize_requirement(item)
        for item in as_list(audit.get("requirements"), field="requirements")
        if isinstance(item, dict)
    ]
    evidence = as_object(audit.get("evidence"), field="evidence")
    unresolved_gaps = [
        item for item in as_list(evidence.get("unresolved_gaps"), field="evidence.unresolved_gaps")
        if isinstance(item, (dict, str))
    ]
    blockers: list[dict[str, Any]] = []
    for requirement in requirements:
        if requirement["status"] in BLOCKING_STATUSES:
            blocker_kind = classify_blocker(requirement)
            blockers.append(
                {
                    "id": requirement["id"],
                    "kind": blocker_kind,
                    "requirement_kind": requirement["kind"],
                    "status": requirement["status"],
                    "issue": requirement["issue"],
                    "refs": requirement["refs"],
                    "remediation": remediation_for(blocker_kind),
                }
            )
    for index, gap in enumerate(unresolved_gaps):
        gap_id = gap.get("gap_id") if isinstance(gap, dict) else str(gap)
        blockers.append(
            {
                "id": str(gap_id or f"gap[{index}]"),
                "kind": "unresolved_gap",
                "requirement_kind": "gap",
                "status": str(gap.get("status") if isinstance(gap, dict) else "open"),
                "issue": str(gap.get("issue") if isinstance(gap, dict) else gap),
                "refs": [],
                "remediation": remediation_for("unresolved_gap"),
            }
        )
    if not source_present:
        blockers.append(
            {
                "id": "completion_audit_source",
                "kind": "source_missing",
                "requirement_kind": "source",
                "status": "missing",
                "issue": "completion audit source is missing",
                "refs": [],
                "remediation": remediation_for("source_missing"),
            }
        )
    completion_allowed = bool_or_false(audit.get("completion_allowed"))
    status = "pass" if completion_allowed and not blockers else "block"
    return {
        "schema": GATE_SCHEMA,
        "generated_at": generated_at,
        "status": status,
        "decision": "closeout_admitted" if status == "pass" else "closeout_blocked",
        "source": {
            "schema": source_schema,
            "overall_status": str(audit.get("overall_status") or audit.get("status") or "unknown"),
            "completion_allowed": completion_allowed,
        },
        "summary": {
            "requirement_count": len(requirements),
            "blocker_count": len(blockers),
            "unresolved_gap_count": len(unresolved_gaps),
            "command_count": len(command_statuses(evidence)),
            "artifact_count": len(artifact_statuses(evidence)),
        },
        "blockers": blockers,
        "operator_next_actions": list(dict.fromkeys(blocker["remediation"] for blocker in blockers)),
        "source_boundaries": {
            "read_only": True,
            "runs_cargo": False,
            "runs_live_provider": False,
            "mutates_beads": False,
            "mutates_agent_mail": False,
            "mutates_rch": False,
            "deletes_files": False,
        },
    }


def load_fixture(fixture_id: str, *, repo_root: Path) -> dict[str, Any]:
    fixture = load_json(repo_root / FIXTURE_PATH)
    if fixture.get("schema") != FIXTURE_SCHEMA:
        raise GateError(f"invalid fixture schema in {FIXTURE_PATH}")
    for scenario in fixture.get("scenarios", []):
        if isinstance(scenario, dict) and scenario.get("id") == fixture_id:
            audit = scenario.get("audit")
            if not isinstance(audit, dict):
                raise GateError(f"fixture {fixture_id} has invalid audit")
            return audit
    known = ", ".join(
        sorted(str(item.get("id")) for item in fixture.get("scenarios", []) if isinstance(item, dict))
    )
    raise GateError(f"unknown fixture id {fixture_id!r}; known fixtures: {known}")


def assert_expected(gate: dict[str, Any], expected: dict[str, Any], *, fixture_id: str) -> None:
    if gate["status"] != expected.get("status"):
        raise GateError(f"{fixture_id}: expected status {expected.get('status')}, got {gate['status']}")
    expected_kinds = sorted(str(item) for item in expected.get("blocker_kinds", []))
    actual_kinds = sorted(str(item.get("kind")) for item in gate["blockers"])
    if expected_kinds != actual_kinds:
        raise GateError(f"{fixture_id}: expected blocker kinds {expected_kinds}, got {actual_kinds}")
    for text in expected.get("contains", []):
        if text not in json_dumps(gate):
            raise GateError(f"{fixture_id}: gate output missing {text!r}")


def canonicalize(gate: dict[str, Any]) -> dict[str, Any]:
    canonical = json.loads(json_dumps(gate))
    canonical["generated_at"] = GOLDEN_GENERATED_AT
    return canonical


def golden_path(repo_root: Path, fixture_id: str) -> Path:
    return repo_root / GOLDEN_DIR / f"{fixture_id}.json"


def diff_text(expected: str, actual: str, *, path: Path) -> str:
    lines = list(
        difflib.unified_diff(
            expected.splitlines(),
            actual.splitlines(),
            fromfile=f"{path} (expected)",
            tofile=f"{path} (actual)",
            lineterm="",
        )
    )
    if len(lines) > 80:
        lines = lines[:80] + ["... diff truncated ..."]
    return "\n".join(lines)


def assert_golden(
    *,
    repo_root: Path,
    fixture_id: str,
    gate: dict[str, Any],
    update_goldens: bool,
) -> None:
    path = golden_path(repo_root, fixture_id)
    actual = json_dumps(canonicalize(gate))
    if update_goldens:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(actual, encoding="utf-8")
        return
    if not path.exists():
        raise GateError(f"{fixture_id}: missing golden {path}; rerun --self-test --update-goldens")
    expected = path.read_text(encoding="utf-8")
    if expected != actual:
        raise GateError(f"{fixture_id}: golden mismatch\n{diff_text(expected, actual, path=path)}")


def self_test(*, repo_root: Path, generated_at: str, update_goldens: bool) -> dict[str, Any]:
    fixture = load_json(repo_root / FIXTURE_PATH)
    if fixture.get("schema") != FIXTURE_SCHEMA:
        raise GateError(f"invalid fixture schema in {FIXTURE_PATH}")
    results = []
    for scenario in fixture.get("scenarios", []):
        if not isinstance(scenario, dict):
            raise GateError("fixture scenario must be an object")
        fixture_id = str(scenario.get("id"))
        audit = as_object(scenario.get("audit"), field=f"{fixture_id}.audit")
        gate = build_gate(audit, generated_at=generated_at)
        assert_expected(gate, as_object(scenario.get("expected"), field=f"{fixture_id}.expected"), fixture_id=fixture_id)
        assert_golden(
            repo_root=repo_root,
            fixture_id=fixture_id,
            gate=gate,
            update_goldens=update_goldens,
        )
        results.append(
            {
                "id": fixture_id,
                "status": gate["status"],
                "blocker_count": gate["summary"]["blocker_count"],
                "golden_checked": not update_goldens,
                "golden_updated": update_goldens,
            }
        )
    return {
        "schema": SELF_TEST_SCHEMA,
        "generated_at": generated_at,
        "status": "pass",
        "golden_mode": "update" if update_goldens else "check",
        "scenario_count": len(results),
        "scenarios": results,
    }


def write_output(path: Path, payload: dict[str, Any]) -> None:
    if path.exists():
        raise GateError(f"refusing to overwrite existing output: {path}")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json_dumps(payload), encoding="utf-8")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Check completion-audit JSON as a closeout gate.")
    parser.add_argument("--audit-json", type=Path, help="Completion-audit JSON artifact")
    parser.add_argument("--fixture-id", help="Run a named self-test fixture")
    parser.add_argument("--out-json", type=Path, help="Write gate JSON output")
    parser.add_argument("--generated-at", help="Override generated_at timestamp")
    parser.add_argument("--self-test", action="store_true", help="Run fixture-backed self-test")
    parser.add_argument("--update-goldens", action="store_true", help="Refresh checked-in self-test goldens")
    parser.add_argument("--repo-root", type=Path, default=Path.cwd())
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    repo_root = args.repo_root.resolve()
    generated_at = args.generated_at or utc_now_iso()
    try:
        if args.self_test:
            payload = self_test(
                repo_root=repo_root,
                generated_at=generated_at,
                update_goldens=args.update_goldens,
            )
            if args.out_json:
                write_output(args.out_json, payload)
            else:
                sys.stdout.write(json_dumps(payload))
            return 0
        if args.update_goldens:
            raise GateError("--update-goldens is only valid with --self-test")
        if args.fixture_id:
            audit = load_fixture(args.fixture_id, repo_root=repo_root)
        elif args.audit_json:
            audit = as_object(load_json(args.audit_json), field="audit_json")
        else:
            raise GateError("provide --audit-json, --fixture-id, or --self-test")
        gate = build_gate(audit, generated_at=generated_at)
        if args.out_json:
            write_output(args.out_json, gate)
        else:
            sys.stdout.write(json_dumps(gate))
        return 0 if gate["status"] == "pass" else 1
    except GateError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
