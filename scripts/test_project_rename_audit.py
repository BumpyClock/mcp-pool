"""Exercise rename inventory against disposable Git repositories."""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from project_rename_audit import audit, markdown


class RenameAuditTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="rename-audit-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.git("init", "--quiet")
        (self.root / ".hidden").mkdir()
        self.source = self.root / ".hidden" / "mcp-pool.txt"
        self.source.write_text("mcp_pool MCP Pool mcppool mcp.pool mcp/pool\nMCP_POOL_HOME=mcp-pool\n")
        (self.root / "Cargo.toml").write_text('name = "mcp-pool"\n')
        (self.root / "binary.dat").write_bytes(b"\x00mcp-pool\xff")
        self.git("add", ".")
        self.git(
            "-c", "user.name=Rename audit fixture", "-c", "user.email=rename-audit@example.invalid",
            "-c", "core.hooksPath=.git/hooks", "commit", "--quiet", "-m", "fixture",
        )

    def git(self, *arguments):
        return subprocess.run(
            ["git", "-C", str(self.root), *arguments], check=True, capture_output=True
        ).stdout

    def test_hidden_files_variants_counts_and_environment_locations(self):
        report = audit(self.root)
        self.assertEqual(report["tracked_paths"], 3)
        self.assertEqual(report["matching_files"], 2)
        self.assertEqual(report["matching_lines"], 3)
        self.assertEqual(report["occurrences"], 8)
        self.assertEqual(report["filename_matches"], [".hidden/mcp-pool.txt"])
        self.assertEqual(report["environment"], {"MCP_POOL_HOME": [".hidden/mcp-pool.txt:2"]})
        self.assertEqual(report["files"][0]["lines"][0]["number"], 1)

    def test_snapshot_does_not_read_worktree_or_untracked_artifacts(self):
        before = audit(self.root)
        self.source.write_text("changed\n")
        (self.root / "untracked.txt").write_text("mcp-pool\n")
        self.assertEqual(audit(self.root), before)
        self.assertEqual(audit(self.root, worktree=True)["matching_files"], 1)

    def test_no_content_matches_retains_filename_findings(self):
        self.source.write_text("changed\n")
        (self.root / "Cargo.toml").write_text('name = "renamed"\n')
        report = audit(self.root, worktree=True)
        self.assertEqual(report["matching_files"], 0)
        self.assertEqual(report["occurrences"], 0)
        self.assertEqual(report["filename_matches"], [".hidden/mcp-pool.txt"])

    def test_invalid_revision_fails_instead_of_reporting_clean(self):
        with self.assertRaises(RuntimeError):
            audit(self.root, "missing-rename-audit-revision")

    def test_markdown_retains_every_matching_path_and_line(self):
        table = markdown(audit(self.root))
        self.assertIn("| `.hidden/mcp-pool.txt` | 7 | 1, 2 |", table)
        self.assertIn("| `Cargo.toml` | 1 | 1 |", table)
        self.assertNotIn("binary.dat", table)

    def test_cli_json_matches_programmatic_report(self):
        script = Path(__file__).with_name("project_rename_audit.py")
        output = subprocess.run(
            [sys.executable, str(script), "--root", str(self.root), "--format", "json"],
            check=True, capture_output=True, text=True,
        ).stdout
        self.assertEqual(json.loads(output), audit(self.root))


if __name__ == "__main__":
    unittest.main()
