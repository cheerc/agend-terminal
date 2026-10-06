#!/usr/bin/env bash
# #1490 — one-shot CI-parity preflight.
#
# Mirrors the GitHub Actions `check` job (.github/workflows/ci.yml) locally so
# the local-green -> CI-red round-trip stops costing a push + a ~10min wait.
# Born from the retro HIGH point: a single session burned 5 CI-red cycles on
# problems a local check would have caught — Windows-only compile errors,
# fmt drift, tray-gated failures, and the 750-LOC file_size_invariant.
#
# Runs, in order — the fmt/clippy surface MATCHES CI's `check` job (task83/d-46):
#   1. scripts/fmt-owned.sh --check   (tracked + untracked/non-ignored owned *.rs,
#                                      vendor/ excluded — CI's exact surface)
#   2. cargo clippy <owned targets + agentic-git wrapper> --features tray -- -D warnings  (CI's exact targets)
#   3. cargo nextest run --features tray   (unit + integration + invariants)
#      This is CI's runner AND selection (unit tests + every tests/*.rs target).
#      If cargo-nextest is missing, the test step FAILS with an install hint;
#      preflight does NOT substitute `cargo test --tests` because its bulk verdict
#      is flaky and indistinguishable from a real regression. A floating stable
#      toolchain is not byte-exact over time either. "Green here" is a strong
#      pre-check, not a byte-exact guarantee of CI.
#   4. Windows cross-check (x86_64-pc-windows-msvc)   <- the keystone
#
# Step 4 catches the class that hurts most: Windows-only code
# (libc::getppid, /bin/sh spawns, UnixStream) compiles fine on a unix dev
# box but breaks CI's windows-latest runner. There is a wrinkle — a plain
# `cargo check --target x86_64-pc-windows-msvc` cannot complete on a stock
# macOS/Linux box because a transitive C dependency (`ring`, via TLS) needs
# the Windows C toolchain (`assert.h` et al.) and its build script aborts
# before our crate is ever type-checked. So this step prefers `cargo xwin`
# (bundles the MSVC CRT/SDK) and degrades gracefully:
#   - `cargo-xwin` installed  -> `cargo xwin check` (real, complete check)
#   - else plain `cargo check` -> works only if the host has a Windows
#     toolchain; on a C-dep build-script failure it SKIPS with a hint
#     rather than reporting a false failure.
#   - target not installed     -> SKIP + `rustup target add` hint.
#
# Usage:
#   scripts/preflight.sh            # full matrix (default)
#   scripts/preflight.sh --quick    # skip the Windows cross-check (host only)
#   scripts/preflight.sh -h|--help
#
# Exit codes:
#   0  — every step that ran passed (a skipped Windows step is still 0)
#   1  — at least one step failed
#   2  — invocation / environment error (cargo missing, bad arg)
#
# NOT a git hook: the full matrix takes minutes; wiring it into pre-push
# would tax every push. Run it manually before pushing (see CLAUDE.md).
# CI stays the source of truth — this just front-runs it.

set -uo pipefail

readonly SCRIPT_NAME="${0##*/}"
readonly WINDOWS_TARGET="x86_64-pc-windows-msvc"

run_quick="false"
while (( $# > 0 )); do
    case "$1" in
        --quick) run_quick="true"; shift ;;
        -h|--help)
            sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "[$SCRIPT_NAME] unknown arg: $1" >&2
            exit 2
            ;;
    esac
done

if ! command -v cargo >/dev/null 2>&1; then
    echo "[$SCRIPT_NAME] cargo not found in PATH" >&2
    echo "  install: https://rustup.rs/" >&2
    exit 2
fi

# Run from repo root so cargo finds the manifest regardless of CWD.
cd "$(dirname "$0")/.." || exit 2

# Daemon-managed agent shells: tests that scope AGEND_HOME to a temp dir and
# then spawn `git` reach the agentic-git shim on PATH. The shim derives its own
# directory from AGENTIC_GIT_HOME (legacy fallback: AGEND_HOME) to exclude
# itself from the PATH it hands the real git; under the scoped value it resolves
# git to itself and trips its recursion guard (#1504) — ~40 deterministic
# failures in instructions / usage_limit_takeover / agent_resolve that CI (no
# shim on PATH) never sees. Pinning the PRIMARY name to the real home keeps the
# shim fully in force (same agent identity, same policy) while the tests scope
# only the crate's own home. No-op outside a daemon-managed shell.
if [[ -z "${AGENTIC_GIT_HOME:-}" && -n "${AGEND_HOME:-}" ]]; then
    export AGENTIC_GIT_HOME="$AGEND_HOME"
