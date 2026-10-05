//! #15 real-entry regressions for the typed receipt's active-assignment subject
//! mismatch diagnostics.
//!
//! These drive the PRODUCTION MCP `send` entry point
//! (`comms::handle_send_to_instance` → `agent_ops::messaging::execute_send` →
//! `review_receipt::authorize_report`) against a real on-disk typed assignment
//! and a real pr-state file, so what is asserted is the shape the reviewer
//! actually receives — not a hand-fed helper input (#1493 producer→consumer
//! fidelity, applied to the error path).
//!
//! The subject-mismatch guard is normally UNREACHABLE through the real entry
//! point, because the pr-state file is loaded by a path derived from the
//! assignment's own repo+branch: advancing the head re-keys nothing, and a
//! caller cannot mutate the file without reaching inside the production sink.
//! So each case reaches the real guard by perturbing exactly ONE persisted
//! column through the production writer that owns it, and asserts the
//! diagnostics differ. Acceptance point 4 of the dispatch — "the comparison
//! logic must stay byte-equivalent" — is what this whole construction rests on:
//! the fixtures are only admissible *because* the guard did not move.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::daemon::pr_state::{self, ReviewClass};
use crate::mcp::handlers::dispatch::RuntimeContext;
use crate::review_receipt::ReviewSlot;

const REVIEW_HEAD_15: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const ADVANCED_HEAD_15: &str = "cccccccccccccccccccccccccccccccccccccccc";
const TASK_ID_15: &str = "t-code-review-15";
const PR_NUMBER_15: u64 = 2769;

fn minimal_runtime() -> RuntimeContext {
    RuntimeContext {
        registry: std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
        configs: Default::default(),
        externals: std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
        capability: crate::api::RestartCapability::Unsupported,
        app_restart: None,
        post_flush: None,
        notifier: None,
        shutdown: None,
    }
}

