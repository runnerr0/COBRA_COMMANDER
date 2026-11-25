//! Drive a WLED node's LEDs directly.
//!
//! Pixels go out in WLED's UDP realtime protocol. WLED shows realtime data in
//! place of its own effect for as long as it keeps arriving, so nothing on the
//! node needs to be reconfigured.
use std::{
    collections::HashSet,
    fmt, io,
    net::{Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket},
    sync::{Mutex, OnceLock},
};

use anyhow::Context;
use rust_dmx::{DmxPort, OpenError, WriteError};
use serde::{Deserialize, Serialize};

use super::discovery::WledNode;

/// How many RGB pixels fit in one DMX universe.
pub const PIXELS_PER_UNIVERSE: u16 = 170;

/// The first byte of a DNRGB packet: RGB pixels from a given start index.
const PROTOCOL_DNRGB: u8 = 4;

/// The most LEDs WLED accepts in one DNRGB packet.
const MAX_LEDS_PER_PACKET: usize = 489;

/// A timeout of zero makes WLED leave realtime mode at once.
const RELEASE: [u8; 2] = [PROTOCOL_DNRGB, 0];

/// An RGB colour for one LED.
pub type Pixel = [u8; 3];

/// A connection for showing pixels on a WLED node.
#[derive(Debug)]
pub struct Realtime {
    socket: UdpSocket,
    /// Seconds WLED keeps showing our pixels after the last packet. This
    /// bounds how long a node holds them if we stop without releasing it.
    timeout_secs: u8,
    packet: Vec<u8>,
}

impl Realtime {
    /// Address the realtime port of a WLED node.
    pub fn connect(node: impl ToSocketAddrs, timeout_secs: u8) -> io::Result<Self> {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        socket.connect(node)?;
        Ok(Self {
            socket,
            timeout_secs,
            packet: Vec::new(),
        })
    }

    /// The address of the node.
    pub fn node(&self) -> io::Result<SocketAddr> {
        self.socket.peer_addr()
    }

    /// Show `pixels` on consecutive LEDs, starting from `first_led`.
    pub fn show(
        &mut self,
        first_led: u16,
        pixels: impl IntoIterator<Item = Pixel>,
    ) -> io::Result<()> {
        let mut pixels = pixels.into_iter().peekable();
        let mut first_led = first_led;
        while pixels.peek().is_some() {
            let [first_hi, first_lo] = first_led.to_be_bytes();
            self.packet.clear();
            self.packet
                .extend([PROTOCOL_DNRGB, self.timeout_secs, first_hi, first_lo]);
            let mut led_count: u16 = 0;
            for pixel in pixels.by_ref().take(MAX_LEDS_PER_PACKET) {
                self.packet.extend(pixel);
                led_count += 1;
            }
            self.socket.send(&self.packet)?;
            first_led = first_led.saturating_add(led_count);
        }
        Ok(())
    }

    /// Hand the LEDs back to WLED now rather than after the timeout.
    pub fn release(&self) -> io::Result<()> {
        self.socket.send(&RELEASE).map(|_| ())
    }
}

/// What pixel ports and WLED fixtures need to know of each other.
///
/// A port belongs to a universe and a fixture to the patch, so neither can
/// reach the other directly; they meet here, by the address a node takes
/// realtime data on.
#[derive(Default)]
struct Nodes {
    /// Nodes whose fixture has asked pixel ports to stand down.
    held: HashSet<SocketAddr>,
    /// Nodes a pixel port is currently sending to.
    streaming: HashSet<SocketAddr>,
}

static NODES: OnceLock<Mutex<Nodes>> = OnceLock::new();

fn nodes() -> &'static Mutex<Nodes> {
    NODES.get_or_init(Mutex::default)
}

