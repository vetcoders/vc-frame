#!/bin/sh
# Same canonical owner/runner lookup as the built-in Agents layout.
root="${VC_FRAME_CONFIG_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/vibecrafted/vc-frame}"
runner="$root/pane-python"
launcher="$root/vc-agent-workshop.py"
if [ -x "$runner" ]; then
    exec "$runner" "$launcher" launcher --font
fi
py="${VIBECRAFTED_PYTHON:-}"
if [ -x "$launcher" ]; then
    if [ -n "$py" ] && [ -x "$py" ]; then
        exec "$py" "$launcher" launcher --font
    fi
    exec "$launcher" launcher --font
fi
printf '\n  Terminal Typography launcher missing: %s\n  Repair: run make install from the Vibecrafted checkout with your verified Runtime Pack\n' "$launcher" >&2
exit 127
