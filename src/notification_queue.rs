use crate::agent_ops;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::time::Duration;

/// #1457: the old fixed compose-idle window. The notification-delivery guards
/// no longer use it (replaced by the input-vs-submit `DraftState`); it now only
/// backs `Pane::is_composing`, a test-only helper — hence `#[cfg(test)]`.
#[cfg(test)]
pub const COMPOSE_IDLE_TIMEOUT: Duration = Duration::from_secs(3);
const COMPOSE_METADATA_KEY: &str = "last_input_epoch_ms";
/// Sprint 54 P2-3: epoch-ms timestamp of the most recent submit-key
/// keystroke (e.g. `\r` for claude). Distinct from
/// `COMPOSE_METADATA_KEY` which records ANY input keystroke. Used by
/// the daemon supervisor to detect "typed but not submitted" state.
const SUBMIT_METADATA_KEY: &str = "last_submit_epoch_ms";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedNotification {
    pub text: String,
    pub timestamp: String,
    /// #1513: actionable work-delivery wake (ci-ready / task / query) vs ambient.
    /// Actionable items drain FIRST and carry a tighter MAX_DEFER cap. `serde
    /// default` keeps pre-#1513 queue lines (no field) deserializing as ambient.
    #[serde(default)]
    pub actionable: bool,
    /// #3324: the external channel this notification originated on, carried
    /// through the DEFERRED path so a queued Telegram message keeps its typed
    /// provenance across the durable hop. `serde default` keeps pre-#3324 rows
    /// deserializing as internal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_origin: Option<crate::channel::ChannelKind>,
    /// #1513: epoch-ms when this item was FIRST deferred. Drives the MAX_DEFER
    /// anti-starvation cap (release after the cap even if the agent stays busy).
    /// Preserved across requeue so the cap counts from the original defer.
    #[serde(default)]
    pub deferred_since_ms: i64,
}

fn queue_path(home: &Path, agent_name: &str) -> PathBuf {
    home.join("notification-queue")
        .join(format!("{agent_name}.jsonl"))
}

/// Legacy fixed-name draining file written by pre-claim-atomic binaries.
/// Production code no longer writes it (claims use unique per-process names),
/// but `list_draining_files` still matches it so stale-claim recovery covers
/// an upgrade-over-crash. Tests use it to simulate a peer's claim.
#[cfg(test)]
fn draining_path(home: &Path, agent_name: &str) -> PathBuf {
    queue_path(home, agent_name).with_extension("draining")
}

/// #3321: latest-wins in-memory activity pair. Both producers update this
/// buffer; the periodic flush drains one pair into one metadata RMW.
#[derive(Debug, Clone)]
struct PendingActivity {
    home: PathBuf,
    agent: String,
    input_ms: Option<i64>,
    submit_ms: Option<i64>,
}

static PENDING_ACTIVITY: std::sync::Mutex<Vec<PendingActivity>> = std::sync::Mutex::new(Vec::new());

fn merge_activity(target: &mut PendingActivity, source: &PendingActivity) {
    if source.home != target.home || source.agent != target.agent {
        return;
    }
    if source.input_ms > target.input_ms {
        target.input_ms = source.input_ms;
    }
    if source.submit_ms > target.submit_ms {
        target.submit_ms = source.submit_ms;
    }
}

fn record_activity(home: &Path, agent_name: &str, input: bool) {
    let timestamp = chrono::Utc::now().timestamp_millis();
    let mut pending = PENDING_ACTIVITY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(entry) = pending
        .iter_mut()
        .find(|entry| entry.home.as_path() == home && entry.agent == agent_name)
    {
        if input {
            entry.input_ms = Some(entry.input_ms.unwrap_or(0).max(timestamp));
        } else {
            entry.submit_ms = Some(entry.submit_ms.unwrap_or(0).max(timestamp));
        }
    } else {
        pending.push(PendingActivity {
            home: home.to_path_buf(),
            agent: agent_name.to_owned(),
            input_ms: input.then_some(timestamp),
            submit_ms: (!input).then_some(timestamp),
        });
    }
}

pub fn record_input_activity(home: &Path, agent_name: &str) {
    record_activity(home, agent_name, true);
}

fn take_pending_activity(home: &Path) -> Vec<PendingActivity> {
    let mut pending = PENDING_ACTIVITY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (drained, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut *pending)
        .into_iter()
        .partition(|entry| entry.home.as_path() == home);
    *pending = kept;
    drained
}

fn activity_values(entry: &PendingActivity) -> Vec<(&str, serde_json::Value)> {
    let mut values = Vec::with_capacity(2);
    if let Some(timestamp) = entry.input_ms {
        values.push((COMPOSE_METADATA_KEY, json!(timestamp)));
    }
    if let Some(timestamp) = entry.submit_ms {
        values.push((SUBMIT_METADATA_KEY, json!(timestamp)));
    }
    values
}

fn requeue_activity(entry: PendingActivity) {
    let mut pending = PENDING_ACTIVITY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(existing) = pending
        .iter_mut()
        .find(|existing| existing.home == entry.home && existing.agent == entry.agent)
    {
        merge_activity(existing, &entry);
    } else {
        pending.push(entry);
    }
}

