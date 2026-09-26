"""Mock MCP stdio server for tau bridge tests.

Speaks newline-delimited JSON-RPC on stdin/stdout: initialize,
notifications/initialized (ignored), tools/list, tools/call.
Tools: echo (returns its text), fail (returns an error result).
"""

import json
import sys

# Test hook: --protocol-version X makes the server choose X in the
# initialize reply instead of echoing the client's request.
FORCED_VERSION = None
if "--protocol-version" in sys.argv:
    i = sys.argv.index("--protocol-version")
    FORCED_VERSION = sys.argv[i + 1]

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


def reply(request_id, result=None, error=None):
    message = {"jsonrpc": "2.0", "id": request_id}
    if error is not None:
        message["error"] = {"code": -32603, "message": error}
    else:
        message["result"] = result
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def main():
    while True:
        line = sys.stdin.readline()
        if not line:
            break
        line = line.strip()
        if not line:
            continue
        request = json.loads(line)
        method = request.get("method", "")
        request_id = request.get("id")
        if method == "initialize":
            reply(
                request_id,
                {
                    "protocolVersion": FORCED_VERSION
                    or request["params"]["protocolVersion"],
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "tau-mock-mcp", "version": "0.1.0"},
                },
            )
        elif method == "notifications/initialized":
            pass  # notification: no response
        elif method == "tools/list":
            reply(request_id, {"tools": TOOLS})
        elif method == "tools/call":
            name = request["params"].get("name", "")
            arguments = request["params"].get("arguments", {})
            if name == "echo":
                reply(
                    request_id,
                    {
                        "content": [{"type": "text", "text": arguments.get("text", "")}],
                        "isError": False,
                    },
                )
            elif name == "fail":
                reply(
                    request_id,
                    {
                        "content": [{"type": "text", "text": "tool failed on purpose"}],
                        "isError": True,
                    },
                )
            else:
                reply(request_id, error=f"unknown tool: {name}")
        else:
            if request_id is not None:
                reply(request_id, error=f"unknown method: {method}")


if __name__ == "__main__":
    main()
