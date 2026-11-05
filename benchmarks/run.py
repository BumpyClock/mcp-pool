#!/usr/bin/env python3
"""Isolated local MCP benchmarks. Results are raw observations, not capacity claims."""

import argparse
import concurrent.futures
import datetime
import hashlib
import json
import os
import platform
import shutil
import sys
import time
import traceback
from pathlib import Path

from bridge import Bridge, head_of_line, summary, sustained
from runtime import HERE, SAMPLE_METADATA, Runtime


def selection(value, choices):
    values = value.split(",")
    if not values or len(set(values)) != len(values) or any(item not in choices for item in values):
        raise argparse.ArgumentTypeError("Choose distinct comma-separated values from " + ",".join(choices))
    return values


def arguments():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.ArgumentDefaultsHelpFormatter)
    parser.add_argument("--mcp-cli", dest="mcp_cli", default=shutil.which("mcp-cli"))
    parser.add_argument("--mcporter", default=shutil.which("mcporter"))
    parser.add_argument("--products", type=lambda value: selection(value, ["mcp-cli", "mcporter"]),
                        default=["mcp-cli", "mcporter"])
    parser.add_argument("--cases", type=lambda value: selection(value, ["cold", "warm", "bridge", "hol", "memory", "fixture"]),
                        default=["cold", "warm", "bridge", "hol", "memory"])
    parser.add_argument("--transports", type=lambda value: selection(value, ["stdio", "http"]),
                        default=["stdio", "http"])
    parser.add_argument("--protocols", type=lambda value: selection(value, ["legacy", "modern"]),
                        default=["legacy", "modern"])
    parser.add_argument("--bridge-modes", type=lambda value: [int(item) for item in selection(value, ["1", "8"])],
                        default=[1], help="mcp-cli serve concurrency; mcporter is always unmodified")
    parser.add_argument("--explicit-bridge-concurrency", action="store_true",
                        help="Also pass --concurrency 1. Omitted by default for installed-release checks.")
    parser.add_argument("--clients", type=lambda value: [int(item) for item in selection(value, ["1", "8"])],
                        default=[1, 8])
    parser.add_argument("--duration", type=float, default=5, help="Seconds per retained workload, excluding final drain")
    parser.add_argument("--repeats", type=int, default=3, help="At least three independent bridge/memory repeats")
    parser.add_argument("--cold-rounds", type=int, default=5)
    parser.add_argument("--warm-rounds", type=int, default=12)
    parser.add_argument("--payload-bytes", type=int, default=64)
    parser.add_argument("--delay-ms", type=int, default=0, help="Fixture service delay in retained load only")
    parser.add_argument("--sample-interval", type=float, default=0.05)
    parser.add_argument("--idle-seconds", type=float, default=0.5, help="Settle time after all clients disconnect")
    parser.add_argument("--profile-wait", type=float, default=0, help="Pause before bridge loads after writing PID artifact")
    parser.add_argument("--output", type=Path, default=HERE / "results" / datetime.datetime.now().strftime("%Y%m%d-%H%M%S-%f"),
                        help="New results directory. Existing results.json is never overwritten.")
    parser.add_argument("--self-test", action="store_true", help="Brief functional tests, not performance comparisons")
    options = parser.parse_args()
    if options.duration <= 0 or options.repeats < 3 or options.cold_rounds < 1 or options.warm_rounds < 1:
        parser.error("duration and round counts must be positive; repeats must be >= 3")
    if not 0 <= options.delay_ms <= 5000 or not 0 <= options.payload_bytes <= 1048576:
        parser.error("delay-ms must be 0..5000; payload-bytes must be 0..1048576")
    if options.sample_interval <= 0 or options.idle_seconds < 0 or options.profile_wait < 0:
        parser.error("sample-interval must be positive; idle-seconds/profile-wait must be nonnegative")
    for product in options.products:
        attribute = product.replace("-", "_")
        binary = shutil.which(getattr(options, attribute) or "")
        if binary is None:
            parser.error(f"{product} executable not found")
        setattr(options, attribute, str(Path(binary).absolute()))
    options.output = options.output.absolute()
    if (options.output / "results.json").exists():
        parser.error("output already contains results.json; choose a new directory")
    return options


def write_results(path, results):
    staging = path.with_suffix(".pending")
    staging.write_text(json.dumps(results, indent=2, allow_nan=False) + "\n")
    staging.replace(path)


