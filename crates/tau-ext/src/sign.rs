//! Component signing and trust: ed25519 signatures embedded in a wasm
//! custom section, verified against a filesystem trust store at load time.
//!
//! Format: a custom section named `tau-signature` whose payload is JSON
//! `{"key": "<base64 pubkey>", "sig": "<base64 signature>"}`. The signed
//! message is the SHA-256 of the module with every `tau-signature` section
//! stripped, so signing never changes what is being signed.
//!
//! Trust store: `~/.tau/trust/<fingerprint>.pub` (base64 ed25519 pubkey),
//! keys in `~/.tau/keys/<fingerprint>.key` (base64 secret seed).
//! Fingerprint: first 16 hex chars of the pubkey's SHA-256.

use std::path::{Path, PathBuf};

use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Name of the custom wasm section holding the signature JSON
/// (`{"key": base64-pubkey, "sig": base64-sig}` per signer).
pub const SIGNATURE_SECTION: &str = "tau-signature";

/// Failures signing, verifying, or trusting components.
#[derive(Debug, Error)]
pub enum SignError {
    /// Input was not a parseable wasm binary.
    #[error("not a wasm binary: {0}")]
    Malformed(String),
    /// Key material handed over directly (a base64 pubkey) was malformed.
    #[error("invalid public key: {0}")]
    BadKey(String),
    /// Signing-key material on disk — or the `--key` fingerprint naming it
    /// — was unusable.
    #[error("invalid signing key: {0}")]
    BadSigningKey(String),
    /// The signature section's JSON did not parse.
    #[error("signature section is not valid json: {0}")]
    BadSignatureJson(String),
    /// A signature failed ed25519 verification.
    #[error("signature does not verify: {0}")]
    BadSignature(String),
    /// The component carries no signature section.
    #[error("component is unsigned (sign it with `tau sign`, or load with --allow-unsigned)")]
    Unsigned,
    /// The signature verifies, but its key is not trusted yet.
    #[error(
        "signing key {0} is not in the trust store ({1}) — trust it with `tau trust --from-component <component>` after verifying the fingerprint out-of-band"
    )]
    Untrusted(String, String),
    /// Filesystem failure (key/trust store access).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Load-time policy for component signatures.
#[derive(Clone)]
pub enum TrustPolicy {
    /// Load unsigned components (and validly-signed ones). A component
    /// that carries a signature section which does not parse or verify
    /// is still refused: unsigned is a developer's choice, a corrupt
    /// signature is evidence of tampering, and a flag named
    /// "allow unsigned" must not silently downgrade tampered bytes to
    /// loadable. Library default; the CLI product defaults to
    /// RequireTrusted and exposes this as --allow-unsigned.
    AllowUnsigned,
    /// Require a valid signature whose key is in `trust_dir`.
    RequireTrusted {
        /// Directory of trusted `<fingerprint>.pub` files.
        trust_dir: PathBuf,
    },
}

// ---------------------------------------------------------------------------
// Wasm binary sections (hand-rolled: id byte, LEB128 size, payload)
// ---------------------------------------------------------------------------

fn read_leb128(bytes: &[u8], pos: &mut usize) -> Result<u64, SignError> {
    let mut result = 0u64;
    let mut shift = 0;
    loop {
        let byte = *bytes
            .get(*pos)
            .ok_or_else(|| SignError::Malformed("truncated leb128".into()))?;
        *pos += 1;
        result |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
        if shift >= 64 {
            return Err(SignError::Malformed("leb128 too long".into()));
        }
    }
}

fn write_leb128(mut value: u64, out: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            return;
        }
    }
}

fn check_header(wasm: &[u8]) -> Result<(), SignError> {
    if wasm.len() < 8 || &wasm[..4] != b"\0asm" {
        return Err(SignError::Malformed("bad magic".into()));
    }
    Ok(())
}

