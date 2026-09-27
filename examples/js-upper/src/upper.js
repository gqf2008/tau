// Minimal tau extension in JavaScript: the `upper` tool, aligned with
// examples/upper (Rust). Build: ./build.sh (npm i + npx jco componentize).
// Exported WIT interfaces map to named ES exports; kebab-case WIT fields
// map to camelCase; the action enum maps to its string tag.

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
            return { content: `unknown tool: ${name}`, isError: true };
        }
        let text;
        try {
            text = JSON.parse(argumentsJson).text;
        } catch {
            text = undefined;
        }
        if (typeof text !== "string") {
            return { content: "missing string argument 'text'", isError: true };
        }
        return { content: text.toUpperCase(), isError: false };
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
