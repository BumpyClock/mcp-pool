"""Brief functional checks invoked by run.py --self-test."""

import concurrent.futures
import contextlib
import json
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest import mock

from bridge import Bridge, sustained
from fixture import Fixture, MODERN, VERSION_KEY
from runtime import Runtime, process_table, validate_output


@contextlib.contextmanager
def delayed_workers(delay):
    submit = concurrent.futures.ThreadPoolExecutor.submit

    def delayed_submit(executor, function, *arguments, **keywords):
        def run():
            time.sleep(delay)
            return function(*arguments, **keywords)

        return submit(executor, run)

    with mock.patch.object(concurrent.futures.ThreadPoolExecutor, "submit", delayed_submit):
        yield


def fixed_operations(bridge, clients=2, operations=2):
    barrier = threading.Barrier(clients + 1)

    def worker():
        barrier.wait(timeout=10)
        return [bridge.call(bridge.runtime.token()) for _ in range(operations)]

    with concurrent.futures.ThreadPoolExecutor(max_workers=clients) as executor:
        jobs = [executor.submit(worker) for _ in range(clients)]
        barrier.wait(timeout=10)
        return [row for job in jobs for row in job.result(timeout=30)]


def gated_overlap(bridge):
    slow = bridge.runtime.token()
    fast = bridge.runtime.token()
    bridge.request("benchmark/hold", {"token": slow})
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
        try:
            slow_job = executor.submit(bridge.call, slow)
            deadline = time.monotonic() + 10
            while not any(row.get("token") == slow and row["event"] == "call_started"
                          for row in bridge.runtime.records()):
                if slow_job.done():
                    slow_job.result()
                    raise AssertionError("Held request completed before release")
                if time.monotonic() > deadline:
                    raise RuntimeError("Held request did not reach fixture")
                time.sleep(0.005)
            fast_row = executor.submit(bridge.call, fast).result(timeout=10)
            assert not slow_job.done(), "Held request completed before release"
        finally:
            bridge.request("benchmark/release", {"token": slow})
        slow_row = slow_job.result(timeout=10)
    return {"slow": slow_row, "fast": fast_row}