/// The module bytes with every `tau-signature` custom section removed:
/// the canonical signed payload.
pub fn strip_signatures(wasm: &[u8]) -> Result<Vec<u8>, SignError> {
    check_header(wasm)?;
    let mut out = wasm[..8].to_vec();
    let mut pos = 8;
    while pos < wasm.len() {
        let id = wasm[pos];
        pos += 1;
        let size = read_leb128(wasm, &mut pos)? as usize;
        let end = pos
            .checked_add(size)
            .filter(|end| *end <= wasm.len())
            .ok_or_else(|| SignError::Malformed("section overruns module".into()))?;
        let payload = &wasm[pos..end];
        let is_signature = id == 0 && custom_section_name(payload) == Some(SIGNATURE_SECTION);
        if !is_signature {
            out.push(id);
            write_leb128(size as u64, &mut out);
            out.extend_from_slice(payload);
        }
        pos = end;
    }
    Ok(out)
}

fn custom_section_name(payload: &[u8]) -> Option<&str> {
    let mut pos = 0;
    let len = read_leb128(payload, &mut pos).ok()? as usize;
    let end = pos.checked_add(len)?;
    std::str::from_utf8(payload.get(pos..end)?).ok()
}

/// Append a `tau-signature` custom section carrying `payload`.
pub fn embed_signature(wasm: &[u8], payload: &[u8]) -> Result<Vec<u8>, SignError> {
    check_header(wasm)?;
    let mut out = wasm.to_vec();
    let mut body = Vec::new();
    write_leb128(SIGNATURE_SECTION.len() as u64, &mut body);
    body.extend_from_slice(SIGNATURE_SECTION.as_bytes());
    body.extend_from_slice(payload);
    out.push(0);
    write_leb128(body.len() as u64, &mut out);
    out.extend_from_slice(&body);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Keys, signing, verifying
// ---------------------------------------------------------------------------

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

/// Short hex fingerprint of a public key (first 16 hex chars of its
/// sha256) — the trust store's filename and the consent store's key.
pub fn fingerprint(key: &VerifyingKey) -> String {
    hex_prefix(&Sha256::digest(key.as_bytes()), 16)
}

/// Fingerprints are 16 lowercase hex chars (see [`fingerprint`]).
/// Anything taking a fingerprint from a caller (CLI flags, store
/// lookups) must check this shape before building a path from it —
/// `../x` must never become a filename outside the store.
pub fn fingerprint_shaped(fp: &str) -> bool {
    fp.len() == 16
        && fp
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn hex_prefix(bytes: &[u8], chars: usize) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .take(chars / 2)
        .collect()
}

struct SignaturePayload {
    key: VerifyingKey,
    sig: Signature,
}

fn encode_payload(key: &VerifyingKey, sig: &Signature) -> Vec<u8> {
    serde_json::json!({
        "key": b64().encode(key.as_bytes()),
        "sig": b64().encode(sig.to_bytes()),
    })
    .to_string()
    .into_bytes()
}

fn decode_payloads(wasm: &[u8]) -> Result<Vec<SignaturePayload>, SignError> {
    check_header(wasm)?;
    let mut payloads = Vec::new();
    let mut pos = 8;
    while pos < wasm.len() {
        let id = wasm[pos];
        pos += 1;
        let size = read_leb128(wasm, &mut pos)? as usize;
        let end = pos
            .checked_add(size)
            .filter(|end| *end <= wasm.len())
            .ok_or_else(|| SignError::Malformed("section overruns module".into()))?;
        let payload = &wasm[pos..end];
        if id == 0 && custom_section_name(payload) == Some(SIGNATURE_SECTION) {
            let mut name_pos = 0;
            let name_len = read_leb128(payload, &mut name_pos)? as usize;
            let json = &payload[name_pos + name_len..];
            let parsed: serde_json::Value = serde_json::from_slice(json)
                .map_err(|e| SignError::BadSignatureJson(e.to_string()))?;
            let key_bytes = b64()
                .decode(parsed["key"].as_str().unwrap_or_default())
                .map_err(|e| SignError::BadSignatureJson(e.to_string()))?;
            let sig_bytes = b64()
                .decode(parsed["sig"].as_str().unwrap_or_default())
                .map_err(|e| SignError::BadSignatureJson(e.to_string()))?;
            let key = VerifyingKey::from_bytes(
                key_bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| SignError::BadSignatureJson("key must be 32 bytes".into()))?,
            )
            .map_err(|e| SignError::BadSignatureJson(e.to_string()))?;
            let sig = Signature::from_bytes(
                sig_bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| SignError::BadSignatureJson("sig must be 64 bytes".into()))?,
            );
            payloads.push(SignaturePayload { key, sig });
        }
        pos = end;
    }
    Ok(payloads)
}

