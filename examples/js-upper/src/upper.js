// Minimal tau extension in JavaScript: the `upper` tool, aligned with
// examples/upper (Rust). Build: ./build.sh (npm i + npx jco componentize).
// Exported WIT interfaces map to named ES exports; kebab-case WIT fields
// map to camelCase; a WIT variant maps to { tag, val }.
// 0.3.0 (docs/tool-media.md): tool-result content is a list of
// result-blocks.
// 0.7.0: `definitions` / `execute` are `async` (a synchronously lowered
// export cannot wait for anything); this tool awaits nothing, so the
// keyword is the whole change. `probe` stays synchronous but takes the
// typed payload and answers with the `verdict` variant ({ tag: ... }).

function textBlock(text) {
    return { tag: "text", val: text };
}

export const tools = {
    async definitions() {
        return [{
            name: "upper",
            description: "Convert text to UPPERCASE",
            parametersJson:
                '{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}',
        }];
    },

    async execute(name, argumentsJson) {
        if (name !== "upper") {
            return { content: [textBlock(`unknown tool: ${name}`)], isError: true };
        }
        let text;
        try {
            text = JSON.parse(argumentsJson).text;
        } catch {
            text = undefined;
        }
        if (typeof text !== "string") {
            return { content: [textBlock("missing string argument 'text'")], isError: true };
        }
        return { content: [textBlock(text.toUpperCase())], isError: false };
    },
};

export const probes = {
    points() {
        return [];
    },

    probe(_point, _payload) {
        return { tag: "continue" };
    },
};
