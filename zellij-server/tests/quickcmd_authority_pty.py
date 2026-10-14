"""Isolated shortcut acceptance; requires pyte 0.8.2 and a committed vc-frame.

Run with --binary PATH --output NEW_DIRECTORY --scratch NEW_DRAGON_DIRECTORY.
Never attaches to an existing user session. Retains ANSI, grids and inventories.
Tabs A/B exercise workspace views, not the separately owned host/guest protocol.
"""

import argparse
import codecs
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import re
import select
import shlex
import signal
import struct
import subprocess
import sys
import termios
import time

import quickcmd_discovery_projection as discovery

QUICK_CMD_TITLE = "❯_ Quick cmd"
COMPOSER_TITLE = "✍ Composer · ⧉ Paste stack"
ABOUT_PLUGIN_URL = "vc-frame:about"
ABOUT_FIXTURE_TITLE = "About fixture"
SEED_TITLES = ("seed-alpha", "seed-beta")
# Printed by the private seed pane. Absence from the client grid is visual
# evidence. It is not a second inventory parser.
SEED_MARKER = "SEED_FLOAT_READY"


def pane_key(pane):
    return (bool(pane["is_plugin"]), pane["id"])


def pane_by_key(panes, key):
    """One row for a ``(is_plugin, id)`` key. A shared numeric id is not a match."""
    found = [pane for pane in panes if pane_key(pane) == key]
    assert len(found) == 1, (key, found)
    return found[0]


def replaced_terminal(before, panes):
    """The same terminal after the fixture argv swap.

    A plugin may reuse the numeric id. ``pane_key`` keeps that plugin out.
    Title, cwd, and the launch command stay. The live argv changes from
    ``junie`` to ``codex --model m1``.
    """
    key = pane_key(before)
    if key[0] is not False:
        raise AssertionError(("terminal kind", key))
    found = pane_by_key(panes, key)
    if found["title"] != before["title"] or found["exited"] is not False:
        raise AssertionError(("terminal identity", key, found["title"], found["exited"]))
    if found.get("pane_cwd") != before.get("pane_cwd"):
        raise AssertionError(("pane cwd", key))
    if found.get("terminal_command") != before.get("terminal_command"):
        raise AssertionError(("launch command", key))
    before_argv = before.get("pane_command")
    after_argv = found.get("pane_command")
    if (
        not isinstance(before_argv, str)
        or not isinstance(after_argv, str)
        or not before_argv.endswith("/junie")
        or not after_argv.endswith("/codex --model m1")
        or before_argv == after_argv
    ):
        raise AssertionError(("pane argv", key))
    return found


def hidden_float_keys(panes):
    return {
        pane_key(pane) for pane in panes
        if pane.get("is_suppressed") and pane.get("is_floating") and not pane.get("exited")
    }


def assert_float_visibility(panes, visible_ids, focused_id, concealed_ids):
    """Reject an inventory where a concealed float is visible.

    ``visible_ids`` and ``concealed_ids`` are ``(is_plugin, id)`` pairs.
    ``focused_id`` is the requested target. A row that merely exists is not
    enough: each concealed id must still be the same floating suppressed pane,
    and the requested id must be visible and focused. Already-visible requested
    panes may stay in ``visible_ids``; this does not require every other float
    to hide.
    """
    keys = [pane_key(pane) for pane in panes]
    if len(keys) != len(set(keys)):
        dupes = sorted(key for key in set(keys) if keys.count(key) > 1)
        raise AssertionError(("duplicate pane ids", dupes))
    by_key = {pane_key(pane): pane for pane in panes}
    for key in concealed_ids:
        pane = by_key.get(key)
        if pane is None or pane.get("exited") or not pane.get("is_floating") or not pane.get("is_suppressed"):
            raise AssertionError(("concealed float became visible or changed", key, pane))
        if pane.get("is_focused"):
            raise AssertionError(("concealed float is focused", key, pane))
    for key in visible_ids:
        pane = by_key.get(key)
        if pane is None or pane.get("exited") or not pane.get("is_floating") or pane.get("is_suppressed"):
            raise AssertionError(("requested float is not visible", key, pane))
    if focused_id is None:
        if visible_ids:
            raise AssertionError(("visible floats have no requested focus", visible_ids))
        return
    focused = by_key.get(focused_id)
    if focused_id not in visible_ids or focused is None or not focused.get("is_focused"):
        raise AssertionError(("requested target is not focused", focused_id, focused))


def assert_context_suppressed(panes, expected):
    """Competing context layer after ``suppress_pane``.

    ``expected`` items are ``((is_plugin, id), title, tab_id)`` for a pane
    already observed as that context layer. ``Tab::pane_infos`` projects a
    ``suppressed_panes`` entry as ``is_suppressed`` true, ``is_floating``
    false, and ``is_focused`` false. The same id, title, and tab must remain.
    A concealed float, a tiled pane, a rename, or a missing pane is not this
    state. An empty ``expected`` checks duplicates only; callers still have
    to account for every previously observed context pane.

    ``is_focused`` and ``exited`` must be explicit false. A missing or null
    bit is not an unfocused live pane.
    """
    keys = [pane_key(pane) for pane in panes]
    if len(keys) != len(set(keys)):
        dupes = sorted(key for key in set(keys) if keys.count(key) > 1)
        raise AssertionError(("duplicate pane ids", dupes))
    by_key = {pane_key(pane): pane for pane in panes}
    for key, title, tab_id in expected:
        pane = by_key.get(key)
        if pane is None:
            raise AssertionError(("context pane vanished", key))
        if pane.get("title") != title:
            raise AssertionError(("context pane renamed", key, title, pane.get("title")))
        if pane.get("tab_id") != tab_id:
            raise AssertionError(("context pane left its tab", key, pane))
        exited = pane.get("exited")
        if exited is not False:
            if exited is True:
                raise AssertionError(("context pane exited", key, pane))
            raise AssertionError(("context pane exited is unknown", key, pane))
        focused = pane.get("is_focused")
        if focused is not False:
            if focused is True:
                raise AssertionError(("context pane is focused", key, pane))
            raise AssertionError(("context pane focus is unknown", key, pane))
        if pane.get("is_suppressed") is not True or pane.get("is_floating") is not False:
            raise AssertionError(("context pane was not suppressed", key, pane))


def assert_layer_hidden(panes, tab, live_ids, grid_text, marker=SEED_MARKER):
    """Whole-layer hide from ``hide-floating-panes``.

    That action clears the conceal set and sets ``show_panes`` false.
    ``is_suppressed`` is only ``pane_is_concealed``, so it stays false.
    The public layer flag is ``TabInfo.are_floating_panes_visible``.
    The named panes must stay live floating panes on this tab, and ``marker``
    must be absent from the rendered grid. Individual conceal is a different
    state and fails this check.

    ``is_focused`` on that inventory is the floating ``active_panes`` id.
    ``hide_floating_panes`` leaves that id in place. Effective keyboard input
    uses ``get_active_pane_id`` / ``write_to_active_terminal``, which select
    the tiled pane when ``panes_are_visible`` is false. A retained true bit
    is therefore allowed here. A missing bit is not.
    """
    if tab.get("are_floating_panes_visible") is not False:
        raise AssertionError((
            "floating layer still visible", tab.get("tab_id"), tab.get("are_floating_panes_visible")))
    if marker and marker in grid_text:
        raise AssertionError(("seed marker rendered while layer hidden", marker))
    keys = [pane_key(pane) for pane in panes]
    if len(keys) != len(set(keys)):
        dupes = sorted(key for key in set(keys) if keys.count(key) > 1)
        raise AssertionError(("duplicate pane ids", dupes))
    by_key = {pane_key(pane): pane for pane in panes}
    for key in live_ids:
        pane = by_key.get(key)
        if pane is None or pane.get("exited") or not pane.get("is_floating"):
            raise AssertionError(("hidden-layer pane is not the same live float", key, pane))
        if pane.get("tab_id") != tab.get("tab_id"):
            raise AssertionError(("hidden-layer pane left its tab", key, pane))
        if pane.get("is_suppressed"):
            raise AssertionError(("whole-layer hide reported individual conceal", key, pane))
        if not isinstance(pane.get("is_focused"), bool):
            raise AssertionError(("hidden-layer focus bit is not explicit", key, pane))


def assert_not_revealed(panes, tab, live_ids, grid_text, marker=SEED_MARKER):
    """Reject a pane that leaves either hidden representation.

    Whole-layer hide is ``are_floating_panes_visible`` false with
    ``is_suppressed`` false. Individual conceal is ``is_suppressed`` true.
    The layer flag must be a bool. Each named pane must stay the same live
    float on this tab. A float that is not suppressed on a visible layer
    was revealed. A pane that is no longer floating is not that hidden float.
    A retained ``is_focused`` bit is allowed only for that explicit hidden
    layer, and only when the pane is not individually concealed. On a visible
    layer a concealed float must not be focused.
    """
    if marker and marker in grid_text:
        raise AssertionError(("hidden pane marker rendered", marker))
    layer_visible = tab.get("are_floating_panes_visible")
    if not isinstance(layer_visible, bool):
        raise AssertionError((
            "floating layer visibility is not explicit",
            tab.get("tab_id"), layer_visible))
    keys = [pane_key(pane) for pane in panes]
    if len(keys) != len(set(keys)):
        dupes = sorted(key for key in set(keys) if keys.count(key) > 1)
        raise AssertionError(("duplicate pane ids", dupes))
    by_key = {pane_key(pane): pane for pane in panes}
    for key in live_ids:
        pane = by_key.get(key)
        if pane is None or pane.get("exited") or pane.get("is_floating") is not True:
            raise AssertionError(("hidden pane is not the same live float", key, pane))
        if pane.get("tab_id") != tab.get("tab_id"):
            raise AssertionError(("hidden pane left its tab", key, pane))
        focused = pane.get("is_focused")
        if not isinstance(focused, bool):
            raise AssertionError(("hidden pane focus bit is not explicit", key, pane))
        retained_selection = layer_visible is False and pane.get("is_suppressed") is False
        if focused and not retained_selection:
            raise AssertionError(("hidden pane is focused", key, pane))
        if not pane.get("is_suppressed") and layer_visible:
            raise AssertionError(("hidden pane was revealed", key, pane))


CLIENT_TABLE_HEADER = "CLIENT_ID ZELLIJ_PANE_ID RUNNING_COMMAND"
_PANE_TOKEN = re.compile(r"^(?:terminal|plugin)_\d+$")


def parse_client_table(text):
    """Rows from public ``action list-clients``.

    ``ClientMetadata::render_many`` prints CLIENT_ID, ZELLIJ_PANE_ID, and
    RUNNING_COMMAND. CLIENT_ID is the socket-local id. The pane column is a
    ``terminal_<id>`` or ``plugin_<id>`` token and is not a client id. The
    row has no pid. ``BTreeMap`` order is numeric id order, not attach order.
    """
    lines = [line.rstrip() for line in text.splitlines()]
    if lines and lines[-1] == "":
        lines = lines[:-1]
    if not lines or lines[0] != CLIENT_TABLE_HEADER:
        raise AssertionError(("client table header", lines[:1]))
    rows = []
    seen = set()
    for line in lines[1:]:
        if line == "":
            raise AssertionError(("client table blank row",))
        parts = line.split(None, 2)
        if len(parts) != 3:
            raise AssertionError(("client table row", line))
        client_token, pane_token, command = parts
        if (
            not client_token.isdigit()
            or str(int(client_token)) != client_token
            or int(client_token) < 1
        ):
            raise AssertionError(("client id is not a socket-local id", client_token))
        if not _PANE_TOKEN.match(pane_token):
            raise AssertionError(("pane token is not a client id", pane_token))
        client_id = int(client_token)
        if client_id in seen:
            raise AssertionError(("duplicate client id", client_id))
        seen.add(client_id)
        rows.append({"client_id": client_id, "pane_id": pane_token, "command": command})
    return rows


def sole_client_id(text):
    """The one interactive client while only one fixture is attached."""
    rows = parse_client_table(text)
    if len(rows) != 1:
        kind = "missing client" if not rows else "ambiguous client"
        raise AssertionError((kind, [row["client_id"] for row in rows]))
    return rows[0]["client_id"]


def client_id_that_appeared(before_text, after_text):
    """The single CLIENT_ID that showed up when one more fixture attached.

    Every id from the earlier table must still be present. Two new ids are
    ambiguous. No new id is missing. The last row, the highest id, the pane
    token, and the command text are not a choice.
    """
    before_ids = {row["client_id"] for row in parse_client_table(before_text)}
    after_ids = {row["client_id"] for row in parse_client_table(after_text)}
    if not before_ids:
        raise AssertionError(("missing client", []))
    disappeared = sorted(before_ids - after_ids)
    if disappeared:
        raise AssertionError(("missing client", disappeared))
    appeared = sorted(after_ids - before_ids)
    if len(appeared) != 1:
        kind = "missing client" if not appeared else "ambiguous client"
        raise AssertionError((kind, appeared))
    return appeared[0]


