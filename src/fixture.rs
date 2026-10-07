use std::{
    fmt::Write as _,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::{
    client::HackmdClient, config::Config, local::LocalFiles, models::Workspace,
    sync::state::TrackedNoteState,
};

/// The token every fixture client presents. Nothing verifies it; it exists so
/// requests carry an Authorization header the assertions can inspect.
pub(crate) const FIXTURE_TOKEN: &str = "fixture-token";
const FIXTURE_DEADLINE: Duration = Duration::from_secs(5);

type FixtureHeaders = &'static [(&'static str, &'static str)];

const EMPTY_HEADERS: FixtureHeaders = &[];

/// One canned response. Owned fields let scenario tests construct headers and
/// bodies dynamically without leaking them for a `'static` tuple.
#[derive(Clone)]
struct FixtureResponse {
    status: u16,
    body: String,
    headers: Vec<(String, String)>,
    delay: Duration,
    /// Runs on the server thread after the request arrives and before the
    /// response goes out, to stage something happening mid-request.
    before: Option<Arc<dyn Fn() + Send + Sync>>,
}

type BodyPredicate = Box<dyn Fn(&str) -> bool + Send + Sync>;

struct ExpectedRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Option<(String, BodyPredicate)>,
}

/// A declarative request/response step for HTTP-facing tests.
pub(crate) struct Scenario {
    expected: ExpectedRequest,
    response: FixtureResponse,
}

impl Scenario {
    pub(crate) fn new(method: &str, encoded_path: &str, status: u16, body: &str) -> Self {
        Self {
            expected: ExpectedRequest {
                method: method.to_owned(),
                path: encoded_path.to_owned(),
                headers: Vec::new(),
                body: None,
            },
            response: FixtureResponse {
                status,
                body: body.to_owned(),
                headers: Vec::new(),
                delay: Duration::ZERO,
                before: None,
            },
        }
    }

    pub(crate) fn expect_header(mut self, name: &str, value: &str) -> Self {
        self.expected
            .headers
            .push((name.to_owned(), value.to_owned()));
        self
    }

    pub(crate) fn expect_body(
        mut self,
        description: &str,
        predicate: impl Fn(&str) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.expected.body = Some((description.to_owned(), Box::new(predicate)));
        self
    }

    /// Runs `action` while this request is in flight: after it arrives, before
    /// the response is sent.
    pub(crate) fn before_response(mut self, action: impl Fn() + Send + Sync + 'static) -> Self {
        self.response.before = Some(Arc::new(action));
        self
    }

    pub(crate) fn response_header(mut self, name: &str, value: &str) -> Self {
        self.response
            .headers
            .push((name.to_owned(), value.to_owned()));
        self
    }

    pub(crate) const fn delay(mut self, delay: Duration) -> Self {
        self.response.delay = delay;
        self
    }
}

fn fixture_response(
    status: u16,
    body: &str,
    headers: FixtureHeaders,
    delay: Duration,
) -> FixtureResponse {
    FixtureResponse {
        status,
        body: body.to_owned(),
        headers: headers
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect(),
        delay,
        before: None,
    }
}

fn assert_scenarios(requests: &[String], expected: &[ExpectedRequest]) {
    assert_eq!(
        requests.len(),
        expected.len(),
        "scenario request count differed"
    );
    for (index, (request, expected)) in requests.iter().zip(expected).enumerate() {
        let (head, body) = request.split_once("\r\n\r\n").unwrap_or((request, ""));
        let mut lines = head.lines();
        let request_line = lines.next().unwrap_or("");
        let mut request_parts = request_line.split_whitespace();
        assert_eq!(
            request_parts.next(),
            Some(expected.method.as_str()),
            "scenario {index} method differed"
        );
        assert_eq!(
            request_parts.next(),
            Some(expected.path.as_str()),
            "scenario {index} encoded path differed"
        );
        let headers = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim(), value.trim()))
            .collect::<Vec<_>>();
        for (name, value) in &expected.headers {
            let actual = headers.iter().find_map(|(candidate, value)| {
                candidate.eq_ignore_ascii_case(name).then_some(value)
            });
            assert_eq!(
                actual,
                Some(&value.as_str()),
                "scenario {index} header {name:?} differed"
            );
        }
        if let Some((description, predicate)) = &expected.body {
            assert!(
                predicate(body),
                "scenario {index} body did not satisfy {description}"
            );
        }
    }
}

