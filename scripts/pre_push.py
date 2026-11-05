"""Validate the clean checked-out push without touching MCP accounts or state."""
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile


def git(*arguments, check=True):
    return subprocess.run(["git", *arguments], check=check, stdout=subprocess.PIPE,
                          stderr=None if check else subprocess.DEVNULL)


def pushed_paths():
    head = git("rev-parse", "HEAD").stdout.strip().decode()
    paths = set()
    full = False
    updates = False
    records = sys.stdin.read().splitlines()
    if not records:
        full = True
        updates = True
    for record in records:
        fields = record.split()
        if len(fields) != 4:
            raise ValueError("invalid pre-push input; expected four fields per ref")
        _, local, _, remote = fields
        if not all(re.fullmatch(r"[0-9a-fA-F]{40}|[0-9a-fA-F]{64}", object_id) for object_id in (local, remote)):
            raise ValueError("invalid pre-push object ID")
        if set(local) == {"0"}:
            continue
        updates = True
        commit = git("rev-parse", "--verify", local + "^{commit}").stdout.strip().decode()
        if commit != head:
            raise ValueError("pushed ref is not HEAD; validate it in its own clean checkout")
        if set(remote) == {"0"}:
            full = True
            continue
        base = git("rev-parse", "--verify", remote + "^{commit}", check=False)
        if base.returncode:
            full = True
            continue
        diff = git("diff", "--name-only", "--no-renames", "-z", base.stdout.strip().decode(), head)
        paths.update(os.fsdecode(path) for path in diff.stdout.split(b"\0") if path)
    if not updates:
        return None
    status = git("status", "--porcelain=v1", "--untracked-files=normal").stdout
    if status:
        raise ValueError("checkout must be clean, including untracked files; never stash automatically")
    if full or not paths:
        return {os.fsdecode(path) for path in git("ls-files", "-z").stdout.split(b"\0") if path}
    return paths


def run(*arguments):
    print("pre-push:", " ".join(arguments), flush=True)
    subprocess.run(arguments, check=True)


def main():
    os.chdir(os.fsdecode(git("rev-parse", "--show-toplevel").stdout.strip()))
    paths = pushed_paths()
    if paths is None:
        print("pre-push: deletion-only update; no code to validate")
        return
    if sys.version_info < (3, 11):
        raise ValueError("prepare Python 3.11+ before pushing")
    if not shutil.which("rustup"):
        raise ValueError("prepare rustup and the pinned toolchain first; see README.md")
    import tomllib
    toolchain = tomllib.loads(Path("rust-toolchain.toml").read_text())["toolchain"]["channel"]
    os.environ.update(CARGO_NET_OFFLINE="true", RUSTUP_NO_UPDATE_CHECK="1")
    # rustup run refuses missing prepared toolchains; it does not install one.
    run("rustup", "run", toolchain, "rustc", "--version")
    run("rustup", "run", toolchain, "rustfmt", "--version")
    cargo = ("rustup", "run", toolchain, "cargo")
    run(*cargo, "fmt", "--all", "--", "--check")
    run(*cargo, "check", "--offline", "--locked", "--bin", "mcp-cli")
    relevant = any(not path.endswith(".md") or path.startswith(("src/", "tests/", "scripts/", ".githooks/", ".cargo/")) for path in paths)
    if relevant:
        if os.name != "posix":
            print("pre-push: Unix IPC gate requires Unix CI; this host check is not Unix acceptance")
            return
        # Keep sockets out of real MCP state and below macOS's socket path limit.
        with tempfile.TemporaryDirectory(prefix="mp-hook-", dir="/tmp") as home:
            os.environ["MCP_CLI_HOME"] = home
            os.environ["MCP_CLI_CREDENTIALS_READ_ONLY"] = "1"
            run(*cargo, "test", "--offline", "--locked", "--bin", "mcp-cli", "--",
                "local_security", "transport::tests", "--test-threads=1")


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as error:
        sys.exit(error.returncode if 0 < error.returncode < 256 else 1)
    except (ValueError, OSError, KeyError) as error:
        print(f"pre-push: {error}", file=sys.stderr)
        sys.exit(1)
