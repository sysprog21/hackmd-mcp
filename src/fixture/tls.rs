//! TLS image host with fixed test identities; never used by production builds.

use std::{
    collections::HashMap,
    io::{Read, Write},
    net::SocketAddr,
    sync::{Arc, mpsc},
    thread,
};

use rustls::{
    ServerConfig, ServerConnection, StreamOwned,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
};

use super::{FIXTURE_DEADLINE, accept_next, bind_loopback};
use crate::client::HackmdClient;

const CERTIFICATE: &[u8] = include_bytes!("../../tests/support/tls/cert.der");
const ROOT: &[u8] = include_bytes!("../../tests/support/tls/root.der");
const KEY: &[u8] = include_bytes!("../../tests/support/tls/key.der");

pub(crate) struct TlsServer {
    address: SocketAddr,
    shutdown: mpsc::Sender<()>,
    thread: Option<thread::JoinHandle<Vec<String>>>,
    expected: usize,
}

impl TlsServer {
    /// Raw responses allow declared, chunked, and close-delimited bodies.
    pub(crate) fn spawn(responses: Vec<Vec<u8>>) -> Self {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("TLS versions")
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(CERTIFICATE.to_vec())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(KEY.to_vec())),
            )
            .expect("test certificate and key");
        let config = Arc::new(config);
        let (listener, address) = bind_loopback();
        let (shutdown, receiver) = mpsc::channel();
        let (ready, started) = mpsc::sync_channel(0);
        let expected = responses.len();
        let thread = thread::spawn(move || {
            let deadline = std::time::Instant::now() + FIXTURE_DEADLINE;
            ready.send(()).expect("TLS fixture ready");
            let mut requests = Vec::new();
            for response in responses {
                let Some(socket) = accept_next(&listener, &receiver, deadline) else {
                    break;
                };
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                socket
                    .set_read_timeout(Some(remaining))
                    .expect("TLS read timeout");
                socket
                    .set_write_timeout(Some(remaining))
                    .expect("TLS write timeout");
                let connection =
                    ServerConnection::new(Arc::clone(&config)).expect("TLS connection");
                let mut stream = StreamOwned::new(connection, socket);
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    if stream.read_exact(&mut byte).is_err() {
                        return requests;
                    }
                    request.push(byte[0]);
                    assert!(request.len() < 16 * 1024, "fixture header limit");
                }
                requests.push(String::from_utf8(request).expect("HTTP headers"));

                // A size or signature refusal may close before the response is
                // sent.
                let _ = stream.write_all(&response);
                let _ = stream.flush();
                stream.conn.send_close_notify();
                let _ = stream.flush();
            }
            requests
        });
        started.recv().expect("TLS fixture started");
        Self {
            address,
            shutdown,
            thread: Some(thread),
            expected,
        }
    }

    pub(crate) fn client(&self, client: HackmdClient) -> HackmdClient {
        client.with_tls_images(
            ROOT,
            HashMap::from([
                ("image.test".to_owned(), self.address),
                ("other.test".to_owned(), self.address),
            ]),
        )
    }

    pub(crate) fn untrusted_client(&mut self, client: HackmdClient) -> HackmdClient {
        self.expected = 0;
        client.with_tls_images(
            // The server leaf cannot supply the missing CA trust anchor.
            CERTIFICATE,
            HashMap::from([("image.test".to_owned(), self.address)]),
        )
    }

    pub(crate) fn finish(mut self) -> Vec<String> {
        let _ = self.shutdown.send(());
        let requests = self
            .thread
            .take()
            .expect("TLS thread")
            .join()
            .expect("TLS fixture finished");
        assert_eq!(requests.len(), self.expected, "TLS request count");
        requests
    }
}

impl Drop for TlsServer {
    fn drop(&mut self) {
        let _ = self.shutdown.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
