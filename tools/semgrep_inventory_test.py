#!/usr/bin/env python3

import copy
import importlib.util
import re
import tempfile
import unittest
from pathlib import Path

MODULE_PATH = Path(__file__).with_name("semgrep_inventory.py")
SPEC = importlib.util.spec_from_file_location("semgrep_inventory", MODULE_PATH)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class SemgrepInventoryTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.baseline = MODULE.read_json(MODULE.BASELINE_PATH)
        cls.rows = MODULE.read_inventory()

    def test_checked_in_inventory_is_valid(self) -> None:
        MODULE.validate_ignore_policy()
        MODULE.validate_inventory(self.rows, self.baseline)

    def test_missing_inventory_row_fails(self) -> None:
        with self.assertRaisesRegex(MODULE.InventoryError, "baseline requires"):
            MODULE.validate_inventory(self.rows[:-1], self.baseline)

    def test_empty_owner_fails(self) -> None:
        rows = copy.deepcopy(self.rows)
        rows[0]["owner"] = ""
        with self.assertRaisesRegex(MODULE.InventoryError, "empty owner"):
            MODULE.validate_inventory(rows, self.baseline)

    def test_unexplained_scanner_fingerprint_fails(self) -> None:
        source = {
            "version": self.baseline["scanner_version"],
            "errors": [],
            "time": {"rules": self.baseline["resolved_rule_ids"]},
            "paths": {"scanned": [f"target-{i}" for i in range(self.baseline["target_count"])]},
            "results": [
                {
                    "check_id": row["rule"],
                    "path": row["path"],
                    "start": {"line": row["line"]},
                    "extra": {"fingerprint": row["fingerprint"]},
                }
                for row in self.rows
            ],
        }
        source["results"][0]["extra"]["fingerprint"] = "unreviewed-fingerprint"
        with tempfile.TemporaryDirectory() as temp_dir:
            results_path = Path(temp_dir) / "results.json"
            results_path.write_text(MODULE.json.dumps(source), encoding="utf-8")
            with self.assertRaisesRegex(MODULE.InventoryError, "finding drift"):
                MODULE.validate_results(results_path, self.rows, self.baseline)

    def test_broad_ignore_policy_fails(self) -> None:
        policy = MODULE.read_json(MODULE.IGNORE_POLICY_PATH)
        policy["allowed_ignores"][0]["pattern"] = "zellij-client/**"
        with tempfile.TemporaryDirectory() as temp_dir:
            policy_path = Path(temp_dir) / "ignore-policy.json"
            policy_path.write_text(MODULE.json.dumps(policy), encoding="utf-8")
            with self.assertRaisesRegex(MODULE.InventoryError, "broad ignore"):
                MODULE.validate_ignore_policy(policy_path)

    @staticmethod
    def finding(rule: str, path: str, line: int) -> dict:
        return {
            "check_id": rule,
            "path": path,
            "start": {"line": line, "col": 1},
            "extra": {"fingerprint": "test-fingerprint"},
        }

    @staticmethod
    def live_lines(path: str, pattern: str) -> list[int]:
        matcher = re.compile(pattern)
        return [
            index
            for index, source_line in enumerate(
                (MODULE.ROOT / path).read_text(encoding="utf-8").splitlines(), 1
            )
            if matcher.search(source_line)
        ]

    def test_live_temp_dir_sources_are_explicitly_allowed(self) -> None:
        rule = "rust.lang.security.temp-dir.temp-dir"
        paths = sorted(MODULE.TERMINAL_CFG_TEST_MODULES | {
            MODULE.WEB_CLIENT_TEST_PATH,
            MODULE.XTASK_INSTALL_PATH,
            MODULE.OS_INPUT_OUTPUT_TEST_PATH,
            "zellij-server/src/tab/mod.rs",
        })
        for path in paths:
            lines = self.live_lines(path, r"\b(?:std::env::)?temp_dir\(\)")
            self.assertTrue(lines, path)
            for line in lines:
                with self.subTest(path=path, line=line):
                    MODULE.adjudicate(self.finding(rule, path, line))

    def test_xtask_install_uses_descriptor_relative_no_follow_paths(self) -> None:
        source = (MODULE.ROOT / MODULE.XTASK_INSTALL_PATH).read_text(encoding="utf-8")
        for required in (
            "open_directory_no_follow",
            "OFlags::NOFOLLOW",
            "rustix::fs::openat",
            "rustix::fs::statat",
            "rustix::fs::renameat",
            "same_file_identity",
            "revalidate_absolute_path",
        ):
            self.assertIn(required, source)
        self.assertNotIn("std::fs::File::open(&parent_path)", source)

    def test_live_current_exe_sources_are_explicitly_allowed(self) -> None:
        rule = "rust.lang.security.current-exe.current-exe"
        for path in sorted(MODULE.CURRENT_EXE_PATHS):
            lines = self.live_lines(path, r"\b(?:std::env::)?current_exe\(\)")
            self.assertTrue(lines, path)
            for line in lines:
                with self.subTest(path=path, line=line):
                    MODULE.adjudicate(self.finding(rule, path, line))

    def test_live_transfer_lock_unsafe_is_explicitly_allowed(self) -> None:
        path = MODULE.TRANSFER_LOCK_PATH
        lines = self.live_lines(path, r"\bunsafe\s*\{")
        self.assertEqual(len(lines), 1)
        MODULE.adjudicate(self.finding(
            "rust.lang.security.unsafe-usage.unsafe-usage", path, lines[0]
        ))

    def test_live_session_socket_unsafe_is_explicitly_allowed(self) -> None:
        path = MODULE.SESSION_SOCKET_PATH
        lines = self.live_lines(path, r"\bunsafe\s*\{")
        self.assertEqual(len(lines), 4)
        for line in lines:
            with self.subTest(line=line):
                MODULE.adjudicate(self.finding(
                    "rust.lang.security.unsafe-usage.unsafe-usage", path, line
                ))

    def test_changed_session_socket_unsafe_shape_fails(self) -> None:
        with self.assertRaisesRegex(MODULE.InventoryError, "source shape"):
            MODULE.require_session_socket_unsafe_policy(
                MODULE.SESSION_SOCKET_PATH,
                ["let result = unsafe { libc::flock(other_fd, libc::LOCK_EX) };"],
                1,
            )

    def test_process_log_scope_args_os_is_explicitly_allowed(self) -> None:
        path = "zellij-utils/src/consts.rs"
        lines = self.live_lines(path, r"std::env::args_os\(\)")
        self.assertEqual(lines, [22])
        MODULE.adjudicate(self.finding(
            "rust.lang.security.args-os.args-os", path, lines[0]
        ))

    def test_changed_process_log_scope_args_os_shape_fails(self) -> None:
        with self.assertRaisesRegex(MODULE.InventoryError, "process log scope args_os source shape"):
            MODULE.require_process_log_scope_args_os_policy(
                "zellij-utils/src/consts.rs", ["let mut args = std::env::args_os();"], 1
            )

    def test_spawn_error_test_unsafe_is_explicitly_allowed(self) -> None:
        path = "zellij-server/src/os_input_output_unix.rs"
        lines = self.live_lines(path, r"let err = unsafe \{")
        self.assertEqual(lines, [1109])
        MODULE.adjudicate(self.finding(
            "rust.lang.security.unsafe-usage.unsafe-usage", path, lines[0]
        ))

    def test_changed_spawn_error_test_unsafe_shape_fails(self) -> None:
        with self.assertRaisesRegex(MODULE.InventoryError, "spawn error test unsafe source shape"):
            MODULE.require_spawn_error_test_unsafe_policy(
                "zellij-server/src/os_input_output_unix.rs", ["let err = unsafe {"], 1
            )

    def test_os_input_output_process_temp_is_explicitly_allowed(self) -> None:
        path = MODULE.OS_INPUT_OUTPUT_TEST_PATH
        lines = self.live_lines(path, r"let process_temp = std::env::temp_dir\(\)")
        self.assertEqual(lines, [139])
        verdict = MODULE.adjudicate(self.finding(
            "rust.lang.security.temp-dir.temp-dir", path, lines[0]
        ))
        self.assertEqual(verdict[0], "scoped_false_positive")
        self.assertIn("never creates a file", verdict[1])

    def test_changed_os_input_output_process_temp_file_creation_fails(self) -> None:
        path = MODULE.OS_INPUT_OUTPUT_TEST_PATH
        lines = (MODULE.ROOT / path).read_text(encoding="utf-8").splitlines()
        lines[146] = 'let _ = std::fs::File::create(process_temp.join("x"));'
        with self.assertRaisesRegex(MODULE.InventoryError, "file creation"):
            MODULE.require_os_input_output_test_temp_dir(
                path, lines, 139, MODULE.ROOT
            )

    def test_changed_os_input_output_process_temp_comparison_shape_fails(self) -> None:
        path = MODULE.OS_INPUT_OUTPUT_TEST_PATH
        lines = (MODULE.ROOT / path).read_text(encoding="utf-8").splitlines()
        lines[148] = 'let legacy = process_temp.join("predictable.sock");'
        with self.assertRaisesRegex(MODULE.InventoryError, "comparison shape"):
            MODULE.require_os_input_output_test_temp_dir(
                path, lines, 139, MODULE.ROOT
            )

    def test_os_input_output_temp_dir_without_parent_gate_fails(self) -> None:
        path = MODULE.OS_INPUT_OUTPUT_TEST_PATH
        parent = MODULE.OS_INPUT_OUTPUT_PARENT_PATH
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            (root / path).parent.mkdir(parents=True)
            (root / parent).parent.mkdir(parents=True, exist_ok=True)
            (root / path).write_text(
                (MODULE.ROOT / path).read_text(encoding="utf-8"),
                encoding="utf-8",
            )
            (root / parent).write_text(
                "mod os_input_output_tests;\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(MODULE.InventoryError, "cfg\\(test\\) parent gate"):
                MODULE.adjudicate(
                    self.finding(
                        "rust.lang.security.temp-dir.temp-dir", path, 139
                    ),
                    root=root,
                )

    def test_os_input_output_temp_dir_in_production_parent_fails(self) -> None:
        with self.assertRaisesRegex(MODULE.InventoryError, "no source policy"):
            MODULE.require_temp_dir_policy(
                MODULE.OS_INPUT_OUTPUT_PARENT_PATH,
                ["let process_temp = std::env::temp_dir();"],
                1,
                MODULE.ROOT,
            )

    def test_sg_0092_open_dir_disposition_is_unchanged(self) -> None:
        row = next(item for item in self.rows if item["id"] == "SG-0092")
        self.assertEqual(row["path"], "zellij-server/src/plugins/plugin_loader.rs")
        self.assertEqual(row["line"], 36)
        self.assertEqual(row["column"], 25)
        self.assertEqual(
            row["rule"], "rust.actix.path-traversal.tainted-path.tainted-path"
        )
        self.assertEqual(row["verdict"], "scoped_false_positive")
        self.assertEqual(
            row["fingerprint"],
            "d647f9d8f9936be80fe7d44f27fc5f8f8dca37dea71e15520eec66ee7f021024f71ab1c51ef7aef6f93816f54b61aa9381bdb3b6423e81923be5c0a3ea078e54_0",
        )
        self.assertEqual(row["owner"], "Plugin filesystem capability")
        self.assertEqual(
            row["invariant"],
            "The path is a host-selected WASI preopen, not an HTTP parameter.",
        )

    def test_temp_dir_in_production_part_of_allowed_file_fails(self) -> None:
        path = "default-plugins/link/src/main.rs"
        with self.assertRaisesRegex(MODULE.InventoryError, "outside terminal"):
            MODULE.adjudicate(self.finding(
                "rust.lang.security.temp-dir.temp-dir", path, 1
            ))

    def test_temp_dir_in_unknown_source_path_fails(self) -> None:
        path = "zellij-utils/src/consts.rs"
        line = self.live_lines(path, r"\btemp_dir\(\)")[0]
        with self.assertRaisesRegex(MODULE.InventoryError, "no source policy"):
            MODULE.adjudicate(self.finding(
                "rust.lang.security.temp-dir.temp-dir", path, line
            ))

    def test_changed_current_exe_callsite_shape_fails(self) -> None:
        path = "zellij-client/src/lib.rs"
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            source_path = root / path
            source_path.parent.mkdir(parents=True)
            source_path.write_text(
                "let executable = current_exe()?;\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(MODULE.InventoryError, "direct Command"):
                MODULE.adjudicate(
                    self.finding(
                        "rust.lang.security.current-exe.current-exe", path, 1
                    ),
                    root=root,
                )

    def test_run_triage_current_exe_outside_reviewed_shapes_fails(self) -> None:
        path = MODULE.TRANSFER_LOCK_PATH
        lines = [
            "fn main() {",
            "let executable = std::env::current_exe().unwrap();",
            "}",
        ]
        with self.assertRaisesRegex(MODULE.InventoryError, "terminal"):
            MODULE.require_current_exe_policy(path, lines, 2)

    def test_changed_isolated_transfer_lock_probe_shape_fails(self) -> None:
        path = MODULE.TRANSFER_LOCK_PATH
        lines = (MODULE.ROOT / path).read_text(encoding="utf-8").splitlines()
        finding_line = self.live_lines(
            path,
            r"let output = Command::new\(std::env::current_exe\(\)\.unwrap\(\)\)",
        )[0]
        lines[finding_line] = '.arg("unreviewed_transfer_lock_child")'
        with self.assertRaisesRegex(MODULE.InventoryError, "source shape"):
            MODULE.require_current_exe_policy(path, lines, finding_line)

    def test_changed_transfer_lock_unsafe_shape_fails(self) -> None:
        with self.assertRaisesRegex(MODULE.InventoryError, "source shape"):
            MODULE.require_transfer_lock_fd_policy(
                MODULE.TRANSFER_LOCK_PATH,
                ["let file = unsafe { std::fs::File::from_raw_fd(other_fd) };"],
                1,
            )

    def test_noncanonical_result_path_fails(self) -> None:
        with self.assertRaisesRegex(MODULE.InventoryError, "not canonical"):
            MODULE.adjudicate(self.finding(
                "rust.lang.security.current-exe.current-exe",
                "./zellij-client/src/lib.rs",
                1,
            ))

    def test_result_line_outside_source_fails(self) -> None:
        path = "zellij-client/src/lib.rs"
        line = len((MODULE.ROOT / path).read_text(encoding="utf-8").splitlines()) + 1
        with self.assertRaisesRegex(MODULE.InventoryError, "outside"):
            MODULE.adjudicate(self.finding(
                "rust.lang.security.current-exe.current-exe", path, line
            ))


if __name__ == "__main__":
    unittest.main()
