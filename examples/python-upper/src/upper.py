"""Minimal tau extension in Python: the `upper` tool, aligned with
examples/upper (Rust). Build: ./build.sh (componentize-py).

The runtime discovers exported-interface implementations by module
attribute name: `Tools` for tau:extension/tools, `Probes` for probes.

0.7.0: `definitions` / `execute` are `async def` (the generated protocol
says so — a synchronously lowered export cannot wait for anything, and a
bridge's tool list lives on a remote server). This tool awaits nothing,
so the `async` keyword is the whole change. The `probes` pair stays
synchronous but is typed now: `points()` returns `Point` values and
`probe()` answers with a `Verdict` variant — no JSON strings."""

import json

from wit_world import exports as _exports
from wit_world.imports.types import ResultBlock_Text


class Tools(_exports.Tools):
    async def definitions(self):
        return [
            _exports.tools.Definition(
                name="upper",
                description="Convert text to UPPERCASE",
                parameters_json='{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}',
            )
        ]

    async def execute(self, name, arguments_json):
        if name != "upper":
            return _exports.tools.ToolResult(
                content=[ResultBlock_Text("unknown tool: " + name)], is_error=True
            )
        try:
            text = json.loads(arguments_json).get("text")
        except ValueError:
            text = None
        if not isinstance(text, str):
            return _exports.tools.ToolResult(
                content=[ResultBlock_Text("missing string argument 'text'")], is_error=True
            )
        # 0.3.0 (docs/tool-media.md): content is a list of result-blocks.
        return _exports.tools.ToolResult(
            content=[ResultBlock_Text(text.upper())], is_error=False
        )


class Probes(_exports.Probes):
    def points(self):
        return []

    def probe(self, point, payload):
        return _exports.probes.Verdict_Continue()
