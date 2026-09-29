use serde_json::Value;
use std::{error::Error, fs, io, path::PathBuf};

fn missing(field: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, field)
}

fn evidence() -> Result<Value, Box<dyn Error>> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("docs/evidence/cooperative-priority-lane-evaluation.json");
    let contents = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&contents)?)
}

#[test]
fn cooperative_priority_lane_evidence_is_claim_gated_no_go() -> Result<(), Box<dyn Error>> {
    let evidence = evidence()?;

    assert_eq!(
        evidence.get("schema").and_then(Value::as_str),
        Some("ra.scheduler.cooperative_priority_lane_evaluation.v1")
    );
    assert_eq!(
        evidence.get("bead_id").and_then(Value::as_str),
        Some("bd-2zcs5.29")
    );
    assert_eq!(
        evidence.get("verdict").and_then(Value::as_str),
        Some("NO_GO")
    );

    let decision = evidence
        .get("decision")
        .and_then(Value::as_object)
        .ok_or_else(|| missing("decision object"))?;
    assert_eq!(
        decision
            .get("worth_implementation_now")
            .and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        decision
            .get("follow_up_implementation_bead_created")
            .and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        decision.get("follow_up_bug_bead").and_then(Value::as_str),
        Some("bd-2zcs5.31")
    );

    let claim_gate = evidence
        .get("claim_gate")
        .and_then(Value::as_object)
        .ok_or_else(|| missing("claim gate object"))?;
    assert_eq!(
        claim_gate
            .get("strict_release_claims_allowed")
            .and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        claim_gate
            .get("implementation_follow_up_allowed")
            .and_then(Value::as_bool),
        Some(false)
    );

    Ok(())
}

#[test]
fn cooperative_priority_lane_required_surfaces_and_metrics_are_explicit()
-> Result<(), Box<dyn Error>> {
    let evidence = evidence()?;

    let workloads = evidence
        .get("workloads")
        .and_then(Value::as_array)
        .ok_or_else(|| missing("workloads array"))?;
    for surface in ["extension_hostcalls", "provider_streaming", "local_tools"] {
        assert!(
            workloads
                .iter()
                .any(|workload| workload.get("surface").and_then(Value::as_str) == Some(surface)),
            "missing workload surface {surface}"
        );
    }

    let metrics = evidence
        .get("required_metrics")
        .and_then(Value::as_object)
        .ok_or_else(|| missing("required metrics object"))?;
    for metric in [
        "cancellation_latency",
        "turn_latency_us",
        "fairness_starvation",
        "final_transcript_equivalence",
    ] {
        assert!(
            metrics.contains_key(metric),
            "missing required metric {metric}"
        );
    }

    assert_eq!(
        metrics
            .get("cancellation_latency")
            .and_then(|metric| metric.get("required_for_promotion"))
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        metrics
            .get("final_transcript_equivalence")
            .and_then(|metric| metric.get("required_for_promotion"))
            .and_then(Value::as_bool),
        Some(true)
    );

    Ok(())
}

#[test]
fn cooperative_priority_lane_candidate_timeout_cannot_claim_latency_win()
-> Result<(), Box<dyn Error>> {
    let evidence = evidence()?;

    assert_eq!(
        evidence
            .pointer("/experiments/baseline/pass/overall")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        evidence
            .pointer("/experiments/candidate/status")
            .and_then(Value::as_str),
        Some("timed_out_no_report")
    );
    assert_eq!(
        evidence
            .pointer("/experiments/candidate/report_emitted")
            .and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        evidence
            .pointer(
                "/required_metrics/turn_latency_us/comparison/candidate_latency_improvement_claimed",
            )
            .and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        evidence
            .pointer("/required_metrics/fairness_starvation/starvation_claim_allowed")
            .and_then(Value::as_bool),
        Some(false)
    );

    Ok(())
}
