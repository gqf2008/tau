//! Origin-allowlisted HTTP capability shared by the provider, realtime and
//! bridge worlds. The registry is granted a set of "scheme://host[:port]"
//! origins by explicit user consent; every request's origin is checked
//! before sending, and redirects are never followed (a redirect would
//! escape consent). Granted empty, every call fails permission-denied at
//! call time, not instantiation time.
//!
//! Since 0.7.0 a started response is a resource the guest owns
//! (`http.response`) and its body is a stream: dropping the stream stops
//! the forwarding task, drops the `reqwest` response and closes the
//! connection -- that drop IS the cancellation an SSE consumer needs. The
//! 0.6.0 handle table, its explicit `close`, its "unknown http handle"
//! error and the guest-supplied `timeout-ms` parameter are all gone:
//! waiting is host policy now (docs/wit-redesign.md). [`REQUEST_TIMEOUT`]
//! bounds the wait for response headers (wit-review F11), [`IDLE_TIMEOUT`]
//! bounds the gap between body bytes, and a body that goes quiet ends the
//! stream -- which is what the contract's `response.body` doc promises.
//!
//! The client is the *async* reqwest: every caller is a host method that is
//! awaited on the runtime by now (docs/wit-redesign.md, stage 1), so a
//! request no longer needs a helper thread or the blocking pool, and the
//! bounded body queue back-pressures a fast peer instead of buffering it.

use std::collections::HashSet;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use wasmtime::component::{
    Destination, Resource, ResourceTable, StreamProducer, StreamResult, VecBuffer,
};
use wasmtime::{AsContextMut, StoreContextMut};

/// Cap on the wait for response HEADERS. A peer that accepts the connection
/// and then says nothing is indistinguishable from a dead one (wit-review
/// F11): 0.6.0 made the guest choose the bound, 0.7.0's contract makes it
/// the host's policy.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Cap on the gap between body bytes: the "idle policy on a silent peer"
/// the contract's `response.body` doc names. Generous enough for the
/// keep-alives an IM stream protocol sends; a dead peer is dropped.
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// The wait-for-headers budget in force: [`REQUEST_TIMEOUT`], or the test
/// knob `TAU_HTTP_REQUEST_TIMEOUT_MS`.
pub(crate) fn request_timeout() -> Duration {
    crate::budget("TAU_HTTP_REQUEST_TIMEOUT_MS", REQUEST_TIMEOUT)
}

/// The body's idle budget in force: [`IDLE_TIMEOUT`], or the test knob
/// `TAU_HTTP_IDLE_TIMEOUT_MS`.
pub(crate) fn idle_timeout() -> Duration {
    crate::budget("TAU_HTTP_IDLE_TIMEOUT_MS", IDLE_TIMEOUT)
}

/// Chunks the forwarding task may run ahead of the guest before it blocks.
/// Bounded on purpose: a slow consumer back-pressures the socket instead of
/// growing host memory without limit.
const BODY_QUEUE: usize = 64;

/// Room reserved when the reader is the host rather than the guest (a
/// read driven by the store's own plumbing): only used for readability
/// waits, never for data the guest asked for.
const DIRECT_CAPACITY: usize = 8192;

/// Why a request never left the process, in the contract's own three-way
/// split (`types.error`): the variant is what a guest branches on, the
/// detail is for the human reading the log.
#[derive(Debug)]
pub(crate) enum HttpError {
    /// No consent covers this origin: the URL is not in the allowlist.
    Refused(String),
    /// Consent covered it and the call failed: the peer is gone, or it
    /// took longer than the host's budget to answer.
    Failed(String),
    /// The call is not valid here: a URL the host cannot dial, a method
    /// token reqwest rejects, or a full resource table.
    Invalid(String),
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(detail) => write!(f, "refused: {detail}"),
            Self::Failed(detail) => write!(f, "failed: {detail}"),
            Self::Invalid(detail) => write!(f, "invalid: {detail}"),
        }
    }
}

