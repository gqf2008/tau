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
    ///
    /// The authority ends at the first `/`, `?`, `#`, OR `\` — reqwest
    /// parses URLs the WHATWG way, where `\` is a path delimiter for
    /// special schemes and `?`/`#` start the query/fragment. Scanning
    /// only for `/` (or stripping userinfo past those delimiters) would
    /// let `http://evil?@consented/` or `http://evil\@consented/`
    /// compute a consented origin while the request goes to evil —
    /// the check must see the same host the client will dial.
    pub(crate) fn origin_of(url: &str) -> Option<String> {
        let (scheme, rest) = url.split_once("://")?;
        if scheme != "http" && scheme != "https" {
            return None;
        }
        let authority_end = rest.find(['/', '?', '#', '\\']).unwrap_or(rest.len());
        let authority = &rest[..authority_end];
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

#[cfg(test)]
mod tests {
    use super::*;

    fn origin(url: &str) -> Option<String> {
        HttpRegistry::origin_of(url)
    }

    #[test]
    fn origin_extraction_strips_userinfo_path_and_case() {
        assert_eq!(
            origin("http://127.0.0.1:8402/path?q=1"),
            Some("http://127.0.0.1:8402".into())
        );
        // Userinfo inside the authority is stripped — that is its job.
        assert_eq!(
            origin("http://user:pw@127.0.0.1:8402/x"),
            Some("http://127.0.0.1:8402".into())
        );
        assert_eq!(
            origin("https://EXAMPLE.com/"),
            Some("https://example.com".into())
        );
        // No path at all, and an explicit default port, are fine.
        assert_eq!(
            origin("http://example.com"),
            Some("http://example.com".into())
        );
    }

    #[test]
    fn origin_extraction_refuses_non_http_and_empty_host() {
        assert_eq!(origin("ftp://example.com/"), None);
        assert_eq!(origin("HTTP://example.com/"), None);
        assert_eq!(origin("http://@/x"), None);
        assert_eq!(origin("http://"), None);
        assert_eq!(origin("http:///path"), None);
        assert_eq!(origin("not a url"), None);
    }

    #[test]
    fn origin_matching_is_exact_and_fail_closed_on_normalized_forms() {
        // WHATWG/IDNA normalizations the client applies but the gate
        // does NOT: each of these computes an origin that differs from
        // the plain form, so consent for one never covers the other.
        // That is a usability wart in the SAFE direction — a mismatch
        // refuses. If normalization is ever added it must happen on
        // BOTH sides of the comparison, or the differential reopens.
        let cases = [
            // Trailing dot: same DNS answer, different origin.
            ("http://example.com./", "http://example.com."),
            // Explicit default port vs the implicit form.
            ("http://example.com:80/", "http://example.com:80"),
            // Empty port (the client drops it).
            ("http://example.com:/", "http://example.com:"),
            // IDN: raw unicode, no punycode mapping on our side.
            ("http://exämple.com/", "http://exämple.com"),
            // UTS-46 ideographic full stop: the client maps 。 to "." —
            // we do not, so consent for example.com does not leak.
            ("http://example。com/", "http://example。com"),
        ];
        for (url, want) in cases {
            assert_eq!(origin(url), Some(want.to_string()), "url: {url}");
        }
        // Tabs/CR/LF (which the URL spec strips entirely) stay raw on
        // our side: they simply never match a consented origin.
        assert_eq!(
            origin("http://example\t.com/"),
            Some("http://example\t.com".into())
        );
        // None of the normalized twins pass a gate consented to the
        // plain form.
        let mut registry =
            HttpRegistry::new(["http://example.com".to_string()].into_iter().collect());
        for url in [
            "http://example.com./",
            "http://example.com:80/",
            "http://example.com:/",
            "http://example\t.com/",
        ] {
            let err = registry.request("GET", url, &[], &[]).unwrap_err();
            assert!(
                err.contains("not in consent allowlist"),
                "normalized twin slipped the gate: {url} → {err}"
            );
        }
    }

    #[test]
    fn gate_and_client_agree_on_every_accepted_url() {
        // The dangerous direction of a parser differential: the gate
        // accepts (origin IS the consented one) but the HTTP client
        // dials a different host. For every URL our extractor maps to
        // the consented origin, reqwest's own parser must name the
        // same scheme, host, and port.
        let consented = "http://127.0.0.1:8402";
        let urls = [
            "http://127.0.0.1:8402/",
            "http://127.0.0.1:8402",
            "http://user:pw@127.0.0.1:8402/x?q=1#f",
            "http://@127.0.0.1:8402/",
            "http://127.0.0.1:8402/path",
            // Backslash after the authority: path material, same host.
            "http://127.0.0.1:8402\\@evil.invalid/",
            // The bypass shapes must NOT reach this test's assert —
            // the gate computes a different origin for them (covered
            // above), so they are skipped here by construction.
            "http://evil.invalid?@127.0.0.1:8402/",
            "http://evil.invalid#@127.0.0.1:8402/",
            "http://evil.invalid\\@127.0.0.1:8402/",
            "http://127.0.0.1:8402.evil.invalid/",
        ];
        for url in urls {
            let Some(computed) = origin(url) else {
                continue;
            };
            if computed != consented {
                continue; // the gate refuses these — asserted elsewhere
            }
            let parsed = reqwest::Url::parse(url)
                .unwrap_or_else(|_| panic!("gate accepted but the client cannot parse: {url}"));
            let client_origin = match parsed.port() {
                Some(port) => format!(
                    "{}://{}:{port}",
                    parsed.scheme(),
                    parsed.host_str().expect("host")
                ),
                None => format!("{}://{}", parsed.scheme(), parsed.host_str().expect("host")),
            };
            assert_eq!(
                client_origin, consented,
                "gate/client disagree on {url}: gate saw {computed}, client dials {client_origin}"
            );
        }
    }

    #[test]
    fn consent_bypasses_via_delimiters_are_closed() {
        // Every one of these asks: does the check see the same host the
        // WHATWG parser in reqwest will dial? The authority ends at the
        // first of / ? # \ — anything after is not userinfo.
        let cases = [
            // `@` inside the query: host is evil, not 127.0.0.1.
            ("http://evil.test?@127.0.0.1:8402/", "http://evil.test"),
            // `@` inside the fragment.
            ("http://evil.test#@127.0.0.1:8402/", "http://evil.test"),
            // Backslash is a path delimiter for special schemes (WHATWG):
            // reqwest dials evil.test, so the origin must be evil.test.
            ("http://evil.test\\@127.0.0.1:8402/", "http://evil.test"),
            // Suffix lookalikes were never the consented host.
            (
                "http://127.0.0.1:8402.evil.test/",
                "http://127.0.0.1:8402.evil.test",
            ),
        ];
        for (url, want) in cases {
            assert_eq!(origin(url), Some(want.to_string()), "url: {url}");
        }
    }

    #[test]
    fn request_gate_applies_the_computed_origin() {
        let mut registry =
            HttpRegistry::new(["http://127.0.0.1:8402".to_string()].into_iter().collect());
        // Consented origin passes the gate (the send itself fails —
        // nothing listens — but the error must not be the allowlist).
        let err = registry
            .request("GET", "http://127.0.0.1:8402/", &[], &[])
            .unwrap_err();
        assert!(
            !err.contains("not in consent allowlist"),
            "consented origin refused: {err}"
        );
        // The delimiter tricks must never pass the gate.
        for url in [
            "http://evil.test?@127.0.0.1:8402/",
            "http://evil.test#@127.0.0.1:8402/",
            "http://evil.test\\@127.0.0.1:8402/",
            "http://127.0.0.1:8402.evil.test/",
        ] {
            let err = registry.request("GET", url, &[], &[]).unwrap_err();
            assert!(
                err.contains("not in consent allowlist"),
                "bypass slipped the gate: {url} → {err}"
            );
        }
    }

    #[test]
    fn redirects_are_never_followed() {
        // A consented origin answering 302 could otherwise move the
        // request somewhere the user never consented to. The guest must
        // see the 302 itself — and any follow-up to the Location target
        // goes through the gate again (covered above).
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let response = "HTTP/1.1 302 Found\r\n\
                            location: http://evil.test/loot\r\n\
                            content-length: 0\r\n\
                            connection: close\r\n\r\n";
            stream.write_all(response.as_bytes()).unwrap();
        });
        let origin = format!("http://127.0.0.1:{port}");
        let mut registry = HttpRegistry::new([origin.clone()].into_iter().collect());
        let handle = registry
            .request("GET", &format!("{origin}/give-up-your-secrets"), &[], &[])
            .expect("consented request");
        assert_eq!(registry.status(handle), Ok(302), "redirect was followed");
        assert_eq!(
            registry.header(handle, "location"),
            Ok(Some("http://evil.test/loot".into())),
            "the guest must see where the redirect wanted to go"
        );
        registry.close(handle);
    }
}
