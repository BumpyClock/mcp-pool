#!/usr/bin/env python3
"""Check audit failure detection and owned-fixture cleanup without either CLI."""

import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from cli_ux_audit import Audit
from cli_ux_process import OwnedProcesses


class AuditTests(unittest.TestCase):
    def setUp(self):
        self.output = tempfile.TemporaryDirectory(prefix="ux-check-")
        self.directory = Path(self.output.name)
        self.binary = self.directory / "cli.py"
        self.binary.write_text(f"#!{sys.executable}\nimport sys\nsys.exit(0)\n")
        self.binary.chmod(0o700)

    def tearDown(self):
        self.output.cleanup()

    def test_successful_cleanup_retires_fixture_and_removes_owned_state(self):
        audit = Audit("mcp-cli", str(self.binary), self.directory)
        audit.close()
        self.assertIsNotNone(audit.http.poll())
        self.assertFalse(audit.root.exists())

    def test_failed_daemon_cleanup_still_retires_http_and_preserves_state(self):
        self.binary.write_text(f"#!{sys.executable}\nimport sys\nsys.exit(1)\n")
        audit = Audit("mcp-cli", str(self.binary), self.directory)
        try:
            with self.assertRaisesRegex(RuntimeError, "state retained at"):
                audit.close()
            self.assertIsNotNone(audit.http.poll())
            self.assertTrue(audit.root.is_dir())
        finally:
            shutil.rmtree(audit.root)

    def test_setup_read_timeout_retires_acquired_child_and_preserves_original_error(self):
        root = Path(tempfile.mkdtemp(prefix="partial-", dir=self.directory))
        children = []
        spawn = OwnedProcesses.spawn

        def capture(owner, command, **options):
            child = spawn(owner, command, **options)
            children.append(child)
            return child

        with patch("cli_ux_audit.tempfile.mkdtemp", return_value=str(root)), \
                patch.object(OwnedProcesses, "spawn", new=capture), \
                patch("cli_ux_audit.read_line", side_effect=RuntimeError("startup read timeout")), \
                self.assertRaisesRegex(RuntimeError, "startup read timeout"):
            Audit("mcp-cli", str(self.binary), self.directory)
        self.assertEqual(len(children), 1)
        self.assertIsNotNone(children[0].poll())
        self.assertFalse(root.exists())

    def test_setup_write_failure_retires_acquired_child_and_preserves_original_error(self):
        root = Path(tempfile.mkdtemp(prefix="partial-", dir=self.directory))
        children = []
        spawn = OwnedProcesses.spawn
        write_text = Path.write_text

        def capture(owner, command, **options):
            child = spawn(owner, command, **options)
            children.append(child)
            return child

        def fail_config(path, *arguments, **options):
            if path.name == "config.json":
                raise OSError("configuration write failed")
            return write_text(path, *arguments, **options)

        with patch("cli_ux_audit.tempfile.mkdtemp", return_value=str(root)), \
                patch.object(OwnedProcesses, "spawn", new=capture), \
                patch.object(Path, "write_text", new=fail_config), \
                self.assertRaisesRegex(OSError, "configuration write failed"):
            Audit("mcp-cli", str(self.binary), self.directory)
        self.assertEqual(len(children), 1)
        self.assertIsNotNone(children[0].poll())
        self.assertFalse(root.exists())

    def test_unverified_final_reap_retains_namespace_for_retry(self):
        audit = Audit("mcp-cli", str(self.binary), self.directory)
        try:
            timeout = subprocess.TimeoutExpired("owned HTTP fixture", 5)
            with patch.object(audit.http, "wait", side_effect=timeout), \
                    patch.object(audit.http, "kill", wraps=audit.http.kill) as kill:
                with self.assertRaisesRegex(RuntimeError, "state retained at"):
                    audit.close()
                kill.assert_called_once()
            self.assertTrue(audit.root.is_dir())
        finally:
            audit.close()
        self.assertFalse(audit.root.exists())

    def test_foreground_status_failures_are_recorded_and_later_scenarios_continue(self):
        for response in [
            subprocess.CompletedProcess([], 0, "not JSON", ""),
            subprocess.CompletedProcess([], 0, '["unexpected"]', ""),
            subprocess.CompletedProcess([], 1, "", "status failed"),
            subprocess.TimeoutExpired("daemon status", 5),
            OSError("status unavailable"),
        ]:
            with self.subTest(response=response):
                audit = Audit("mcporter", str(self.binary), self.directory)
                try:
                    with patch("cli_ux_audit.subprocess.run", side_effect=[response]):
                        audit.foreground()
                    self.assertEqual(audit.rows[0]["label"], "foreground-daemon")
                    self.assertEqual(audit.rows[0]["exit"], 1)
                    self.assertTrue(audit.rows[0]["problems"])
                    self.assertTrue((self.directory / "mcporter-foreground-daemon.txt").is_file())
                    later = audit.run("later-scenario", [])
                    self.assertEqual(later["problems"], [])
                finally:
                    audit.close()
                self.assertFalse(audit.root.exists())

    def test_timeout_transcripts_decode_partial_output_without_byte_representations(self):
        audit = Audit("mcp-cli", str(self.binary), self.directory)
        try:
            timeout = subprocess.TimeoutExpired("fixture", 20, output=b"partial\xff\n", stderr=b"diagnostic\n")
            with patch("cli_ux_audit.subprocess.run", side_effect=timeout):
                row = audit.run("timeout", ["call"])
            self.assertEqual(row["exit"], 124)
            self.assertEqual(row["stdout"], "partial\ufffd\n")
            self.assertEqual(row["stderr"], "diagnostic\n")
            self.assertIn("exit 124, expected 0", row["problems"])
            transcript = (self.directory / "mcp-cli-timeout.txt").read_text()
            self.assertIn("partial\ufffd\n", transcript)
            self.assertNotIn("b'", transcript)
        finally:
            audit.close()

    def test_gate_rejects_wrong_values_invalid_json_leaks_and_output_noise(self):
        audit = Audit("mcp-cli", str(self.binary), self.directory)
        try:
            good = audit.record("good", ["call"], '{"count":2}', "", 0, expected_json={"count": 2})
            self.assertEqual(good["problems"], [])
            for label, stdout, stderr, code, options in [
                ("wrong-value", '{"count":"2"}', "", 0, {"expected_json": {"count": 2}}),
                ("invalid-json", "not JSON", "", 0, {"machine": True}),
                ("ansi", "\x1b[32mtext", "", 0, {}),
                ("quiet-failure", "", "unexpected error", 1, {"expected": 1}),
                ("leak", "synthetic-audit-token", "", 0, {}),
                ("exit", "", "", 1, {}),
            ]:
                row = audit.record(label, ["call"], stdout, stderr, code, **options)
                self.assertTrue(row["problems"], label)
                self.assertTrue((self.directory / f"mcp-cli-{label}.txt").is_file())
        finally:
            audit.close()


if __name__ == "__main__":
    unittest.main()
