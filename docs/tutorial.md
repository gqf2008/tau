# From scaffold to a published extension

The hands-on version of `docs/extensions.md`: an empty directory → a loaded
tool → a signed component → a registry digest you can pin. Every command
below was run against tau 0.6.0 (contract `tau:extension@0.6.0`); outputs
are trimmed to the lines that carry meaning, and paths are elided where they
are machine-specific. Fingerprints and digests in the transcripts come from
the machine this was written on and yours will differ — a digest covers the
exact bytes, and those carry your build paths. That is the point of them.

Read alongside: `docs/extensions.md` (the API surface, the three worlds),
`wit/tau.wit` (the contract itself), `docs/signing.md`, `docs/oci.md`.

## 0. Prerequisites

- Rust with the wasm target: `rustup target add wasm32-wasip2`
- `tau` on PATH: `cargo install tau-cli --locked` (or a checkout, built)
- Nothing else — no API key. `--demo` runs a scripted model.

## 1. Scaffold

A component is a `cdylib` crate in its own workspace. Vendor the contract
into it: the `path` in `generate!` is resolved at compile time against your
crate, and tracking one contract version is what keeps your build
reproducible when the contract moves.

```bash
mkdir -p wordcount/src wordcount/wit && cd wordcount
curl -fsSL -o wit/tau.wit \
    https://raw.githubusercontent.com/gqf2008/tau/v0.6.0/wit/tau.wit
```

Match that tag to the tau you will load with — the `package` line in the
file *is* the contract version. The same file ships inside the published
`tau-ext` crate, and sits at `wit/tau.wit` in a checkout.

`Cargo.toml`:

```toml
[package]
name = "wordcount"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
wit-bindgen = "0.46"
serde_json = "1"

[workspace]
```

`src/lib.rs`:

```rust
//! A minimal tau extension: one tool, no probes.

wit_bindgen::generate!({
    path: "wit/tau.wit", // the vendored copy in this repo
    world: "extension",
});

use exports::tau::extension::probes::{Action, Guest as Probes, Verdict};
use exports::tau::extension::tools::{Definition, Guest as Tools, ToolResult};
use tau::extension::types::ResultBlock;

struct Wordcount;

impl Tools for Wordcount {
    fn definitions() -> Vec<Definition> {
        vec![Definition {
            name: "wordcount".into(),
            description: "Count the words and characters in a text".into(),
            parameters_json: r#"{
                "type": "object",
                "properties": { "text": { "type": "string", "description": "text to measure" } },
                "required": ["text"]
            }"#
            .into(),
        }]
    }

    fn execute(name: String, arguments_json: String) -> ToolResult {
        if name != "wordcount" {
            return ToolResult {
                content: vec![ResultBlock::Text(format!("unknown tool: {name}"))],
                is_error: true,
            };
        }
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&arguments_json);
        match parsed.ok().and_then(|v| v["text"].as_str().map(str::to_string)) {
            Some(text) => ToolResult {
                content: vec![ResultBlock::Text(format!(
                    "{} words, {} characters",
                    text.split_whitespace().count(),
                    text.chars().count()
                ))],
                is_error: false,
            },
            None => ToolResult {
                content: vec![ResultBlock::Text("missing string argument 'text'".into())],
                is_error: true,
            },
        }
    }
}

impl Probes for Wordcount {
    fn points() -> Vec<String> {
        Vec::new() // this extension handles no probe points
    }
    fn probe(_point: String, _payload_json: String) -> Verdict {
        Verdict {
            action: Action::Continue,
            payload_json: None,
            reason: None,
        }
    }
}

export!(Wordcount);
```

Four things about that listing:

- `world: "extension"` (the `world extension { … }` block in `wit/tau.wit`) exports **both**
  `tools` and `probes`, so both are implemented even though this extension
  never probes. An empty `points()` means the host never calls `probe` —
  the interface contract, not a stub you have to grow into.
- `parameters_json` is a JSON Schema **string**: JSON Schema is a schema
  language tau does not own, so it stays JSON where the message trunk is
  typed.
- Failures come back as `is_error: true` with a text block (the
  `unknown tool:` branch), never as a panic — a panicking component is a
  failed call with no diagnosis attached.
- The empty `[workspace]` table opts this crate out of an enclosing
  workspace; components build standalone.

## 2. Build

```bash
cargo build --target wasm32-wasip2 --release
```

```
   Compiling wordcount v0.1.0 (…\wordcount)
    Finished `release` profile [optimized] target(s) in 23.14s
```

The artifact is `target/wasm32-wasip2/release/wordcount.wasm` — 116 KB
here. One naming detail: a crate named `my-ext` produces `my_ext.wasm`
(dashes become underscores), which is exactly how the shipped examples are
named (`examples/media-tool` → `media_tool.wasm`).

## 3. First run: load it unsigned

The default policy is `RequireTrusted`, so a brand-new component needs the
development escape hatch:

```bash
tau --allow-unsigned -e target/wasm32-wasip2/release/wordcount.wasm --demo \
    --session ./session.jsonl -p "count the words in this sentence"
```

