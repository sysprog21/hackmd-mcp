use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::mpsc::{self, Receiver},
    thread::{self, JoinHandle},
    time::Duration,
};

pub(crate) struct SequenceServer {
    pub(crate) api_url: String,
    requests: Receiver<Vec<String>>,
    thread: JoinHandle<()>,
}

impl SequenceServer {
    pub(crate) fn spawn<const N: usize>(responses: [(u16, &'static str); N]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("fixture should bind");
        let address = listener.local_addr().expect("fixture address should exist");
        let (sender, requests) = mpsc::channel();
        let thread = thread::spawn(move || {
            let mut captured = Vec::with_capacity(N);
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().expect("request should connect");
                captured.push(read_request(&mut stream));
                let response = format!(
                    "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("response should write");
            }
            sender.send(captured).expect("requests should send");
        });
        Self {
            api_url: format!("http://{address}/v1"),
            requests,
            thread,
        }
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
            return String::from_utf8(request).expect("fixture request should be UTF-8");
        }
    }
}
