//! OCI distribution: pull wasm components from OCI registries by reference
//! (`oci://ghcr.io/org/upper:0.1.0`), cache content-addressed, hand the
//! cached file to the normal load path — signature verification and trust
//! policy apply to pulled bytes exactly as to local files. Push (`tau
//! push`) uploads a component the same minimal way: monolithic blob PUTs
//! plus a manifest PUT.
//!
//! Hand-rolled minimal registry v2 client (manifest + blob GETs/PUTs,
//! bearer-token dance; optional basic credentials from
//! `TAU_REGISTRY_USER`/`TAU_REGISTRY_PASSWORD` for the token request, which
//! is what ghcr-style registries require). Pull tags are accepted,
//! resolved to a blob digest, and cached by digest; the caller is told
//! when a mutable tag was used.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::sign;

/// Media type of the wasm layer, wasm-to-oci convention (pull matches any
/// media type containing "wasm").
const WASM_LAYER_MEDIA_TYPE: &str = "application/vnd.wasm.content.layer.v1+wasm";
const WASM_CONFIG_MEDIA_TYPE: &str = "application/vnd.wasm.config.v1+json";
const MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

/// Failures pulling from or pushing to an OCI registry.
#[derive(Debug, Error)]
pub enum OciError {
    /// The `oci://` reference did not parse.
    #[error(
        "bad oci reference {0:?} (want oci://registry/repo:tag or oci://registry/repo@sha256:...)"
    )]
    BadReference(String),
    /// The registry rejected a request (HTTP error, auth, network).
    #[error("registry {0}")]
    Registry(String),
    /// The manifest was unusable.
    #[error("manifest for {reference}: {reason}")]
    Manifest {
        /// The reference being resolved.
        reference: String,
        /// Why the manifest was rejected.
        reason: String,
    },
    /// A downloaded blob did not match its manifest digest.
    #[error("blob digest mismatch: manifest said {expected}, got {actual}")]
    DigestMismatch {
        /// The digest the manifest declared.
        expected: String,
        /// The digest of the bytes actually received.
        actual: String,
    },
    /// Filesystem failure (cache read/write).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// A parsed `oci://registry/repo[:tag|@digest]` reference.
#[derive(Debug, Clone, PartialEq)]
pub struct OciRef {
    /// Registry host[:port].
    pub registry: String,
    /// Repository path within the registry.
    pub repo: String,
    /// Tag (mutable) or `sha256:...` digest (immutable).
    pub reference: String,
}

impl OciRef {
    /// Whether the reference pins a digest (immutable) vs. a tag.
    pub fn is_digest(&self) -> bool {
        self.reference.starts_with("sha256:")
    }
}

/// Parse an `oci://registry/repo[:tag|@sha256:...]` reference.
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

/// One registry session: base URL, repo scope, and a lazily-fetched
/// bearer token (cached per instance; the dance re-runs on a fresh 401).
struct Registry {
    client: reqwest::blocking::Client,
    base: String,
    repo: String,
    token: Option<String>,
}

/// What came back from one request: Location (for upload sessions) and
/// the body bytes. The status was already checked against 2xx/202.
struct Reply {
    location: Option<String>,
    body: Vec<u8>,
}

impl Registry {
    fn new(oci_ref: &OciRef) -> Self {
        // Loopback registries (local dev, tests) are plain http;
        // everything else is https-only.
        let scheme = if oci_ref.registry.starts_with("127.0.0.1")
            || oci_ref.registry.starts_with("localhost")
            || oci_ref.registry.starts_with("[::1]")
        {
            "http"
        } else {
            "https"
        };
        Self {
            client: reqwest::blocking::Client::new(),
            base: format!("{scheme}://{}", oci_ref.registry),
            repo: oci_ref.repo.clone(),
            token: None,
        }
    }

    fn get(&mut self, url: &str, headers: &[(&str, &str)]) -> Result<Reply, OciError> {
        self.send(reqwest::Method::GET, url, headers, None)
    }

    fn post(&mut self, url: &str) -> Result<Reply, OciError> {
        self.send(reqwest::Method::POST, url, &[], None)
    }

    fn put(&mut self, url: &str, body: Vec<u8>, content_type: &str) -> Result<Reply, OciError> {
        self.send(
            reqwest::Method::PUT,
            url,
            &[("content-type", content_type)],
            Some(body),
        )
    }

