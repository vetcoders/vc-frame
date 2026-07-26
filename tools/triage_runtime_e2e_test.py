#!/usr/bin/env python3

import importlib.util
import json
import pathlib
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


MODULE_PATH = pathlib.Path(__file__).parents[1] / "scripts" / "triage-runtime-e2e.py"
SPEC = importlib.util.spec_from_file_location("triage_runtime_e2e", MODULE_PATH)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)

SHA = "0123456789abcdef0123456789abcdef01234567"


def completed(
    returncode: int,
    *,
    stdout: str = "",
    stderr: str = "",
) -> subprocess.CompletedProcess[str]:
    return subprocess.CompletedProcess(
        args=["vc-frame"],
        returncode=returncode,
        stdout=stdout,
        stderr=stderr,
    )


class ProvenanceTests(unittest.TestCase):
    def test_exact_clean_profile_matched_build_is_accepted(self) -> None:
        MODULE.validate_build_info(
            {
                "product": "vc-frame",
                "git_sha": SHA,
                "git_dirty": False,
                "profile": "debug",
            },
            expected_sha=SHA,
            expected_profile="debug",
        )

    def test_each_provenance_mismatch_fails_closed(self) -> None:
        valid = {
            "product": "vc-frame",
            "git_sha": SHA,
            "git_dirty": False,
            "profile": "debug",
        }
        cases = {
            "product": {**valid, "product": "zellij"},
            "git_sha": {**valid, "git_sha": "f" * 40},
            "git_dirty": {**valid, "git_dirty": True},
            "profile": {**valid, "profile": "release"},
        }
        for field, build_info in cases.items():
            with self.subTest(field=field), self.assertRaises(AssertionError):
                MODULE.validate_build_info(
                    build_info,
                    expected_sha=SHA,
                    expected_profile="debug",
                )

    def test_sha_must_be_full_hex(self) -> None:
        for value in ("abc", "g" * 40, SHA + "0"):
            with (
                self.subTest(value=value),
                self.assertRaisesRegex(AssertionError, "40-character"),
            ):
                MODULE.validate_sha(value, "test SHA")

    @mock.patch.object(MODULE.subprocess, "run")
    def test_current_checkout_sha_rejects_dirty_tree(self, run: mock.Mock) -> None:
        run.side_effect = [
            completed(0, stdout="/checkout\n"),
            completed(0, stdout=" M tracked.txt\n"),
        ]
        with self.assertRaisesRegex(AssertionError, "dirty tree"):
            MODULE.current_checkout_sha(pathlib.Path("/checkout"))

    @mock.patch.object(MODULE.subprocess, "run")
    def test_current_checkout_sha_resolves_only_after_clean_status(
        self, run: mock.Mock
    ) -> None:
        run.side_effect = [
            completed(0, stdout="/checkout\n"),
            completed(0, stdout=""),
            completed(0, stdout=f"{SHA}\n"),
        ]
        self.assertEqual(
            MODULE.current_checkout_sha(pathlib.Path("/checkout")),
            SHA,
        )
        self.assertIn("--untracked-files=all", run.call_args_list[1].args[0])


