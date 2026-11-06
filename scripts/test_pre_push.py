"""Run executable hook/installer fixtures without real pushes or MCP services."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib
import unittest

SOURCE = Path(__file__).resolve().parents[1]
ZERO = "0" * 40
TOOLCHAIN = tomllib.loads((SOURCE / "rust-toolchain.toml").read_text())["toolchain"]["channel"]
CARGO = ["rustup", "run", TOOLCHAIN, "cargo"]
FORMAT = CARGO + ["fmt", "--all", "--", "--check"]
CHECK = CARGO + ["check", "--offline", "--locked", "--bin", "mcp-cli"]
IPC = CARGO + ["test", "--offline", "--locked", "--bin", "mcp-cli", "--", "local_security", "transport::tests", "--test-threads=1"]


class HookTests(unittest.TestCase):
    def setUp(self):
        self.fixture = tempfile.TemporaryDirectory(prefix="mcp hook fixture ")
        self.addCleanup(self.fixture.cleanup)
        self.root = Path(self.fixture.name)
        self.repo = self.root / "checkout with spaces"
        self.repo.mkdir()
        self.tools = self.root / "tools"
        self.tools.mkdir()
        self.log = self.root / "calls.jsonl"
        self.ipc_log = self.root / "ipc-home"
        self.user_home = self.root / "existing-mcp-state"
        self.user_home.mkdir()
        (self.user_home / "sentinel").write_text("unchanged")
        for tool in ("git", "python3"):
            executable = shutil.which(tool)
            if executable is None:
                raise RuntimeError(f'Prepare {tool} before running these fixtures.')
            (self.tools / tool).symlink_to(executable)
        self.env = dict(os.environ, PATH=str(self.tools), GIT_CONFIG_GLOBAL=os.devnull,
                        GIT_CONFIG_NOSYSTEM="1", HOOK_TEST_LOG=str(self.log),
                        HOOK_IPC_LOG=str(self.ipc_log), MCP_CLI_HOME=str(self.user_home))
        self.git("init", "-q")
        self.git("config", "user.name", "Hook fixture")
        self.git("config", "user.email", "fixture@example.invalid")
        self.git("config", "commit.gpgsign", "false")
        for filename in (".gitattributes", ".githooks/pre-push", "scripts/pre_push.py", "scripts/install-hooks.sh", "rust-toolchain.toml"):
            target = self.repo / filename
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(SOURCE / filename, target)
        (self.repo / "src").mkdir()
        (self.repo / "src/main.rs").write_text("fn main() {}\n")
        (self.repo / "README.md").write_text("fixture\n")
        self.base = self.commit()
        stub = self.tools / "rustup"
        stub.write_text("""#!/usr/bin/env python3
import json,os,pathlib,sys
call=['rustup',*sys.argv[1:]]
assert os.environ['CARGO_NET_OFFLINE']=='true'
with open(os.environ['HOOK_TEST_LOG'],'a') as f:f.write(json.dumps(call)+'\\n')
if 'cargo' in call and 'test' in call:
    home=pathlib.Path(os.environ['MCP_CLI_HOME'])
    assert str(home).startswith('/tmp/mp-hook-') and home.stat().st_mode & 0o777 == 0o700
    assert os.environ['MCP_CLI_CREDENTIALS_READ_ONLY']=='1'
    pathlib.Path(os.environ['HOOK_IPC_LOG']).write_text(str(home))
    (home/'fixture-marker').write_text('temporary only')
