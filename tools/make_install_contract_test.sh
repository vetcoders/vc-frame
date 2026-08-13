#!/bin/sh
set -eu

plan="$(make -n install)"

parity_line="$(
    printf '%s\n' "$plan" |
        awk '/plugins-parity\.zsh check/ { print NR; exit }'
)"
install_line="$(
    printf '%s\n' "$plan" |
        awk '/xtask install/ { print NR; exit }'
)"

if [ -z "$parity_line" ]; then
    echo "FAIL: make install does not verify committed plugin assets" >&2
    exit 1
fi

if [ -z "$install_line" ]; then
    echo "FAIL: make install does not invoke cargo xtask install" >&2
    exit 1
fi

if [ "$parity_line" -ge "$install_line" ]; then
    echo "FAIL: plugin parity must run before cargo xtask install" >&2
    exit 1
fi

if ! printf '%s\n' "$plan" | awk '/xtask install/ && /--no-plugins/ { found=1 } END { exit !found }'; then
    echo "FAIL: make install rebuilds tracked plugin assets" >&2
    exit 1
fi

if ! printf '%s\n' "$plan" | grep -Fq 'CARGO_TARGET_DIR="$install_target"'; then
    echo "FAIL: make install does not export the isolated Cargo target" >&2
    exit 1
fi

if ! printf '%s\n' "$plan" | grep -Fq 'vc-frame/build'; then
    echo "FAIL: make install does not isolate Cargo output from the Living Tree" >&2
    exit 1
fi

echo "PASS: make install verifies plugins and builds outside the Living Tree"
