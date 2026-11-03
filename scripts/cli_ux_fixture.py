#!/usr/bin/env python3
"""Deterministic local MCP server for the CLI UX audit. No external services."""

import argparse
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

SCHEMA = {
    "type": "object",
    "properties": {
        "message": {"type": "string", "description": "Text to echo back unchanged."},
        "count": {"type": "integer", "description": "Number of results.", "default": 1},
        "verbose": {"type": "boolean", "description": "Include extra details."},
        "mode": {"type": "string", "enum": ["fast", "full"], "description": "Search mode."},
        "tags": {"type": "array", "items": {"type": "string"}},
        "offset": {"type": "integer"},
        "metadata": {"type": "object"},
    },
    "required": ["message"],
}
TOOLS = [
    {"name": "echo", "description": "Echo supplied text and typed arguments. Safe, local, deterministic.",
     "inputSchema": SCHEMA},
    {"name": "fail", "description": "Return an intentional tool error.",
     "inputSchema": {"type": "object", "properties": {}}},
    {"name": "plain_text", "description": "Return plain text.",
     "inputSchema": {"type": "object", "properties": {}}},
]


def respond(request):
    if "id" not in request:
        return None
    method = request.get("method")
    parameters = request.get("params", {})
    if method == "initialize":
        result = {"protocolVersion": "2025-11-25", "capabilities": {"tools": {}, "resources": {}},
                  "serverInfo": {"name": "cli-ux-fixture", "version": "1.0"}}
    elif method == "tools/list":
        result = {"tools": TOOLS}
    elif method == "tools/call":
        name = parameters.get("name")
        if name == "fail":
            result = {"isError": True, "content": [{"type": "text", "text": "Intentional fixture failure"}]}
        elif name == "plain_text":
            result = {"content": [{"type": "text", "text": "Hello from local fixture."}]}
        elif name == "echo":
            result = {"content": [{"type": "text", "text": json.dumps(parameters.get("arguments", {}))}]}
        else:
            return {"jsonrpc": "2.0", "id": request["id"],
                    "error": {"code": -32602, "message": "Unknown fixture tool"}}
    elif method == "resources/list":
        result = {"resources": [{"uri": "fixture://guide", "name": "guide", "mimeType": "text/markdown"}]}
    elif method == "resources/read":
        result = {"contents": [{"uri": parameters.get("uri"), "mimeType": "text/markdown", "text": "# Fixture guide\nLocal content."}]}
    elif method == "ping":
        result = {}
    else:
        return {"jsonrpc": "2.0", "id": request["id"],
                "error": {"code": -32601, "message": "Unsupported fixture method"}}
    return {"jsonrpc": "2.0", "id": request["id"], "result": result}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--http", action="store_true")
    arguments = parser.parse_args()
    if not arguments.http:
        for line in sys.stdin:
            try:
                response = respond(json.loads(line))
            except (ValueError, TypeError) as error:
                print(f"Invalid fixture request: {error}", file=sys.stderr)
                continue
            if response is not None:
                print(json.dumps(response), flush=True)
        return

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, format, *args):
            pass

        def do_GET(self):
            self.send_error(404)

        def do_POST(self):
            try:
                request = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
            except (ValueError, TypeError) as error:
                self.send_error(400, str(error))
                return
            response = respond(request)
            body = b"" if response is None else json.dumps(response).encode()
            self.send_response(202 if response is None else 200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
        print(server.server_port, flush=True)
        server.serve_forever()


if __name__ == "__main__":
    main()
