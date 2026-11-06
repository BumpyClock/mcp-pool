"""Owned benchmark processes, isolated configuration, counters, and OS samples."""

import collections
import json
import os
import platform
import queue
import resource
import shutil
import subprocess
import sys
import threading
import time
import uuid
from pathlib import Path

HERE = Path(__file__).resolve().parent


def process_table():
    text = subprocess.check_output(
        ["ps", "-axo", "pid=,ppid=,rss=,time=,comm="], text=True, timeout=5)
    table = {}
    for line in text.splitlines():
        fields = line.split(None, 4)
        if len(fields) != 5:
            continue
        identifier, parent, resident, cpu, command = fields
        days, separator, clock = cpu.rpartition("-")
        seconds = sum(float(part) * 60 ** position
                      for position, part in enumerate(reversed(clock.split(":"))))
        table[int(identifier)] = {
            "parent": int(parent), "rss_kib": int(resident),
            "cpu_s": seconds + (int(days) * 86400 if separator else 0), "command": command,
        }
    return table


def descendants(table, roots):
    found = set(roots) & table.keys()
    while True:
        added = {identifier for identifier, row in table.items()
                 if row["parent"] in found} - found
        if not added:
            return found
        found.update(added)


def read_line(stream, timeout=10):
    lines = queue.Queue()

    def read():
        lines.put(stream.readline())

    threading.Thread(target=read, daemon=True).start()
    try:
        return lines.get(timeout=timeout)
    except queue.Empty as error:
        raise RuntimeError("Timed out waiting for fixture readiness") from error


def decode_json(contents, source):
    try:
        return json.loads(contents)
    except json.JSONDecodeError as error:
        raise RuntimeError(f"Invalid JSON from {source}") from error


def validate_output(value, token, size=64):
    if not isinstance(value, dict) or value.get("isError") or "error" in value:
        raise RuntimeError(f"Tool returned error or invalid output: {value!r}")
    if "content" in value:
        content = value["content"]
        if len(content) != 1 or content[0].get("type") != "text":
            raise RuntimeError(f"Unexpected tool content: {value!r}")
        value = decode_json(content[0]["text"], "tool content")
    if value != {"token": token, "payload": "x" * size}:
        raise RuntimeError(f"Incorrect output for token {token}: {value!r}")


