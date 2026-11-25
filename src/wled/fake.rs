//! A stand-in for a WLED node, for tests.
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::mpsc::{Receiver, Sender, channel},
    time::Duration,
};

use reqwest::Url;
use wled_json_api_library::structures::state::State;

/// The LED count the fake reports.
pub const LED_COUNT: u16 = 300;

/// The realtime UDP port the fake reports.
pub const UDP_PORT: u16 = 21324;

/// Answers the HTTP requests CobraCommander makes of a WLED node, and reports
/// the states posted to it.
pub struct FakeWled {
    /// The URL of the fake on the loopback interface.
    pub url: Url,
    /// The port the fake listens on, on every interface.
    pub port: u16,
    posted: Receiver<State>,
}

impl FakeWled {
    pub fn start() -> Self {
        let listener = TcpListener::bind("0.0.0.0:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = Url::parse(&format!("http://127.0.0.1:{port}")).unwrap();
        let (send, posted) = channel();
        // Serves until the test process exits.
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                serve(stream, &send);
            }
        });
        Self { url, port, posted }
    }

    /// The next state posted, failing the test if none arrives.
    pub fn next_state(&self) -> State {
        self.posted.recv_timeout(Duration::from_secs(5)).unwrap()
    }
}

/// Answer one request, closing the connection afterwards.
fn serve(stream: TcpStream, posted: &Sender<State>) {
    let mut reader = BufReader::new(&stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).unwrap();
    let mut body_len = 0;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).unwrap();
        if header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            body_len = value.trim().parse().unwrap();
        }
    }
    let mut body = vec![0; body_len];
    reader.read_exact(&mut body).unwrap();

    let response = if request_line.starts_with("POST /json/state") {
        // JSON is a subset of YAML.
        let _ = posted.send(serde_yaml::from_slice(&body).unwrap());
        "{}".to_string()
    } else if request_line.starts_with("GET /json/info") {
        format!(r#"{{"ver":"0.15.0","leds":{{"count":{LED_COUNT}}},"udpport":{UDP_PORT}}}"#)
    } else {
        "{}".to_string()
    };
    write!(
        &stream,
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
        response.len()
    )
    .unwrap();
}
