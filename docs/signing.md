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
tau trust --from-component component.wasm   # trust the key(s) embedded in a signed
                           #  component — local file or oci:// reference
tau trust --list           # show trusted fingerprints
```

## Receiving a signed component

The signature section embeds the signer's pubkey, so a signed component
carries everything needed to onboard its author: `tau trust
--from-component` verifies each embedded signature against the exact
bytes and trusts only keys that verify, then prints the fingerprints.
**Verify the fingerprint out-of-band** (the publisher's site, a signed
release note) before relying on it — the command onboards *whoever
signed these bytes*; only you can confirm that is who you expected.
Works on `oci://` references too (the component is pulled, then trusted
from the verified local bytes).

## Enforcement

The CLI loads components with `TrustPolicy::RequireTrusted`; unsigned or
untrusted components are refused before compilation:

```
Error: loading mcp bridge mcp_bridge.wasm

Caused by:
    extension mcp_bridge.wasm: component is unsigned (sign it with `tau sign`, or load with --allow-unsigned)
```

`--allow-unsigned` is the explicit escape hatch for development. It only
excuses *absent* signatures: a component whose signature section does not
parse or verify is refused under every policy — unsigned is a developer's
choice, a corrupt signature is evidence of tampering, and the escape hatch
must not launder it. A signed
component whose key is not in the trust store fails with the fingerprint
and the exact `tau trust --from-component` command that would onboard it
— signing proves authorship, trusting is a separate decision.

## What this does and does not do

- **Does**: tamper evidence (any byte change breaks verification), publisher
  identity (the fingerprint is the component's author id), and the
  onboarding decision itself — trusting a fingerprint is the one
  authorization act left. 0.8.0 deleted the consent store with the gates:
  there is no `~/.tau/consent/*`, no `--remember`, no
  `tau consent --list` / `--revoke`, and no per-fingerprint capability
  grants, because there is no call-time gate to pass.
- **Does not**: say what a component may do. A trusted component gets
  ambient WASI and runs with the permissions of the tau process, exactly
  like any other component its world admits (`docs/extensions.md` §7).
  Signature answers "who wrote this and was it modified"; the world it was
  installed as says what it can reach; neither is a sandbox.
