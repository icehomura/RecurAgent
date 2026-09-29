#!/usr/bin/env python3
"""Dry-run per-agent swarm budget lease simulator.

The simulator consumes a requested budget profile, active leases, and new agent
requests. It emits deterministic recommendations only; it does not reserve
capacity, mutate Beads, change Agent Mail state, or enforce runtime throttles.
"""

from __future__ import annotations

import argparse
import json
import sys
from datetime import datetime, timedelta, timezone
from fractions import Fraction
from pathlib import Path
from typing import Any


INPUT_SCHEMA = "ra.swarm.budget_lease_request.v1"
OUTPUT_SCHEMA = "ra.swarm.budget_lease_simulation.v1"
CONTRACT_SCHEMA = "ra.swarm.budget_lease_simulator_contract.v1"
FIXTURE_SCHEMA = "ra.swarm.budget_lease_simulator_fixtures.v1"
CONTRACT_PATH = Path("docs/contracts/swarm-budget-lease-simulator-contract.json")
FIXTURE_PATH = Path("tests/fixtures/swarm_budget_leases/scenarios.json")
DEFAULT_TTL_SECONDS = 3600
RESOURCE_KEYS = (
    "provider_tokens",
    "provider_spend_microusd",
    "tool_invocations",
    "rch_slots",
    "temp_dir_bytes",
    "session_write_bytes",
    "evidence_write_bytes",
)
DECISIONS = ("grant", "partial", "reject")
STATUSES = ("admit", "limited", "blocked")


class SimulationError(Exception):
    """Raised when input or contract data is not usable."""


def json_dumps(value: Any) -> str:
    return json.dumps(value, indent=2, sort_keys=True) + "\n"


def parse_utc(value: str) -> datetime:
    text = value.strip()
    if text.endswith("Z"):
        text = f"{text[:-1]}+00:00"
    parsed = datetime.fromisoformat(text)
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=timezone.utc)
    return parsed.astimezone(timezone.utc)


def utc_now_iso() -> str:
    return datetime.now(timezone.utc).replace(microsecond=0).isoformat()


def int_value(value: Any, *, field: str) -> int:
    if isinstance(value, bool):
        raise SimulationError(f"{field} must be an integer, not bool")
    if value is None:
        return 0
    if isinstance(value, int):
        result = value
    elif isinstance(value, str) and value.strip().isdigit():
        result = int(value.strip())
    else:
        raise SimulationError(f"{field} must be a non-negative integer")
    if result < 0:
        raise SimulationError(f"{field} must be non-negative")
    return result


def resource_map(value: Any, *, field: str) -> dict[str, int]:
    if value is None:
        value = {}
    if not isinstance(value, dict):
        raise SimulationError(f"{field} must be an object")
    return {
        key: int_value(value.get(key), field=f"{field}.{key}")
        for key in RESOURCE_KEYS
    }


def priority_weight(priority: Any) -> int:
    if isinstance(priority, str):
        text = priority.strip().upper()
        if text.startswith("P") and text[1:].isdigit():
            priority = int(text[1:])
        elif text.isdigit():
            priority = int(text)
        else:
            return 1
    if not isinstance(priority, int) or isinstance(priority, bool):
        return 1
    priority = max(0, min(4, priority))
    return 5 - priority


def load_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError as exc:
        raise SimulationError(f"missing JSON file: {path}") from exc
    except json.JSONDecodeError as exc:
        raise SimulationError(f"malformed JSON file {path}: {exc}") from exc


def stable_request_id(item: dict[str, Any], index: int) -> str:
    value = item.get("request_id") or item.get("id")
    if value:
        return str(value)
    agent = str(item.get("agent") or f"agent-{index + 1}")
    return f"{agent}-request"


