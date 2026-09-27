"""Minimal tau extension in Python: the `upper` tool, aligned with
examples/upper (Rust). Build: ./build.sh (componentize-py).

The runtime discovers exported-interface implementations by module
attribute name: `Tools` for tau:extension/tools, `Probes` for probes."""

import json

from wit_world import exports as _exports
from wit_world.imports.types import ResultBlock_Text


class Tools(_exports.Tools):
    def definitions(self):
        return [
            _exports.tools.Definition(
                name="upper",
                description="Convert text to UPPERCASE",
                parameters_json='{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}',
            )
        ]

    def execute(self, name, arguments_json):
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

    def probe(self, point, payload_json):
        return _exports.probes.Verdict(
            action=_exports.probes.Action.CONTINUE, payload_json=None, reason=None
        )
