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
            "zellij-server/src/tab/mod.rs",
        })
        for path in paths:
            lines = self.live_lines(path, r"\b(?:std::env::)?temp_dir\(\)")
            self.assertTrue(lines, path)
            for line in lines:
                with self.subTest(path=path, line=line):
                    MODULE.adjudicate(self.finding(rule, path, line))

    def test_live_xtask_install_paths_are_explicitly_allowed(self) -> None:
        path = MODULE.XTASK_INSTALL_PATH
        lines = self.live_lines(path, r"std::fs::File::open\((?:&parent_path|source\.path\(\))\)")
        self.assertEqual(len(lines), 2)
        for line in lines:
            with self.subTest(line=line):
                MODULE.adjudicate(self.finding(
                    "rust.actix.path-traversal.tainted-path.tainted-path",
                    path,
                    line,
                ))

    def test_changed_xtask_install_path_shape_fails(self) -> None:
        with self.assertRaisesRegex(MODULE.InventoryError, "source shape"):
            MODULE.require_xtask_install_path_policy(
                MODULE.XTASK_INSTALL_PATH,
                ["let file = std::fs::File::open(unreviewed_path)?;"],
                1,
            )

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