def normalize_requests(payload: dict[str, Any]) -> list[dict[str, Any]]:
    raw_requests = payload.get("requests")
    if not isinstance(raw_requests, list) or not raw_requests:
        raise SimulationError("requests must be a non-empty array")
    normalized = []
    for index, item in enumerate(raw_requests):
        if not isinstance(item, dict):
            raise SimulationError(f"requests[{index}] must be an object")
        agent = str(item.get("agent") or "").strip()
        if not agent:
            raise SimulationError(f"requests[{index}].agent is required")
        requested = resource_map(item.get("requested"), field=f"requests[{index}].requested")
        minimum = resource_map(item.get("minimum"), field=f"requests[{index}].minimum")
        for key in RESOURCE_KEYS:
            if minimum[key] > requested[key]:
                raise SimulationError(
                    f"requests[{index}].minimum.{key} exceeds requested.{key}"
                )
        priority = item.get("priority", 2)
        normalized.append(
            {
                "index": index,
                "request_id": stable_request_id(item, index),
                "agent": agent,
                "priority": priority,
                "priority_weight": priority_weight(priority),
                "requested": requested,
                "minimum": minimum,
            }
        )
    return sorted(
        normalized,
        key=lambda item: (
            -int(item["priority_weight"]),
            str(item["agent"]),
            str(item["request_id"]),
        ),
    )


def normalize_leases(
    payload: dict[str, Any],
    *,
    generated_at: datetime,
) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    active = []
    expired = []
    raw_leases = payload.get("existing_leases") or []
    if not isinstance(raw_leases, list):
        raise SimulationError("existing_leases must be an array when provided")
    for index, item in enumerate(raw_leases):
        if not isinstance(item, dict):
            raise SimulationError(f"existing_leases[{index}] must be an object")
        expires_at = str(item.get("expires_at") or "").strip()
        if not expires_at:
            raise SimulationError(f"existing_leases[{index}].expires_at is required")
        parsed_expires_at = parse_utc(expires_at)
        lease = {
            "lease_id": str(item.get("lease_id") or item.get("id") or f"lease-{index + 1}"),
            "agent": str(item.get("agent") or "unknown"),
            "priority": item.get("priority", 2),
            "expires_at": parsed_expires_at.isoformat(),
            "resources": resource_map(
                item.get("resources"),
                field=f"existing_leases[{index}].resources",
            ),
        }
        if parsed_expires_at <= generated_at:
            lease["expired"] = True
            expired.append(lease)
        else:
            lease["expired"] = False
            active.append(lease)
    return (
        sorted(active, key=lambda item: (str(item["agent"]), str(item["lease_id"]))),
        sorted(expired, key=lambda item: (str(item["agent"]), str(item["lease_id"]))),
    )


