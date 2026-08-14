#!/usr/bin/env python3
"""Fail closed when vc-frame grows a second product/release boundary."""

from __future__ import annotations

from pathlib import Path
import re


ROOT = Path(__file__).resolve().parent.parent


def require(condition: bool, message: str) -> None:
    if not condition:
        raise SystemExit(message)


def main() -> int:
    makefile = (ROOT / "Makefile").read_text(encoding="utf-8")
    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    release_doc = (ROOT / "docs" / "RELEASE.md").read_text(encoding="utf-8")

    forbidden_paths = (
        ROOT / ".github" / "workflows" / "release.yml",
        ROOT / "tools" / "install.sh",
        ROOT / "scripts" / "release-provenance.zsh",
        ROOT / "scripts" / "package-vibecrafted-app.zsh",
    )
    for path in forbidden_paths:
        require(not path.exists(), f"split-product surface must stay absent: {path}")

    require(
        re.search(r"(?m)^release: doctor-quiet$", makefile) is not None,
        "vc-frame must retain the donor release-build target",
    )
    require(
        re.search(
            r"(?m)^release-binary: doctor-quiet plugins-parity$", makefile
        )
        is not None,
        "vc-frame must expose the provenance-stable Vibecrafted.app donor target",
    )
    require(
        "$(CARGO) xtask build --release --no-plugins" in makefile,
        "the Vibecrafted.app donor target must not rewrite committed plugin assets",
    )
    for target in ("install", "package", "release-tag", "release-push"):
        require(
            re.search(rf"(?m)^{re.escape(target)}\s*:", makefile) is None,
            f"vc-frame must not own product target: {target}",
        )

    ownership = "Vibecrafted.app owns"
    require(ownership in readme, "README must name the parent product owner")
    require(ownership in release_doc, "release docs must name the parent product owner")
    require("single `Vibecrafted.dmg`" in release_doc, "single-DMG contract missing")

    combined = "\n".join((makefile, readme, release_doc))
    for forbidden in (
        "releases/latest/download/install.sh",
        "make install",
        "gh release upload",
        "vc-frame.dmg",
    ):
        require(forbidden not in combined, f"legacy split-product promise returned: {forbidden}")

    print("vc-frame donor ownership contract: PASS")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