/// #3321: flush pending activity without waiting on an instance metadata lock.
/// Called from the ~1s `sync_badges` cadence. Contention and persistence
/// errors requeue the drained latest-wins pair; the input/render loop never
/// falls back to a blocking metadata write.
pub fn flush_pending_input_activity(home: &Path) {
    for entry in take_pending_activity(home) {
        let values = activity_values(&entry);
        match agent_ops::try_save_metadata_batch(home, &entry.agent, &values) {
            agent_ops::TryMetadataBatchOutcome::Applied => {}
            agent_ops::TryMetadataBatchOutcome::Contended
            | agent_ops::TryMetadataBatchOutcome::Failed => {
                requeue_activity(entry);
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn pending_input_count_for(home: &Path) -> usize {
    PENDING_ACTIVITY
        .lock()
        .map(|pending| pending.iter().filter(|entry| entry.home == home).count())
        .unwrap_or(0)
}

/// Sprint 54 P2-3: record a submit-key keystroke (e.g. claude `\r`).
/// Caller (`app::write_to_focused`) is responsible for the backend
/// allowlist + submit-key match — this helper only records the timestamp in
/// the same pending pair as input activity. The daemon supervisor tick reads
/// it via `last_submit_at_ms` and compares against `last_input_at_ms` for the
/// typed-but-not-submitted detection.
pub fn record_submit_activity(home: &Path, agent_name: &str) {
    record_activity(home, agent_name, false);
}

/// Flush the remaining activity after the event loop has stopped. This is the
/// only blocking activity flush: normal and render-error teardown are no
/// longer latency-sensitive UI paths, so a final locked batch preserves the
/// last pair before the process exits.
pub fn flush_pending_activity_at_teardown(home: &Path) {
    for entry in take_pending_activity(home) {
        let values = activity_values(&entry);
        agent_ops::save_metadata_batch(home, &entry.agent, &values);
    }
}

/// Sprint 54 P2-3: read the last input/submit timestamps. Returns
/// `(typed_ms, submit_ms)` tuple; either component is `0` when missing
/// (legacy data, agent never typed, or non-submit-detected backend).
/// Used by the daemon supervisor tick for typed-but-not-submitted
/// detection — keeps the read inline-cheap (single file read, single
/// JSON parse) so per-tick overhead stays bounded.
pub fn read_input_submit_timestamps(home: &Path, agent_name: &str) -> (i64, i64) {
    // #1680: resolve via the SAME path resolver the write side uses
    // (`save_metadata` → `metadata_path_resolved`). The previous hand-coded
    // `metadata/<name>.json` read the never-written name file while the write
    // landed on `metadata/<uuid>.json`, so `draft_state` was permanently stale
    // (`None`) and the inject path force-submitted the operator's unsent draft.
    let meta_path = agent_ops::metadata_path_resolved(home, agent_name);
    let Ok(content) = std::fs::read_to_string(meta_path) else {
        return (0, 0);
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
        return (0, 0);
    };
    let typed_ms = value[COMPOSE_METADATA_KEY].as_i64().unwrap_or(0);
    let submit_ms = value[SUBMIT_METADATA_KEY].as_i64().unwrap_or(0);
    (typed_ms, submit_ms)
}

/// #1457: how long an unsent draft defers notification delivery before the
/// escape valve releases it (operator likely walked away mid-draft).
/// Fixed const 300s / 5 min (#env-cleanup: was env-overridable via
/// `AGEND_DRAFT_ESCAPE_SECS`; demoted to YAGNI for single-user deploys).
fn draft_escape_timeout_ms() -> i64 {
    const DRAFT_ESCAPE_MS: i64 = 300_000;
    DRAFT_ESCAPE_MS
}

/// #1457: draft state used to gate notification delivery. Derived from the
/// relative ORDER of the last input vs last submit keystroke (not a fixed idle
/// window) — fixes the `is_composing` false-negative where a >3s pause mid-draft
/// was misread as "no draft" and a notification clobbered the operator's input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftState {
    /// No unsent draft: everything typed has been submitted (or never typed).
    None,
    /// Unsent draft present and operator likely still composing → defer all.
    Drafting,
    /// Unsent draft present but idle past the escape window → trickle-release.
    Abandoned,
}

/// #1457: classify the focused pane's draft state for delivery gating.
/// `typed > submit` means keystrokes were entered but not submitted (a live
/// draft); `typed <= submit` (or never typed) means the buffer is clean.
pub fn draft_state(home: &Path, agent_name: &str) -> DraftState {
    let (typed_ms, submit_ms) = read_input_submit_timestamps(home, agent_name);
    if typed_ms == 0 || typed_ms <= submit_ms {
        return DraftState::None;
    }
    let now_ms = chrono::Utc::now().timestamp_millis();
    if now_ms.saturating_sub(typed_ms) < draft_escape_timeout_ms() {
        DraftState::Drafting
    } else if submit_ms == 0 {
        // #1473 (scoped fix): an unsent draft idle past the escape window with
        // NO submit ever recorded is not evidence of a real operator draft —
        // e.g. the operator poked an agent pane once but never composed a
        // message there. Treat as None so notifications deliver normally,
        // rather than trapping the pane in Abandoned forever. The Drafting
        // branch above (recent typing = active draft) is untouched, so #1457's
        // "don't clobber an in-progress draft" protection is preserved.
        // NOTE: scoped to the Abandoned branch ON PURPOSE — a naive top-level
        // `submit_ms == 0 → None` would mis-classify the operator's first-ever
        // draft (recent typing, no prior submit) and re-introduce the #1457 bug.
        DraftState::None
    } else {
        DraftState::Abandoned
    }
}

/// #1944: is the backend's input box (located by its prompt `marker` in the
/// rendered screen `tail`) actually EMPTY? This refines `draft_state`'s
/// timestamp-only heuristic, which reads a type-then-clear (typed then deleted
/// to empty, or typed-but-not-submitted) as a live `Drafting` for up to 5 min
/// even though the input line is visibly empty.
///
/// Returns:
/// - `Some(true)` — the input prompt is present and nothing non-whitespace
///   follows the marker → the box is empty (a stale draft; deliver normally).
/// - `Some(false)` — content follows the marker → a real live draft (protect).
/// - `None` — no prompt line found (agent mid-output, or a backend with no
///   marker) → the caller cannot tell, so it FAILS TOWARD PROTECTION (keep
///   deferring), never risking a clobber of a real draft.
///
/// Robustness: the input prompt is the BOTTOM-MOST line whose first non-blank
/// char is the marker (the input box sits at the screen bottom, below any
/// conversation/prose), so a `>`/`❯` appearing mid-prose above it is not matched
/// (the #1944 prose-false-positive guard).
pub fn input_box_is_empty(tail: &str, marker: &str) -> Option<bool> {
    let prompt_line = tail
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with(marker))?;
    let after_marker = prompt_line
        .trim_start()
        .strip_prefix(marker)
        .unwrap_or_default();
    Some(after_marker.trim().is_empty())
}

/// #1948 v2: unified per-backend empty-input-box probe. Tries the prompt-line
/// `marker` first (claude/codex/agy — content after the marker == non-empty);
/// if that can't decide (no prompt line found / no marker) AND a `placeholder`
/// is supplied (kiro), treats the placeholder being VISIBLE in the tail as the
/// box being empty (the TUI shows it only while empty). Returns:
/// - `Some(true)` — box verifiably empty (deliver),
/// - `Some(false)` — box has content (protect),
/// - `None` — undeterminable → caller FAILS TOWARD PROTECTION (keep deferring).
///
/// A backend has at most one of (marker, placeholder); the order is just a safe
/// precedence. Placeholder ABSENCE returns `None` (not `Some(false)`): it could
/// mean "typed" OR "agent mid-output / placeholder string changed across
/// versions" — either way fail toward protection (a placeholder-text change
/// disables the kiro path without ever risking a clobber of a real draft).
pub fn input_box_empty_probe(
    tail: &str,
    marker: Option<&str>,
    placeholder: Option<&str>,
) -> Option<bool> {
    if let Some(m) = marker {
        if let Some(empty) = input_box_is_empty(tail, m) {
            return Some(empty);
        }
    }
    if let Some(p) = placeholder {
        if tail.contains(p) {
            return Some(true);
        }
    }
    None
}

/// #1948(b): empty-box check for a backend (codex) whose EMPTY box renders a
/// rotating ghost/placeholder phrase after `marker` in the DIM attribute — which
/// a plain marker probe mis-reads as typed content (the v1 codex blind spot).
/// `text`/`dim` come from [`crate::vterm::VTerm::tail_lines_with_dim`] and are
/// 1:1 char-aligned. The marker's own glyph is excluded (codex renders `›` BOLD,
/// the ghost DIM; the operator's real input is normal intensity).
///
/// Returns `Some(true)` if the BOTTOM-MOST marker line has, after the marker,
/// only whitespace OR only DIM text (ghost → box empty); `Some(false)` if ANY
/// non-whitespace non-DIM glyph follows (a real live draft → protect); `None` if
/// no marker line is present (agent mid-output → fail toward protection).
pub fn input_box_dim_aware_empty(text: &str, dim: &[bool], marker: &str) -> Option<bool> {
    let chars: Vec<char> = text.chars().collect();
    // Find the BOTTOM-MOST line whose first non-blank char is the marker, and the
    // char range of its content AFTER the marker. Char indices align 1:1 with
    // `dim` (each `\n` contributes one char + one `dim` entry).
    let mut line_start = 0usize;
    let mut after_marker: Option<(usize, usize)> = None;
    for line in text.split('\n') {
        let line_len = line.chars().count();
        let line_end = line_start + line_len;
        if line.trim_start().starts_with(marker) {
            let leading_ws = line.chars().take_while(|c| c.is_whitespace()).count();
            let start = line_start + leading_ws + marker.chars().count();
            after_marker = Some((start, line_end));
        }
        line_start = line_end + 1; // +1 for the '\n' separator char
    }
    let (start, end) = after_marker?;
    for i in start..end {
        if chars.get(i).copied().unwrap_or(' ').is_whitespace() {
            continue;
        }
        if !dim.get(i).copied().unwrap_or(false) {
            return Some(false); // a normal-intensity glyph after the marker = real input
        }
    }
    Some(true) // only whitespace / only DIM ghost after the marker
}

/// #1457: pop and return the single OLDEST queued notification, leaving the
/// rest queued. The escape valve uses this so an abandoned-draft pane trickles
/// its backlog one-per-tick instead of clobbering the draft with a full batch.
/// Routes through the claim-atomic `drain` (then requeues the tail) so a
/// concurrent flusher can never read the same lines mid-rewrite.
pub fn drain_one(home: &Path, agent_name: &str) -> Option<QueuedNotification> {
    // #2028: drain_one is a SINGLE-SHOT caller (the Abandoned-trickle path and
    // tests) — unlike the flushers it has no "next tick" to absorb a transient
    // false-empty, so a lock/claim hiccup must be retried, not reported as
    // "queue empty". Bounded (no double-drain dead-wait regression): a live
    // peer holds the drain lock for microseconds (rename + read), so a few
    // short retries comfortably outlast any healthy contention window.
    const RETRIES: u32 = 5;
    const RETRY_SLEEP: std::time::Duration = std::time::Duration::from_millis(10);
    for attempt in 0..=RETRIES {
        match try_drain_with_stale_threshold(home, agent_name, STALE_DRAINING_MS) {
            DrainAttempt::Drained(mut all) => {
                if all.is_empty() {
                    return None; // TRUE empty — claim succeeded, nothing queued.
                }
                let oldest = all.remove(0);
                if !all.is_empty() {
                    requeue_all(home, agent_name, &all);
                }
                return Some(oldest);
            }
            DrainAttempt::Unavailable => {
                if attempt < RETRIES {
                    std::thread::sleep(RETRY_SLEEP);
                }
            }
        }
    }
    tracing::warn!(
        agent = agent_name,
        "#2028: drain_one gave up after {RETRIES} contended attempts — \
         deferring to the next trickle cycle (not claiming empty)"
    );
    None
}

/// Ambient enqueue with no channel provenance.
///
/// #3324: `#[cfg(test)]` deliberately. The production ambient defer path
/// (`route_notification`) now writes its row through
/// [`enqueue_classified_with_origin`], because the queued row is what the drain
/// later injects — a row that lost its origin comes back out classified as
/// internal, which is the permissive side of the ChannelBridge reply guard.
/// Keeping a provenance-free entry point available to production is how that
/// regression would return silently, so it is available to tests only.
#[cfg(test)]
pub fn enqueue(home: &Path, agent_name: &str, text: &str) -> anyhow::Result<()> {
    enqueue_classified(home, agent_name, text, false)
}

/// #1513: enqueue a notification tagged actionable/ambient. `deferred_since_ms`
/// is stamped now (first defer). Actionable items drain first and carry a
/// tighter MAX_DEFER cap; ambient retains the legacy contract.
pub fn enqueue_classified(
    home: &Path,
    agent_name: &str,
    text: &str,
    actionable: bool,
) -> anyhow::Result<()> {
    enqueue_classified_with_origin(home, agent_name, text, actionable, None)
}

/// #3324: same, carrying the typed external-channel provenance across the
/// durable defer hop. Internal callers keep using [`enqueue_classified`]; only
/// the inbound notification path has an origin to carry.
pub fn enqueue_classified_with_origin(
    home: &Path,
    agent_name: &str,
    text: &str,
    actionable: bool,
    channel_origin: Option<crate::channel::ChannelKind>,
) -> anyhow::Result<()> {
    let msg = QueuedNotification {
        text: text.to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        actionable,
        channel_origin,
        deferred_since_ms: chrono::Utc::now().timestamp_millis(),
    };
    append_queued(home, agent_name, &msg)
}

/// #1513: append a fully-formed `QueuedNotification` verbatim, preserving its
/// `actionable` + `deferred_since_ms` (used by `requeue_all` so the MAX_DEFER
/// cap counts from the ORIGINAL defer, not the requeue).
fn append_queued(home: &Path, agent_name: &str, msg: &QueuedNotification) -> anyhow::Result<()> {
    let path = queue_path(home, agent_name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{}", serde_json::to_string(msg)?)?;
    Ok(())
}

/// #t-3558 P2: the `[AGEND-AUTO kind=X]` coalesce key for `text`, or `None` when
/// `text` is not an auto-inject nudge. Two nudges coalesce iff they share this
/// EXACT prefix — so a different `kind=` (e.g. `progress-backstop` vs
/// `ratelimit-retry`) and any ordinary notification never match.
fn agend_auto_kind_prefix(text: &str) -> Option<String> {
    if !text.starts_with(crate::agent::DAEMON_AUTO_INJECT_MARKER) {
        return None;
    }
    let close = text.find(']')?;
    Some(text[..=close].to_string())
}

/// #t-3558 P2: enqueue an `[AGEND-AUTO]` auto-inject nudge, COALESCING it with any
/// already-queued nudge of the SAME `[AGEND-AUTO kind=X]` kind (keep-latest). A
/// non-draining agent otherwise accumulates a stack of identical retry nudges and
/// replays the whole pile on its next wake (the operator-visible noise this
/// fixes). ONLY same-kind AGEND-AUTO lines are dropped — a different `kind=` and
/// EVERY ordinary notification are preserved verbatim (byte-for-byte, including a
/// line that fails to parse).
///
/// No message loss: the read-modify-write is serialized against the drainer by
/// the SAME per-agent `drain.lock`, and snapshots the queue via the drainer's
/// atomic rename-claim — a lock-free [`append_queued`] racing in AFTER the claim
/// lands in a fresh queue file and is preserved by the re-append, never clobbered.
/// If the drain lock is held (a drainer is mid-delivery) the coalesce is SKIPPED
/// and we fall back to a plain append: skipping a round cannot lose a message (at
/// worst a transient duplicate the drainer is already consuming). On any IO error
/// after the claim, the claim file is LEFT for stale-recovery rather than removed
/// (re-delivered, never dropped).
pub fn enqueue_coalesced_auto(home: &Path, agent_name: &str, text: &str) -> anyhow::Result<()> {
    let new_msg = QueuedNotification {
        text: text.to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        actionable: false,
        // AGEND-AUTO nudges are daemon-generated, never channel-originated.
        channel_origin: None,
        deferred_since_ms: chrono::Utc::now().timestamp_millis(),
    };
    let Some(kind_key) = agend_auto_kind_prefix(text) else {
        // Not an AGEND-AUTO nudge (defensive — the caller only routes those
        // here): nothing to coalesce on, append unchanged.
        return append_queued(home, agent_name, &new_msg);
    };
    // Serialize vs the drainer; lock held → plain append (no coalesce, no loss).
    let Ok(Some(_lock)) = acquire_drain_lock(&drain_lock_path(home, agent_name)) else {
        return append_queued(home, agent_name, &new_msg);
    };
    let path = queue_path(home, agent_name);
    if path.exists() {
        let claim = draining_claim_path(home, agent_name);
        if std::fs::rename(&path, &claim).is_ok() {
            // Re-append every claimed RAW line EXCEPT same-kind AGEND-AUTO nudges.
            // Raw lines (not re-serialized structs) so a non-matching OR
            // unparseable line survives byte-for-byte. Remove the claim only
            // after a successful re-append; on IO error leave it for the
            // drainer's stale-claim recovery (re-delivered, never lost).
            if let Ok(content) = std::fs::read_to_string(&claim) {
                if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
                    for line in content.lines() {
                        let drop_it = serde_json::from_str::<QueuedNotification>(line)
                            .map(|m| m.text.starts_with(&kind_key))
                            .unwrap_or(false);
                        if !drop_it {
                            let _ = writeln!(f, "{line}");
                        }
                    }
                    let _ = std::fs::remove_file(&claim);
                }
            }
        }
        // rename failed (queue vanished mid-claim) → nothing to coalesce.
    }
    append_queued(home, agent_name, &new_msg)
    // `_lock` drops here → drain lock released.
}

pub fn pending_count(home: &Path, agent_name: &str) -> usize {
    let mut count = 0;
    let mut paths = list_draining_files(home, agent_name);
    paths.push(queue_path(home, agent_name));
    for path in paths {
        #[cfg(test)]
        test_hooks::note_content_read();
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        count += content.lines().count();
    }
    count
}

/// #2967/#2978/#2979: ONE `read_dir` of the shared `notification-queue/`
/// directory per pass. The per-agent accessors below apply the SAME
/// predicates the per-agent helpers (`queue_path` / `list_draining_files`)
/// always used, against this in-memory listing instead of re-enumerating the
/// shared directory once per agent — turning F `read_dir` calls per pass
/// (one per fleet agent) into one.
pub struct QueueDirSnapshot {
    /// The `notification-queue/` dir the snapshot was scanned from — content
    /// reads (`pending_count`) join file names back onto this.
    dir: PathBuf,
    /// `(file_name, byte_len)` for every entry directly under `dir` at scan
    /// time. File names only (not full paths) — every predicate below
    /// matches on the name, exactly like `list_draining_files`/`queue_path`
    /// always did.
    entries: Vec<(String, u64)>,
}

impl QueueDirSnapshot {
    /// ONE `read_dir`. Absent/unreadable directory → empty snapshot (the same
    /// fail-soft behavior `list_draining_files` already has).
    pub fn scan(home: &Path) -> Self {
        #[cfg(test)]
        test_hooks::note_dir_scan();
        let dir = home.join("notification-queue");
        let Ok(read) = std::fs::read_dir(&dir) else {
            return Self {
                dir,
                entries: Vec::new(),
            };
        };
        let entries = read
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let len = e.metadata().ok()?.len();
                Some((name, len))
            })
            .collect();
        Self { dir, entries }
    }

    /// This agent's queue file name (mirrors `queue_path`'s exact-match name).
    fn queue_file_name(agent_name: &str) -> String {
        format!("{agent_name}.jsonl")
    }

    /// This agent's draining-file prefix (mirrors `list_draining_files`).
    /// Deliberately does NOT match `<agent>.drain.lock` — see that fn's doc
    /// comment for why the lock file must never be counted as queue content.
    fn draining_prefix(agent_name: &str) -> String {
        format!("{agent_name}.draining")
    }

    fn agent_files(&self, agent_name: &str) -> impl Iterator<Item = &(String, u64)> {
        let queue_name = Self::queue_file_name(agent_name);
        let draining_prefix = Self::draining_prefix(agent_name);
        self.entries
            .iter()
            .filter(move |(name, _)| *name == queue_name || name.starts_with(&draining_prefix))
    }

    /// True iff this agent has any queue or draining file with non-zero
    /// length. Reads NO file contents.
    ///
    /// Correctness: for these files, `metadata_len > 0` ⟺
    /// `content.lines().count() >= 1` (an empty file yields 0 lines; any
    /// non-empty file yields ≥1 — `lines()` never returns 0 for non-empty
    /// content since a trailing newline doesn't add a phantom empty line and
    /// content with no newline is still one line). So gating on `has_pending`
    /// is EXACTLY equivalent to the current `pending_count(..) == 0` gate
    /// while reading zero bytes.
    pub fn has_pending(&self, agent_name: &str) -> bool {
        self.agent_files(agent_name).any(|(_, len)| *len > 0)
    }

    /// Exact line count across this agent's queue + draining files —
    /// reads contents, byte-for-byte the same arithmetic as `pending_count`.
    pub fn pending_count(&self, agent_name: &str) -> usize {
        self.agent_files(agent_name)
            .map(|(name, _)| {
                #[cfg(test)]
                test_hooks::note_content_read();
                std::fs::read_to_string(self.dir.join(name))
                    .map(|c| c.lines().count())
                    .unwrap_or(0)
            })
            .sum()
    }
}