def distribute_integer_budget(
    budget: int,
    caps: list[int],
    weights: list[int],
    sort_keys: list[tuple[Any, ...]],
) -> list[int]:
    allocated = [0 for _ in caps]
    remaining = budget
    while remaining > 0 and any(cap > allocated[index] for index, cap in enumerate(caps)):
        eligible = [
            index for index, cap in enumerate(caps) if cap > allocated[index]
        ]
        total_weight = sum(max(1, weights[index]) for index in eligible)
        shares: list[tuple[Fraction, int]] = []
        used = 0
        for index in eligible:
            weight = max(1, weights[index])
            raw_share = Fraction(remaining * weight, total_weight)
            grant = min(caps[index] - allocated[index], raw_share.numerator // raw_share.denominator)
            if grant:
                allocated[index] += grant
                used += grant
            shares.append((raw_share - int(raw_share), index))
        remaining -= used
        if used:
            continue
        shares.sort(key=lambda item: (-item[0], sort_keys[item[1]]))
        for _remainder, index in shares:
            if remaining == 0:
                break
            if allocated[index] < caps[index]:
                allocated[index] += 1
                remaining -= 1
    return allocated


def allocate_resource(
    key: str,
    *,
    capacity: int,
    active_reserved: int,
    expired_reserved: int,
    requests: list[dict[str, Any]],
) -> tuple[dict[str, Any], list[int]]:
    available = max(0, capacity - active_reserved)
    requested_values = [request["requested"][key] for request in requests]
    minimum_values = [request["minimum"][key] for request in requests]
    requested_total = sum(requested_values)
    minimum_total = sum(minimum_values)
    allocations = [0 for _ in requests]
    status = "ok"
    if requested_total == 0:
        status = "unused"
    elif requested_total <= available:
        allocations = requested_values[:]
    elif minimum_total > available:
        status = "hard_conflict"
        allocations = distribute_integer_budget(
            available,
            minimum_values,
            [request["priority_weight"] for request in requests],
            [
                (request["agent"], request["request_id"])
                for request in requests
            ],
        )
    else:
        status = "trimmed"
        allocations = minimum_values[:]
        remaining = available - minimum_total
        caps = [
            requested_values[index] - minimum_values[index]
            for index in range(len(requests))
        ]
        increments = distribute_integer_budget(
            remaining,
            caps,
            [request["priority_weight"] for request in requests],
            [
                (request["agent"], request["request_id"])
                for request in requests
            ],
        )
        allocations = [
            allocations[index] + increments[index]
            for index in range(len(requests))
        ]
    summary = {
        "capacity": capacity,
        "active_reserved": active_reserved,
        "expired_ignored": expired_reserved,
        "available": available,
        "requested_total": requested_total,
        "minimum_total": minimum_total,
        "allocated_total": sum(allocations),
        "status": status,
    }
    return summary, allocations


def conflict_for_resource(
    key: str,
    summary: dict[str, Any],
    requests: list[dict[str, Any]],
) -> list[dict[str, Any]]:
    requested_total = int(summary["requested_total"])
    if requested_total == 0 or requested_total <= int(summary["available"]):
        return []
    agents = [request["agent"] for request in requests if request["requested"][key] > 0]
    conflicts = []
    available = int(summary["available"])
    minimum_total = int(summary["minimum_total"])
    if int(summary["minimum_total"]) > int(summary["available"]):
        conflicts.append(
            (
                "over_commit_rejected",
                "critical",
                f"{key} minimum leases exceed currently available capacity",
            )
        )
    if key == "rch_slots" and available == 0:
        conflicts.append(
            (
                "rch_saturation",
                "high" if minimum_total <= available else "critical",
                "RCH slot leases are saturated by active reservations",
            )
        )
    if key in {"provider_tokens", "provider_spend_microusd"}:
        conflicts.append(
            (
                "provider_budget_exhausted",
                "high" if minimum_total <= available else "critical",
                f"{key} budget cannot cover requested provider leases",
            )
        )
    if not conflicts:
        conflicts.append(
            (
                "lease_trimmed",
                "medium",
                f"{key} requests exceed currently available capacity",
            )
        )
    return [
        {
            "id": f"{key}:{conflict_type}",
            "type": conflict_type,
            "severity": severity,
            "resource": key,
            "agents": agents,
            "requested_total": summary["requested_total"],
            "minimum_total": summary["minimum_total"],
            "available": summary["available"],
            "active_reserved": summary["active_reserved"],
            "explanation": explanation,
            "evidence_paths": [
                f"resource_summaries.{key}.requested_total",
                f"resource_summaries.{key}.available",
                f"resource_summaries.{key}.active_reserved",
            ],
            "recommendation": (
                "lower requested leases, wait for active leases to expire, "
                "or increase the accepted swarm budget before admitting more agents"
            ),
        }
        for conflict_type, severity, explanation in conflicts
    ]


def request_decision(
    request: dict[str, Any],
    allocation: dict[str, int],
) -> tuple[str, dict[str, int], list[str]]:
    shortfalls: dict[str, int] = {}
    below_minimum = []
    for key in RESOURCE_KEYS:
        requested = request["requested"][key]
        granted = allocation[key]
        if granted < requested:
            shortfalls[key] = requested - granted
        if granted < request["minimum"][key]:
            below_minimum.append(key)
    if below_minimum:
        return "reject", shortfalls, below_minimum
    if shortfalls:
        return "partial", shortfalls, below_minimum
    return "grant", shortfalls, below_minimum


def load_contract(repo_root: Path) -> dict[str, Any]:
    contract = load_json(repo_root / CONTRACT_PATH)
    if not isinstance(contract, dict):
        raise SimulationError("budget lease simulator contract must be an object")
    if contract.get("schema") != CONTRACT_SCHEMA:
        raise SimulationError(f"unexpected contract schema: {contract.get('schema')}")
    if contract.get("simulation_schema") != OUTPUT_SCHEMA:
        raise SimulationError("contract simulation schema does not match simulator")
    return contract


def assert_contract(simulation: dict[str, Any], *, contract: dict[str, Any]) -> None:
    if simulation.get("schema") != contract.get("simulation_schema"):
        raise AssertionError("simulation schema mismatch")
    if simulation.get("purpose") != contract.get("purpose"):
        raise AssertionError("simulation purpose mismatch")
    if simulation.get("status") not in contract.get("allowed_statuses", []):
        raise AssertionError(f"invalid simulation status: {simulation.get('status')}")
    for key in contract.get("required_top_level_keys", []):
        if key not in simulation:
            raise AssertionError(f"missing top-level key: {key}")
    if list(simulation.get("resource_keys", [])) != list(contract.get("resource_keys", [])):
        raise AssertionError("resource key ordering mismatch")
    for key in contract.get("resource_keys", []):
        if key not in simulation["resource_summaries"]:
            raise AssertionError(f"missing resource summary: {key}")
    for item in simulation.get("recommendations", []):
        if item.get("decision") not in contract.get("allowed_decisions", []):
            raise AssertionError(f"invalid recommendation decision: {item}")
    guards = simulation.get("simulator_guards")
    if not isinstance(guards, dict):
        raise AssertionError("simulator_guards must be an object")
    for guard in contract.get("required_true_guards", []):
        if guards.get(guard) is not True:
            raise AssertionError(f"guard must be true: {guard}")


def build_simulation(
    payload: dict[str, Any],
    *,
    generated_at: str | None,
    contract: dict[str, Any],
) -> dict[str, Any]:
    if not isinstance(payload, dict):
        raise SimulationError("input payload must be an object")
    if payload.get("schema") not in {None, INPUT_SCHEMA}:
        raise SimulationError(f"unexpected input schema: {payload.get('schema')}")
    generated_at = generated_at or payload.get("generated_at") or utc_now_iso()
    generated_at_dt = parse_utc(str(generated_at))
    budgets = resource_map(payload.get("budgets"), field="budgets")
    requests = normalize_requests(payload)
    active_leases, expired_leases = normalize_leases(payload, generated_at=generated_at_dt)
    ttl_seconds = int_value(
        payload.get("lease_ttl_seconds", DEFAULT_TTL_SECONDS),
        field="lease_ttl_seconds",
    )
    if ttl_seconds <= 0:
        raise SimulationError("lease_ttl_seconds must be positive")
    lease_expires_at = (generated_at_dt + timedelta(seconds=ttl_seconds)).isoformat()

    resource_summaries: dict[str, dict[str, Any]] = {}
    allocations = [
        {key: 0 for key in RESOURCE_KEYS}
        for _request in requests
    ]
    conflicts: list[dict[str, Any]] = []
    for key in RESOURCE_KEYS:
        active_reserved = sum(lease["resources"][key] for lease in active_leases)
        expired_reserved = sum(lease["resources"][key] for lease in expired_leases)
        summary, resource_allocations = allocate_resource(
            key,
            capacity=budgets[key],
            active_reserved=active_reserved,
            expired_reserved=expired_reserved,
            requests=requests,
        )
        resource_summaries[key] = summary
        for index, value in enumerate(resource_allocations):
            allocations[index][key] = value
        conflicts.extend(conflict_for_resource(key, summary, requests))

    conflict_resources = {
        conflict["resource"]: conflict
        for conflict in conflicts
        if isinstance(conflict.get("resource"), str)
    }
    recommendations = []
    for index, request in enumerate(requests):
        allocation = allocations[index]
        decision, shortfalls, below_minimum = request_decision(request, allocation)
        request_conflicts = []
        for key in shortfalls:
            if key in conflict_resources:
                request_conflicts.append(conflict_resources[key]["id"])
        recommendations.append(
            {
                "agent": request["agent"],
                "request_id": request["request_id"],
                "priority": request["priority"],
                "priority_weight": request["priority_weight"],
                "decision": decision,
                "requested": request["requested"],
                "minimum": request["minimum"],
                "recommended_lease": allocation,
                "shortfalls": shortfalls,
                "below_minimum": below_minimum,
                "conflicts": sorted(set(request_conflicts)),
                "lease": {
                    "dry_run": True,
                    "ttl_seconds": ttl_seconds,
                    "expires_at": lease_expires_at,
                },
            }
        )

    if any(conflict["severity"] == "critical" for conflict in conflicts):
        status = "blocked"
    elif conflicts or any(item["decision"] == "partial" for item in recommendations):
        status = "limited"
    else:
        status = "admit"

    simulation = {
        "schema": OUTPUT_SCHEMA,
        "generated_at": generated_at_dt.isoformat(),
        "status": status,
        "purpose": contract["purpose"],
        "input_schema": payload.get("schema") or INPUT_SCHEMA,
        "resource_keys": list(RESOURCE_KEYS),
        "summary": {
            "request_count": len(requests),
            "active_lease_count": len(active_leases),
            "expired_lease_count": len(expired_leases),
            "conflict_count": len(conflicts),
            "admit_new_agents": status == "admit",
        },
        "resource_summaries": resource_summaries,
        "recommendations": recommendations,
        "conflicts": conflicts,
        "active_leases": active_leases,
        "expired_leases": expired_leases,
        "simulator_guards": {
            "dry_run_only": True,
            "no_runtime_throttles_enforced": True,
            "no_source_mutation": True,
            "output_overwrite_refusal": True,
            "deterministic_allocation": True,
        },
        "authority_boundary": {
            "does_not_replace": [
                "Beads",
                "Agent Mail",
                "RCH",
                "provider billing limits",
                "filesystem quotas",
                "validation broker",
                "operator judgment",
            ],
            "may_recommend_only": True,
            "must_not_reserve_capacity": True,
        },
    }
    assert_contract(simulation, contract=contract)
    return simulation


def assert_fixture_expectations(
    scenario_id: str,
    simulation: dict[str, Any],
    expected: dict[str, Any],
) -> None:
    if simulation["status"] != expected.get("status"):
        raise AssertionError(
            f"{scenario_id}: expected status {expected.get('status')}, got {simulation['status']}"
        )
    expected_decisions = expected.get("decisions", {})
    actual_decisions = {
        item["agent"]: item["decision"]
        for item in simulation["recommendations"]
    }
    for agent, decision in expected_decisions.items():
        if actual_decisions.get(agent) != decision:
            raise AssertionError(
                f"{scenario_id}: expected {agent} decision {decision}, got {actual_decisions.get(agent)}"
            )
    conflict_types = {item["type"] for item in simulation["conflicts"]}
    for expected_type in expected.get("conflict_types", []):
        if expected_type not in conflict_types:
            raise AssertionError(
                f"{scenario_id}: missing conflict type {expected_type}; got {sorted(conflict_types)}"
            )
    for key, value in expected.get("summary", {}).items():
        if simulation["summary"].get(key) != value:
            raise AssertionError(
                f"{scenario_id}: expected summary.{key}={value}, got {simulation['summary'].get(key)}"
            )
    allocations = {
        item["agent"]: item["recommended_lease"]
        for item in simulation["recommendations"]
    }
    for check in expected.get("allocation_checks", []):
        if "difference_at_most" in check:
            spec = check["difference_at_most"]
            agents = spec["agents"]
            resource = spec["resource"]
            values = [allocations[agent][resource] for agent in agents]
            if max(values) - min(values) > int(spec["value"]):
                raise AssertionError(
                    f"{scenario_id}: expected {resource} allocation spread <= {spec['value']}, got {values}"
                )
        if "greater_than" in check:
            spec = check["greater_than"]
            left = allocations[spec["left_agent"]][spec["resource"]]
            right = allocations[spec["right_agent"]][spec["resource"]]
            if left <= right:
                raise AssertionError(
                    f"{scenario_id}: expected {spec['left_agent']} {left} > {spec['right_agent']} {right}"
                )
        if "at_least" in check:
            spec = check["at_least"]
            value = allocations[spec["agent"]][spec["resource"]]
            if value < int(spec["value"]):
                raise AssertionError(
                    f"{scenario_id}: expected {spec['agent']} {spec['resource']} >= {spec['value']}, got {value}"
                )


def run_self_test(repo_root: Path) -> int:
    contract = load_contract(repo_root)
    fixtures = load_json(repo_root / FIXTURE_PATH)
    if not isinstance(fixtures, dict) or fixtures.get("schema") != FIXTURE_SCHEMA:
        raise SimulationError(f"unexpected fixture schema: {fixtures!r}")
    required_scenarios = set(contract.get("required_fixture_scenarios", []))
    seen = set()
    for scenario in fixtures.get("scenarios", []):
        if not isinstance(scenario, dict):
            raise SimulationError("fixture scenarios must be objects")
        scenario_id = str(scenario.get("id") or "")
        if not scenario_id:
            raise SimulationError("fixture scenario id is required")
        seen.add(scenario_id)
        simulation = build_simulation(
            scenario["input"],
            generated_at=scenario["input"].get("generated_at"),
            contract=contract,
        )
        assert_fixture_expectations(
            scenario_id,
            simulation,
            scenario.get("expected", {}),
        )
    missing = sorted(required_scenarios - seen)
    if missing:
        raise AssertionError(f"missing required fixture scenarios: {missing}")
    print(
        json_dumps(
            {
                "schema": "ra.swarm.budget_lease_simulator_self_test.v1",
                "status": "pass",
                "scenario_count": len(seen),
                "scenarios": sorted(seen),
            }
        ),
        end="",
    )
    return 0


def load_fixture_input(repo_root: Path, fixture_id: str) -> dict[str, Any]:
    fixtures = load_json(repo_root / FIXTURE_PATH)
    if not isinstance(fixtures, dict) or fixtures.get("schema") != FIXTURE_SCHEMA:
        raise SimulationError(f"unexpected fixture schema: {fixtures!r}")
    for scenario in fixtures.get("scenarios", []):
        if isinstance(scenario, dict) and scenario.get("id") == fixture_id:
            payload = scenario.get("input")
            if not isinstance(payload, dict):
                raise SimulationError(f"fixture {fixture_id} input must be an object")
            return payload
    raise SimulationError(f"unknown fixture id: {fixture_id}")


def write_output(path: Path, simulation: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists():
        raise SimulationError(f"refusing to overwrite existing output: {path}")
    path.write_text(json_dumps(simulation), encoding="utf-8")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input-json", type=Path, help="budget lease request JSON")
    parser.add_argument(
        "--fixture-id",
        help="load a named self-test fixture as the input payload",
    )
    parser.add_argument("--out-json", type=Path, help="write simulation JSON; refuses overwrite")
    parser.add_argument("--generated-at", help="override generated_at for deterministic runs")
    parser.add_argument("--json", action="store_true", help="print simulation JSON")
    parser.add_argument("--self-test", action="store_true", help="run fixture-backed self-test")
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=Path("."),
        help="repository root for contract and fixture lookup",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    repo_root = args.repo_root.resolve()
    try:
        if args.self_test:
            return run_self_test(repo_root)
        if args.input_json is not None and args.fixture_id:
            raise SimulationError("use only one of --input-json or --fixture-id")
        if args.input_json is None and not args.fixture_id:
            raise SimulationError(
                "--input-json or --fixture-id is required unless --self-test is used"
            )
        contract = load_contract(repo_root)
        payload = (
            load_fixture_input(repo_root, args.fixture_id)
            if args.fixture_id
            else load_json(args.input_json)
        )
        simulation = build_simulation(
            payload,
            generated_at=args.generated_at,
            contract=contract,
        )
        if args.out_json is not None:
            write_output(args.out_json, simulation)
        if args.json or args.out_json is None:
            print(json_dumps(simulation), end="")
    except (AssertionError, SimulationError, ValueError) as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