/// Verify every embedded signature against the stripped module bytes.
/// Returns the signing keys. Unsigned modules return an empty vec.
pub fn verify(wasm: &[u8]) -> Result<Vec<VerifyingKey>, SignError> {
    let payloads = decode_payloads(wasm)?;
    if payloads.is_empty() {
        return Ok(Vec::new());
    }
    let digest = Sha256::digest(strip_signatures(wasm)?);
    payloads
        .into_iter()
        .map(|p| {
            p.key
                .verify_strict(digest.as_slice(), &p.sig)
                .map_err(|e| SignError::BadSignature(e.to_string()))?;
            Ok(p.key)
        })
        .collect()
}

/// Sign `wasm` with `key`: strips existing signatures, signs the canonical
/// bytes, embeds the new signature section.
pub fn sign(wasm: &[u8], key: &SigningKey) -> Result<Vec<u8>, SignError> {
    let stripped = strip_signatures(wasm)?;
    let digest = Sha256::digest(&stripped);
    let signature = key.sign(digest.as_slice());
    embed_signature(&stripped, &encode_payload(&key.verifying_key(), &signature))
}

/// Enforce a trust policy on raw component bytes.
pub fn check_policy(wasm: &[u8], policy: &TrustPolicy) -> Result<(), SignError> {
    let TrustPolicy::RequireTrusted { trust_dir } = policy else {
        // Absent signature section: fine, that is what the flag allows.
        // Present but unverifiable: refuse — see the variant's docs.
        verify(wasm)?;
        return Ok(());
    };
    let keys = verify(wasm)?;
    if keys.is_empty() {
        return Err(SignError::Unsigned);
    }
    for key in &keys {
        let fp = fingerprint(key);
        if trust_dir.join(format!("{fp}.pub")).is_file() {
            return Ok(());
        }
    }
    let fps: Vec<String> = keys.iter().map(fingerprint).collect();
    Err(SignError::Untrusted(
        fps.join(", "),
        trust_dir.display().to_string(),
    ))
}

// ---------------------------------------------------------------------------
// Filesystem: config dir, keygen, trust store
// ---------------------------------------------------------------------------

/// tau's per-user config root (`~/.tau`).
pub fn config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".tau")
}

/// Where private signing keys live (`~/.tau/keys`).
pub fn keys_dir() -> PathBuf {
    config_dir().join("keys")
}

/// Where trusted public keys live (`~/.tau/trust`).
pub fn trust_dir() -> PathBuf {
    config_dir().join("trust")
}

/// Generate a new keypair: secret into keys/, pubkey into trust/ (a key you
/// generate is one you trust). Returns the fingerprint.
pub fn keygen() -> Result<String, SignError> {
    let key = SigningKey::generate(&mut rand::rngs::OsRng);
    let fp = fingerprint(&key.verifying_key());
    std::fs::create_dir_all(keys_dir())?;
    std::fs::create_dir_all(trust_dir())?;
    std::fs::write(
        keys_dir().join(format!("{fp}.key")),
        b64().encode(key.to_bytes()),
    )?;
    std::fs::write(
        trust_dir().join(format!("{fp}.pub")),
        b64().encode(key.verifying_key().to_bytes()),
    )?;
    Ok(fp)
}