/// A foreign draining file older than this is a crashed drainer's leftover and
/// gets recovered into the next drain. A healthy in-flight claim lives for
/// milliseconds (rename → read → inject), so 30s is comfortably past any live
/// window while still bounding how long a crash can strand its claimed lines.
const STALE_DRAINING_MS: u128 = 30_000;

/// Monotonic per-process suffix so every claim file is unique even within one
/// millisecond (two flushers in the same process, e.g. TUI loop + per-tick
/// handler in app-mode).
static CLAIM_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn draining_claim_path(home: &Path, agent_name: &str) -> PathBuf {
    let seq = CLAIM_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    queue_path(home, agent_name).with_extension(format!("draining-{}-{}", std::process::id(), seq))
}

/// Every draining file for `agent_name`, regardless of claim suffix. Also
/// matches the legacy fixed `<agent>.draining` name written by older binaries
/// (crash recovery must still pick those up after an upgrade).
fn list_draining_files(home: &Path, agent_name: &str) -> Vec<PathBuf> {
    #[cfg(test)]
    test_hooks::note_dir_scan();
    let dir = home.join("notification-queue");
    let prefix = format!("{agent_name}.draining");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().starts_with(prefix.as_str()))
                .unwrap_or(false)
        })
        .collect();
    out.sort();
    out
}

