"""Mock MCP streamable-HTTP server for tau bridge tests.

POST /mcp with a JSON-RPC message. Responds with application/json for
requests, 202 for notifications. initialize returns an Mcp-Session-Id
header. Pass the port as argv[1]; binds 127.0.0.1 only.
"""

import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

TOOLS = [
    {
        "name": "echo",
        "description": "Echo the given text back",
        "inputSchema": {
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
        },
    },
    {
        "name": "fail",
        "description": "Always returns an error result",
        "inputSchema": {"type": "object", "properties": {}},
    },
]

SESSION_ID = "mock-session-1"


def route(request):
    method = request.get("method", "")
    if method == "initialize":
        return {
            "protocolVersion": request["params"]["protocolVersion"],
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "tau-mock-mcp-http", "version": "0.1.0"},
        }
    if method == "tools/list":
        return {"tools": TOOLS}
    if method == "tools/call":
        name = request["params"].get("name", "")
        arguments = request["params"].get("arguments", {})
        if name == "echo":
            return {
                "content": [{"type": "text", "text": str(arguments.get("text", ""))}],
                "isError": False,
            }
        if name == "fail":
            return {
                "content": [{"type": "text", "text": "tool failed on purpose"}],
                "isError": True,
            }
        return None
    return None


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("content-length", 0))
        request = json.loads(self.rfile.read(length) or b"{}")
        if "id" not in request:
            self.send_response(202)
            self.end_headers()
            return
        result = route(request)
        if result is None:
            body = {
                "jsonrpc": "2.0",
                "id": request["id"],
                "error": {"code": -32603, "message": f"unknown: {request.get('method')}"},
            }
        else:
            body = {"jsonrpc": "2.0", "id": request["id"], "result": result}
        payload = json.dumps(body).encode()
        self.send_response(200)
        if request.get("method") == "initialize":
            self.send_header("mcp-session-id", SESSION_ID)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    port = int(sys.argv[1])
    HTTPServer(("127.0.0.1", port), Handler).serve_forever()
