#!/usr/bin/env python3
"""Report whether the Beads queue has actually converged.

This is a read-only operator report. It packages the manual proof agents run at
session boundaries: Beads queue state, optional br/bv robot output, degraded
Agent Mail/RCH signals, and closeout-gate freshness. It never mutates Beads,
git, Agent Mail, RCH, runpack inputs, or source files.
"""

from __future__ import annotations

import argparse
import json
import os
import shlex
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any


REPORT_SCHEMA = "ra.swarm.empty_queue_convergence_report.v1"
POLICY = "read_only_no_mutation"
IW_DRIFT_T1_PREFIX = "IW-DRIFT-T1:"
DEFAULT_STALE_IN_PROGRESS_HOURS = 2
VALIDATION_BROKER_CLI_STATUS_SCHEMA = "ra.validation_broker.cli_status.v1"
VALIDATION_BROKER_CLI_PLAN_SCHEMA = "ra.validation_broker.cli_plan.v1"
VALIDATION_BROKER_DOCTOR_SCHEMA = "ra.doctor.validation_broker_posture.v1"
VALIDATION_BROKER_SAMPLE_LIMIT = 5
NON_BLOCKING_DEP_TYPES = {"parent-child", "related"}
DEFERRED_PLANNING_LABELS = {
    "idea-wizard",
    "planning",
    "roadmap",
}
DEFERRED_PLANNING_TERMS = (
    "backlog",
    "idea-wizard",
    "roadmap",
    "work_to_plan",
)
AGENT_MAIL_SCHEMA_CORRUPT_FIXTURE = Path(
    "tests/fixtures/agent_mail/schema_corrupt_health.json"
)
CLOSEOUT_FRESHNESS_CHECKER = Path("scripts/check_closeout_gate_freshness.py")


@dataclass(frozen=True)
class LoadedJson:
    path: str | None
    payload: Any | None
    error: str | None


def utc_now() -> datetime:
    return datetime.now(timezone.utc)


def utc_now_iso() -> str:
    return utc_now().replace(microsecond=0).isoformat().replace("+00:00", "Z")


def json_dumps(payload: Any, *, pretty: bool = False) -> str:
    if pretty:
        return json.dumps(payload, indent=2, sort_keys=True) + "\n"
    return json.dumps(payload, sort_keys=True, separators=(",", ":")) + "\n"


def parse_iso_datetime(raw: object) -> datetime | None:
    if not isinstance(raw, str):
        return None
    value = raw.strip()
    if not value:
        return None
    if value.endswith("Z"):
        value = f"{value[:-1]}+00:00"
    try:
        parsed = datetime.fromisoformat(value)
    except ValueError:
        return None
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=timezone.utc)
    return parsed.astimezone(timezone.utc)


def read_json(path: Path | None) -> LoadedJson:
    if path is None:
        return LoadedJson(path=None, payload=None, error=None)
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as exc:
        return LoadedJson(path=str(path), payload=None, error=str(exc))
    try:
        return LoadedJson(path=str(path), payload=json.loads(text), error=None)
    except json.JSONDecodeError as exc:
        return LoadedJson(path=str(path), payload=None, error=f"invalid JSON: {exc}")


def read_fixture_json(repo_root: Path, relative_path: Path) -> LoadedJson:
    return read_json(repo_root / relative_path)


