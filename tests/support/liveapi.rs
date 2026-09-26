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
        Self {
            http: reqwest::Client::builder()
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
        }
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
