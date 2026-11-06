#!/usr/bin/env python3

import copy
import importlib.util
import re
import tempfile
import unittest
import hashlib
from types import SimpleNamespace
from unittest.mock import patch
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
        self.assertTrue(lines)
        source = (MODULE.ROOT / path).read_text(encoding="utf-8").splitlines()
        operations = set()
        for line in lines:
            body = "\n".join(source[line - 1:line + 16])
            for operation in ("libc::flock(", "libc::renameatx_np(", "libc::SYS_renameat2,"):
                if operation in body:
                    operations.add(operation)
        self.assertEqual(operations, {"libc::flock(", "libc::renameatx_np(", "libc::SYS_renameat2,"})
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
        self.assertEqual(len(lines), 1)
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
        self.assertEqual(len(lines), 1)
        verdict = MODULE.adjudicate(self.finding(
            "rust.lang.security.temp-dir.temp-dir", path, lines[0]
        ))
        self.assertEqual(verdict[0], "scoped_false_positive")
        self.assertIn("never creates a file", verdict[1])

    def test_changed_os_input_output_process_temp_file_creation_fails(self) -> None:
        path = MODULE.OS_INPUT_OUTPUT_TEST_PATH
        lines = (MODULE.ROOT / path).read_text(encoding="utf-8").splitlines()
        line = self.live_lines(path, r"let process_temp = std::env::temp_dir\(\)")[0]
        lines.insert(line + 1, '        let _ = std::fs::File::create(process_temp.join("x"));')
        with self.assertRaisesRegex(MODULE.InventoryError, "file creation"):
            MODULE.require_os_input_output_test_temp_dir(path, lines, line, MODULE.ROOT)

    def test_changed_os_input_output_process_temp_comparison_shape_fails(self) -> None:
        path = MODULE.OS_INPUT_OUTPUT_TEST_PATH
        lines = (MODULE.ROOT / path).read_text(encoding="utf-8").splitlines()
        line = self.live_lines(path, r"let process_temp = std::env::temp_dir\(\)")[0]
        legacy = next(i for i, text in enumerate(lines) if "let legacy = process_temp.join(format!(" in text)
        lines[legacy] = '        let legacy = process_temp.join("predictable.sock");'
        with self.assertRaisesRegex(MODULE.InventoryError, "comparison shape"):
            MODULE.require_os_input_output_test_temp_dir(path, lines, line, MODULE.ROOT)

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

    def test_open_dir_disposition_is_bound_to_source_identity(self) -> None:
        path = "zellij-server/src/plugins/plugin_loader.rs"
        rows = [item for item in self.rows if item["path"] == path
                and item["rule"] == "rust.actix.path-traversal.tainted-path.tainted-path"]
        self.assertEqual(len(rows), 1)
        row = rows[0]
        line = self.live_lines(path, r"std::fs::File::open\(path\)")[0]
        source = (MODULE.ROOT / path).read_text().splitlines()[line - 1]
        self.assertEqual(row["line"], line)
        self.assertEqual(row["column"], source.index("(path)") + 2)
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
        with self.assertRaises(MODULE.InventoryError):
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


