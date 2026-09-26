//! OCI distribution: pull wasm components from OCI registries by reference
//! (`oci://ghcr.io/org/upper:0.1.0`), cache content-addressed, hand the
//! cached file to the normal load path — signature verification and trust
//! policy apply to pulled bytes exactly as to local files.
//!
//! Hand-rolled minimal registry v2 client (manifest + blob GETs, anonymous
//! bearer-token dance). Pull-only: push is `docker`/`oras`/`crane`'s job
//! for now. Tags are accepted, resolved to a blob digest, and cached by
//! digest; the caller is told when a mutable tag was used.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::sign;

#[derive(Debug, Error)]
pub enum OciError {
    #[error("bad oci reference {0:?} (want oci://registry/repo:tag or oci://registry/repo@sha256:...)")]
    BadReference(String),
    #[error("registry {0}")]
    Registry(String),
    #[error("manifest for {reference}: {reason}")]
    Manifest { reference: String, reason: String },
    #[error("blob digest mismatch: manifest said {expected}, got {actual}")]
    DigestMismatch { expected: String, actual: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// A parsed `oci://registry/repo[:tag|@digest]` reference.
#[derive(Debug, Clone, PartialEq)]
pub struct OciRef {
    pub registry: String,
    pub repo: String,
    /// Tag (mutable) or `sha256:...` digest (immutable).
    pub reference: String,
}

impl OciRef {
    pub fn is_digest(&self) -> bool {
        self.reference.starts_with("sha256:")
    }
}

pub fn parse(reference: &str) -> Result<OciRef, OciError> {
    let rest = reference
        .strip_prefix("oci://")
        .ok_or_else(|| OciError::BadReference(reference.into()))?;
    let (registry, repo_ref) = rest
        .split_once('/')
        .ok_or_else(|| OciError::BadReference(reference.into()))?;
    if registry.is_empty() {
        return Err(OciError::BadReference(reference.into()));
    }
    let (repo, reference) = if let Some((repo, digest)) = repo_ref.split_once('@') {
        (repo, digest)
    } else if let Some((repo, tag)) = repo_ref.rsplit_once(':') {
        (repo, tag)
    } else {
        (repo_ref, "latest")
    };
    if repo.is_empty() || reference.is_empty() {
        return Err(OciError::BadReference(reference.into()));
    }
    Ok(OciRef {
        registry: registry.to_ascii_lowercase(),
        repo: repo.to_string(),
        reference: reference.to_string(),
    })
}

/// What a pull resolved to: the cached blob path and whether the reference
/// was a mutable tag (so the caller can note mutability).
pub struct Pulled {
    pub path: PathBuf,
    pub digest: String,
    pub mutable_tag: bool,
}

pub fn cache_dir() -> PathBuf {
    sign::config_dir().join("oci").join("blobs")
}

/// Pull `reference` into the blob cache and return the cached path.
/// Blocking (reqwest blocking): call from a non-runtime thread.
pub fn pull(reference: &str) -> Result<Pulled, OciError> {
    pull_into(reference, &cache_dir())
}

pub fn pull_into(reference: &str, cache: &Path) -> Result<Pulled, OciError> {
    let oci_ref = parse(reference)?;
    let client = reqwest::blocking::Client::new();
    // Loopback registries (local dev, tests) are plain http; everything
    // else is https-only.
    let scheme = if oci_ref.registry.starts_with("127.0.0.1")
        || oci_ref.registry.starts_with("localhost")
        || oci_ref.registry.starts_with("[::1]")
    {
        "http"
    } else {
        "https"
    };
    let base = format!("{scheme}://{}", oci_ref.registry);

    let manifest = get_json(
        &client,
        &format!(
            "{base}/v2/{}/manifests/{}",
            oci_ref.repo, oci_ref.reference
        ),
        &oci_ref.repo,
        &[
            ("accept",
             "application/vnd.oci.image.manifest.v1+json, \
              application/vnd.docker.distribution.manifest.v2+json"),
        ],
    )
    .map_err(|e| OciError::Manifest {
        reference: reference.into(),
        reason: e.to_string(),
    })?;

    // The wasm layer: prefer a wasm-ish media type, else the single layer.
    let layers = manifest["layers"]
        .as_array()
        .ok_or_else(|| OciError::Registry(format!("no layers in manifest for {reference}")))?;
    let layer = layers
        .iter()
        .find(|l| {
            l["mediaType"]
                .as_str()
                .is_some_and(|mt| mt.contains("wasm"))
        })
        .or(layers.first())
        .ok_or_else(|| OciError::Registry(format!("empty manifest for {reference}")))?;
    let digest = layer["digest"]
        .as_str()
        .ok_or_else(|| OciError::Registry("layer without digest".into()))?
        .to_string();

    let blob_path = cache.join(digest.replace(':', "_"));
    if !blob_path.is_file() {
        let bytes = get_bytes(
            &client,
            &format!("{base}/v2/{}/blobs/{digest}", oci_ref.repo),
            &oci_ref.repo,
        )?;
        let actual = format!("sha256:{}", hex(&Sha256::digest(&bytes)));
        if actual != digest {
            return Err(OciError::DigestMismatch {
                expected: digest,
                actual,
            });
        }
        std::fs::create_dir_all(cache)?;
        std::fs::write(&blob_path, bytes)?;
    }

    Ok(Pulled {
        path: blob_path,
        digest,
        mutable_tag: !oci_ref.is_digest(),
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// GET json, performing the anonymous bearer-token dance on 401.
fn get_json(
    client: &reqwest::blocking::Client,
    url: &str,
    repo: &str,
    headers: &[(&str, &str)],
) -> Result<serde_json::Value, OciError> {
    let bytes = get_with_auth(client, url, repo, headers)?;
    serde_json::from_slice(&bytes).map_err(|e| OciError::Registry(format!("bad json: {e}")))
}

fn get_bytes(
    client: &reqwest::blocking::Client,
    url: &str,
    repo: &str,
) -> Result<Vec<u8>, OciError> {
    get_with_auth(client, url, repo, &[])
}

fn get_with_auth(
    client: &reqwest::blocking::Client,
    url: &str,
    repo: &str,
    headers: &[(&str, &str)],
) -> Result<Vec<u8>, OciError> {
    let send = |token: Option<&str>| -> reqwest::Result<reqwest::blocking::Response> {
        let mut request = client.get(url);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        request.send()
    };
    let response = send(None).map_err(|e| OciError::Registry(format!("{url}: {e}")))?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        let challenge = response
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let token = fetch_token(client, &challenge, repo)?;
        let retry = send(Some(&token)).map_err(|e| OciError::Registry(format!("{url}: {e}")))?;
        return read_ok(retry, url);
    }
    read_ok(response, url)
}

fn read_ok(response: reqwest::blocking::Response, url: &str) -> Result<Vec<u8>, OciError> {
    if !response.status().is_success() {
        return Err(OciError::Registry(format!(
            "{url}: http {}",
            response.status()
        )));
    }
    response
        .bytes()
        .map(|b| b.to_vec())
        .map_err(|e| OciError::Registry(format!("{url}: {e}")))
}

/// Anonymous bearer token: parse the WWW-Authenticate challenge and hit the
/// realm with service+scope.
fn fetch_token(
    client: &reqwest::blocking::Client,
    challenge: &str,
    repo: &str,
) -> Result<String, OciError> {
    let realm = challenge_param(challenge, "realm")
        .ok_or_else(|| OciError::Registry(format!("no realm in challenge: {challenge}")))?;
    let service = challenge_param(challenge, "service");
    let scope = challenge_param(challenge, "scope")
        .unwrap_or_else(|| format!("repository:{repo}:pull"));
    let mut url = format!("{realm}?scope={scope}");
    if let Some(service) = service {
        url.push_str(&format!("&service={service}"));
    }
    let body: serde_json::Value = client
        .get(&url)
        .send()
        .and_then(|r| r.bytes())
        .map_err(|e| OciError::Registry(format!("token fetch {realm}: {e}")))
        .and_then(|b| {
            serde_json::from_slice(&b)
                .map_err(|e| OciError::Registry(format!("bad token json: {e}")))
        })?;
    body["token"]
        .as_str()
        .or_else(|| body["access_token"].as_str())
        .map(str::to_string)
        .ok_or_else(|| OciError::Registry("token response without token".into()))
}

/// Parse one `key="value"` out of a WWW-Authenticate challenge.
fn challenge_param(challenge: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=\"");
    let start = challenge.find(&needle)? + needle.len();
    let end = challenge[start..].find('"')? + start;
    Some(challenge[start..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_references() {
        assert_eq!(
            parse("oci://ghcr.io/org/upper:0.1.0").unwrap(),
            OciRef {
                registry: "ghcr.io".into(),
                repo: "org/upper".into(),
                reference: "0.1.0".into(),
            }
        );
        assert_eq!(
            parse("oci://registry.example.com:5000/deep/nested/comp@sha256:abc").unwrap(),
            OciRef {
                registry: "registry.example.com:5000".into(),
                repo: "deep/nested/comp".into(),
                reference: "sha256:abc".into(),
            }
        );
        assert_eq!(parse("oci://ghcr.io/org/upper").unwrap().reference, "latest");
        assert!(parse("oci://ghcr.io/org/upper:0.1.0")
            .map(|r| !r.is_digest())
            .unwrap());
        assert!(parse("not-oci").is_err());
        assert!(parse("oci://norepo").is_err());
        assert!(parse("oci:///repo:tag").is_err());
    }

    #[test]
    fn parses_auth_challenges() {
        let challenge = r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:org/upper:pull""#;
        assert_eq!(
            challenge_param(challenge, "realm"),
            Some("https://ghcr.io/token".into())
        );
        assert_eq!(challenge_param(challenge, "service"), Some("ghcr.io".into()));
        assert_eq!(challenge_param(challenge, "missing"), None);
    }
}