fi

# ── #2: real-git starting line ────────────────────────────────────────────
# The pin above is only HALF the starting line. It makes the shim's
# self-exclusion work, but it does not guarantee the shim has a real git to
# exec in the environment the tests build. When BOTH of the shim's resolution
# defences are down — Priority 1 unusable (the daemon's spawn-time
# AGEND_REAL_GIT was computed from a different PATH, or the canonical
# AGENTIC_GIT_REAL_GIT is absent / stale / points at the shim) and Priority 2
# missed (self-exclusion reads AGENTIC_GIT_HOME, so a test that scopes
# AGEND_HOME to a temp dir moves the exclusion off the shim's real dir) — the
# shim resolves `git` to ITSELF and trips its recursion guard (#1504), turning
# deterministic tests into false reds that mask real failures.
#
# So make sure the shim has one: derive it, or promote a still-good injected
# value, and hand it over. Whether that is NEEDED is decided by what `git`
# actually RESOLVES TO on PATH, not by whether some env var happens to be set —
# that keeps the boundary derivable from the program alone:
#
#   * first `git` on PATH is the agentic-git/agend-git shim → managed agent
#     shell → the shim is live and NEEDS a real git to hand off to. (Detecting
#     the shim, not "is AGEND_HOME set", also covers a shell where the shim is
#     on PATH but AGEND_HOME was never exported — the pin above cannot fire
#     there, which is exactly the shape the recursion guard then bites on.)
#   * first `git` on PATH is a real git → plain shell → do NOTHING. No export,
#     no PATH rewrite, no policy change; behaviour is byte-identical to a run
#     without this block.
#
# Gate on the CANONICAL AGENTIC_GIT_REAL_GIT, not on the legacy AGEND_REAL_GIT:
# the shim reads the canonical name first and only falls back to the legacy one
# inside `env_compat` (vendor/.../src/lib.rs), so a guard that treats a set
# AGEND_REAL_GIT as "already handled" would skip this block in exactly the
# standard daemon-managed shell it exists to serve — where the daemon always
# injects that legacy name at spawn.
#
# Env-only by construction: we export a variable, never rewrite PATH and never
# touch AGENTIC_GIT_HOME's meaning, so the shim keeps enforcing the same policy
# for the same agent identity. (Do NOT "fix" this by sourcing the fixture-only
# shell seam under scripts/lib/ — that helper PREPENDS the real git's directory
# onto PATH, which would change path resolution outside a managed shell, and it
# is fixture-only by contract; see tests/fixture_real_git_provenance.rs.)

# Physical path of an existing file/dir, following symlinks (bash 3.2 has no
# `readlink -f`; realpath(1) exists on macOS 13+ and on CI's linux runners).
pf_canon() {
    if [ -e "$1" ]; then
        realpath "$1" 2>/dev/null && return 0
    fi
    local d b
    d="$(cd "$(dirname "$1")" 2>/dev/null && pwd -P)" || return 1
    b="$(basename "$1")"
    printf '%s\n' "$d/$b"
}

# True when $1 (a file) sits in a known shim install dir ($AGEND_HOME/bin).
pf_in_shim_dir() {
    local fd h sd
    fd="$(cd "$(dirname "$1")" 2>/dev/null && pwd -P)" || return 1
    for h in "${AGENTIC_GIT_HOME:-}" "${AGEND_HOME:-}"; do
        [ -n "$h" ] || continue
        sd="$(cd "$h/bin" 2>/dev/null && pwd -P)" || continue
        [ "$fd" = "$sd" ] && return 0
    done
    return 1
}

# True when the first `git` on PATH is the agentic-git / agend-git shim.
pf_first_git_is_shim() {
    local g canon base
    g="$(command -v git 2>/dev/null)" || return 1
    [ -n "$g" ] || return 1
    canon="$(pf_canon "$g")" || return 1
    base="${canon##*/}"
    case "$base" in
        agentic-git | agentic-git.exe | agend-git | agend-git.exe) return 0 ;;
    esac
    pf_in_shim_dir "$canon"
}

# First PATH git that is neither the shim nor in a shim dir, proven to answer
# `git version`.
pf_derive_real_git() {
    local first first_canon entry cand cc oldifs
    first="$(command -v git 2>/dev/null)" || return 1
    first_canon="$(pf_canon "$first")" || return 1
    oldifs="$IFS"
    IFS=:
    for entry in $PATH; do
        IFS="$oldifs"
        if [ -n "$entry" ]; then
            cand="$entry/git"
            if [ -x "$cand" ]; then
                if cc="$(pf_canon "$cand")" && [ "$cc" != "$first_canon" ] &&
                    ! pf_in_shim_dir "$cc" &&
                    "$cc" version 2>/dev/null | grep -q '^git version'; then
                    IFS="$oldifs"
                    printf '%s\n' "$cc"
                    return 0
                fi
            fi
        fi
        IFS=:
    done
    IFS="$oldifs"
    return 1
}

