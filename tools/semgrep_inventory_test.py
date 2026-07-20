#!/usr/bin/env python3

import copy
import importlib.util
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


if __name__ == "__main__":
    unittest.main()