if os.environ.get('HOOK_FAIL')==' '.join(call):sys.exit(37)
""")
        stub.chmod(0o755)

    def git(self, *arguments, cwd=None):
        return subprocess.check_output(["git", *arguments], cwd=cwd or self.repo, env=self.env,
                                       stderr=subprocess.DEVNULL).decode().strip()

    def commit(self):
        self.git("add", ".")
        self.git("-c", "core.hooksPath=/dev/null", "commit", "-qm", "fixture")
        return self.git("rev-parse", "HEAD")

    def update(self, filename="src/file with spaces.rs"):
        (self.repo / filename).write_text("updated\n")
        return self.commit()

    def record(self, local=None, remote=None):
        return f"refs/heads/local {local or self.git('rev-parse', 'HEAD')} refs/heads/remote {remote or self.base}\n"

    def hook(self, stdin, cwd=None, **extra):
        checkout = cwd or self.repo
        return subprocess.run([str(checkout / ".githooks/pre-push"), "origin", "unused"],
                              cwd=checkout, env=dict(self.env, **extra), input=stdin,
                              text=True, capture_output=True)

    def calls(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []

    def install(self, cwd=None):
        checkout = cwd or self.repo
        return subprocess.run([str(checkout / "scripts/install-hooks.sh")], cwd=checkout,
                              env=self.env, capture_output=True, text=True)

    def test_spaces_full_graph_and_private_ipc_cleanup(self):
        self.update()
        result = self.hook(self.record())
        self.assertEqual(result.returncode, 0, result.stderr)
        for command in (FORMAT, CHECK, IPC):
            self.assertIn(command, self.calls())
        self.assertFalse(Path(self.ipc_log.read_text()).exists())
        self.assertEqual((self.user_home / "sentinel").read_text(), "unchanged")
        self.assertEqual(self.git("status", "--porcelain"), "")

    def test_new_branch_unknown_empty_and_unchanged_ranges(self):
        self.update()
        for stdin in (self.record(remote=ZERO), self.record(remote="f" * 40), "", self.record(remote=self.git("rev-parse", "HEAD"))):
            with self.subTest(stdin=stdin):
                self.log.unlink(missing_ok=True)
                result = self.hook(stdin)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn(IPC, self.calls())

    def test_shared_config_rename_and_deletion(self):
        self.git("mv", "src/main.rs", "src/renamed file.rs")
        (self.repo / "Cargo.lock").write_text("fixture lock\n")
        renamed_commit = self.commit()
        self.assertEqual(self.hook(self.record()).returncode, 0)
        self.assertIn(IPC, self.calls())
        (self.repo / "src/renamed file.rs").unlink()
        self.commit()
        self.log.unlink()
        self.assertEqual(self.hook(self.record(remote=renamed_commit)).returncode, 0)
        self.assertIn(FORMAT, self.calls())
        self.assertIn(CHECK, self.calls())
        self.assertIn(IPC, self.calls())

    def test_doc_only_keeps_format_and_check_without_ipc(self):
        self.update("README.md")
        result = self.hook(self.record())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(FORMAT, self.calls())
        self.assertIn(CHECK, self.calls())
        self.assertNotIn(IPC, self.calls())

    def test_multiple_refs_and_deletion_only(self):
        self.update()
        self.assertEqual(self.hook(self.record() + self.record(remote=ZERO)).returncode, 0)
        self.log.unlink()
        (self.repo / "dirty").write_text("untracked")
        self.assertEqual(self.hook(self.record(local=ZERO)).returncode, 0)
        self.assertEqual(self.calls(), [])

    def test_wrong_ref_dirty_and_malformed_fail_before_tools(self):
        self.update()
        for stdin in (self.record(local=self.base), self.record() + self.record(local=self.base), "bad record\n", self.record(local="x" * 40)):
            self.assertNotEqual(self.hook(stdin).returncode, 0)
            self.assertEqual(self.calls(), [])
        (self.repo / "dirty").write_text("untracked")
        self.assertNotEqual(self.hook(self.record()).returncode, 0)
        self.assertEqual(self.calls(), [])

    def test_nonzero_propagation_and_ipc_cleanup_on_failure(self):
        self.update()
        for command in (FORMAT, CHECK, IPC, ["rustup", "run", TOOLCHAIN, "rustc", "--version"]):
            with self.subTest(command=command):
                self.assertEqual(self.hook(self.record(), HOOK_FAIL=" ".join(command)).returncode, 37)
                if self.ipc_log.exists():
                    self.assertFalse(Path(self.ipc_log.read_text()).exists())

    def test_missing_tool_is_actionable(self):
        self.update()
        (self.tools / "rustup").unlink()
        result = self.hook(self.record())
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("prepare rustup", result.stderr)

    def test_git_status_failure_cannot_become_green(self):
        self.update()
        real_git = shutil.which("git")
        wrapper = self.tools / "git"
        wrapper.unlink()
        wrapper.write_text(f"#!/usr/bin/env python3\nimport os,sys\nif sys.argv[1:2] == ['status']: sys.exit(41)\nos.execv({real_git!r}, [{real_git!r}, *sys.argv[1:]])\n")
        wrapper.chmod(0o755)
        self.assertEqual(self.hook(self.record()).returncode, 41)
        self.assertEqual(self.calls(), [])

    def test_symlinked_default_hook_directory_is_preserved(self):
        hooks = self.repo / ".git/hooks"
        shutil.rmtree(hooks)
        existing = self.root / "existing hooks"
        existing.mkdir()
        active = existing / "pre-commit"
        active.write_text("existing unrelated hook")
        active.chmod(0o755)
        hooks.symlink_to(existing, target_is_directory=True)
        self.assertNotEqual(self.install().returncode, 0)
        self.assertEqual(active.read_text(), "existing unrelated hook")
        self.assertTrue(hooks.is_symlink())

    def test_autocrlf_checkout_keeps_shell_entrypoints_executable(self):
        self.git("config", "core.autocrlf", "true")
        self.git("checkout-index", "--force", "--all")
        for path in (".githooks/pre-push", "scripts/install-hooks.sh"):
            self.assertNotIn(b"\r\n", (self.repo / path).read_bytes())
        self.assertEqual(self.install().returncode, 0)
        result = self.hook(self.record(remote=ZERO))
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_installer_idempotent_and_refuses_unrelated_hooks(self):
        self.assertEqual(self.install().returncode, 0)
        self.assertEqual(self.install().returncode, 0)
        self.assertEqual(self.git("config", "--get", "core.hooksPath"), ".githooks")
        self.git("config", "--unset", "core.hooksPath")
        active = self.repo / ".git/hooks/pre-commit"
        active.write_text("existing hook")
        self.assertNotEqual(self.install().returncode, 0)
        self.assertEqual(active.read_text(), "existing hook")
        self.git("config", "core.hooksPath", "unrelated")
        self.assertNotEqual(self.install().returncode, 0)
        self.assertEqual(self.git("config", "--get", "core.hooksPath"), "unrelated")

    def test_linked_worktree_installer_and_executable(self):
        linked = self.root / "linked worktree"
        self.git("worktree", "add", "-qb", "fixture-linked", str(linked))
        self.assertEqual(self.install(cwd=linked).returncode, 0)
        result = self.hook(self.record(remote=ZERO), cwd=linked)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(IPC, self.calls())

    def run_workflow_step(self, mktemp_exit=0, cargo_exit=0):
        body = (SOURCE / 'scripts/test_unix_ipc.sh').read_text()
        shell = shutil.which('bash')
        if shell is None:
            raise RuntimeError('Prepare Bash before running workflow fixtures.')
        functions = '''
mktemp() { if [ "$WORKFLOW_MKTEMP_EXIT" != 0 ]; then return "$WORKFLOW_MKTEMP_EXIT"; fi; printf '%s\\n' "$WORKFLOW_HOME"; }
cargo() { printf '%s' "$MCP_CLI_HOME" > "$WORKFLOW_CARGO_MARKER"; return "$WORKFLOW_CARGO_EXIT"; }
rm() { printf '%s' "$*" > "$WORKFLOW_CLEANUP_MARKER"; }
'''
        env = dict(self.env, WORKFLOW_MKTEMP_EXIT=str(mktemp_exit), WORKFLOW_CARGO_EXIT=str(cargo_exit),
                   WORKFLOW_HOME=str(self.root / 'private workflow home'),
                   WORKFLOW_CARGO_MARKER=str(self.root / 'workflow-cargo'),
                   WORKFLOW_CLEANUP_MARKER=str(self.root / 'workflow-cleanup'))
        return subprocess.run([shell, '--noprofile', '--norc', '-e', '-o', 'pipefail', '-c', functions + body],
                              cwd=self.repo, env=env, text=True, capture_output=True)

    def test_workflow_temp_creation_failure_stops_before_cargo(self):
        result = self.run_workflow_step(mktemp_exit=41)
        self.assertEqual(result.returncode, 41, result.stdout + result.stderr)
        self.assertFalse((self.root / 'workflow-cargo').exists())

    def test_workflow_uses_private_home_and_preserves_cargo_failure(self):
        result = self.run_workflow_step(cargo_exit=37)
        self.assertEqual(result.returncode, 37, result.stdout + result.stderr)
        self.assertEqual((self.root / 'workflow-cargo').read_text(), str(self.root / 'private workflow home'))
        self.assertIn(str(self.root / 'private workflow home'), (self.root / 'workflow-cleanup').read_text())


if __name__ == "__main__":
    unittest.main()
