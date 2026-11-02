#!/usr/bin/env python3
"""Local, concurrent MCP echo fixture. No network access except loopback serving."""

import argparse
import concurrent.futures
import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

MODERN = "2026-07-28"
VERSION_KEY = "io.modelcontextprotocol/protocolVersion"
SCHEMA = {
    "type": "object",
    "properties": {
        "token": {"type": "string"},
        "delayMs": {"type": "integer", "minimum": 0, "maximum": 5000},
        "payloadBytes": {"type": "integer", "minimum": 0, "maximum": 1048576},
    },
    "required": ["token"],
}
TOOLS = [
    {"name": "echo" if index == 0 else f"tool_{index}",
     "description": f"Local benchmark tool {index}", "inputSchema": SCHEMA}
    for index in range(25)
]


class Fixture:
    def __init__(self, events, protocol):
        self.events = Path(events)
        self.protocol = protocol
        self.lock = threading.Lock()
        self.record("spawn", parent=os.getppid(), protocol=protocol)

    def record(self, event, **details):
        with self.lock:
            with self.events.open("a") as output:
                output.write(json.dumps({
                    "event": event, "pid": os.getpid(),
                    "monotonic_ns": time.monotonic_ns(), **details,
                }, separators=(",", ":")) + "\n")

    def respond(self, message, headers=None):
        method = message.get("method")
        self.record("request", method=method)
        if "id" not in message:
            return None
        response = {"jsonrpc": "2.0", "id": message["id"]}
        try:
            parameters = message.get("params", {})
            if self.protocol == "modern":
                if method == "initialize":
                    raise LookupError("Modern fixture does not initialize")
                if parameters.get("_meta", {}).get(VERSION_KEY) != MODERN:
                    raise ValueError("Modern request omitted protocol metadata")
                if headers is not None:
                    if headers.get("mcp-protocol-version") != MODERN:
                        raise ValueError("Missing modern HTTP version header")
                    if headers.get("mcp-method") != method:
                        raise ValueError("Incorrect modern HTTP method header")
                    if method == "tools/call" and headers.get("mcp-name") != parameters.get("name"):
                        raise ValueError("Incorrect modern HTTP name header")
            if method == "server/discover":
                if self.protocol != "modern":
                    raise LookupError("Legacy fixture does not discover")
                result = {"supportedVersions": [MODERN], "capabilities": {"tools": {}},
                          "serverInfo": {"name": "benchmark-fixture", "version": "2"}}
            elif method == "initialize":
                result = {"protocolVersion": "2025-11-25", "capabilities": {"tools": {}},
                          "serverInfo": {"name": "benchmark-fixture", "version": "2"}}
            elif method == "tools/list":
                result = {"tools": TOOLS}
            elif method == "tools/call":
                if parameters.get("name") not in {tool["name"] for tool in TOOLS}:
                    raise ValueError("Unknown fixture tool")
                supplied = parameters["arguments"]
                token = supplied["token"]
                delay = supplied.get("delayMs", 0)
                size = supplied.get("payloadBytes", 64)
                if not isinstance(token, str) or not token:
                    raise ValueError("Expected nonempty token")
                if type(delay) is not int or not 0 <= delay <= 5000:
                    raise ValueError("Invalid delayMs")
                if type(size) is not int or not 0 <= size <= 1048576:
                    raise ValueError("Invalid payloadBytes")
                started = time.monotonic_ns()
                cpu = time.thread_time_ns()
                self.record("call_started", token=token, delay_ms=delay, payload_bytes=size)
                if delay:
                    time.sleep(delay / 1000)
                result = {"content": [{"type": "text", "text": json.dumps({
                    "token": token, "payload": "x" * size,
                })}], "isError": False}
                self.record("call_complete", token=token,
                            service_ns=time.monotonic_ns() - started,
                            thread_cpu_ns=time.thread_time_ns() - cpu)
            elif method == "ping":
                result = {}
            else:
                raise LookupError("Unsupported fixture method")
            if self.protocol == "modern":
                result["resultType"] = "complete"
                if method in {"server/discover", "tools/list"}:
                    result.update(ttlMs=60000, cacheScope="private")
            response["result"] = result
        except (KeyError, TypeError, ValueError, LookupError) as error:
            code = -32601 if isinstance(error, LookupError) and not isinstance(error, KeyError) else -32602
            response["error"] = {"code": code, "message": str(error)}
            self.record("error", method=method, error=str(error))
        return response


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--events", required=True)
    parser.add_argument("--protocol", choices=["legacy", "modern"], default="legacy")
    parser.add_argument("--http", action="store_true")
    arguments = parser.parse_args()
    fixture = Fixture(arguments.events, arguments.protocol)
    if arguments.http:
        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"
            disable_nagle_algorithm = True

            def log_message(self, *_):
                pass

            def do_POST(self):
                if self.path != "/mcp":
                    self.send_error(404)
                    return
                try:
                    length = int(self.headers.get("Content-Length", "0"))
                    if not 0 < length <= 2 * 1048576:
                        raise ValueError("Invalid Content-Length")
                    request = json.loads(self.rfile.read(length))
                    response = fixture.respond(request, self.headers)
                    body = b"" if response is None else json.dumps(response).encode()
                    self.send_response(202 if response is None else 200)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                except (ValueError, KeyError, TypeError) as error:
                    fixture.record("http_error", error=str(error))
                    self.send_error(400)

        with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
            server.daemon_threads = True
            print(server.server_address[1], flush=True)
            server.serve_forever(poll_interval=0.1)
    else:
        write_lock = threading.Lock()

        def dispatch(message):
            response = fixture.respond(message)
            if response is not None:
                with write_lock:
                    print(json.dumps(response, separators=(",", ":")), flush=True)

        # A serial fixture would falsely attribute fixture head-of-line blocking to the bridge.
        with concurrent.futures.ThreadPoolExecutor(max_workers=64) as executor:
            for line in sys.stdin:
                executor.submit(dispatch, json.loads(line))


if __name__ == "__main__":
    main()