pub(crate) struct SequenceServer {
    pub(crate) api_url: String,
    requests: Receiver<Vec<String>>,
    shutdown: Sender<()>,
    thread: Option<JoinHandle<()>>,
    expected: Option<Vec<ExpectedRequest>>,
}

impl SequenceServer {
    /// Accepts one complete request and closes the connection without a
    /// response, producing a deterministic transport error without releasing a
    /// port that another parallel fixture could claim.
    pub(crate) fn spawn_disconnect() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("fixture should bind");
        listener
            .set_nonblocking(true)
            .expect("fixture listener should be nonblocking");
        let address = listener.local_addr().expect("fixture address should exist");
        let (sender, requests) = mpsc::channel();
        let (shutdown, shutdown_rx) = mpsc::channel();
        let (ready, ready_rx) = mpsc::sync_channel(0);
        let thread = thread::spawn(move || {
            let deadline = Instant::now() + FIXTURE_DEADLINE;
            ready.send(()).expect("fixture readiness should send");
            let captured = accept_next(&listener, &shutdown_rx, deadline)
                .map(|mut stream| vec![read_request(&mut stream, deadline)])
                .unwrap_or_default();
            let _ = sender.send(captured);
        });
        ready_rx.recv().expect("fixture thread should become ready");
        Self {
            api_url: format!("http://{address}/v1"),
            requests,
            shutdown,
            thread: Some(thread),
            expected: None,
        }
    }

    pub(crate) fn spawn_scenarios<const N: usize>(scenarios: [Scenario; N]) -> Self {
        let mut responses = Vec::with_capacity(N);
        let mut expected = Vec::with_capacity(N);
        for scenario in scenarios {
            responses.push(scenario.response);
            expected.push(scenario.expected);
        }
        Self::spawn_with_mode(responses, false, Some(expected))
    }

    /// Replays the sequence, then keeps answering with its last response.
    ///
    /// For tests about a condition that never becomes true, where pinning the
    /// exact number of requests would pin `poll_readback`'s schedule instead of
    /// the behavior under test. Such a fixture is never finished; it is dropped
    /// with its thread still waiting.
    pub(crate) fn spawn_repeating<const N: usize>(responses: [(u16, &str); N]) -> Self {
        Self::spawn_with_mode(
            responses
                .into_iter()
                .map(|(status, body)| fixture_response(status, body, EMPTY_HEADERS, Duration::ZERO))
                .collect(),
            true,
            None,
        )
    }

    fn spawn_with_mode(
        responses: Vec<FixtureResponse>,
        repeating: bool,
        expected: Option<Vec<ExpectedRequest>>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("fixture should bind");
        listener
            .set_nonblocking(true)
            .expect("fixture listener should be nonblocking");
        let address = listener.local_addr().expect("fixture address should exist");

        // A body can name a link on the fixture itself, whose port is known
        // only now: `{origin}` stands for it.
        let origin = format!("http://{address}");
        let mut responses = responses;
        for response in &mut responses {
            response.body = response.body.replace("{origin}", &origin);
            for (_, value) in &mut response.headers {
                *value = value.replace("{origin}", &origin);
            }
        }
        let (sender, requests) = mpsc::channel();
        let (shutdown, shutdown_rx) = mpsc::channel();
        let (ready, ready_rx) = mpsc::sync_channel(0);
        let thread = thread::spawn(move || {
            let deadline = Instant::now() + FIXTURE_DEADLINE;
            ready.send(()).expect("fixture readiness should send");
            let mut captured = Vec::with_capacity(responses.len());
            let last = responses.last().cloned();
            for response in responses {
                let Some(mut stream) = accept_next(&listener, &shutdown_rx, deadline) else {
                    let _ = sender.send(captured);
                    return;
                };
                captured.push(read_request(&mut stream, deadline));
                if let Some(action) = &response.before {
                    action();
                }
                let extra_headers =
                    response
                        .headers
                        .iter()
                        .fold(String::new(), |mut output, (name, value)| {
                            write!(output, "{name}: {value}\r\n")
                                .expect("writing headers to String cannot fail");
                            output
                        });
                if !wait_delay(response.delay, &shutdown_rx, deadline) {
                    let _ = sender.send(captured);
                    return;
                }

                // A scenario that names its own content type replaces the JSON
                // default rather than sending two.
                let content_type = if response
                    .headers
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
                {
                    ""
                } else {
                    "Content-Type: application/json\r\n"
                };
                let response = format!(
                    "HTTP/1.1 {} Fixture\r\n{content_type}{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.status,
                    response.body.len(),
                    response.body
                );
                let _ = stream.write_all(response.as_bytes());
            }
            if sender.send(captured).is_err() {
                return;
            }

            let Some(response) = last.filter(|_| repeating) else {
                return;
            };
            let response = format!(
                "HTTP/1.1 {} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.status,
                response.body.len(),
                response.body
            );
            while let Some(mut stream) = accept_next(&listener, &shutdown_rx, deadline) {
                read_request(&mut stream, deadline);
                let _ = stream.write_all(response.as_bytes());
            }
        });
        ready_rx.recv().expect("fixture thread should become ready");
        Self {
            api_url: format!("http://{address}/v1"),
            requests,
            shutdown,
            thread: Some(thread),
            expected,
        }
    }

    /// A client pointed at this fixture.
    pub(crate) fn client(&self) -> HackmdClient {
        HackmdClient::new(Config::for_loopback_test(
            &self.api_url,
            Some(FIXTURE_TOKEN),
        ))
        .expect("fixture client should build")
    }

    /// A client whose note-list cache is on, for tests about caching.
    pub(crate) fn client_with_cache(&self) -> HackmdClient {
        HackmdClient::new(Config::for_loopback_test_with_cache(&self.api_url))
            .expect("cache-enabled client should build")
    }

    /// A client that presents `token`, for tests that assert on the header or
    /// on redaction.
    pub(crate) fn client_with_token(&self, token: &str) -> HackmdClient {
        HackmdClient::new(Config::for_loopback_test(&self.api_url, Some(token)))
            .expect("fixture client should build")
    }

    /// A client that retries with millisecond backoff, for tests that a write
    /// is not retried: script a failure then a success, and only a single
    /// attempt fails.
    pub(crate) fn client_with_fast_retry(&self) -> HackmdClient {
        HackmdClient::new(Config::for_loopback_test_with_retry(
            &self.api_url,
            FIXTURE_TOKEN,
            crate::config::RetryConfig {
                max_retries: 3,
                initial_backoff: Duration::from_millis(1),
                max_backoff: Duration::from_millis(2),
            },
        ))
        .expect("retry client should build")
    }

    /// A client that fails instead of retrying, so a test sees the first
    /// status.
    pub(crate) fn client_without_retry(&self, token: &str) -> HackmdClient {
        HackmdClient::new(Config::for_loopback_test_no_retry(&self.api_url, token))
            .expect("fixture client should build")
    }

    pub(crate) fn finish(mut self) -> Vec<String> {
        let requests = self
            .requests
            .recv_timeout(FIXTURE_DEADLINE)
            .expect("requests should be captured");
        let _ = self.shutdown.send(());
        self.thread
            .take()
            .expect("fixture thread should exist")
            .join()
            .expect("fixture thread should finish");
        if let Some(expected) = self.expected.take() {
            assert_scenarios(&requests, &expected);
        }
        requests
    }
}