    fn get_json(
        &mut self,
        url: &str,
        headers: &[(&str, &str)],
    ) -> Result<serde_json::Value, OciError> {
        let reply = self.get(url, headers)?;
        serde_json::from_slice(&reply.body)
            .map_err(|e| OciError::Registry(format!("bad json: {e}")))
    }

    /// One request with the bearer-token dance: anonymous first; on 401
    /// parse the challenge, fetch a token, retry once. Non-2xx/202 comes
    /// back as an error string naming the URL.
    fn send(
        &mut self,
        method: reqwest::Method,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<Vec<u8>>,
    ) -> Result<Reply, OciError> {
        let attempt = |token: Option<&str>| -> reqwest::Result<reqwest::blocking::Response> {
            let mut request = self.client.request(method.clone(), url);
            for (name, value) in headers {
                request = request.header(*name, *value);
            }
            if let Some(token) = token {
                request = request.bearer_auth(token);
            }
            if let Some(body) = &body {
                request = request.body(body.clone());
            }
            request.send()
        };
        let mut response = attempt(self.token.as_deref())
            .map_err(|e| OciError::Registry(format!("{url}: {e}")))?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            let challenge = response
                .headers()
                .get("www-authenticate")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            self.token = Some(self.fetch_token(&challenge)?);
            response = attempt(self.token.as_deref())
                .map_err(|e| OciError::Registry(format!("{url}: {e}")))?;
        }
        let status = response.status();
        if !(status.is_success() || status == reqwest::StatusCode::ACCEPTED) {
            return Err(OciError::Registry(format!("{url}: http {status}")));
        }
        let location = response
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = response
            .bytes()
            .map(|b| b.to_vec())
            .map_err(|e| OciError::Registry(format!("{url}: {e}")))?;
        Ok(Reply { location, body })
    }

    /// Bearer token from the challenge's realm. The challenge's scope wins;
    /// the fallback covers registries that omit it. Basic credentials from
    /// `TAU_REGISTRY_USER`/`TAU_REGISTRY_PASSWORD`, when set, authenticate
    /// the token request itself (ghcr-style).
    fn fetch_token(&self, challenge: &str) -> Result<String, OciError> {
        let realm = challenge_param(challenge, "realm")
            .ok_or_else(|| OciError::Registry(format!("no realm in challenge: {challenge}")))?;
        let service = challenge_param(challenge, "service");
        let scope = challenge_param(challenge, "scope")
            .unwrap_or_else(|| format!("repository:{}:pull,push", self.repo));
        let mut url = format!("{realm}?scope={scope}");
        if let Some(service) = service {
            url.push_str(&format!("&service={service}"));
        }
        let mut request = self.client.get(&url);
        if let (Ok(user), Ok(password)) = (
            std::env::var("TAU_REGISTRY_USER"),
            std::env::var("TAU_REGISTRY_PASSWORD"),
        ) {
            request = request.basic_auth(user, Some(password));
        }
        let body: serde_json::Value = request
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
}

/// What a pull resolved to: the cached blob path and whether the reference
/// was a mutable tag (so the caller can note mutability).
pub struct Pulled {
    /// Local path of the cached component blob.
    pub path: PathBuf,
    /// The `sha256:...` digest the reference resolved to.
    pub digest: String,
    /// Whether the reference was a tag (mutable — may change upstream).
    pub mutable_tag: bool,
}

/// Where pulled blobs are cached (`~/.tau/oci/blobs`).
pub fn cache_dir() -> PathBuf {
    sign::config_dir().join("oci").join("blobs")
}

/// Pull `reference` into the blob cache and return the cached path.
/// Blocking (reqwest blocking): call from a non-runtime thread.
pub fn pull(reference: &str) -> Result<Pulled, OciError> {
    pull_into(reference, &cache_dir())
}