/// Load a signing key by fingerprint; with None, the single key in keys/.
pub fn load_key(fp: Option<&str>) -> Result<(String, SigningKey), SignError> {
    load_key_in(&keys_dir(), fp)
}

/// [`load_key`] against an explicit keys directory (tests use a temp dir).
///
/// Every failure here is about the KEY, and renders as such: these paths
/// used to share the module parser's `Malformed` variant and so read as
/// "not a wasm binary" — a message about a file that was never opened.
fn load_key_in(dir: &Path, fp: Option<&str>) -> Result<(String, SigningKey), SignError> {
    let chosen = match fp {
        Some(fp) => {
            if !fingerprint_shaped(fp) {
                return Err(SignError::BadSigningKey(format!(
                    "{fp:?} is not a signing fingerprint (16 lowercase hex chars)"
                )));
            }
            fp.to_string()
        }
        None => {
            let mut keys: Vec<_> = std::fs::read_dir(dir)?
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|x| x == "key"))
                .collect();
            if keys.len() != 1 {
                return Err(SignError::BadSigningKey(format!(
                    "expected exactly one key in {}, found {} — pass --key",
                    dir.display(),
                    keys.len()
                )));
            }
            keys.remove(0)
                .file_name()
                .to_string_lossy()
                .replace(".key", "")
        }
    };
    let bytes = std::fs::read(dir.join(format!("{chosen}.key")))?;
    let seed: [u8; 32] = b64()
        .decode(bytes)
        .map_err(|e| SignError::BadSigningKey(e.to_string()))?
        .as_slice()
        .try_into()
        .map_err(|_| SignError::BadSigningKey("key must be 32 bytes".into()))?;
    Ok((chosen, SigningKey::from_bytes(&seed)))
}

/// Add a raw base64 pubkey to the trust store. Returns the fingerprint.
/// Add a base64 public key to the default trust store; returns its
/// fingerprint.
pub fn trust_key(pubkey_b64: &str) -> Result<String, SignError> {
    trust_key_in(&trust_dir(), pubkey_b64)
}

/// [`trust_key`] against an explicit trust directory.
pub fn trust_key_in(dir: &Path, pubkey_b64: &str) -> Result<String, SignError> {
    let bytes: [u8; 32] = b64()
        .decode(pubkey_b64.trim())
        .map_err(|e| SignError::BadKey(e.to_string()))?
        .as_slice()
        .try_into()
        .map_err(|_| SignError::BadKey("pubkey must be 32 bytes".into()))?;
    let key = VerifyingKey::from_bytes(&bytes).map_err(|e| SignError::BadKey(e.to_string()))?;
    let fp = fingerprint(&key);
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join(format!("{fp}.pub")), pubkey_b64.trim())?;
    Ok(fp)
}

/// Trust every key whose signature verifies on this component: the
/// signature section embeds the pubkeys, and verification already proved
/// they sign these exact bytes — so trusting what verifies is sound.
/// Returns the fingerprints, which the caller MUST print for out-of-band
/// verification (the whole point of the chain). Unsigned components get
/// [`SignError::Unsigned`].
pub fn trust_component_keys(wasm: &[u8]) -> Result<Vec<String>, SignError> {
    trust_component_keys_in(&trust_dir(), wasm)
}

/// [`trust_component_keys`] against an explicit trust directory.
pub fn trust_component_keys_in(dir: &Path, wasm: &[u8]) -> Result<Vec<String>, SignError> {
    let keys = verify(wasm)?;
    if keys.is_empty() {
        return Err(SignError::Unsigned);
    }
    keys.iter()
        .map(|key| trust_key_in(dir, &b64().encode(key.to_bytes())))
        .collect()
}

