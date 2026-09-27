//! Remembered capability consent: grants recorded per component fingerprint
//! so a trusted component does not re-ask every run.
//!
//! Layout: `~/.tau/consent/<fingerprint>.json`, one file per fingerprint —
//! same shape as the trust store. The store is consulted by the CLI's
//! composition layer (never inside the host): explicit flags win per field,
//! remembered grants fill the gaps, `--remember` persists the merged result.
//! Unsigned components have no fingerprint and can never be remembered —
//! that is the signing → consent chain working as intended.

use std::collections::HashSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::bridge::BridgeConsent;
use crate::sign::{self, SignError, fingerprint_shaped};

/// Per-fingerprint capability grants persisted under
/// `~/.tau/consent/`. Secrets are never stored — only the grant to
/// deliver them.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RememberedConsent {
    /// The argv this component's bridge may spawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// The MCP endpoint URL this component's bridge may use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_url: Option<String>,
    /// HTTP origins this component may reach.
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    pub origins: HashSet<String>,
    /// Grant to deliver the provider credential (`TAU_PROVIDER_AUTH`)
    /// into this component's memory. The grant is remembered; the secret
    /// never is — it is re-given via the environment each run.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auth_delivery: bool,
    /// Remembered WASI sandbox: this component loads under
    /// [`crate::WasiPolicy::DenyAll`] even without `--deny-wasi`. Sticky —
    /// the only way back is `tau consent --revoke`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub wasi_deny: bool,
    /// Session injection: this component may push messages into the
    /// session (`host.steer` / `host.follow-up`). Sticky like wasi_deny.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub inject: bool,
    /// Webhook ingress: the `addr:port` list this bridge may listen on
    /// (docs/im-channels.md). Unions on merge like origins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ingress: Vec<String>,
    /// Microphone: this realtime provider may drive HOST-side mic
    /// capture (docs/realtime-av.md — the category guards the DEVICE,
    /// not the session; synthetic uplink like `/live N sine` needs no
    /// grant). Sticky like wasi_deny.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub microphone: bool,
    /// Camera: registered with the taxonomy but admits no capture path
    /// yet — always absent in practice (no UX is invented for a path
    /// that does not exist).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub camera: bool,
}

impl RememberedConsent {
    /// Whether no capability is granted (nothing worth persisting).
    pub fn is_empty(&self) -> bool {
        self.command.is_none()
            && self.mcp_url.is_none()
            && self.origins.is_empty()
            && !self.auth_delivery
            && !self.wasi_deny
            && !self.inject
            && self.ingress.is_empty()
            && !self.microphone
            && !self.camera
    }
}

impl From<BridgeConsent> for RememberedConsent {
    fn from(consent: BridgeConsent) -> Self {
        Self {
            command: consent.command,
            mcp_url: consent.mcp_url,
            origins: consent.origins,
            auth_delivery: false,
            wasi_deny: false,
            inject: consent.inject,
            ingress: consent.ingress,
            // Bridge consent carries no device categories (realtime-av
            // guards provider-driven capture, not bridges).
            microphone: false,
            camera: false,
        }
    }
}

impl From<RememberedConsent> for BridgeConsent {
    fn from(remembered: RememberedConsent) -> Self {
        Self {
            command: remembered.command,
            mcp_url: remembered.mcp_url,
            origins: remembered.origins,
            inject: remembered.inject,
            ingress: remembered.ingress,
        }
    }
}

/// Merge a run's grants into the persisted record: present fields win,
/// absent fields keep what was stored, origins union, boolean grants are
/// sticky-on (`--remember` never revokes — that is what `--revoke` is for).
pub fn remember_into(existing: RememberedConsent, grant: RememberedConsent) -> RememberedConsent {
    let mut origins = existing.origins;
    origins.extend(grant.origins);
    let mut ingress = existing.ingress;
    for addr in grant.ingress {
        if !ingress.contains(&addr) {
            ingress.push(addr);
        }
    }
    RememberedConsent {
        command: grant.command.or(existing.command),
        mcp_url: grant.mcp_url.or(existing.mcp_url),
        origins,
        auth_delivery: existing.auth_delivery || grant.auth_delivery,
        wasi_deny: existing.wasi_deny || grant.wasi_deny,
        inject: existing.inject || grant.inject,
        ingress,
        microphone: existing.microphone || grant.microphone,
        camera: existing.camera || grant.camera,
    }
}