fn draining_file_is_stale(path: &Path, stale_ms: u128) -> bool {
    // Metadata anomalies (file vanished mid-scan, future mtime after a clock
    // step) are treated as STALE: this check only runs under the per-agent
    // drain lock, where no live peer can own the file — recovering it is
    // safe, while skipping would strand it forever and permanently inflate
    // `pending_count` (reviewer challenge 3, PR #1).
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|age| age.as_millis() >= stale_ms)
        .unwrap_or(true)
}

/// Per-agent drain mutex file. Sibling of the queue file; the name shares the
/// `<agent>.` prefix but NOT the `<agent>.draining` prefix, so neither
/// `list_draining_files` nor `pending_count` ever picks it up as queue content.
fn drain_lock_path(home: &Path, agent_name: &str) -> PathBuf {
    queue_path(home, agent_name).with_extension("drain.lock")
}

/// Acquire the per-agent drain lock. Production is a direct pass-through to
/// [`crate::store::try_acquire_file_lock`] — byte-identical.
///
/// In `#[cfg(test)]` builds it (a) honors a path-keyed `force_contention` seam
/// so a test can simulate a peer holding the lock with NO real thread or timing
/// window, and (b) RETRIES PAST a transient `Err` (an `EMFILE` hiccup from the
/// Coverage job's llvm-cov fd pressure — the lock is FREE, the `open()` just
/// failed) so an UNCONTENDED acquire is deterministic. `Ok(None)` (a real peer
/// holds the OS lock) is returned immediately, never retried, so contention
/// semantics are unchanged. The retry is BOUNDED: after `ACQUIRE_RETRIES` the
/// ORIGINAL (first) `Err` is PROPAGATED — not the last retry's — so a genuine,
/// non-transient open failure surfaces loudly and identically as the caller's
/// contended/`Unavailable` path (never an infinite hang). This is the root fix
/// for the #2028/#2072/#2074/#2333/#2383 flake family, where
/// `Err`-under-fd-pressure was misread as contention and broke strict-outcome
/// assertions.
fn acquire_drain_lock(lock_path: &Path) -> anyhow::Result<Option<crate::store::FileFlockGuard>> {
    #[cfg(not(test))]
    {
        crate::store::try_acquire_file_lock(lock_path)
    }
    #[cfg(test)]
    {
        test_hooks::note_acquire_attempt(lock_path);
        if test_hooks::force_contention(lock_path) {
            return Ok(None);
        }
        // Preserve the FIRST error: an `Ok(..)` (acquired or real contention)
        // returns immediately; otherwise retry transient `Err`s but keep the
        // original so IT is what propagates once the bound is exhausted — not
        // the last retry's error. `raw_acquire` also honors a forced-error seam
        // so a test can prove that identity across exhaustion.
        let first = test_hooks::raw_acquire(lock_path);
        if first.is_ok() {
            return first;
        }
        for _ in 0..test_hooks::ACQUIRE_RETRIES {
            std::thread::sleep(std::time::Duration::from_millis(2));
            let retry = test_hooks::raw_acquire(lock_path);
            if retry.is_ok() {
                return retry;
            }
        }
        first
    }
}