def run_default_closeout_freshness(repo_root: Path) -> LoadedJson:
    checker_path = repo_root / CLOSEOUT_FRESHNESS_CHECKER
    source = f"generated:{CLOSEOUT_FRESHNESS_CHECKER.as_posix()}"
    if not checker_path.exists():
        return LoadedJson(path=source, payload=None, error=f"{checker_path}: file does not exist")
    completed = subprocess.run(
        [
            sys.executable,
            str(checker_path),
            "--repo-root",
            str(repo_root),
            "--compact",
        ],
        cwd=repo_root,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    stdout = completed.stdout.strip()
    if not stdout:
        message = f"closeout freshness checker emitted no JSON (exit {completed.returncode})"
        if completed.stderr.strip():
            message = f"{message}: {completed.stderr.strip()}"
        return LoadedJson(path=source, payload=None, error=message)
    try:
        payload = json.loads(stdout)
    except json.JSONDecodeError as exc:
        message = f"closeout freshness checker emitted malformed JSON: {exc}"
        if completed.stderr.strip():
            message = f"{message}: {completed.stderr.strip()}"
        return LoadedJson(path=source, payload=None, error=message)
    if completed.returncode not in {0, 1}:
        message = f"closeout freshness checker exited {completed.returncode}"
        if completed.stderr.strip():
            message = f"{message}: {completed.stderr.strip()}"
        return LoadedJson(path=source, payload=payload, error=message)
    return LoadedJson(path=source, payload=payload, error=None)


def read_issues(path: Path) -> tuple[list[dict[str, Any]], str | None]:
    issues: list[dict[str, Any]] = []
    if not path.exists():
        return issues, f"{path}: file does not exist"
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except OSError as exc:
        return issues, str(exc)
    for line_number, line in enumerate(lines, 1):
        stripped = line.strip()
        if not stripped:
            continue
        try:
            record = json.loads(stripped)
        except json.JSONDecodeError as exc:
            return issues, f"{path}:{line_number}: invalid JSONL record: {exc}"
        if isinstance(record, dict):
            issues.append(record)
    return issues, None


def normalize_issue_records(payload: Any) -> list[dict[str, Any]]:
    if isinstance(payload, list):
        return [item for item in payload if isinstance(item, dict)]
    if isinstance(payload, dict):
        for key in ("issues", "items", "ready", "data"):
            value = payload.get(key)
            if isinstance(value, list):
                return [item for item in value if isinstance(item, dict)]
    return []


def issue_summary(issue: dict[str, Any], *, now: datetime | None = None) -> dict[str, Any]:
    updated_at = parse_iso_datetime(issue.get("updated_at"))
    age_hours = None
    if now is not None and updated_at is not None:
        age_hours = round((now - updated_at).total_seconds() / 3600, 2)
    summary = {
        "id": issue.get("id"),
        "title": issue.get("title"),
        "status": issue.get("status"),
        "priority": issue.get("priority"),
        "issue_type": issue.get("issue_type"),
        "assignee": issue.get("assignee"),
        "updated_at": issue.get("updated_at"),
        "labels": issue.get("labels", []),
    }
    if age_hours is not None:
        summary["age_hours"] = age_hours
    return summary


def dependency_blocks(issue: dict[str, Any], issue_by_id: dict[str, dict[str, Any]]) -> bool:
    for dep in issue.get("dependencies") or []:
        if not isinstance(dep, dict):
            continue
        dep_type = str(dep.get("type", "")).strip()
        if dep_type in NON_BLOCKING_DEP_TYPES:
            continue
        depends_on = dep.get("depends_on_id") or dep.get("id")
        blocker = issue_by_id.get(str(depends_on))
        blocker_status = str(blocker.get("status", "")) if blocker else "missing"
        if blocker_status not in {"closed", "tombstone"}:
            return True
    return False


def compute_ready_issues(issues: list[dict[str, Any]]) -> list[dict[str, Any]]:
    issue_by_id = {str(issue.get("id")): issue for issue in issues if issue.get("id")}
    ready: list[dict[str, Any]] = []
    for issue in issues:
        if issue.get("status") != "open":
            continue
        if dependency_blocks(issue, issue_by_id):
            continue
        ready.append(issue)
    return sorted(ready, key=lambda item: (int(item.get("priority", 99)), str(item.get("id", ""))))


def child_issues_by_parent(issues: list[dict[str, Any]]) -> dict[str, list[dict[str, Any]]]:
    children: dict[str, list[dict[str, Any]]] = {}
    for issue in issues:
        for dep in issue.get("dependencies") or []:
            if not isinstance(dep, dict):
                continue
            if dep.get("type") != "parent-child":
                continue
            parent_id = dep.get("depends_on_id") or dep.get("id")
            if parent_id:
                children.setdefault(str(parent_id), []).append(issue)
    return children


def is_deferred_planning_epic(issue: dict[str, Any]) -> bool:
    if issue.get("status") != "deferred":
        return False
    if issue.get("issue_type") != "epic":
        return False
    labels = {str(label).lower() for label in issue.get("labels") or []}
    if labels & DEFERRED_PLANNING_LABELS:
        return True
    haystack = " ".join(
        str(issue.get(field, "")).lower()
        for field in ("title", "description", "notes")
    )
    return any(term in haystack for term in DEFERRED_PLANNING_TERMS)


def deferred_planning_items(
    issues: list[dict[str, Any]],
    *,
    now: datetime,
) -> list[dict[str, Any]]:
    children_by_parent = child_issues_by_parent(issues)
    items: list[dict[str, Any]] = []
    for issue in issues:
        if not is_deferred_planning_epic(issue):
            continue
        children = children_by_parent.get(str(issue.get("id")), [])
        child_statuses: dict[str, int] = {}
        for child in children:
            status = str(child.get("status", "unknown"))
            child_statuses[status] = child_statuses.get(status, 0) + 1
        active_child_count = child_statuses.get("open", 0) + child_statuses.get("in_progress", 0)
        if active_child_count:
            continue
        items.append(
            {
                **issue_summary(issue, now=now),
                "child_count": len(children),
                "child_statuses": dict(sorted(child_statuses.items())),
                "planning_reason": "deferred roadmap/planning epic has no open or in-progress child work",
            }
        )
    return sorted(items, key=lambda item: (int(item.get("priority", 99)), str(item.get("id", ""))))


def candidate_child_title(item: dict[str, Any]) -> str:
    labels = {str(label).lower() for label in item.get("labels") or []}
    title = str(item.get("title") or "roadmap").strip()
    if "operations" in labels:
        return "Add next swarm operations diagnostic from roadmap"
    if "performance" in labels or "reliability" in labels:
        return "Extract next swarm responsiveness regression from roadmap"
    if "closeout" in labels:
        return "Refresh closeout proof-memory follow-up from roadmap"
    if "evidence" in labels:
        return "Add next artifact truth maintenance guard from roadmap"
    return f"Refine {title} into next executable child bead"


def child_priority(item: dict[str, Any]) -> int:
    try:
        priority = int(item.get("priority", 2))
    except (TypeError, ValueError):
        priority = 2
    return min(3, max(2, priority + 1))


def shell_command(args: list[str]) -> str:
    return " ".join(shlex.quote(arg) for arg in args)


def deferred_planning_create_templates(items: list[dict[str, Any]]) -> list[dict[str, Any]]:
    templates: list[dict[str, Any]] = []
    for item in items[:10]:
        parent_id = str(item.get("id") or "").strip()
        if not parent_id:
            continue
        labels = sorted(
            {
                str(label).lower()
                for label in item.get("labels") or []
                if str(label).strip()
            }
        )
        child_labels = labels or ["planning"]
        title = candidate_child_title(item)
        priority = child_priority(item)
        description = (
            f"Created from empty-queue convergence planning prompt for {parent_id}. "
            "Convert the deferred roadmap into one narrow, executable child task with "
            "explicit validation evidence and no broad planning-only scope."
        )
        command = shell_command(
            [
                "br",
                "create",
                "--title",
                title,
                "--type",
                "task",
                "--priority",
                str(priority),
                "--parent",
                parent_id,
                "--labels",
                ",".join(child_labels),
                "--description",
                description,
            ]
        )
        templates.append(
            {
                "parent_id": parent_id,
                "parent_title": item.get("title"),
                "title": title,
                "priority": priority,
                "labels": child_labels,
                "command": command,
            }
        )
    return templates


def analyze_beads(
    issues: list[dict[str, Any]],
    *,
    br_ready: list[dict[str, Any]] | None,
    now: datetime,
    stale_hours: float,
) -> dict[str, Any]:
    by_status: dict[str, int] = {}
    for issue in issues:
        status = str(issue.get("status", "unknown"))
        by_status[status] = by_status.get(status, 0) + 1

    ready_source = "br_ready_json" if br_ready is not None else "computed_from_beads_jsonl"
    ready_issues = br_ready if br_ready is not None else compute_ready_issues(issues)
    in_progress = [issue for issue in issues if issue.get("status") == "in_progress"]
    deferred = [issue for issue in issues if issue.get("status") == "deferred"]
    tombstones = [issue for issue in issues if issue.get("status") == "tombstone"]
    planning_items = deferred_planning_items(issues, now=now)

    stale_in_progress = []
    for issue in in_progress:
        updated_at = parse_iso_datetime(issue.get("updated_at"))
        if updated_at is None:
            stale_in_progress.append({**issue_summary(issue, now=now), "stale_reason": "missing_updated_at"})
            continue
        age_hours = (now - updated_at).total_seconds() / 3600
        if age_hours >= stale_hours:
            stale_in_progress.append({**issue_summary(issue, now=now), "stale_reason": "age_exceeds_threshold"})

    return {
        "source": ready_source,
        "total_count": len(issues),
        "by_status": dict(sorted(by_status.items())),
        "ready_count": len(ready_issues),
        "open_count": by_status.get("open", 0),
        "in_progress_count": len(in_progress),
        "deferred_count": len(deferred),
        "deferred_planning_count": len(planning_items),
        "tombstone_count": len(tombstones),
        "stale_in_progress_count": len(stale_in_progress),
        "ready_items": [issue_summary(issue, now=now) for issue in ready_issues[:25]],
        "in_progress_items": [issue_summary(issue, now=now) for issue in in_progress[:25]],
        "stale_in_progress_items": stale_in_progress[:25],
        "deferred_only_items": [issue_summary(issue, now=now) for issue in deferred[:25]],
        "deferred_planning_items": planning_items[:25],
        "deferred_planning_create_templates": deferred_planning_create_templates(planning_items),
    }


def iter_bv_plan_items(payload: Any) -> list[dict[str, Any]]:
    if not isinstance(payload, dict):
        return []
    tracks = ((payload.get("plan") or {}).get("tracks")) if isinstance(payload.get("plan"), dict) else []
    if not isinstance(tracks, list):
        return []
    items: list[dict[str, Any]] = []
    for track in tracks:
        if not isinstance(track, dict):
            continue
        for item in track.get("items") or []:
            if isinstance(item, dict):
                items.append(item)
    return items


def analyze_bv(payload: Any, issue_by_id: dict[str, dict[str, Any]]) -> dict[str, Any]:
    if payload is None:
        return {
            "status": "unavailable",
            "source": None,
            "actionable_count": None,
            "tombstone_mismatch_count": 0,
            "tombstone_mismatches": [],
            "warnings": ["no bv robot JSON supplied"],
        }
    items = iter_bv_plan_items(payload)
    mismatches: list[dict[str, Any]] = []
    for item in items:
        issue_id = str(item.get("id", ""))
        issue = issue_by_id.get(issue_id)
        item_status = str(item.get("status", ""))
        live_status = str(issue.get("status", "missing")) if issue else "missing"
        if item_status == "tombstone" or live_status == "tombstone":
            mismatches.append(
                {
                    "id": issue_id,
                    "title": item.get("title") or (issue or {}).get("title"),
                    "bv_status": item_status,
                    "beads_status": live_status,
                }
            )
    return {
        "status": "ok" if not mismatches else "mismatch",
        "source": "bv_plan_json",
        "actionable_count": len(items),
        "tombstone_mismatch_count": len(mismatches),
        "tombstone_mismatches": mismatches,
        "warnings": [],
    }


def analyze_agent_mail(payload: Any, source_path: str | None, error: str | None) -> dict[str, Any]:
    if error is not None:
        return {
            "status": "unavailable",
            "source": source_path,
            "degraded": True,
            "health_level": None,
            "semantic_readiness": None,
            "recovery_mode": None,
            "recovery_action": None,
            "warnings": [error],
            "recommended_operator_action": "Use Beads status/comments as the coordination fallback.",
        }
    if payload is None:
        return {
            "status": "unavailable",
            "source": None,
            "degraded": False,
            "health_level": None,
            "semantic_readiness": None,
            "recovery_mode": None,
            "recovery_action": None,
            "warnings": ["no Agent Mail health JSON supplied"],
            "recommended_operator_action": "Optional: attach Agent Mail health JSON for coordination posture.",
        }
    status = str(payload.get("status", "unknown")) if isinstance(payload, dict) else "invalid"
    health_level = payload.get("health_level") if isinstance(payload, dict) else None
    semantic = payload.get("semantic_readiness") if isinstance(payload, dict) else None
    recovery = payload.get("recovery") if isinstance(payload, dict) else None
    semantic_status = semantic.get("status") if isinstance(semantic, dict) else None
    semantic_detail = semantic.get("detail") if isinstance(semantic, dict) else None
    recovery_mode = recovery.get("mode") if isinstance(recovery, dict) else None
    recovery_action = recovery.get("next_action") if isinstance(recovery, dict) else None
    degraded = status not in {"ok", "healthy"} or health_level in {"red", "degraded", "yellow"} or semantic_status == "fail"
    return {
        "status": "degraded" if degraded else "ok",
        "source": source_path,
        "degraded": degraded,
        "health_level": health_level,
        "semantic_readiness": semantic_status,
        "semantic_readiness_detail": semantic_detail,
        "recovery_mode": recovery_mode,
        "recovery_action": recovery_action,
        "warnings": [] if not degraded else ["Agent Mail is degraded; this does not block Beads-only progress."],
        "recommended_operator_action": (
            "Use Beads status/comments as the soft lock until Agent Mail is repaired."
            if degraded
            else "Agent Mail appears usable."
        ),
    }


def analyze_optional_status(payload: Any, source_path: str | None, error: str | None, name: str) -> dict[str, Any]:
    if error is not None:
        return {"status": "unavailable", "source": source_path, "warnings": [error]}
    if payload is None:
        return {
            "status": "unavailable",
            "source": None,
            "warnings": [f"no {name} JSON supplied"],
        }
    if isinstance(payload, dict):
        raw_status = payload.get("status") or payload.get("health_level") or payload.get("decision")
        return {
            "status": str(raw_status or "present"),
            "source": source_path,
            "schema": payload.get("schema"),
            "warnings": [],
        }
    return {"status": "invalid", "source": source_path, "warnings": [f"{name} JSON root is not an object"]}


def bounded_list(value: Any, max_items: int = VALIDATION_BROKER_SAMPLE_LIMIT) -> list[Any]:
    if not isinstance(value, list):
        return []
    return value[:max_items]


def nonnegative_int(value: Any) -> int | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, int) and value >= 0:
        return value
    if isinstance(value, float) and value.is_integer() and value >= 0:
        return int(value)
    return None


