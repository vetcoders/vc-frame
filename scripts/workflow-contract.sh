#!/usr/bin/env bash
# Workflow contract — asserts the canonical CI gate keeps its shape.
# 𝚅𝚒𝚋𝚎𝚌𝚛𝚊𝚏𝚝𝚎𝚍. with AI Agents by VetCoders (c)2024-2026 LibraxisAI
#
# Invariants guarded here (W0-A):
#   1. rust.yml and e2e.yml trigger on push/PR for BOTH main and develop.
#   2. Every job in those workflows carries an explicit timeout-minutes.
#   3. Every plugin-building job installs the wasm32-wasip1 target.
#   4. The canonical test job invokes the repo-native gate (make ci),
#      so the local gate and the remote gate mean the same thing.
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

    # 1. Branch triggers: every `branches:` line must list main AND develop.
    while IFS= read -r line; do
        case "$line" in
            *main*develop*|*develop*main*) : ;;
            *) err "$name: branch trigger missing main+develop: $line" ;;
        esac
    done < <(grep 'branches:' "$wf")
    grep -q 'branches:' "$wf" && ok "$name: push/PR branch triggers list main + develop"

    # 2. Explicit timeout on every job (one timeout-minutes per runs-on).
    jobs=$(grep -c 'runs-on:' "$wf")
    timeouts=$(grep -c 'timeout-minutes:' "$wf")
    if [ "$jobs" -eq "$timeouts" ]; then
        ok "$name: all $jobs job(s) carry timeout-minutes"
    else
        err "$name: $jobs job(s) but $timeouts timeout-minutes entr(y/ies)"
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
grep -q 'wasm32-wasip1' "$e2e_yml" \
    && ok "e2e.yml: wasm32-wasip1 target declared" \
    || err "e2e.yml: wasm32-wasip1 target missing"

# 4. Canonical gate: the test job runs the repo-native `make ci`.
grep -q 'run: make ci' "$rust_yml" \
    && ok "rust.yml: canonical job invokes repo-native gate (make ci)" \
    || err "rust.yml: canonical test job does not invoke 'make ci'"

if [ "$fail" -ne 0 ]; then
    echo "workflow contract BROKEN" >&2
    exit 1
fi
echo "workflow contract holds"