class Runtime:
    def __init__(self, product, binary, transport, protocol, output, label):
        self.product, self.binary = product, binary
        self.transport, self.protocol = transport, protocol
        self.output, self.label = Path(output), label
        # macOS TMPDIR and checkout paths leave too little room for pool socket identities.
        self.root = Path("/tmp") / ("mpb-" + uuid.uuid4().hex[:12])
        self.events = self.output / f"{label}.events.jsonl"
        self.expected = set()
        self.processes, self.logs = [], []
        self.daemon = self.http = None
        self.closed = False
        self.created = False
        self.config = self.root / "c.json"
        self.base = [binary, "--config", str(self.config), "--root", str(self.root)]
        self.environment = {
            key: value for key, value in os.environ.items()
            if not key.startswith(("MCP_", "MCPORTER_", "XDG_", "NODE_"))
            and key not in {"HOME", "USERPROFILE", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY",
                            "http_proxy", "https_proxy", "all_proxy"}
        }
        self.environment.update({
            "HOME": str(self.root / "h"), "USERPROFILE": str(self.root / "h"),
            "XDG_CONFIG_HOME": str(self.root / "x"), "XDG_DATA_HOME": str(self.root / "d"),
            "XDG_CACHE_HOME": str(self.root / "k"), "XDG_STATE_HOME": str(self.root / "s"),
            "XDG_RUNTIME_DIR": str(self.root / "r"), "TMPDIR": str(self.root / "t"),
            "MCP_CLI_HOME": str(self.root), "MCPORTER_DAEMON_DIR": str(self.root / "m"),
            "MCP_CLI_CREDENTIALS_READ_ONLY": "1", "NO_PROXY": "127.0.0.1,localhost",
        })

    def __enter__(self):
        try:
            self.root.mkdir(mode=0o700)
            self.created = True
            for name in ("h", "x", "d", "k", "s", "r", "t", "m"):
                (self.root / name).mkdir(mode=0o700)
            self.events.write_text("")
            definition: dict[str, object] = {"protocolVersion": "legacy" if self.protocol == "legacy" else "2026-07-28",
                          "lifecycle": "keep-alive"}
            command = [sys.executable, str(HERE / "fixture.py"), "--events", str(self.events),
                       "--protocol", self.protocol]
            if self.transport == "stdio":
                definition.update(command=command[0], args=command[1:], cwd=str(self.root))
            else:
                self.http = self.spawn(command + ["--http"], "fixture")
                port = read_line(self.http.stdout).strip()
                if not port.isdecimal():
                    raise RuntimeError(f"HTTP fixture did not report a port: {port!r}")
                definition["url"] = f"http://127.0.0.1:{port}/mcp"
            self.config.write_text(json.dumps({"imports": [], "mcpServers": {"fixture": definition}}))
            return self
        except BaseException:
            self.close()
            raise

    def __exit__(self, exception_type, exception, traceback):
        self.close()

    def spawn(self, command, suffix, pipes=True):
        log = (self.output / f"{self.label}.{suffix}.stderr").open("a")
        self.logs.append(log)
        process = subprocess.Popen(
            command, env=self.environment, cwd=self.root, text=True, bufsize=1,
            stdin=subprocess.PIPE if pipes else subprocess.DEVNULL,
            stdout=subprocess.PIPE if pipes else subprocess.DEVNULL, stderr=log,
        )
        self.processes.append(process)
        return process

    def command(self, arguments, cpu=False, timeout=20):
        before = resource.getrusage(resource.RUSAGE_CHILDREN) if cpu else None
        started = time.perf_counter_ns()
        process = self.spawn(self.base + arguments, f"cli-{uuid.uuid4().hex[:8]}")
        try:
            stdout, _ = process.communicate(timeout=timeout)
        except BaseException:
            self.terminate(process)
            raise
        row: dict[str, int | float] = {"wall_ns": time.perf_counter_ns() - started}
        if before is not None:
            after = resource.getrusage(resource.RUSAGE_CHILDREN)
            row["client_waited_cpu_s"] = (
                after.ru_utime + after.ru_stime - before.ru_utime - before.ru_stime)
        if process.returncode:
            raise RuntimeError(f"{self.label}: command failed ({process.returncode}): "
                               f"{arguments!r}; stdout={stdout!r}; see stderr artifacts")
        return stdout, row

    def start_daemon(self):
        command = ["pool", "serve"] if self.product == "mcp-cli" else ["daemon", "start", "--foreground"]
        self.daemon = self.spawn(self.base + command, "daemon", pipes=False)
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            if self.daemon.poll() is not None:
                raise RuntimeError("Owned foreground daemon exited before readiness")
            try:
                stdout, _ = self.command(["daemon", "status", "--json"], timeout=5)
            except RuntimeError:
                pass
            else:
                status = decode_json(stdout, "daemon status output")
                ready = isinstance(status, dict) and (
                    status.get("running") is True
                    if self.product == "mcp-cli" else isinstance(status.get("pid"), int))
                if ready:
                    return
            time.sleep(0.05)
        raise RuntimeError("Owned foreground daemon did not become ready")

    def token(self):
        token = f"{self.label}-{uuid.uuid4().hex}"
        self.expected.add(token)
        return token

    def call(self, token, delay=0, size=64, cpu=False):
        stdout, row = self.command([
            "call", "fixture.echo", "--args", json.dumps({
                "token": token, "delayMs": delay, "payloadBytes": size}),
            "--output", "json", "--timeout", "10000", "--no-oauth",
        ], cpu=cpu)
        validate_output(decode_json(stdout, "CLI call output"), token, size)
        return {**row, "token": token}

    def discover(self):
        stdout, row = self.command(["list", "fixture", "--json", "--timeout", "10000", "--no-oauth"],
                                   cpu=True)
        tools = decode_json(stdout, "CLI discovery output").get("tools")
        if not isinstance(tools, list) or len(tools) != 25 or not any(tool.get("name") == "echo" for tool in tools):
            raise RuntimeError(f"Incorrect discovery: {stdout}")
        return row

    def records(self):
        return [decode_json(line, self.events.name) for line in self.events.read_text().splitlines()]

    def verify(self):
        records = self.records()
        counts = collections.Counter(row["event"] for row in records)
        methods = collections.Counter(row["method"] for row in records if row["event"] == "request")
        started = collections.Counter(row["token"] for row in records if row["event"] == "call_started")
        complete = collections.Counter(row["token"] for row in records if row["event"] == "call_complete")
        expected = collections.Counter(self.expected)
        if started != expected or complete != expected or methods["tools/call"] != len(self.expected):
            raise RuntimeError(f"Calls skipped, replayed, or unfinished: expected={len(expected)}, "
                               f"started={sum(started.values())}, completed={sum(complete.values())}")
        if counts["spawn"] != 1 or counts["error"] or counts["http_error"]:
            raise RuntimeError(f"Fixture spawn/error invariant failed: {dict(counts)}")
        handshake = "initialize" if self.protocol == "legacy" else "server/discover"
        if methods[handshake] < 1:
            raise RuntimeError(f"Required {handshake} did not reach fixture")
        return {"events": dict(counts), "methods": dict(methods), "unique_verified_tokens": len(expected),
                "fixture_service_ns": sum(row.get("service_ns", 0) for row in records),
                "fixture_thread_cpu_ns": sum(row.get("thread_cpu_ns", 0) for row in records)}

    def snapshot(self, clients=()):
        table = process_table()
        persistent_roots = [process.pid for process in (self.daemon, self.http)
                            if process is not None and process.poll() is None]
        persistent = descendants(table, persistent_roots)
        client_roots = [process.pid for process in clients if process.poll() is None]
        client_tree = descendants(table, client_roots) - persistent
        fixture_roots = {row["pid"] for row in self.records() if row["event"] == "spawn"}
        fixture_tree = descendants(table, fixture_roots)

        def group(identifiers):
            return {"rss_kib": sum(table[identifier]["rss_kib"] for identifier in identifiers),
                    "cpu_s": sum(table[identifier]["cpu_s"] for identifier in identifiers),
                    "processes": [{"pid": identifier, **table[identifier]}
                                  for identifier in sorted(identifiers)]}

        return {"monotonic_ns": time.monotonic_ns(), "persistent_tree": group(persistent),
                "clients_only_tree": group(client_tree), "fixture_subset": group(fixture_tree),
                "combined_tree": group(persistent | client_tree)}

    @staticmethod
    def terminate(process):
        try:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=3)
        finally:
            for stream in (process.stdin, process.stdout):
                if stream is not None and not stream.closed:
                    stream.close()

    def close(self):
        if self.closed:
            return
        if not self.created:
            self.closed = True
            return
        errors = []
        try:
            # The namespace also addresses cold daemons that the CLI launched for us.
            if self.config.exists():
                self.command(["daemon", "stop"], timeout=15)
        except Exception as error:
            errors.append(str(error))
        finally:
            for process in reversed(self.processes):
                try:
                    self.terminate(process)
                except Exception as error:
                    errors.append(str(error))
            for log in self.logs:
                try:
                    log.close()
                except Exception as error:
                    errors.append(str(error))
        if not errors:
            try:
                shutil.rmtree(self.root)
            except Exception as error:
                errors.append(str(error))
        # A recorded PID is historical evidence, not authority to signal a process.
        (self.output / f"{self.label}.cleanup.json").write_text(json.dumps({
            "namespace": str(self.root),
            "owned_pids": [process.pid for process in self.processes],
            "errors": errors,
        }, indent=2))
        if errors:
            raise RuntimeError(f"Cleanup failed; namespace retained at {self.root}: " + "; ".join(errors))
        self.closed = True


SAMPLE_METADATA = {
    "source": "ps -axo pid=,ppid=,rss=,time=,comm=",
    "rss_unit": "KiB; sum of resident pages, shared pages may be counted repeatedly",
    "rss_page_size_bytes": os.sysconf("SC_PAGE_SIZE"),
    "cpu_unit": "seconds cumulative per PID; ps display precision, not high resolution profiling",
    "cpu_display_quantum_s": 0.01 if platform.system() == "Darwin" else 1.0,
    "client_cpu": "RUSAGE_CHILDREN delta for sequential CLI only; waited CLI and its waited "
                  "children, excludes detached daemon and fixture; no ps sampler runs concurrently",
    "group_overlap": "fixture_subset overlaps persistent_tree; combined_tree unions persistent "
                     "and client PIDs and does not add the fixture subset again",
}
