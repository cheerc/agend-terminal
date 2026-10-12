//! agend-hook-observer — #20 step 1 (R1) daemon-owned observer helper.
//!
//! A named branch mismatch the daemon-owned `prepare-commit-msg` hook observes
//! is measured here: parse the hook-supplied observer fields, assemble ONE fixed
//! observer event (`kind = "hook_branch_mismatch"` — a main-repo-owned kind,
//! never the vendor `git_event` envelope), and append it through the shared
//! serialized appender (`append_audit_line_best_effort`: one non-blocking lock
//! attempt, skip on contention, no unlocked fallback, no bounded wait).
//!
//! HARD BOUNDARIES (General d-20261012073745427104-1, d-20261012075922736592-2):
//! * append-only. No generic JSON/kind/destination writer: every field is
//!   positional argv, the kind is a constant, the sink is the one shared
//!   appender. Malformed or non-mismatch input exits 0 with no record.
//! * Never blocks the commit path. Every failure — missing home, parse
//!   failure, lock contention, I/O failure — exits 0. `TryOnce` only promises
//!   the lock is never waited on; it does not promise the whole I/O is
//!   non-blocking, and this helper claims nothing more.
//! * No daemon state: no bootstrap, no socket, no registry, no git, no config.
//!   Inputs are argv + file paths only.

use std::path::PathBuf;

/// The single observer event kind this helper may emit. Fixed — argv can never
/// change it, so this binary cannot be repurposed as a generic audit writer.
const OBSERVER_KIND: &str = "hook_branch_mismatch";

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2).find_map(|w| {
        if w[0] == flag {
            Some(w[1].clone())
        } else {
            None
        }
    })
}

fn main() {
    // Exit-0 on every path: this helper measures; it never refuses a commit.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(home) = flag_value(&args, "--home") else {
        return;
    };
    let Some(agent) = flag_value(&args, "--agent") else {
        return;
    };
    let Some(task) = flag_value(&args, "--task") else {
        return;
    };
    let Some(bound) = flag_value(&args, "--bound-branch") else {
        return;
    };
    let Some(actual) = flag_value(&args, "--actual-branch") else {
        return;
    };
    let Some(head_raw) = flag_value(&args, "--head-raw") else {
        return;
    };
    let Some(commit_source) = flag_value(&args, "--commit-source") else {
        return;
    };
    if home.is_empty() || agent.is_empty() {
        return;
    }

    // Record ONLY a provable named mismatch. Every other withhold shape —
    // match, detached, unknown, binding-empty, existing short-circuits — skips
    // silently. Byte comparison, matching the hook's own semantics.
    if bound.is_empty() || actual.is_empty() || bound == actual {
        return;
    }

    let event = serde_json::json!({
        "kind": OBSERVER_KIND,
        "agent": agent,
        "task": task,
        "bound_branch": bound,
        "actual_branch": actual,
        "head_raw": head_raw,
        "commit_source": commit_source,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });
    let _ = agentic_audit_append::append_audit_line_best_effort(&PathBuf::from(home), &event);
}