def attended_client_id(text, attended_id):
    """Return ``attended_id`` only while that exact id is still in the table.

    ``None`` does not select the last row. A pane token is not a client id.
    """
    ids = [row["client_id"] for row in parse_client_table(text)]
    if not isinstance(attended_id, int) or isinstance(attended_id, bool):
        raise AssertionError(("ambiguous client", attended_id, ids))
    if attended_id not in ids:
        raise AssertionError(("missing client", attended_id, ids))
    return attended_id


def bind_fixture_client(before_text, after_text, which):
    """Bind the fixture observed alone, or the one id that appeared after it."""
    first_id = sole_client_id(before_text)
    other_id = client_id_that_appeared(before_text, after_text)
    if which == "first":
        attended = first_id
    elif which == "other":
        attended = other_id
    else:
        raise AssertionError(("unknown fixture", which))
    if first_id == other_id:
        raise AssertionError(("ambiguous client", first_id, other_id))
    return attended_client_id(after_text, attended)


def client_action_args(client_id, command):
    """Root ``--client-id`` before the ``action`` subcommand.

    ``CliArgs.client_id`` is a global long on the root parser. The action
    operand stays after the subcommand, so ``go-to-tab``'s tab index is not
    the client id and the flag is not an action payload.
    """
    if not isinstance(client_id, int) or isinstance(client_id, bool) or client_id < 1:
        raise AssertionError(("client id is not a socket-local id", client_id))
    command = tuple(command)
    if not command or command[0] != "action":
        raise AssertionError(("not an action command", command))
    if "--client-id" in command:
        raise AssertionError(("client id belongs before the action payload", command))
    return ("--client-id", str(client_id), *command)


def session_cli_args(session, command, client_id=None):
    """Inventory calls stay unscoped. A client action receives the root flag."""
    head = ["--session", session]
    if client_id is None:
        return head + list(command)
    return head + list(client_action_args(client_id, command))


# Canonical pane token. ``terminal_00`` is not ``terminal_0``. Display text
# ``terminal_<id>`` is not the Debug sentence ``Terminal(<id>)``.
_FOCUS_TOKEN = re.compile(r"(terminal|plugin)_(0|[1-9]\d*)")
# ``validated_cli_action_origin`` refusals. LogError makes the process exit 2.
# That exit is not an already-focused pane.
_OWNERSHIP_REFUSALS = (
    "multiple attached clients; select --client-id from action list-clients before navigating\n",
    "no attached interactive client to navigate; attach a frontend first\n",
)
_UNATTACHED_CLIENT = re.compile(
    r"client (?:0|[1-9]\d*) is not attached to this session; refresh action list-clients\n"
)


def already_focused_notice(token):
    """Stderr for ``Pane {:?} is already focused`` plus eprintln's newline.

    ``FocusPaneWithId`` writes that sentence with ``PaneId``'s Debug form,
    ``Terminal(0)`` or ``Plugin(3)``. The public process exit for the LogError
    is 2. The screen thread's completion status 1 is not this sentence.
    """
    match = _FOCUS_TOKEN.fullmatch(token) if isinstance(token, str) else None
    if match is None:
        raise AssertionError(("focus token", token))
    kind, digits = match.group(1), match.group(2)
    label = "Terminal" if kind == "terminal" else "Plugin"
    return f"Pane {label}({int(digits)}) is already focused\n"


def idempotent_focus_outcome(returncode, stdout, stderr, token, clients_text, attended_id):
    """Classify one routed ``focus-pane-id`` for an admitted CLIENT_ID.

    Exit 0 is a fresh focus and does not read the client table or the notice.
    Already-focused is only public exit 2, stderr exactly the notice for
    ``token``, empty stdout, and that client's current ``ZELLIJ_PANE_ID``
    equal to ``token``. Any other status, a different sentence, a missing
    client, another pane, or an ownership refusal fails. No retry.
    """
    if returncode == 0:
        return "focused"
    if returncode != 2:
        raise AssertionError(("focus status", returncode, stderr, stdout))
    if stderr in _OWNERSHIP_REFUSALS or (
        isinstance(stderr, str) and _UNATTACHED_CLIENT.fullmatch(stderr)
    ):
        raise AssertionError(("ownership refusal", stderr))
    expected = already_focused_notice(token)
    if stderr != expected or stdout != "":
        raise AssertionError(("focus text", token, stderr, stdout))
    admitted = attended_client_id(clients_text, attended_id)
    named = next(
        row["pane_id"]
        for row in parse_client_table(clients_text)
        if row["client_id"] == admitted
    )
    if named != token:
        raise AssertionError(("other client focus", admitted, named, token))
    return "already_focused"


def run_visibility_controls():
    def row(pane_id, suppressed, focused, plugin=False):
        return {
            "is_plugin": plugin, "id": pane_id, "is_floating": True,
            "is_suppressed": suppressed, "is_focused": focused, "exited": False,
        }

    negative = [row(1, False, True), row(2, False, False)]
    try:
        assert_float_visibility(negative, [(False, 1)], (False, 1), [(False, 2)])
    except AssertionError as error:
        if "concealed float" not in str(error):
            raise
    else:
        raise AssertionError("negative inventory accepted an unrelated visible float")

    positive = [row(1, False, True), row(2, True, False), row(3, True, False)]
    assert_float_visibility(positive, [(False, 1)], (False, 1), [(False, 2), (False, 3)])

    stacked = [row(1, False, False), row(4, False, True), row(2, True, False), row(3, True, False)]
    assert_float_visibility(
        stacked, [(False, 4), (False, 1)], (False, 4), [(False, 2), (False, 3)])

    unfocused = [row(1, False, False), row(2, True, False)]
    try:
        assert_float_visibility(unfocused, [(False, 1)], (False, 1), [(False, 2)])
    except AssertionError as error:
        if "not focused" not in str(error):
            raise
    else:
        raise AssertionError("visible target without focus was accepted")

    duplicated = [row(1, False, True), row(1, True, False)]
    try:
        assert_float_visibility(duplicated, [(False, 1)], (False, 1), [])
    except AssertionError as error:
        if "duplicate" not in str(error):
            raise
    else:
        raise AssertionError("duplicate pane id was accepted")

    def layer_row(pane_id, suppressed, focused):
        return {
            "is_plugin": False, "id": pane_id, "is_floating": True, "tab_id": 7,
            "is_suppressed": suppressed, "is_focused": focused, "exited": False,
        }

    def layer_tab(visible):
        return {"tab_id": 7, "are_floating_panes_visible": visible}

    hidden_layer = [layer_row(1, False, False), layer_row(2, False, False)]
    assert_layer_hidden(hidden_layer, layer_tab(False), [(False, 1), (False, 2)], "Lena fork\n")

    try:
        assert_layer_hidden(
            hidden_layer, layer_tab(False), [(False, 1), (False, 2)], "SEED_FLOAT_READY\n")
    except AssertionError as error:
        if "seed marker" not in str(error):
            raise
    else:
        raise AssertionError("rendered seed marker accepted as layer hide")

    try:
        assert_layer_hidden(hidden_layer, layer_tab(True), [(False, 1), (False, 2)], "")
    except AssertionError as error:
        if "floating layer" not in str(error):
            raise
    else:
        raise AssertionError("visible layer accepted as whole-layer hide")

    individually = [layer_row(1, True, False), layer_row(2, True, False)]
    try:
        assert_layer_hidden(individually, layer_tab(False), [(False, 1), (False, 2)], "")
    except AssertionError as error:
        if "individual conceal" not in str(error):
            raise
    else:
        raise AssertionError("individual conceal accepted as whole-layer hide")

    # Same-tab activation opens the layer and conceals the other floats.
    # That outcome stays is_suppressed, not the layer flag.
    opened = [layer_row(4, False, True), layer_row(1, True, False), layer_row(2, True, False)]
    opened[0]["is_focused"] = True
    assert_float_visibility(opened, [(False, 4)], (False, 4), [(False, 1), (False, 2)])
    try:
        assert_layer_hidden(opened, layer_tab(True), [(False, 1), (False, 2)], "")
    except AssertionError as error:
        if "floating layer" not in str(error):
            raise
    else:
        raise AssertionError("activated layer accepted as whole-layer hide")

    assert_not_revealed(hidden_layer, layer_tab(False), [(False, 1), (False, 2)], "Lena fork\n")
    assert_not_revealed(individually, layer_tab(True), [(False, 1), (False, 2)], "")
    revealed = [layer_row(1, False, False), layer_row(2, True, False)]
    try:
        assert_not_revealed(revealed, layer_tab(True), [(False, 1)], "")
    except AssertionError as error:
        if "was revealed" not in str(error):
            raise
    else:
        raise AssertionError("unrelated visible float accepted as still hidden")
    try:
        assert_not_revealed(hidden_layer, layer_tab(False), [(False, 1), (False, 2)], SEED_MARKER)
    except AssertionError as error:
        if "marker rendered" not in str(error):
            raise
    else:
        raise AssertionError("rendered seed marker accepted after activation")

    # Parent counterexample 1: a missing layer flag is not a hidden layer.
    # The panes below are live, unsuppressed floats.
    try:
        assert_not_revealed(
            hidden_layer, {"tab_id": 7}, [(False, 1), (False, 2)], "Lena fork\n")
    except AssertionError as error:
        if "not explicit" not in str(error):
            raise
    else:
        raise AssertionError("missing layer visibility accepted an unsuppressed pane")

    try:
        assert_not_revealed(
            hidden_layer,
            {"tab_id": 7, "are_floating_panes_visible": None},
            [(False, 1)],
            "Lena fork\n")
    except AssertionError as error:
        if "not explicit" not in str(error):
            raise
    else:
        raise AssertionError("null layer visibility accepted an unsuppressed pane")

    try:
        assert_not_revealed(
            hidden_layer,
            {"tab_id": 7, "are_floating_panes_visible": "false"},
            [(False, 1)],
            "Lena fork\n")
    except AssertionError as error:
        if "not explicit" not in str(error):
            raise
    else:
        raise AssertionError("non-bool layer visibility accepted an unsuppressed pane")

    # Parent counterexample 2: a pane that left the floating layer is not
    # the same hidden float, even when the layer flag is an explicit false.
    tiled = [dict(layer_row(1, False, False), is_floating=False)]
    try:
        assert_not_revealed(tiled, layer_tab(False), [(False, 1)], "Lena fork\n")
    except AssertionError as error:
        if "same live float" not in str(error):
            raise
    else:
        raise AssertionError("tiled pane accepted as the same hidden float")

    ambiguous = [layer_row(1, False, False), layer_row(1, True, False)]
    try:
        assert_not_revealed(ambiguous, layer_tab(False), [(False, 1)], "")
    except AssertionError as error:
        if "duplicate" not in str(error):
            raise
    else:
        raise AssertionError("ambiguous pane id accepted as hidden")

    # Installed Frame 959 against harness b4: seed-beta id 8 stayed
    # is_focused while tab 0 are_floating_panes_visible was false, the seed
    # marker was absent, and the pane was a live unsuppressed float.
    # b4 raised "hidden-layer pane is focused" here. The successor accepts it.
    retained = [{
        "is_plugin": False, "id": 8, "is_floating": True, "tab_id": 0,
        "is_suppressed": False, "is_focused": True, "exited": False,
        "title": "seed-beta",
    }]
    retained_tab = {"tab_id": 0, "are_floating_panes_visible": False}
    assert_layer_hidden(retained, retained_tab, [(False, 8)], "Lena fork\n")
    assert_not_revealed(retained, retained_tab, [(False, 8)], "Lena fork\n")
    try:
        assert_layer_hidden(retained, retained_tab, [(False, 8)], SEED_MARKER)
    except AssertionError as error:
        if "seed marker" not in str(error):
            raise
    else:
        raise AssertionError("retained focus accepted a rendered seed marker")

    missing_focus = [dict(layer_row(1, False, False))]
    del missing_focus[0]["is_focused"]
    try:
        assert_layer_hidden(missing_focus, layer_tab(False), [(False, 1)], "Lena fork\n")
    except AssertionError as error:
        if "focus bit" not in str(error):
            raise
    else:
        raise AssertionError("hidden layer accepted a pane with no focus bit")

    concealed_focused = [layer_row(1, True, True)]
    try:
        assert_layer_hidden(concealed_focused, layer_tab(False), [(False, 1)], "")
    except AssertionError as error:
        if "individual conceal" not in str(error):
            raise
    else:
        raise AssertionError("focused conceal accepted as whole-layer hide")
    try:
        assert_not_revealed(concealed_focused, layer_tab(True), [(False, 1)], "")
    except AssertionError as error:
        if "is focused" not in str(error):
            raise
    else:
        raise AssertionError("concealed float on a visible layer accepted while focused")

    def context_row(**overrides):
        row = {
            "is_plugin": False, "id": 9, "is_floating": False, "tab_id": 0,
            "is_suppressed": True, "is_focused": False, "exited": False,
            "title": QUICK_CMD_TITLE,
        }
        row.update(overrides)
        return row

    # Installed Frame 959: after Composer, Quick cmd id 9 is live on tab 0
    # with this exact projection. assert_float_visibility still rejects it.
    # The successor accepts it only as context suppression.
    quick_receipt = [context_row()]
    quick_expected = [((False, 9), QUICK_CMD_TITLE, 0)]
    try:
        assert_float_visibility(quick_receipt, [], None, [(False, 9)])
    except AssertionError as error:
        if "became visible or changed" not in str(error):
            raise
    else:
        raise AssertionError("suppressed quick cmd accepted as a concealed float")
    assert_context_suppressed(quick_receipt, quick_expected)

    seed_tiled = [context_row(id=8, title="seed-beta", is_suppressed=False)]
    try:
        assert_float_visibility(seed_tiled, [], None, [(False, 8)])
    except AssertionError as error:
        if "became visible or changed" not in str(error):
            raise
    else:
        raise AssertionError("tiled seed accepted as a concealed float")

    def expect_context(rows, expected, text):
        try:
            assert_context_suppressed(rows, expected)
        except AssertionError as error:
            if text not in str(error):
                raise
        else:
            raise AssertionError(("context suppression accepted", text))

    expect_context([], quick_expected, "vanished")
    expect_context([context_row(title="seed-beta")], quick_expected, "renamed")
    expect_context([context_row(tab_id=1)], quick_expected, "left its tab")
    expect_context([context_row(exited=True)], quick_expected, "exited")
    expect_context([context_row(is_focused=True)], quick_expected, "is focused")
    expect_context([context_row(is_suppressed=False)], quick_expected, "was not suppressed")
    expect_context(
        [context_row(is_floating=True, is_suppressed=False)],
        quick_expected, "was not suppressed")
    expect_context(
        [context_row(is_floating=True, is_suppressed=True)],
        quick_expected, "was not suppressed")
    expect_context([context_row(), context_row()], quick_expected, "duplicate")

    unknown_focus = context_row()
    del unknown_focus["is_focused"]
    expect_context([unknown_focus], quick_expected, "focus is unknown")
    expect_context([context_row(is_focused=None)], quick_expected, "focus is unknown")
    unknown_exited = context_row()
    del unknown_exited["exited"]
    expect_context([unknown_exited], quick_expected, "exited is unknown")
    expect_context([context_row(exited=None)], quick_expected, "exited is unknown")

    print("quickcmd_authority_pty: visibility controls passed")
    run_client_binding_controls()
    run_idempotent_focus_controls()
    run_typed_pane_controls()
    run_foreground_handoff_controls()


