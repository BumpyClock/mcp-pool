"""Retained JSON-lines bridge client and bounded, closed-loop workloads."""

import concurrent.futures
import json
import math
import resource
import statistics
import threading
import time

from runtime import validate_output


def summary(values):
    ordered = sorted(values)
    if not ordered:
        raise RuntimeError("Workload completed no calls")
    return {"count": len(ordered), "min": ordered[0], "median": statistics.median(ordered),
            "p95": ordered[math.ceil(0.95 * len(ordered)) - 1], "max": ordered[-1]}


class Bridge:
    def __init__(self, runtime, mode=1, explicit_mode=False, direct=False):
        self.runtime, self.mode, self.explicit_mode = runtime, mode, explicit_mode
        self.direct = direct
        self.pending, self.lock, self.sequence = {}, threading.Lock(), 0
        self.process = None
        self.reader = None
        self.failure = None
        self.closing = False

    def __enter__(self):
        try:
            if self.direct:
                import sys
                from runtime import HERE
                command = [sys.executable, str(HERE / "fixture.py"), "--events", str(self.runtime.events),
                           "--protocol", self.runtime.protocol]
            else:
                command = self.runtime.base + ["serve", "--stdio", "--servers", "fixture"]
                if self.runtime.product == "mcp-cli" and (self.mode != 1 or self.explicit_mode):
                    command += ["--concurrency", str(self.mode)]
            self.process = self.runtime.spawn(command, "bridge")
            self.reader = threading.Thread(target=self.read, daemon=True)
            self.reader.start()
            if self.runtime.protocol == "modern":
                discovery = self.request("server/discover", {})
                if "2026-07-28" not in discovery.get("supportedVersions", []):
                    raise RuntimeError("Bridge did not advertise modern protocol")
            else:
                self.request("initialize", {"protocolVersion": "2025-11-25", "capabilities": {},
                                            "clientInfo": {"name": "benchmark", "version": "2"}})
                self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
            tools = self.request("tools/list", {})["tools"]
            names = [tool["name"] for tool in tools
                     if tool["name"] == "echo" or tool["name"].endswith("__echo")]
            if len(tools) != 25 or len(names) != 1:
                raise RuntimeError(f"Unexpected bridge discovery: {tools!r}")
            self.tool = names[0]
            return self
        except BaseException:
            self.close()
            raise

    def __exit__(self, *_):
        self.close()

    def send(self, message):
        with self.lock:
            if self.failure is not None:
                raise self.failure
            self.process.stdin.write(json.dumps(message, separators=(",", ":")) + "\n")
            self.process.stdin.flush()

    def read(self):
        try:
            for line in self.process.stdout:
                response = json.loads(line)
                with self.lock:
                    future = self.pending.pop(response.get("id"), None)
                if future is None:
                    if "id" in response:
                        raise RuntimeError(f"Unexpected response ID: {response!r}")
                    continue
                if "error" in response:
                    future.set_exception(RuntimeError(f"Bridge JSON-RPC error: {response['error']}"))
                else:
                    future.set_result(response["result"])
            if not self.closing:
                raise RuntimeError("Bridge stdout closed")
        except BaseException as error:
            with self.lock:
                self.failure = error
                for future in self.pending.values():
                    future.set_exception(error)
                self.pending.clear()

    def request(self, method, parameters):
        if self.runtime.protocol == "modern":
            parameters = {**parameters, "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {}}}
        future = concurrent.futures.Future()
        with self.lock:
            self.sequence += 1
            identifier = self.sequence
            self.pending[identifier] = future
        self.send({"jsonrpc": "2.0", "id": identifier, "method": method, "params": parameters})
        return future.result(timeout=15)

    def call(self, token, delay=0, size=64):
        started = time.perf_counter_ns()
        result = self.request("tools/call", {"name": self.tool, "arguments": {
            "token": token, "delayMs": delay, "payloadBytes": size}})
        validate_output(result, token, size)
        return {"token": token, "wall_ns": time.perf_counter_ns() - started}

    def close(self):
        if self.process is None:
            return
        self.closing = True
        if self.process.stdin is not None and not self.process.stdin.closed:
            self.process.stdin.close()
        try:
            self.process.wait(timeout=5)
        except Exception:
            self.runtime.terminate(self.process)
            raise
        finally:
            if self.reader is not None:
                self.reader.join(timeout=3)
        if self.process.returncode:
            raise RuntimeError(f"Bridge exited with status {self.process.returncode}")
        if self.failure is not None:
            raise self.failure


def sustained(bridge, clients, duration, delay=0, size=64):
    barrier = threading.Barrier(clients + 1)
    started = time.perf_counter_ns()
    deadline = 0
    before_cpu = resource.getrusage(resource.RUSAGE_SELF)

    def worker():
        rows = []
        barrier.wait(timeout=10)
        while time.perf_counter_ns() < deadline:
            rows.append(bridge.call(bridge.runtime.token(), delay, size))
        return rows

    with concurrent.futures.ThreadPoolExecutor(max_workers=clients) as executor:
        jobs = [executor.submit(worker) for _ in range(clients)]
        started = time.perf_counter_ns()
        deadline = started + int(duration * 1e9)
        barrier.wait(timeout=10)
        rows = [row for job in jobs for row in job.result()]
    elapsed = time.perf_counter_ns() - started
    after_cpu = resource.getrusage(resource.RUSAGE_SELF)
    return {"clients": clients, "requested_duration_s": duration, "elapsed_ns": elapsed,
            "drain_ns": max(0, elapsed - int(duration * 1e9)), "completed_calls": len(rows),
            "latency_ns": summary([row["wall_ns"] for row in rows]),
            "harness_cpu_s": after_cpu.ru_utime + after_cpu.ru_stime
                             - before_cpu.ru_utime - before_cpu.ru_stime,
            "raw_calls": rows}


def head_of_line(bridge):
    runtime = bridge.runtime
    slow = runtime.token()
    fast = runtime.token()
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
        started = time.perf_counter_ns()
        slow_job = executor.submit(bridge.call, slow, 400)
        deadline = time.monotonic() + 5
        while not any(row.get("token") == slow and row["event"] == "call_started"
                      for row in runtime.records()):
            if time.monotonic() > deadline:
                raise RuntimeError("Slow request did not reach fixture")
            time.sleep(0.005)
        fast_row = executor.submit(bridge.call, fast).result(timeout=10)
        fast_done = time.perf_counter_ns() - started
        slow_row = slow_job.result(timeout=10)
    return {"slow": slow_row, "fast": fast_row, "fast_done_from_slow_start_ns": fast_done,
            "slow_delay_ms": 400, "submission": "fast submitted after fixture recorded slow start"}
