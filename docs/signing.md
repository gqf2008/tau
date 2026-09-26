# Signing and trust

Every component tau loads — extension, provider, or bridge — must carry a
valid ed25519 signature from a trusted key. This is the product default; the
library API stays lenient (`ExtensionHost::new()` loads unsigned code) so
embedders and tests choose their own policy.

## Format

The signature lives **inside the .wasm file** as a custom section named
`tau-signature` — one file to distribute, matching the "drop a .wasm to
extend" philosophy. The section payload is JSON:

```json
{ "key": "<base64 ed25519 pubkey>", "sig": "<base64 signature>" }
```

The signed message is the SHA-256 of the module with every `tau-signature`
section stripped, so re-signing is well-defined and the signature never
covers itself. Verification: strip → hash → verify → check trust.

## Keys and the trust store

```
~/.tau/keys/<fingerprint>.key    secret seed (base64) — never leaves this dir
~/.tau/trust/<fingerprint>.pub   trusted pubkeys, one per file
```

Fingerprint: first 16 hex chars of the pubkey's SHA-256.

```
tau keygen                 # generate a keypair; the pubkey is trusted automatically
tau sign component.wasm    # embed a signature (uses the single key, or --key <fp>)
tau trust <base64-pubkey>  # trust someone else's key
tau trust --list           # show trusted fingerprints
```

## Enforcement

The CLI loads components with `TrustPolicy::RequireTrusted`; unsigned or
untrusted components are refused before compilation:

```
Error: loading mcp bridge mcp_bridge.wasm

Caused by:
    extension mcp_bridge.wasm: component is unsigned (sign it with `tau sign`, or load with --allow-unsigned)
```

`--allow-unsigned` is the explicit escape hatch for development. A signed
component whose key is not in the trust store fails the same way — signing
proves authorship, trusting is a separate decision.

## What this does and does not do

- **Does**: tamper evidence (any byte change breaks verification), publisher
  identity (the fingerprint is the component's author id), and the foundation
  for remembered consent — capability grants can later be recorded per
  fingerprint instead of re-asked per run.
- **Does not**: replace the sandbox. A trusted component still gets no
  capabilities beyond its world's imports and the host's consent gates.
  Signature answers "who wrote this and was it modified", the sandbox answers
  "what can it do". Both are needed; neither implies the other.