def _client_table(rows):
    lines = [CLIENT_TABLE_HEADER]
    for client_id, pane, command in rows:
        lines.append(f"{client_id:<9} {pane:<14} {command:<15}")
    return "\n".join(lines)


def _refuse(call, text):
    try:
        call()
    except AssertionError as error:
        if text not in str(error):
            raise
    else:
        raise AssertionError(("client bind accepted", text))


def run_client_binding_controls():
    """Pure list-clients binding. No PTY and no native binary."""
    workload = "/Volumes/py /private/tmp/vc-panels/workload.py"
    # Installed a85965a594 shape: two clients, one pane, one command, no pid.
    # The last printed row is client 2. That row is not a bind.
    installed = _client_table([
        (1, "terminal_0", workload),
        (2, "terminal_0", workload),
    ])
    assert parse_client_table(installed)[-1]["client_id"] == 2
    _refuse(lambda: attended_client_id(installed, None), "ambiguous client")
    _refuse(lambda: sole_client_id(installed), "ambiguous client")
    _refuse(lambda: attended_client_id(installed, 9), "missing client")

    # Different panes and commands still do not name a fixture. Pane id is
    # not a client id, and RUNNING_COMMAND is the focused pane command.
    distinct = _client_table([
        (1, "terminal_0", "python /tmp/first.py"),
        (2, "terminal_9", "python /tmp/other.py"),
    ])
    _refuse(lambda: attended_client_id(distinct, None), "ambiguous client")
    _refuse(
        lambda: parse_client_table(
            "CLIENT_ID ZELLIJ_PANE_ID RUNNING_COMMAND\n"
            "terminal_0  terminal_0     python workload.py"
        ),
        "socket-local",
    )
    _refuse(
        lambda: parse_client_table(_client_table([
            (1, "terminal_0", "python a"),
            (1, "terminal_1", "python b"),
        ])),
        "duplicate",
    )

    # Active fixture is the id observed while that process was the only
    # client, or the single id that appeared when the other process attached.
    # Numeric order would print 4 before 7, so the last row is the first
    # fixture. The other fixture is 4.
    before = _client_table([(7, "terminal_0", workload)])
    after = _client_table([
        (4, "terminal_0", workload),
        (7, "terminal_0", workload),
    ])
    assert parse_client_table(after)[-1]["client_id"] == 7
    assert bind_fixture_client(before, after, "first") == 7
    assert bind_fixture_client(before, after, "other") == 4
    assert attended_client_id(after, 7) == 7
    assert attended_client_id(after, 4) == 4
    assert {row["client_id"] for row in parse_client_table(after)} == {4, 7}

    sequential_before = _client_table([(1, "terminal_0", workload)])
    sequential_after = _client_table([
        (1, "terminal_0", workload),
        (2, "terminal_0", workload),
    ])
    assert bind_fixture_client(sequential_before, sequential_after, "first") == 1
    assert bind_fixture_client(sequential_before, sequential_after, "other") == 2

    lost = _client_table([(4, "terminal_0", workload)])
    _refuse(lambda: client_id_that_appeared(before, lost), "missing client")
    _refuse(lambda: attended_client_id(lost, 7), "missing client")
    _refuse(lambda: client_id_that_appeared(before, before), "missing client")
    crowded = _client_table([
        (4, "terminal_0", workload),
        (7, "terminal_0", workload),
        (9, "terminal_1", workload),
    ])
    _refuse(lambda: client_id_that_appeared(before, crowded), "ambiguous client")
    _refuse(lambda: sole_client_id(CLIENT_TABLE_HEADER + "\n"), "missing client")

    routed = session_cli_args("qca9", ("action", "go-to-tab", "2"), 1)
    assert routed == ["--session", "qca9", "--client-id", "1", "action", "go-to-tab", "2"]
    assert routed.index("--client-id") < routed.index("action")
    assert routed[routed.index("go-to-tab") + 1] == "2"
    for inventory in (
        ("action", "list-clients"),
        ("action", "list-panes", "--all", "--json"),
        ("action", "list-tabs", "--all", "--json"),
        ("action", "doctor-routes", "--json"),
    ):
        argv = session_cli_args("qca9", inventory, None)
        assert argv[:2] == ["--session", "qca9"] and "--client-id" not in argv
    _refuse(
        lambda: client_action_args(1, ("action", "go-to-tab", "2", "--client-id", "9")),
        "payload",
    )
    moved = session_cli_args(
        "qca9",
        ("action", "launch-or-focus-plugin", "--floating", "--move-to-focused-tab", "vc-frame:about"),
        4,
    )
    assert moved[:4] == ["--session", "qca9", "--client-id", "4"]
    assert moved[-1] == "vc-frame:about"
    focused = session_cli_args("qca9", ("action", "focus-pane-id", "plugin_3"), 7)
    assert focused == ["--session", "qca9", "--client-id", "7", "action", "focus-pane-id", "plugin_3"]
    print("quickcmd_authority_pty: client binding controls passed")


def run_idempotent_focus_controls():
    """Exact already-focused classification. No PTY and no native binary.

    The positive is the installed b26077d298 teardown: public exit 2, the
    exact ``Pane Terminal(0) is already focused`` line, and attended client
    2's current row naming ``terminal_0``. Client 1 may name that same pane.
    """
    workload = "/Volumes/py /private/tmp/vc-panels/workload.py"
    both = _client_table([
        (1, "terminal_0", workload),
        (2, "terminal_0", workload),
    ])
    notice = "Pane Terminal(0) is already focused\n"
    assert already_focused_notice("terminal_0") == notice
    assert idempotent_focus_outcome(2, "", notice, "terminal_0", both, 2) == "already_focused"
    assert idempotent_focus_outcome(2, "", notice, "terminal_0", both, 1) == "already_focused"
    plugin_rows = _client_table([
        (1, "terminal_0", workload),
        (2, "plugin_3", workload),
    ])
    plugin_notice = "Pane Plugin(3) is already focused\n"
    assert already_focused_notice("plugin_3") == plugin_notice
    assert idempotent_focus_outcome(
        2, "", plugin_notice, "plugin_3", plugin_rows, 2) == "already_focused"
    # Exit 0 ignores the notice, the token grammar, and the client table.
    assert idempotent_focus_outcome(0, notice, "already focused", "terminal_00", "not a table", None) == "focused"

    def refuse(call, text):
        try:
            call()
        except AssertionError as error:
            if text not in str(error):
                raise
        else:
            raise AssertionError(("idempotent focus accepted", text))

    refuse(lambda: idempotent_focus_outcome(1, "", notice, "terminal_0", both, 2), "focus status")
    refuse(lambda: idempotent_focus_outcome(3, "", notice, "terminal_0", both, 2), "focus status")
    refuse(lambda: idempotent_focus_outcome(2, "", "already focused\n", "terminal_0", both, 2), "focus text")
    refuse(lambda: idempotent_focus_outcome(2, "", notice + "extra", "terminal_0", both, 2), "focus text")
    refuse(lambda: idempotent_focus_outcome(2, "", notice[:-1], "terminal_0", both, 2), "focus text")
    refuse(lambda: idempotent_focus_outcome(2, "\n", notice, "terminal_0", both, 2), "focus text")
    refuse(
        lambda: idempotent_focus_outcome(
            2, "", "Pane Terminal(1) is already focused\n", "terminal_0", both, 2),
        "focus text",
    )
    refuse(
        lambda: idempotent_focus_outcome(
            2, "", "Pane Plugin(0) is already focused\n", "terminal_0", both, 2),
        "focus text",
    )
    refuse(
        lambda: idempotent_focus_outcome(
            2, "", "Pane with id Terminal(0) not found\n", "terminal_0", both, 2),
        "focus text",
    )
    other = _client_table([
        (1, "terminal_0", workload),
        (2, "terminal_9", "python /tmp/other.py"),
    ])
    refuse(lambda: idempotent_focus_outcome(2, "", notice, "terminal_0", other, 2), "other client focus")
    refuse(lambda: idempotent_focus_outcome(2, "", notice, "terminal_0", both, 9), "missing client")
    refuse(lambda: already_focused_notice("terminal_00"), "focus token")
    refuse(lambda: idempotent_focus_outcome(2, "", notice, "terminal_00", both, 2), "focus token")
    for refusal in (
        "multiple attached clients; select --client-id from action list-clients before navigating\n",
        "no attached interactive client to navigate; attach a frontend first\n",
        "client 2 is not attached to this session; refresh action list-clients\n",
    ):
        refuse(lambda refusal=refusal: idempotent_focus_outcome(2, "", refusal, "terminal_0", both, 2), "ownership refusal")
    refuse(
        lambda: idempotent_focus_outcome(
            2, "",
            "multiple attached clients; select --client-id from action list-clients before navigating",
            "terminal_0", both, 2),
        "focus text",
    )
    refuse(lambda: idempotent_focus_outcome(2, "", "\n", "terminal_0", both, 2), "focus text")
    print("quickcmd_authority_pty: idempotent focus controls passed")


def run_typed_pane_controls():
    """Typed terminal lookup when a plugin reuses the numeric id. No PTY.

    Installed ``e1ba061a98`` had plugin session-manager 6 and terminal Junie 6.
    A bare ``id == 6`` filter returned both. The terminal argv changed from
    ``junie`` to ``codex --model m1``; cwd and the launch command stayed.
    """
    cwd = "/fx/seen/live"
    launch = "/bin/argv-fixture-holder"
    junie_argv = "/bin/python /bin/junie"
    codex_argv = "/bin/python /bin/codex --model m1"
    plugin = {
        "is_plugin": True, "id": 6, "title": "session-manager", "exited": False,
        "terminal_command": None, "pane_command": None, "pane_cwd": None,
        "plugin_url": "session-manager",
    }
    before = {
        "is_plugin": False, "id": 6, "title": "Junie", "exited": False,
        "terminal_command": launch, "pane_command": junie_argv, "pane_cwd": cwd,
    }
    after_terminal = dict(before)
    after_terminal["pane_command"] = codex_argv
    after = [plugin, after_terminal]
    bare = [pane for pane in after if pane["id"] == 6]
    assert len(bare) == 2 and {pane_key(pane) for pane in bare} == {(True, 6), (False, 6)}
    found = replaced_terminal(before, after)
    assert pane_key(found) == (False, 6)
    assert found["pane_cwd"] == cwd and found["terminal_command"] == launch
    assert found["pane_command"] == codex_argv and found["title"] == "Junie"
    stayed = pane_by_key(after, (True, 6))
    assert stayed["title"] == "session-manager" and stayed.get("pane_command") is None
    assert stayed is not found

    def refuse(call, text):
        try:
            call()
        except AssertionError as error:
            if text not in str(error):
                raise
        else:
            raise AssertionError(("typed pane accepted", text))

    duplicate = [plugin, after_terminal, dict(after_terminal)]
    refuse(lambda: pane_by_key(duplicate, (False, 6)), "(False, 6)")
    refuse(lambda: pane_by_key([plugin], (False, 6)), "(False, 6)")
    refuse(lambda: replaced_terminal(plugin, after), "terminal kind")
    moved = dict(after_terminal)
    moved["pane_cwd"] = "/elsewhere"
    refuse(lambda: replaced_terminal(before, [plugin, moved]), "pane cwd")
    unchanged = dict(before)
    refuse(lambda: replaced_terminal(before, [plugin, unchanged]), "pane argv")
    print("quickcmd_authority_pty: typed pane controls passed")