```
[tau] loaded extension: wordcount
[tau]   tool: wordcount

[tau] tool → wordcount
[tau] tool ← wordcount: 6 words, 32 characters
tau is alive. The tool answered: 6 words, 32 characters. (faux model — set ANTHROPIC_API_KEY or OPENAI_API_KEY for a real one)
[tau] session: ./session.jsonl
```

What just happened: the load line and the `tool:` line are the host reading
your `definitions()`; `--demo` is a scripted model that calls the **first**
tool once, filling required string parameters with your prompt text (numbers
with `1`, booleans with `true`; other required shapes skip the call), then
answers in prose. It is deterministic and offline, which is why it is the
right way to test a component — `docs/extensions.md` §9's checklist is about
the real model, this is about your code.

`--session` is optional and defaults to `.tau/session.jsonl` in the current
directory; the run above writes its transcript there.

Drop the escape hatch and the same command refuses, before compiling
anything:

```bash
tau -e target/wasm32-wasip2/release/wordcount.wasm --demo \
    --session ./session.jsonl -p "count the words in this sentence"
```

```
Error: loading target/wasm32-wasip2/release/wordcount.wasm

Caused by:
    extension target/wasm32-wasip2/release/wordcount.wasm: component is unsigned (sign it with `tau sign`, or load with --allow-unsigned)
```

## 4. Sign it

Once per machine:

```bash
tau keygen
```

```
key generated and trusted: f7fecae971a390ca
  secret: C:\Users\gxh\.tau\keys\f7fecae971a390ca.key
```

`keygen` writes the secret to `~/.tau/keys/<fingerprint>.key` and the pubkey
to `~/.tau/trust/<fingerprint>.pub` — a key you generate is a key you trust.
The fingerprint is the first 16 hex chars of the pubkey's SHA-256; it is
your identity as a publisher (and the key under which capability consent is
remembered, see `docs/signing.md`). The secret never leaves `~/.tau/keys`.

Then sign the artifact — in place, after the build:

```bash
tau sign target/wasm32-wasip2/release/wordcount.wasm
```

```
signed target/wasm32-wasip2/release/wordcount.wasm with f7fecae971a390ca
```

