#!/usr/bin/env python3

import errno
import importlib.util
import json
import os
import pathlib
import shlex
import signal
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
    def test_makefile_preserves_ci_artifact_root_and_explicit_override(self) -> None:
        repo_root = MODULE_PATH.parents[1]
        makefile = repo_root / "Makefile"
        probe = (
            f"include {makefile}\n"
            "print-triage-artifact-root:\n"
            "\t@printf '%s' '$(TRIAGE_RUNTIME_E2E_ARTIFACT_ROOT)'\n"
        )
        base_env = os.environ.copy()
        base_env.pop("VC_FRAME_E2E_ARTIFACT_ROOT", None)
        base_env.pop("TRIAGE_RUNTIME_E2E_ARTIFACT_ROOT", None)

        def resolve(**overrides: str) -> str:
            env = base_env | overrides
            result = subprocess.run(
                ["make", "-s", "-f", "-", "print-triage-artifact-root"],
                input=probe,
                cwd=repo_root,
                env=env,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            return result.stdout

        self.assertEqual(resolve(), "/tmp/vc-frame-triage-runtime-e2e")
        self.assertEqual(
            resolve(VC_FRAME_E2E_ARTIFACT_ROOT="/runner/evidence"),
            "/runner/evidence",
        )
        self.assertEqual(
            resolve(
                VC_FRAME_E2E_ARTIFACT_ROOT="/runner/evidence",
                TRIAGE_RUNTIME_E2E_ARTIFACT_ROOT="/operator/evidence",
            ),
            "/operator/evidence",
        )

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

    @mock.patch.object(MODULE, "command")
    def test_query_session_treats_successful_empty_inventory_as_ambiguity(
        self, command: mock.Mock
    ) -> None:
        command.return_value = completed(0, stdout="", stderr="")
        with self.assertRaisesRegex(
            MODULE.AmbiguousSessionError,
            "successful but invalid list-tabs inventory",
        ):
            MODULE.query_session(pathlib.Path("vc-frame"), {}, "starting")

    @mock.patch.object(MODULE, "command")
    def test_query_session_treats_successful_malformed_inventory_as_ambiguity(
        self, command: mock.Mock
    ) -> None:
        command.return_value = completed(0, stdout="{not-json", stderr="noise")
        with self.assertRaisesRegex(
            MODULE.AmbiguousSessionError,
            "successful but invalid list-tabs inventory",
        ):
            MODULE.query_session(pathlib.Path("vc-frame"), {}, "starting")

    @mock.patch.object(MODULE.time, "sleep")
    @mock.patch.object(MODULE, "session_tabs")
    def test_wait_for_tabs_retries_live_query_ambiguity_without_calling_it_absent(
        self, session_tabs: mock.Mock, _sleep: mock.Mock
    ) -> None:
        ready = [{"tab_id": 1, "name": "ready", "active": True}]
        session_tabs.side_effect = [
            MODULE.AmbiguousSessionError("startup race"),
            ready,
        ]
        self.assertEqual(
            MODULE.wait_for_tabs(pathlib.Path("vc-frame"), {}, "starting"),
            ready,
        )
        self.assertEqual(session_tabs.call_count, 2)

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

    def test_short_runtime_root_keeps_owned_socket_paths_within_limit(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vcf-e2e-", dir="/tmp") as temporary:
            root = pathlib.Path(temporary)
            proof = MODULE.socket_path_budget(
                root / "p" / "sockets",
                {"e1234567890-origin", "Finalized runs"},
            )
            self.assertLessEqual(
                int(proof["longest"]["bytes"]),
                MODULE.UNIX_SOCKET_PATH_LIMIT,
            )
            self.assertGreater(int(proof["remaining_bytes"]), 0)

    def test_socket_path_budget_fails_closed_before_overflow(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vcf-e2e-", dir="/tmp") as temporary:
            with self.assertRaisesRegex(AssertionError, "exceeds portable limit"):
                MODULE.socket_path_budget(
                    pathlib.Path(temporary) / "sockets",
                    {"s" * MODULE.UNIX_SOCKET_PATH_LIMIT},
                )

    def test_runtime_root_cleanup_refuses_symlink_before_resolution(self) -> None:
        target = pathlib.Path(tempfile.mkdtemp(prefix="vcf-e2e-target-", dir="/tmp"))
        link = target.parent / f"vcf-e2e-link-{os.getpid()}-{MODULE.time.time_ns()}"
        link.symlink_to(target, target_is_directory=True)
        try:
            with self.assertRaisesRegex(AssertionError, "symlink runtime root"):
                MODULE.remove_runtime_root(link)
            self.assertTrue(target.is_dir())
        finally:
            link.unlink(missing_ok=True)
            target.rmdir()

    def test_operator_guard_excludes_recursive_control_plane_archives(self) -> None:
        with mock.patch.dict(
            os.environ,
            {
                "HOME": "/tmp/operator",
                "VIBECRAFTED_CONTROL_PLANE": "/tmp/operator/multi-gig-control-plane",
            },
            clear=False,
        ):
            paths = MODULE.operator_guard_paths()
        self.assertNotIn(
            pathlib.Path("/tmp/operator/multi-gig-control-plane"),
            paths,
        )

    def test_operator_guard_receipt_is_compact_and_volatile_log_is_identity_only(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            log = root / "vc-frame-log" / "zellij.log"
            log.parent.mkdir()
            log.write_text("before", encoding="utf-8")
            before = MODULE.guarded_tree_snapshot(
                [root],
                volatile_files={log},
            )
            log.write_text("before + concurrent operator append", encoding="utf-8")
            after = MODULE.guarded_tree_snapshot(
                [root],
                volatile_files={log},
            )
            self.assertEqual(before, after)
            summary = MODULE.guarded_snapshot_summary(before)
            self.assertNotIn("entries", summary)
            self.assertEqual(summary["entry_count"], len(before["entries"]))
            log_entry = next(
                entry
                for entry in before["entries"]
                if entry.get("path") == "vc-frame-log/zellij.log"
            )
            self.assertEqual(log_entry["kind"], "volatile_file_identity")
            self.assertNotIn("sha256", log_entry)
            self.assertNotIn("bytes", log_entry)

    def test_operator_guard_marks_preexisting_session_metadata_volatile(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            home = root / "home"
            runtime_root = root / "tmp" / f"vc-frame-{os.getuid()}"
            live_metadata = (
                home
                / "Library"
                / "Caches"
                / "io.vetcoders.vc-frame"
                / MODULE.SOCKET_CONTRACT_DIRECTORY
                / "session_info"
                / "live session"
                / "session-metadata.kdl"
            )
            stale_metadata = live_metadata.parents[1] / "stale" / live_metadata.name
            live_metadata.parent.mkdir(parents=True)
            stale_metadata.parent.mkdir(parents=True)
            live_metadata.write_text("heartbeat=1", encoding="utf-8")
            stale_metadata.write_text("stale", encoding="utf-8")
            with mock.patch.dict(
                os.environ,
                {"HOME": str(home), "TMPDIR": str(root / "tmp")},
                clear=False,
            ):
                volatile = MODULE.operator_guard_volatile_paths()

            self.assertIn(live_metadata, volatile)
            self.assertIn(stale_metadata, volatile)
            self.assertIn(
                runtime_root.resolve() / "vc-frame-log" / "zellij.log",
                volatile,
            )

    def test_operator_guard_diff_reports_only_changed_path_summary(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            state = root / "state"
            state.write_text("before", encoding="utf-8")
            before = MODULE.guarded_tree_snapshot([root])
            state.write_text("after", encoding="utf-8")
            after = MODULE.guarded_tree_snapshot([root])
            difference = MODULE.guarded_snapshot_diff(before, after)
            self.assertEqual(difference["changed_count"], 1)
            self.assertEqual(
                difference["changed"],
                [{"root": str(root.resolve()), "path": "state"}],
            )
            self.assertNotIn("before", difference)
            self.assertNotIn("after", difference)

    def test_operator_guard_attributes_shared_runtime_drift_without_global_quiescence(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            runtime_root = (
                root / "Library" / "Caches" / "io.vetcoders.vc-frame"
            )
            runtime_root.mkdir(parents=True)
            before = MODULE.guarded_tree_snapshot([runtime_root])
            foreign = runtime_root / "foreign-incarnation" / "plugin-cache"
            foreign.parent.mkdir()
            foreign.write_text("foreign", encoding="utf-8")
            after = MODULE.guarded_tree_snapshot([runtime_root])

            attribution = MODULE.attribute_operator_guard_changes(
                before,
                after,
                {"fixture-123"},
            )

            self.assertTrue(attribution["safe"])
            self.assertEqual(
                attribution["concurrent_runtime_drift"]["count"],
                2,
            )
            self.assertEqual(attribution["fixture_attributed"]["count"], 0)

    def test_operator_guard_rejects_fixture_identity_in_shared_runtime_root(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            runtime_root = (
                root / "Library" / "Caches" / "io.vetcoders.vc-frame"
            )
            runtime_root.mkdir(parents=True)
            before = MODULE.guarded_tree_snapshot([runtime_root])
            leaked = runtime_root / "fixture-incarnation" / "plugin-cache"
            leaked.parent.mkdir()
            leaked.write_text("fixture", encoding="utf-8")
            after = MODULE.guarded_tree_snapshot([runtime_root])

            attribution = MODULE.attribute_operator_guard_changes(
                before,
                after,
                {"fixture-incarnation"},
            )

            self.assertFalse(attribution["safe"])
            self.assertEqual(attribution["fixture_attributed"]["count"], 2)

    def test_operator_guard_rejects_unattributed_durable_state_change(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            durable_root = root / ".config" / "vc-frame"
            durable_root.mkdir(parents=True)
            before = MODULE.guarded_tree_snapshot([durable_root])
            (durable_root / "config.kdl").write_text("changed", encoding="utf-8")
            after = MODULE.guarded_tree_snapshot([durable_root])

            attribution = MODULE.attribute_operator_guard_changes(
                before,
                after,
                {"fixture-123"},
            )

            self.assertFalse(attribution["safe"])
            self.assertEqual(
                attribution["unattributed_sensitive"]["count"],
                1,
            )

    def test_operator_guard_collects_fixture_runtime_identities(self) -> None:
        markers = MODULE.operator_guard_fixture_markers(
            "fixture-123",
            {
                "receipt": {
                    "session_incarnation": "incarnation-456",
                    "viewer_tab_identity": {
                        "tab_instance_id": "instance-789",
                    },
                }
            },
        )

        self.assertEqual(
            markers,
            {"fixture-123", "incarnation-456", "instance-789"},
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

    def test_runtime_transcript_manifest_binds_run_root_bytes_and_digest(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            transcript = root / "runtime.log"
            transcript.write_bytes(b"runtime proof")
            manifest_path = MODULE.write_runtime_transcript_manifest(
                transcript,
                run="run-1",
                ownership_root=root,
            )
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
            self.assertEqual(
                manifest,
                {
                    "version": 1,
                    "run_id": "run-1",
                    "transcript": str(transcript.resolve()),
                    "root": str(root.resolve()),
                    "bytes": 13,
                    "sha256": MODULE.sha256_bytes(b"runtime proof"),
                },
            )

    def test_capture_pending_and_confirmed_killpoint_states_are_distinct(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            control_plane = pathlib.Path(temporary)
            capture_dir = control_plane / "finished_runs" / "run-1"
            capture_dir.mkdir(parents=True)
            (capture_dir / "scrollback.txt").write_bytes(b"proof")
            (capture_dir / "capture.manifest.json").write_text(
                json.dumps({"version": 1, "evidence": {"bytes": 5}}),
                encoding="utf-8",
            )
            receipt = {
                "version": 4,
                "capture_committed": True,
                "metadata_committed": False,
                "viewer_confirmed": False,
                "viewer_creation_pending": False,
                "viewer_tab_identity": None,
                "origin_tab_state": "preserved",
                "viewer_token": "0123456789abcdef0123456789abcdef",
            }
            (capture_dir / "transfer.json").write_text(
                json.dumps(receipt), encoding="utf-8"
            )
            self.assertIsNotNone(
                MODULE.capture_receipt_killpoint_state(control_plane, "run-1")
            )
            self.assertIsNone(
                MODULE.pending_viewer_reservation_killpoint_state(
                    control_plane, "run-1"
                )
            )
            self.assertIsNone(
                MODULE.viewer_confirmation_killpoint_state(control_plane, "run-1")
            )
            receipt.update(
                {
                    "metadata_committed": True,
                    "viewer_creation_pending": True,
                    "viewer_creation_generation": 1,
                    "fault": None,
                }
            )
            (capture_dir / "transfer.json").write_text(
                json.dumps(receipt), encoding="utf-8"
            )
            self.assertIsNone(
                MODULE.capture_receipt_killpoint_state(control_plane, "run-1")
            )
            self.assertIsNotNone(
                MODULE.pending_viewer_reservation_killpoint_state(
                    control_plane, "run-1"
                )
            )
            self.assertIsNone(
                MODULE.viewer_confirmation_killpoint_state(control_plane, "run-1")
            )
            receipt.update(
                {
                    "viewer_confirmed": True,
                    "viewer_creation_pending": False,
                    "viewer_tab_identity": {"id": 7},
                }
            )
            (capture_dir / "transfer.json").write_text(
                json.dumps(receipt), encoding="utf-8"
            )
            self.assertIsNone(
                MODULE.capture_receipt_killpoint_state(control_plane, "run-1")
            )
            self.assertIsNone(
                MODULE.pending_viewer_reservation_killpoint_state(
                    control_plane, "run-1"
                )
            )
            self.assertIsNotNone(
                MODULE.viewer_confirmation_killpoint_state(control_plane, "run-1")
            )

    def test_controlled_interruption_kills_only_after_observed_state(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            marker = root / "ready"
            script = f"printf ready > {shlex.quote(str(marker))}; exec /bin/sleep 300"

            def observe() -> dict[str, object] | None:
                if not marker.is_file():
                    return None
                contents = marker.read_text(encoding="utf-8")
                return {"marker": contents} if contents == "ready" else None

            with mock.patch.object(
                MODULE.os,
                "killpg",
                side_effect=AssertionError("must not signal a process group"),
            ):
                result = MODULE.interrupt_process_at_state(
                    pathlib.Path("/bin/sh"),
                    dict(os.environ),
                    ["-c", script],
                    scenario="unit-killpoint",
                    artifact_root=root,
                    observe=observe,
                    slice_seconds=0.001,
                    max_slices=1_000,
                )
            self.assertEqual(result.returncode, -signal.SIGKILL)
            self.assertEqual(result.observed_state, {"marker": "ready"})

    def test_group_interruption_signals_only_its_fresh_owned_group(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            marker = root / "ready"
            script = (
                "import pathlib, subprocess, time; "
                "child = subprocess.Popen(['/bin/sleep', '300']); "
                f"pathlib.Path({str(marker)!r}).write_text("
                "str(child.pid), encoding='utf-8'); "
                "time.sleep(300)"
            )
            real_kill = os.kill
            real_process_group_members = MODULE.process_group_members
            exact_signals: list[tuple[int, int]] = []
            member_snapshots: list[list[dict[str, object]]] = []

            def observe() -> dict[str, object] | None:
                if not marker.is_file():
                    return None
                return {"marker": marker.read_text(encoding="utf-8")}

            def signal_exact_process(pid: int, signal_number: int) -> None:
                exact_signals.append((pid, signal_number))
                real_kill(pid, signal_number)

            def inspect_owned_group(group: int) -> list[dict[str, object]]:
                members = real_process_group_members(group)
                member_snapshots.append([dict(member) for member in members])
                return members

            def record_stopped_child(
                state: dict[str, object],
            ) -> dict[str, object]:
                child_pid = int(marker.read_text(encoding="utf-8"))
                return {
                    **state,
                    "child_pid": child_pid,
                    "child_state": MODULE.process_state(child_pid),
                }

            with mock.patch.object(
                MODULE.os,
                "killpg",
                side_effect=AssertionError("must not signal a process group"),
            ), mock.patch.object(
                MODULE.os,
                "kill",
                side_effect=signal_exact_process,
            ), mock.patch.object(
                MODULE,
                "process_group_members",
                side_effect=inspect_owned_group,
            ):
                result = MODULE.interrupt_process_at_state(
                    pathlib.Path(sys.executable),
                    dict(os.environ),
                    ["-c", script],
                    scenario="unit-owned-group-killpoint",
                    artifact_root=root,
                    observe=observe,
                    before_interrupt=record_stopped_child,
                    signal_process_group=True,
                    slice_seconds=0.001,
                    max_slices=1_000,
                )

            self.assertEqual(result.returncode, -signal.SIGKILL)
            self.assertIn(
                "T",
                str(result.observed_state.get("child_state")),
            )
            child_pid = int(result.observed_state["child_pid"])
            self.assertTrue(exact_signals)
            self.assertEqual(
                {pid for pid, _signal_number in exact_signals},
                {result.pid, child_pid},
            )
            self.assertIn((child_pid, signal.SIGSTOP), exact_signals)
            self.assertIn((child_pid, signal.SIGKILL), exact_signals)
            self.assertIn((result.pid, signal.SIGKILL), exact_signals)
            self.assertTrue(
                any(
                    int(member["pid"]) == child_pid
                    and "Z" in str(member.get("state", ""))
                    for members in member_snapshots
                    for member in members
                ),
                member_snapshots,
            )

    def test_group_continue_resumes_revalidated_descendants_before_leader(
        self,
    ) -> None:
        process = mock.Mock()
        process.pid = 9_401
        process.poll.return_value = None
        members = [
            {
                "pid": process.pid,
                "ppid": 1,
                "pgid": process.pid,
                "uid": 501,
                "sid": process.pid,
                "sid_errno": None,
                "sid_error": None,
                "state": "T",
                "command": "owned-leader",
            },
            {
                "pid": 9_402,
                "ppid": process.pid,
                "pgid": process.pid,
                "uid": 501,
                "sid": process.pid,
                "sid_errno": None,
                "sid_error": None,
                "state": "T",
                "command": "owned-child-a",
            },
            {
                "pid": 9_403,
                "ppid": process.pid,
                "pgid": process.pid,
                "uid": 501,
                "sid": process.pid,
                "sid_errno": None,
                "sid_error": None,
                "state": "T",
                "command": "owned-child-b",
            },
        ]
        with mock.patch.object(
            MODULE,
            "process_group_members",
            return_value=members,
        ) as inventory, mock.patch.object(
            MODULE.os,
            "getpgid",
            return_value=process.pid,
        ), mock.patch.object(
            MODULE.os,
            "getsid",
            return_value=process.pid,
        ), mock.patch.object(
            MODULE.os,
            "geteuid",
            return_value=501,
        ), mock.patch.object(
            MODULE.os,
            "kill",
        ) as exact_kill, mock.patch.object(
            MODULE.os,
            "killpg",
            side_effect=AssertionError("must not signal a process group"),
        ):
            MODULE.continue_owned_process_group(process)
        self.assertEqual(inventory.call_count, 4)
        self.assertEqual(
            exact_kill.call_args_list,
            [
                mock.call(9_402, signal.SIGCONT),
                mock.call(9_403, signal.SIGCONT),
                mock.call(process.pid, signal.SIGCONT),
            ],
        )

    def test_group_topology_rejects_grandchild_and_orphan_before_signal(
        self,
    ) -> None:
        process = mock.Mock()
        process.pid = 9_701
        process.poll.return_value = None
        leader = {
            "pid": process.pid,
            "ppid": 1,
            "pgid": process.pid,
            "uid": 501,
            "sid": process.pid,
            "sid_errno": None,
            "sid_error": None,
            "state": "T",
            "command": "owned-leader",
        }
        direct_child = {
            "pid": 9_702,
            "ppid": process.pid,
            "pgid": process.pid,
            "uid": 501,
            "sid": process.pid,
            "sid_errno": None,
            "sid_error": None,
            "state": "T",
            "command": "owned-child",
        }
        for topology, invalid_member in (
            (
                "grandchild",
                {
                    **direct_child,
                    "pid": 9_703,
                    "ppid": direct_child["pid"],
                    "command": "owned-grandchild",
                },
            ),
            (
                "orphan",
                {
                    **direct_child,
                    "pid": 9_704,
                    "ppid": 1,
                    "command": "owned-orphan",
                },
            ),
        ):
            with self.subTest(topology=topology), mock.patch.object(
                MODULE.os,
                "getpgid",
                return_value=process.pid,
            ), mock.patch.object(
                MODULE.os,
                "getsid",
                return_value=process.pid,
            ), mock.patch.object(
                MODULE.os,
                "geteuid",
                return_value=501,
            ), mock.patch.object(
                MODULE,
                "process_group_members",
                return_value=[leader, direct_child, invalid_member],
            ), mock.patch.object(
                MODULE.os,
                "kill",
            ) as exact_kill, mock.patch.object(
                MODULE.os,
                "killpg",
                side_effect=AssertionError("must not signal a process group"),
            ):
                with self.assertRaisesRegex(
                    MODULE.OwnedProcessGroupRefusal,
                    rf"invalid_members=.*{invalid_member['pid']}.*"
                    rf"'ppid': {invalid_member['ppid']}",
                ):
                    MODULE.continue_owned_process_group(process)
            exact_kill.assert_not_called()

    def test_group_continue_refuses_unstopped_revalidated_target_without_signal(
        self,
    ) -> None:
        process = mock.Mock()
        process.pid = 9_501
        process.poll.return_value = None
        leader = {
            "pid": process.pid,
            "ppid": 1,
            "pgid": process.pid,
            "uid": 501,
            "sid": process.pid,
            "sid_errno": None,
            "sid_error": None,
            "state": "T",
            "command": "owned-leader",
        }
        stopped_child = {
            "pid": 9_502,
            "ppid": process.pid,
            "pgid": process.pid,
            "uid": 501,
            "sid": process.pid,
            "sid_errno": None,
            "sid_error": None,
            "state": "T",
            "command": "owned-child",
        }
        running_child = {**stopped_child, "state": "S"}
        with mock.patch.object(
            MODULE,
            "validated_owned_process_group_members",
            side_effect=[
                [leader, stopped_child],
                [leader, running_child],
            ],
        ) as inventory, mock.patch.object(
            MODULE.os,
            "kill",
        ) as exact_kill, mock.patch.object(
            MODULE.os,
            "killpg",
            side_effect=AssertionError("must not signal a process group"),
        ):
            with self.assertRaisesRegex(
                MODULE.OwnedProcessGroupRefusal,
                r"refusing SIGCONT for exact owned member 9502: "
                r"revalidated state is 'S'",
            ):
                MODULE.continue_owned_process_group(process)
        self.assertEqual(inventory.call_count, 2)
        exact_kill.assert_not_called()

    def test_group_stop_observes_owned_leader_exit_before_signalling(self) -> None:
        process = mock.Mock()
        process.pid = 9_601
        process.poll.return_value = 0
        with mock.patch.object(
            MODULE,
            "stop_owned_process_group",
        ) as stop_group, mock.patch.object(
            MODULE.os,
            "kill",
        ) as exact_kill, mock.patch.object(
            MODULE.os,
            "killpg",
            side_effect=AssertionError("must not signal a process group"),
        ):
            with self.assertRaisesRegex(
                AssertionError,
                r"killpoint process 9601 exited before SIGSTOP: returncode=0",
            ):
                MODULE.wait_for_process_stop(
                    process,
                    reassert_stop=True,
                    signal_process_group=True,
                )
        process.poll.assert_called_once_with()
        stop_group.assert_not_called()
        exact_kill.assert_not_called()

    def test_transient_non_zombie_esrch_disappears_without_signal(self) -> None:
        process = mock.Mock()
        process.pid = 9_201
        process.poll.return_value = None
        leader = {
            "pid": process.pid,
            "ppid": 1,
            "pgid": process.pid,
            "uid": 501,
            "sid": process.pid,
            "sid_errno": None,
            "sid_error": None,
            "state": "T",
            "command": "owned-leader",
        }
        exiting_child = {
            "pid": 9_202,
            "ppid": process.pid,
            "pgid": process.pid,
            "uid": 501,
            "sid": None,
            "sid_errno": errno.ESRCH,
            "sid_error": "ProcessLookupError: gone",
            "state": "S",
            "command": "(vc-frame)",
        }
        with mock.patch.object(
            MODULE.os,
            "getpgid",
            return_value=process.pid,
        ), mock.patch.object(
            MODULE.os,
            "getsid",
            return_value=process.pid,
        ), mock.patch.object(
            MODULE.os,
            "geteuid",
            return_value=501,
        ), mock.patch.object(
            MODULE,
            "process_group_members",
            side_effect=[[leader, exiting_child], [leader]],
        ) as inventory, mock.patch.object(
            MODULE.time,
            "sleep",
        ) as pause, mock.patch.object(
            MODULE.os,
            "kill",
        ) as exact_kill, mock.patch.object(
            MODULE.os,
            "killpg",
            side_effect=AssertionError("must not signal a process group"),
        ):
            signalled = MODULE.signal_exact_owned_group_member(
                process,
                9_202,
                signal.SIGSTOP,
                deadline=MODULE.time.monotonic() + 1,
            )
        self.assertFalse(signalled)
        self.assertEqual(inventory.call_count, 2)
        pause.assert_called_once_with(0.001)
        exact_kill.assert_not_called()

    def test_persistent_non_zombie_esrch_refuses_without_signal(self) -> None:
        process = mock.Mock()
        process.pid = 9_301
        process.poll.return_value = None
        members = [
            {
                "pid": process.pid,
                "ppid": 1,
                "pgid": process.pid,
                "uid": 501,
                "sid": process.pid,
                "sid_errno": None,
                "sid_error": None,
                "state": "T",
                "command": "owned-leader",
            },
            {
                "pid": 9_302,
                "ppid": process.pid,
                "pgid": process.pid,
                "uid": 501,
                "sid": None,
                "sid_errno": errno.ESRCH,
                "sid_error": "ProcessLookupError: still unresolved",
                "state": "S",
                "command": "(vc-frame)",
            },
        ]
        with mock.patch.object(
            MODULE.os,
            "getpgid",
            return_value=process.pid,
        ), mock.patch.object(
            MODULE.os,
            "getsid",
            return_value=process.pid,
        ), mock.patch.object(
            MODULE.os,
            "geteuid",
            return_value=501,
        ), mock.patch.object(
            MODULE,
            "process_group_members",
            return_value=members,
        ) as inventory, mock.patch.object(
            MODULE.time,
            "sleep",
        ) as pause, mock.patch.object(
            MODULE.os,
            "kill",
        ) as exact_kill, mock.patch.object(
            MODULE.os,
            "killpg",
            side_effect=AssertionError("must not signal a process group"),
        ):
            with self.assertRaisesRegex(
                MODULE.OwnedProcessGroupRefusal,
                r"persistently ambiguous process group 9301: "
                r".*ambiguous_members=.*9302.*observations=",
            ):
                MODULE.signal_exact_owned_group_member(
                    process,
                    9_302,
                    signal.SIGSTOP,
                    deadline=MODULE.time.monotonic() + 1,
                )
        self.assertEqual(inventory.call_count, 4)
        self.assertEqual(pause.call_count, 3)
        exact_kill.assert_not_called()

    def test_exact_owned_member_eperm_refuses_without_group_fallback(self) -> None:
        process = mock.Mock()
        process.pid = 9_001
        process.poll.return_value = None
        members = [
            {
                "pid": process.pid,
                "ppid": 1,
                "pgid": process.pid,
                "uid": 501,
                "sid": process.pid,
                "sid_error": None,
                "state": "T",
                "command": "owned-leader",
            },
            {
                "pid": 9_002,
                "ppid": process.pid,
                "pgid": process.pid,
                "uid": 501,
                "sid": process.pid,
                "sid_error": None,
                "state": "S",
                "command": "owned-child",
            },
        ]
        permission_error = PermissionError(errno.EPERM, "Operation not permitted")
        with mock.patch.object(
            MODULE.os,
            "getpgid",
            return_value=process.pid,
        ), mock.patch.object(
            MODULE.os,
            "getsid",
            return_value=process.pid,
        ), mock.patch.object(
            MODULE.os,
            "geteuid",
            return_value=501,
        ), mock.patch.object(
            MODULE,
            "process_group_members",
            return_value=members,
        ), mock.patch.object(
            MODULE.os,
            "kill",
            side_effect=permission_error,
        ) as exact_kill, mock.patch.object(
            MODULE.os,
            "killpg",
            side_effect=AssertionError("must not signal a process group"),
        ):
            with self.assertRaisesRegex(
                MODULE.OwnedProcessGroupRefusal,
                r"exact owned member 9002 refused SIGSTOP: .*members=",
            ):
                MODULE.signal_exact_owned_group_member(
                    process,
                    9_002,
                    signal.SIGSTOP,
                    deadline=MODULE.time.monotonic(),
                )
        exact_kill.assert_called_once_with(9_002, signal.SIGSTOP)

    def test_foreign_uid_group_member_refuses_before_any_signal(self) -> None:
        process = mock.Mock()
        process.pid = 9_101
        process.poll.return_value = None
        members = [
            {
                "pid": process.pid,
                "ppid": 1,
                "pgid": process.pid,
                "uid": 501,
                "sid": process.pid,
                "sid_error": None,
                "state": "T",
                "command": "owned-leader",
            },
            {
                "pid": 9_102,
                "ppid": process.pid,
                "pgid": process.pid,
                "uid": 502,
                "sid": process.pid,
                "sid_error": None,
                "state": "S",
                "command": "foreign-child",
            },
        ]
        with mock.patch.object(
            MODULE.os,
            "getpgid",
            return_value=process.pid,
        ), mock.patch.object(
            MODULE.os,
            "getsid",
            return_value=process.pid,
        ), mock.patch.object(
            MODULE.os,
            "geteuid",
            return_value=501,
        ), mock.patch.object(
            MODULE,
            "process_group_members",
            return_value=members,
        ), mock.patch.object(MODULE.os, "kill") as exact_kill, mock.patch.object(
            MODULE.os,
            "killpg",
            side_effect=AssertionError("must not signal a process group"),
        ):
            with self.assertRaisesRegex(
                MODULE.OwnedProcessGroupRefusal,
                r"expected_uid=501.*invalid_members=.*9102",
            ):
                MODULE.validated_owned_process_group_members(process)
        exact_kill.assert_not_called()

    def test_early_exit_never_signals_process_group(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            with mock.patch.object(
                MODULE.os,
                "killpg",
                side_effect=AssertionError("must not signal a process group"),
            ):
                with self.assertRaisesRegex(
                    AssertionError,
                    "exited before (?:SIGSTOP|its killpoint)",
                ):
                    MODULE.interrupt_process_at_state(
                        pathlib.Path("/usr/bin/true"),
                        dict(os.environ),
                        [],
                        scenario="unit-early-exit",
                        artifact_root=root,
                        observe=lambda: None,
                        slice_seconds=0.001,
                        max_slices=10,
                    )

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
            self.assertTrue(
                recorder.data["negative_probes"][0]["state_contract_satisfied"]
            )
            with self.assertRaisesRegex(AssertionError, "changed tab focus"):
                MODULE.record_negative_probe(
                    recorder,
                    scenario="mutating_failure",
                    result=completed(2, stderr="failed"),
                    before=state,
                    after={"digest": "changed", "state": {"tabs": []}},
                    error_category="unexpected_mutation",
                )

    def test_failed_transfer_audit_allows_only_typed_receipt_and_empty_lock(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            control_plane = pathlib.Path(temporary)
            run = "run-failed"
            run_directory = control_plane / "finished_runs" / run
            run_directory.mkdir(parents=True)
            receipt = {
                "version": 4,
                "run": run,
                "exit_code": 2,
                "origin_session": "missing",
                "origin_tab": run,
                "capture": None,
                "capture_committed": False,
                "metadata_committed": False,
                "viewer_confirmed": False,
                "viewer_tab_identity": None,
                "viewer_creation_pending": False,
                "viewer_token": "a" * 32,
                "origin_tab_state": "preserved",
                "fault": "Capture: session missing",
            }
            receipt_path = run_directory / "transfer.json"
            receipt_path.write_text(json.dumps(receipt), encoding="utf-8")
            lock_path = run_directory / "transfer.lock"
            lock_path.write_bytes(b"")
            before_control = MODULE.artifact_tree_snapshot(
                control_plane / "before-absent"
            )
            before_control["root"] = str(control_plane.resolve())
            after_control = MODULE.artifact_tree_snapshot(control_plane)
            stable_runtime = {
                "sessions": {"missing": {"state": "absent"}},
                "session_inventory": {},
                "server_processes": [],
            }
            before = {
                "state": {
                    **stable_runtime,
                    "control_plane": before_control,
                },
                "digest": "before",
            }
            after = {
                "state": {
                    **stable_runtime,
                    "control_plane": after_control,
                },
                "digest": "after",
            }
            evidence = MODULE.failed_transfer_audit_evidence(
                control_plane,
                run=run,
                exit_code=2,
                origin_session="missing",
                before=before,
                after=after,
            )
            self.assertTrue(evidence["success_artifacts_absent"])
            self.assertEqual(
                evidence["allowed_changed_paths"],
                [
                    f"finished_runs/{run}/transfer.json",
                    f"finished_runs/{run}/transfer.lock",
                ],
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
        wait_processes: mock.Mock,
    ) -> None:
        events: list[str] = []
        inventory.side_effect = [
            {"owned": "live"},
            {"owned": "exited"},
            {},
            {},
        ]
        query.return_value = MODULE.SessionQuery(
            state="live",
            tabs=[],
            list_tabs_exit=0,
            list_tabs_stderr="",
            inventory_state="live",
        )

        def record_command(*args: object, **_kwargs: object) -> subprocess.CompletedProcess:
            events.append(str(args[2]))
            return completed(0)

        def record_process_barrier(
            *_args: object, **_kwargs: object
        ) -> list[dict[str, object]]:
            events.append("wait-processes")
            return []

        command.side_effect = record_command
        wait_processes.side_effect = record_process_barrier
        receipt = MODULE.cleanup_namespace(
            pathlib.Path("vc-frame"),
            {"VC_FRAME_SOCKET_DIR": "/tmp/proof/sockets"},
            {"owned"},
            timeout=0.01,
            stable_empty_for=0,
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
        self.assertEqual(
            events,
            [
                "kill-session",
                "wait-processes",
                "delete-session",
                "wait-processes",
            ],
        )

    @mock.patch.object(MODULE, "wait_for_no_server_processes", return_value=[])
    @mock.patch.object(MODULE, "wait_for_session_gone")
    @mock.patch.object(MODULE, "command")
    @mock.patch.object(MODULE, "session_inventory")
    @mock.patch.object(MODULE, "server_processes_for_socket_root")
    def test_cleanup_kills_an_exited_inventory_entry_with_a_live_server_pid(
        self,
        processes: mock.Mock,
        inventory: mock.Mock,
        command: mock.Mock,
        _wait_gone: mock.Mock,
        _wait_processes: mock.Mock,
    ) -> None:
        processes.return_value = [
            {
                "pid": 42,
                "command": (
                    "vc-frame --server "
                    "/tmp/proof/sockets/contract_version_1/owned"
                ),
            }
        ]
        inventory.side_effect = [{"owned": "exited"}, {}, {}]
        command.return_value = completed(0)

        receipt = MODULE.cleanup_namespace(
            pathlib.Path("vc-frame"),
            {"VC_FRAME_SOCKET_DIR": "/tmp/proof/sockets"},
            {"owned"},
            timeout=0.01,
            stable_empty_for=0,
        )

        self.assertEqual(receipt["killed_sessions"], ["owned"])
        self.assertEqual(
            [call.args[2:] for call in command.call_args_list],
            [("kill-session", "owned")],
        )

    @mock.patch.object(MODULE, "wait_for_no_server_processes", return_value=[])
    @mock.patch.object(MODULE, "wait_for_session_gone")
    @mock.patch.object(MODULE, "query_session")
    @mock.patch.object(MODULE, "command")
    @mock.patch.object(MODULE, "session_inventory")
    def test_cleanup_deletes_exited_session_that_appears_after_transient_empty_inventory(
        self,
        inventory: mock.Mock,
        command: mock.Mock,
        query: mock.Mock,
        _wait_gone: mock.Mock,
        _wait_processes: mock.Mock,
    ) -> None:
        inventory.side_effect = [
            {"owned": "live"},
            {},
            {"owned": "exited"},
            {},
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
            stable_empty_for=0,
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