HANDOFF_READY = b"ready\n"
HANDOFF_ACK = b"go\n"


def read_exact_token(fd, token, kind):
    """Read one exact token. EOF and a mismatched byte are different failures."""
    buf = b""
    while len(buf) < len(token):
        chunk = os.read(fd, len(token) - len(buf))
        if not chunk:
            raise AssertionError((kind + " eof", buf))
        buf += chunk
        if not token.startswith(buf):
            raise AssertionError((kind + " malformed", buf))
    return buf


def child_establish_group(ready_w, ack_r):
    """Create this process's own group, then wait for the parent ACK before exec."""
    os.setpgrp()
    os.write(ready_w, HANDOFF_READY)
    os.close(ready_w)
    try:
        got = read_exact_token(ack_r, HANDOFF_ACK, "ack")
    finally:
        try:
            os.close(ack_r)
        except OSError:
            pass
    if got != HANDOFF_ACK:
        raise AssertionError(("ack malformed", got))


def parent_adopt_foreground(pid, ready_r, ack_w, tty_fd):
    """Foreground the child only after its ready token and its own pgid.

    ``getpgid`` before ready is the race: it still names the parent group.
    """
    try:
        got = read_exact_token(ready_r, HANDOFF_READY, "ready")
    finally:
        try:
            os.close(ready_r)
        except OSError:
            pass
    if got != HANDOFF_READY:
        raise AssertionError(("ready malformed", got))
    pgid = os.getpgid(pid)
    if pgid != pid:
        raise AssertionError(("foreign pgid", pgid, pid))
    try:
        os.tcsetpgrp(tty_fd, pgid)
        foreground = os.tcgetpgrp(tty_fd)
    except OSError as error:
        raise AssertionError(("tc foreground", error.errno))
    if foreground != pid:
        raise AssertionError(("tc foreground", foreground, pid))
    os.write(ack_w, HANDOFF_ACK)
    os.close(ack_w)


def argv_holder_source(executable):
    """Standalone pane shell. The child execs junie only after the foreground ACK."""
    return (
        "#!%s\n" % executable
        + "import os, sys, time, signal\n"
        + "# Stay at the layout cwd, step to the parent of the repo directory,\n"
        + "# then step into the repo directory. The second step is the cwd\n"
        + "# the drawer can observe after the host records the earlier one.\n"
        + "time.sleep(0.4)\n"
        + "os.chdir(os.path.dirname(os.environ['VC_FIXTURE_REPO']))\n"
        + "print('FIXTURE_ARGV cwd-step', os.getcwd(), flush=True)\n"
        + "time.sleep(0.4)\n"
        + "os.chdir(os.environ['VC_FIXTURE_REPO'])\n"
        + "print(os.environ['VC_FIXTURE_NOTICE'], flush=True)\n"
        + "print('FIXTURE_ARGV cwd', os.getcwd(), flush=True)\n"
        + "ready_r, ready_w = os.pipe()\n"
        + "ack_r, ack_w = os.pipe()\n"
        + "pid = os.fork()\n"
        + "if pid == 0:\n"
        + "    os.close(ready_r)\n"
        + "    os.close(ack_w)\n"
        + "    try:\n"
        + "        os.setpgrp()\n"
        + "        os.write(ready_w, b'ready\\n')\n"
        + "        os.close(ready_w)\n"
        + "        ack = b''\n"
        + "        while len(ack) < 3:\n"
        + "            chunk = os.read(ack_r, 3 - len(ack))\n"
        + "            if not chunk:\n"
        + "                break\n"
        + "            ack += chunk\n"
        + "        os.close(ack_r)\n"
        + "        if ack != b'go\\n':\n"
        + "            os._exit(2)\n"
        + "        os.execv(sys.executable, [sys.executable, os.environ['VC_FIXTURE_JUNIE']])\n"
        + "    except OSError:\n"
        + "        os._exit(3)\n"
        + "    os._exit(3)\n"
        + "os.close(ready_w)\n"
        + "os.close(ack_r)\n"
        + "open_fds = [ready_r, ack_w]\n"
        + "def handoff_fail(message):\n"
        + "    while open_fds:\n"
        + "        fd = open_fds.pop()\n"
        + "        try:\n"
        + "            os.close(fd)\n"
        + "        except OSError:\n"
        + "            pass\n"
        + "    try:\n"
        + "        os.kill(pid, signal.SIGKILL)\n"
        + "    except OSError:\n"
        + "        pass\n"
        + "    try:\n"
        + "        os.waitpid(pid, 0)\n"
        + "    except OSError:\n"
        + "        pass\n"
        + "    print('FIXTURE_ARGV handoff-failed', message, flush=True)\n"
        + "    raise SystemExit(1)\n"
        + "ready = b''\n"
        + "while len(ready) < 6:\n"
        + "    try:\n"
        + "        chunk = os.read(ready_r, 6 - len(ready))\n"
        + "    except OSError as error:\n"
        + "        handoff_fail(error)\n"
        + "    if not chunk:\n"
        + "        handoff_fail('ready eof')\n"
        + "    ready += chunk\n"
        + "    if not b'ready\\n'.startswith(ready):\n"
        + "        handoff_fail('ready malformed')\n"
        + "open_fds.remove(ready_r)\n"
        + "os.close(ready_r)\n"
        + "try:\n"
        + "    pgid = os.getpgid(pid)\n"
        + "except OSError as error:\n"
        + "    handoff_fail(error)\n"
        + "if pgid != pid:\n"
        + "    handoff_fail('foreign pgid')\n"
        + "try:\n"
        + "    tty = sys.stdout.fileno()\n"
        + "    os.tcsetpgrp(tty, pgid)\n"
        + "    foreground = os.tcgetpgrp(tty)\n"
        + "except OSError as error:\n"
        + "    handoff_fail(error)\n"
        + "if foreground != pgid:\n"
        + "    handoff_fail('tc foreground')\n"
        + "os.write(ack_w, b'go\\n')\n"
        + "open_fds.remove(ack_w)\n"
        + "os.close(ack_w)\n"
        + "print('FIXTURE_ARGV holder-waiting', flush=True)\n"
        + "while True:\n"
        + "    try:\n"
        + "        os.waitpid(pid, 0)\n"
        + "        break\n"
        + "    except InterruptedError:\n"
        + "        continue\n"
        + "print('FIXTURE_ARGV holder-child-exited', flush=True)\n"
        + "signal.pause()\n"
    )


def _reap_own_child(pid):
    if not pid:
        return
    try:
        os.kill(pid, signal.SIGKILL)
    except OSError:
        pass
    try:
        os.waitpid(pid, 0)
    except ChildProcessError:
        pass


def _close_fds(fds):
    for fd in fds:
        if isinstance(fd, int) and fd >= 0:
            try:
                os.close(fd)
            except OSError:
                pass


def _in_private_tty(fn):
    """Run fn(slave) in a private session. The worker terminal stays untouched."""
    out_r, out_w = os.pipe()
    outer = os.fork()
    if outer == 0:
        os.close(out_r)
        master = slave = -1
        try:
            os.setsid()
            master, slave = os.openpty()
            fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
            fn(slave)
            os.write(out_w, b"ok\n")
        except BaseException as error:
            text = ("%s: %s" % (type(error).__name__, error)).encode()[:300]
            try:
                os.write(out_w, b"err\n" + text)
            except OSError:
                pass
        finally:
            _close_fds((master, slave, out_w))
            os._exit(0)
    os.close(out_w)
    data = b""
    while True:
        chunk = os.read(out_r, 1024)
        if not chunk:
            break
        data += chunk
    os.close(out_r)
    _reap_own_child(outer)
    if data.startswith(b"ok\n"):
        return
    raise AssertionError(("private tty", data[:300]))


def _forced_parent_first_groups():
    """Old order: parent getpgid before the child setpgrp names the parent group."""
    hold_r, hold_w = os.pipe()
    done_r, done_w = os.pipe()
    release_r, release_w = os.pipe()
    pid = os.fork()
    if pid == 0:
        try:
            _close_fds((hold_w, done_r, release_w))
            os.read(hold_r, 1)
            os.close(hold_r)
            os.setpgrp()
            os.write(done_w, b"1")
            os.close(done_w)
            os.read(release_r, 1)
            os.close(release_r)
        finally:
            os._exit(0)
    _close_fds((hold_r, done_w, release_r))
    try:
        before = os.getpgid(pid)
        parent = os.getpgrp()
        if before != parent or before == pid:
            raise AssertionError(("parent-first", before, parent, pid))
        os.write(hold_w, b"x")
        os.close(hold_w)
        hold_w = -1
        if os.read(done_r, 1) != b"1":
            raise AssertionError(("parent-first done",))
        os.close(done_r)
        done_r = -1
        after = os.getpgid(pid)
        if after != pid or after == parent:
            raise AssertionError(("after setpgrp", after, pid, parent))
        os.write(release_w, b"y")
        os.close(release_w)
        release_w = -1
        child = pid
        waited, status = os.waitpid(child, 0)
        pid = 0
        if waited != child or not os.WIFEXITED(status) or os.WEXITSTATUS(status) != 0:
            raise AssertionError(("parent-first status", status))
    finally:
        _close_fds((hold_w, done_r, release_w))
        _reap_own_child(pid)


def run_foreground_handoff_controls():
    """Process-group handoff. No Frame and no product PTY.

    Installed ``a096f62aa5`` left terminal Junie 6 on the holder command.
    The generated holder called getpgid before the child setpgrp. A private
    session where the parent wins that race keeps the parent group foreground.
    """
    source = argv_holder_source(sys.executable)
    ready_at = source.index("b'ready\\n'")
    pgid_at = source.index("os.getpgid(pid)")
    assert ready_at < pgid_at
    assert "os.tcsetpgrp(sys.stdout.fileno(), os.getpgid(pid))" not in source
    assert "os.killpg" not in source and "killpg" not in source
    for token in ("ready eof", "ready malformed", "foreign pgid", "tc foreground"):
        assert token in source
    assert "os.execv(sys.executable, [sys.executable, os.environ['VC_FIXTURE_JUNIE']])" in source
    assert source.count("time.sleep(0.4)") == 2

    _forced_parent_first_groups()

    def parent_keeps_foreground(slave):
        hold_r, hold_w = os.pipe()
        done_r, done_w = os.pipe()
        release_r, release_w = os.pipe()
        pid = os.fork()
        if pid == 0:
            try:
                _close_fds((hold_w, done_r, release_w))
                os.read(hold_r, 1)
                os.close(hold_r)
                os.setpgrp()
                os.write(done_w, b"1")
                os.close(done_w)
                os.read(release_r, 1)
                os.close(release_r)
            finally:
                os._exit(0)
        _close_fds((hold_r, done_w, release_r))
        try:
            sampled = os.getpgid(pid)
            parent = os.getpgrp()
            if sampled != parent or sampled == pid:
                raise AssertionError(("tty parent-first", sampled, parent, pid))
            os.tcsetpgrp(slave, sampled)
            os.write(hold_w, b"x")
            os.close(hold_w)
            hold_w = -1
            if os.read(done_r, 1) != b"1":
                raise AssertionError(("tty parent-first done",))
            os.close(done_r)
            done_r = -1
            after = os.getpgid(pid)
            foreground = os.tcgetpgrp(slave)
            if after != pid or foreground != parent or foreground == pid:
                raise AssertionError(("tty foreground stayed", after, foreground, parent, pid))
            os.write(release_w, b"y")
            os.close(release_w)
            release_w = -1
            os.waitpid(pid, 0)
            pid = 0
        finally:
            _close_fds((hold_w, done_r, release_w))
            _reap_own_child(pid)

    def handshake(slave):
        ready_r, ready_w = os.pipe()
        ack_r, ack_w = os.pipe()
        report_r, report_w = os.pipe()
        pid = os.fork()
        if pid == 0:
            try:
                _close_fds((ready_r, ack_w, report_r))
                child_establish_group(ready_w, ack_r)
                os.set_inheritable(report_w, True)
                os.write(report_w, str(os.getpid()).encode() + b"\n")
                code = "import os; os.write(%d, str(os.getpid()).encode() + b'\\n')" % report_w
                os.execv(sys.executable, [sys.executable, "-c", code])
            finally:
                os._exit(9)
        _close_fds((ready_w, ack_r, report_w))
        try:
            parent_adopt_foreground(pid, ready_r, ack_w, slave)
            ready_r = ack_w = -1

            def readline():
                buf = b""
                while not buf.endswith(b"\n"):
                    chunk = os.read(report_r, 64)
                    if not chunk:
                        break
                    buf += chunk
                return buf

            before = readline()
            after = readline()
            if before != after or before != str(pid).encode() + b"\n":
                raise AssertionError(("exec pid", before, after, pid))
            os.close(report_r)
            report_r = -1
            waited, status = os.waitpid(pid, 0)
            if waited != pid or not os.WIFEXITED(status) or os.WEXITSTATUS(status) != 0:
                raise AssertionError(("handshake status", status))
            pid = 0
        finally:
            _close_fds((ready_r, ack_w, report_r))
            _reap_own_child(pid)

    def refuse_case(slave, child_fn, text):
        ready_r, ready_w = os.pipe()
        ack_r, ack_w = os.pipe()
        bogus_r, bogus_w = os.pipe()
        pid = os.fork()
        if pid == 0:
            try:
                _close_fds((ready_r, ack_w, bogus_r, bogus_w))
                child_fn(ready_w, ack_r)
            finally:
                os._exit(0)
        _close_fds((ready_w, ack_r))
        tty_fd = bogus_r if text == "tc foreground" else slave
        try:
            try:
                parent_adopt_foreground(pid, ready_r, ack_w, tty_fd)
            except AssertionError as error:
                if text not in str(error):
                    raise
            else:
                raise AssertionError(("handoff accepted", text))
            ready_r = -1
        finally:
            _close_fds((ready_r, ack_w, bogus_r, bogus_w))
            _reap_own_child(pid)

    def child_missing(ready_w, ack_r):
        _close_fds((ready_w, ack_r))

    def child_malformed(ready_w, ack_r):
        os.write(ready_w, b"nope\n")
        _close_fds((ready_w,))
        os.read(ack_r, 1)
        os.close(ack_r)

    def child_foreign(ready_w, ack_r):
        os.write(ready_w, HANDOFF_READY)
        os.close(ready_w)
        os.read(ack_r, 1)
        os.close(ack_r)

    def child_tty_refusal(ready_w, ack_r):
        child_establish_group(ready_w, ack_r)

    _in_private_tty(parent_keeps_foreground)
    _in_private_tty(handshake)
    _in_private_tty(lambda slave: refuse_case(slave, child_missing, "ready eof"))
    _in_private_tty(lambda slave: refuse_case(slave, child_malformed, "ready malformed"))
    _in_private_tty(lambda slave: refuse_case(slave, child_foreign, "foreign pgid"))
    _in_private_tty(lambda slave: refuse_case(slave, child_tty_refusal, "tc foreground"))
    print("quickcmd_authority_pty: foreground handoff controls passed")


