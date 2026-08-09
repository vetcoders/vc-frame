#!/usr/bin/env python3
"""Deterministic narrow-terminal chrome reproduction receipt.

This tool does not mutate sessions. It validates the fixed-width glyph contract
and prints the commands an operator can use for a live before/after capture.
"""

from __future__ import annotations

import argparse
import json
import unicodedata


CHROME_GLYPHS = "◉○⚿⌁│·*"
MIN_COLUMNS = {
    "compact-bar": 36,
    "tab-bar": 20,
    "status-bar": 24,
    "session-manager": 32,
    "composer": 40,
    "plugin-manager": 32,
}


def narrow_cell_width(text: str) -> int:
    return sum(0 if unicodedata.combining(char) else 1 for char in text)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--columns", type=int, default=80)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    failures = [name for name, width in MIN_COLUMNS.items() if args.columns < width]
    wide_glyphs = [
        {"glyph": char, "eaw": unicodedata.east_asian_width(char)}
        for char in CHROME_GLYPHS
        if unicodedata.east_asian_width(char) in {"W", "F"}
    ]
    receipt = {
        "columns": args.columns,
        "eaw": "ambiguous-narrow",
        "wide_glyphs": wide_glyphs,
        "glyph_width": narrow_cell_width(CHROME_GLYPHS),
        "glyph_count": len(CHROME_GLYPHS),
        "contracts": MIN_COLUMNS,
        "below_minimum": failures,
        "live_probe": f"resize terminal to {args.columns} columns; vc-frame action doctor-routes --json",
    }
    if args.json:
        print(json.dumps(receipt, sort_keys=True))
    else:
        for key, value in receipt.items():
            print(f"{key}: {value}")
    return 0 if not wide_glyphs and receipt["glyph_width"] == receipt["glyph_count"] else 2


if __name__ == "__main__":
    raise SystemExit(main())
