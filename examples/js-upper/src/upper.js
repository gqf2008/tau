// Minimal tau extension in JavaScript: the `upper` tool, aligned with
// examples/upper (Rust). Build: ./build.sh (npm i + npx jco componentize).
// Exported WIT interfaces map to named ES exports; kebab-case WIT fields
// map to camelCase; the action enum maps to its string tag.
// 0.3.0 (docs/tool-media.md): tool-result content is a list of
// result-blocks; a WIT variant maps to { tag, val }.

function textBlock(text) {
    return { tag: "text", val: text };
}

export const tools = {
    definitions() {
        return [{
            name: "upper",
            description: "Convert text to UPPERCASE",
            parametersJson:
                '{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}',
        }];
    },

    execute(name, argumentsJson) {
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

    probe(_point, _payloadJson) {
        return { action: "continue" };
    },
};