The signature is an embedded custom section (`tau-signature`), so the file
stays one distributable artifact, and the signature covers the module with
every such section stripped — re-signing is well-defined. If your keyring
holds more than one key, `tau sign` refuses until you name one (this
machine's keyring did; the count is whatever your `~/.tau/keys` holds):

```
Error: invalid signing key: expected exactly one key in C:\Users\gxh\.tau\keys, found 2 — pass --key
```

…so pass `--key <fingerprint>`; `ls ~/.tau/keys` lists the candidates. (On
0.6.0 that refusal opened with `not a wasm binary:` — the key error shared
the wasm parser's variant, so it named a file it never opens.)
Now the run from §3 works unchanged, without the escape hatch:

```
[tau] loaded extension: wordcount
[tau]   tool: wordcount

[tau] tool → wordcount
[tau] tool ← wordcount: 6 words, 32 characters
tau is alive. The tool answered: 6 words, 32 characters. (faux model — set ANTHROPIC_API_KEY or OPENAI_API_KEY for a real one)
[tau] session: ./session.jsonl
```

What signing buys you, and what it does not, is worth internalizing before
you publish: it is tamper evidence (any byte change breaks verification),
publisher identity (the fingerprint), and the handle for remembered consent.
It is not a sandbox — a trusted component gets nothing beyond its world's
imports and the host's consent gates. `--allow-unsigned` only excuses an
*absent* signature: a component whose signature does not verify is refused
under every policy. And a fresh `cargo build` produces an unsigned artifact
again — sign after the final build, every time.

## 5. Publish and load it over OCI

Any registry v2 works; the reference forms are
`oci://<host>/<repo>:<tag>` and `oci://<host>/<repo>@sha256:<digest>`.
A tag is mutable, a digest is not — publish by tag, consume by digest.

Pushing:

```bash
tau push target/wasm32-wasip2/release/wordcount.wasm \
    oci://ghcr.io/<you>/wordcount:0.1.0
```

That is the real-registry form (`docs/oci.md` has the auth details: ghcr and
friends take `TAU_REGISTRY_USER` / `TAU_REGISTRY_PASSWORD`, which tau adds as
basic auth to the token request; loopback registries are allowed plain
http, everything else must be https). The walkthrough's transcripts above
ran against the repo's mock registry instead, because it needs no account —
it is the same fixture `scripts/validate.sh` step 7 uses, from a checkout:

```bash
python crates/tau-ext/tests/mock_oci_registry.py 8406 wordcount.wasm --auth &
```

(`--auth` makes it 401 with a `WWW-Authenticate` challenge and serve tokens,
i.e. exercise the same bearer dance a real registry does.)

The push, against `oci://127.0.0.1:8406/test/component:v1`:

```
pushed oci://127.0.0.1:8406/test/component:v1 (sha256:ac210b76b27f855cdd3322a7fe2290e5fab28e67663b27fe5583fdc0f2a933ca)
```

Loading it back is the same load path as a local file — the bytes are pulled
into a content-addressed cache, verified against the digest, and then
checked for signature and trust exactly as in §4:

```
[tau] oci: oci://127.0.0.1:8406/test/component:v1 -> sha256:ac210b76… (…\.tau\oci\blobs\sha256_ac210b76…)
[tau] note: mutable tag — pin @sha256:ac210b76b27f855cdd3322a7fe2290e5fab28e67663b27fe5583fdc0f2a933ca for reproducible loads
[tau] loaded extension: sha256_ac210b76b27f855cdd3322a7fe2290e5fab28e67663b27fe5583fdc0f2a933ca
[tau]   tool: wordcount

[tau] tool → wordcount
[tau] tool ← wordcount: 6 words, 32 characters
```

Notice it loaded without `--allow-unsigned`: the signature travelled with
the bytes and the key was already trusted here. Pin the digest when you want
the load to be reproducible — the manifest is re-fetched every time (so a
mutable tag sees new digests), while blobs are cached under
`~/.tau/oci/blobs/<sha256:… with : as _>`:

```bash
tau -e oci://127.0.0.1:8406/test/component@sha256:ac210b76b27f855cdd3322a7fe2290e5fab28e67663b27fe5583fdc0f2a933ca \
    --demo -p "count the words in this sentence"
```

### The consumer side

A signed component carries everything needed to onboard its author: the
signature section embeds the signer's pubkey, and `tau trust
--from-component` verifies each embedded signature against the exact bytes
before trusting it. It accepts `oci://` references:

```
trusted: f7fecae971a390ca (from oci://127.0.0.1:8406/test/component:v1)
verify this fingerprint out-of-band before relying on it
```

That last line is the whole trust model in one sentence: the command
onboards *whoever signed these bytes*; only you can confirm that is who you
expected. Confirm the fingerprint through a channel you trust (a signed
release note, your own site) before relying on it — and remember the
consumer may equally choose `--allow-unsigned`, which is fine for their
development and unacceptable for their production.

## 6. Where to go next

- **Probes** — influence a run instead of just serving calls:
  `docs/probes.md`, and `tau probes` prints the point list with payload
  shapes and verdict semantics for the tau you have installed.
- **Other worlds** — a component can also be a provider (`world provider`),
  a realtime provider (`world realtime`), or a bridge (`world bridge`, the
  external-protocol world: stdio/http/websocket + ingress):
  `docs/extensions.md` §5–§6, `docs/bridges.md`.
- **The host channel** — notify/emit facts, ask for consent, steer the run:
  `docs/extensions.md` §4, `docs/host-channel.md`.
- **Other languages** — this walkthrough is Rust (`wit-bindgen`); the
  contract is a WIT world, and `docs/wasm-languages.md` carries the build
  commands and acceptance for C, C++, Go, Java, JavaScript, Python,
  TypeScript — the `examples/{c,cpp,go,java,js,python,ts}-upper` crates are
  the same tool in each.
- **Capabilities and consent** — ambient WASI is on by default; scoped
  capabilities (bridge spawn, provider origins) are gated, and a signed
  component can remember grants per fingerprint with `--remember`
  (`tau consent --list`, `--revoke`). Unsigned components can never be
  remembered: no fingerprint, no memory.
- **Media results** — a tool result can carry images/audio/files, not just
  text: `docs/tool-media.md`.
- **Before you publish** — `docs/extensions.md` §9's checklist
  (fast `definitions()`, no panics, fast probes, works under `--deny-wasi`,
  signed after the final build, fingerprint published out-of-band).

## 7. Traps

Each of these was hit for real while writing this; they are the failures
that read like something else.

- **A vendored WIT that no longer matches the host.** After a contract bump
  the refusal names both versions and the fix, which is why it is worth
  vendor *and* re-vendor deliberately:
  `no exported instance named tau:extension/tools@0.6.0 [component targets
  tau:extension@0.5.0; this host requires @0.6.0; rebuild with the 0.6.0
  bindings (wit/tau.wit), see CHANGELOG.md]`.
- **More than one key in the keyring** → `tau sign` refuses until you pass
  `--key <fingerprint>` (see §4).
- **A rebuild erases the signature.** `tau sign` is a post-build step; a
  component that loaded yesterday is unsigned after `cargo build`.
- **Paths on Windows.** A native `tau.exe` cannot read an MSYS `/tmp/...`
  argument — it fails as `Error: reading /tmp/...` + `os error 3`, which
  reads like a broken component and is not one. Use `C:/...` forms or
  relative paths.
- **Tags move, digests do not.** A tag reference prints the resolved digest
  and warns; pin `@sha256:…` when a reproducible load matters.

## Keeping this document honest

Every command above was run as written against tau 0.6.0 — the `tau sign`
refusal in §4 was re-captured on the fix that followed it, as it says — and
the two listings were compiled by extracting them from *this file*, not
copied from a source tree that happens to be in sync. That is also the
maintenance rule: when the listings or the contract change, re-run the
document the same way before trusting it. A tutorial that quotes output is a
tutorial that has to be re-run — `116 KB`, the digest, and `6 words, 32
characters` are 0.6.0's numbers, and none of them ages well on its own.