def count_or_zero(value: Any) -> int:
    return nonnegative_int(value) or 0


def validation_broker_slot_projection(value: Any) -> dict[str, Any]:
    if isinstance(value, dict) and isinstance(value.get("lease"), dict):
        value = value["lease"]
    if not isinstance(value, dict):
        return {"kind": type(value).__name__}

    safe_fields = (
        "slot_id",
        "state",
        "owner_agent",
        "bead_id",
        "command_class",
        "command_fingerprint",
        "environment_fingerprint",
        "runner",
        "git_head",
        "artifact_schema",
        "artifact_hash",
        "state_reason",
        "expires_at_utc",
        "heartbeat_at_utc",
    )
    projection = {field: value[field] for field in safe_fields if value.get(field) is not None}
    if isinstance(value.get("command"), list):
        projection["command_redacted"] = True
        projection["command_arg_count"] = len(value["command"])
    if any(value.get(field) for field in ("cwd", "target_dir", "tmpdir")):
        projection["paths_redacted"] = True
    return projection


def validation_broker_group_projection(value: Any) -> dict[str, Any]:
    if not isinstance(value, dict):
        return {"kind": type(value).__name__}
    safe_fields = (
        "command_class",
        "command_fingerprint",
        "environment_fingerprint",
        "runner",
        "count",
        "slot_count",
    )
    projection = {field: value[field] for field in safe_fields if value.get(field) is not None}
    for list_field in ("slots", "active_slots", "members", "sample"):
        if isinstance(value.get(list_field), list):
            projection[list_field] = [
                validation_broker_slot_projection(item)
                for item in bounded_list(value[list_field])
            ]
    if not projection:
        projection["fields_present"] = sorted(str(key) for key in value)
    return projection


def validation_broker_status_from_parts(
    *,
    source_status: str | None,
    stale_count: int,
    saturated: bool,
    malformed: bool,
    duplicate_count: int,
) -> str:
    degraded_statuses = {"degraded", "error", "fail", "failed", "invalid", "unavailable"}
    if malformed or (source_status or "").lower() in degraded_statuses or stale_count or saturated:
        return "degraded"
    if duplicate_count:
        return "advisory"
    return "ok"


def analyze_validation_broker(payload: Any, source_path: str | None, error: str | None) -> dict[str, Any]:
    base = {
        "source": source_path,
        "schema": None,
        "status": "unavailable",
        "source_status": None,
        "fail_closed": False,
        "warnings": [],
        "current_slots": {
            "total": None,
            "active": None,
            "reusable": None,
            "stale": None,
            "expired_at_report_time": None,
            "sample": [],
        },
        "duplicate_gate_opportunities": {
            "count": 0,
            "samples": [],
            "active_equivalent_groups": [],
            "bounded_to": VALIDATION_BROKER_SAMPLE_LIMIT,
            "redaction": "raw commands and local paths are omitted",
        },
        "stale_slots": {
            "count": 0,
            "expired_at_report_time": 0,
            "samples": [],
        },
        "saturated_queue": {
            "saturated": False,
            "active": None,
            "total": None,
            "reason": None,
        },
        "guards": {
            "advisory_only": True,
            "read_only": True,
            "live_mutations": 0,
            "destructive_actions": 0,
            "provider_calls": 0,
        },
        "recommended_operator_action": "Validation broker JSON is optional for this proof.",
    }
    if error is not None:
        return {
            **base,
            "status": "degraded",
            "fail_closed": True,
            "warnings": [error],
            "recommended_operator_action": "Inspect or regenerate validation-broker JSON before declaring the queue clean.",
        }
    if payload is None:
        return {
            **base,
            "warnings": ["no validation-broker JSON supplied"],
        }
    if not isinstance(payload, dict):
        return {
            **base,
            "status": "degraded",
            "fail_closed": True,
            "warnings": ["validation-broker JSON root is not an object"],
            "recommended_operator_action": "Inspect or regenerate validation-broker JSON before declaring the queue clean.",
        }

    schema = str(payload.get("schema", ""))
    store = payload.get("store") if isinstance(payload.get("store"), dict) else {}
    guards = payload.get("guards") if isinstance(payload.get("guards"), dict) else {}
    warnings: list[str] = []
    source_status = str(store.get("status", "unknown"))
    total = count_or_zero(store.get("total_slots"))
    active = count_or_zero(store.get("active_slots"))
    reusable = count_or_zero(store.get("reusable_slots"))
    stale = count_or_zero(store.get("stale_slots"))
    expired = count_or_zero(store.get("expired_at_report_time_slots"))
    duplicate_samples: list[dict[str, Any]] = []
    duplicate_groups: list[dict[str, Any]] = []
    stale_samples: list[dict[str, Any]] = []
    current_samples: list[dict[str, Any]] = []

    if schema == VALIDATION_BROKER_DOCTOR_SCHEMA:
        source_map = payload.get("source_status") if isinstance(payload.get("source_status"), dict) else {}
        current = payload.get("current_slots") if isinstance(payload.get("current_slots"), dict) else {}
        duplicate = (
            payload.get("duplicate_gate_opportunities")
            if isinstance(payload.get("duplicate_gate_opportunities"), dict)
            else {}
        )
        stale_warnings = (
            payload.get("stale_build_warnings")
            if isinstance(payload.get("stale_build_warnings"), dict)
            else {}
        )
        source_status = str(source_map.get("validation_slot_store") or store.get("status", "unknown"))
        total = count_or_zero(current.get("total") if current.get("total") is not None else store.get("total_slots"))
        active = count_or_zero(current.get("active"))
        reusable = count_or_zero(current.get("reusable"))
        stale = count_or_zero(current.get("stale"))
        expired = count_or_zero(current.get("expired_at_report_time"))
        duplicate_count = count_or_zero(duplicate.get("count"))
        stale_count = count_or_zero(stale_warnings.get("count") if stale_warnings.get("count") is not None else stale)
        duplicate_samples = [
            validation_broker_slot_projection(item)
            for item in bounded_list(duplicate.get("reusable_slots"))
        ]
        duplicate_groups = [
            validation_broker_group_projection(item)
            for item in bounded_list(duplicate.get("active_equivalent_groups"))
        ]
        stale_samples = [
            validation_broker_slot_projection(item)
            for item in bounded_list(stale_warnings.get("expired_slots"))
        ]
        current_samples = [
            validation_broker_slot_projection(item)
            for item in bounded_list(current.get("sample"))
        ]
    elif schema in {VALIDATION_BROKER_CLI_STATUS_SCHEMA, VALIDATION_BROKER_CLI_PLAN_SCHEMA}:
        decision = payload.get("decision") if isinstance(payload.get("decision"), dict) else {}
        rejected_reusable = bounded_list(decision.get("rejected_reusable_slots"))
        duplicate_count = max(reusable, len(rejected_reusable))
        stale_count = max(stale, expired)
        duplicate_samples = [
            validation_broker_slot_projection(item)
            for item in rejected_reusable
        ]
    else:
        return {
            **base,
            "schema": schema or None,
            "status": "degraded",
            "fail_closed": True,
            "warnings": [
                "validation-broker source schema mismatch: expected one of "
                f"{sorted([VALIDATION_BROKER_CLI_PLAN_SCHEMA, VALIDATION_BROKER_CLI_STATUS_SCHEMA, VALIDATION_BROKER_DOCTOR_SCHEMA])}, got {schema or 'missing'}"
            ],
            "recommended_operator_action": "Inspect or regenerate validation-broker JSON before declaring the queue clean.",
        }

    saturated = bool(total and active >= total and reusable == 0)
    if stale_count:
        warnings.append("validation broker reports stale or expired validation slots")
    if duplicate_count:
        warnings.append("validation broker reports duplicate expensive cargo gate opportunities")
    if saturated:
        warnings.append("validation broker queue appears saturated")
    status = validation_broker_status_from_parts(
        source_status=source_status,
        stale_count=stale_count,
        saturated=saturated,
        malformed=False,
        duplicate_count=duplicate_count,
    )
    if not warnings and status == "ok":
        recommended = "Validation broker posture is healthy."
    elif saturated:
        recommended = "Defer or coalesce broad cargo gates until validation-broker slots clear."
    elif stale_count:
        recommended = "Recover stale validation slots through the owner-visible broker workflow."
    elif duplicate_count:
        recommended = "Prefer reusable validation evidence or coalesce equivalent cargo gates."
    else:
        recommended = "Inspect validation-broker posture before declaring the queue clean."

    merged_guards = {
        **base["guards"],
        "read_only": bool(guards.get("read_only_plan", True)),
        "live_mutations": count_or_zero(guards.get("live_mutations")),
        "destructive_actions": count_or_zero(guards.get("destructive_actions")),
        "provider_calls": count_or_zero(guards.get("provider_calls")),
    }
    if (
        not merged_guards["read_only"]
        or merged_guards["live_mutations"]
        or merged_guards["destructive_actions"]
        or merged_guards["provider_calls"]
    ):
        warnings.append("validation broker guard metadata reports non-read-only activity")
        status = "degraded"
        recommended = "Inspect validation-broker posture before declaring the queue clean."
    return {
        **base,
        "schema": schema,
        "status": status,
        "source_status": source_status,
        "warnings": warnings,
        "current_slots": {
            "total": total,
            "active": active,
            "reusable": reusable,
            "stale": stale,
            "expired_at_report_time": expired,
            "sample": current_samples,
        },
        "duplicate_gate_opportunities": {
            "count": duplicate_count,
            "samples": duplicate_samples,
            "active_equivalent_groups": duplicate_groups,
            "bounded_to": VALIDATION_BROKER_SAMPLE_LIMIT,
            "redaction": "raw commands and local paths are omitted",
        },
        "stale_slots": {
            "count": stale_count,
            "expired_at_report_time": expired,
            "samples": stale_samples,
        },
        "saturated_queue": {
            "saturated": saturated,
            "active": active,
            "total": total,
            "reason": "active_slots_at_or_above_total_without_reusable_slots" if saturated else None,
        },
        "guards": merged_guards,
        "recommended_operator_action": recommended,
    }