class SessionTruthTests(unittest.TestCase):
    @mock.patch.object(MODULE, "command")
    def test_session_inventory_preserves_live_and_exited_entries(
        self, command: mock.Mock
    ) -> None:
        command.return_value = completed(
            0,
            stdout=(
                "live [Created 1s ago] \n"
                "saved [Created 2s ago] (EXITED - attach to resurrect)\n"
            ),
        )
        self.assertEqual(
            MODULE.session_inventory(pathlib.Path("vc-frame"), {}),
            {"live": "live", "saved": "exited"},
        )

    @mock.patch.object(MODULE, "command")
    def test_session_inventory_command_failure_is_not_empty(
        self, command: mock.Mock
    ) -> None:
        command.return_value = completed(2, stderr="socket scan failed")
        with self.assertRaisesRegex(AssertionError, "cannot prove"):
            MODULE.session_inventory(pathlib.Path("vc-frame"), {})

    @mock.patch.object(MODULE, "session_inventory", return_value={})
    @mock.patch.object(MODULE, "command")
    def test_query_session_confirms_missing_only_after_inventory(
        self, command: mock.Mock, inventory: mock.Mock
    ) -> None:
        command.return_value = completed(1, stderr="not found")
        result = MODULE.query_session(pathlib.Path("vc-frame"), {}, "missing")
        self.assertEqual(result.state, "absent")
        self.assertEqual(result.inventory_state, "missing")
        inventory.assert_called_once()

    @mock.patch.object(MODULE, "session_inventory", return_value={"saved": "exited"})
    @mock.patch.object(MODULE, "command")
    def test_query_session_distinguishes_exited_inventory(
        self, command: mock.Mock, _inventory: mock.Mock
    ) -> None:
        command.return_value = completed(1, stderr="not running")
        result = MODULE.query_session(pathlib.Path("vc-frame"), {}, "saved")
        self.assertEqual(result.state, "absent")
        self.assertEqual(result.inventory_state, "exited")

    @mock.patch.object(MODULE, "session_inventory", return_value={"ambiguous": "live"})
    @mock.patch.object(MODULE, "command")
    def test_query_session_never_turns_live_command_failure_into_absence(
        self, command: mock.Mock, _inventory: mock.Mock
    ) -> None:
        command.return_value = completed(2, stderr="list-tabs timed out")
        with self.assertRaisesRegex(AssertionError, "active but list-tabs failed"):
            MODULE.query_session(pathlib.Path("vc-frame"), {}, "ambiguous")

    @mock.patch.object(MODULE, "command")
    def test_query_session_accepts_successful_live_inventory(
        self, command: mock.Mock
    ) -> None:
        command.return_value = completed(
            0,
            stdout=json.dumps([{"tab_id": 2, "name": "operator", "active": True}]),
        )
        result = MODULE.query_session(pathlib.Path("vc-frame"), {}, "live")
        self.assertEqual(result.state, "live")
        self.assertEqual(result.inventory_state, "live")
        self.assertEqual(result.tabs[0]["tab_id"], 2)

    def test_tab_state_requires_and_preserves_focus(self) -> None:
        tabs = [
            {"tab_id": 2, "name": "two", "active": False, "position": 1},
            {"tab_id": 1, "name": "one", "active": True, "position": 0},
        ]
        self.assertEqual(
            MODULE.tab_state(tabs),
            [
                {"active": True, "name": "one", "position": 0, "tab_id": 1},
                {"active": False, "name": "two", "position": 1, "tab_id": 2},
            ],
        )
        with self.assertRaisesRegex(AssertionError, "focus/selection"):
            MODULE.tab_state([{"tab_id": 1, "name": "one"}])


