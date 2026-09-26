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
use crate::sign::{self, SignError};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RememberedConsent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_url: Option<String>,
    #[serde(default)]
    pub origins: HashSet<String>,
}

impl RememberedConsent {
    pub fn is_empty(&self) -> bool {
        self.command.is_none() && self.mcp_url.is_none() && self.origins.is_empty()
    }
}

impl From<BridgeConsent> for RememberedConsent {
    fn from(consent: BridgeConsent) -> Self {
        Self {
            command: consent.command,
            mcp_url: consent.mcp_url,
            origins: consent.origins,
        }
    }
}

impl From<RememberedConsent> for BridgeConsent {
    fn from(remembered: RememberedConsent) -> Self {
        Self {
            command: remembered.command,
            mcp_url: remembered.mcp_url,
            origins: remembered.origins,
        }
    }
}

pub struct ConsentStore {
    dir: PathBuf,
}

impl ConsentStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn load(&self, fingerprint: &str) -> Option<RememberedConsent> {
        let bytes = std::fs::read(self.dir.join(format!("{fingerprint}.json"))).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn save(&self, fingerprint: &str, consent: &RememberedConsent) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(
            self.dir.join(format!("{fingerprint}.json")),
            serde_json::to_string_pretty(consent).expect("consent serializes"),
        )
    }

    pub fn revoke(&self, fingerprint: &str) -> std::io::Result<bool> {
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
    BridgeConsent {
        command: explicit.command.or(remembered.command),
        mcp_url: explicit.mcp_url.or(remembered.mcp_url),
        origins,
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
        };
        store.save("abc123", &consent).unwrap();
        assert_eq!(store.load("abc123"), Some(consent));
        assert_eq!(store.list(), vec!["abc123".to_string()]);
        assert!(store.revoke("abc123").unwrap());
        assert!(!store.revoke("abc123").unwrap());
        assert_eq!(store.load("abc123"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explicit_wins_per_field_origins_union() {
        let explicit = BridgeConsent {
            command: Some(vec!["explicit".into()]),
            mcp_url: None,
            origins: ["https://b.example".into()].into_iter().collect(),
        };
        let remembered = RememberedConsent {
            command: Some(vec!["remembered".into()]),
            mcp_url: Some("https://a.example/mcp".into()),
            origins: ["https://a.example".into()].into_iter().collect(),
        };
        let merged = merge(explicit, remembered);
        assert_eq!(merged.command, Some(vec!["explicit".into()]));
        assert_eq!(merged.mcp_url, Some("https://a.example/mcp".into()));
        assert!(merged.origins.contains("https://a.example"));
        assert!(merged.origins.contains("https://b.example"));
    }
}
