// Minimal tau extension in TypeScript: the `upper` tool, aligned with
// examples/upper (Rust). Build: ./build.sh (npm i + npx jco componentize;
// TS is bundled automatically by jco). Same WIT-to-JS mapping rules as the
// JavaScript example: interfaces → named exports, kebab-case → camelCase.
// 0.7.0: `definitions` / `execute` are `async` (a synchronously lowered
// export cannot wait for anything); this tool awaits nothing, so the
// keyword is the whole change. `probe` stays synchronous but takes the
// typed payload and answers with the `verdict` variant ({ tag: ... }) —
// no JSON strings.

interface Definition {
    name: string;
    description: string;
    parametersJson: string;
}

// 0.3.0 (docs/tool-media.md): tool-result content is a list of
// result-blocks; a WIT variant maps to { tag, val }.
type ResultBlock = { tag: "text"; val: string } | { tag: "media"; val: unknown };

interface ToolResult {
    content: ResultBlock[];
    isError: boolean;
}

function textBlock(text: string): ResultBlock {
    return { tag: "text", val: text };
}

// 0.7.0: the probe answer is a variant, not an action string + JSON blob.
// `points`/`probe` take and return the typed `point`/`payload`/`verdict`.
type Point =
    | "before-run"
    | "transform-context"
    | "before-request"
    | "after-response"
    | "before-tool"
    | "after-tool"
    | "before-run-end"
    | "before-compaction"
    | "before-navigation"
    | "session-start"
    | "branch"
    | "session-end";

type Payload = unknown;

type Verdict =
    | { tag: "continue" }
    | { tag: "replace"; val: Payload }
    | { tag: "block"; val: string };

const UPPER: Definition = {
    name: "upper",
    description: "Convert text to UPPERCASE",
    parametersJson:
        '{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}',
};

export const tools = {
    async definitions(): Promise<Definition[]> {
        return [UPPER];
    },

    async execute(name: string, argumentsJson: string): Promise<ToolResult> {
        if (name !== "upper") {
            return { content: [textBlock(`unknown tool: ${name}`)], isError: true };
        }
        let text: unknown;
        try {
            text = (JSON.parse(argumentsJson) as { text?: unknown }).text;
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
    points(): Point[] {
        return [];
    },

    probe(_point: Point, _payload: Payload): Verdict {
        return { tag: "continue" };
    },
};