class EvidenceAndCleanupTests(unittest.TestCase):
    def test_artifact_snapshot_records_path_size_and_digest(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            artifact = root / "nested" / "receipt.json"
            artifact.parent.mkdir()
            artifact.write_bytes(b"proof")
            snapshot = MODULE.artifact_tree_snapshot(root)
            self.assertEqual(
                snapshot["files"],
                [
                    {
                        "path": "nested/receipt.json",
                        "bytes": 5,
                        "sha256": MODULE.sha256_bytes(b"proof"),
                    }
                ],
            )
            self.assertEqual(
                snapshot["digest"],
                MODULE.sha256_bytes(MODULE.canonical_json(snapshot["files"]).encode()),
            )

    def test_evidence_recorder_persists_every_transition(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = pathlib.Path(temporary) / "evidence.json"
            recorder = MODULE.EvidenceRecorder(path, {"status": "initializing"})
            recorder.set("status", "running")
            recorder.append("checks", {"name": "preflight", "exit": 0})
            self.assertEqual(
                json.loads(path.read_text(encoding="utf-8")),
                {
                    "status": "running",
                    "checks": [{"name": "preflight", "exit": 0}],
                },
            )
            self.assertFalse(path.with_name(".evidence.json.tmp").exists())

    def test_process_inventory_matches_exact_server_path_boundary(self) -> None:
        root = pathlib.Path("/tmp/vc-frame-proof/sockets")
        raw = "\n".join(
            [
                "101 /bin/vc-frame --server /tmp/vc-frame-proof/sockets/session-a",
                "102 /bin/vc-frame --server=/tmp/vc-frame-proof/sockets/session-b",
                "103 /bin/vc-frame --server /tmp/vc-frame-proof/sockets-neighbor/x",
                "104 /bin/sh -c 'echo --server /tmp/vc-frame-proof/sockets/fake'",
            ]
        )
        self.assertEqual(
            [entry["pid"] for entry in MODULE.server_processes_from_ps(raw, root)],
            [101, 102],
        )

    def test_write_error_has_exact_category(self) -> None:
        self.assertEqual(
            MODULE.write_error_category("Is a directory (os error 21)"),
            "destination_is_directory",
        )
        self.assertIsNone(MODULE.write_error_category("pane not found"))

    @mock.patch.object(MODULE, "command")
    def test_marker_assignment_does_not_assume_pane_id_order(
        self, command: mock.Mock
    ) -> None:
        def dump_screen(
            _binary: pathlib.Path,
            _env: dict[str, str],
            *arguments: str,
            **_kwargs: object,
        ) -> subprocess.CompletedProcess[str]:
            destination = pathlib.Path(arguments[arguments.index("--path") + 1])
            pane_id = int(arguments[arguments.index("--pane-id") + 1])
            destination.write_text(
                "TARGET-proof" if pane_id == 20 else "SIBLING-proof",
                encoding="utf-8",
            )
            return completed(0)

        command.side_effect = dump_screen
        with tempfile.TemporaryDirectory() as temporary:
            assignment = MODULE.wait_for_marker_assignment(
                pathlib.Path("vc-frame"),
                {},
                "origin",
                [10, 20],
                ["TARGET-proof", "SIBLING-proof"],
                pathlib.Path(temporary),
            )
        self.assertEqual(
            assignment,
            {"TARGET-proof": 20, "SIBLING-proof": 10},
        )

    def test_negative_probe_records_and_enforces_state_invariance(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            recorder = MODULE.EvidenceRecorder(
                pathlib.Path(temporary) / "evidence.json",
                {"negative_probes": []},
            )
            state = {"digest": "same", "state": {"tabs": []}}
            MODULE.record_negative_probe(
                recorder,
                scenario="missing",
                result=completed(2, stderr="missing"),
                before=state,
                after=state,
                error_category="missing",
            )
            self.assertTrue(recorder.data["negative_probes"][0]["state_unchanged"])
            with self.assertRaisesRegex(AssertionError, "changed tab focus"):
                MODULE.record_negative_probe(
                    recorder,
                    scenario="mutating_failure",
                    result=completed(2, stderr="failed"),
                    before=state,
                    after={"digest": "changed", "state": {"tabs": []}},
                    error_category="unexpected_mutation",
                )

    @mock.patch.object(MODULE, "session_inventory")
    def test_cleanup_refuses_unowned_inventory(self, inventory: mock.Mock) -> None:
        inventory.return_value = {"foreign": "live"}
        with self.assertRaisesRegex(AssertionError, "unexpected isolated sessions"):
            MODULE.cleanup_namespace(
                pathlib.Path("vc-frame"),
                {"VC_FRAME_SOCKET_DIR": "/tmp/proof/sockets"},
                {"owned"},
                timeout=0.01,
            )

    @mock.patch.object(MODULE, "wait_for_no_server_processes", return_value=[])
    @mock.patch.object(MODULE, "wait_for_session_gone")
    @mock.patch.object(MODULE, "query_session")
    @mock.patch.object(MODULE, "command")
    @mock.patch.object(MODULE, "session_inventory")
    def test_cleanup_kills_deletes_and_proves_exact_empty_inventory(
        self,
        inventory: mock.Mock,
        command: mock.Mock,
        query: mock.Mock,
        _wait_gone: mock.Mock,
        _wait_processes: mock.Mock,
    ) -> None:
        inventory.side_effect = [
            {"owned": "live"},
            {"owned": "exited"},
            {},
        ]
        query.return_value = MODULE.SessionQuery(
            state="live",
            tabs=[],
            list_tabs_exit=0,
            list_tabs_stderr="",
            inventory_state="live",
        )
        command.return_value = completed(0)
        receipt = MODULE.cleanup_namespace(
            pathlib.Path("vc-frame"),
            {"VC_FRAME_SOCKET_DIR": "/tmp/proof/sockets"},
            {"owned"},
            timeout=0.01,
        )
        self.assertEqual(receipt["final_session_inventory"], {})
        self.assertEqual(receipt["killed_sessions"], ["owned"])
        self.assertEqual(receipt["deleted_sessions"], ["owned"])
        self.assertEqual(
            [call.args[2:] for call in command.call_args_list],
            [
                ("kill-session", "owned"),
                ("delete-session", "owned", "--force"),
            ],
        )

    @mock.patch.object(MODULE, "query_session")
    @mock.patch.object(MODULE, "session_inventory")
    def test_cleanup_fails_on_ambiguous_live_query(
        self, inventory: mock.Mock, query: mock.Mock
    ) -> None:
        inventory.return_value = {"owned": "live"}
        query.side_effect = AssertionError("list-tabs timed out")
        with self.assertRaisesRegex(AssertionError, "list-tabs timed out"):
            MODULE.cleanup_namespace(
                pathlib.Path("vc-frame"),
                {"VC_FRAME_SOCKET_DIR": "/tmp/proof/sockets"},
                {"owned"},
                timeout=0.01,
            )


if __name__ == "__main__":
    unittest.main()