/// Ask the pixel ports of `node` to stand down, or let them resume.
///
/// While held, a port sends nothing and the node shows its own effect.
pub fn hold_pixels(node: SocketAddr, hold: bool) {
    let Ok(mut nodes) = nodes().lock() else {
        return;
    };
    if hold {
        nodes.held.insert(node);
    } else {
        nodes.held.remove(&node);
    }
}

/// Whether a pixel port is currently sending to `node`.
pub fn pixels_streaming(node: SocketAddr) -> bool {
    nodes()
        .lock()
        .is_ok_and(|nodes| nodes.streaming.contains(&node))
}

/// Record that a port wants to send to `node`, returning whether it may.
fn claim(node: SocketAddr) -> bool {
    let Ok(mut nodes) = nodes().lock() else {
        return true;
    };
    if nodes.held.contains(&node) {
        nodes.streaming.remove(&node);
        false
    } else {
        nodes.streaming.insert(node);
        true
    }
}

/// Record that no port is sending to `node` any longer.
fn unclaim(node: SocketAddr) {
    if let Ok(mut nodes) = nodes().lock() {
        nodes.streaming.remove(&node);
    }
}

/// Seconds a node holds a port's last frame if the port stops without
/// releasing it.
const PORT_TIMEOUT_SECS: u8 = 2;

fn one_led_per_pixel() -> u16 {
    1
}

/// A run of a WLED node's LEDs, as a DMX port.
///
/// Each pixel takes three consecutive DMX channels, red, green then blue,
/// starting from channel 1, and lights `leds_per_pixel` consecutive LEDs.
#[derive(Serialize, Deserialize)]
pub struct WledPixelPort {
    /// The name of the node, for display.
    name: String,
    address: Ipv4Addr,
    udp_port: u16,
    /// Index of the first LED this port drives.
    first_led: u16,
    led_count: u16,
    #[serde(default = "one_led_per_pixel")]
    leds_per_pixel: u16,
    #[serde(skip)]
    realtime: Option<Realtime>,
    /// Whether the node is showing our pixels, as far as we know.
    #[serde(skip)]
    showing: bool,
}

impl WledPixelPort {
    /// The ports that together cover every LED of `node`, each pixel lighting
    /// `leds_per_pixel` LEDs.
    ///
    /// A node that has not reported its LED count gets one full universe.
    pub fn for_node(node: &WledNode, leds_per_pixel: u16) -> Vec<Self> {
        let leds_per_pixel = leds_per_pixel.max(1);
        let leds_per_universe = PIXELS_PER_UNIVERSE.saturating_mul(leds_per_pixel);
        let total = node.led_count.unwrap_or(leds_per_universe);
        (0..total)
            .step_by(usize::from(leds_per_universe))
            .map(|first_led| Self {
                name: node.name.clone(),
                address: node.address,
                udp_port: node.udp_port,
                first_led,
                led_count: (total - first_led).min(leds_per_universe),
                leds_per_pixel,
                realtime: None,
                showing: false,
            })
            .collect()
    }

    /// The address the node takes realtime data on.
    fn node(&self) -> SocketAddr {
        (self.address, self.udp_port).into()
    }
}

impl fmt::Display for WledPixelPort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "WLED {} at {} LEDs {}-{}",
            self.name,
            self.address,
            self.first_led + 1,
            self.first_led + self.led_count,
        )?;
        if self.leds_per_pixel > 1 {
            write!(f, ", {} per pixel", self.leds_per_pixel)?;
        }
        Ok(())
    }
}

#[typetag::serde]
impl DmxPort for WledPixelPort {
    fn open(&mut self) -> Result<(), OpenError> {
        if self.realtime.is_some() {
            return Ok(());
        }
        let realtime = Realtime::connect((self.address, self.udp_port), PORT_TIMEOUT_SECS)
            .context("failed to open a UDP socket to the WLED node")?;
        self.realtime = Some(realtime);
        Ok(())
    }

    fn close(&mut self) {
        if let Some(realtime) = self.realtime.take() {
            let _ = realtime.release();
            self.showing = false;
            unclaim(self.node());
        }
    }