def self_test(options):
    checks = []
    unit_events = options.output / "self-test-fixture.events.jsonl"
    fixture = Fixture(unit_events, "legacy")
    unknown = fixture.respond({"jsonrpc": "2.0", "id": 1, "method": "not/a/method"})
    assert unknown["error"]["code"] == -32601
    arguments = {"token": "unit-unique", "payloadBytes": 3}
    result = fixture.respond({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                              "params": {"name": "echo", "arguments": arguments}})
    validate_output(result["result"], "unit-unique", 3)
    for bad in ({"error": {}}, {"isError": True}, {"token": "wrong", "payload": "xxx"},
                {"content": [{"type": "text", "text": "{"}]}):
        try:
            validate_output(bad, "unit-unique", 3)
        except (RuntimeError, json.JSONDecodeError):
            pass
        else:
            raise AssertionError("Invalid tool output was accepted")
    modern = Fixture(options.output / "self-test-modern.events.jsonl", "modern")
    rejected = modern.respond({"jsonrpc": "2.0", "id": 1, "method": "server/discover", "params": {}})
    assert "error" in rejected
    discovery = modern.respond({"jsonrpc": "2.0", "id": 2, "method": "server/discover",
                                "params": {"_meta": {VERSION_KEY: MODERN}}})
    assert discovery["result"]["supportedVersions"] == [MODERN]
    checks.append({"check": "fixture-errors-unique-output-modern-metadata", "status": "passed"})

    for product in options.products:
        binary = getattr(options, product.replace("-", "_"))
        for transport in options.transports:
            for protocol in options.protocols:
                label = f"self-test-{product}-{transport}-{protocol}"
                with Runtime(product, binary, transport, protocol, options.output, label) as runtime:
                    root = runtime.root
                    runtime.start_daemon()
                    runtime.call(runtime.token())
                    runtime.discover()
                    with Bridge(runtime) as bridge:
                        with delayed_workers(0.1):
                            rows = fixed_operations(bridge)
                        assert len(rows) == 4
                        assert len({row["token"] for row in rows}) == 4
                    counts = runtime.verify()
                    owned = {row["pid"] for row in runtime.records() if row["event"] == "spawn"}
                    assert runtime.daemon is not None
                    owned.add(runtime.daemon.pid)
                assert not root.exists(), "Namespace was not removed"
                assert not owned.intersection(process_table()), "Owned subprocess survived cleanup"
                checks.append({"check": label, "status": "passed", "counters": counts})

    product = options.products[0]
    binary = getattr(options, product.replace("-", "_"))
    with Runtime(product, binary, "stdio", "legacy", options.output, "self-test-direct") as runtime:
        with Bridge(runtime, direct=True) as bridge:
            with delayed_workers(0.5):
                overlap = gated_overlap(bridge)
            completions = {row["token"]: row["monotonic_ns"] for row in runtime.records()
                           if row["event"] == "call_complete"}
            assert completions[overlap["fast"]["token"]] < completions[overlap["slow"]["token"]]
        runtime.verify()
    checks.append({"check": "fixture-concurrent-stdio-no-head-of-line", "status": "passed"})

    runtime = Runtime(product, binary, "http", "legacy", options.output, "self-test-setup-failure")
    original_write = Path.write_text

    def fail_config(path, *args, **kwargs):
        if path == runtime.config:
            raise OSError("injected setup failure")
        return original_write(path, *args, **kwargs)

    try:
        with mock.patch.object(Path, "write_text", fail_config):
            with runtime:
                raise AssertionError("Setup failure was not raised")
    except OSError:
        pass
    assert not runtime.root.exists(), "Setup failure leaked namespace"
    checks.append({"check": "setup-failure-cleanup", "status": "passed"})

    failed_runtime = Runtime(product, binary, "http", "legacy", options.output, "self-test-failure")
    failed_processes: set[int] = set()
    try:
        with failed_runtime as runtime:
            runtime.start_daemon()
            runtime.call(runtime.token())
            assert runtime.http is not None and runtime.daemon is not None
            failed_processes.update({runtime.http.pid, runtime.daemon.pid})
            try:
                runtime.command(["call", "fixture.echo", "--args", "{", "--no-oauth"])
            except RuntimeError:
                pass
            else:
                raise AssertionError("Invalid CLI command was counted as success")
            runtime.verify()
            raise InterruptedError("injected workload failure")
    except InterruptedError:
        pass
    assert len(failed_processes) == 2
    assert not failed_runtime.root.exists() and not failed_processes.intersection(process_table())
    checks.append({"check": "command-error-cleanup", "status": "passed"})

    with Runtime(product, binary, "http", "legacy", options.output, "self-test-cleanup-ownership") as runtime:
        root = runtime.root
        fixture_process = runtime.http
        assert fixture_process is not None
        # Cleanup must use owned handles, not sampling or historical event PIDs.
        with mock.patch("runtime.process_table", side_effect=OSError("sampler unavailable")), \
                mock.patch.object(runtime, "records", side_effect=OSError("ledger unavailable")), \
                mock.patch.object(runtime, "command") as shutdown:
            runtime.close()
            shutdown.assert_called_once_with(["daemon", "stop"], timeout=15)
        assert fixture_process.poll() is not None
        assert fixture_process.stdout is not None and fixture_process.stdout.closed
        assert all(log.closed for log in runtime.logs)
        assert not root.exists()
    checks.append({"check": "cleanup-independent-of-sampler-and-ledger", "status": "passed"})

    runtime = Runtime(product, binary, "stdio", "legacy", options.output, "self-test-stale-pid")
    runtime.root.mkdir(mode=0o700)
    runtime.created = True
    runtime.events.write_text('{"event":"spawn","pid":999999}\n')
    with mock.patch("os.kill") as signal_process:
        runtime.close()
        signal_process.assert_not_called()
    checks.append({"check": "historical-pids-never-signaled", "status": "passed"})

    runtime = Runtime(product, binary, "http", "legacy", options.output, "self-test-cleanup-retry")
    runtime.__enter__()
    fixture_process = runtime.http
    assert fixture_process is not None
    try:
        with mock.patch.object(runtime, "command", side_effect=OSError("shutdown unavailable")):
            try:
                runtime.close()
            except RuntimeError as error:
                assert "namespace retained" in str(error)
            else:
                raise AssertionError("Shutdown failure was hidden")
        assert fixture_process.poll() is not None
        assert all(log.closed for log in runtime.logs)
        assert runtime.root.exists()
        assert not runtime.closed
    finally:
        runtime.close()
    assert not runtime.root.exists()
    checks.append({"check": "failed-shutdown-tears-down-owned-handles-and-allows-retry", "status": "passed"})
    return checks


