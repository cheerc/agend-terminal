//! #1176: UsageLimit reaction pipeline.
//!
//! - State transition audit log (all transitions → state-transitions.jsonl)
//! - UsageLimit propagation (same-backend → QuotaExceeded)
//! - Telegram notify on UsageLimit events

use crate::state::AgentState;
use std::path::Path;
use std::time::Duration;

/// #3669: retention for `state-transitions.jsonl` — same three dimensions as
/// `mcp::usage_stats` (live bytes + generation count + max age). Kept in sync
/// with `crate::jsonl_retention`'s retention table.
pub const TRANSITIONS_MAX_LIVE_BYTES: u64 = 10 * 1024 * 1024;
pub const TRANSITIONS_MAX_ROTATED_FILES: usize = 5;
pub const TRANSITIONS_MAX_ROTATED_AGE: Duration = Duration::from_secs(30 * 86400);

/// #1527: log a state transition to `state-transitions.jsonl` with an explicit
/// `ts` — the instant the transition was RECORDED (`StateTracker::record_set`),
/// not the later drain time. The supervisor drains buffered transitions and
/// logs each with its captured timestamp so the on-disk order + times reflect
/// reality.
///
/// #3669: the append runs through `crate::jsonl_retention`
/// (companion-lock serialized + rotate/prune on write). The lock is the
/// advisory companion file (`state-transitions.jsonl.lock`), NOT the agent
/// core mutex — so this stays #1492-safe: no registry/core lock is taken,
/// and the self-IPC assert only fires on paths that actually self-IPC.
pub fn log_state_transition_at(
    home: &Path,
    agent: &str,
    from: AgentState,
    to: AgentState,
    ts: &str,
    pty_snippet: &str,
) {
    let snippet: String = pty_snippet.chars().take(200).collect();
    let entry = serde_json::json!({
        "ts": ts,
        "agent": agent,
        "from": from.display_name(),
        "to": to.display_name(),
        "pty_snippet": snippet,
    });
    let path = home.join("state-transitions.jsonl");
    let _ = crate::jsonl_retention::append_line_with_retention(
        &path,
        &entry,
        crate::jsonl_retention::RetentionPolicy {
            max_live_bytes: TRANSITIONS_MAX_LIVE_BYTES,
            max_rotated_files: TRANSITIONS_MAX_ROTATED_FILES,
            max_rotated_age: TRANSITIONS_MAX_ROTATED_AGE,
        },
    );
}

/// Propagate UsageLimit: set QuotaExceeded on all same-backend agents.
/// Returns the list of affected agent names.
pub fn propagate_usage_limit(
    home: &Path,
    source_agent: &str,
    source_backend: &crate::backend::Backend,
    registry: &crate::agent::AgentRegistry,
) -> Vec<String> {
    let names: Vec<_> = crate::agent::lock_registry(registry)
        .values()
        .map(|handle| handle.name.to_string())
        .collect();
    // Job quota handling belongs to its controller, not a peer's observation.
    let job_workers: std::collections::HashSet<_> = names
        .into_iter()
        .filter(|name| crate::schedule_jobs::owns_worker(home, name))
        .collect();
    let mut affected = Vec::new();
    let reg = crate::agent::lock_registry(registry);
    for handle in reg.values() {
        if handle.name.as_str() == source_agent || job_workers.contains(handle.name.as_str()) {
            continue;
        }
        let their_backend = handle
            .declared_backend
            .clone()
            .or_else(|| crate::backend::Backend::from_command(&handle.backend_command));
        if their_backend.as_ref() == Some(source_backend) {
            let mut core = handle.core.lock();
            core.health
                .set_blocked_reason(crate::health::BlockedReason::QuotaExceeded);
            affected.push(handle.name.to_string());
        }
    }
    drop(reg);

    // Log propagation event
    crate::event_log::log(
        home,
        "usage_limit_propagated",
        source_agent,
        &format!(
            "backend={:?} affected=[{}]",
            source_backend,
            affected.join(", ")
        ),
    );
    affected
}

/// Notify operator via telegram about UsageLimit event.
/// Uses the reply channel if active, falls back to event log.
pub fn notify_operator_usage_limit(
    home: &Path,
    agent: &str,
    backend: &crate::backend::Backend,
    pty_snippet: &str,
    affected: &[String],
) {
    let snippet: String = pty_snippet.chars().take(200).collect();
    let affected_str = if affected.is_empty() {
        "propagation disabled".to_string()
    } else {
        affected.join(", ")
    };
    let text = format!(
        "[usage_limit] agent={agent} backend={backend:?} affected=[{affected_str}] snippet={snippet}"
    );
    crate::event_log::log(home, "usage_limit_detected", agent, &text);
    tracing::error!(
        agent,
        backend = ?backend,
        affected = ?affected,
        "UsageLimit detected — operator notification"
    );
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn log_state_transition_creates_file() {
        let dir = std::env::temp_dir().join("agend-test-state-transitions");
        std::fs::create_dir_all(&dir).ok();
        std::fs::remove_file(dir.join("state-transitions.jsonl")).ok();

        log_state_transition_at(
            &dir,
            "dev",
            AgentState::Idle,
            AgentState::UsageLimit,
            "2026-05-31T00:00:00+00:00",
            "You've hit your limit",
        );

        let content = std::fs::read_to_string(dir.join("state-transitions.jsonl")).unwrap();
        assert!(content.contains("\"agent\":\"dev\""));
        assert!(content.contains("\"to\":\"usage_limit\""));
        assert!(content.contains("hit your limit"));
        assert!(
            content.contains("\"ts\":\"2026-05-31T00:00:00+00:00\""),
            "must use the explicit (push-time) ts: {content}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #3669: an oversized live transitions file rotates down on the next
    /// logged transition (same append-then-rotate pattern as usage_stats).
    #[test]
    fn oversized_transitions_rotate_on_next_write_3669() {
        let dir = std::env::temp_dir().join(format!(
            "agend-3669-transitions-rotate-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("state-transitions.jsonl");
        std::fs::write(&path, format!("old:{}\n", "x".repeat(11 * 1024 * 1024))).ok();

        log_state_transition_at(
            &dir,
            "dev",
            AgentState::Idle,
            AgentState::UsageLimit,
            "2026-09-16T00:00:00+00:00",
            "snippet",
        );

        let live = std::fs::metadata(&path).unwrap().len();
        assert!(
            live <= TRANSITIONS_MAX_LIVE_BYTES,
            "oversized transitions must rotate down on next write; got {live}"
        );
        assert!(
            dir.join("state-transitions.jsonl.1").exists(),
            "rotation must preserve history in generation .1"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
