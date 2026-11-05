#!/usr/bin/env python3
"""Run CLI UX scenarios against isolated mcp-cli and mcporter; retain transcripts.

Only synthetic credentials, disposable config, and loopback servers are used.
No browser consent, external tool calls, legacy retirement, or user daemon access.
"""

import argparse
import http.client
import json
import os
import queue
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

from cli_ux_process import OwnedProcesses

HERE = Path(__file__).resolve().parent
HELP = [[], ["list"], ["describe"], ["list-tools"], ["call"], ["resource"], ["resources"],
        ["auth"], ["vault"], ["vault", "set"], ["vault", "clear"], ["config"],
        *[["config", command] for command in ("list", "get", "add", "remove", "import", "login", "logout", "doctor")],
        ["import"], ["daemon"], *[["daemon", command] for command in ("start", "status", "stop", "restart", "migrate")],
        ["serve"]]
NATIVE = [["pool"], ["pool", "list"], ["pool", "serve"],
          *[[command] for command in ("start", "stop", "restart", "status", "add", "remove", "proxy", "shutdown")]]


def read_line(stream):
    lines = queue.Queue()
    threading.Thread(target=lambda: lines.put(stream.readline()), daemon=True).start()
    try:
        return lines.get(timeout=10)
    except queue.Empty as error:
        raise RuntimeError("RPC fixture did not respond within 10 seconds") from error