/// [`pull`] with an explicit cache directory (tests, air-gapped use).
pub fn pull_into(reference: &str, cache: &Path) -> Result<Pulled, OciError> {
    let oci_ref = parse(reference)?;
    let mut registry = Registry::new(&oci_ref);
    let base = registry.base.clone();

    let manifest = registry
        .get_json(
            &format!("{base}/v2/{}/manifests/{}", oci_ref.repo, oci_ref.reference),
            &[(
                "accept",
                "application/vnd.oci.image.manifest.v1+json, \
                  application/vnd.docker.distribution.manifest.v2+json",
            )],
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
    // The cache is content-addressed: a hit must still verify, or a torn
    // write / disk rot / tampering would hand bad bytes to the load path
    // (which would fail loudly but leave the user deleting the cache by
    // hand). A mismatch self-heals by re-pulling.
    let cached_ok = blob_path.is_file() && digest_of(&blob_path)? == digest;
    if !cached_ok {
        if blob_path.is_file() {
            std::fs::remove_file(&blob_path)?;
        }
        let reply = registry.get(&format!("{base}/v2/{}/blobs/{digest}", oci_ref.repo), &[])?;
        let bytes = reply.body;
        let actual = format!("sha256:{}", hex(&Sha256::digest(&bytes)));
        if actual != digest {
            return Err(OciError::DigestMismatch {
                expected: digest,
                actual,
            });
        }
        std::fs::create_dir_all(cache)?;
        // Atomic write (temp file + rename): a crash mid-write tears the
        // temp file, never the content-addressed cache entry.
        let tmp = cache.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            digest.replace(':', "-")
        ));
        std::fs::write(&tmp, &bytes)?;
        if let Err(e) = std::fs::rename(&tmp, &blob_path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
    }

    Ok(Pulled {
        path: blob_path,
        digest,
        mutable_tag: !oci_ref.is_digest(),
    })
}

/// What a push uploaded: the wasm blob's digest, under the given
/// (mutable) tag.
pub struct Pushed {
    /// The `sha256:...` digest of the uploaded wasm blob.
    pub digest: String,
    /// The full reference the component is now reachable under.
    pub reference: String,
}

/// Push the component at `wasm` to `reference` (must name a tag, not a
/// digest): monolithic blob PUTs (config + layer) then the manifest PUT.
/// Blocking (reqwest blocking): call from a non-runtime thread.
pub fn push(reference: &str, wasm: &Path) -> Result<Pushed, OciError> {
    let oci_ref = parse(reference)?;
    if oci_ref.is_digest() {
        return Err(OciError::BadReference(format!(
            "{reference}: push needs a tag, not a digest"
        )));
    }
    let mut registry = Registry::new(&oci_ref);
    let base = registry.base.clone();
    let repo = &oci_ref.repo;

    let upload = |registry: &mut Registry, bytes: &[u8]| -> Result<String, OciError> {
        let digest = format!("sha256:{}", hex(&Sha256::digest(bytes)));
        let session = registry.post(&format!("{base}/v2/{repo}/blobs/uploads/"))?;
        let location = session
            .location
            .ok_or_else(|| OciError::Registry("upload session without Location".into()))?;
        // Location may be absolute, or a root-relative path.
        let url = if location.starts_with("http") {
            location
        } else {
            format!("{base}/{}", location.trim_start_matches('/'))
        };
        let sep = if url.contains('?') { '&' } else { '?' };
        registry.put(
            &format!("{url}{sep}digest={digest}"),
            bytes.to_vec(),
            "application/octet-stream",
        )?;
        Ok(digest)
    };

    let config = b"{}".as_slice();
    let config_digest = upload(&mut registry, config)?;
    let wasm_bytes = std::fs::read(wasm)?;
    let layer_digest = upload(&mut registry, &wasm_bytes)?;

    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": MANIFEST_MEDIA_TYPE,
        "config": {
            "mediaType": WASM_CONFIG_MEDIA_TYPE,
            "digest": config_digest,
            "size": config.len(),
        },
        "layers": [{
            "mediaType": WASM_LAYER_MEDIA_TYPE,
            "digest": layer_digest,
            "size": wasm_bytes.len(),
        }],
    });
    registry.put(
        &format!("{base}/v2/{repo}/manifests/{}", oci_ref.reference),
        serde_json::to_vec(&manifest)
            .map_err(|e| OciError::Registry(format!("manifest json: {e}")))?,
        MANIFEST_MEDIA_TYPE,
    )?;

    Ok(Pushed {
        digest: layer_digest,
        reference: reference.to_string(),
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The `sha256:<hex>` of a cached file's current bytes.
fn digest_of(path: &Path) -> Result<String, OciError> {
    let bytes = std::fs::read(path)?;
    Ok(format!("sha256:{}", hex(&Sha256::digest(&bytes))))
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
        assert_eq!(
            parse("oci://ghcr.io/org/upper").unwrap().reference,
            "latest"
        );
        assert!(
            parse("oci://ghcr.io/org/upper:0.1.0")
                .map(|r| !r.is_digest())
                .unwrap()
        );
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
        assert_eq!(
            challenge_param(challenge, "service"),
            Some("ghcr.io".into())
        );
        assert_eq!(challenge_param(challenge, "missing"), None);
    }
}