def metadata(options):
    binaries = {}
    for product in options.products:
        binary = getattr(options, product.replace("-", "_"))
        with Runtime(product, binary, "stdio", "legacy", options.output, f"metadata-{product}") as runtime:
            version, _ = runtime.command(["--version"])
        binaries[product] = {"path": binary, "version": version.strip(),
                             "launcher_sha256": hashlib.sha256(Path(binary).read_bytes()).hexdigest()}
    return {
        "started_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "platform": platform.platform(), "python": sys.version, "binaries": binaries,
        "argv": sys.argv, "harness_pid": os.getpid(),
        "runtime_environment": {"TOKIO_WORKER_THREADS": os.environ.get("TOKIO_WORKER_THREADS")},
        "options": {name: str(value) if isinstance(value, Path) else value
                    for name, value in vars(options).items()},
        "source_sha256": {name: hashlib.sha256((HERE / name).read_bytes()).hexdigest()
                          for name in ("run.py", "runtime.py", "bridge.py", "fixture.py", "test_benchmark.py")},
        "clock_resolution_s": time.get_clock_info("perf_counter").resolution,
        "sampling": SAMPLE_METADATA,
        "fixture": {"tools": 25, "max_workers_stdio": 64, "http": "ThreadingHTTPServer on 127.0.0.1",
                    "event_logging": "Every request and call start/completion is written to JSONL",
                    "payload_bytes": options.payload_bytes, "load_delay_ms": options.delay_ms,
                    "modern_discovery_ttl_ms": 60000},
        "limits": [
            "Closed-loop, Python-threaded load generator and instrumented Python fixture can cap completed calls.",
            "No saturation capacity or product-throughput claims follow from these raw counts.",
            "Run --cases fixture --transports stdio to measure the same generator directly against the fixture.",
            "HTTP fixture, stdio fixture, and load generator costs are included or separately labeled, not subtracted.",
            "ps CPU precision can round short runs to zero; PID-resolved raw snapshots are retained.",
            "RSS sums double-count shared pages. Sampling can miss startup peaks.",
            "Foreground daemon startup is excluded from warm/bridge/memory, included in cold automatic launch.",
            "The HTTP fixture is started before cold timing; stdio fixture startup is inside cold timing.",
            "Local executable/OS page caches are not cleared. Cold means fresh namespace and daemon, not cold disk cache.",
            "Results include all token-verified calls. Operation errors fail the run rather than becoming successes.",
            "The mcporter launcher hash is not a hash of its complete JavaScript dependency graph.",
        ],
    }


def memory_probe(runtime, clients, options):
    before = runtime.snapshot()
    samples = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=clients) as executor:
        jobs = [executor.submit(runtime.call, runtime.token(), 500, options.payload_bytes)
                for _ in range(clients)]
        deadline = time.monotonic() + 20
        while not all(job.done() for job in jobs):
            samples.append(runtime.snapshot([process for process in runtime.processes
                                             if process not in (runtime.daemon, runtime.http)]))
            if time.monotonic() > deadline:
                raise RuntimeError("Memory workload exceeded deadline")
            time.sleep(options.sample_interval)
        rows = [job.result() for job in jobs]
    time.sleep(options.idle_seconds)
    idle = []
    for _ in range(3):
        idle.append(runtime.snapshot())
        time.sleep(options.sample_interval)
    if not samples:
        raise RuntimeError("No active memory samples captured")
    return {"clients": clients, "raw_calls": rows, "before_clients": before,
            "active_samples": samples, "after_disconnect_idle_samples": idle,
            "idle_settle_s": options.idle_seconds, "requested_sample_interval_s": options.sample_interval,
            "observed_sample_interval_ns": [
                later["monotonic_ns"] - earlier["monotonic_ns"]
                for earlier, later in zip(samples, samples[1:])],
            "peak_observed_combined_rss_kib": max(row["combined_tree"]["rss_kib"] for row in samples)}


def warm(runtime, options):
    runtime.start_daemon()
    warmup = runtime.call(runtime.token(), size=options.payload_bytes)
    before = runtime.snapshot()
    calls = [runtime.call(runtime.token(), size=options.payload_bytes, cpu=True)
             for _ in range(options.warm_rounds)]
    discovery = [runtime.discover() for _ in range(options.warm_rounds)]
    return {"warmup": warmup, "calls": calls, "discovery": discovery,
            "call_wall_ns": summary([row["wall_ns"] for row in calls]),
            "before": before, "after": runtime.snapshot()}