/// Sign a component file in place.
pub fn sign_file(path: &Path, key: &SigningKey) -> Result<String, SignError> {
    let wasm = std::fs::read(path)?;
    let signed = sign(&wasm, key)?;
    std::fs::write(path, signed)?;
    Ok(fingerprint(&key.verifying_key()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A garbage pubkey must be refused as a bad KEY — the shared
    /// Malformed variant used to misreport it as "not a wasm binary".
    #[test]
    fn trust_key_rejects_garbage_with_a_key_shaped_message() {
        let dir = std::env::temp_dir().join(format!("tau-badkey-{}", std::process::id()));
        let err = trust_key_in(&dir, "not-base64!!!").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("invalid public key"), "got: {message}");
        assert!(!message.contains("wasm"), "misleading: {message}");

        // Valid base64, wrong length.
        let err = trust_key_in(&dir, "aGVsbG8=").unwrap_err();
        assert!(err.to_string().contains("32 bytes"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Minimal valid module: magic + version + one custom section.
    fn tiny_module() -> Vec<u8> {
        let mut wasm = b"\0asm\x01\0\0\0".to_vec();
        let mut body = Vec::new();
        write_leb128(4, &mut body);
        body.extend_from_slice(b"test");
        body.extend_from_slice(b"hello");
        wasm.push(0);
        write_leb128(body.len() as u64, &mut wasm);
        wasm.extend_from_slice(&body);
        wasm
    }

    #[test]
    fn sign_verify_round_trip() {
        let key = SigningKey::generate(&mut rand::rngs::OsRng);
        let signed = sign(&tiny_module(), &key).unwrap();
        let keys = verify(&signed).unwrap();
        assert_eq!(keys, vec![key.verifying_key()]);
    }

    #[test]
    fn tampered_module_fails_verification() {
        let key = SigningKey::generate(&mut rand::rngs::OsRng);
        let mut signed = sign(&tiny_module(), &key).unwrap();
        // Flip a byte in the original custom section (before the signature
        // section appended at the end).
        signed[12] ^= 0xff;
        assert!(matches!(verify(&signed), Err(SignError::BadSignature(_))));
    }

    /// Every signing-key failure must read as a key problem, never as a
    /// wasm parse failure: `load_key` used to share the parser's
    /// `Malformed` variant, so an ambiguous keyring was reported as
    /// "not a wasm binary" — a message about a file it never opened.
    #[test]
    fn load_key_errors_are_key_shaped() {
        let dir = std::env::temp_dir().join(format!("tau-loadkey-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let message = |fp: Option<&str>| match load_key_in(&dir, fp) {
            Ok(_) => panic!("expected an error for {fp:?}"),
            Err(e) => e.to_string(),
        };
        let assert_key_shaped = |m: &str, what: &str| {
            assert!(m.starts_with("invalid signing key"), "{what} got: {m}");
            assert!(!m.contains("wasm"), "{what} misleads: {m}");
            assert_ne!(m, "invalid signing key: ", "{what} carries no detail");
        };
        let write_key = |name: &str, bytes: &[u8]| {
            std::fs::write(dir.join(format!("{name}.key")), bytes).unwrap()
        };

        // The shape check fires before any filesystem access, so
        // `tau sign --key "../x"` must die here, not in a path join.
        let m = message(Some("../escape"));
        assert!(m.contains("not a signing fingerprint"), "got: {m}");
        assert_key_shaped(&m, "path-shaped --key");
        assert_key_shaped(&message(Some("AAAAAAAAAAAAAAAA")), "uppercase --key");

        // Keyring selection: zero then two keys, both named by count.
        let m = message(None);
        assert!(m.contains("found 0"), "got: {m}");
        assert_key_shaped(&m, "empty keyring");
        write_key("0000000000000000", b"x");
        write_key("0000000000000001", b"x");
        let m = message(None);
        assert!(m.contains("found 2"), "got: {m}");
        assert_key_shaped(&m, "ambiguous keyring");

        // The selected file itself: not base64, then not 32 bytes.
        assert_key_shaped(&message(Some("0000000000000000")), "undecodable key");
        write_key("0000000000000000", b64().encode([7u8; 16]).as_bytes());
        let m = message(Some("0000000000000000"));
        assert!(m.contains("32 bytes"), "got: {m}");
        assert_key_shaped(&m, "short key");

        // And the happy path still resolves the file named by the fingerprint.
        write_key("0000000000000000", b64().encode([7u8; 32]).as_bytes());
        let (fp, key) = load_key_in(&dir, Some("0000000000000000")).unwrap();
        assert_eq!(fp, "0000000000000000");
        assert_eq!(
            key.verifying_key().to_bytes(),
            SigningKey::from_bytes(&[7u8; 32]).verifying_key().to_bytes()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resigning_replaces_old_signature() {
        let key_a = SigningKey::generate(&mut rand::rngs::OsRng);
        let key_b = SigningKey::generate(&mut rand::rngs::OsRng);
        let signed = sign(&sign(&tiny_module(), &key_a).unwrap(), &key_b).unwrap();
        let keys = verify(&signed).unwrap();
        assert_eq!(keys, vec![key_b.verifying_key()]);
    }

    #[test]
    fn policy_gates_unsigned_and_untrusted() {
        let dir = std::env::temp_dir().join(format!("tau-test-trust-{}", std::process::id()));
        let policy = TrustPolicy::RequireTrusted {
            trust_dir: dir.clone(),
        };
        // unsigned → rejected
        assert!(matches!(
            check_policy(&tiny_module(), &policy),
            Err(SignError::Unsigned)
        ));
        // signed but untrusted → rejected
        let key = SigningKey::generate(&mut rand::rngs::OsRng);
        let signed = sign(&tiny_module(), &key).unwrap();
        assert!(matches!(
            check_policy(&signed, &policy),
            Err(SignError::Untrusted(_, _))
        ));
        // trust the key → accepted
        trust_key_in(&dir, &b64().encode(key.verifying_key().to_bytes())).unwrap();
        check_policy(&signed, &policy).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn allow_unsigned_still_refuses_a_corrupt_signature() {
        // Unsigned loads: that is the flag's whole point.
        check_policy(&tiny_module(), &TrustPolicy::AllowUnsigned).unwrap();
        // Validly signed loads (any key — no trust-dir consult).
        let key = SigningKey::generate(&mut rand::rngs::OsRng);
        let signed = sign(&tiny_module(), &key).unwrap();
        check_policy(&signed, &TrustPolicy::AllowUnsigned).unwrap();
        // Signature section present but bytes flipped: refused, not
        // silently downgraded to "unsigned". Flip a byte deep inside
        // the signature payload (near the end of the module).
        let mut tampered = signed.clone();
        let n = tampered.len();
        tampered[n - 20] ^= 0xFF;
        assert!(
            check_policy(&tampered, &TrustPolicy::AllowUnsigned).is_err(),
            "a corrupt signature section must never load, under any policy"
        );
    }

    #[test]
    fn trusting_from_component_onboards_the_verified_key() {
        let dir = std::env::temp_dir().join(format!("tau-test-tofu-{}", std::process::id()));
        let key = SigningKey::generate(&mut rand::rngs::OsRng);
        let signed = sign(&tiny_module(), &key).unwrap();
        // Not trusted yet, then onboarded straight from the component.
        assert!(trust_dir_is_empty(&dir));
        let fps = trust_component_keys_in(&dir, &signed).unwrap();
        assert_eq!(fps, vec![fingerprint(&key.verifying_key())]);
        let policy = TrustPolicy::RequireTrusted {
            trust_dir: dir.clone(),
        };
        check_policy(&signed, &policy).unwrap();
        // Unsigned components have nothing to onboard.
        assert!(matches!(
            trust_component_keys_in(&dir, &tiny_module()),
            Err(SignError::Unsigned)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn trust_dir_is_empty(dir: &Path) -> bool {
        std::fs::read_dir(dir)
            .map(|mut rd| rd.next().is_none())
            .unwrap_or(true)
    }
}