def find_iw_drift_t1(issues: list[dict[str, Any]]) -> dict[str, Any] | None:
    for issue in issues:
        title = str(issue.get("title", ""))
        if title.startswith(IW_DRIFT_T1_PREFIX):
            return issue
    return None


def analyze_closeout(
    payload: Any,
    source_path: str | None,
    error: str | None,
    iw_drift_t1: dict[str, Any] | None,
) -> dict[str, Any]:
    base = {
        "source": source_path,
        "linked_freshness_bead": issue_summary(iw_drift_t1) if iw_drift_t1 else None,
    }
    if error is not None:
        return {
            **base,
            "status": "unavailable",
            "schema": None,
            "warnings": [error],
            "recommended_operator_action": "Regenerate or provide closeout freshness JSON before declaring an empty queue clean.",
        }
    if payload is None:
        action = (
            f"Work or close {iw_drift_t1.get('id')} before using closeout freshness in empty-queue proof."
            if iw_drift_t1 and iw_drift_t1.get("status") != "closed"
            else "Provide closeout freshness JSON when available."
        )
        return {
            **base,
            "status": "unavailable",
            "schema": None,
            "warnings": ["no closeout freshness JSON supplied"],
            "recommended_operator_action": action,
        }
    status = str(payload.get("status", "unknown")) if isinstance(payload, dict) else "invalid"
    return {
        **base,
        "status": status,
        "schema": payload.get("schema") if isinstance(payload, dict) else None,
        "warnings": [] if status in {"pass", "ready", "ok"} else ["closeout freshness is not passing"],
        "recommended_operator_action": (
            "Closeout freshness supports empty-queue proof."
            if status in {"pass", "ready", "ok"}
            else "Refresh closeout-gate evidence before declaring the queue clean."
        ),
    }


def build_next_actions(
    beads: dict[str, Any],
    bv: dict[str, Any],
    agent_mail: dict[str, Any],
    validation_broker: dict[str, Any],
    closeout: dict[str, Any],
) -> list[dict[str, Any]]:
    actions: list[dict[str, Any]] = []
    if beads.get("source_error_count"):
        actions.append(
            {
                "action": "inspect_queue_source_inputs",
                "source_errors": beads.get("source_errors", []),
                "reason": (
                    "required queue source input was malformed or unreadable; repair "
                    "the source before declaring empty-queue convergence."
                ),
            }
        )
    if beads["ready_count"]:
        first = beads["ready_items"][0]
        actions.append(
            {
                "action": "start_ready_bead",
                "issue_id": first.get("id"),
                "reason": "ready Beads work exists; claim the highest-value non-overlapping item.",
            }
        )
    if beads["stale_in_progress_count"]:
        actions.append(
            {
                "action": "review_stale_in_progress",
                "issue_ids": [item.get("id") for item in beads["stale_in_progress_items"]],
                "reason": "in-progress work exceeded the stale threshold.",
            }
        )
    if bv["tombstone_mismatch_count"]:
        actions.append(
            {
                "action": "ignore_or_repair_bv_tombstone_mismatch",
                "issue_ids": [item.get("id") for item in bv["tombstone_mismatches"]],
                "reason": "bv surfaced tombstoned work; tombstones are not actionable.",
            }
        )
    if agent_mail["degraded"]:
        actions.append(
            {
                "action": "use_beads_fallback",
                "reason": agent_mail["recommended_operator_action"],
            }
        )
    if validation_broker["fail_closed"]:
        actions.append(
            {
                "action": "inspect_validation_broker_source",
                "reason": validation_broker["recommended_operator_action"],
            }
        )
    if validation_broker["stale_slots"]["count"]:
        actions.append(
            {
                "action": "recover_validation_broker_stale_slots",
                "reason": validation_broker["recommended_operator_action"],
                "stale_slot_count": validation_broker["stale_slots"]["count"],
            }
        )
    if validation_broker["duplicate_gate_opportunities"]["count"]:
        actions.append(
            {
                "action": "coalesce_duplicate_validation_gates",
                "reason": validation_broker["recommended_operator_action"],
                "duplicate_gate_count": validation_broker["duplicate_gate_opportunities"]["count"],
                "samples": validation_broker["duplicate_gate_opportunities"]["samples"],
            }
        )
    if validation_broker["saturated_queue"]["saturated"]:
        actions.append(
            {
                "action": "throttle_validation_broker_queue",
                "reason": validation_broker["recommended_operator_action"],
            }
        )
    if not beads["ready_count"] and not beads["in_progress_count"]:
        if beads["deferred_planning_count"]:
            actions.append(
                {
                    "action": "create_or_refine_backlog",
                    "issue_ids": [item.get("id") for item in beads["deferred_planning_items"]],
                    "epics": [
                        {"id": item.get("id"), "title": item.get("title")}
                        for item in beads["deferred_planning_items"]
                    ],
                    "create_templates": beads["deferred_planning_create_templates"],
                    "reason": (
                        "deferred roadmap/planning epics remain without open child work; create "
                        "or refine actionable child Beads before declaring the queue clean."
                    ),
                }
            )
        elif closeout["status"] in {"pass", "ready", "ok"}:
            actions.append(
                {
                    "action": "stop_queue_clean",
                    "reason": "no ready or in-progress work remains and closeout freshness is passing.",
                }
            )
        else:
            actions.append(
                {
                    "action": "create_or_finish_audit_bead",
                    "reason": closeout["recommended_operator_action"],
                }
            )
    if not actions:
        actions.append(
            {
                "action": "continue_monitoring",
                "reason": "no immediate corrective action was selected.",
            }
        )
    return actions