def bridges(runtime, options, mode, kind):
    direct = kind == "fixture"
    if not direct:
        runtime.start_daemon()
    rows = []
    with Bridge(runtime, mode, options.explicit_bridge_concurrency, direct=direct) as bridge:
        bridge.call(runtime.token(), size=options.payload_bytes)
        snapshot = runtime.snapshot([bridge.process])
        (options.output / f"{runtime.label}.profile.json").write_text(json.dumps({
            "label": runtime.label, "namespace": str(runtime.root),
            "bridge_pid": bridge.process.pid,
            "daemon_pid": None if runtime.daemon is None else runtime.daemon.pid,
            "snapshot": snapshot,
        }, indent=2))
        if options.profile_wait:
            time.sleep(options.profile_wait)
        for repeat in range(options.repeats):
            if kind == "hol":
                rows.append({"repeat": repeat, **head_of_line(bridge)})
            else:
                clients_order = options.clients if repeat % 2 == 0 else list(reversed(options.clients))
                for clients in clients_order:
                    before = runtime.snapshot([bridge.process])
                    row = sustained(bridge, clients, options.duration, options.delay_ms, options.payload_bytes)
                    rows.append({"repeat": repeat, **row, "before": before,
                                 "after": runtime.snapshot([bridge.process])})
    time.sleep(options.idle_seconds)
    idle = []
    for _ in range(3):
        idle.append(runtime.snapshot())
        time.sleep(options.sample_interval)
    return {"serve_concurrency": mode if runtime.product == "mcp-cli" and not direct else None,
            "mode_1_flag": "explicit" if options.explicit_bridge_concurrency else "default omitted",
            "raw_runs": rows, "after_bridge_disconnect_idle_samples": idle}


def run(options, results, result_path):
    for transport in options.transports:
        for protocol in options.protocols:
            for kind in options.cases:
                if kind == "fixture" and transport != "stdio":
                    continue
                products = options.products if kind != "fixture" else options.products[:1]
                for product in products:
                    binary = getattr(options, product.replace("-", "_"))
                    modes = options.bridge_modes if kind in ("bridge", "hol") and product == "mcp-cli" else [1]
                    repetitions = range(options.cold_rounds) if kind == "cold" else [None]
                    for mode in modes:
                        for repeat in repetitions:
                            label = f"{product}-{transport}-{protocol}-{kind}-mode{mode}"
                            if repeat is not None:
                                label += f"-{repeat}"
                            row = {"label": label, "product": "fixture" if kind == "fixture" else product,
                                   "transport": transport, "protocol": protocol, "case": kind,
                                   "status": "running"}
                            results["runs"].append(row)
                            write_results(result_path, results)
                            with Runtime(product, binary, transport, protocol, options.output, label) as runtime:
                                if kind == "cold":
                                    row["calls"] = [runtime.call(runtime.token(), size=options.payload_bytes, cpu=True)]
                                elif kind == "warm":
                                    row.update(warm(runtime, options))
                                elif kind == "memory":
                                    runtime.start_daemon()
                                    runtime.call(runtime.token(), size=options.payload_bytes)
                                    row["probes"] = [
                                        {"repeat": index, **memory_probe(runtime, clients, options)}
                                        for index in range(options.repeats) for clients in options.clients]
                                else:
                                    row.update(bridges(runtime, options, mode, kind))
                                row["upstream"] = runtime.verify()
                            row["status"] = "passed"
                            write_results(result_path, results)
                            print(f"{label}: passed", file=sys.stderr, flush=True)


def main():
    options = arguments()
    options.output.mkdir(parents=True, exist_ok=True)
    result_path = options.output / "results.json"
    results = {"status": "running", "runs": []}
    try:
        results["metadata"] = metadata(options)
        write_results(result_path, results)
        if options.self_test:
            from test_benchmark import self_test
            results["self_test"] = self_test(options)
        else:
            run(options, results, result_path)
        results["status"] = "passed"
    except BaseException as error:
        results["status"] = "failed"
        results["error"] = f"{type(error).__name__}: {error}"
        if results["runs"] and results["runs"][-1]["status"] == "running":
            results["runs"][-1]["status"] = "failed"
        (options.output / "failure.stderr").write_text(traceback.format_exc())
        print(results["error"], file=sys.stderr)
    finally:
        results["finished_utc"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        write_results(result_path, results)
        print(result_path, flush=True)
    return 0 if results["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
