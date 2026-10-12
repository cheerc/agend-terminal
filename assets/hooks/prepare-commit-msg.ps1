# agend-terminal prepare-commit-msg hook (PowerShell) — injects fleet trailers.
# Windows equivalent of the bash hook.

param($CommitMsgFile, $CommitSource)

# Skip merge/squash/template commits.
if ($CommitSource -in @("merge", "squash", "template")) { exit 0 }

$Agent = $env:AGEND_INSTANCE_NAME
if (-not $Agent) { exit 0 }

$HomeDir = $env:AGEND_HOME
if (-not $HomeDir) { exit 0 }

$Binding = Join-Path $HomeDir "runtime" $Agent "binding.json"
if (-not (Test-Path $Binding)) { exit 0 }

# Idempotent: skip if trailer already present.
$Content = Get-Content $CommitMsgFile -Raw -ErrorAction SilentlyContinue
if ($Content -match "(?m)^Agend-Agent:") { exit 0 }

# Parse binding.json.
try {
    $Json = Get-Content $Binding -Raw | ConvertFrom-Json
    $Task = $Json.task_id
    $Branch = $Json.branch
    $Issued = $Json.issued_at
} catch { exit 0 }

# #3545: same rule as the bash hook — the binding names the BOUND branch, which
# a `bind:false` dispatch leaves pointing at the previous task. Read the branch
# actually being committed on from $GIT_DIR/HEAD (never `git rev-parse`, which
# the shim redirects to the bound worktree, #2234/#2481), and withhold the
# branch-derived trailers when it cannot be confirmed to match.
#
# `-cmatch` / `-cne`, not `-match` / `-ne`: PowerShell's comparison operators
# are CASE-INSENSITIVE by default, but git branch names are case-sensitive and
# the bash hook compares bytes. Without the `c` prefix `feature/X` and
# `feature/x` would compare equal here and this hook would write the wrong
# branch on Windows while the bash one withheld it — the two must agree.
#
# NOTE on reach: git sets `GIT_DIR` for hooks in a LINKED worktree (which is
# what the daemon binds an agent to), but NOT for a commit in a repository's
# main worktree — there the variable is unset and the branch-derived trailers
# are withheld. That is the fail-closed side and it is intended.
$Actual = ""
if ($env:GIT_DIR) {
    $HeadFile = Join-Path $env:GIT_DIR "HEAD"
    if (Test-Path $HeadFile) {
        $HeadRef = Get-Content $HeadFile -Raw -ErrorAction SilentlyContinue
        if ($HeadRef -cmatch "^ref: refs/heads/(.+?)\s*$") { $Actual = $Matches[1] }
    }
}
if ((-not $Actual) -or ($Actual -cne $Branch)) {
    # #20 step 1 (R1): measure a PROVABLE named mismatch only — both sides
    # non-empty and byte-unequal (-cne). Detached / unknown / binding-empty
    # arrive here with an empty side and must NOT record. Best-effort:
    # helper missing or failing never changes the commit outcome. The guard
    # uses the case-sensitive -cne semantics of the outer comparison (branch
    # names are case-sensitive; a plain -and truthiness probe would treat
    # `feature/X` vs `feature/x` as needing no observer distinction).
    if (($Branch -cne "") -and ($Actual -cne "")) {
        $Observer = Join-Path $HomeDir "bin" "agend-hook-observer.exe"
        if (Test-Path $Observer) {
            & $Observer --home $HomeDir --agent $Agent `
                --task $Task --bound-branch $Branch `
                --actual-branch $Actual --head-raw $HeadRef `
                --commit-source $CommitSource 2>$null | Out-Null
        }
    }
    $Task = ""
    $Branch = ""
    $Issued = ""
}

# Append trailers.
$Trailers = "`n`nAgend-Agent: $Agent"
if ($Task) { $Trailers += "`nAgend-Task: $Task" }
if ($Branch) { $Trailers += "`nAgend-Branch: $Branch" }
if ($Issued) { $Trailers += "`nAgend-Issued-At: $Issued" }

Add-Content -Path $CommitMsgFile -Value $Trailers
exit 0