/// #2967/#2978/#2979 test seam: re-exported so callers outside this module
/// (`daemon::per_tick::notification_flush`, `app::mod` tests) can read the
/// per-process scan/read counters without reaching into `test_hooks`
/// directly.
#[cfg(test)]
pub(crate) use test_hooks::{content_read_count, dir_scan_count, reset_scan_counters};

/// Path-keyed test seams for the drain-lock acquire. Keyed by the lock PATH
/// (every test uses a unique `home` → unique path) so arming/counting for one
/// test never touches another running in parallel — no `serial` needed.
#[cfg(test)]
mod test_hooks {
    use parking_lot::Mutex;
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};

    // #2967/#2978/#2979 test seam: counts, on THIS THREAD, of every
    // queue-directory `read_dir` (`QueueDirSnapshot::scan` AND the
    // pre-existing `list_draining_files`) and every queue-FILE content read
    // (`QueueDirSnapshot::pending_count`, the module-level `pending_count`,
    // AND `read_drain_file`, which `drain` itself uses). Instrumenting all
    // the real call sites — not just the new snapshot type — is what makes
    // the RED tests measure the actual end-to-end syscall shape
    // `flush_all_with` produces, both pre- and post-fix.
    //
    // THREAD-LOCAL, matching the house seam pattern (`agent_ops.rs`'s
    // `METADATA_RMW_COUNT`, `dispatch_idle`'s `LIST_PENDING_CALLS`). A
    // process-global counter would be perturbed by any sibling test touching
    // the queue on another thread under the plain threaded `cargo test`
    // runner, which would have made these exact-count assertions depend on
    // the harness rather than on the code — and needing `#[serial]` to hide
    // that is the smell the pattern exists to avoid.
    //
    // Sound here because every assertion window is single-threaded: each
    // counter-asserting test resets, drives the real pass synchronously on
    // the test thread, and asserts on that same thread. The thread-spawning
    // tests in this module (concurrent-drain races) never read these
    // counters, so no increment is stranded on a worker thread.
    std::thread_local! {
        static DIR_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        static CONTENT_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    pub(super) fn note_dir_scan() {
        DIR_SCANS.with(|count| count.set(count.get() + 1));
    }
    pub(super) fn note_content_read() {
        CONTENT_READS.with(|count| count.set(count.get() + 1));
    }
    pub(crate) fn dir_scan_count() -> usize {
        DIR_SCANS.with(|count| count.get())
    }
    pub(crate) fn content_read_count() -> usize {
        CONTENT_READS.with(|count| count.get())
    }
    pub(crate) fn reset_scan_counters() {
        DIR_SCANS.with(|count| count.set(0));
        CONTENT_READS.with(|count| count.set(0));
    }

    /// Upper bound on retries past a transient `Err`; ~128ms at 2ms/step. Past
    /// this the original `Err` propagates (loud real-failure, never a hang).
    pub(super) const ACQUIRE_RETRIES: u32 = 64;

    /// Lock paths whose acquire must deterministically read as held (`Ok(None)`).
    static FORCED: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);
    /// Per-path count of acquire ATTEMPTS (one per `acquire_drain_lock` call,
    /// NOT per internal Err-retry) so a test asserts structural properties like
    /// "true-empty drained in exactly one attempt" without a wall-clock bound.
    static ATTEMPTS: Mutex<Option<HashMap<PathBuf, u64>>> = Mutex::new(None);

    pub(super) fn note_acquire_attempt(p: &Path) {
        *ATTEMPTS
            .lock()
            .get_or_insert_with(HashMap::new)
            .entry(p.to_path_buf())
            .or_insert(0) += 1;
    }
    pub(super) fn acquire_attempts(p: &Path) -> u64 {
        ATTEMPTS
            .lock()
            .as_ref()
            .and_then(|m| m.get(p).copied())
            .unwrap_or(0)
    }
    pub(super) fn reset_acquire_attempts(p: &Path) {
        if let Some(m) = ATTEMPTS.lock().as_mut() {
            m.remove(p);
        }
    }
    pub(super) fn force_contention(p: &Path) -> bool {
        FORCED.lock().as_ref().is_some_and(|s| s.contains(p))
    }
    pub(super) fn arm_contention(p: &Path) {
        FORCED
            .lock()
            .get_or_insert_with(HashSet::new)
            .insert(p.to_path_buf());
    }
    pub(super) fn clear_contention(p: &Path) {
        if let Some(s) = FORCED.lock().as_mut() {
            s.remove(p);
        }
    }

    /// Per-path forced-error mode. While armed, `raw_acquire` returns a DISTINCT
    /// `Err` on every call (message carries an incrementing ordinal) instead of
    /// touching the real lock, so a test can exhaust the retry bound and assert
    /// that the FIRST/original `Err` is the one propagated. Value = count so far.
    static FORCED_ERRORS: Mutex<Option<HashMap<PathBuf, u64>>> = Mutex::new(None);

    pub(super) fn arm_forced_errors(p: &Path) {
        FORCED_ERRORS
            .lock()
            .get_or_insert_with(HashMap::new)
            .insert(p.to_path_buf(), 0);
    }
    pub(super) fn clear_forced_errors(p: &Path) {
        if let Some(m) = FORCED_ERRORS.lock().as_mut() {
            m.remove(p);
        }
    }

    /// The single lock-acquire step used by `acquire_drain_lock`: the real OS
    /// lock, UNLESS forced-error mode is armed for `p` — then a distinct
    /// `Err("forced-acquire-err-{n}")` (n = 1, 2, 3…) so the retry-exhaustion
    /// path is exercised with identifiable errors. The `FORCED_ERRORS` guard is
    /// dropped before the real lock op.
    pub(super) fn raw_acquire(p: &Path) -> anyhow::Result<Option<crate::store::FileFlockGuard>> {
        {
            let mut g = FORCED_ERRORS.lock();
            if let Some(n) = g.as_mut().and_then(|m| m.get_mut(p)) {
                *n += 1;
                return Err(anyhow::anyhow!("forced-acquire-err-{n}"));
            }
        }
        crate::store::try_acquire_file_lock(p)
    }
}

