#!/usr/bin/env bash
state_file="/tmp/vc-frame-e2e/cache/append-echo-script-output"
run_count=1
if [ -f "$state_file" ]; then
    run_count=$(( $(wc -l < "$state_file") + 1 ))
fi
printf 'foo-%s\n' "$run_count" >> "$state_file"
cat "$state_file"