class Audit:
    def __init__(self, product, binary, output):
        self.product, self.binary, self.output = product, binary, output
        self.processes = OwnedProcesses()
        self.root = Path(tempfile.mkdtemp(prefix="ux-", dir="/tmp"))
        self.config = self.root / "config.json"
        self.rows = []
        try:
            self.setup()
        except BaseException:
            try:
                self.processes.close()
            except Exception as error:
                raise RuntimeError(f"Setup rollback unverified; state retained at {self.root}") from error
            shutil.rmtree(self.root)
            raise

    def setup(self):
        self.environment = {key: value for key, value in os.environ.items()
                            if not key.startswith(("MCP_", "MCPORTER_", "XDG_", "NODE_"))
                            and key not in ("HOME", "USERPROFILE", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY",
                                            "http_proxy", "https_proxy", "all_proxy")}
        for name in ("home", "data", "cache", "state", "config", "runtime", "legacy"):
            (self.root / name).mkdir(mode=0o700)
        self.environment.update({
            "HOME": str(self.root / "home"), "USERPROFILE": str(self.root / "home"),
            "XDG_DATA_HOME": str(self.root / "data"), "XDG_CACHE_HOME": str(self.root / "cache"),
            "XDG_CONFIG_HOME": str(self.root / "config"), "XDG_STATE_HOME": str(self.root / "state"),
            "XDG_RUNTIME_DIR": str(self.root / "runtime"), "MCP_CLI_HOME": str(self.root / "pool"),
            "MCPORTER_DAEMON_DIR": str(self.root / "legacy"), "NO_COLOR": "1", "TERM": "dumb",
            "NO_PROXY": "127.0.0.1,localhost",
        })
        self.base = [self.binary, "--config", str(self.config), "--root", str(self.root)]
        self.http = self.processes.spawn([sys.executable, str(HERE / "cli_ux_fixture.py"), "--http"],
                                         stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        port = read_line(self.http.stdout).strip()
        if not port.isdecimal():
            raise RuntimeError(f"Invalid HTTP fixture port {port!r}")
        self.url = f"http://127.0.0.1:{port}/mcp"
        definition = {"command": sys.executable, "args": [str(HERE / "cli_ux_fixture.py")],
                      "description": "Local CLI UX fixture.", "protocolVersion": "legacy", "lifecycle": "keep-alive"}
        self.config.write_text(json.dumps({"imports": [], "mcpServers": {
            "fixture": definition, "remote": {"url": self.url, "protocolVersion": "legacy"},
            "protected": {"url": self.url, "auth": "oauth", "protocolVersion": "legacy"}}}))
        self.tokens = self.root / "tokens.json"
        self.tokens.write_text(json.dumps({"tokens": {"access_token": "synthetic-audit-token", "token_type": "Bearer",
                                                      "expires_at": 4102444800},
                                          "clientInfo": {"client_id": "synthetic-audit-client"}}))
        self.source = self.root / "source.json"
        self.source.write_text(json.dumps({"mcpServers": {"imported": definition}, "imports": []}))

    def close(self):
        stopped = False
        try:
            cleanup = subprocess.run(self.base + ["daemon", "stop"], env=self.environment, cwd=self.root,
                                     capture_output=True, text=True, timeout=20)
            if cleanup.returncode:
                raise RuntimeError(f"Isolated daemon cleanup failed; state retained at {self.root}: {cleanup.stderr}")
            stopped = True
        finally:
            try:
                self.processes.close()
            except Exception as error:
                raise RuntimeError(f"Owned child retirement unverified; state retained at {self.root}") from error
            # Do not unlink a possibly live daemon's control socket after failed retirement.
            if stopped:
                shutil.rmtree(self.root)

    def record(self, label, arguments, stdout, stderr, code, expected=0, contains=(), machine=False,
               expected_json=None):
        problems = []
        if code != expected:
            problems.append(f"exit {code}, expected {expected}")
        for text in contains:
            if text not in stdout + stderr:
                problems.append(f"missing {text!r}")
        if "\x1b[" in stdout + stderr:
            problems.append("ANSI escape in redirected output")
        if "synthetic-audit-token" in stdout + stderr:
            problems.append("credential value leaked")
        if machine or expected_json is not None:
            try:
                parsed = json.loads(stdout)
                if expected_json is not None and parsed != expected_json:
                    problems.append(f"incorrect JSON result, expected {expected_json!r}")
            except ValueError:
                problems.append("stdout is not a single JSON document")
        if label == "quiet-failure" and stdout + stderr:
            problems.append("quiet mode emitted output")
        row = {"label": label, "arguments": arguments, "exit": code, "problems": problems,
               "stdout": stdout, "stderr": stderr}
        self.rows.append(row)
        transcript = f"$ {self.product} {' '.join(arguments)}\nexit={code}\n\nSTDOUT\n{stdout}\nSTDERR\n{stderr}"
        (self.output / f"{self.product}-{label}.txt").write_text(transcript)
        return row

    def run(self, label, arguments, expected=0, contains=(), machine=False, stdin=None, expected_json=None):
        try:
            result = subprocess.run(self.base + arguments, env=self.environment, cwd=self.root,
                                    input=stdin, capture_output=True, text=True, timeout=20)
            return self.record(label, arguments, result.stdout, result.stderr, result.returncode,
                               expected, contains, machine, expected_json)
        except subprocess.TimeoutExpired as error:
            stdout, stderr = (
                value.decode("utf-8", errors="replace") if isinstance(value, bytes) else (value or "")
                for value in (error.stdout, error.stderr)
            )
            return self.record(label, arguments, stdout, stderr,
                               124, expected, contains, machine, expected_json)

    def rpc(self, label, arguments):
        process = self.processes.spawn(self.base + arguments, env=self.environment, cwd=self.root,
                                       stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        if process.stdin is None or process.stdout is None or process.stderr is None:
            raise RuntimeError("RPC pipes unavailable")
        responses = []
        try:
            for identifier, (method, parameters) in enumerate([
                ("initialize", {"protocolVersion": "2025-11-25", "capabilities": {},
                                "clientInfo": {"name": "ux-audit", "version": "1"}}),
                ("tools/list", {}),
                ("tools/call", {"name": "fixture__echo" if arguments[0] == "serve" else "echo",
                                "arguments": {"message": "rpc-check"}}),
            ], 1):
                process.stdin.write(json.dumps({"jsonrpc": "2.0", "id": identifier,
                                                "method": method, "params": parameters}) + "\n")
                process.stdin.flush()
                response = json.loads(read_line(process.stdout))
                if response.get("id") != identifier or "error" in response:
                    raise RuntimeError(f"Unexpected RPC response {response!r}")
                responses.append(response)
                if identifier == 1:
                    process.stdin.write('{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
                    process.stdin.flush()
            self.record(label, arguments, json.dumps(responses, indent=2), "", 0,
                        contains=("rpc-check", "echo"))
        except (RuntimeError, ValueError, OSError) as error:
            self.record(label, arguments, json.dumps(responses), str(error), 1)
        finally:
            self.processes.retire(process)

    def foreground(self):
        arguments = ["pool", "serve"] if self.product == "mcp-cli" else ["daemon", "start", "--foreground"]
        process = self.processes.spawn(self.base + arguments, env=self.environment, cwd=self.root,
                                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            try:
                result = self.wait_for_daemon(process)
            except (RuntimeError, ValueError, OSError, subprocess.TimeoutExpired) as error:
                self.record("foreground-daemon", arguments, "", str(error), 1)
            else:
                self.record("foreground-daemon", arguments, result.stdout, result.stderr, result.returncode)
                self.run("foreground-stop", ["daemon", "stop"])
        finally:
            self.processes.retire(process)

    def wait_for_daemon(self, process):
        for _ in range(50):
            result = subprocess.run(self.base + ["daemon", "status", "--json"], env=self.environment,
                                    cwd=self.root, capture_output=True, text=True, timeout=5)
            if result.returncode:
                raise RuntimeError(f"Daemon status exited {result.returncode}: {result.stderr}")
            try:
                status = json.loads(result.stdout)
            except ValueError as error:
                raise ValueError(f"Invalid daemon status JSON: {result.stdout!r}") from error
            if isinstance(status, dict):
                identifier = status.get("pid")
                if status.get("running") is True or (type(identifier) is int and identifier > 0):
                    return result
            elif status is not None:
                raise ValueError(f"Invalid daemon status object: {result.stdout!r}")
            if process.poll() is not None:
                raise RuntimeError("Foreground daemon exited before readiness")
            time.sleep(0.05)
        raise RuntimeError("Foreground daemon did not become ready")

    def http_bridge(self):
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        arguments = ["serve", "--http", str(port), "--servers", "fixture"]
        process = self.processes.spawn(self.base + arguments, env=self.environment, cwd=self.root,
                                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        responses = []
        try:
            session = None
            for identifier, (method, parameters) in enumerate([
                ("initialize", {"protocolVersion": "2025-11-25", "capabilities": {},
                                "clientInfo": {"name": "ux-audit", "version": "1"}}),
                ("tools/list", {}),
                ("tools/call", {"name": "fixture__echo", "arguments": {"message": "http-bridge-check"}}),
            ], 1):
                headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream"}
                if session:
                    headers["Mcp-Session-Id"] = session
                body = json.dumps({"jsonrpc": "2.0", "id": identifier, "method": method, "params": parameters}).encode()
                for attempt in range(50):
                    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
                    try:
                        connection.request("POST", "/mcp", body, headers)
                        response = connection.getresponse()
                        if response.status != 200:
                            raise RuntimeError(f"HTTP bridge returned {response.status}: {response.read()!r}")
                        session = response.getheader("Mcp-Session-Id", session)
                        if "text/event-stream" in response.getheader("Content-Type", ""):
                            data = []
                            while True:
                                encoded = response.readline()
                                if not encoded:
                                    raise RuntimeError("HTTP SSE ended without a correlated response")
                                line = encoded.decode().rstrip("\r\n")
                                if line.startswith("data:"):
                                    data.append(line.removeprefix("data:").lstrip())
                                elif not line and data:
                                    value = json.loads("\n".join(data))
                                    if value.get("id") == identifier:
                                        break
                                    data.clear()
                        else:
                            value = json.loads(response.read())
                        break
                    except ConnectionRefusedError:
                        if identifier != 1 or attempt == 49:
                            raise
                        time.sleep(0.05)
                    finally:
                        connection.close()
                else:
                    raise RuntimeError("HTTP bridge did not become ready")
                if value.get("id") != identifier or "error" in value:
                    raise RuntimeError(f"Unexpected HTTP bridge response {value!r}")
                responses.append(value)
            self.record("serve-http", arguments, json.dumps(responses), "", 0, contains=("http-bridge-check",))
        except (OSError, RuntimeError, ValueError) as error:
            self.record("serve-http", arguments, json.dumps(responses), str(error), 1)
        finally:
            self.processes.retire(process)

    def exercise(self):
        for command in HELP + (NATIVE if self.product == "mcp-cli" else []):
            expected = (f"Usage: mcp-cli {' '.join(command)}",) if self.product == "mcp-cli" and len(command) == 2 else ()
            self.run("help-" + ("-".join(command) or "root"), command + ["--help"], contains=expected)
        self.run("help-command", ["help", "call"], contains=("Usage",))
        for command in ("config", "daemon"):
            self.run(command + "-help", [command, "help"])
        self.run("version", ["--version"])
        for arguments in (["config", "list"], ["config", "list", "--json"], ["config", "get", "fixture"],
                          ["config", "get", "fixture", "--json"], ["config", "doctor"]):
            expected = () if self.product == "mcporter" and arguments == ["config", "doctor"] else ("fixture",)
            self.run("-".join(arguments).replace("--", ""), list(arguments), contains=expected)
        self.run("config-get-details", ["config", "get", "fixture"], contains=(sys.executable, "cli_ux_fixture.py"))
        self.run("daemon-initial", ["daemon", "status", "--json"], machine=True)
        self.run("daemon-start", ["daemon", "start"])
        for suffix in ([], ["--json"], ["--sources"], ["--status"]):
            self.run("list-all-" + ("-".join(suffix) or "text"), ["list", *suffix, "--no-oauth", "--timeout", "2000"],
                     contains=("fixture",), machine="--json" in suffix)
        for suffix in ([], ["--brief"], ["--schema"], ["--all-parameters"], ["--json"]):
            expected = ("echo", "optional (2): offset, metadata") if not suffix else ("echo",)
            self.run("list-fixture-" + ("-".join(suffix) or "text"), ["list", "fixture", *suffix],
                     contains=expected, machine="--json" in suffix)
        for command in ("describe", "list-tools"):
            self.run(command, [command, "fixture", "--brief"], contains=("echo",))
        self.run("list-tool", ["list", "fixture.echo", "--json"], machine=True, contains=("message",))
        self.run("inferred-server", ["fixture", "--brief"], contains=("echo",))
        self.run("inferred-call", ["fixture.echo", "message=inferred"], contains=("inferred",))
        self.run("inferred-server-tool", ["fixture", "echo", "message=server-first"], contains=("server-first",))
        self.run("adhoc-stdio", ["call", "--stdio", sys.executable, "--stdio-arg", str(HERE / "cli_ux_fixture.py"),
                                 "--name", "adhoc", "echo", "message=adhoc-stdio"], contains=("adhoc-stdio",))
        self.run("adhoc-http", ["call", "--http-url", self.url, "--allow-http", "--name", "adhoc-http",
                                "echo", "message=adhoc-http"], contains=("adhoc-http",))
        self.run("call-typed", ["call", "fixture.echo", "message=hello", "count:2", "verbose:true", "--output", "json"],
                 machine=True, expected_json={"message": "hello", "count": 2, "verbose": True})
        for mode in ("auto", "text", "markdown", "json", "raw"):
            self.run("call-" + mode, ["call", "fixture.echo", "message=hello", "--output", mode], contains=("hello",),
                     machine=mode == "json" or (mode == "raw" and self.product == "mcp-cli"))
        self.run("call-plain-text", ["call", "fixture.plain_text"], contains=("Hello from local fixture.",))
        self.run("call-stdin", ["call", "fixture.echo", "--args", "-", "--output", "json"],
                 stdin='{"message":"stdin-check"}', contains=("stdin-check",), machine=True)
        self.run("call-expression", ["call", 'fixture.echo(message: "expression")'], contains=("expression",))
        self.run("call-remote", ["call", "remote.echo", "message=remote-check"], contains=("remote-check",))
        self.run("call-tool-error", ["call", "fixture.fail"], expected=1, contains=("Intentional fixture failure",))
        for label, arguments in (
            ("unknown-server", ["list", "missing"]), ("unknown-tool", ["call", "fixture.ech"]),
            ("unknown-flag", ["list", "fixture", "--nonsense"]),
            ("invalid-json", ["call", "fixture.echo", "--args", "{bad}"]),
            ("missing-call", ["call"]), ("missing-resource", ["resource"]),
            ("missing-config-get", ["config", "get"]),
            ("quiet-failure", ["list", "protected", "--quiet", "--no-oauth"]),
        ):
            self.run(label, arguments, expected=1)
        self.run("resource-list", ["resource", "fixture"], contains=("fixture://guide",))
        self.run("resource-read", ["resource", "fixture", "fixture://guide"], contains=("Fixture guide",))
        self.run("resource-json", ["resource", "fixture", "fixture://guide", "--output", "json"], machine=True)
        self.run("vault-set", ["vault", "set", "protected", "--tokens-file", str(self.tokens)])
        self.run("auth-cached", ["auth", "protected", "--no-browser"])
        self.run("login-cached", ["config", "login", "protected", "--no-browser"])
        self.run("doctor-cached", ["config", "doctor"],
                 contains=("protected: cached OAuth credentials valid",) if self.product == "mcp-cli" else ())
        self.run("logout", ["config", "logout", "protected"])
        self.run("vault-stdin", ["vault", "set", "protected", "--stdin"], stdin=self.tokens.read_text())
        self.run("vault-clear", ["vault", "clear", "protected"])
        self.run("config-add", ["config", "add", "added", "--command", sys.executable, "--arg", str(HERE / "cli_ux_fixture.py")])
        self.run("config-added", ["config", "get", "added", "--json"], machine=True, contains=("added",))
        self.run("config-remove", ["config", "remove", "added"])
        self.run("config-removed", ["config", "get", "added"], expected=1)
        if self.product == "mcp-cli":
            self.run("import-dry", ["import", "mcporter", "--from", str(self.source), "--dry-run"], contains=("imported",))
            self.run("config-import", ["config", "import", "mcporter", "--from", str(self.source)], contains=("imported",))
            self.run("config-import-repeat", ["config", "import", "mcporter", "--from", str(self.source)], contains=("imported",))
            self.run("resources-alias", ["resources", "fixture"], contains=("fixture://guide",))
            self.run("pool-list", ["pool", "list"], contains=("fixture",))
            self.run("start", ["start", "fixture"], contains=("started",))
            self.run("status", ["status"], contains=("fixture", "READINESS"))
            self.run("status-json", ["status", "fixture", "--json"], machine=True)
            self.run("restart", ["restart", "fixture"], contains=("restarted",))
            self.rpc("proxy", ["proxy", "fixture"])
            self.rpc("pool-proxy", ["pool", "proxy", "fixture"])
            self.run("stop", ["stop", "fixture"], contains=("stopped",))
            self.run("native-add", ["add", "native", "--", sys.executable, str(HERE / "cli_ux_fixture.py")])
            self.run("native-remove", ["remove", "native", "--yes"])
            self.run("daemon-migrate", ["daemon", "migrate"], machine=True)
        else:
            self.run("config-import", ["config", "import", "claude", "--path", str(self.source), "--copy"], contains=("imported",))
            self.run("daemon-migrate", ["daemon", "migrate", "--json"], machine=True)
        self.rpc("serve-stdio", ["serve", "--stdio", "--servers", "fixture"])
        self.http_bridge()
        self.run("daemon-status", ["daemon", "status"])
        self.run("daemon-restart", ["daemon", "restart"])
        self.run("daemon-stop", ["daemon", "stop"])
        self.run("daemon-stopped", ["daemon", "status", "--json"], machine=True)
        self.foreground()
        if self.product == "mcp-cli":
            original = self.config.read_text()
            try:
                self.config.write_text(self.source.read_text())
                self.run("start-all", ["start"], contains=("imported", "started"))
            finally:
                self.config.write_text(original)
            self.run("shutdown", ["shutdown"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mcp-cli", default="target/debug/mcp-cli")
    parser.add_argument("--mcporter", default="mcporter")
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()
    arguments.output.mkdir(parents=True, exist_ok=False)
    reports = {}
    for product, executable in (("mcp-cli", arguments.mcp_cli), ("mcporter", arguments.mcporter)):
        binary = shutil.which(executable)
        if binary is None:
            parser.error(f"Executable not found: {executable}")
        audit = Audit(product, str(Path(binary).absolute()), arguments.output)
        try:
            audit.exercise()
        finally:
            reports[product] = audit.rows
            try:
                (arguments.output / "results.json").write_text(json.dumps(reports, indent=2) + "\n")
            finally:
                audit.close()
        failures = [row for row in audit.rows if row["problems"]]
        kind = "need review" if product == "mcp-cli" else "reference differences"
        print(f"{product}: {len(audit.rows)} scenarios; {len(failures)} {kind}")
        for row in failures:
            print(f"  {row['label']}: {'; '.join(row['problems'])}")
    return int(any(row["problems"] for row in reports["mcp-cli"]))


if __name__ == "__main__":
    raise SystemExit(main())
