//! Shared HTTP client for the opt-in live API contract tests.

use reqwest::{Method, StatusCode, Url, header::HeaderMap};
use serde_json::Value;

pub(crate) struct LiveApi {
    pub(crate) http: reqwest::Client,
    base: Url,
    pub(crate) token: String,
}

pub(crate) struct LiveResponse {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) value: Option<Value>,
    /// HTTP body bytes transferred, before decompression and excluding headers.
    pub(crate) body_len: usize,
}

impl LiveApi {
    pub(crate) fn readonly_from_env() -> Self {
        assert_eq!(
            std::env::var("HACKMD_RUN_LIVE_READONLY_TESTS").as_deref(),
            Ok("1"),
            "set HACKMD_RUN_LIVE_READONLY_TESTS=1 to run the read-only live probe"
        );
        Self::from_env()
    }

    pub(crate) fn destructive_from_env() -> Self {
        assert_eq!(
            std::env::var("HACKMD_RUN_LIVE_TESTS").as_deref(),
            Ok("1"),
            "set HACKMD_RUN_LIVE_TESTS=1 to acknowledge destructive live tests"
        );
        assert_eq!(
            std::env::var("HACKMD_CONFIRM_DESTRUCTIVE_LIVE_TESTS").as_deref(),
            Ok("YES"),
            "set HACKMD_CONFIRM_DESTRUCTIVE_LIVE_TESTS=YES to confirm writes and cleanup"
        );
        Self::from_env()
    }

    fn from_env() -> Self {
        let token = std::env::var("HACKMD_LIVE_TEST_TOKEN").unwrap_or_else(|_| {
            panic!("set a dedicated HACKMD_LIVE_TEST_TOKEN; do not use the normal server token")
        });
        assert!(
            !token.trim().is_empty(),
            "live test token must not be empty"
        );
        let base = std::env::var("HACKMD_LIVE_TEST_API_URL")
            .unwrap_or_else(|_| "https://api.hackmd.io/v1".to_owned());
        let base = Url::parse(&base).expect("HACKMD_LIVE_TEST_API_URL must be a valid URL");
        assert_eq!(base.scheme(), "https", "live API URL must use HTTPS");
        assert!(
            base.username().is_empty()
                && base.password().is_none()
                && base.query().is_none()
                && base.fragment().is_none(),
            "live API URL must not contain credentials, a query, or a fragment"
        );
        Self::new(base, token)
    }

    fn new(base: Url, token: String) -> Self {
        Self {
            http: reqwest::Client::builder()
                .no_gzip()
                .no_brotli()
                .no_deflate()
                .no_zstd()
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("live HTTP client should build"),
            base,
            token,
        }
    }

    pub(crate) fn url(&self, segments: &[&str]) -> Url {
        let mut url = self.base.clone();
        let mut path = url
            .path_segments_mut()
            .expect("HTTPS live API URL must support path segments");
        path.pop_if_empty();
        path.extend(segments);
        drop(path);
        url
    }

    pub(crate) async fn get(
        &self,
        segments: &[&str],
        conditional: Option<(&str, &str)>,
    ) -> LiveResponse {
        let mut request = self.http.get(self.url(segments)).bearer_auth(&self.token);
        if let Some((name, value)) = conditional {
            request = request.header(name, value);
        }
        self.send(request).await
    }

