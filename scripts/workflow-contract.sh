#!/usr/bin/env bash
# Workflow contract — asserts the canonical CI gate keeps its shape.
# 𝚅𝚒𝚋𝚎𝚌𝚛𝚊𝚏𝚝𝚎𝚍. with AI Agents by VetCoders (c)2024-2026 LibraxisAI
#
# Invariants guarded here (W0-A):
#   1. rust.yml and e2e.yml run once for main-bound delivery:
#      direct pushes to main and pull requests targeting main. Develop pushes
#      must not duplicate the pull_request run.
#   2. Every job in those workflows carries an explicit timeout-minutes.
#   3. Every plugin-building job installs the wasm32-wasip1 target.
#   4. The canonical test job invokes the repo-native gate (make ci),
#      so the local gate and the remote gate mean the same thing.
#   5. Superseded runs are cancelled per workflow/ref or pull request.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
rust_yml="$repo_root/.github/workflows/rust.yml"
e2e_yml="$repo_root/.github/workflows/e2e.yml"

fail=0
err() { echo "✗ $1" >&2; fail=1; }
ok() { echo "✓ $1"; }

for wf in "$rust_yml" "$e2e_yml"; do
    name="$(basename "$wf")"
    [ -f "$wf" ] || { err "$name: workflow file missing"; continue; }

    # 1. Delivery triggers: push and pull_request each target main only.
    #    Running again on every develop push duplicates the main-bound PR run
    #    and wastes the scarce self-hosted runner fleet.
    branch_lines=0
    while IFS= read -r line; do
        branch_lines=$((branch_lines + 1))
        case "$line" in
            *"branches: [main]"*) : ;;
            *) err "$name: branch trigger must target main only: $line" ;;
        esac
    done < <(grep 'branches:' "$wf")
    if [ "$branch_lines" -eq 2 ]; then
        ok "$name: push and pull_request each target main once"
    else
        err "$name: expected 2 branch trigger declarations, found $branch_lines"
    fi
    if grep -q 'branches:.*develop' "$wf"; then
        err "$name: develop push/PR trigger would duplicate main-bound PR work"
    fi

    # 2. Explicit timeout on every job (one timeout-minutes per runs-on).
    jobs=$(grep -c 'runs-on:' "$wf")
    timeouts=$(grep -c 'timeout-minutes:' "$wf")
    if [ "$jobs" -eq "$timeouts" ]; then
        ok "$name: all $jobs job(s) carry timeout-minutes"
    else
        err "$name: $jobs job(s) but $timeouts timeout-minutes entr(y/ies)"
    fi

    # 5. Cancel superseded work so stale SHAs do not occupy the fleet.
    if grep -q 'github.event.pull_request.number || github.ref' "$wf" \
        && grep -q 'cancel-in-progress: true' "$wf"; then
        ok "$name: superseded workflow/ref runs are cancelled"
    else
        err "$name: concurrency cancellation contract missing"
    fi
done

# 3. Plugin-building jobs install wasm32-wasip1.
#    rust.yml: build, build-windows, test, test-no-web (4). e2e.yml: test-e2e (1).
rust_wasi=$(grep -c 'wasm32-wasip1' "$rust_yml")
if [ "$rust_wasi" -ge 4 ]; then
    ok "rust.yml: $rust_wasi wasm32-wasip1 target declaration(s) (>= 4 plugin-building jobs)"
else
    err "rust.yml: only $rust_wasi wasm32-wasip1 declaration(s); plugin-building jobs need 4"
fi
if grep -q 'wasm32-wasip1' "$e2e_yml"; then
    ok "e2e.yml: wasm32-wasip1 target declared"
else
    err "e2e.yml: wasm32-wasip1 target missing"
fi

# 4. Canonical gate: the test job runs the repo-native `make ci`.
if grep -q 'run: make ci' "$rust_yml"; then
    ok "rust.yml: canonical job invokes repo-native gate (make ci)"
else
    err "rust.yml: canonical test job does not invoke 'make ci'"
fi

if [ "$fail" -ne 0 ]; then
    echo "workflow contract BROKEN" >&2
    exit 1
fi
echo "workflow contract holds"