impl Drop for SequenceServer {
    fn drop(&mut self) {
        let _ = self.shutdown.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn accept_next(
    listener: &TcpListener,
    shutdown: &Receiver<()>,
    deadline: Instant,
) -> Option<TcpStream> {
    loop {
        if shutdown.try_recv().is_ok() {
            return None;
        }
        if Instant::now() >= deadline {
            return None;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_nonblocking(false)
                    .expect("fixture stream should be blocking");
                return Some(stream);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("fixture accept failed: {error}"),
        }
    }
}

fn wait_delay(delay: Duration, shutdown: &Receiver<()>, deadline: Instant) -> bool {
    if delay.is_zero() {
        return true;
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return false;
    }
    let wait = delay.min(remaining);
    match shutdown.recv_timeout(wait) {
        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => false,
        Err(mpsc::RecvTimeoutError::Timeout) => delay <= remaining && Instant::now() <= deadline,
    }
}

fn read_request(stream: &mut TcpStream, deadline: Instant) -> String {
    let remaining = deadline.saturating_duration_since(Instant::now());
    assert!(
        !remaining.is_zero(),
        "fixture deadline elapsed before request read"
    );
    stream
        .set_read_timeout(Some(remaining))
        .expect("read timeout should set");
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        assert!(
            Instant::now() < deadline,
            "fixture deadline elapsed while reading request"
        );
        let count = match stream.read(&mut buffer) {
            Ok(count) => count,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Interrupted
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(error) => panic!("request should read: {error}"),
        };
        assert!(count > 0, "connection closed before full request arrived");
        request.extend_from_slice(&buffer[..count]);
        let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        if request.len() >= header_end + 4 + length {
            // Lossy: multipart uploads carry binary bodies, and assertions only
            // ever look at the textual parts.
            return String::from_utf8_lossy(&request).into_owned();
        }
    }
}

/// Local storage whose state directory already tracks `local_path` at
/// `baseline`, which is the starting point every sync tool test needs.
pub(crate) fn tracked_files(
    state_root: &Path,
    note_id: &str,
    local_path: &Path,
    baseline: &str,
) -> LocalFiles {
    tracked_files_in(
        Workspace::Personal,
        state_root,
        note_id,
        local_path,
        baseline,
    )
}

/// [`tracked_files`] for a note in `workspace`.
pub(crate) fn tracked_files_in(
    workspace: Workspace,
    state_root: &Path,
    note_id: &str,
    local_path: &Path,
    baseline: &str,
) -> LocalFiles {
    let files = unconfined_files(state_root.join("state"));
    files
        .state()
        .persist_from_sync(
            &TrackedNoteState::capture(
                note_id.to_owned(),
                workspace,
                local_path,
                baseline,
                Some(1),
            )
            .expect("fixture state should capture"),
            baseline,
        )
        .expect("tracked state should persist");
    files
}

/// Local storage with no configured root, matching the default deployment.
/// Tool tests exercise tools; the path policy has its own tests in `local`.
pub(crate) fn unconfined_files(state_dir: PathBuf) -> LocalFiles {
    LocalFiles::new(state_dir, None)
}

/// A state directory for a test that never inspects it.
pub(crate) fn scratch_files() -> LocalFiles {
    unconfined_files(std::env::temp_dir().join("hackmd-mcp-test"))
}

#[cfg(test)]
mod tests {
    use std::{
        sync::mpsc,
        time::{Duration, Instant},
    };

    use super::{Scenario, SequenceServer, wait_delay};

    #[tokio::test]
    async fn scenario_declares_request_contract_response_headers_and_delay() {
        let server = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/me",
            200,
            r#"{"id":"user","name":"User","userPath":"user"}"#,
        )
        .expect_header("authorization", "Bearer fixture-token")
        .expect_body("an empty GET body", str::is_empty)
        .response_header("X-Fixture", "present")
        .delay(Duration::from_millis(1))]);

        let profile = server
            .client()
            .get_me()
            .await
            .expect("declared response should deserialize");

        assert_eq!(profile.id, "user");
        server.finish();
    }

    #[test]
    fn declared_delay_cannot_outlive_the_overall_deadline() {
        let (_shutdown, receiver) = mpsc::channel();
        let started = Instant::now();

        assert!(!wait_delay(
            Duration::from_secs(1),
            &receiver,
            started + Duration::from_millis(2)
        ));
    }
}