    pub(crate) async fn request(
        &self,
        method: Method,
        segments: &[&str],
        body: Option<&Value>,
    ) -> LiveResponse {
        let mut request = self
            .http
            .request(method, self.url(segments))
            .bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(body);
        }
        self.send(request).await
    }

    /// POSTs `bytes` to an image route as the multipart `image` field the
    /// upload tool sends.
    pub(crate) async fn upload_png(&self, segments: &[&str], bytes: Vec<u8>) -> LiveResponse {
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name("probe.png")
            .mime_str("image/png")
            .expect("static MIME type should parse");
        let request = self
            .http
            .post(self.url(segments))
            .bearer_auth(&self.token)
            .multipart(reqwest::multipart::Form::new().part("image", part));
        self.send(request).await
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> LiveResponse {
        let response = request
            .send()
            .await
            .expect("live API request should complete");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response
            .bytes()
            .await
            .expect("live response body should read");
        let value = (!bytes.is_empty()).then(|| {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        });
        LiveResponse {
            status,
            headers,
            value,
            body_len: bytes.len(),
        }
    }

    /// GETs `segments` until `accepted` holds, up to ten reads a second
    /// apart, since `HackMD` shows some writes only after a delay. A 429 or
    /// 5xx read counts as not yet. `None` if it never did.
    pub(crate) async fn poll_json(
        &self,
        segments: &[&str],
        accepted: impl Fn(&Value) -> bool,
    ) -> Option<Value> {
        for attempt in 0..10 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            let response = self.request(Method::GET, segments, None).await;
            if response.status == StatusCode::TOO_MANY_REQUESTS || response.status.is_server_error()
            {
                continue;
            }
            assert!(
                response.status.is_success(),
                "live API returned {}: {:?}",
                response.status,
                response.value
            );
            let read = response
                .value
                .expect("live API response should contain JSON");
            if accepted(&read) {
                return Some(read);
            }
        }
        None
    }

    pub(crate) async fn json(
        &self,
        method: Method,
        segments: &[&str],
        body: Option<&Value>,
    ) -> Value {
        let response = self.request(method, segments, body).await;
        assert!(
            response.status.is_success(),
            "live API returned {}: {:?}",
            response.status,
            response.value
        );
        response
            .value
            .expect("live API response should contain JSON")
    }

    pub(crate) async fn empty_ok(&self, method: Method, segments: &[&str], body: Option<&Value>) {
        let response = self.request(method, segments, body).await;
        assert!(
            response.status.is_success(),
            "live API returned {}: {:?}",
            response.status,
            response.value
        );
    }

    pub(crate) async fn delete_note(&self, note_id: &str) {
        self.empty_ok(Method::DELETE, &["notes", note_id], None)
            .await;
    }

    pub(crate) async fn cleanup_delete(&self, segments: &[&str], resource: &str) {
        match self
            .http
            .delete(self.url(segments))
            .bearer_auth(&self.token)
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => eprintln!(
                "cleanup failed for {resource}: status={}",
                response.status()
            ),
            Err(error) => eprintln!("cleanup failed for {resource}: transport error: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        time::{Duration, Instant},
    };

    #[tokio::test]
    async fn compressed_probe_counts_raw_body_and_sends_requested_encoding() {
        const PACKED: &[u8] = &[
            31, 139, 8, 0, 0, 0, 0, 0, 0, 255, 171, 86, 202, 207, 86, 178, 42, 41, 42, 77, 173, 5,
            0, 144, 95, 212, 167, 11, 0, 0, 0,
        ];
        let listener = TcpListener::bind("127.0.0.1:0").expect("probe fixture bind");
        let address = listener.local_addr().expect("probe fixture address");
        listener.set_nonblocking(true).expect("nonblocking fixture");
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("probe fixture accept: {error}"),
                }
            };
            stream
                .set_nonblocking(false)
                .expect("blocking fixture stream");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("fixture timeout");
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .expect("fixture timeout");
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).expect("probe request headers");
                request.push(byte[0]);
                assert!(request.len() < 16 * 1024, "fixture header limit");
            }
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                PACKED.len()
            );
            stream
                .write_all(headers.as_bytes())
                .expect("fixture response headers");
            stream.write_all(PACKED).expect("fixture compressed body");
            String::from_utf8(request).expect("request header text")
        });
        let api = super::LiveApi::new(
            reqwest::Url::parse(&format!("http://{address}/v1")).expect("fixture URL"),
            "fixture-only-token".to_owned(),
        );
        let response = api
            .get(&["notes", "id"], Some(("accept-encoding", "gzip, br")))
            .await;
        assert_eq!(response.body_len, PACKED.len());
        assert_eq!(response.headers["content-encoding"], "gzip");
        let request = server
            .join()
            .expect("probe fixture finished")
            .to_ascii_lowercase();
        assert!(
            request.starts_with("get /v1/notes/id http/1.1"),
            "{request}"
        );
        assert!(request.contains("accept-encoding: gzip, br"), "{request}");
        assert!(
            request.contains("authorization: bearer fixture-only-token"),
            "{request}"
        );
    }
}