    fn write(&mut self, frame: &[u8]) -> Result<(), WriteError> {
        let node = self.node();
        let Some(realtime) = &mut self.realtime else {
            return Err(WriteError::Disconnected);
        };
        if !claim(node) {
            // The node's fixture wants its own effect shown: let go once,
            // then stay quiet.
            if self.showing {
                self.showing = false;
                realtime
                    .release()
                    .context("failed to release the WLED node")?;
            }
            return Ok(());
        }
        self.showing = true;
        // A frame shorter than the universe leaves the remaining LEDs black.
        let pixels = frame
            .chunks_exact(3)
            .map(|rgb| [rgb[0], rgb[1], rgb[2]])
            .chain(std::iter::repeat([0; 3]))
            .flat_map(|pixel| std::iter::repeat_n(pixel, usize::from(self.leds_per_pixel)))
            .take(usize::from(self.led_count));
        realtime
            .show(self.first_led, pixels)
            .context("failed to send pixels to WLED")?;
        Ok(())
    }
}

impl Drop for WledPixelPort {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::wled::discovery::DEFAULT_UDP_PORT;

    /// A UDP socket standing in for a WLED node, and the node describing it.
    fn listening_node(led_count: Option<u16>) -> (UdpSocket, WledNode) {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let node = WledNode {
            name: "wled-kitchen".to_string(),
            address: Ipv4Addr::LOCALHOST,
            port: 80,
            udp_port: socket.local_addr().unwrap().port(),
            led_count,
        };
        (socket, node)
    }

    fn receive(socket: &UdpSocket) -> Vec<u8> {
        let mut buf = [0; 2048];
        let len = socket.recv(&mut buf).unwrap();
        buf[..len].to_vec()
    }

    fn node(led_count: Option<u16>) -> WledNode {
        WledNode {
            name: "wled-kitchen".to_string(),
            address: Ipv4Addr::new(10, 0, 0, 7),
            port: 80,
            udp_port: DEFAULT_UDP_PORT,
            led_count,
        }
    }

