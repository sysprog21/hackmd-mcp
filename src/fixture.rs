use std::{
    fmt::Write as _,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    thread::{self, JoinHandle},
    time::Duration,
};

use crate::{
    client::HackmdClient, config::Config, local::LocalFiles, models::Workspace,
    sync::state::TrackedNoteState,
};

/// The token every fixture client presents. Nothing verifies it; it exists so
/// requests carry an Authorization header the assertions can inspect.
pub(crate) const FIXTURE_TOKEN: &str = "fixture-token";

type FixtureHeaders = &'static [(&'static str, &'static str)];

const EMPTY_HEADERS: FixtureHeaders = &[];

/// One canned response: status, body, and any extra headers. The body is owned
/// so a test can build it at runtime, for example around a secret it then
/// asserts was redacted.
type FixtureResponse = (u16, String, FixtureHeaders);

pub(crate) struct SequenceServer {
    pub(crate) api_url: String,
    requests: Receiver<Vec<String>>,
    thread: JoinHandle<()>,
}

impl SequenceServer {
    pub(crate) fn spawn<const N: usize>(responses: [(u16, &str); N]) -> Self {
        Self::spawn_with_headers(responses.map(|(status, body)| (status, body, EMPTY_HEADERS)))
    }

    /// Replays one response after `delay`, for tests that need the client to be
    /// waiting when something else happens.
    pub(crate) fn spawn_delayed(status: u16, body: &str, delay: Duration) -> Self {
        Self::spawn_inner([(status, body.to_owned(), EMPTY_HEADERS)], delay)
    }

    pub(crate) fn spawn_with_headers<const N: usize>(
        responses: [(u16, &str, FixtureHeaders); N],
    ) -> Self {
        Self::spawn_inner(
            responses.map(|(status, body, headers)| (status, body.to_owned(), headers)),
            Duration::ZERO,
        )
    }

    /// Replays the sequence, then keeps answering with its last response.
    ///
    /// For tests about a condition that never becomes true, where pinning the
    /// exact number of requests would pin `poll_readback`'s schedule instead of
    /// the behavior under test. Such a fixture is never finished; it is dropped
    /// with its thread still waiting.
    pub(crate) fn spawn_repeating<const N: usize>(responses: [(u16, &str); N]) -> Self {
        Self::spawn_with_mode(
            responses.map(|(status, body)| (status, body.to_owned(), EMPTY_HEADERS)),
            Duration::ZERO,
            true,
        )
    }

    fn spawn_inner<const N: usize>(responses: [FixtureResponse; N], delay: Duration) -> Self {
        Self::spawn_with_mode(responses, delay, false)
    }

    fn spawn_with_mode<const N: usize>(
        responses: [FixtureResponse; N],
        delay: Duration,
        repeating: bool,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("fixture should bind");
        let address = listener.local_addr().expect("fixture address should exist");
        let (sender, requests) = mpsc::channel();
        let thread = thread::spawn(move || {
            let mut captured = Vec::with_capacity(N);
            let last = responses.last().cloned();
            for (status, body, headers) in responses {
                let (mut stream, _) = listener.accept().expect("request should connect");
                captured.push(read_request(&mut stream));
                let extra_headers =
                    headers
                        .iter()
                        .fold(String::new(), |mut output, (name, value)| {
                            write!(output, "{name}: {value}\r\n")
                                .expect("writing headers to String cannot fail");
                            output
                        });
                thread::sleep(delay);
                let response = format!(
                    "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("response should write");
            }
            sender.send(captured).expect("requests should send");

            let Some((status, body, _)) = last.filter(|_| repeating) else {
                return;
            };
            let response = format!(
                "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            while let Ok((mut stream, _)) = listener.accept() {
                read_request(&mut stream);
                if stream.write_all(response.as_bytes()).is_err() {
                    return;
                }
            }
        });
        Self {
            api_url: format!("http://{address}/v1"),
            requests,
            thread,
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

    /// A client that fails instead of retrying, so a test sees the first
    /// status.
    pub(crate) fn client_without_retry(&self, token: &str) -> HackmdClient {
        HackmdClient::new(Config::for_loopback_test_no_retry(&self.api_url, token))
            .expect("fixture client should build")
    }

    /// The single request this fixture was expected to serve.
    pub(crate) fn finish_one(self) -> String {
        let mut requests = self.finish();
        assert_eq!(
            requests.len(),
            1,
            "fixture should serve exactly one request"
        );
        requests.remove(0)
    }

    pub(crate) fn finish(self) -> Vec<String> {
        let requests = self
            .requests
            .recv_timeout(Duration::from_secs(2))
            .expect("requests should be captured");
        self.thread.join().expect("fixture thread should finish");
        requests
    }
}

fn read_request(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout should set");
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let count = stream.read(&mut buffer).expect("request should read");
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
    let files = unconfined_files(state_root.join("state"));
    files
        .state()
        .persist_from_sync(
            &TrackedNoteState::capture(
                note_id.to_owned(),
                Workspace::Personal,
                local_path.to_path_buf(),
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
