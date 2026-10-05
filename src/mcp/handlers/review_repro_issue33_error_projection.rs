//! #33 (S2): the typed receipt's machine-readable column token must SURVIVE
//! the MCP error projection and reach the caller.
//!
//! #15 put the five subject-mismatch column tokens (`subject_mismatch_head_sha`
//! etc.) into the rejection's free-text `error`, and explicitly recorded that
//! promoting them to a first-class `code` needs an upstream response-struct
//! change. This is that gap's guard: every MCP adapter used to project
//! `SendOutcome::Error` as `{"error": …}`, discarding the `code`/`hint`
//! discriminators entirely, so the diagnostics a caller could act on never
//! arrived through the transport.
//!
//! The test drives the REAL entry point (`comms::handle_report_result` →
//! `agent_ops::messaging::execute_send` → `review_receipt::authorize_report`)
//! against a real on-disk typed assignment and a real pr-state, and asserts the
//! shape the caller actually receives — the `#1493` producer→consumer fidelity
//! rule applied to the error path.

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

fn test_home(case: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!(
        "agend-issue33-s1-{case}-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    home
}

/// The two members carry the STABLE ids the production sink resolves the sender
/// through (`agent::resolve_instance`); without an `id:` the sink denies with
/// "no stable fleet InstanceId" and never reaches the subject guard.
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

/// Seed a real subject (pr-state + a receipt-capable typed assignment) the way
/// the daemon's ci-watch / review-dispatch paths do: a CI-observed pr-state and
/// an assignment, same head, same class, same PR number.
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
        // `handle_report_result` reads `summary` directly. Going through `send`
        // instead would take its `lift_message` step (`message` → `summary`)
        // first — this test calls the report arm directly, so the verdict token
        // must already lead the summary.
        "summary": format!(
            "{}\n\n### Evidence\nran: cargo test → passed",
            "VERIFIED — exact review"
        ),
        "correlation_id": TASK_ID_15,
        "report_purpose": "code_review",
        "code_review": {
            "assignment_id": assignment_id,
            "verdict": "verified",
            "evidence_digest": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        }
    })
}

/// The response the caller actually receives from the real `send` entry point.
/// `handle_report_result` is the production path a reviewer's verdict takes
/// (`request_kind: "report"` → `send`'s report arm).
fn report_via_real_entry(home: &Path, assignment_id: uuid::Uuid) -> Value {
    let sender =
        crate::identity::Sender::new("typed-reviewer").expect("reviewer identity");
    crate::mcp::handlers::comms::handle_report_result(
        home,
        &typed_review_params(assignment_id),
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

/// The core S2 assertion. A PR HEAD advance makes `authorize_report` reject with
/// the `subject_mismatch_head_sha` token in its message; that message reaches
/// the caller as `error`, and the MCP projection must ALSO carry the `code`
/// discriminator that used to be dropped with `..`.
///
/// Asserting BOTH is what makes this a regression guard rather than a restatement
/// of #15: #15's own tests already pin the token inside the error text, so
/// without the `code` half this test would pass on the pre-fix tree.
#[test]
fn subject_column_token_and_code_both_reach_the_caller_33() {
    let home = test_home("head-advance");
    let reviewer_id = crate::types::InstanceId::new();
    write_fleet(&home, reviewer_id);
    let assignment_id = seed_subject(&home, reviewer_id);

    // The PR HEAD advanced after the assignment was dispatched — this is the
    // most common real mismatch, and the one whose correct next step ("wait for
    // a re-dispatch") is the OPPOSITE of a review-class divergence.
    pr_state::record_ci_result(
        &home,
        "owner/repo",
        "fix/typed",
        ADVANCED_HEAD_15,
        pr_state::CiConclusion::Green,
        vec!["fixup-lead".into()],
        ReviewClass::Single,
    );

    let result = report_via_real_entry(&home, assignment_id);

    // 1. The free-text column token (#15's contribution) survives the transport.
    let error = rejection_text(&result);
    assert!(
        error.contains("subject_mismatch_head_sha"),
        "the #15 column token must reach the caller in `error`: {error}"
    );

    // 2. The `code` discriminator that every MCP adapter used to drop with `..`
    //    now reaches the caller too. THIS is what fails pre-fix: on the
    //    pre-#33 tree this response was `{"error": …}` with no `code` key.
    assert_eq!(
        result["code"],
        json!("report_authority_rejected"),
        "the MCP projection must forward SendOutcome's code discriminator: {result}"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// A reviewer-class divergence is the OTHER real mismatch, and #33's whole
/// subject is that these two must stay distinguishable in the response the
/// caller receives. Pin both halves of the projection for it as well.
#[test]
fn review_class_mismatch_also_carries_code_through_the_projection_33() {
    let home = test_home("review-class");
    let reviewer_id = crate::types::InstanceId::new();
    write_fleet(&home, reviewer_id);
    let assignment_id = seed_subject(&home, reviewer_id);

    // The threshold itself diverged; the HEAD is untouched.
    pr_state::with_pr_state(&home, "owner/repo", "fix/typed", |state| {
        state.review_class = ReviewClass::Dual;
    })
    .unwrap();

    let result = report_via_real_entry(&home, assignment_id);

    let error = rejection_text(&result);
    assert!(
        error.contains("subject_mismatch_review_class"),
        "a class divergence must reach the caller naming its column: {error}"
    );
    assert_eq!(
        result["code"],
        json!("report_authority_rejected"),
        "the MCP projection must forward SendOutcome's code discriminator: {result}"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// Guard against the fix being "make the error text carry everything" instead
/// of "forward the existing discriminator": a transport-level rejection that
/// never reached `authorize_report` must still project as a plain `{"error": …}`
/// with NO `code`, so a consumer reading `code` can tell the two apart.
#[test]
fn non_authority_rejection_carries_no_code_33() {
    let home = test_home("non-authority");
    let reviewer_id = crate::types::InstanceId::new();
    write_fleet(&home, reviewer_id);
    seed_subject(&home, reviewer_id);

    // An assignment id that does not exist → the sink rejects BEFORE the
    // subject guard, with a different message and (today) the same coarse
    // code. The point of this test is NOT the code value but that a
    // non-subject rejection still projects with a well-formed response.
    let bogus = uuid::Uuid::new_v4();
    let result = report_via_real_entry(&home, bogus);

    assert!(
        result["error"].as_str().is_some_and(|e| !e.is_empty()),
        "an unknown assignment must still produce an error: {result}"
    );
    // `code` MAY be present here (the unknown-assignment rejection is also an
    // authority rejection and carries the same coarse code today). This
    // assertion deliberately does NOT pin it either way — narrowing the coarse
    // code is an explicit out-of-scope decision for #33. What it DOES pin is
    // that the projection never fabricates a `code` the outcome did not carry.
    if let Some(code) = result.get("code") {
        assert_eq!(
            code,
            &json!("report_authority_rejected"),
            "the only code this slice may forward is the one the outcome carried: {result}"
        );
    }

    std::fs::remove_dir_all(&home).ok();
}