class ResolvedConfigTests(unittest.TestCase):
    @staticmethod
    def dump(origin: str, body: str = 'pattern="unsafe"; severity=Warning; language=Rust; modifier=true; rules=60') -> str:
        return ('{ Rule_fetching.rules = [' + body + '];\n  origin =\n'
                '  (Rule_fetching.Local_file\n     ' + origin + ')\n  }\n')

    def test_randomized_origin_directory_and_basename_are_nonsemantic(self) -> None:
        expected = MODULE.normalize_resolved_config(self.dump('/tmp/a/osemgrep1-abc.yaml'))
        for index in range(20):
            path = f'/var/folders/random-{index}/T/owned-{index}/osemgrep{index}-F00BA4.yaml'
            self.assertEqual(MODULE.normalize_resolved_config(self.dump(path)), expected)

    def test_real_rule_fields_remain_fatal(self) -> None:
        original = self.dump('/tmp/a/osemgrep1-abc.yaml')
        baseline = {'resolved_config_normalization': MODULE.CONFIG_NORMALIZATION,
                    'resolved_config_sha256': hashlib.sha256(MODULE.normalize_resolved_config(original).encode()).hexdigest()}
        for old, new in [('unsafe', 'Command'), ('Warning', 'Error'), ('Rust', 'Python'),
                         ('modifier=true', 'modifier=false'), ('rules=60', 'rules=59')]:
            with self.subTest(field=old), patch.object(MODULE.subprocess, 'run', return_value=SimpleNamespace(returncode=0, stdout=original.replace(old, new))):
                with self.assertRaisesRegex(MODULE.InventoryError, 'rule content drifted'):
                    MODULE.validate_resolved_config('p/rust', baseline)

    def test_rule_body_temporary_looking_filenames_are_not_normalized(self) -> None:
        a = self.dump('/tmp/a/osemgrep1-abc.yaml', 'pattern="osemgrepABC.yaml"')
        b = self.dump('/tmp/a/osemgrep1-abc.yaml', 'pattern="osemgrepXYZ.yaml"')
        self.assertNotEqual(MODULE.normalize_resolved_config(a), MODULE.normalize_resolved_config(b))

    def test_missing_ambiguous_and_changed_origin_shapes_fail(self) -> None:
        original = self.dump('/tmp/a/osemgrep1-abc.yaml')
        for malformed in [original.split('  origin =')[0], original + original,
                          self.dump('relative/osemgrep1-abc.yaml'), self.dump('/tmp/custom.yaml'),
                          original + 'trailing content', original.replace('Local_file', 'Registry')]:
            with self.subTest(shape=malformed[-60:]), self.assertRaises(MODULE.InventoryError):
                MODULE.normalize_resolved_config(malformed)

    def test_normalizer_version_mismatch_fails(self) -> None:
        with self.assertRaisesRegex(MODULE.InventoryError, 'normalization version'):
            MODULE.validate_resolved_config('p/rust', {'resolved_config_normalization': 'old'})


