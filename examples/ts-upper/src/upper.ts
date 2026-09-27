// Minimal tau extension in TypeScript: the `upper` tool, aligned with
// examples/upper (Rust). Build: ./build.sh (npm i + npx jco componentize;
// TS is bundled automatically by jco). Same WIT-to-JS mapping rules as the
// JavaScript example: interfaces → named exports, kebab-case → camelCase.

interface Definition {
    name: string;
    description: string;
    parametersJson: string;
}

interface ToolResult {
    content: string;
    isError: boolean;
}

type Action = "continue" | "replace" | "block";

interface Verdict {
    action: Action;
    payloadJson?: string;
    reason?: string;
}

const UPPER: Definition = {
    name: "upper",
    description: "Convert text to UPPERCASE",
    parametersJson:
        '{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}',
};

export const tools = {
    definitions(): Definition[] {
        return [UPPER];
    },

    execute(name: string, argumentsJson: string): ToolResult {
        if (name !== "upper") {
            return { content: `unknown tool: ${name}`, isError: true };
        }
        let text: unknown;
        try {
            text = (JSON.parse(argumentsJson) as { text?: unknown }).text;
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
    points(): string[] {
        return [];
    },

    probe(_point: string, _payloadJson: string): Verdict {
        return { action: "continue" };
    },
};