    #[test]
    fn ports_cover_every_led_of_a_node() {
        let names: Vec<String> = WledPixelPort::for_node(&node(Some(300)), 1)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            names,
            [
                "WLED wled-kitchen at 10.0.0.7 LEDs 1-170",
                "WLED wled-kitchen at 10.0.0.7 LEDs 171-300",
            ]
        );
    }

    #[test]
    fn node_of_unknown_size_gets_one_universe() {
        let ports = WledPixelPort::for_node(&node(None), 1);
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].led_count, PIXELS_PER_UNIVERSE);
    }

    #[test]
    fn wider_pixels_cover_more_leds_per_port() {
        let names: Vec<String> = WledPixelPort::for_node(&node(Some(300)), 2)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            names,
            ["WLED wled-kitchen at 10.0.0.7 LEDs 1-300, 2 per pixel"]
        );
    }

    #[test]
    fn wider_pixel_lights_each_of_its_leds() {
        let (socket, node) = listening_node(Some(5));
        let mut ports = WledPixelPort::for_node(&node, 2);
        let port = &mut ports[0];
        port.open().unwrap();
        port.write(&[1, 2, 3, 4, 5, 6, 7, 8, 9]).unwrap();

        // Two LEDs for each of the first two pixels, one for the third.
        assert_eq!(
            receive(&socket),
            [4, 2, 0, 0, 1, 2, 3, 1, 2, 3, 4, 5, 6, 4, 5, 6, 7, 8, 9]
        );
    }

    #[test]
    fn long_run_is_split_across_packets() {
        let (socket, _) = listening_node(None);
        let mut realtime = Realtime::connect(socket.local_addr().unwrap(), 1).unwrap();
        realtime.show(0, std::iter::repeat_n([7; 3], 500)).unwrap();

        let first = receive(&socket);
        assert_eq!(first[..4], [4, 1, 0, 0]);
        assert_eq!(first.len(), 4 + 489 * 3);
        // The second packet picks up at LED 489.
        let second = receive(&socket);
        assert_eq!(second[..4], [4, 1, 1, 233]);
        assert_eq!(second.len(), 4 + 11 * 3);
    }

    #[test]
    fn write_sends_the_frame_as_dnrgb() {
        let (socket, node) = listening_node(Some(2));
        let mut ports = WledPixelPort::for_node(&node, 1);
        let port = &mut ports[0];
        port.open().unwrap();
        port.write(&[1, 2, 3, 4, 5, 6, 7, 8, 9]).unwrap();

        // Protocol, timeout, start index, then only this port's two LEDs.
        assert_eq!(receive(&socket), [4, 2, 0, 0, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn second_port_starts_at_its_first_led() {
        let (socket, node) = listening_node(Some(171));
        let mut ports = WledPixelPort::for_node(&node, 1);
        let port = &mut ports[1];
        port.open().unwrap();
        port.write(&[9, 8, 7]).unwrap();

        assert_eq!(receive(&socket), [4, 2, 0, 170, 9, 8, 7]);
    }

    #[test]
    fn short_frame_is_padded_with_black() {
        let (socket, node) = listening_node(Some(2));
        let mut ports = WledPixelPort::for_node(&node, 1);
        let port = &mut ports[0];
        port.open().unwrap();
        port.write(&[1, 2, 3]).unwrap();

        assert_eq!(receive(&socket), [4, 2, 0, 0, 1, 2, 3, 0, 0, 0]);
    }

    #[test]
    fn closing_releases_the_node() {
        let (socket, node) = listening_node(Some(2));
        let mut ports = WledPixelPort::for_node(&node, 1);
        let port = &mut ports[0];
        port.open().unwrap();
        port.close();

        assert_eq!(receive(&socket), RELEASE);
    }

    #[test]
    fn held_port_releases_the_node_once_then_stays_quiet() {
        let (socket, node) = listening_node(Some(1));
        let mut ports = WledPixelPort::for_node(&node, 1);
        let port = &mut ports[0];
        port.open().unwrap();
        port.write(&[1, 2, 3]).unwrap();
        assert_eq!(receive(&socket), [4, 2, 0, 0, 1, 2, 3]);
        assert!(pixels_streaming(port.node()));

        hold_pixels(port.node(), true);
        port.write(&[1, 2, 3]).unwrap();
        port.write(&[1, 2, 3]).unwrap();
        assert_eq!(receive(&socket), RELEASE);
        assert!(!pixels_streaming(port.node()));

        // Had the second held write sent anything, it would arrive before this.
        hold_pixels(port.node(), false);
        port.write(&[4, 5, 6]).unwrap();
        assert_eq!(receive(&socket), [4, 2, 0, 0, 4, 5, 6]);
    }

    #[test]
    fn write_fails_until_opened() {
        let mut ports = WledPixelPort::for_node(&node(Some(2)), 1);
        assert!(matches!(
            ports[0].write(&[1, 2, 3]),
            Err(WriteError::Disconnected)
        ));
    }

    /// A port saved in a show file comes back closed and reopens.
    #[test]
    fn port_survives_a_round_trip_through_a_show_file() {
        let (socket, node) = listening_node(Some(2));
        let port: Box<dyn DmxPort> = Box::new(WledPixelPort::for_node(&node, 1).remove(0));
        let saved = serde_yaml::to_string(&port).unwrap();
        let mut loaded: Box<dyn DmxPort> = serde_yaml::from_str(&saved).unwrap();

        assert_eq!(loaded.to_string(), port.to_string());
        loaded.open().unwrap();
        loaded.write(&[1, 2, 3, 4, 5, 6]).unwrap();
        assert_eq!(receive(&socket), [4, 2, 0, 0, 1, 2, 3, 4, 5, 6]);
    }
}