class AdditionalPolicyTests(unittest.TestCase):
    finding = staticmethod(SemgrepInventoryTests.finding)
    live_lines = staticmethod(SemgrepInventoryTests.live_lines)
    # Each row names the finding and an independently material safety predicate.
    CASES = [
        ('current-exe', 'src/project_prepare.rs', 'let executable = std::env::current_exe()',
         'validate_runtime_engine(&root, &frame_entry, &executable)?;', 'let _ = &executable;'),
        ('current-exe', 'src/project_prepare.rs', 'std::fs::copy(std::env::current_exe().unwrap(), &engine).unwrap();',
         'mod tests {', 'mod unrelated {'),
        ('current-exe', 'src/project_prepare.rs', 'let foreign = std::env::current_exe().unwrap().canonicalize().unwrap();',
         'mod tests {', 'mod unrelated {'),
        ('temp-dir', 'xtask/src/build.rs', 'let root = std::env::temp_dir().join("xtask-repo-root");',
         'assert!(root.is_absolute() && central.is_absolute());', 'std::fs::write(&root, "x").unwrap();'),
        ('temp-dir', 'xtask/src/build.rs', 'let central = std::env::temp_dir().join("xtask-central-target");',
         'assert!(root.is_absolute() && central.is_absolute());', 'std::fs::write(&central, "x").unwrap();'),
        ('traversal', 'xtask/src/ci.rs', 'std::fs::copy(&source, plugin_dir.join(&file_name))',
         '&build::plugin_member_names()?,', '&untrusted_names,'),
        ('temp-dir', 'xtask/src/ci.rs', 'scratch_in(&std::env::temp_dir(), tag)',
         '.tempdir_in(parent)', '.create_dir_all(parent)'),
        ('unsafe', 'zellij-client/src/lib.rs', 'let rc = unsafe { listen(listener.as_raw_fd(), 1) };',
         'mod workspace_projection_readiness_tests {', 'mod production_listener {'),
        ('unsafe', 'zellij-server/src/background_jobs.rs', 'if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {',
         '.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)', '.custom_flags(0)'),
        ('unsafe', 'zellij-server/src/os_input_output.rs', 'let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };',
         'if rc != expected {', 'if rc == expected {'),
        ('unsafe', 'zellij-server/src/os_input_output.rs', 'let rc = unsafe {',
         '&mut info as *mut _ as *mut libc::c_void,', 'std::ptr::null_mut(),'),
        ('temp-dir', 'zellij-utils/src/consts.rs', 'let candidate = std::env::temp_dir().join(format!(',
         'std::fs::create_dir(&path)?;', 'std::fs::create_dir_all(&path)?;'),
        ('unsafe', 'zellij-utils/src/logging.rs', 'let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };',
         'let _lease = lease;', 'drop(lease);'),
        ('temp-dir', 'zellij-utils/src/setup.rs', 'Self::in_dir(&std::env::temp_dir(), label)',
         '.tempdir_in(parent)', '.create_dir_all(parent)'),
        ('unsafe', MODULE.OS_INPUT_OUTPUT_TEST_PATH, 'let read = unsafe {',
         'let mut granted_len = std::mem::size_of_val(&granted) as libc::socklen_t;', 'let mut granted_len = 9999;'),
        ('unsafe', 'zellij-utils/src/sessions_windows.rs', 'unsafe {',
         'CloseHandle(handle);', 'let _ = handle;'),
        ('args-os', 'zellij-utils/src/logging.rs', 'std::env::args_os().any(|argument| {',
         '.is_some_and(|value| value.starts_with("--server="))', '.is_some_and(|value| value.starts_with("--anything="))'),
        ('temp-dir', MODULE.OS_INPUT_OUTPUT_TEST_PATH, 'let path = std::env::temp_dir().join("terminal-history.dump");',
         '.env("TMPDIR", directory.path())', '.env("TMPDIR", "/tmp")'),
        ('current-exe', MODULE.OS_INPUT_OUTPUT_TEST_PATH, 'let output = Command::new(std::env::current_exe().unwrap())',
         '"--test-threads=1",', '"--all-tests",'),
    ]

    @staticmethod
    def rule(kind: str) -> str:
        return {'unsafe': 'rust.lang.security.unsafe-usage.unsafe-usage',
                'traversal': 'rust.actix.path-traversal.tainted-path.tainted-path'}.get(kind, f'rust.lang.security.{kind}.{kind}')

    def test_each_additional_match_has_a_narrow_positive_and_negative_control(self) -> None:
        for kind, path, source, predicate, mutation in self.CASES:
            with self.subTest(path=path, source=source):
                text = (MODULE.ROOT / path).read_text()
                matches = [i for i, row in enumerate(text.splitlines(), 1) if row.strip() == source]
                self.assertEqual(len(matches), 1)
                finding = self.finding(self.rule(kind), path, matches[0])
                MODULE.adjudicate(finding)
                self.assertIn(predicate, text)
                with tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    target = root / path
                    target.parent.mkdir(parents=True)
                    target.write_text(text.replace(predicate, mutation, 1))
                    if path == MODULE.OS_INPUT_OUTPUT_TEST_PATH:
                        parent = root / MODULE.OS_INPUT_OUTPUT_PARENT_PATH
                        parent.parent.mkdir(parents=True, exist_ok=True)
                        parent.write_text((MODULE.ROOT / MODULE.OS_INPUT_OUTPUT_PARENT_PATH).read_text())
                    with self.assertRaises(MODULE.InventoryError):
                        MODULE.adjudicate(finding, root)

    def test_new_guest_trampolines_require_the_exact_serialized_command(self) -> None:
        path = 'zellij-tile/src/shim.rs'
        text = (MODULE.ROOT / path).read_text()
        lines = text.splitlines()
        commands = [
            'let plugin_command = PluginCommand::RunAction(Box::new(action), context);',
            'let plugin_command = PluginCommand::ShowFloatingPanes { tab_id };',
            'let plugin_command = PluginCommand::HideFloatingPanes { tab_id };',
            'let plugin_command = PluginCommand::SetPaneRegexHighlights(pane_id, highlights);',
            'let plugin_command = PluginCommand::ClearPaneHighlights(pane_id);',
        ]
        for command in commands:
            start = next(i for i, row in enumerate(lines) if row.strip() == command)
            line = next(i + 1 for i in range(start, start + 8) if lines[i].strip() == 'unsafe { host_run_plugin_command() };')
            finding = self.finding(self.rule('unsafe'), path, line)
            with self.subTest(command=command):
                MODULE.adjudicate(finding)
                with tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    target = root / path
                    target.parent.mkdir(parents=True)
                    target.write_text(text.replace(command, 'let plugin_command = unreviewed_command;', 1))
                    with self.assertRaises(MODULE.InventoryError):
                        MODULE.adjudicate(finding, root)

    def test_saturation_fixture_ffi_requires_checked_address_capacity(self) -> None:
        path = 'zellij-utils/src/consts.rs'
        text = (MODULE.ROOT / path).read_text()
        lines = text.splitlines()
        start = next(i for i, row in enumerate(lines) if 'fn saturated_listener(' in row)
        sources = ['let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };',
                   'let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };',
                   'unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw) };',
                   'let flags = unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_GETFL) };',
                   'let rc = unsafe {']
        for source in sources:
            line = next(i + 1 for i in range(start, len(lines)) if lines[i].strip() == source)
            finding = self.finding(self.rule('unsafe'), path, line)
            with self.subTest(source=source):
                MODULE.adjudicate(finding)
                with tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    target = root / path
                    target.parent.mkdir(parents=True)
                    target.write_text(text.replace('assert!(bytes.len() < address.sun_path.len());', 'let _ = bytes.len();', 1))
                    with self.assertRaises(MODULE.InventoryError):
                        MODULE.adjudicate(finding, root)

    def test_unrelated_unsafe_in_newly_reviewed_files_fails(self) -> None:
        for path in ['zellij-client/src/lib.rs', 'zellij-server/src/background_jobs.rs',
                     'zellij-server/src/os_input_output.rs', 'zellij-utils/src/logging.rs']:
            text = (MODULE.ROOT / path).read_text()
            with self.subTest(path=path), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                target = root / path
                target.parent.mkdir(parents=True)
                target.write_text(text + '\nfn unreviewed() {\n    unsafe { arbitrary_call(); }\n}\n')
                line = len(text.splitlines()) + 3
                with self.assertRaises(MODULE.InventoryError):
                    MODULE.adjudicate(self.finding(self.rule('unsafe'), path, line), root)

    def test_harmless_line_insertion_keeps_semantic_test_anchor(self) -> None:
        path = MODULE.OS_INPUT_OUTPUT_TEST_PATH
        lines = (MODULE.ROOT / path).read_text().splitlines()
        line = self.live_lines(path, r'let process_temp = std::env::temp_dir\(\)')[0]
        shifted = ['// harmless inserted line'] * 31 + lines
        MODULE.require_os_input_output_test_temp_dir(path, shifted, line + 31, MODULE.ROOT)

    def test_uuid_alone_does_not_admit_nonprivate_writer(self) -> None:
        path = 'zellij-server/src/tab/mod.rs'
        lines = (MODULE.ROOT / path).read_text().splitlines()
        line = self.live_lines(path, r'let mut file = temp_dir\(\)')[0]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            writer = root / MODULE.OS_INPUT_OUTPUT_PARENT_PATH
            writer.parent.mkdir(parents=True)
            writer.write_text((MODULE.ROOT / MODULE.OS_INPUT_OUTPUT_PARENT_PATH).read_text().replace('options.mode(0o600);', 'options.mode(0o666);'))
            with self.assertRaises(MODULE.InventoryError):
                MODULE.require_temp_dir_policy(path, lines, line, root)



if __name__ == "__main__":
    unittest.main()
