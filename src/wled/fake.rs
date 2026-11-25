//! A stand-in for a WLED node, for tests.
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{Ipv4Addr, TcpListener, TcpStream, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc::{Receiver, Sender, channel},
    },
    time::Duration,
};

use reqwest::Url;
use tungstenite::Message;
use wled_json_api_library::structures::state::State;

/// The LED count the fake reports.
pub const LED_COUNT: u16 = 300;

/// The highest ID among the presets the fake reports.
pub const PRESET_COUNT: u8 = 3;

/// How a state reached the fake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Http,
    WebSocket,
}

/// Answers the requests CobraCommander makes of a WLED node, and reports the
/// states posted and the realtime packets sent to it.
pub struct FakeWled {
    /// The URL of the fake on the loopback interface.
    pub url: Url,
    /// The port the fake listens on, on every interface.
    pub port: u16,
    /// The realtime port the fake reports and listens on.
    pub udp_port: u16,
    posted: Receiver<(Transport, State)>,
    realtime: UdpSocket,
    faults: Arc<Faults>,
}

/// The ways a test can make the fake misbehave.
#[derive(Default)]
struct Faults {
    /// The WebSocket connections currently open.
    sockets: Mutex<Vec<TcpStream>>,
    /// How many more requests for the node's info to fail.
    info_failures: AtomicUsize,
    /// How many heartbeats have arrived over WebSockets.
    heartbeats: AtomicUsize,
}

impl FakeWled {
    pub fn start() -> Self {
        Self::start_with(true)
    }

    /// A node built without WebSocket support.
    pub fn start_without_websocket() -> Self {
        Self::start_with(false)
    }

    fn start_with(websocket: bool) -> Self {
        let listener = TcpListener::bind("0.0.0.0:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = Url::parse(&format!("http://127.0.0.1:{port}")).unwrap();
        let realtime = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let udp_port = realtime.local_addr().unwrap().port();
        let (send, posted) = channel();
        let faults = Arc::new(Faults::default());
        let serving_faults = faults.clone();
        // Serves until the test process exits.
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let send = send.clone();
                let faults = serving_faults.clone();
                std::thread::spawn(move || serve(stream, websocket, udp_port, &send, &faults));
            }
        });
        Self {
            url,
            port,
            udp_port,
            posted,
            realtime,
            faults,
        }
    }

    /// Drop every open WebSocket without a closing handshake, as a node does
    /// when it restarts.
    pub fn drop_websockets(&self) {
        for socket in self.faults.sockets.lock().unwrap().drain(..) {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
    }

    /// How many heartbeats have arrived over WebSockets.
    pub fn heartbeats(&self) -> usize {
        self.faults.heartbeats.load(Ordering::SeqCst)
    }

    /// Fail the next `count` requests for the node's info.
    pub fn fail_info_requests(&self, count: usize) {
        self.faults.info_failures.store(count, Ordering::SeqCst);
    }

    /// The next state posted, failing the test if none arrives.
    pub fn next_state(&self) -> State {
        self.next_posted().1
    }

    /// The next state posted and how it arrived, failing the test if none
    /// arrives.
    pub fn next_posted(&self) -> (Transport, State) {
        self.posted.recv_timeout(Duration::from_secs(5)).unwrap()
    }

    /// The next realtime packet sent within `timeout`, if any.
    pub fn next_realtime_packet(&self, timeout: Duration) -> Option<Vec<u8>> {
        self.realtime.set_read_timeout(Some(timeout)).unwrap();
        let mut buf = [0; 2048];
        let len = self.realtime.recv(&mut buf).ok()?;
        Some(buf[..len].to_vec())
    }
}

/// Serve one connection: a WebSocket for as long as it stays open, or a
/// single HTTP request.
fn serve(
    stream: TcpStream,
    websocket: bool,
    udp_port: u16,
    posted: &Sender<(Transport, State)>,
    faults: &Faults,
) {
    let mut start = [0; 7];
    let peeked = stream.peek(&mut start).unwrap_or(0);
    if websocket && start[..peeked] == *b"GET /ws" {
        if let Ok(handle) = stream.try_clone() {
            faults.sockets.lock().unwrap().push(handle);
        }
        serve_websocket(stream, posted, faults);
    } else {
        serve_http(stream, udp_port, posted, faults);
    }
}

fn serve_websocket(stream: TcpStream, posted: &Sender<(Transport, State)>, faults: &Faults) {
    let Ok(mut socket) = tungstenite::accept(stream) else {
        return;
    };
    // WLED greets every client with its state and info.
    let _ = socket.send(Message::text(r#"{"state":{},"info":{}}"#));
    while let Ok(message) = socket.read() {
        if let Message::Text(text) = &message
            && text.as_str() == "p"
        {
            // WLED's heartbeat.
            faults.heartbeats.fetch_add(1, Ordering::SeqCst);
            let _ = socket.send(Message::text("pong"));
            continue;
        }
        if let Message::Text(json) = message {
            // JSON is a subset of YAML.
            let state = serde_yaml::from_str(json.as_str()).unwrap();
            let _ = posted.send((Transport::WebSocket, state));
            let _ = socket.send(Message::text(r#"{"success":true}"#));
        }
    }
}

/// Answer one request, closing the connection afterwards.
fn serve_http(
    stream: TcpStream,
    udp_port: u16,
    posted: &Sender<(Transport, State)>,
    faults: &Faults,
) {
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

    let (status, response) = if request_line.starts_with("POST /json/state") {
        // JSON is a subset of YAML.
        let _ = posted.send((Transport::Http, serde_yaml::from_slice(&body).unwrap()));
        ("200 OK", "{}".to_string())
    } else if request_line.starts_with("GET /json/info")
        && faults
            .info_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
    {
        ("500 Internal Server Error", String::new())
    } else if request_line.starts_with("GET /json/info") {
        let info =
            format!(r#"{{"ver":"0.15.0","leds":{{"count":{LED_COUNT}}},"udpport":{udp_port}}}"#);
        ("200 OK", info)
    } else if request_line.starts_with("GET /presets.json") {
        let presets = r#"{"0":{},"1":{"n":"Warm"},"3":{"n":"Chase"}}"#;
        ("200 OK", presets.to_string())
    } else if request_line.starts_with("GET /ws") {
        ("404 Not Found", String::new())
    } else {
        ("200 OK", "{}".to_string())
    };
    let _ = write!(
        &stream,
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
        response.len()
    );
}
