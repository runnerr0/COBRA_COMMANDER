//! Send state to a WLED node over its WebSocket.
//!
//! WLED takes the same JSON over a WebSocket as over HTTP. The socket stays
//! open, so a change costs one small frame instead of a request and response.
use std::{
    io,
    net::{TcpStream, ToSocketAddrs},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::Url;
use tungstenite::{Message, WebSocket};
use wled_json_api_library::structures::state::State;

/// How long to wait for the node to accept a connection or a frame.
const IO_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a read waits when checking for messages from the node. We only
/// read to discard, so this is as short as a timeout can be.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(1);

/// An open WebSocket to a WLED node's JSON API.
#[derive(Debug)]
pub struct StateSocket {
    socket: WebSocket<TcpStream>,
    /// When we last sent the node anything.
    sent: Instant,
    /// When the node last sent us anything.
    heard: Instant,
    /// Whether the node has ever answered a heartbeat. Firmware that
    /// predates the heartbeat stays silent, and must not be taken for dead.
    answers_heartbeat: bool,
}

/// How long a socket may sit quiet before we ping the node.
const PING_INTERVAL: Duration = Duration::from_secs(1);

/// How long a node that answers heartbeats may go without sending anything
/// before we take it to be gone. A node that loses power closes nothing, and
/// what we send it is accepted by our own side for a long time afterwards.
const SILENCE_LIMIT: Duration = Duration::from_secs(3);

/// What WLED answers a heartbeat with.
const HEARTBEAT_ANSWER: &str = "pong";

/// The text WLED takes as a heartbeat.
const HEARTBEAT: &str = "p";

/// The most messages from the node to discard in one go.
const MAX_DRAINED_MESSAGES: usize = 64;

impl StateSocket {
    /// Open the WebSocket of the node whose HTTP API is at `url`.
    pub fn connect(url: &Url) -> Result<Self> {
        let host = url.host_str().context("the node's URL has no host")?;
        let port = url
            .port_or_known_default()
            .context("the node's URL has no port")?;
        let address = (host, port)
            .to_socket_addrs()
            .context("failed to resolve the node's address")?
            .next()
            .context("the node's address resolved to nothing")?;
        let stream = TcpStream::connect_timeout(&address, IO_TIMEOUT)
            .context("failed to connect to the node")?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        let (socket, _) = tungstenite::client(format!("ws://{host}:{port}/ws"), stream)
            .map_err(|err| anyhow!("WebSocket handshake failed: {err}"))?;
        socket.get_ref().set_read_timeout(Some(DRAIN_TIMEOUT))?;
        Ok(Self {
            socket,
            sent: Instant::now(),
            heard: Instant::now(),
            answers_heartbeat: false,
        })
    }

    /// Send `state` to the node.
    pub fn send(&mut self, state: &State) -> Result<()> {
        let json: String = state.try_into().context("failed to encode the state")?;
        self.sent = Instant::now();
        self.socket
            .send(Message::text(json))
            .context("failed to send the state")?;
        self.drain()
    }

    /// Look after an idle socket: discard what the node has sent, and ping it
    /// if we have sent nothing for a while.
    ///
    /// A node that restarts leaves no trace on a quiet connection. The ping
    /// draws a reset out of it, or goes unanswered by a node that is gone,
    /// so the loss shows up here rather than on the operator's next move.
    /// WLED does not answer WebSocket ping frames; its
    /// own heartbeat is a text message, which it answers with "pong".
    pub fn keep_alive(&mut self) -> Result<()> {
        if self.sent.elapsed() >= PING_INTERVAL {
            self.sent = Instant::now();
            self.socket
                .send(Message::text(HEARTBEAT))
                .context("failed to ping the node")?;
        }
        self.drain()
    }

    /// Discard whatever the node has sent us.
    ///
    /// WLED pushes its state to every client whenever it changes. Unread, that
    /// would back up until the node dropped the connection. Fail if the socket
    /// is no longer usable.
    fn drain(&mut self) -> Result<()> {
        // Bounded so that a node sending without pause cannot keep us here.
        for _ in 0..MAX_DRAINED_MESSAGES {
            match self.socket.read() {
                Ok(Message::Close(_)) => bail!("the node closed the WebSocket"),
                Ok(message) => {
                    self.heard = Instant::now();
                    if let Message::Text(text) = &message
                        && text.as_str() == HEARTBEAT_ANSWER
                    {
                        self.answers_heartbeat = true;
                    }
                }
                Err(tungstenite::Error::Io(err))
                    if matches!(
                        err.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    break;
                }
                Err(err) => return Err(err).context("the WebSocket failed"),
            }
        }
        if self.answers_heartbeat && self.heard.elapsed() > SILENCE_LIMIT {
            bail!("the node has stopped answering");
        }
        Ok(())
    }
}