def main():
    import pyte
    import pyte.screens

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--scratch", type=Path, required=True)
    parser.add_argument("--modes", nargs="+", choices=("tab", "normal", "locked", "normal-after-B-A", "click-tab"),
                        default=("tab", "normal", "locked", "normal-after-B-A", "click-tab"))
    parser.add_argument("--allow-dirty", action="store_true", help="diagnostic build only")
    parser.add_argument("--repeat", type=int, default=1)
    parser.add_argument("--resize-burst", type=int, default=0,
                        help="bounded real SIGWINCH event burst before each action")
    parser.add_argument("--max-ready-seconds", type=float)
    parser.add_argument("--context-layers", action="store_true",
                        help="also exercise Quick cmd -> Panels -> details -> same Quick cmd")
    parser.add_argument("--panels-contract", action="store_true",
                        help="exercise narrow/repeated-open/native-close inventory contract")
    parser.add_argument("--frontend-fixture", type=Path,
                        help="private three product-owned startup commands, never user argv")
    args = parser.parse_args()
    if args.panels_contract and (not args.context_layers or not args.frontend_fixture):
        parser.error("--panels-contract requires --context-layers and --frontend-fixture")
    columns = 167 if args.panels_contract else 120
    terminal_rows = 45 if args.panels_contract else 30
    assert 1 <= args.repeat <= 10 and 0 <= args.resize_burst <= 100
    args.modes = list(args.modes) * args.repeat
    args.output.mkdir(parents=True, exist_ok=False)
    args.scratch.mkdir(parents=True, exist_ok=False)
    binary = str(args.binary.resolve())
    session = f"qca{os.getpid()}"
    env = {k: os.environ[k] for k in ("PATH", "USER", "LANG") if k in os.environ}
    env.update(TERM="xterm-256color", SHELL="/bin/sh", VC_FRAME_SERVER_FOREGROUND="1",
               VC_FRAME_ROUTE_DIAGNOSTICS="1")
    for key, directory in {
        # macOS ProjectDirs ignores XDG paths. A subprocess-only fixture home
        # keeps even startup migration and --build-info away from user state.
        "HOME": "home", "TMPDIR": "tmp", "XDG_CONFIG_HOME": "config", "XDG_CACHE_HOME": "cache",
        "XDG_DATA_HOME": "data", "XDG_RUNTIME_DIR": "runtime", "XDG_STATE_HOME": "state",
        "VIBECRAFTED_HOME": "vc", "VC_FRAME_SOCKET_DIR": "s",
    }.items():
        path = args.scratch / directory
        path.mkdir()
        env[key] = str(path)
    env["VIBECRAFTED_CONTROL_PLANE"] = str(args.scratch / "no-control-plane.json")
    workload = args.scratch / "workload.py"
    events = args.output / "workloads.jsonl"
    workload.write_text(
        f"#!{sys.executable}\nimport os,sys,json\n"
        f"f=os.open({str(events)!r},os.O_WRONLY|os.O_CREAT|os.O_APPEND,0o600)\n"
        "def event(kind,value):\n"
        " os.write(f,(json.dumps(dict(pid=os.getpid(),kind=kind,value=value))+'\\n').encode())\n"
        "event('start',os.environ.get('ZELLIJ_PANE_ID'))\n"
        "print('WORKLOAD_PID='+str(os.getpid()),flush=True)\n"
        "for line in sys.stdin:\n"
        " event('input',line.rstrip());print('RECEIVED:'+line.rstrip(),flush=True)\n"
    )
    workload.chmod(0o700)
    if args.panels_contract:
        frontends = args.scratch / "frontends"
        frontends.mkdir()
        env["VC_FRAME_CONFIG_DIR"] = str(frontends)
        env["FIXTURE_PYTHON"] = sys.executable
        frontend = (f"#!{sys.executable}\nimport signal\n"
                    "print('OWNED FRONTEND FIXTURE', flush=True)\nsignal.pause()\n")
        for name in ("vc-start-here.py", "vc-agent-workshop.py"):
            (frontends / name).write_text(frontend)
        runner = frontends / "pane-python"
        runner.write_text('#!/bin/sh\nexec "$FIXTURE_PYTHON" "$@"\n')
        runner.chmod(0o700)
        private_bin = args.scratch / "bin"
        private_bin.mkdir()
        voc = private_bin / "vibecrafted"
        voc.write_text('#!/bin/sh\n[ "$1" = tui ] || exit 97\nexec "$FIXTURE_PYTHON" "' + str(frontends / "vc-agent-workshop.py") + '"\n')
        voc.chmod(0o700)
        env["PATH"] = str(private_bin) + os.pathsep + env.get("PATH", "")
        fixture = json.loads(args.frontend_fixture.read_text())
        assert len(fixture["rows"]) == 3
        panes = []
        for row in fixture["rows"]:
            command = row["terminal_command"]
            if command.startswith("bash -lc "):
                executable, option, body = "bash", "-lc", command[9:]
            elif command.startswith("sh -c "):
                executable, option, body = "sh", "-c", command[6:]
            else:
                raise AssertionError("unrecognized owned fixture command vector")
            panes.append('pane command=' + json.dumps(executable) + ' name=' + json.dumps(row["title"], ensure_ascii=False) + ' { args ' + json.dumps(option) + ' ' + json.dumps(body, ensure_ascii=False) + '; }')
        for title in ("Panels", "Composer"):
            panes.append('pane command=' + json.dumps(str(workload)) + ' name=' + json.dumps(title) + ';')
        # Private argv fixture. The holder is the pane shell. Its child is the
        # discovered command: basename junie, then the same child execs basename
        # codex with --model m1. The pane title stays "Junie". These scripts do
        # not start a provider; FIXTURE_NOTICE says so.
        fixture_cwd = args.scratch / "fx"
        fixture_live = fixture_cwd / "seen" / "live"
        fixture_live.mkdir(parents=True)
        swap_trigger = args.scratch / "argv-swap"
        junie_script = private_bin / "junie"
        codex_script = private_bin / "codex"
        holder_script = private_bin / "argv-fixture-holder"
        env["VC_FIXTURE_NOTICE"] = discovery.FIXTURE_NOTICE
        env["VC_FIXTURE_ARGV_SWAP"] = str(swap_trigger)
        env["VC_FIXTURE_JUNIE"] = str(junie_script)
        env["VC_FIXTURE_CODEX"] = str(codex_script)
        env["VC_FIXTURE_REPO"] = str(fixture_live)
        junie_script.write_text(
            f"#!{sys.executable}\n"
            "import os, sys, time\n"
            "print(os.environ['VC_FIXTURE_NOTICE'], flush=True)\n"
            "print('FIXTURE_ARGV junie', flush=True)\n"
            "trigger = os.environ['VC_FIXTURE_ARGV_SWAP']\n"
            "while not os.path.exists(trigger):\n"
            "    time.sleep(0.05)\n"
            "print('FIXTURE_ARGV swap', flush=True)\n"
            "codex = os.environ['VC_FIXTURE_CODEX']\n"
            "os.execv(sys.executable, [sys.executable, codex, '--model', 'm1'])\n"
        )
        codex_script.write_text(
            f"#!{sys.executable}\n"
            "import os, signal\n"
            "print(os.environ['VC_FIXTURE_NOTICE'], flush=True)\n"
            "print('FIXTURE_ARGV codex', flush=True)\n"
            "signal.pause()\n"
        )
        holder_script.write_text(argv_holder_source(sys.executable))
        for path in (junie_script, codex_script, holder_script):
            path.chmod(0o700)
        panes.append(
            'pane command=' + json.dumps(str(holder_script))
            + ' name="Junie" cwd=' + json.dumps(str(fixture_cwd)) + ';'
        )
        assert fixture_live.name == discovery.CWD_BASENAME
        junie_label = discovery.JUNIE_LABEL
        codex_label = discovery.CODEX_LABEL
        panes.append('pane name="Files" { plugin location="zellij:strider"; }')
        frontend_layout = args.scratch / "frontends.kdl"
        frontend_layout.write_text('layout {\n' + '\n'.join(panes) + '\n}\n')
    config = args.scratch / "config.kdl"
    config.write_text(
        f'default_shell "{workload}"\nshow_startup_tips false\n'
        'show_release_notes false\nsession_serialization false\ndefault_mode "normal"\n'
        # Keep mode isolation observable beyond the default 30-second idle
        # lock. This fixture tests action ownership, not the idle timer.
        'auto_lock_after_seconds 0\n'
        'plugins { compact-bar location="zellij:compact-bar"; }\n'
        'keybinds { shared { bind "Super Shift ." { '
        'MessagePlugin "compact-bar" { name "vc_quick_cmd"; }; }; }; }\n'
    )
    repo = Path(__file__).resolve().parents[2]
    layout = args.scratch / "layout.kdl"
    template = (repo / "zellij-utils/assets/layouts/default.kdl").read_text()
    if args.panels_contract:
        fixture_tabs = '\n tab name="A" { pane name="Lena fork"; }\n tab name="B" {\n' + '\n'.join(panes) + '\n}\n}\n'
    else:
        fixture_tabs = '\n tab name="A" { pane; }\n tab name="B" { pane; }\n}\n'
    layout.write_text(template[:template.rfind("}")] + fixture_tabs)
    (args.output / "config.kdl").write_text(config.read_text())
    (args.output / "layout.kdl").write_text(layout.read_text())
    receipt = {
        "binary": binary, "sha256": hashlib.sha256(args.binary.read_bytes()).hexdigest(),
        "session": session, "scratch": str(args.scratch), "steps": [],
        "shortcut_hex": "1b5b34363b313075",  # Kitty CSI-u: '.' + Shift + Super
        "requested_modes": args.modes,
        "limits": ["No physical macOS key event", "A/B are tabs, not host/guest sessions"],
    }
    receipt["build_info"] = json.loads(subprocess.check_output([binary, "--build-info"], env=env, timeout=20))
    assert args.allow_dirty or not receipt["build_info"]["git_dirty"], "requires a committed binary"
    children = []

    # Match Frame's documented width convention; never patch rendered text.
    original_width = pyte.screens.wcwidth
    pyte.screens.wcwidth = lambda char: 1 if char == "𝌁" else original_width(char)

    class Terminal(pyte.Screen):
        def write_process_input(self, data):
            os.write(self.fd, data.encode())

        def report_device_status(self, mode, private=False):
            if private and mode == 6:
                self.write_process_input(f"\x1b[?{self.cursor.y + 1};{self.cursor.x + 1}R")
            else:
                super().report_device_status(mode)

    def launch(create):
        pid, fd = pty.fork()
        if pid == 0:
            argv = [binary, "--config", str(config)]
            if create:
                argv += ["--new-session-with-layout", str(layout), "--session", session]
            else:
                argv += ["attach", session]
            os.execve(binary, argv, env)
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", terminal_rows, columns, 0, 0))
        screen = Terminal(columns, terminal_rows)
        screen.fd = fd
        child = dict(pid=pid, fd=fd, screen=screen, stream=pyte.Stream(screen),
                     decoder=codecs.getincrementaldecoder("utf-8")("replace"),
                     raw=(args.output / f"client-{len(children)}.ansi").open("wb"))
        children.append(child)
        return child

    def drain(seconds=0.2):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            ready, _, _ = select.select([c["fd"] for c in children], [], [], 0.05)
            for child in children:
                if child["fd"] not in ready:
                    continue
                try:
                    data = os.read(child["fd"], 65536)
                except OSError:
                    continue
                child["raw"].write(data)
                child["raw"].flush()
                child["stream"].feed(child["decoder"].decode(data))

    def mode_visible(child, mode):
        top, bottom = child["screen"].display[0], child["screen"].display[-1]
        if mode == "normal":
            return "▷ N" in top and all(word in bottom for word in ("LOCK", "PANE", "TAB", "RESIZE"))
        if mode == "tab":
            return "𝌁 T" in top and "Break out" in bottom
        if mode == "locked":
            return "⚿ L" in top and "LOCK" in bottom and "PANE" not in bottom
        raise ValueError(mode)

    def wait(predicate, name, timeout=30):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            drain()
            if predicate():
                return
        snapshot(name + "-timeout")
        raise AssertionError(name)

    def snapshot(name):
        for i, child in enumerate(children):
            (args.output / f"{name}-client-{i}.txt").write_text("\n".join(child["screen"].display))

    def cli(*command, timeout=10, allow=(0,), client_id=None):
        result = subprocess.run(
            [binary, *session_cli_args(session, command, client_id)],
            env=env, capture_output=True, text=True, timeout=timeout)
        assert result.returncode in allow, (command, result.returncode, result.stderr, result.stdout)
        return result.stdout

    def inventory(name):
        data = json.loads(cli("action", "list-panes", "--all", "--json"))
        (args.output / f"{name}-panes.json").write_text(json.dumps(data, indent=2))
        return data

    def chrome(panes):
        rows = [p for p in panes if p["is_plugin"] and p["title"] != "Panels" and
                (p.get("plugin_url") or "").split(":")[-1] in
                ("compact-bar", "session-manager", "status-bar")]
        for kind in ("compact-bar", "session-manager", "status-bar"):
            instances = [p for p in rows if p["plugin_url"].split(":")[-1] == kind]
            assert len(instances) == 2, (kind, instances)
            assert len({p["plugin_runtime_id"] for p in instances}) == 1
            assert all(not p["exited"] and not p["is_suppressed"] for p in instances)
            if kind != "session-manager":
                assert all(p["pane_rows"] == 1 for p in instances)
        return sorted((p["id"], p["plugin_runtime_id"], p["tab_id"]) for p in rows)

    try:
        first = launch(True)
        wait(lambda: "WORKLOAD_PID=" in "\n".join(first["screen"].display)
             and "PANE" in first["screen"].display[-1], "bootstrap", 60)
        # This substrate starts one default tab even with an explicit startup
        # layout. Materialize the two-tab fixture through the real session API.
        initial_tabs = json.loads(cli("action", "list-tabs", "--all", "--json"))
        receipt["startup_tabs"] = initial_tabs
        if len(initial_tabs) == 1:
            cli("action", "rename-tab", "A")
            if args.panels_contract:
                cli("action", "rename-pane", "Lena fork")
                cli("action", "new-tab", "--name", "B", "--layout", str(frontend_layout))
            else:
                cli("action", "new-tab", "--name", "B")
            # One interactive client. Unscoped GoToTab is legal until the peer attaches.
            cli("action", "go-to-tab", "1")
            wait(lambda: "◉ A" in first["screen"].display[0], "two-tab-fixture")
        first_clients = cli("action", "list-clients")
        first["client_id"] = sole_client_id(first_clients)
        active = launch(False)
        wait(lambda: "WORKLOAD_PID=" in "\n".join(active["screen"].display)
             and mode_visible(active, "normal"), "second-client", 60)
        before_clients = cli("action", "list-clients")
        receipt["clients_before"] = before_clients
        active["client_id"] = client_id_that_appeared(first_clients, before_clients)
        receipt["attended_client_ids"] = {
            "first": first["client_id"],
            "active": active["client_id"],
        }
        receipt["client_pids"] = [child["pid"] for child in children]
        receipt["panes_before"] = inventory("before")
        if args.panels_contract:
            frontend_hashes = []
            for row in fixture["rows"]:
                actual = [pane for pane in receipt["panes_before"]
                          if not pane["is_plugin"] and pane["title"] == row["title"]]
                assert len(actual) == 1 and actual[0]["terminal_command"] == row["terminal_command"], row["title"]
                frontend_hashes.append({"title": row["title"], "id": actual[0]["id"],
                                        "sha256": hashlib.sha256(actual[0]["terminal_command"].encode()).hexdigest()})
            assert len([pane for pane in receipt["panes_before"] if pane["is_plugin"]
                        and pane["plugin_url"] == "vc-frame:strider"]) == 1
            receipt["product_frontend_command_hashes"] = frontend_hashes
        chrome_before = chrome(receipt["panes_before"])
        terminal_ids = {p["id"] for p in receipt["panes_before"] if not p["is_plugin"]}
        receipt["tabs_before"] = json.loads(cli("action", "list-tabs", "--all", "--json"))
        agent_pid = int(re.search(r"WORKLOAD_PID=(\d+)", "\n".join(active["screen"].display))[1])
        receipt["agent_pid"] = agent_pid
        if args.panels_contract:
            # Source-prepared private scenario. It runs only with the installed
            # binary the caller passes. Visibility is inventory, not a title search.
            origin = next(tab for tab in receipt["tabs_before"] if tab["name"] == "A")
            destination = next(tab for tab in receipt["tabs_before"] if tab["name"] == "B")
            workload_pane = next(pane for pane in receipt["panes_before"]
                                 if not pane["is_plugin"] and pane["title"] == "Lena fork")
            seed_script = args.scratch / "seed-float.py"
            seed_script.write_text(
                f"#!{sys.executable}\nimport signal\n"
                f"print({SEED_MARKER!r}, flush=True)\nsignal.pause()\n"
            )
            seed_script.chmod(0o700)

            def returned_id(text, prefix):
                found = re.findall(rf"{prefix}_(\d+)", text)
                assert len(found) == 1, text
                return int(found[0])

            def click_chip(label):
                wait(lambda: label in active["screen"].display[0], f"chip-{label}")
                column = active["screen"].display[0].index(label) + 2
                os.write(active["fd"], f"\x1b[<0;{column};1M\x1b[<0;{column};1m".encode())

            def hide_tab(tab_id):
                # 0 = state changed, 2 = already hidden. 1 (missing tab) fails.
                cli("action", "hide-floating-panes", "--tab-id", str(tab_id), allow=(0, 2))

            def focus_existing(token, child):
                # Exit 0 is a fresh focus. Exit 2 is the public LogError for a
                # pane this client already focuses. Classify that outcome once.
                # Do not retry, and do not read list-clients after exit 0.
                routed = session_cli_args(
                    session, ("action", "focus-pane-id", token), child["client_id"])
                result = subprocess.run(
                    [binary, *routed],
                    env=env, capture_output=True, text=True, timeout=30)
                clients = "" if result.returncode == 0 else cli("action", "list-clients")
                idempotent_focus_outcome(
                    result.returncode, result.stdout, result.stderr,
                    token, clients, child["client_id"])

            def rendered():
                return "\n".join(active["screen"].display + first["screen"].display)

            def tab_by_id(tab_id):
                tabs = json.loads(cli("action", "list-tabs", "--all", "--json"))
                return next(tab for tab in tabs if tab["tab_id"] == tab_id)

            seed_ids = []
            for title in SEED_TITLES:
                token = cli("action", "new-pane", "--floating", "--name", title,
                            "--tab-id", str(origin["tab_id"]), "--", str(seed_script), timeout=30)
                seed_id = returned_id(token, "terminal")
                cli("action", "rename-pane", "--pane-id", f"terminal_{seed_id}", title)
                seed_ids.append(seed_id)
            assert len(set(seed_ids)) == 2, seed_ids
            seeded = inventory("float-seeds-open")
            for title, seed_id in zip(SEED_TITLES, seed_ids):
                pane = pane_by_key(seeded, (False, seed_id))
                assert pane["title"] == title and pane["is_floating"] and not pane["is_suppressed"], pane
                assert pane["tab_id"] == origin["tab_id"], pane
            seed_keys = [(False, seed_id) for seed_id in seed_ids]
            # The seed buffer must contain the marker while the layer is open.
            # hide-floating-panes then clears individual conceal and hides the
            # layer, so is_suppressed stays false. The public evidence is the
            # tab flag plus the marker leaving the grid.
            wait(lambda: SEED_MARKER in rendered(), "seed-marker-visible")
            hide_tab(origin["tab_id"])

            def layer_hidden():
                if SEED_MARKER in rendered():
                    return False
                return tab_by_id(origin["tab_id"]).get("are_floating_panes_visible") is False

            wait(layer_hidden, "float-layer-hidden")
            concealed = inventory("float-seeds-hidden")
            assert_layer_hidden(
                concealed, tab_by_id(origin["tab_id"]), seed_keys, rendered(), SEED_MARKER)

            click_chip("Quick cmd")
            def quick_open():
                panes = inventory("float-quick-open")
                opened = [pane for pane in panes if not pane["is_plugin"]
                          and pane["title"] == QUICK_CMD_TITLE and not pane["is_suppressed"]]
                return len(opened) == 1 and opened[0]["is_focused"] and opened[0]["is_floating"]
            wait(quick_open, "quick-cmd-chip")
            after_quick = inventory("float-quick")
            quick_matches = [pane for pane in after_quick if not pane["is_plugin"]
                             and pane["title"] == QUICK_CMD_TITLE]
            assert len(quick_matches) == 1, quick_matches
            quick_id = quick_matches[0]["id"]
            assert quick_id not in seed_ids and quick_id not in terminal_ids
            # new_floating_pane calls activate_only. A hidden layer opens for
            # this pane and conceals every other float. Seeds must be that
            # individual conceal, not merely absent from a still-hidden layer.
            assert_float_visibility(
                after_quick, [(False, quick_id)], (False, quick_id), seed_keys)
            if SEED_MARKER in rendered():
                raise AssertionError(("seed marker rendered after quick cmd", rendered()))
            snapshot("float-quick")

            before_composer = hidden_float_keys(after_quick)
            click_chip("Composer")
            def composer_open():
                panes = inventory("float-composer-open")
                opened = [pane for pane in panes if not pane["is_plugin"]
                          and pane["title"] == COMPOSER_TITLE and not pane["is_suppressed"]]
                return len(opened) == 1 and opened[0]["is_focused"] and opened[0]["is_floating"]
            wait(composer_open, "composer-chip")
            after_composer = inventory("float-composer")
            composer_matches = [pane for pane in after_composer if not pane["is_plugin"]
                                and pane["title"] == COMPOSER_TITLE]
            assert len(composer_matches) == 1, composer_matches
            composer_id = composer_matches[0]["id"]
            assert composer_id not in seed_ids and composer_id != quick_id
            quick_after = pane_by_key(after_composer, (False, quick_id))
            quick_key = (False, quick_id)
            # prepare_context_layer suppresses competing context titles.
            # suppress_pane moves that pane into suppressed_panes. Seeds are
            # not context layers and stay individually concealed floats.
            # A missing manifest leaves Quick cmd as the same floating pane.
            # Either representation must be this id, title, and tab.
            if quick_after.get("title") != QUICK_CMD_TITLE or quick_after.get("exited"):
                raise AssertionError(("quick cmd lost its identity", quick_after))
            if quick_after.get("tab_id") != origin["tab_id"]:
                raise AssertionError(("quick cmd left its tab", quick_after))
            stay_hidden = set(seed_keys)
            stay_hidden |= before_composer - {(False, composer_id), quick_key}
            visible = [(False, composer_id)]
            context_suppressed = []
            floating_context = []
            if quick_after.get("is_floating") is True and quick_after.get("is_suppressed") is False:
                visible.append(quick_key)
                floating_context.append(quick_key)
            elif (
                quick_after.get("is_floating") is False
                and quick_after.get("is_suppressed") is True
                and quick_after.get("is_focused") is False
            ):
                context_suppressed.append((quick_key, QUICK_CMD_TITLE, origin["tab_id"]))
            else:
                raise AssertionError(("quick cmd left both legal representations", quick_after))
            assert_float_visibility(
                after_composer, visible, (False, composer_id), sorted(stay_hidden))
            assert_context_suppressed(after_composer, context_suppressed)
            if SEED_MARKER in rendered():
                raise AssertionError(("seed marker rendered after composer", rendered()))
            assert chrome(after_composer) == chrome_before
            assert len([row for row in cli("action", "list-clients").splitlines()[1:] if row.strip()]) == 2
            snapshot("float-composer")

            launched = cli("action", "launch-plugin", "--floating", "--tab-id", str(origin["tab_id"]),
                           ABOUT_PLUGIN_URL, timeout=30)
            plugin_id = returned_id(launched, "plugin")
            cli("action", "rename-pane", "--pane-id", f"plugin_{plugin_id}", ABOUT_FIXTURE_TITLE)
            renamed = pane_by_key(inventory("float-plugin-renamed"), (True, plugin_id))
            assert renamed["title"] == ABOUT_FIXTURE_TITLE, renamed
            assert renamed["plugin_url"] == ABOUT_PLUGIN_URL, renamed
            cli("action", "go-to-tab", str(destination["position"] + 1),
                client_id=active["client_id"])
            moved = cli(
                "action", "launch-or-focus-plugin", "--floating", "--move-to-focused-tab",
                ABOUT_PLUGIN_URL, timeout=30, client_id=active["client_id"])
            assert returned_id(moved, "plugin") == plugin_id, moved
            moved_panes = inventory("float-plugin-moved")
            moved_plugin = pane_by_key(moved_panes, (True, plugin_id))
            assert moved_plugin["tab_id"] == destination["tab_id"], moved_plugin
            assert moved_plugin["title"] == ABOUT_FIXTURE_TITLE, moved_plugin
            assert len([pane for pane in moved_panes if pane.get("plugin_url") == ABOUT_PLUGIN_URL]) == 1
            hide_tab(origin["tab_id"])
            hide_tab(destination["tab_id"])

            def both_layers_hidden():
                if SEED_MARKER in rendered():
                    return False
                origin_tab = tab_by_id(origin["tab_id"])
                destination_tab = tab_by_id(destination["tab_id"])
                return (
                    origin_tab.get("are_floating_panes_visible") is False
                    and destination_tab.get("are_floating_panes_visible") is False
                )

            wait(both_layers_hidden, "plugin-layers-hidden")
            hidden_plugin = inventory("float-plugin-hidden")
            # Layer hide does not pull a suppressed context pane back into
            # the floating collection. Seeds and Composer are that layer.
            # Quick cmd stays on whichever representation Composer left.
            origin_floats = seed_keys + [(False, composer_id)] + floating_context
            assert_layer_hidden(
                hidden_plugin, tab_by_id(origin["tab_id"]),
                origin_floats, rendered(), SEED_MARKER)
            assert_context_suppressed(hidden_plugin, context_suppressed)
            assert_layer_hidden(
                hidden_plugin, tab_by_id(destination["tab_id"]),
                [(True, plugin_id)], rendered(), SEED_MARKER)
            # FocusPaneWithId returns without revealing a pane that is already
            # the client focus. Point focus at the tiled workload first so the
            # following id selects the plugin and is the reveal. The plugin is
            # not individually suppressed: the whole destination layer is hidden.
            focus_existing(f"terminal_{workload_pane['id']}", active)
            # Selector is the plugin id. DrawerCommand::Focus sends the same
            # FocusPaneWithId(should_float_if_hidden=true) instruction; a title
            # match is not the producer.
            cli("action", "focus-pane-id", f"plugin_{plugin_id}", timeout=30,
                client_id=active["client_id"])
            def plugin_focused():
                panes = inventory("float-plugin-focus")
                pane = pane_by_key(panes, (True, plugin_id))
                return pane["is_focused"] and not pane["is_suppressed"] and pane["title"] == ABOUT_FIXTURE_TITLE
            wait(plugin_focused, "plugin-focused-by-id")
            focused_panes = inventory("float-plugin-focused")
            chrome_urls = {"compact-bar", "session-manager", "status-bar"}
            destination_others = []
            for pane in focused_panes:
                if pane.get("tab_id") != destination["tab_id"] or pane.get("exited"):
                    continue
                if not pane.get("is_floating") or pane_key(pane) == (True, plugin_id):
                    continue
                url = (pane.get("plugin_url") or "").split(":")[-1]
                if pane.get("is_plugin") and url in chrome_urls and pane.get("title") != "Panels":
                    continue
                destination_others.append(pane_key(pane))
            # Same-tab floats opened with this focus must be individually
            # concealed. Seeds live on the other tab: hide cleared their
            # conceal bit, so they stay hidden only while that layer stays shut.
            assert_float_visibility(
                focused_panes, [(True, plugin_id)], (True, plugin_id), sorted(destination_others))
            assert_not_revealed(
                focused_panes, tab_by_id(origin["tab_id"]),
                origin_floats, rendered(), SEED_MARKER)
            assert_context_suppressed(focused_panes, context_suppressed)
            focused_plugin = pane_by_key(focused_panes, (True, plugin_id))
            assert focused_plugin["tab_id"] == destination["tab_id"], focused_plugin
            assert chrome(focused_panes) == chrome_before
            tabs_now = json.loads(cli("action", "list-tabs", "--all", "--json"))

            def tab_identity(tabs):
                return sorted((tab["tab_id"], tab["tab_instance_id"], tab["session_incarnation"])
                              for tab in tabs)

            assert tab_identity(tabs_now) == tab_identity(receipt["tabs_before"])
            assert len([row for row in cli("action", "list-clients").splitlines()[1:] if row.strip()]) == 2
            snapshot("float-plugin")
            receipt["float_visibility"] = {
                "seed_ids": seed_ids,
                "quick_cmd_id": quick_id,
                "composer_id": composer_id,
                "plugin_id": plugin_id,
                "plugin_url": ABOUT_PLUGIN_URL,
                "plugin_title": ABOUT_FIXTURE_TITLE,
                "origin_tab_id": origin["tab_id"],
                "destination_tab_id": destination["tab_id"],
                "quick_cmd_producer": "compact-bar chip click on visible Quick cmd",
                "composer_producer": "compact-bar chip click on visible Composer",
                "plugin_move_producer": "action launch-or-focus-plugin --floating --move-to-focused-tab",
                "plugin_focus_producer": "action focus-pane-id plugin_<id>",
                "plugin_focus_kernel": "FocusPaneWithId should_float_if_hidden=true; same instruction as Panels DrawerCommand::Focus via show_pane_with_id",
                "layer_hide_field": "list-tabs TabInfo.are_floating_panes_visible",
                "layer_hide_marker": SEED_MARKER,
                "individual_conceal_field": "list-panes is_suppressed on a pane that stays is_floating",
                "context_suppress_field": "suppressed_panes projection: is_suppressed true, is_floating false, is_focused false",
                "layer_hide_projection": "hide-floating-panes clears conceal and sets show_panes false; is_suppressed stays false",
                "context_suppress_projection": "prepare_context_layer suppress_pane moves a competing context pane out of the floating collection",
                "float_multiple_panes": "not invoked",
            }
            for kind, pane_id in (
                ("terminal", quick_id), ("terminal", composer_id),
                ("plugin", plugin_id), ("terminal", seed_ids[0]), ("terminal", seed_ids[1]),
            ):
                cli("action", "close-pane", "--pane-id", f"{kind}_{pane_id}")
            def scenario_closed():
                panes = inventory("float-scenario-closed")
                closed_keys = {(False, quick_id), (False, composer_id), (True, plugin_id), *seed_keys}
                return not any(pane_key(pane) in closed_keys for pane in panes)
            wait(scenario_closed, "float-scenario-closed")
            cli("action", "go-to-tab", str(origin["position"] + 1),
                client_id=active["client_id"])
            focus_existing(f"terminal_{workload_pane['id']}", active)
            wait(lambda: mode_visible(active, "normal") and mode_visible(first, "normal"),
                 "float-scenario-restored-normal")
            restored = inventory("float-scenario-restored")
            assert {pane["id"] for pane in restored if not pane["is_plugin"]} == terminal_ids
            assert chrome(restored) == chrome_before
        # Deliberately keep the peer in TAB so an accidental all-client Normal
        # transition is observable. Its mode must remain unchanged throughout.
        os.write(first["fd"], b"\x14")
        wait(lambda: mode_visible(first, "tab"), "peer-tab-mode")
        snapshot("before")
        drain(2)  # ordinary warmed session; identical before/after conditions

        for index, mode in enumerate(args.modes):
            if mode == "normal-after-B-A":
                for position in (2, 1):
                    os.write(active["fd"], b"\x14")
                    drain(1)
                    os.write(active["fd"], str(position).encode())
                    current_tabs = json.loads(cli("action", "list-tabs", "--all", "--json"))
                    target = next(tab for tab in current_tabs if tab["position"] == position - 1)
                    baseline_tab = next(tab for tab in receipt["tabs_before"] if tab["tab_id"] == target["tab_id"])
                    assert target["tab_instance_id"] == baseline_tab["tab_instance_id"]
                    expected_tab = target["name"]
                    wait(lambda: mode_visible(active, "normal")
                         and f"◉ {expected_tab}" in active["screen"].display[0], f"switch-{position}")
                    snapshot(f"switch-{position}")
            elif mode in ("tab", "click-tab"):
                os.write(active["fd"], b"\x14")
                wait(lambda: mode_visible(active, "tab"), "tab-mode")
            elif mode == "locked":
                os.write(active["fd"], b"\x07")
                wait(lambda: mode_visible(active, "locked"), "locked-mode")
            for burst_index in range(args.resize_burst):
                burst_columns = columns - 1 if burst_index % 2 == 0 else columns
                fcntl.ioctl(active["fd"], termios.TIOCSWINSZ,
                            struct.pack("HHHH", terminal_rows, burst_columns, 0, 0))
                os.kill(active["pid"], signal.SIGWINCH)
                drain(0.02)
            if args.resize_burst:
                fcntl.ioctl(active["fd"], termios.TIOCSWINSZ, struct.pack("HHHH", terminal_rows, columns, 0, 0))
                os.kill(active["pid"], signal.SIGWINCH)
            if mode == "click-tab":
                wait(lambda: "Quick cmd" in active["screen"].display[0], "click-target-visible")
            snapshot(f"{index}-{mode}-before")
            opened_at = time.monotonic()
            if mode == "click-tab":
                column = active["screen"].display[0].index("Quick cmd") + 2
                os.write(active["fd"], f"\x1b[<0;{column};1M\x1b[<0;{column};1m".encode())
            else:
                os.write(active["fd"], bytes.fromhex(receipt["shortcut_hex"]))
            # Measure rendered input readiness before diagnostic CLI round trips.
            wait(lambda: "❯_ Quick cmd" in "\n".join(active["screen"].display[1:-1])
                 and mode_visible(active, "normal")
                 and mode_visible(first, "tab"), f"{mode}-ready")
            ready_seconds = time.monotonic() - opened_at
            marker = args.output / f"{index}-command-executed"
            command = f"printf QC_EXECUTED_{index}; printf ok > {shlex.quote(str(marker))}\r"
            os.write(active["fd"], command.encode())
            wait(lambda: marker.exists(), f"{mode}-usable-command")
            assert marker.read_text() == "ok"
            executed_seconds = time.monotonic() - opened_at
            deadline = time.monotonic() + 30
            while True:
                drain(0.5)
                opened = inventory(f"{index}-open")
                new_panes = [p for p in opened if pane_key(p)[0] is False and pane_key(p)[1] not in terminal_ids]
                if new_panes or time.monotonic() >= deadline:
                    break
            snapshot(f"{index}-{mode}-opened")
            assert chrome(opened) == chrome_before
            assert len(new_panes) == 1 and new_panes[0]["is_floating"], new_panes
            clients_open = cli("action", "list-clients")
            origin_id = str(attended_client_id(clients_open, active["client_id"]))
            origin_row = next(row.split() for row in clients_open.splitlines()[1:]
                              if row.split()[0] == origin_id)
            assert origin_row[1] == f"terminal_{new_panes[0]['id']}", "origin selection lost"
            receipt["steps"].append({"mode": mode, "panes_open": opened,
                                      "clients_open": clients_open, "origin_client_id": origin_id,
                                      "ready_seconds": ready_seconds,
                                      "executed_seconds": executed_seconds,
                                      "resize_burst": args.resize_burst})
            wait(lambda: "❯_ Quick cmd" in "\n".join(active["screen"].display[1:-1]),
                 f"{mode}-rendered-command")
            snapshot(f"{index}-{mode}-rendered")
            # Pane title and mode chrome arrive on separate render updates.
            # Preserve the first frame, then require both client projections
            # to converge without injecting any input-mode action.
            wait(lambda: mode_visible(active, "normal")
                 and mode_visible(first, "tab"), f"{mode}-input-mode-projections")
            snapshot(f"{index}-{mode}-input-ready")
            receipt["steps"][-1]["peer_mode"] = "Tab"
            receipt["steps"][-1]["open_seconds"] = time.monotonic() - opened_at
            expected_pipes = sum(step["mode"] != "click-tab" for step in receipt["steps"])
            def completed_pipe():
                routes = json.loads(cli("action", "doctor-routes", "--json"))
                receipt["steps"][-1]["routes"] = routes
                metric = routes["metrics"].get("interactive:KeybindPipe", {})
                return metric.get("count", 0) == expected_pipes
            wait(completed_pipe, f"{mode}-pipe-completion")
            for name, metric in receipt["steps"][-1]["routes"]["metrics"].items():
                if name in ("interactive:KeybindPipe", "plugin:SwitchToMode",
                            "plugin:NewFloatingPane", "plugin:RenameTerminalPane"):
                    assert metric["timeouts"] == 0 and metric["failures"] == 0, (name, metric)
            snapshot(f"{index}-{mode}-executed")
            if args.context_layers and index == len(args.modes) - 1:
                quick_id = new_panes[0]["id"]
                wait(lambda: "Panels" in active["screen"].display[0], "panels-chip")
                column = active["screen"].display[0].index("Panels") + 2
                os.write(active["fd"], f"\x1b[<0;{column};1M\x1b[<0;{column};1m".encode())
                wait(lambda: "d details" in "\n".join(active["screen"].display[1:-1]),
                     "panels-open")
                def quick_hidden():
                    panes = inventory("context-panels")
                    return (any(pane_key(p) == (False, quick_id)
                                and p["is_suppressed"] and not p["exited"] for p in panes)
                            and any(p["is_plugin"] and p["title"] == "Panels"
                                    and p["is_floating"] and not p["is_suppressed"] for p in panes))
                wait(quick_hidden, "quick-suppressed-with-process-preserved")
                snapshot("context-panels")
                if args.panels_contract:
                    def drawer_pane():
                        return next(p for p in inventory("panels-contract")
                                    if p["is_plugin"] and p["title"] == "Panels")

                    def drawer_open():
                        pane = drawer_pane()
                        return pane["is_floating"] and not pane["is_suppressed"]

                    def drawer_slice():
                        pane = drawer_pane()
                        return "\n".join(
                            line[pane["pane_x"]:pane["pane_x"] + pane["pane_columns"]]
                            for line in active["screen"].display[pane["pane_y"]:pane["pane_y"] + pane["pane_rows"]]
                        )

                    def click(column, row):
                        os.write(active["fd"], f"\x1b[<0;{column + 1};{row + 1}M\x1b[<0;{column + 1};{row + 1}m".encode())

                    def click_panels():
                        top = active["screen"].display[0]
                        assert "Panels" in top
                        click(top.index("Panels") + 2, 0)

                    def check_geometry(expected_columns, expected_rows):
                        pane = drawer_pane()
                        assert pane["pane_x"] + pane["pane_columns"] == expected_columns, pane
                        assert pane["pane_columns"] == expected_columns * 24 // 100, pane
                        assert pane["pane_y"] <= 2 and pane["pane_y"] + pane["pane_rows"] >= expected_rows - 1, pane
                        assert pane["is_focused"] and drawer_open(), "drawer must be focused"
                        return pane

                    pane = check_geometry(columns, terminal_rows)
                    # Lena fork stays: its shell is workload.py, which is not an
                    # interactive agent, so the drawer falls back to the pane title.
                    # Codex fork is not a row. The fixture pane's title is Junie;
                    # the row must be the argv projection, not that title.
                    wait(lambda: junie_label in drawer_slice(), "panels-junie-fixture-label")
                    content = drawer_slice()
                    for title in ("Launchpad", "Lena fork", "Composer", "Quick cmd"):
                        assert title in content, (title, content)
                    assert junie_label in content, (junie_label, content)
                    assert "Codex fork" not in content, content
                    assert "Junie" not in content, content
                    for title in ("Agent Workspaces", "Voc · Host console", "Files", "Strider", "Links", "Tab titles", "Sessions"):
                        assert title not in content, (title, content)
                    assert "PIN" not in content
                    fixture_rows = [p for p in inventory("panels-fixture-junie")
                                    if not p["is_plugin"] and p["title"] == "Junie"]
                    assert len(fixture_rows) == 1, fixture_rows
                    fixture_id = fixture_rows[0]["id"]
                    assert not fixture_rows[0]["exited"]
                    junie_inventory = content
                    initial_drawer_id = pane["id"]
                    click_panels()
                    drain(1)
                    assert pane_key(check_geometry(columns, terminal_rows)) == (True, initial_drawer_id)
                    assert len([p for p in inventory("panels-repeated-open") if p["is_plugin"] and p["title"] == "Panels"]) == 1
                    # Reopen restores product geometry after a manual change.
                    cli("action", "change-floating-pane-coordinates", "--pane-id", f"plugin_{initial_drawer_id}", "--x", "40%", "--width", "58%")
                    click_panels()
                    wait(lambda: drawer_pane()["pane_columns"] == columns * 24 // 100, "panels-reopen-restores-narrow")
                    # One client resize exercises the viewport with its session rail.
                    geometry = []
                    for width, height in ((240, 55), (90, 36), (167, 45)):
                        active["screen"].resize(height, width)
                        fcntl.ioctl(active["fd"], termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
                        os.kill(active["pid"], signal.SIGWINCH)
                        # The peer constrains shared geometry; resize its fixture too.
                        first["screen"].resize(height, width)
                        fcntl.ioctl(first["fd"], termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
                        os.kill(first["pid"], signal.SIGWINCH)
                        wait(lambda width=width: drawer_pane()["pane_x"] + drawer_pane()["pane_columns"] == width
                             and drawer_pane()["pane_columns"] == width * 24 // 100,
                             f"panels-right-edge-{width}")
                        geometry.append(check_geometry(width, height))
                    pane = drawer_pane()
                    x_column = pane["pane_x"] + pane["pane_columns"] - 3
                    wait(lambda pane=pane, x_column=x_column: active["screen"].display[pane["pane_y"]][x_column] == "x", "painted-native-x")
                    before_close = inventory("panels-before-native-close")
                    click(x_column, pane["pane_y"])
                    wait(lambda: not drawer_open(), "native-x-hides-drawer")
                    after_close = inventory("panels-after-native-close")
                    assert {pane_key(p) for p in before_close} == {pane_key(p) for p in after_close}, "x closed another pane"
                    assert all(not p["exited"] for p in after_close if not p["is_plugin"])
                    click_panels()
                    wait(drawer_open, "panels-opens-after-native-close")
                    check_geometry(columns, terminal_rows)
                    wait(lambda: junie_label in drawer_slice(), "panels-junie-label-restored")
                    swap_trigger.write_text("fixture\n")

                    def argv_swapped():
                        text = drawer_slice()
                        return codex_label in text and junie_label not in text

                    wait(argv_swapped, "panels-codex-same-pane-argv")
                    content = drawer_slice()
                    assert "Junie" not in content, content
                    assert "Codex fork" not in content, content
                    swapped = replaced_terminal(fixture_rows[0], inventory("panels-fixture-codex"))
                    receipt["panels_discovery"] = {
                        "notice": discovery.FIXTURE_NOTICE,
                        "proves_provider_started": False,
                        "cwd": str(fixture_live),
                        "cwd_basename": discovery.CWD_BASENAME,
                        "stale_title": discovery.STALE_TITLE,
                        "state": discovery.STATE,
                        "pane_id": fixture_id,
                        "junie_label": junie_label,
                        "codex_label": codex_label,
                        "junie_inventory": junie_inventory,
                        "launch_command_before": fixture_rows[0].get("terminal_command"),
                        "launch_command_after": swapped.get("terminal_command"),
                    }
                    receipt["panels_uncovered"] = [
                        "list-panes PaneInfo.terminal_command is the launch command and has no live argv field",
                        "FloatMultiplePanes is plugin-only; no public CLI calls it, so native execution is not a step",
                        "screen_tests cover hidden-float kernel behavior in source only",
                    ]
                    receipt["panels_contract"] = {"drawer_id": initial_drawer_id, "geometry": geometry,
                                                   "native_x_hid_only_drawer": True, "user_inventory": content}
                os.write(active["fd"], b"d")
                wait(lambda: "Details" in "\n".join(active["screen"].display[1:-1]),
                     "panel-details-expanded")
                snapshot("context-details")
                os.write(active["fd"], b"\x1b")
                wait(lambda: "d details" not in "\n".join(active["screen"].display[1:-1]),
                     "panels-dismissed")
                os.write(active["fd"], bytes.fromhex(receipt["shortcut_hex"]))
                wait(lambda: "❯_ Quick cmd" in "\n".join(active["screen"].display[1:-1]),
                     "same-quick-restored")
                def geometry_restored():
                    return any(pane_key(p) == (False, quick_id)
                               and not p["is_suppressed"]
                               and (p["pane_y"], p["pane_rows"]) == (
                                   new_panes[0]["pane_y"], new_panes[0]["pane_rows"])
                               for p in inventory("context-restoring"))
                wait(geometry_restored, "quick-footprint-restored")
                restored = inventory("context-restored")
                quicks = [p for p in restored if pane_key(p)[0] is False and pane_key(p)[1] not in terminal_ids]
                assert len(quicks) == 1 and pane_key(quicks[0]) == (False, quick_id) and not quicks[0]["is_suppressed"]
                assert (quicks[0]["pane_y"], quicks[0]["pane_rows"]) == (
                    new_panes[0]["pane_y"], new_panes[0]["pane_rows"]), "restored tool moved over content headers"
                assert all(p["is_suppressed"] for p in restored
                           if p["is_plugin"] and p["title"] == "Panels")
                resumed_marker = args.output / "context-shell-resumed"
                os.write(active["fd"], f"printf resumed > {shlex.quote(str(resumed_marker))}\r".encode())
                wait(resumed_marker.exists, "restored-shell-accepts-input")
                assert resumed_marker.read_text() == "resumed"
                snapshot("context-restored")
                receipt["context_layers"] = {"quick_id": quick_id, "preserved": True,
                                             "panes": restored}
            # Close this command shell's floating pane via the standard pane keys.
            os.write(active["fd"], b"\x10")
            wait(lambda: "Fullscreen" in active["screen"].display[-1], "pane-mode-for-dismissal")
            os.write(active["fd"], b"x")
            wait(lambda: "WORKLOAD_PID=" in "\n".join(active["screen"].display)
                 and mode_visible(active, "normal"), f"{mode}-dismissed")
            snapshot(f"{index}-{mode}-dismissed")
            closed = inventory(f"{index}-closed")
            assert chrome(closed) == chrome_before
            assert {p["id"] for p in closed if not p["is_plugin"]} == terminal_ids
        receipt["clients_after"] = cli("action", "list-clients")
        assert receipt["clients_after"] == before_clients
        for child in children:
            assert os.waitpid(child["pid"], os.WNOHANG) == (0, 0), "client exited during shortcut sequence"
        receipt["panes_after"] = inventory("after")
        assert chrome(receipt["panes_after"]) == chrome_before
        receipt["tabs_after"] = json.loads(cli("action", "list-tabs", "--all", "--json"))
        identity = lambda tabs: sorted((t["tab_id"], t["tab_instance_id"], t["session_incarnation"]) for t in tabs)
        assert identity(receipt["tabs_after"]) == identity(receipt["tabs_before"])
        os.write(active["fd"], b"AGENT_ALIVE\r")
        wait(lambda: "RECEIVED:AGENT_ALIVE" in "\n".join(active["screen"].display), "agent-still-responsive")
        snapshot("agent-still-responsive")
        recorded_events = list(map(json.loads, events.read_text().splitlines()))
        assert [e for e in recorded_events if e["kind"] == "input"] == [
            {"pid": agent_pid, "kind": "input", "value": "AGENT_ALIVE"}
        ], "command input leaked or the original agent process was replaced"
        for event in recorded_events:
            os.kill(event["pid"], 0)
        if args.max_ready_seconds is not None:
            assert all(step["ready_seconds"] <= args.max_ready_seconds for step in receipt["steps"]), (
                "readiness target exceeded", [step["ready_seconds"] for step in receipt["steps"]])
        receipt["status"] = "passed_bounded_pty_scenario"
    except Exception as error:
        receipt.update(status="failed", error=repr(error))
        snapshot("failure")
        raise
    finally:
        # Preserve route latency/failure and raw server receipts even on a
        # failed assertion. Never turn a timeout into a successful scenario.
        try:
            receipt["routes"] = json.loads(cli("action", "doctor-routes", "--json"))
        except Exception as error:
            receipt["routes_error"] = repr(error)
        logs = Path(f"/tmp/vc-frame-{os.getuid()}/vc-frame-log") / session
        for log in logs.glob("*"):
            if log.is_file():
                (args.output / ("server-" + log.name)).write_bytes(log.read_bytes())
        receipt_path = args.output / "receipt.json"
        receipt_path.write_text(json.dumps(receipt, indent=2))
        if children:
            try:
                result = subprocess.run([binary, "kill-session", session, "--yes"], env=env,
                                        capture_output=True, text=True, timeout=10)
                receipt["cleanup"] = {"code": result.returncode, "stderr": result.stderr}
            except subprocess.TimeoutExpired:
                receipt["cleanup"] = {"error": "own session shutdown timed out"}
        for child in children:
            # Holding the PTY master keeps owned clients in kernel PTY wait
            # after kill-session returns 0. Close first, then reap.
            try:
                os.close(child["fd"])
            except OSError:
                pass
            child["fd"] = -1
            child["raw"].close()
        for child in children:
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                try:
                    if os.waitpid(child["pid"], os.WNOHANG)[0]:
                        break
                except ChildProcessError:
                    break
                time.sleep(0.1)
            else:
                try:
                    os.kill(child["pid"], signal.SIGKILL)
                except ProcessLookupError:
                    pass
                # A macOS process in kernel exit can outlive SIGKILL. Never
                # lose the scenario receipt to an unbounded blocking waitpid.
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    try:
                        if os.waitpid(child["pid"], os.WNOHANG)[0]:
                            break
                    except ChildProcessError:
                        break
                    time.sleep(0.1)
                else:
                    receipt.setdefault("unreaped_owned_children", []).append(child["pid"])
        for log in logs.glob("*"):
            if log.is_file():
                (args.output / ("server-" + log.name)).write_bytes(log.read_bytes())
        receipt_path.write_text(json.dumps(receipt, indent=2))


if __name__ == "__main__":
    if sys.argv[1:2] == ["--visibility-controls"]:
        run_visibility_controls()
    else:
        main()
