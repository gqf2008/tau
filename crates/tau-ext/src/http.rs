//! Origin-allowlisted HTTP capability shared by the bridge and provider
//! worlds. The registry is granted a set of "scheme://host[:port]"
//! origins by explicit user consent; every request's origin is checked
//! before sending, redirects are never followed (a redirect would escape
//! consent), and response bodies stream through a reader thread so the
//! wasm engine never blocks on a raw socket. Granted empty, every call
//! fails permission-denied at call time, not instantiation time.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;

/// One in-flight HTTP response: headers already received, body drained by
/// a reader thread into a channel — same shape as bridge ChildProcess, so
/// SSE streams can be consumed incrementally and closed early.
pub(crate) struct HttpResponse {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    rx: std::sync::mpsc::Receiver<Result<Vec<u8>, String>>,
    pending: VecDeque<u8>,
    eof: bool,
}

#[derive(Default)]
pub(crate) struct HttpRegistry {
    next: u64,
    responses: HashMap<u64, HttpResponse>,
    /// Consented origins: "scheme://host[:port]". Empty = deny all.
    origins: HashSet<String>,
}

impl HttpRegistry {
    pub(crate) fn new(origins: HashSet<String>) -> Self {
        Self {
            origins,
            ..Self::default()
        }
    }

    /// Extract the consent origin ("scheme://host[:port]") from an http(s)
    /// URL; strips userinfo and any path.
    pub(crate) fn origin_of(url: &str) -> Option<String> {
        let (scheme, rest) = url.split_once("://")?;
        if scheme != "http" && scheme != "https" {
            return None;
        }
        let authority = rest.split('/').next()?;
        // Strip any userinfo; host[:port] is what consent covers.
        let host_port = authority.rsplit('@').next()?;
        if host_port.is_empty() {
            return None;
        }
        Some(format!("{scheme}://{}", host_port.to_ascii_lowercase()))
    }

    pub(crate) fn request(
        &mut self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<u64, String> {
        let origin = Self::origin_of(url).ok_or_else(|| format!("bad url: {url}"))?;
        if !self.origins.contains(&origin) {
            return Err(format!(
                "http: origin {origin} not in consent allowlist ({} granted)",
                self.origins.len()
            ));
        }
        // Redirects are never followed: a redirect would silently move the
        // request to an origin the user did not consent to.
        let client = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|e| format!("bad method {method}: {e}"))?;
        let mut request = client.request(method, url).body(body.to_vec());
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let mut response = request.send().map_err(|e| format!("http {url}: {e}"))?;
        let status = response.status().as_u16();
        let response_headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(n, v)| {
                (
                    n.as_str().to_string(),
                    v.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match response.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(Ok(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(format!("read body: {e}")));
                        break;
                    }
                }
            }
        });
        let handle = self.next;
        self.next += 1;
        self.responses.insert(
            handle,
            HttpResponse {
                status,
                headers: response_headers,
                rx,
                pending: VecDeque::new(),
                eof: false,
            },
        );
        Ok(handle)
    }

    fn get(&mut self, handle: u64) -> Result<&mut HttpResponse, String> {
        self.responses
            .get_mut(&handle)
            .ok_or_else(|| format!("unknown http handle {handle}"))
    }

    pub(crate) fn status(&mut self, handle: u64) -> Result<u16, String> {
        Ok(self.get(handle)?.status)
    }

    pub(crate) fn header(&mut self, handle: u64, name: &str) -> Result<Option<String>, String> {
        let response = self.get(handle)?;
        let name = name.to_ascii_lowercase();
        Ok(response
            .headers
            .iter()
            .find(|(n, _)| n.to_ascii_lowercase() == name)
            .map(|(_, v)| v.clone()))
    }

    /// Block only until SOMETHING is available, then return immediately:
    /// waiting to fill `max` would deadlock any peer that sends a short
    /// message and then waits for a reply (SSE streams included).
    pub(crate) fn read_body(&mut self, handle: u64, max: u32) -> Result<(Vec<u8>, bool), String> {
        let response = self.get(handle)?;
        let max = max.max(1) as usize;
        if response.pending.is_empty() && !response.eof {
            match response.rx.recv() {
                Ok(Ok(chunk)) => response.pending.extend(chunk),
                Ok(Err(e)) => return Err(e),
                Err(_) => response.eof = true,
            }
        }
        let take = response.pending.len().min(max);
        let bytes: Vec<u8> = response.pending.drain(..take).collect();
        Ok((bytes, response.eof && response.pending.is_empty()))
    }

    pub(crate) fn close(&mut self, handle: u64) {
        self.responses.remove(&handle);
    }
}
