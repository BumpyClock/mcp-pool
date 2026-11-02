"""Brief functional checks invoked by run.py --self-test."""

from pathlib import Path
from unittest import mock

from bridge import Bridge, head_of_line, sustained
from fixture import Fixture, MODERN, VERSION_KEY
from runtime import Runtime, process_table, validate_output


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
    for bad in ({"error": {}}, {"isError": True}, {"token": "wrong", "payload": "xxx"}):
        try:
            validate_output(bad, "unit-unique", 3)
        except RuntimeError:
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
                        head_of_line(bridge)
                        load = sustained(bridge, 2, 0.04)
                        assert load["completed_calls"] >= 2
                    counts = runtime.verify()
                    owned = {row["pid"] for row in runtime.records() if row["event"] == "spawn"}
                    owned.add(runtime.daemon.pid)
                assert not root.exists(), "Namespace was not removed"
                assert not owned.intersection(process_table()), "Owned subprocess survived cleanup"
                checks.append({"check": label, "status": "passed", "counters": counts})

    product = options.products[0]
    binary = getattr(options, product.replace("-", "_"))
    with Runtime(product, binary, "stdio", "legacy", options.output, "self-test-direct") as runtime:
        with Bridge(runtime, direct=True) as bridge:
            overlap = head_of_line(bridge)
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

    try:
        with Runtime(product, binary, "http", "legacy", options.output, "self-test-failure") as runtime:
            root = runtime.root
            runtime.start_daemon()
            runtime.call(runtime.token())
            fixture_pid = runtime.http.pid
            daemon_pid = runtime.daemon.pid
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
    assert not root.exists() and not {fixture_pid, daemon_pid}.intersection(process_table())
    checks.append({"check": "command-error-cleanup", "status": "passed"})
    return checks