// Every store method checks the key's shape (see
// [`sign::fingerprint_shaped`]): the filename is built from the
// fingerprint, and a caller-supplied value (`tau consent --revoke
// <arg>`) must never become a path — `../x` would escape the store
// directory.
fn bad_fingerprint(fingerprint: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("{fingerprint:?} is not a signing fingerprint (16 lowercase hex chars)"),
    )
}

/// Filesystem-backed store of [`RememberedConsent`] records, one
/// `<fingerprint>.json` per file.
pub struct ConsentStore {
    dir: PathBuf,
}

impl ConsentStore {
    /// A store rooted at `dir` (created on first [`save`](Self::save)).
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// The remembered grants for `fingerprint`, if any (a corrupt
    /// file reads as absent, never as an error).
    pub fn load(&self, fingerprint: &str) -> Option<RememberedConsent> {
        if !fingerprint_shaped(fingerprint) {
            return None;
        }
        let bytes = std::fs::read(self.dir.join(format!("{fingerprint}.json"))).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Persist `consent` for `fingerprint` (overwrites).
    pub fn save(&self, fingerprint: &str, consent: &RememberedConsent) -> std::io::Result<()> {
        if !fingerprint_shaped(fingerprint) {
            return Err(bad_fingerprint(fingerprint));
        }
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(
            self.dir.join(format!("{fingerprint}.json")),
            serde_json::to_string_pretty(consent).expect("consent serializes"),
        )
    }

    /// Delete the record for `fingerprint`; `false` when none existed.
    pub fn revoke(&self, fingerprint: &str) -> std::io::Result<bool> {
        if !fingerprint_shaped(fingerprint) {
            return Err(bad_fingerprint(fingerprint));
        }
        match std::fs::remove_file(self.dir.join(format!("{fingerprint}.json"))) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Fingerprints with remembered grants, sorted.
    pub fn list(&self) -> Vec<String> {
        let mut fps: Vec<String> = std::fs::read_dir(&self.dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter_map(|e| {
                        e.file_name()
                            .to_string_lossy()
                            .strip_suffix(".json")
                            .map(str::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default();
        fps.sort();
        fps
    }
}

impl Default for ConsentStore {
    fn default() -> Self {
        Self::new(crate::sign::config_dir().join("consent"))
    }
}

/// Fingerprints of every key that signed this component, in section order.
/// Unsigned components get an empty vec — and with it, no remembered consent.
pub fn component_fingerprints(wasm: &[u8]) -> Result<Vec<String>, SignError> {
    Ok(sign::verify(wasm)?.iter().map(sign::fingerprint).collect())
}

/// Merge explicit consent over remembered grants: explicit wins per field,
/// origins union.
pub fn merge(explicit: BridgeConsent, remembered: RememberedConsent) -> BridgeConsent {
    let mut origins = remembered.origins;
    origins.extend(explicit.origins);
    let mut ingress = remembered.ingress;
    for addr in explicit.ingress {
        if !ingress.contains(&addr) {
            ingress.push(addr);
        }
    }
    BridgeConsent {
        command: explicit.command.or(remembered.command),
        mcp_url: explicit.mcp_url.or(remembered.mcp_url),
        origins,
        // Sticky-on like remember_into: a remembered grant is never
        // lifted by omitting the flag (that is what --revoke is for).
        inject: explicit.inject || remembered.inject,
        ingress,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (PathBuf, ConsentStore) {
        let dir = std::env::temp_dir().join(format!(
            "tau-test-consent-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        (dir.clone(), ConsentStore::new(dir))
    }

    #[test]
    fn save_load_revoke_round_trip() {
        let (dir, store) = store();
        let consent = RememberedConsent {
            command: Some(vec!["python".into(), "server.py".into()]),
            mcp_url: None,
            origins: ["https://api.example.com".into()].into_iter().collect(),
            auth_delivery: true,
            wasi_deny: false,
            inject: true,
            ingress: Vec::new(),
            microphone: false,
            camera: false,
        };
        store.save("abc123abc123abc1", &consent).unwrap();
        assert_eq!(store.load("abc123abc123abc1"), Some(consent));
        assert_eq!(store.list(), vec!["abc123abc123abc1".to_string()]);
        assert!(store.revoke("abc123abc123abc1").unwrap());
        assert!(!store.revoke("abc123abc123abc1").unwrap());
        assert_eq!(store.load("abc123abc123abc1"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn files_without_capability_grants_still_load() {
        // Written before auth_delivery/wasi_deny existed: serde defaults
        // fill them, and the record reads as transport-consent-only.
        let (dir, store) = store();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("0dd0dd0dd0dd0dd0.json"),
            r#"{"command":["python","server.py"],"origins":["https://a.example"]}"#,
        )
        .unwrap();
        let loaded = store.load("0dd0dd0dd0dd0dd0").unwrap();
        assert!(!loaded.auth_delivery);
        assert!(!loaded.wasi_deny);
        assert!(loaded.command.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_files_read_as_absent_and_weird_keys_never_escape_the_dir() {
        let (dir, store) = store();
        std::fs::create_dir_all(&dir).unwrap();
        // Corrupt JSON: absent, never an error (fail-closed).
        std::fs::write(dir.join("aaaaaaaaaaaaaaaa.json"), b"{ not json").unwrap();
        assert_eq!(store.load("aaaaaaaaaaaaaaaa"), None);
        // An empty object loads as a grant-nothing record.
        std::fs::write(dir.join("bbbbbbbbbbbbbbbb.json"), b"{}").unwrap();
        assert_eq!(
            store.load("bbbbbbbbbbbbbbbb"),
            Some(RememberedConsent::default())
        );
        // A caller-supplied "fingerprint" must never become a path:
        // create a sibling file the traversal would delete if it worked.
        let sibling = dir
            .parent()
            .unwrap()
            .join(format!("escape-{}.json", std::process::id()));
        std::fs::write(&sibling, b"x").unwrap();
        let traversal = "../escape";
        assert_eq!(store.load(traversal), None);
        assert!(
            store
                .save(traversal, &RememberedConsent::default())
                .is_err()
        );
        assert!(store.revoke(traversal).is_err());
        // Uppercase hex is not a fingerprint either (the store is
        // canonical lowercase — accepting both would split identity).
        assert!(
            store
                .save("AAAAAAAAAAAAAAAA", &RememberedConsent::default())
                .is_err()
        );
        // Nothing outside the store dir was touched.
        assert!(sibling.exists());
        let _ = std::fs::remove_file(&sibling);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remember_into_is_sticky_and_keeps_absent_fields() {
        let existing = RememberedConsent {
            command: Some(vec!["old".into()]),
            mcp_url: None,
            origins: ["https://a.example".into()].into_iter().collect(),
            auth_delivery: true,
            wasi_deny: false,
            inject: false,
            ingress: Vec::new(),
            microphone: false,
            camera: false,
        };
        let grant = RememberedConsent {
            command: None,
            mcp_url: Some("https://b.example/mcp".into()),
            origins: ["https://b.example".into()].into_iter().collect(),
            auth_delivery: false,
            wasi_deny: true,
            inject: true,
            ingress: Vec::new(),
            microphone: false,
            camera: false,
        };
        let merged = remember_into(existing, grant);
        assert_eq!(merged.command, Some(vec!["old".into()]));
        assert_eq!(merged.mcp_url, Some("https://b.example/mcp".into()));
        assert!(merged.origins.contains("https://a.example"));
        assert!(merged.origins.contains("https://b.example"));
        // Sticky-on in both directions: a run without the grant never
        // erases it, a run with it never loses what was stored.
        assert!(merged.auth_delivery);
        assert!(merged.wasi_deny);
        // Sticky-on: the grant's inject survives even though the existing
        // record lacks it.
        assert!(merged.inject);
    }

    #[test]
    fn explicit_wins_per_field_origins_union() {
        let explicit = BridgeConsent {
            command: Some(vec!["explicit".into()]),
            mcp_url: None,
            origins: ["https://b.example".into()].into_iter().collect(),
            inject: false,
            ingress: Vec::new(),
        };
        let remembered = RememberedConsent {
            command: Some(vec!["remembered".into()]),
            mcp_url: Some("https://a.example/mcp".into()),
            origins: ["https://a.example".into()].into_iter().collect(),
            auth_delivery: false,
            wasi_deny: false,
            inject: false,
            ingress: Vec::new(),
            microphone: false,
            camera: false,
        };
        let merged = merge(explicit, remembered);
        assert_eq!(merged.command, Some(vec!["explicit".into()]));
        assert_eq!(merged.mcp_url, Some("https://a.example/mcp".into()));
        assert!(merged.origins.contains("https://a.example"));
        assert!(merged.origins.contains("https://b.example"));
    }

    #[test]
    fn ingress_unions_and_round_trips() {
        // BridgeConsent -> RememberedConsent -> BridgeConsent keeps the
        // listen addresses; merge unions explicit over remembered
        // without duplicates (a repeated --ingress is idempotent).
        let consent = BridgeConsent {
            ingress: vec!["127.0.0.1:8080".into()],
            ..BridgeConsent::default()
        };
        let remembered = RememberedConsent::from(consent);
        assert_eq!(remembered.ingress, vec!["127.0.0.1:8080".to_string()]);
        assert!(!remembered.is_empty());
        let back = BridgeConsent::from(remembered.clone());
        assert_eq!(back.ingress, vec!["127.0.0.1:8080".to_string()]);

        let explicit = BridgeConsent {
            ingress: vec!["127.0.0.1:8080".into(), "127.0.0.1:9090".into()],
            ..BridgeConsent::default()
        };
        let merged = merge(explicit, remembered);
        assert_eq!(
            merged.ingress,
            vec!["127.0.0.1:8080".to_string(), "127.0.0.1:9090".to_string()]
        );

        // Serde back-compat: a record written before the field existed
        // reads with an empty list.
        let legacy: RememberedConsent =
            serde_json::from_str(r#"{"command":null,"mcp_url":null,"origins":[],"auth_delivery":false,"wasi_deny":false,"inject":false}"#)
                .unwrap();
        assert!(legacy.ingress.is_empty());
        assert!(legacy.is_empty());
    }

    #[test]
    fn merge_inject_is_sticky_on() {
        // The remembered grant survives a run without the flag (lifting
        // it is --revoke's job); the explicit flag grants over nothing.
        let remembered = RememberedConsent {
            inject: true,
            ..Default::default()
        };
        assert!(merge(BridgeConsent::default(), remembered).inject);
        assert!(
            merge(
                BridgeConsent {
                    inject: true,
                    ..Default::default()
                },
                RememberedConsent::default()
            )
            .inject
        );
        assert!(!merge(BridgeConsent::default(), RememberedConsent::default()).inject);
    }

    #[test]
    fn bridge_consent_round_trips_inject_through_remembered() {
        let consent = BridgeConsent {
            inject: true,
            ..Default::default()
        };
        let remembered = RememberedConsent::from(consent);
        assert!(remembered.inject);
        assert!(!remembered.is_empty());
        assert!(BridgeConsent::from(remembered).inject);
    }
}
