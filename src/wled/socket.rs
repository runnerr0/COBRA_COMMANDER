//! Send state to a WLED node over its WebSocket.
//!
//! WLED takes the same JSON over a WebSocket as over HTTP. The socket stays
//! open, so a change costs one small frame instead of a request and response.
use std::{
    io,
    net::{TcpStream, ToSocketAddrs},
    time::Duration,
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
pub struct StateSocket(WebSocket<TcpStream>);

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
        Ok(Self(socket))
    }

    /// Send `state` to the node.
    pub fn send(&mut self, state: &State) -> Result<()> {
        let json: String = state.try_into().context("failed to encode the state")?;
        self.0
            .send(Message::text(json))
            .context("failed to send the state")?;
        self.drain()
    }

    /// Discard whatever the node has sent us.
    ///
    /// WLED pushes its state to every client whenever it changes. Unread, that
    /// would back up until the node dropped the connection. Fail if the socket
    /// is no longer usable.
    pub fn drain(&mut self) -> Result<()> {
        loop {
            match self.0.read() {
                Ok(Message::Close(_)) => bail!("the node closed the WebSocket"),
                Ok(_) => {}
                Err(tungstenite::Error::Io(err))
                    if matches!(
                        err.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(());
                }
                Err(err) => return Err(err).context("the WebSocket failed"),
            }
        }
    }
}