/// One `AGEND_HOME` per case, `agend-`-prefixed per the repo's temp-fixture
/// isolation convention.
fn test_home(case: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!(
        "agend-issue15-{case}-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    home
}

/// A fleet whose two members carry the STABLE ids the production sink resolves
/// the sender through (`agent::resolve_instance`). Without an `id:` the sink
/// denies with "no stable fleet InstanceId" and never reaches the guard.
fn write_fleet(home: &Path, reviewer_id: crate::types::InstanceId) {
    std::fs::write(
        crate::fleet::fleet_yaml_path(home),
        format!(
            "instances:\n  fixup-lead:\n    backend: claude\n    id: {}\n  typed-reviewer:\n    backend: claude\n    id: {}\nteams:\n  fixup:\n    members: [fixup-lead, typed-reviewer]\n    orchestrator: fixup-lead\n",
            crate::types::InstanceId::new().full(),
            reviewer_id.full(),
        ),
    )
    .unwrap();
}

/// Seed a real subject the way the daemon's ci-watch + review-dispatch paths do:
/// a CI-observed pr-state and a receipt-capable typed assignment, same head,
/// same class, same PR number.
fn seed_subject(home: &Path, reviewer_id: crate::types::InstanceId) -> uuid::Uuid {
    pr_state::record_ci_result(
        home,
        "owner/repo",
        "fix/typed",
        REVIEW_HEAD_15,
        pr_state::CiConclusion::Green,
        vec!["fixup-lead".into()],
        ReviewClass::Single,
    );
    pr_state::with_pr_state(home, "owner/repo", "fix/typed", |state| {
        state.pr_number = PR_NUMBER_15;
    })
    .unwrap();
    let assignment = crate::daemon::assignment_authority::ActiveAssignment::new_pending_typed(
        "owner/repo",
        "fix/typed",
        "typed-reviewer",
        reviewer_id,
        PR_NUMBER_15,
        REVIEW_HEAD_15,
        ReviewSlot::Primary,
        "fixup-lead",
        TASK_ID_15,
        ReviewClass::Single,
        crate::mcp::handlers::comms_gates::ReviewAuthor::External("octocat".into()),
        "review exact head",
        None,
        None,
        "2026-07-14T00:00:00Z",
    );
    crate::daemon::assignment_authority::persist(home, &assignment).unwrap();
    assignment.assignment_id
}

fn typed_review_params(assignment_id: uuid::Uuid) -> Value {
    json!({
        "instance": "fixup-lead",
        "message": crate::mcp::handlers::build_report_text(
            "VERIFIED — exact review\n\n### Evidence\nran: cargo test → passed",
            Some(TASK_ID_15),
            None,
        ),
        "request_kind": "report",
        "correlation_id": TASK_ID_15,
        "reviewed_head": "display-only-caller-value",
        "report_purpose": "code_review",
        "code_review": {
            "assignment_id": assignment_id,
            "verdict": "verified",
            "evidence_digest": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        }
    })
}

/// The response the caller actually receives from the real `send` entry point.
fn send_typed_review(home: &Path, assignment_id: uuid::Uuid) -> Value {
    let sender = crate::identity::Sender::new("typed-reviewer").expect("reviewer identity");
    crate::mcp::handlers::comms::handle_send_to_instance(
        home,
        &typed_review_params(assignment_id),
        "send",
        &Some(sender),
        Some(&minimal_runtime()),
    )
}

fn rejection_text(result: &Value) -> String {
    result["error"]
        .as_str()
        .unwrap_or_else(|| panic!("expected a rejection carrying an error, got {result}"))
        .to_string()
}

/// #15's headline: a PR HEAD advance and a review-class divergence carry
/// OPPOSITE correct dispositions ("wait for a re-dispatch" vs "check task
/// authority"), so the two rejections must be distinguishable — by the
/// machine-readable column token, and by which next step the text names.
#[test]
fn head_advance_and_review_class_mismatch_are_distinguishable_15() {
    // Control first: the untouched subject must be ACCEPTED, proving the two
    // rejections below are caused by the perturbation and not by fixture drift.
    let control_home = test_home("control");
    let control_id = crate::types::InstanceId::new();
    write_fleet(&control_home, control_id);
    let control_assignment = seed_subject(&control_home, control_id);
    let control = send_typed_review(&control_home, control_assignment);
    assert!(
        control.get("error").is_none(),
        "an aligned subject must still be accepted: {control}"
    );
    std::fs::remove_dir_all(&control_home).ok();

    // Case A — the PR HEAD advanced after the assignment was dispatched.
    let head_home = test_home("head-advance");
    let head_id = crate::types::InstanceId::new();
    write_fleet(&head_home, head_id);
    let head_assignment = seed_subject(&head_home, head_id);
    pr_state::record_ci_result(
        &head_home,
        "owner/repo",
        "fix/typed",
        ADVANCED_HEAD_15,
        pr_state::CiConclusion::Green,
        vec!["fixup-lead".into()],
        ReviewClass::Single,
    );
    let head_error = rejection_text(&send_typed_review(&head_home, head_assignment));
    assert!(
        head_error.contains("subject_mismatch_head_sha"),
        "a head advance must report the head_sha column: {head_error}"
    );
    assert!(
        head_error.contains("re-dispatch"),
        "a head advance's next step is waiting for a fresh assignment: {head_error}"
    );
    assert!(
        !head_error.contains("subject_mismatch_review_class"),
        "a head advance must not be reported as a review-class mismatch: {head_error}"
    );
    std::fs::remove_dir_all(&head_home).ok();

    // Case B — the review threshold itself diverged (task authority vs the
    // PR's reconciled class), with the HEAD untouched.
    let class_home = test_home("review-class");
    let class_id = crate::types::InstanceId::new();
    write_fleet(&class_home, class_id);
    let class_assignment = seed_subject(&class_home, class_id);
    pr_state::with_pr_state(&class_home, "owner/repo", "fix/typed", |state| {
        state.review_class = ReviewClass::Dual;
    })
    .unwrap();
    let class_error = rejection_text(&send_typed_review(&class_home, class_assignment));
    assert!(
        class_error.contains("subject_mismatch_review_class"),
        "a class divergence must report the review_class column: {class_error}"
    );
    assert!(
        class_error.contains("task"),
        "a class divergence's next step is checking task authority: {class_error}"
    );
    assert!(
        !class_error.contains("subject_mismatch_head_sha"),
        "a class divergence must not be reported as a head advance: {class_error}"
    );

    // The two diagnostics are genuinely different text, not one string reused.
    assert_ne!(
        head_error, class_error,
        "#15's whole point: the two mismatches must not share one message"
    );
    // Both mention a re-dispatch, but with OPPOSITE polarity — which is the
    // disposition difference #15 is really about: a head advance tells the
    // reviewer to WAIT for one; a class divergence tells them NOT to.
    assert!(
        head_error.contains("Wait for the dispatching lead to re-dispatch a fresh assignment"),
        "a head advance's next step is waiting for a fresh assignment: {head_error}"
    );
    assert!(
        class_error.contains("do NOT wait for a re-dispatch"),
        "a class divergence's next step is explicitly NOT waiting: {class_error}"
    );
    std::fs::remove_dir_all(&class_home).ok();
}

/// Every column of the guard reports its OWN name. The two near-tautological
/// columns (#15 flags `state.repo`/`state.branch` as round-trip identities) are
/// NOT silently dropped: the diagnostic still names them correctly when the
/// persisted file says otherwise. A hand-written pr-state file is the only way
/// to reach them — `pr_state_filename` derives the load path from the
/// assignment's own repo+branch — so the fixture here is the deliberate
/// odd-one-out, and it keeps the guard's coverage intact.
#[test]
fn every_subject_column_reports_its_own_name_15() {
    let home = test_home("all-columns");
    let reviewer_id = crate::types::InstanceId::new();
    write_fleet(&home, reviewer_id);
    let assignment_id = seed_subject(&home, reviewer_id);

    let path =
        pr_state::pr_state_dir(&home).join(pr_state::pr_state_filename("owner/repo", "fix/typed"));
    let original = std::fs::read(&path).expect("seeded pr-state");

    // Each case perturbs exactly one persisted column, in the guard's own
    // evaluation order, so the reported column is the FIRST one that differs.
    // `review_class` uses the derived `PrState` representation (the variant
    // name, NOT the `single`/`dual` token form) — the same bytes
    // `to_string_pretty` writes at persist time. The expected-value token is
    // the `as_token()` form the diagnostic prints, which differs from the
    // serialized form; that difference is itself what keeps the assertion from
    // passing on an arbitrary substring.
    let cases: [(&str, &str, Value, &str); 5] = [
        (
            "repo",
            "subject_mismatch_repo",
            json!("other/repo"),
            "other/repo",
        ),
        (
            "branch",
            "subject_mismatch_branch",
            json!("fix/other"),
            "fix/other",
        ),
        (
            "pr_number",
            "subject_mismatch_pr_number",
            json!(3131),
            "3131",
        ),
        (
            "head_sha",
            "subject_mismatch_head_sha",
            json!(ADVANCED_HEAD_15),
            ADVANCED_HEAD_15,
        ),
        (
            "review_class",
            "subject_mismatch_review_class",
            json!("Dual"),
            "dual",
        ),
    ];

    for (column, code, perturbed, expected_value) in cases {
        let mut state: Value = serde_json::from_slice(&original).expect("pr-state parses");
        state[column] = perturbed;
        std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();

        let error = rejection_text(&send_typed_review(&home, assignment_id));
        assert!(
            error.contains(code),
            "a diverging {column} must be reported as {code}: {error}"
        );
        assert!(
            error.contains(expected_value),
            "the {code} diagnostic must show the observed value {expected_value}: {error}"
        );
    }

    // Restoring the byte-exact seed must restore acceptance — proving the
    // rejections above were each caused solely by their own column.
    std::fs::write(&path, &original).unwrap();
    let restored = send_typed_review(&home, assignment_id);
    assert!(
        restored.get("error").is_none(),
        "restoring the seed must restore acceptance: {restored}"
    );
    std::fs::remove_dir_all(&home).ok();
}