/// Claim-exclusive drain. The TUI flush loop and the daemon's per-tick
/// `notification_flush` handler run in DIFFERENT processes and may drain the
/// same agent concurrently, so the whole critical section is serialized by a
/// per-agent OS file lock (`store::try_acquire_file_lock` — the #1629
/// FLOCK_DEPTH chokepoint):
///
/// 1. Try-lock on `<agent>.drain.lock` — held means a peer flusher is
///    draining this agent right now; walk away empty (the holder delivers,
///    and our caller retries next tick). The lock releases on drop, including
///    on crash (the OS releases file locks with the process).
/// 2. Inside the lock: a FRESH foreign draining file is a recently-crashed
///    peer's in-flight claim — leave it alone until the STALE window (≥30s)
///    passes, then crash-recover its lines. (A LIVE peer is excluded by the
///    lock, so any foreign claim file seen here belongs to a dead drainer.)
/// 3. The live queue is claimed by renaming it to a unique per-process claim
///    file, then read + removed.
///
/// Plain rename-arbitration without the lock double-delivered under CI
/// concurrency (windows-latest, run 27248027241): two racing renames of the
/// same source can interleave (open-source → set-rename-info), re-renaming
/// the winner's just-claimed file so BOTH drainers read the same lines. The
/// OS lock makes single-drainer-per-agent a structural invariant instead of
/// a rename race.
pub fn drain(home: &Path, agent_name: &str) -> Vec<QueuedNotification> {
    drain_with_stale_threshold(home, agent_name, STALE_DRAINING_MS)
}