class SynchronizationTests(unittest.TestCase):
    def test_fixed_operations_survive_delayed_workers(self):
        bridge = mock.Mock()
        bridge.runtime.token.side_effect = ["first", "second", "third", "fourth"]
        bridge.call.side_effect = lambda token: {"token": token, "wall_ns": 1}
        with delayed_workers(0.1):
            rows = fixed_operations(bridge)
        self.assertEqual({row["token"] for row in rows}, {"first", "second", "third", "fourth"})
        self.assertEqual(len(rows), 4)

    def test_fixed_operations_propagate_malformed_output(self):
        bridge = mock.Mock()
        bridge.call.side_effect = json.JSONDecodeError("Malformed tool output", "{", 1)
        with self.assertRaises(json.JSONDecodeError):
            fixed_operations(bridge)

    def test_malformed_successful_readiness_is_not_retried(self):
        with tempfile.TemporaryDirectory() as directory:
            runtime = Runtime("mcp-cli", "unused", "stdio", "legacy", directory, "readiness")
            process = mock.Mock()
            process.poll.return_value = None
            with mock.patch.object(runtime, "spawn", return_value=process), \
                    mock.patch.object(runtime, "command", side_effect=[
                        ("{", {}), ('{"running":true}', {}),
                    ]) as command, \
                    mock.patch("runtime.time.sleep") as sleep, \
                    self.assertRaisesRegex(RuntimeError, "Invalid JSON from daemon status output") as failure:
                runtime.start_daemon()
            self.assertIsInstance(failure.exception.__cause__, json.JSONDecodeError)
            self.assertEqual(command.call_count, 1)
            sleep.assert_not_called()

    def test_sustained_measures_from_release_and_stops_at_deadline(self):
        clock = 0
        barrier_type = threading.Barrier
        bridge = mock.Mock()
        bridge.runtime.token.side_effect = ["first", "second", "third"]

        def delayed_barrier(parties, action=None):
            def release():
                nonlocal clock
                clock += 5_000_000_000
                if action is not None:
                    action()

            return barrier_type(parties, action=release)

        def call(token, delay, size):
            nonlocal clock
            clock += 1_000_000_000
            return {"token": token, "wall_ns": 1_000_000_000}

        bridge.call.side_effect = call
        with mock.patch("bridge.threading.Barrier", delayed_barrier), \
                mock.patch("bridge.time.perf_counter_ns", side_effect=lambda: clock):
            result = sustained(bridge, 1, 3)
        self.assertEqual(result["completed_calls"], 3)
        self.assertEqual(result["elapsed_ns"], 3_000_000_000)
        self.assertEqual(result["requested_duration_s"], 3)
        self.assertEqual(result["drain_ns"], 0)
        self.assertEqual(set(result), {
            "clients", "requested_duration_s", "elapsed_ns", "drain_ns", "completed_calls",
            "latency_ns", "harness_cpu_s", "raw_calls",
        })

    def test_fixture_gate_timeout_is_an_error_and_keeps_call_schema(self):
        with tempfile.TemporaryDirectory() as directory:
            for protocol in ("legacy", "modern"):
                with self.subTest(protocol=protocol):
                    fixture = Fixture(Path(directory) / f"{protocol}.jsonl", protocol)

                    def request(method, **parameters):
                        if protocol == "modern":
                            parameters["_meta"] = {VERSION_KEY: MODERN}
                        return fixture.respond({"jsonrpc": "2.0", "id": 1,
                                                "method": method, "params": parameters})

                    tools = request("tools/list")["result"]["tools"]
                    self.assertEqual(len(tools), 25)
                    self.assertEqual(set(tools[0]["inputSchema"]["properties"]),
                                     {"token", "delayMs", "payloadBytes"})
                    result = request("tools/call", name="echo", arguments={"token": "ordinary"})
                    validate_output(result["result"], "ordinary")
                    self.assertIn("result", request("benchmark/hold", token="held"))
                    with mock.patch.object(fixture.gates["held"], "wait", return_value=False) as wait:
                        result = request("tools/call", name="echo", arguments={"token": "held"})
                    wait.assert_called_once_with(timeout=10)
                    self.assertIn("timed out", result["error"]["message"])
                    self.assertNotIn("result", result)
                    self.assertIn("result", request("benchmark/release", token="held"))


if __name__ == "__main__":
    unittest.main()