def overall_status(
    beads: dict[str, Any],
    bv: dict[str, Any],
    validation_broker: dict[str, Any],
    closeout: dict[str, Any],
) -> str:
    if beads.get("source_error_count"):
        return "needs_attention"
    if beads["stale_in_progress_count"] or bv["tombstone_mismatch_count"]:
        return "needs_attention"
    if beads["ready_count"]:
        return "ready_work_available"
    if beads["in_progress_count"]:
        return "work_in_progress"
    if (
        validation_broker["fail_closed"]
        or validation_broker["status"] == "degraded"
        or validation_broker["stale_slots"]["count"]
        or validation_broker["saturated_queue"]["saturated"]
    ):
        return "needs_attention"
    if beads["deferred_planning_count"]:
        return "work_to_plan"
    if closeout["status"] in {"pass", "ready", "ok"}:
        return "queue_clean"
    return "needs_attention"


def build_report(
    *,
    repo_root: Path,
    beads_path: Path,
    br_ready_json: LoadedJson,
    bv_plan_json: LoadedJson,
    agent_mail_health_json: LoadedJson,
    validation_broker_json: LoadedJson,
    rch_json: LoadedJson,
    closeout_freshness_json: LoadedJson,
    now: datetime,
    stale_hours: float,
) -> dict[str, Any]:
    issues, issues_error = read_issues(beads_path)
    br_ready = None
    if br_ready_json.error is None and br_ready_json.payload is not None:
        br_ready = normalize_issue_records(br_ready_json.payload)

    beads = analyze_beads(issues, br_ready=br_ready, now=now, stale_hours=stale_hours)
    source_errors: list[dict[str, str | None]] = []
    if issues_error is not None:
        beads["load_error"] = issues_error
        source_errors.append(
            {
                "source": "beads_jsonl",
                "path": str(beads_path),
                "error": issues_error,
            }
        )
    if br_ready_json.error is not None:
        source_errors.append(
            {
                "source": "br_ready_json",
                "path": br_ready_json.path,
                "error": br_ready_json.error,
            }
        )
    beads["source_error_count"] = len(source_errors)
    beads["source_errors"] = source_errors
    issue_by_id = {str(issue.get("id")): issue for issue in issues if issue.get("id")}

    bv = analyze_bv(bv_plan_json.payload, issue_by_id)
    if bv_plan_json.error is not None:
        bv["status"] = "unavailable"
        bv["warnings"] = [bv_plan_json.error]
    bv["source_path"] = bv_plan_json.path

    agent_mail = analyze_agent_mail(
        agent_mail_health_json.payload,
        agent_mail_health_json.path,
        agent_mail_health_json.error,
    )
    validation_broker = analyze_validation_broker(
        validation_broker_json.payload,
        validation_broker_json.path,
        validation_broker_json.error,
    )
    rch = analyze_optional_status(rch_json.payload, rch_json.path, rch_json.error, "RCH")
    closeout = analyze_closeout(
        closeout_freshness_json.payload,
        closeout_freshness_json.path,
        closeout_freshness_json.error,
        find_iw_drift_t1(issues),
    )

    actions = build_next_actions(beads, bv, agent_mail, validation_broker, closeout)
    status = overall_status(beads, bv, validation_broker, closeout)
    return {
        "schema": REPORT_SCHEMA,
        "generated_at": now.replace(microsecond=0).isoformat().replace("+00:00", "Z"),
        "status": status,
        "policy": POLICY,
        "source_root": str(repo_root),
        "source_files": {
            "beads": str(beads_path),
            "br_ready_json": br_ready_json.path,
            "bv_plan_json": bv_plan_json.path,
            "agent_mail_health_json": agent_mail_health_json.path,
            "validation_broker_json": validation_broker_json.path,
            "rch_json": rch_json.path,
            "closeout_freshness_json": closeout_freshness_json.path,
        },
        "thresholds": {
            "stale_in_progress_hours": stale_hours,
        },
        "summary": {
            "ready_count": beads["ready_count"],
            "in_progress_count": beads["in_progress_count"],
            "deferred_count": beads["deferred_count"],
            "deferred_planning_count": beads["deferred_planning_count"],
            "tombstone_count": beads["tombstone_count"],
            "beads_source_error_count": beads["source_error_count"],
            "bv_actionable_count": bv["actionable_count"],
            "bv_tombstone_mismatch_count": bv["tombstone_mismatch_count"],
            "agent_mail_status": agent_mail["status"],
            "validation_broker_status": validation_broker["status"],
            "validation_broker_stale_slot_count": validation_broker["stale_slots"]["count"],
            "validation_broker_duplicate_gate_count": validation_broker["duplicate_gate_opportunities"]["count"],
            "validation_broker_saturated": validation_broker["saturated_queue"]["saturated"],
            "rch_status": rch["status"],
            "closeout_freshness_status": closeout["status"],
        },
        "beads": beads,
        "bv": bv,
        "coordination": {
            "agent_mail": agent_mail,
            "validation_broker": validation_broker,
            "rch": rch,
        },
        "closeout_freshness": closeout,
        "next_actions": actions,
    }


def print_text_report(report: dict[str, Any]) -> None:
    summary = report["summary"]
    print(
        "status={status} ready={ready} in_progress={in_progress} deferred={deferred} "
        "deferred_planning={deferred_planning} bv_tombstone_mismatches={mismatches} "
        "agent_mail={agent_mail} validation_broker={validation_broker} closeout={closeout}".format(
            status=report["status"],
            ready=summary["ready_count"],
            in_progress=summary["in_progress_count"],
            deferred=summary["deferred_count"],
            deferred_planning=summary["deferred_planning_count"],
            mismatches=summary["bv_tombstone_mismatch_count"],
            agent_mail=summary["agent_mail_status"],
            validation_broker=summary["validation_broker_status"],
            closeout=summary["closeout_freshness_status"],
        )
    )
    for action in report["next_actions"]:
        issue = f" issue={action['issue_id']}" if action.get("issue_id") else ""
        issues = ""
        if action.get("issue_ids"):
            issues = " issues=" + ",".join(str(item) for item in action["issue_ids"])
        print(f"- next_action={action['action']}{issue}{issues}: {action['reason']}")
        for template in action.get("create_templates", []):
            print(f"  create_template={template['command']}")


def write_json(path: Path, payload: Any) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json_dumps(payload, pretty=True), encoding="utf-8")
    return path


def write_issues(path: Path, issues: list[dict[str, Any]]) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("".join(json_dumps(issue) for issue in issues), encoding="utf-8")
    return path


def assert_condition(condition: bool, message: str, report: dict[str, Any] | None = None) -> None:
    if condition:
        return
    if report is not None:
        sys.stderr.write(json_dumps(report, pretty=True))
    raise AssertionError(message)


def fixture_issue(
    issue_id: str,
    title: str,
    *,
    status: str = "open",
    priority: int = 2,
    updated_at: str = "2026-05-15T11:30:00Z",
    labels: list[str] | None = None,
) -> dict[str, Any]:
    return {
        "id": issue_id,
        "title": title,
        "description": "fixture",
        "status": status,
        "priority": priority,
        "issue_type": "task",
        "updated_at": updated_at,
        "labels": labels or [],
    }