/// #2028: outcome of one drain attempt. The flusher callers collapse
/// `Unavailable` to empty (they retry next tick — unchanged behavior);
/// single-shot callers (`drain_one`) retry instead of trusting a transient
/// hiccup as "queue empty".
pub(crate) enum DrainAttempt {
    Drained(Vec<QueuedNotification>),
    /// Could not claim: drain lock held/unopenable, or the queue file exists
    /// but the claim rename failed. "Not sure" — NEVER "empty".
    Unavailable,
}

/// `stale_ms` injected for deterministic tests (0 = recover any leftover now).
/// Flusher-facing wrapper: `Unavailable` collapses to empty (the holder
/// delivers; this caller retries next tick).
pub(crate) fn drain_with_stale_threshold(
    home: &Path,
    agent_name: &str,
    stale_ms: u128,
) -> Vec<QueuedNotification> {
    match try_drain_with_stale_threshold(home, agent_name, stale_ms) {
        DrainAttempt::Drained(v) => v,
        DrainAttempt::Unavailable => Vec::new(),
    }
}

/// #1629: routed through the store chokepoint so the flock bumps
/// FLOCK_DEPTH for the self-IPC deadlock guard. Non-blocking on purpose:
/// a held lock means a live peer flusher is mid-delivery — we must not
/// dead-wait on it. #2028 made the non-blocking outcome HONEST: lock
/// unavailable (or unopenable, or a failed claim rename over an existing
/// queue) is `Unavailable`, not an empty vec — under llvm-cov-grade load a
/// transient open/lock hiccup was reported as "queue empty" and single-shot
/// callers believed it. No inject/self-IPC happens while the guard is held:
/// drain only touches files and returns; injection runs after the guard
/// drops.
pub(crate) fn try_drain_with_stale_threshold(
    home: &Path,
    agent_name: &str,
    stale_ms: u128,
) -> DrainAttempt {
    let Ok(Some(_drain_lock)) = acquire_drain_lock(&drain_lock_path(home, agent_name)) else {
        return DrainAttempt::Unavailable;
    };
    let mut out = Vec::new();
    for leftover in list_draining_files(home, agent_name) {
        if draining_file_is_stale(&leftover, stale_ms) {
            out.extend(read_drain_file(&leftover));
        }
    }
    let path = queue_path(home, agent_name);
    if path.exists() {
        let claim = draining_claim_path(home, agent_name);
        match std::fs::rename(&path, &claim) {
            Ok(()) => out.extend(read_drain_file(&claim)),
            Err(_) if path.exists() => {
                // The queue is RIGHT THERE but we couldn't claim it. Stale
                // leftovers (if any) were already consumed above and must be
                // DELIVERED, not dropped — so this is Unavailable only when
                // we'd otherwise return a false "empty".
                if out.is_empty() {
                    return DrainAttempt::Unavailable;
                }
            }
            Err(_) => {} // queue vanished mid-claim — genuinely nothing left for us
        }
    }
    // `_drain_lock` drops here → OS lock released + FLOCK_DEPTH decremented.
    DrainAttempt::Drained(out)
}

pub fn requeue_all(home: &Path, agent_name: &str, notifications: &[QueuedNotification]) {
    for notification in notifications {
        // #1513: preserve actionable + deferred_since_ms verbatim so the
        // MAX_DEFER cap keeps counting from the original defer.
        // #2028: a swallowed append here is MESSAGE LOSS (the items were
        // already claimed out of the queue) — surface it loudly; the next
        // drain honestly reports the smaller queue either way.
        if let Err(e) = append_queued(home, agent_name, notification) {
            tracing::error!(
                agent = agent_name,
                error = %e,
                text = %notification.text.chars().take(80).collect::<String>(),
                "notification requeue FAILED — this queued message is LOST"
            );
        }
    }
}

fn read_drain_file(path: &Path) -> Vec<QueuedNotification> {
    #[cfg(test)]
    test_hooks::note_content_read();
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let notifications = content
        .lines()
        .filter_map(|line| serde_json::from_str::<QueuedNotification>(line).ok())
        .collect::<Vec<_>>();
    let _ = std::fs::remove_file(path);
    notifications
}

#[cfg(test)]
mod tests;