/// One started response, owned by the guest as `http.response`. Metadata is
/// ready when `request` returns; the body moves out on the first `body()`.
pub struct HostResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Option<BodyStream>,
}

// `unwrap_err` on the request gate needs the success side to be printable,
// and the body's channel is not: print the metadata.
impl std::fmt::Debug for HostResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostResponse")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .finish_non_exhaustive()
    }
}

impl HostResponse {
    /// The status code, as it arrived.
    pub(crate) fn status(&self) -> u16 {
        self.status
    }

    /// One response header value, case-insensitively.
    pub(crate) fn header(&self, name: &str) -> Option<String> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(n, _)| n.to_ascii_lowercase() == name)
            .map(|(_, v)| v.clone())
    }

    /// Take the body stream. The contract's `body` is `func() -> stream<u8>`
    /// with no error to report a second call with, so a second call gets an
    /// empty stream -- the first one already owns the bytes.
    pub(crate) fn take_body(&mut self) -> BodyStream {
        self.body.take().unwrap_or_default()
    }
}

/// The response body as the guest reads it: the chunks the forwarding task
/// has queued, plus whatever is left of the chunk currently being handed
/// over. Ends when the task's sender drops (clean EOF, or the final error
/// that says why a silent or broken peer ended it).
pub struct BodyStream {
    rx: tokio::sync::mpsc::Receiver<Result<Vec<u8>, String>>,
    /// Bytes received but not yet written to the guest's buffer. Carrying
    /// the remainder here (rather than trusting the guest's buffer size)
    /// keeps delivery independent of how much room one read offers.
    pending: Vec<u8>,
}

impl Default for BodyStream {
    /// A stream that is already over: the reader end exists but its sender
    /// is gone, so the first poll reports EOF. This is what `body()` hands
    /// back on an exhausted response (or one that has left the table) --
    /// an empty body, never a panic.
    fn default() -> Self {
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        Self {
            rx,
            pending: Vec::new(),
        }
    }
}