def run_self_test() -> int:
    now = datetime(2026, 5, 15, 12, 0, 0, tzinfo=timezone.utc)
    with tempfile.TemporaryDirectory(prefix="pi_empty_queue_convergence_") as temp_dir:
        root = Path(temp_dir)
        beads_path = root / ".beads/issues.jsonl"
        ready_issue = fixture_issue("bd-ready", "Ready work", priority=1)
        drift_t1 = fixture_issue("bd-fresh", "IW-DRIFT-T1: Add closeout-gate freshness auditor")
        write_issues(beads_path, [ready_issue, drift_t1])
        ready_json = write_json(root / "ready.json", [ready_issue])
        bv_json = write_json(
            root / "bv.json",
            {"plan": {"tracks": [{"items": [{"id": "bd-ready", "status": "open", "title": "Ready work"}]}]}},
        )
        report = build_report(
            repo_root=root,
            beads_path=beads_path,
            br_ready_json=read_json(ready_json),
            bv_plan_json=read_json(bv_json),
            agent_mail_health_json=LoadedJson(None, None, None),
            validation_broker_json=LoadedJson(None, None, None),
            rch_json=LoadedJson(None, None, None),
            closeout_freshness_json=LoadedJson(None, None, None),
            now=now,
            stale_hours=DEFAULT_STALE_IN_PROGRESS_HOURS,
        )
        assert_condition(report["status"] == "ready_work_available", "ready work should win", report)
        assert_condition(report["summary"]["ready_count"] == 1, "ready count should come from br ready", report)

        clean_beads = root / "clean/.beads/issues.jsonl"
        deferred = fixture_issue("bd-epic", "Deferred epic", status="deferred")
        write_issues(clean_beads, [deferred])
        closeout_json = write_json(root / "closeout.json", {"schema": "fixture.closeout.v1", "status": "pass"})
        clean_report = build_report(
            repo_root=root,
            beads_path=clean_beads,
            br_ready_json=LoadedJson(None, [], None),
            bv_plan_json=LoadedJson(None, {"plan": {"tracks": []}}, None),
            agent_mail_health_json=LoadedJson(None, None, None),
            validation_broker_json=LoadedJson(None, None, None),
            rch_json=LoadedJson(None, None, None),
            closeout_freshness_json=read_json(closeout_json),
            now=now,
            stale_hours=DEFAULT_STALE_IN_PROGRESS_HOURS,
        )
        assert_condition(clean_report["status"] == "queue_clean", "empty queue should be clean", clean_report)
        assert_condition(
            clean_report["next_actions"][0]["action"] == "stop_queue_clean",
            "clean queue should tell operator to stop",
            clean_report,
        )

        missing_closeout_report = build_report(
            repo_root=root,
            beads_path=clean_beads,
            br_ready_json=LoadedJson(None, [], None),
            bv_plan_json=LoadedJson(None, {"plan": {"tracks": []}}, None),
            agent_mail_health_json=LoadedJson(None, None, None),
            validation_broker_json=LoadedJson(None, None, None),
            rch_json=LoadedJson(None, None, None),
            closeout_freshness_json=LoadedJson(None, None, None),
            now=now,
            stale_hours=DEFAULT_STALE_IN_PROGRESS_HOURS,
        )
        assert_condition(
            missing_closeout_report["status"] == "needs_attention",
            "missing closeout freshness should block queue_clean",
            missing_closeout_report,
        )
        assert_condition(
            missing_closeout_report["next_actions"][0]["action"] == "create_or_finish_audit_bead",
            "missing closeout freshness should produce an audit/freshness action",
            missing_closeout_report,
        )

        failing_closeout_json = write_json(
            root / "closeout-failing.json",
            {"schema": "fixture.closeout.v1", "status": "fail"},
        )
        failing_closeout_report = build_report(
            repo_root=root,
            beads_path=clean_beads,
            br_ready_json=LoadedJson(None, [], None),
            bv_plan_json=LoadedJson(None, {"plan": {"tracks": []}}, None),
            agent_mail_health_json=LoadedJson(None, None, None),
            validation_broker_json=LoadedJson(None, None, None),
            rch_json=LoadedJson(None, None, None),
            closeout_freshness_json=read_json(failing_closeout_json),
            now=now,
            stale_hours=DEFAULT_STALE_IN_PROGRESS_HOURS,
        )
        assert_condition(
            failing_closeout_report["status"] == "needs_attention",
            "failing closeout freshness should block queue_clean",
            failing_closeout_report,
        )
        assert_condition(
            failing_closeout_report["closeout_freshness"]["warnings"] == [
                "closeout freshness is not passing"
            ],
            "failing closeout freshness should preserve warning text",
            failing_closeout_report,
        )

        malformed_ready_json = root / "malformed-ready.json"
        malformed_ready_json.write_text("{", encoding="utf-8")
        malformed_ready_report = build_report(
            repo_root=root,
            beads_path=clean_beads,
            br_ready_json=read_json(malformed_ready_json),
            bv_plan_json=LoadedJson(None, {"plan": {"tracks": []}}, None),
            agent_mail_health_json=LoadedJson(None, None, None),
            validation_broker_json=LoadedJson(None, None, None),
            rch_json=LoadedJson(None, None, None),
            closeout_freshness_json=read_json(closeout_json),
            now=now,
            stale_hours=DEFAULT_STALE_IN_PROGRESS_HOURS,
        )
        assert_condition(
            malformed_ready_report["status"] == "needs_attention",
            "malformed br ready JSON should fail closed",
            malformed_ready_report,
        )
        assert_condition(
            malformed_ready_report["summary"]["beads_source_error_count"] == 1,
            "malformed br ready JSON should be counted as a queue source error",
            malformed_ready_report,
        )
        assert_condition(
            malformed_ready_report["beads"]["source_errors"][0]["source"] == "br_ready_json",
            "malformed br ready JSON should preserve source identity",
            malformed_ready_report,
        )
        assert_condition(
            malformed_ready_report["next_actions"][0]["action"] == "inspect_queue_source_inputs",
            "malformed br ready JSON should produce source-inspection action",
            malformed_ready_report,
        )

        malformed_beads = root / "malformed/.beads/issues.jsonl"
        malformed_beads.parent.mkdir(parents=True, exist_ok=True)
        malformed_beads.write_text(
            json_dumps(fixture_issue("bd-done", "Closed work", status="closed")) + "{",
            encoding="utf-8",
        )
        malformed_beads_report = build_report(
            repo_root=root,
            beads_path=malformed_beads,
            br_ready_json=LoadedJson(None, [], None),
            bv_plan_json=LoadedJson(None, {"plan": {"tracks": []}}, None),
            agent_mail_health_json=LoadedJson(None, None, None),
            validation_broker_json=LoadedJson(None, None, None),
            rch_json=LoadedJson(None, None, None),
            closeout_freshness_json=read_json(closeout_json),
            now=now,
            stale_hours=DEFAULT_STALE_IN_PROGRESS_HOURS,
        )
        assert_condition(
            malformed_beads_report["status"] == "needs_attention",
            "malformed Beads JSONL should fail closed",
            malformed_beads_report,
        )
        assert_condition(
            malformed_beads_report["beads"]["source_errors"][0]["source"] == "beads_jsonl",
            "malformed Beads JSONL should preserve source identity",
            malformed_beads_report,
        )

        planning_beads = root / "planning/.beads/issues.jsonl"
        planning_epic = fixture_issue(
            "bd-plan",
            "Swarm responsiveness follow-up roadmap",
            status="deferred",
            labels=["idea-wizard", "performance", "reliability", "swarm"],
        )
        planning_epic["issue_type"] = "epic"
        closed_child = fixture_issue(
            "bd-plan.1",
            "Closed child",
            status="closed",
        )
        closed_child["dependencies"] = [
            {"type": "parent-child", "depends_on_id": "bd-plan"},
        ]
        write_issues(planning_beads, [planning_epic, closed_child])
        planning_report = build_report(
            repo_root=root,
            beads_path=planning_beads,
            br_ready_json=LoadedJson(None, [], None),
            bv_plan_json=LoadedJson(None, {"plan": {"tracks": []}}, None),
            agent_mail_health_json=LoadedJson(None, None, None),
            validation_broker_json=LoadedJson(None, None, None),
            rch_json=LoadedJson(None, None, None),
            closeout_freshness_json=read_json(closeout_json),
            now=now,
            stale_hours=DEFAULT_STALE_IN_PROGRESS_HOURS,
        )
        assert_condition(
            planning_report["status"] == "work_to_plan",
            "deferred planning epic should block queue_clean",
            planning_report,
        )
        assert_condition(
            planning_report["summary"]["deferred_planning_count"] == 1,
            "planning epic should be counted explicitly",
            planning_report,
        )
        assert_condition(
            planning_report["next_actions"][0]["action"] == "create_or_refine_backlog",
            "planning epic should ask for backlog refinement",
            planning_report,
        )
        assert_condition(
            planning_report["next_actions"][0]["issue_ids"] == ["bd-plan"],
            "planning next action should name the deferred epic",
            planning_report,
        )
        create_templates = planning_report["next_actions"][0]["create_templates"]
        assert_condition(
            len(create_templates) == 1,
            "planning next action should include one concrete create template",
            planning_report,
        )
        assert_condition(
            create_templates[0]["parent_id"] == "bd-plan",
            "create template should preserve the parent epic",
            planning_report,
        )
        assert_condition(
            create_templates[0]["title"] == "Extract next swarm responsiveness regression from roadmap",
            "swarm responsiveness roadmap should produce the concrete responsiveness regression template",
            planning_report,
        )
        assert_condition(
            "explicit validation evidence" in create_templates[0]["command"],
            "create template should require explicit validation evidence",
            planning_report,
        )
        assert_condition(
            "--parent bd-plan" in create_templates[0]["command"],
            "create template should include an exact parent argument",
            planning_report,
        )
        assert_condition(
            "br create --title" in create_templates[0]["command"],
            "create template should include an executable br create command",
            planning_report,
        )

        active_planning_beads = root / "active-planning/.beads/issues.jsonl"
        active_child = fixture_issue(
            "bd-plan.2",
            "Active swarm responsiveness child",
            status="in_progress",
            updated_at="2026-05-15T11:45:00Z",
        )
        active_child["dependencies"] = [
            {"type": "parent-child", "depends_on_id": "bd-plan"},
        ]
        write_issues(active_planning_beads, [planning_epic, closed_child, active_child])
        active_planning_report = build_report(
            repo_root=root,
            beads_path=active_planning_beads,
            br_ready_json=LoadedJson(None, [], None),
            bv_plan_json=LoadedJson(None, {"plan": {"tracks": []}}, None),
            agent_mail_health_json=LoadedJson(None, None, None),
            validation_broker_json=LoadedJson(None, None, None),
            rch_json=LoadedJson(None, None, None),
            closeout_freshness_json=read_json(closeout_json),
            now=now,
            stale_hours=DEFAULT_STALE_IN_PROGRESS_HOURS,
        )
        assert_condition(
            active_planning_report["status"] == "work_in_progress",
            "active roadmap child should be treated as work in progress",
            active_planning_report,
        )
        assert_condition(
            active_planning_report["summary"]["deferred_planning_count"] == 0,
            "active roadmap child should suppress duplicate planning templates",
            active_planning_report,
        )
        assert_condition(
            active_planning_report["next_actions"][0]["action"] == "continue_monitoring",
            "active roadmap child should not ask operators to create another child",
            active_planning_report,
        )

        stale_beads = root / "stale/.beads/issues.jsonl"
        stale = fixture_issue(
            "bd-stale",
            "Stale work",
            status="in_progress",
            updated_at="2026-05-15T08:30:00Z",
        )
        write_issues(stale_beads, [stale])
        stale_report = build_report(
            repo_root=root,
            beads_path=stale_beads,
            br_ready_json=LoadedJson(None, [], None),
            bv_plan_json=LoadedJson(None, {"plan": {"tracks": []}}, None),
            agent_mail_health_json=LoadedJson(None, None, None),
            validation_broker_json=LoadedJson(None, None, None),
            rch_json=LoadedJson(None, None, None),
            closeout_freshness_json=LoadedJson(None, None, None),
            now=now,
            stale_hours=DEFAULT_STALE_IN_PROGRESS_HOURS,
        )
        assert_condition(stale_report["status"] == "needs_attention", "stale work should need attention", stale_report)
        assert_condition(
            stale_report["summary"]["in_progress_count"] == 1,
            "stale report should count in-progress work",
            stale_report,
        )

        tombstone_beads = root / "tombstone/.beads/issues.jsonl"
        tombstone = fixture_issue("bd-old", "Deleted item", status="tombstone")
        write_issues(tombstone_beads, [tombstone])
        tombstone_report = build_report(
            repo_root=root,
            beads_path=tombstone_beads,
            br_ready_json=LoadedJson(None, [], None),
            bv_plan_json=LoadedJson(None, {"plan": {"tracks": [{"items": [{"id": "bd-old", "status": "tombstone"}]}]}}, None),
            agent_mail_health_json=LoadedJson(None, None, None),
            validation_broker_json=LoadedJson(None, None, None),
            rch_json=LoadedJson(None, None, None),
            closeout_freshness_json=LoadedJson(None, None, None),
            now=now,
            stale_hours=DEFAULT_STALE_IN_PROGRESS_HOURS,
        )
        assert_condition(
            tombstone_report["summary"]["bv_tombstone_mismatch_count"] == 1,
            "bv tombstone mismatch should be explicit",
            tombstone_report,
        )

        assert_condition(
            report["coordination"]["validation_broker"]["status"] == "unavailable",
            "missing validation broker JSON should be optional and explicit",
            report,
        )

        healthy_validation_broker = write_json(
            root / "validation-broker-healthy.json",
            {
                "schema": VALIDATION_BROKER_CLI_STATUS_SCHEMA,
                "store": {
                    "status": "available",
                    "total_slots": 2,
                    "active_slots": 1,
                    "reusable_slots": 0,
                    "stale_slots": 0,
                    "expired_at_report_time_slots": 0,
                    "degraded_reasons": [],
                },
                "guards": {
                    "read_only_plan": True,
                    "live_mutations": 0,
                    "destructive_actions": 0,
                    "provider_calls": 0,
                },
            },
        )
        healthy_validation_report = build_report(
            repo_root=root,
            beads_path=clean_beads,
            br_ready_json=LoadedJson(None, [], None),
            bv_plan_json=LoadedJson(None, {"plan": {"tracks": []}}, None),
            agent_mail_health_json=LoadedJson(None, None, None),
            validation_broker_json=read_json(healthy_validation_broker),
            rch_json=LoadedJson(None, None, None),
            closeout_freshness_json=read_json(closeout_json),
            now=now,
            stale_hours=DEFAULT_STALE_IN_PROGRESS_HOURS,
        )
        assert_condition(
            healthy_validation_report["coordination"]["validation_broker"]["status"] == "ok",
            "healthy validation broker should be advisory-ok",
            healthy_validation_report,
        )
        assert_condition(
            healthy_validation_report["status"] == "queue_clean",
            "healthy validation broker should not block clean empty queue proof",
            healthy_validation_report,
        )

        stale_validation_broker = write_json(
            root / "validation-broker-stale.json",
            {
                "schema": VALIDATION_BROKER_CLI_STATUS_SCHEMA,
                "store": {
                    "status": "available",
                    "total_slots": 2,
                    "active_slots": 0,
                    "reusable_slots": 0,
                    "stale_slots": 1,
                    "expired_at_report_time_slots": 1,
                    "degraded_reasons": [],
                },
                "guards": {"read_only_plan": True, "live_mutations": 0},
            },
        )
        stale_validation_report = build_report(
            repo_root=root,
            beads_path=clean_beads,
            br_ready_json=LoadedJson(None, [], None),
            bv_plan_json=LoadedJson(None, {"plan": {"tracks": []}}, None),
            agent_mail_health_json=LoadedJson(None, None, None),
            validation_broker_json=read_json(stale_validation_broker),
            rch_json=LoadedJson(None, None, None),
            closeout_freshness_json=read_json(closeout_json),
            now=now,
            stale_hours=DEFAULT_STALE_IN_PROGRESS_HOURS,
        )
        assert_condition(
            stale_validation_report["status"] == "needs_attention",
            "stale validation broker slots should fail closed before queue_clean",
            stale_validation_report,
        )
        assert_condition(
            any(action["action"] == "recover_validation_broker_stale_slots" for action in stale_validation_report["next_actions"]),
            "stale validation broker slots should produce a recovery action",
            stale_validation_report,
        )

        duplicate_slots = [
            {
                "slot_id": f"slot-dup-{index}",
                "state": "reusable",
                "owner_agent": "AmberOsprey",
                "bead_id": "bd-dup",
                "command": ["cargo", "check", "--all-targets", f"--secret-{index}"],
                "command_class": "cargo_check_all_targets",
                "command_fingerprint": f"cmdfp-{index}",
                "environment_fingerprint": "envfp",
                "cwd": "/data/projects/RecurAgent",
                "target_dir": "/data/tmp/recur_agent_cargo/secret/target",
                "tmpdir": "/data/tmp/recur_agent_cargo/secret/tmp",
                "runner": "rch",
            }
            for index in range(7)
        ]
        duplicate_validation_broker = write_json(
            root / "validation-broker-duplicate.json",
            {
                "schema": VALIDATION_BROKER_DOCTOR_SCHEMA,
                "source_status": {"validation_slot_store": "available"},
                "store": {"status": "available", "total_slots": 7},
                "current_slots": {
                    "total": 7,
                    "active": 1,
                    "reusable": 6,
                    "stale": 0,
                    "expired_at_report_time": 0,
                    "sample": duplicate_slots,
                },
                "duplicate_gate_opportunities": {
                    "count": 7,
                    "active_equivalent_groups": [],
                    "reusable_slots": duplicate_slots,
                },
                "stale_build_warnings": {"count": 0, "expired_slots": []},
                "guards": {"advisory_only": True, "no_live_mutation": True},
            },
        )
        duplicate_validation_report = build_report(
            repo_root=root,
            beads_path=clean_beads,
            br_ready_json=LoadedJson(None, [], None),
            bv_plan_json=LoadedJson(None, {"plan": {"tracks": []}}, None),
            agent_mail_health_json=LoadedJson(None, None, None),
            validation_broker_json=read_json(duplicate_validation_broker),
            rch_json=LoadedJson(None, None, None),
            closeout_freshness_json=read_json(closeout_json),
            now=now,
            stale_hours=DEFAULT_STALE_IN_PROGRESS_HOURS,
        )
        duplicate_summary = duplicate_validation_report["coordination"]["validation_broker"][
            "duplicate_gate_opportunities"
        ]
        duplicate_sample_json = json_dumps(duplicate_summary["samples"], pretty=True)
        assert_condition(
            duplicate_summary["count"] == 7,
            "duplicate validation gate count should be preserved",
            duplicate_validation_report,
        )
        assert_condition(
            len(duplicate_summary["samples"]) == VALIDATION_BROKER_SAMPLE_LIMIT,
            "duplicate validation gate samples should be bounded",
            duplicate_validation_report,
        )
        assert_condition(
            '"command":' not in duplicate_sample_json
            and "--secret" not in duplicate_sample_json
            and "/data/tmp" not in duplicate_sample_json,
            "duplicate validation gate samples should redact raw commands and paths",
            duplicate_validation_report,
        )
        assert_condition(
            any(action["action"] == "coalesce_duplicate_validation_gates" for action in duplicate_validation_report["next_actions"]),
            "duplicate validation gates should produce a coalescing action",
            duplicate_validation_report,
        )

        saturated_validation_broker = write_json(
            root / "validation-broker-saturated.json",
            {
                "schema": VALIDATION_BROKER_CLI_STATUS_SCHEMA,
                "store": {
                    "status": "available",
                    "total_slots": 4,
                    "active_slots": 4,
                    "reusable_slots": 0,
                    "stale_slots": 0,
                    "expired_at_report_time_slots": 0,
                    "degraded_reasons": [],
                },
                "guards": {"read_only_plan": True, "live_mutations": 0},
            },
        )
        saturated_validation_report = build_report(
            repo_root=root,
            beads_path=clean_beads,
            br_ready_json=LoadedJson(None, [], None),
            bv_plan_json=LoadedJson(None, {"plan": {"tracks": []}}, None),
            agent_mail_health_json=LoadedJson(None, None, None),
            validation_broker_json=read_json(saturated_validation_broker),
            rch_json=LoadedJson(None, None, None),
            closeout_freshness_json=read_json(closeout_json),
            now=now,
            stale_hours=DEFAULT_STALE_IN_PROGRESS_HOURS,
        )
        assert_condition(
            saturated_validation_report["coordination"]["validation_broker"]["saturated_queue"]["saturated"],
            "saturated validation broker queue should be explicit",
            saturated_validation_report,
        )
        assert_condition(
            any(action["action"] == "throttle_validation_broker_queue" for action in saturated_validation_report["next_actions"]),
            "saturated validation broker queue should produce a throttle action",
            saturated_validation_report,
        )

        malformed_validation_broker = root / "validation-broker-malformed.json"
        malformed_validation_broker.write_text("{", encoding="utf-8")
        malformed_validation_report = build_report(
            repo_root=root,
            beads_path=clean_beads,
            br_ready_json=LoadedJson(None, [], None),
            bv_plan_json=LoadedJson(None, {"plan": {"tracks": []}}, None),
            agent_mail_health_json=LoadedJson(None, None, None),
            validation_broker_json=read_json(malformed_validation_broker),
            rch_json=LoadedJson(None, None, None),
            closeout_freshness_json=read_json(closeout_json),
            now=now,
            stale_hours=DEFAULT_STALE_IN_PROGRESS_HOURS,
        )
        assert_condition(
            malformed_validation_report["status"] == "needs_attention",
            "malformed validation broker JSON should fail closed",
            malformed_validation_report,
        )
        assert_condition(
            malformed_validation_report["coordination"]["validation_broker"]["fail_closed"],
            "malformed validation broker JSON should mark fail_closed",
            malformed_validation_report,
        )

        corrupt_mail = read_fixture_json(
            Path(__file__).resolve().parents[1],
            AGENT_MAIL_SCHEMA_CORRUPT_FIXTURE,
        )
        mail_report = build_report(
            repo_root=root,
            beads_path=beads_path,
            br_ready_json=read_json(ready_json),
            bv_plan_json=read_json(bv_json),
            agent_mail_health_json=corrupt_mail,
            validation_broker_json=LoadedJson(None, None, None),
            rch_json=LoadedJson(None, {"status": "ok", "schema": "fixture.rch.v1"}, None),
            closeout_freshness_json=LoadedJson(None, None, None),
            now=now,
            stale_hours=DEFAULT_STALE_IN_PROGRESS_HOURS,
        )
        assert_condition(
            mail_report["coordination"]["agent_mail"]["status"] == "degraded",
            "corrupt Agent Mail should be degraded, not fatal",
            mail_report,
        )
        assert_condition(
            mail_report["status"] == "ready_work_available",
            "corrupt Agent Mail should not hide ready Beads work",
            mail_report,
        )
        assert_condition(
            any(action["action"] == "use_beads_fallback" for action in mail_report["next_actions"]),
            "corrupt Agent Mail should recommend Beads fallback",
            mail_report,
        )
        assert_condition(
            mail_report["coordination"]["agent_mail"]["semantic_readiness_detail"]
            == "sqlite schema missing required health_check tables: projects, agents, messages, message_recipients",
            "corrupt Agent Mail fixture should preserve missing-table detail",
            mail_report,
        )
        assert_condition(
            mail_report["coordination"]["agent_mail"]["recovery_action"]
            == "Run `am doctor repair --yes` or restore from archive backup",
            "corrupt Agent Mail fixture should preserve exact recovery action",
            mail_report,
        )
    print("SELF-TEST PASS")
    return 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=Path(__file__).resolve().parents[1],
        help="Repository root. Defaults to this script's parent repository.",
    )
    parser.add_argument(
        "--beads-jsonl",
        type=Path,
        help="Path to Beads issues JSONL. Defaults to <repo-root>/.beads/issues.jsonl.",
    )
    parser.add_argument("--br-ready-json", type=Path, help="Optional captured `br ready --json` output.")
    parser.add_argument("--bv-plan-json", type=Path, help="Optional captured `bv --robot-plan` output.")
    parser.add_argument("--agent-mail-health-json", type=Path, help="Optional captured Agent Mail health JSON.")
    parser.add_argument("--validation-broker-json", type=Path, help="Optional captured validation-broker JSON.")
    parser.add_argument("--rch-json", type=Path, help="Optional captured RCH/headroom JSON.")
    parser.add_argument("--closeout-freshness-json", type=Path, help="Optional closeout/runpack freshness JSON.")
    parser.add_argument(
        "--stale-in-progress-hours",
        type=float,
        default=DEFAULT_STALE_IN_PROGRESS_HOURS,
        help="Age threshold for stale in-progress Beads.",
    )
    parser.add_argument("--json", action="store_true", help="emit pretty JSON")
    parser.add_argument("--self-test", action="store_true", help="run fixture-backed self-test")
    return parser.parse_args()


