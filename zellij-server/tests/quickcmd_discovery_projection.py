"""Explicit drawer text for one private Panels fixture.

These strings are the expected output of that fixture. They are not a parser,
a provider registry, or a second copy of compact-bar's projection rules.
A label match does not prove that a provider process started.

The fixture cwd directory is named ``live``. The stale pane title is ``Junie``.
Before the swap the row is agent ``junie`` and model ``unknown``. After it the
same pane is agent ``codex`` and model ``m1`` from ``--model m1``.
"""

FIXTURE_NOTICE = (
    "FIXTURE: argv identity only. A matching label does not prove a provider started."
)

CWD_BASENAME = "live"
STALE_TITLE = "Junie"
STATE = "unknown"
BEFORE_AGENT = "junie"
BEFORE_MODEL = "unknown"
AFTER_AGENT = "codex"
AFTER_MODEL = "m1"

JUNIE_LABEL = "unknown · live · junie · unknown"
CODEX_LABEL = "unknown · live · codex · m1"

# Short enough for the 24% drawer at 167 columns, leaving room for the row prefix.
MAX_LABEL_COLUMNS = 32


def fixture_expectations():
    return {
        "notice": FIXTURE_NOTICE,
        "proves_provider_started": False,
        "cwd_basename": CWD_BASENAME,
        "stale_title": STALE_TITLE,
        "state": STATE,
        "before_agent": BEFORE_AGENT,
        "before_model": BEFORE_MODEL,
        "after_agent": AFTER_AGENT,
        "after_model": AFTER_MODEL,
        "junie_label": JUNIE_LABEL,
        "codex_label": CODEX_LABEL,
    }


def _check(condition, message):
    if not condition:
        raise AssertionError(message)


def _run_controls():
    expected = fixture_expectations()
    _check("FIXTURE" in expected["notice"], expected["notice"])
    _check("does not prove a provider started" in expected["notice"], expected["notice"])
    _check(expected["proves_provider_started"] is False, expected)
    _check(expected["stale_title"] == "Junie", expected)
    _check(expected["state"] == "unknown", expected)
    _check(JUNIE_LABEL == "unknown · live · junie · unknown", JUNIE_LABEL)
    _check(CODEX_LABEL == "unknown · live · codex · m1", CODEX_LABEL)
    _check(STALE_TITLE not in JUNIE_LABEL and STALE_TITLE not in CODEX_LABEL, "stale title leaked")
    _check(" · junie · " in JUNIE_LABEL and JUNIE_LABEL.endswith(" · unknown"), JUNIE_LABEL)
    _check(" · codex · " in CODEX_LABEL and CODEX_LABEL.endswith(" · m1"), CODEX_LABEL)
    _check(len(JUNIE_LABEL) <= MAX_LABEL_COLUMNS and len(CODEX_LABEL) <= MAX_LABEL_COLUMNS, "label width")
    _check(JUNIE_LABEL != CODEX_LABEL, "swap must change the row")


if __name__ == "__main__":
    _run_controls()
    print("quickcmd_discovery_projection: controls passed")