impl<D> StreamProducer<D> for BodyStream {
    type Item = u8;
    type Buffer = VecBuffer<u8>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: StoreContextMut<'a, D>,
        dst: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        // An empty destination buffer is the guest asking to WAIT until this
        // stream is readable. There is no way to await that here, so answer
        // "maybe later" the way wasmtime-wasi does
        // (WebAssembly/component-model#561).
        if dst.remaining(store.as_context_mut()) == Some(0) {
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        let this = self.as_mut().get_mut();
        if this.pending.is_empty() {
            match this.rx.poll_recv(cx) {
                Poll::Ready(Some(Ok(chunk))) => this.pending = chunk,
                Poll::Ready(Some(Err(why))) => {
                    // The stream still ends; the reason goes to the log
                    // because the contract's `body` has one terminal state.
                    eprintln!("tau http.response.body: {why}");
                    return Poll::Ready(Ok(StreamResult::Dropped));
                }
                Poll::Ready(None) => return Poll::Ready(Ok(StreamResult::Dropped)),
                Poll::Pending if finish => return Poll::Ready(Ok(StreamResult::Cancelled)),
                Poll::Pending => return Poll::Pending,
            }
        }
        let mut dst = dst.as_direct(store, DIRECT_CAPACITY);
        let buf = dst.remaining();
        if buf.is_empty() {
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        let n = buf.len().min(this.pending.len());
        buf[..n].copy_from_slice(&this.pending[..n]);
        this.pending.drain(..n);
        dst.mark_written(n);
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

/// Hand the response's body to the guest. A resource that is not in the
/// table (or whose body was already taken) yields an empty stream -- the
/// contract's `body` has no error to report with.
pub(crate) fn take_body(table: &mut ResourceTable, response: &Resource<HostResponse>) -> BodyStream {
    match table.get_mut(response) {
        Ok(response) => response.take_body(),
        Err(_) => {
            eprintln!("tau http.response.body: the response resource is not in the table");
            BodyStream::default()
        }
    }
}

/// Send one request and return once the response HEADERS are in. `body` is
/// the whole request body (MCP JSON-RPC messages and OAuth token calls are
/// not worth a second direction of streaming -- the contract says so).
///
/// The body then streams into a bounded queue from a task that stops the
/// moment the guest drops the stream (`tx.closed()`), which is what closes
/// the connection.
pub(crate) async fn send(
    client: &reqwest::Client,
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
    request_timeout: Duration,
    idle_timeout: Duration,
) -> Result<HostResponse, HttpError> {
    let method = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|e| HttpError::Invalid(format!("bad method {method}: {e}")))?;
    let mut request = client.request(method, url).body(body.to_vec());
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let response = tokio::time::timeout(request_timeout, request.send())
        .await
        .map_err(|_| {
            HttpError::Failed(format!(
                "http.request: no response headers within {}ms",
                request_timeout.as_millis()
            ))
        })?
        .map_err(|e| HttpError::Failed(format!("http {url}: {e}")))?;
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
    let (tx, rx) = tokio::sync::mpsc::channel(BODY_QUEUE);
    tokio::spawn(async move {
        let mut response = response;
        loop {
            tokio::select! {
                // The guest dropped the stream: stop reading, drop the
                // response, close the connection.
                _ = tx.closed() => break,
                chunk = tokio::time::timeout(idle_timeout, response.chunk()) => match chunk {
                    Ok(Ok(Some(bytes))) => {
                        if tx.send(Ok(bytes.to_vec())).await.is_err() {
                            break;
                        }
                    }
                    Ok(Ok(None)) => break,
                    Ok(Err(e)) => {
                        let _ = tx.send(Err(format!("http body: {e}"))).await;
                        break;
                    }
                    Err(_) => {
                        let _ = tx
                            .send(Err(format!(
                                "http body: no bytes within {}ms",
                                idle_timeout.as_millis()
                            )))
                            .await;
                        break;
                    }
                },
            }
        }
    });
    Ok(HostResponse {
        status,
        headers: response_headers,
        body: Some(BodyStream { rx, pending: Vec::new() }),
    })
}

/// The consent allowlist plus the shared HTTP client. Every request goes
/// through [`HttpRegistry::start`] before a byte leaves the process.
pub(crate) struct HttpRegistry {
    /// Consented origins: "scheme://host[:port]". Empty = deny all.
    origins: HashSet<String>,
    /// Redirects are never followed: a redirect would silently move the
    /// request to an origin the user did not consent to.
    client: reqwest::Client,
}

impl HttpRegistry {
    pub(crate) fn new(origins: HashSet<String>) -> Self {
        Self {
            origins,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap_or_default(),
        }
    }

    /// Extract the consent origin ("scheme://host[:port]") from an http(s)
    /// URL; strips userinfo and any path.
    ///
    /// The authority ends at the first `/`, `?`, `#`, OR `\` — reqwest
    /// parses URLs the WHATWG way, where `\` is a path delimiter for
    /// special schemes and `?`/`#` start the query/fragment. Scanning
    /// only for `/` (or stripping userinfo past those delimiters) would
    /// let `http://evil?@consented/` or `http://evil\\@consented/`
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

    /// Check consent and hand back the shared client. Deliberately sync and
    /// lock-scoped: the caller awaits the request itself, so no mutex guard
    /// is ever held across an await (a std guard across an await makes the
    /// host's future `!Send`).
    pub(crate) fn start(&self, url: &str) -> Result<reqwest::Client, HttpError> {
        let origin =
            Self::origin_of(url).ok_or_else(|| HttpError::Invalid(format!("bad url: {url}")))?;
        if !self.origins.contains(&origin) {
            return Err(HttpError::Refused(format!(
                "http: origin {origin} not in consent allowlist ({} granted)",
                self.origins.len()
            )));
        }
        Ok(self.client.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};

    /// A budget generous enough for a loopback handshake: tests that
    /// exercise the gate (not the clock) pass this and ignore it.
    const BUDGET: Duration = Duration::from_secs(5);

    fn origin(url: &str) -> Option<String> {
        HttpRegistry::origin_of(url)
    }

    /// A loopback server that answers the first request with `headers` and
    /// then hands the socket to `then`. Returns the origin to consent to.
    fn serve<F>(headers: &'static str, then: F) -> String
    where
        F: FnOnce(std::net::TcpStream) + Send + 'static,
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(headers.as_bytes());
            then(stream);
        });
        format!("http://127.0.0.1:{port}")
    }

    /// Drain a body stream the way a guest's read loop does, without
    /// wasmtime: receive until the forwarding task's sender is gone.
    async fn drain(body: &mut BodyStream) -> Vec<Result<Vec<u8>, String>> {
        let mut out = Vec::new();
        while let Some(item) = body.rx.recv().await {
            out.push(item);
        }
        out
    }

    fn registry_for(url: &str) -> HttpRegistry {
        HttpRegistry::new([origin(url).expect("origin")].into_iter().collect())
    }

    #[test]
    fn origin_extraction_strips_userinfo_path_and_case() {
        assert_eq!(
            origin("http://127.0.0.1:8402/path?q=1"),
            Some("http://127.0.0.1:8402".into())
        );
        // Userinfo inside the authority is stripped -- that is its job.
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
            // UTS-46 ideographic full stop: the client maps it to ".";
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
        let registry = HttpRegistry::new(["http://example.com".to_string()].into_iter().collect());
        for url in [
            "http://example.com./",
            "http://example.com:80/",
            "http://example.com:/",
            "http://example\t.com/",
        ] {
            let err = registry.start(url).unwrap_err();
            assert!(
                matches!(err, HttpError::Refused(_)),
                "normalized twin slipped the gate: {url} -> {err}"
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

    #[tokio::test]
    async fn request_gate_applies_the_computed_origin() {
        let registry = registry_for("http://127.0.0.1:8402");
        // The consented origin passes the gate (the send itself then
        // fails — nothing listens — but the error must not be the
        // allowlist).
        let client = registry
            .start("http://127.0.0.1:8402/")
            .expect("consented origin passes the gate");
        let err = send(&client, "GET", "http://127.0.0.1:8402/", &[], &[], BUDGET, BUDGET)
            .await
            .unwrap_err();
        assert!(
            !matches!(err, HttpError::Refused(_)),
            "consented origin refused: {err}"
        );
        // The delimiter tricks must never pass the gate.
        for url in [
            "http://evil.test?@127.0.0.1:8402/",
            "http://evil.test#@127.0.0.1:8402/",
            "http://evil.test\\@127.0.0.1:8402/",
            "http://127.0.0.1:8402.evil.test/",
        ] {
            let err = registry.start(url).unwrap_err();
            assert!(
                matches!(err, HttpError::Refused(_)),
                "bypass slipped the gate: {url} -> {err}"
            );
        }
    }

    #[tokio::test]
    async fn redirects_are_never_followed() {
        // A consented origin answering 302 could otherwise move the
        // request somewhere the user never consented to. The guest must
        // see the 302 itself — and any follow-up to the Location target
        // goes through the gate again (covered above).
        let origin = serve(
            "HTTP/1.1 302 Found\r\n\
             location: http://evil.test/loot\r\n\
             content-length: 0\r\n\
             connection: close\r\n\r\n",
            |_| {},
        );
        let registry = registry_for(&origin);
        let url = format!("{origin}/give-up-your-secrets");
        let client = registry.start(&url).expect("consented");
        let mut response = send(&client, "GET", &url, &[], &[], BUDGET, BUDGET)
            .await
            .expect("consented request");
        assert_eq!(response.status(), 302, "redirect was followed");
        assert_eq!(
            response.header("location"),
            Some("http://evil.test/loot".into()),
            "the guest must see where the redirect wanted to go"
        );
        assert_eq!(drain(&mut response.take_body()).await, Vec::new());
    }

    #[tokio::test]
    async fn request_times_out_when_the_peer_never_sends_headers() {
        // The peer accepts the connection and then says nothing at all: no
        // headers, no error, no FIN. Before F11 this call parked the host
        // thread until the process exited — a stalled IM/MCP endpoint could
        // wedge a whole run.
        let origin = serve("", |_| {
            // Hold the connection open, silent, far past the budget.
            std::thread::sleep(Duration::from_millis(1500));
        });
        let registry = registry_for(&origin);
        let url = format!("{origin}/silent");
        let client = registry.start(&url).expect("consented");
        let started = std::time::Instant::now();
        let err = send(
            &client,
            "GET",
            &url,
            &[],
            &[],
            Duration::from_millis(300),
            BUDGET,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("no response headers within 300ms"),
            "unexpected error: {err}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(1200),
            "request outlived its budget: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_body_that_ends_cleanly_closes_the_stream() {
        // The ordinary case: the peer sends the body and hangs up. The
        // guest's read loop sees the bytes, then the end.
        let origin = serve(
            "HTTP/1.1 200 OK\r\ncontent-length: 5\r\nconnection: close\r\n\r\nhello",
            |_| {},
        );
        let registry = registry_for(&origin);
        let client = registry.start(&origin).expect("consented");
        let mut response = send(&client, "GET", &origin, &[], &[], BUDGET, BUDGET)
            .await
            .expect("consented request");
        let chunks = drain(&mut response.take_body()).await;
        let bytes: Vec<u8> = chunks
            .into_iter()
            .flat_map(|chunk| chunk.expect("a clean body reports no error"))
            .collect();
        assert_eq!(bytes, b"hello");
    }

    #[tokio::test]
    async fn a_silent_body_ends_the_stream_instead_of_hanging() {
        // Headers arrive, the body never does, the connection stays open:
        // the silent-SSE / half-open-TCP case. Before F9 the guest's read
        // blocked the host thread until process exit; the forwarding
        // task's idle bound now reports it and ends the stream.
        let origin = serve(
            "HTTP/1.1 200 OK\r\ncontent-length: 100\r\nconnection: keep-alive\r\n\r\n",
            |mut stream| {
                std::thread::sleep(Duration::from_millis(1500));
                let _ = stream.write_all(b"late");
            },
        );
        let registry = registry_for(&origin);
        let client = registry.start(&origin).expect("consented");
        let mut response = send(
            &client,
            "GET",
            &origin,
            &[],
            &[],
            BUDGET,
            Duration::from_millis(200),
        )
        .await
        .expect("consented request");
        let started = std::time::Instant::now();
        let items = drain(&mut response.take_body()).await;
        assert_eq!(items.len(), 1, "a silent body ends with one report");
        let err = items[0].as_ref().expect_err("the quiet peer is an error");
        assert!(err.contains("no bytes within 200ms"), "{err}");
        assert!(
            started.elapsed() < Duration::from_millis(1200),
            "the body outlived its idle budget: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn dropping_the_body_abandons_the_connection() {
        // A chatty peer (an SSE stream) and a guest that stops reading
        // after one chunk: the drop must close the connection, or a
        // component that walks away would leave the host reading a
        // socket nobody wants. This drop IS the cancellation the
        // contract's `response.body` doc advertises.
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let abandoned = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&abandoned);
        let origin = serve(
            "HTTP/1.1 200 OK\r\ncontent-length: 1000000\r\n\r\n",
            move |mut stream| {
                // Keep talking until a write fails: that failure means the
                // socket is gone, i.e. the guest's drop closed it.
                for _ in 0..600 {
                    if stream.write_all(&[b'x'; 1024]).is_err() {
                        flag.store(true, Ordering::SeqCst);
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            },
        );
        let registry = registry_for(&origin);
        let client = registry.start(&origin).expect("consented");
        let mut response = send(&client, "GET", &origin, &[], &[], BUDGET, BUDGET)
            .await
            .expect("consented request");
        let mut body = response.take_body();
        assert!(
            body.rx.recv().await.is_some(),
            "the peer's first chunk arrives"
        );
        drop(body);
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !abandoned.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            abandoned.load(Ordering::SeqCst),
            "the connection stayed open after the guest dropped the body"
        );
    }
}