def resolve_input_path(repo_root: Path, path: Path | None) -> Path | None:
    if path is None:
        return None
    return path if path.is_absolute() else repo_root / path


def main() -> int:
    args = parse_args()
    if args.self_test:
        return run_self_test()

    repo_root = args.repo_root.resolve()
    beads_path = resolve_input_path(repo_root, args.beads_jsonl) or repo_root / ".beads/issues.jsonl"
    assert beads_path is not None
    closeout_freshness_json = (
        read_json(resolve_input_path(repo_root, args.closeout_freshness_json))
        if args.closeout_freshness_json is not None
        else run_default_closeout_freshness(repo_root)
    )
    report = build_report(
        repo_root=repo_root,
        beads_path=beads_path,
        br_ready_json=read_json(resolve_input_path(repo_root, args.br_ready_json)),
        bv_plan_json=read_json(resolve_input_path(repo_root, args.bv_plan_json)),
        agent_mail_health_json=read_json(resolve_input_path(repo_root, args.agent_mail_health_json)),
        validation_broker_json=read_json(resolve_input_path(repo_root, args.validation_broker_json)),
        rch_json=read_json(resolve_input_path(repo_root, args.rch_json)),
        closeout_freshness_json=closeout_freshness_json,
        now=utc_now(),
        stale_hours=args.stale_in_progress_hours,
    )
    if args.json:
        sys.stdout.write(json_dumps(report, pretty=True))
    else:
        print_text_report(report)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