# True when $1 is usable AS the shim's real git right now: an absolute path to
# an existing file that is not the shim itself. The shim applies the same test
# at exec.rs:86-90 (`exists` + `points_at_self`) — being no looser than the
# consumer is the floor.
#
# The daemon injects AGEND_REAL_GIT at SPAWN time from the PATH of that moment;
# the shim reaches it only through the legacy fallback inside `env_compat`. So
# its mere presence does not mean it still fits the environment the tests build
# — hence judge the VALUE, not the variable's existence.
pf_real_git_usable() {
    local v="$1" canon shim_canon
    [ -n "$v" ] || return 1
    case "$v" in
        /* | [A-Za-z]:[\\/]*) ;; # absolute (POSIX, or a Windows drive path)
        *) return 1 ;;
    esac
    [ -f "$v" ] || return 1
    canon="$(pf_canon "$v")" || return 1
    [ -f "$canon" ] || return 1
    # Never treat the shim as its own real git — that is the loop #1504 contains.
    if shim_canon="$(pf_canon "$(command -v git 2>/dev/null)" 2>/dev/null)"; then
        [ "$canon" != "$shim_canon" ] || return 1
    fi
    pf_in_shim_dir "$canon" && return 1
    return 0
}

# Re-pin whenever the CANONICAL variable is absent or no longer usable.
if ! pf_real_git_usable "${AGENTIC_GIT_REAL_GIT:-}"; then
    if pf_first_git_is_shim; then
        if pf_real="$(pf_derive_real_git)"; then
            export AGENTIC_GIT_REAL_GIT="$pf_real"
            echo "[$SCRIPT_NAME] agent shell: pinned AGENTIC_GIT_REAL_GIT=$pf_real (shim at $(command -v git)) — #1504" >&2
        elif pf_real_git_usable "${AGEND_REAL_GIT:-}"; then
            # The daemon-injected legacy value is still good here; promoting it to
            # the canonical name is what the shim reads first.
            export AGENTIC_GIT_REAL_GIT="$AGEND_REAL_GIT"
            echo "[$SCRIPT_NAME] agent shell: promoted AGEND_REAL_GIT=$AGEND_REAL_GIT to AGENTIC_GIT_REAL_GIT (shim at $(command -v git)) — #1504" >&2
        else
            # Acceptance: an unresolvable real git must be NAMED, not turned
            # into a second wave of false reds. Say what broke, how to confirm
            # it, and how to fix it, then keep going — the tests themselves
            # still decide the verdict.
            echo "[$SCRIPT_NAME] WARNING: the agentic-git shim is first on PATH but no usable real git could be resolved." >&2
            echo "  Symptom: if the test phase reports 'FATAL recursion guard tripped (AGENTIC_GIT_SHIM_DEPTH=3)' (#1504)," >&2
            echo "           those reds are shim self-resolution, NOT your change." >&2
            echo "  Fix:     export AGENTIC_GIT_REAL_GIT=\"\$(command -v git)\" — any real git that is NOT the shim," >&2
            echo "           e.g. /usr/bin/git. A stale, relative, or shim-pointing value is ignored here" >&2
            echo "           for the same reason the shim ignores it." >&2
        fi
    fi
    unset pf_real
fi

passed=()
failed=()
skipped=()

banner() {
    echo
    echo "──────────────────────────────────────────────────────────────"
    echo "[$SCRIPT_NAME] $1"
    echo "──────────────────────────────────────────────────────────────"
}

# #13 — tracked worktree content must match what HEAD records, so the local
# checks cannot pass on content CI will not receive. See the call site for the
# full rationale and the exact shapes covered.
pf_check_worktree_matches_head() {
    # Read-only; needs no cargo, and the shim passes `diff`/`diff-index` through.
    local staged unstaged
    staged="$(git diff --cached --name-only --no-renames 2>/dev/null)"
    unstaged="$(git diff --name-only --no-renames 2>/dev/null)"

    if [[ -z "$staged" && -z "$unstaged" ]]; then
        return 0
    fi

    if [[ -n "$unstaged" ]]; then
        echo
        echo "[$SCRIPT_NAME] These TRACKED files differ from the index (edited but not staged):"
        while IFS= read -r f; do
            [[ -n "$f" ]] && echo "    $f"
        done <<<"$unstaged"
    fi
    if [[ -n "$staged" ]]; then
        echo
        echo "[$SCRIPT_NAME] These files are staged but NOT committed (index is ahead of HEAD):"
        while IFS= read -r f; do
            [[ -n "$f" ]] && echo "    $f"
        done <<<"$staged"
    fi
    if [[ -n "$unstaged" && -n "$staged" ]]; then
        cat >&2 <<EOF

  A file can appear in both lists when it was staged and then edited again —
  that is the #13 accident shape: the COMMIT holds the staged version while the
  worktree holds the newer one, so local checks (which read the worktree) pass
  while CI (which reads the commit) fails.
EOF
    fi

    cat >&2 <<EOF

  Every later step in this run reads the WORKTREE, so a pass below is a pass on
  content that is not what will be pushed. Untracked files are deliberately NOT
  reported here — they are a legitimate part of iterating in this repo.

  Fix: commit the current content (git add <files> && git commit), or restore the
  files you did not mean to change (git restore <files>) before re-running.
  Stashing (git stash push) also clears the staged/unstaged state.
EOF
    return 1
}

# step "<label>" cmd args...   — runs cmd, records pass/fail, never aborts the
# script (run-all so the dev sees every problem in one pass, not one at a time).
step() {
    local label="$1"; shift
    banner "$label"
    echo "  \$ $*"
    if "$@"; then
        passed+=("$label")
    else
        failed+=("$label")
    fi
}

untracked_rs_found="false"
while IFS= read -r -d '' _; do
    untracked_rs_found="true"
    break
done < <(git ls-files -z --others --exclude-standard -- '*.rs' ':!:vendor/**')
if [[ "$untracked_rs_found" == "true" ]]; then
    echo "[$SCRIPT_NAME] note: untracked, non-ignored *.rs files are included in the fmt check" >&2
fi

# ── #13: tracked files must match HEAD ─────────────────────────────────────
# The #13 accident: an agent edits the worktree, runs `git add`, edits the SAME
# file AGAIN, and only then commits. The commit carries the intermediate
# version; the worktree holds the final one. Every local check below reads the
# WORKTREE and passes, while CI reads the COMMIT and fails — and neither
# `fmt --check` (the file need not be owned Rust) nor `git diff-tree` (the
# committed content is byte-identical to the pre-edit stage) can see it.
#
# Scope is deliberately TRACKED files only. `git status --porcelain
# --untracked-files=normal` would also flag every untracked scratch file an agent
# creates while iterating — but untracked files are legitimate here by the repo's
# own convention: `scripts/fmt-owned.sh` defines its owned surface as "tracked
# PLUS untracked/non-ignored *.rs", and the untracked-*.rs note above prints
# rather than blocks. Requiring a fully clean worktree would contradict that, so
# this gate uses `git diff` (worktree vs INDEX) plus `git diff --cached` (index vs
# HEAD) and ignores untracked paths entirely.
#
# Those two together cover every shape where what a later step reads differs from
# what HEAD records:
#   * `MM` (staged AND further edited) — the #13 accident itself; the unstaged
#     half is what the commit silently dropped.
#   * ` M` (tracked edit, never staged) — the commit does not contain the edit at
#     all, and local checks are validating something CI will never see.
#   * `M ` / `A ` (staged, not committed) — the index is ahead of HEAD, so this
#     run is validating an uncommitted state.
# It does NOT cover untracked files (above), deletions the agent intends to
# commit later, or a worktree that is clean here but whose HEAD differs from
# origin — none of which make local checks validate a state CI will disagree
# with.
#
# Placement: this runs FIRST among the checks, so a tracked-file divergence is
# reported before the cargo steps rather than buried among them. Note that it
# does NOT short-circuit the run: `step()` is run-all by design (so one pass
# shows every problem), so the clippy and nextest steps still execute after this
# one fails. Reporting the divergence early is the point; skipping the expensive
# steps on it would need an abort-on-failure path that deliberately departs from
# that run-all contract.
#
# Coverage is "when the agent RUNS preflight", not "automatically on push". In a
# daemon-managed worktree `core.hooksPath` points at `$AGEND_HOME/hooks`, where
# the active hooks are the daemon's own (CLAUDE.md, "Which hooks actually fire"),
# so `scripts/hooks/pre-push` is not the active hook there and this gate is not
# reached on push. Even in the `scripts/hooks` regime that hook only runs its
# CI-parity block when the push touches `src/ tests/ Cargo.toml Cargo.lock
# build.rs`, so a scripts-only change like this one would not trigger it either.
# That is consistent with preflight being a manual gate by design — CLAUDE.md
# says so explicitly — and an agent iterating on a worktree does run it, which
# is the case #13 is about.
step "worktree matches HEAD (tracked files)" \
    pf_check_worktree_matches_head

step "fmt --check (owned surface)" \
    scripts/fmt-owned.sh --check
step "clippy (owned targets --features tray -D warnings)" \
    cargo clippy --lib --bin agend-terminal --bin agend-git --bin agend-mcp-bridge --bin agentic-git --tests --examples --features tray -- -D warnings
if cargo nextest --version >/dev/null 2>&1; then
    step "test (nextest --features tray: unit + integration + invariants — CI's runner)" \
        cargo nextest run --features tray
else
    cargo_nextest_missing() {
        echo "[$SCRIPT_NAME] ERROR: required test runner cargo-nextest is not installed; the test suite was NOT run." >&2
        echo "  install: cargo install cargo-nextest --locked" >&2
        return 1
    }
    step "test (FAIL: runner missing — cargo-nextest required)" \
        cargo_nextest_missing
fi

# ── Step 4: Windows cross-check ──────────────────────────────────────────
windows_check() {
    if [[ "$run_quick" == "true" ]]; then
        skipped+=("windows check ($WINDOWS_TARGET) — --quick")
        return
    fi

    if command -v rustup >/dev/null 2>&1 \
        && ! rustup target list --installed 2>/dev/null | grep -qx "$WINDOWS_TARGET"; then
        banner "windows check ($WINDOWS_TARGET) — target not installed, SKIP"
        echo "  enable (catches windows-only compile errors locally):"
        echo "      rustup target add $WINDOWS_TARGET"
        skipped+=("windows check ($WINDOWS_TARGET) — target not installed")
        return
    fi

    # Preferred path: cargo-xwin bundles the MSVC CRT/SDK so the `ring` C
    # build script (and every other Windows compile) succeeds on a unix host.
    if command -v cargo-xwin >/dev/null 2>&1; then
        step "windows check (cargo xwin check --target $WINDOWS_TARGET --all-targets --features tray)" \
            cargo xwin check --target "$WINDOWS_TARGET" --all-targets --features tray
        return
    fi

    # Fallback: plain cargo check. Works only when the host already has a
    # Windows C toolchain. On a C-dep build-script failure (the `ring`
    # assert.h wall) we SKIP with a hint instead of false-failing.
    banner "windows check (cargo check --target $WINDOWS_TARGET --all-targets --features tray)"
    local log
    log="$(mktemp -t "preflight-win.XXXXXX")"
    if cargo check --target "$WINDOWS_TARGET" --all-targets --features tray 2>&1 | tee "$log"; then
        passed+=("windows check ($WINDOWS_TARGET)")
        rm -f "$log"
        return
    fi

    if grep -qE "(error occurred in cc-rs|failed to run custom build command|fatal error: '.*\.h' file not found|linker .* not found)" "$log"; then
        echo
        echo "[$SCRIPT_NAME] windows check blocked by a C-dependency build script"
        echo "  (e.g. 'ring' needs the Windows C toolchain) — this is NOT your code."
        echo "  for a real local Windows check on unix, install cargo-xwin:"
        echo "      cargo install cargo-xwin && rustup target add $WINDOWS_TARGET"
        echo "  (otherwise CI's windows-latest runner is the backstop.)"
        skipped+=("windows check ($WINDOWS_TARGET) — C-dep toolchain missing; install cargo-xwin")
        rm -f "$log"
        return
    fi

    failed+=("windows check ($WINDOWS_TARGET)")
    rm -f "$log"
}
windows_check

# ── Summary ──────────────────────────────────────────────────────────────
echo
echo "══════════════════════════════════════════════════════════════"
echo "[$SCRIPT_NAME] Summary"
echo "══════════════════════════════════════════════════════════════"
echo "  passed  (${#passed[@]}): ${passed[*]:-<none>}"
echo "  failed  (${#failed[@]}): ${failed[*]:-<none>}"
if (( ${#skipped[@]} > 0 )); then
    echo "  skipped (${#skipped[@]}):"
    for s in "${skipped[@]}"; do
        echo "    - $s"
    done
fi
echo

if (( ${#failed[@]} > 0 )); then
    echo "[$SCRIPT_NAME] PREFLIGHT FAILED — fix the above before pushing." >&2
    exit 1
fi

echo "[$SCRIPT_NAME] OK — local CI matrix clean. Safe to push."
exit 0
