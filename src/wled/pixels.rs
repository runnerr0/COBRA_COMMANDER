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
    time::{Duration, Instant},
};

use anyhow::Context;
use log::{info, warn};
use rust_dmx::{DmxPort, OpenError, WriteError};
use serde::{Deserialize, Serialize};

use super::{
    WledMac,
    discovery::{self, WledDiscovery, WledNode},
};

/// How many RGB pixels fit in one DMX universe.
pub const PIXELS_PER_UNIVERSE: u16 = 170;

/// How many RGBW pixels fit in one DMX universe.
pub const RGBW_PIXELS_PER_UNIVERSE: u16 = 128;

/// The first byte of a DNRGB packet: RGB pixels from a given start index.
const PROTOCOL_DNRGB: u8 = 4;

/// The first byte of a DNRGBW packet: RGBW pixels from a given start index.
const PROTOCOL_DNRGBW: u8 = 5;

/// The largest realtime packet WLED accepts.
const MAX_PACKET_LEN: usize = 1472;

/// The bytes of a packet that come before its pixels.
const HEADER_LEN: usize = 4;

/// A timeout of zero makes WLED leave realtime mode at once.
const RELEASE: [u8; 2] = [PROTOCOL_DNRGB, 0];

/// An RGB colour for one LED.
pub type Pixel = [u8; 3];

/// An RGBW colour for one LED.
pub type WhitePixel = [u8; 4];

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
        self.send_run(PROTOCOL_DNRGB, first_led, pixels)
    }

    /// Show `pixels` on consecutive LEDs that have a white channel, starting
    /// from `first_led`.
    pub fn show_rgbw(
        &mut self,
        first_led: u16,
        pixels: impl IntoIterator<Item = WhitePixel>,
    ) -> io::Result<()> {
        self.send_run(PROTOCOL_DNRGBW, first_led, pixels)
    }

    /// Send a run of pixels of `N` channels each, in as many packets as it
    /// takes.
    fn send_run<const N: usize>(
        &mut self,
        protocol: u8,
        first_led: u16,
        pixels: impl IntoIterator<Item = [u8; N]>,
    ) -> io::Result<()> {
        let leds_per_packet = ((MAX_PACKET_LEN - HEADER_LEN) / N.max(1)).max(1);
        let mut pixels = pixels.into_iter().peekable();
        let mut first_led = first_led;
        while pixels.peek().is_some() {
            let [first_hi, first_lo] = first_led.to_be_bytes();
            self.packet.clear();
            self.packet
                .extend([protocol, self.timeout_secs, first_hi, first_lo]);
            let mut led_count: u16 = 0;
            for pixel in pixels.by_ref().take(leds_per_packet) {
                self.packet.extend(pixel);
                led_count += 1;
            }
            if led_count == 1 {
                // WLED ignores a packet no longer than a header and one LED.
                // It reads LEDs only while a whole one remains, so a spare
                // byte makes the packet long enough and lights nothing more.
                self.packet.push(0);
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

/// How often a port checks whether its node has been given another address.
const RELOCATE_INTERVAL: Duration = Duration::from_secs(5);

/// How long a port must go without a failed send before it reports one again.
const FAILURE_QUIET_PERIOD: Duration = Duration::from_secs(10);

/// Note a failed send at `now`, returning whether it is the first in a while
/// and so worth reporting.
fn first_of_a_run(failed: &mut Option<Instant>, now: Instant) -> bool {
    let recently =
        failed.is_some_and(|failed| now.saturating_duration_since(failed) < FAILURE_QUIET_PERIOD);
    *failed = Some(now);
    !recently
}

/// The colour of each of `led_count` LEDs, given a DMX frame of pixels of `N`
/// channels that each light `leds_per_pixel` LEDs.
///
/// A frame shorter than the universe leaves the remaining LEDs black.
fn led_colours<const N: usize>(
    frame: &[u8],
    leds_per_pixel: u16,
    led_count: u16,
) -> impl Iterator<Item = [u8; N]> {
    frame
        .chunks_exact(N.max(1))
        .filter_map(|pixel| <[u8; N]>::try_from(pixel).ok())
        .chain(std::iter::repeat([0; N]))
        // A show file edited by hand can say zero, which would yield nothing
        // for ever.
        .flat_map(move |pixel| std::iter::repeat_n(pixel, usize::from(leds_per_pixel.max(1))))
        .take(usize::from(led_count))
}

fn one_led_per_pixel() -> u16 {
    1
}

/// A run of a WLED node's LEDs, as a DMX port.
///
/// Each pixel takes three consecutive DMX channels, red, green then blue,
/// starting from channel 1, and lights `leds_per_pixel` consecutive LEDs. A
/// port with a white channel takes four, white last.
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
    /// Whether each pixel carries a white channel after its blue.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    white: bool,
    /// The node's hardware address. With it the port follows the node when
    /// the network gives it a different IP address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mac: Option<WledMac>,
    /// When the port last checked where its node is.
    #[serde(skip)]
    located: Option<Instant>,
    #[serde(skip)]
    realtime: Option<Realtime>,
    /// Whether the node is showing our pixels, as far as we know.
    #[serde(skip)]
    showing: bool,
    /// When a send to the node last failed.
    #[serde(skip)]
    failed: Option<Instant>,
}

impl WledPixelPort {
    /// The ports that together cover every LED of `node`, each pixel lighting
    /// `leds_per_pixel` LEDs.
    ///
    /// A node that has not reported its LED count gets one full universe.
    pub fn for_node(node: &WledNode, leds_per_pixel: u16) -> Vec<Self> {
        Self::covering(node, leds_per_pixel, false)
    }

    /// As [`Self::for_node`], for LEDs with a white channel: each pixel takes
    /// four DMX channels, so fewer fit in a universe.
    pub fn for_rgbw_node(node: &WledNode, leds_per_pixel: u16) -> Vec<Self> {
        Self::covering(node, leds_per_pixel, true)
    }

    fn covering(node: &WledNode, leds_per_pixel: u16, white: bool) -> Vec<Self> {
        let leds_per_pixel = leds_per_pixel.max(1);
        let pixels_per_universe = if white {
            RGBW_PIXELS_PER_UNIVERSE
        } else {
            PIXELS_PER_UNIVERSE
        };
        let leds_per_universe = pixels_per_universe.saturating_mul(leds_per_pixel);
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
                white,
                mac: node.mac.clone(),
                located: None,
                realtime: None,
                showing: false,
                failed: None,
            })
            .collect()
    }

    /// The address the node takes realtime data on.
    fn node(&self) -> SocketAddr {
        (self.address, self.udp_port).into()
    }

    /// Follow the node to the address `discovery` finds it at, if that is
    /// not where the port has been sending.
    fn relocate(&mut self, discovery: &WledDiscovery) {
        let Some(node) = self.mac.as_ref().and_then(|mac| discovery.find(mac)) else {
            return;
        };
        if (node.address, node.udp_port) == (self.address, self.udp_port) {
            return;
        }
        info!(
            "WLED node {} has moved from {} to {}; following it.",
            self.name, self.address, node.address
        );
        let was_open = self.realtime.is_some();
        // Lets go of the node at the old address, which may be another node
        // by now.
        self.close();
        self.address = node.address;
        self.udp_port = node.udp_port;
        if was_open && let Err(err) = self.connect() {
            warn!(
                "failed to open a UDP socket to WLED node {} at {}: {err}",
                self.name, self.address
            );
        }
    }

    fn connect(&mut self) -> io::Result<()> {
        self.realtime = Some(Realtime::connect(
            (self.address, self.udp_port),
            PORT_TIMEOUT_SECS,
        )?);
        Ok(())
    }
}

impl fmt::Display for WledPixelPort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "WLED {} at {} LEDs {}-{}",
            self.name,
            self.address,
            // A hand-edited show file can hold a run that ends past the last
            // index a u16 can name.
            u32::from(self.first_led) + 1,
            u32::from(self.first_led) + u32::from(self.led_count),
        )?;
        if self.leds_per_pixel > 1 {
            write!(f, ", {} per pixel", self.leds_per_pixel)?;
        }
        if self.white {
            write!(f, ", RGBW")?;
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
        if self.mac.is_some() {
            self.located = Some(Instant::now());
            self.relocate(&discovery::shared());
        }
        self.connect()
            .context("failed to open a UDP socket to the WLED node")?;
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
        if self.mac.is_some()
            && self
                .located
                .is_none_or(|at| at.elapsed() >= RELOCATE_INTERVAL)
        {
            self.located = Some(Instant::now());
            self.relocate(&discovery::shared());
        }
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
        let sent = if self.white {
            let pixels = led_colours(frame, self.leds_per_pixel, self.led_count);
            realtime.show_rgbw(self.first_led, pixels)
        } else {
            let pixels = led_colours(frame, self.leds_per_pixel, self.led_count);
            realtime.show(self.first_led, pixels)
        };
        let Err(err) = sent else {
            return Ok(());
        };
        // A node that is switched off fails every frame, or every other one.
        // The show goes on either way, so say so once rather than forty
        // times a second.
        if !first_of_a_run(&mut self.failed, Instant::now()) {
            return Ok(());
        }
        Err(anyhow::Error::new(err)
            .context(format!(
                "failed to send pixels to WLED node {} at {}; still trying",
                self.name, self.address
            ))
            .into())
    }
}

impl Drop for WledPixelPort {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
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
            preset_count: None,
            rgbw: false,
            mac: None,
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
            preset_count: None,
            rgbw: false,
            mac: None,
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

        assert_eq!(receive(&socket), [4, 2, 0, 170, 9, 8, 7, 0]);
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
        assert_eq!(receive(&socket), [4, 2, 0, 0, 1, 2, 3, 0]);
        assert!(pixels_streaming(port.node()));

        hold_pixels(port.node(), true);
        port.write(&[1, 2, 3]).unwrap();
        port.write(&[1, 2, 3]).unwrap();
        assert_eq!(receive(&socket), RELEASE);
        assert!(!pixels_streaming(port.node()));

        // Had the second held write sent anything, it would arrive before this.
        hold_pixels(port.node(), false);
        port.write(&[4, 5, 6]).unwrap();
        assert_eq!(receive(&socket), [4, 2, 0, 0, 4, 5, 6, 0]);
    }

    /// A port saved with its node's hardware address follows the node to a
    /// new address, and goes on sending without being reopened.
    #[test]
    fn port_follows_its_node_to_a_new_address() {
        let (socket, mut node) = listening_node(Some(2));
        node.mac = WledMac::parse("a842e38d9828");
        let (old_socket, old_node) = listening_node(Some(2));
        let mut saved = node.clone();
        saved.udp_port = old_node.udp_port;
        let mut port = WledPixelPort::for_node(&saved, 1).remove(0);
        // Opened by hand: `open` asks the app's own discovery.
        port.connect().unwrap();
        port.write(&[1, 2, 3, 4, 5, 6]).unwrap();
        assert_eq!(receive(&old_socket), [4, 2, 0, 0, 1, 2, 3, 4, 5, 6]);

        let discovery =
            WledDiscovery::with_browser(discovery::mock::FixedWledBrowser(vec![node.clone()]));
        port.relocate(&discovery);
        // The old address is let go, in case another node has it now.
        assert_eq!(receive(&old_socket), [4, 0]);
        port.write(&[1, 2, 3, 4, 5, 6]).unwrap();
        assert_eq!(receive(&socket), [4, 2, 0, 0, 1, 2, 3, 4, 5, 6]);
    }

    /// A port saved before ports carried a hardware address still loads, and
    /// one that has an address keeps it when saved.
    #[test]
    fn hardware_address_is_saved_only_when_known() {
        let mut node = node(Some(2));
        let port = WledPixelPort::for_node(&node, 1).remove(0);
        assert!(!serde_yaml::to_string(&port).unwrap().contains("mac"));

        node.mac = WledMac::parse("a842e38d9828");
        let port = WledPixelPort::for_node(&node, 1).remove(0);
        let saved = serde_yaml::to_string(&port).unwrap();
        assert!(saved.contains("mac: a842e38d9828"), "{saved}");
        let loaded: WledPixelPort = serde_yaml::from_str(&saved).unwrap();
        assert_eq!(loaded.mac, node.mac);
    }

    #[test]
    fn write_fails_until_opened() {
        let mut ports = WledPixelPort::for_node(&node(Some(2)), 1);
        assert!(matches!(
            ports[0].write(&[1, 2, 3]),
            Err(WriteError::Disconnected)
        ));
    }

    /// A node that is not there refuses every packet. The port reports that
    /// once, not on every frame.
    #[test]
    fn unreachable_node_is_reported_once() {
        let (socket, node) = listening_node(Some(1));
        drop(socket);
        let mut port = WledPixelPort::for_node(&node, 1).remove(0);
        port.open().unwrap();

        let mut failures = 0;
        for _ in 0..20 {
            if port.write(&[1, 2, 3]).is_err() {
                failures += 1;
            }
            // Give the refusal time to come back.
            std::thread::sleep(Duration::from_millis(2));
        }
        // Whether the refusal is seen at all is up to the network stack;
        // what matters is that it is not seen twice.
        assert!(failures <= 1, "{failures} failures reported");
    }

    /// LEDs with a white channel take four DMX channels each, so fewer fit
    /// in a universe.
    #[test]
    fn rgbw_ports_hold_fewer_pixels() {
        let names: Vec<String> = WledPixelPort::for_rgbw_node(&node(Some(300)), 1)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            names,
            [
                "WLED wled-kitchen at 10.0.0.7 LEDs 1-128, RGBW",
                "WLED wled-kitchen at 10.0.0.7 LEDs 129-256, RGBW",
                "WLED wled-kitchen at 10.0.0.7 LEDs 257-300, RGBW",
            ]
        );
    }

    /// An RGBW port sends four channels a pixel in WLED's DNRGBW format.
    #[test]
    fn rgbw_port_sends_the_white_channel() {
        let (socket, node) = listening_node(Some(3));
        let mut port = WledPixelPort::for_rgbw_node(&node, 1).remove(0);
        port.open().unwrap();
        // The third pixel is missing from the frame and stays black.
        port.write(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        assert_eq!(
            receive(&socket),
            [5, 2, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0]
        );
    }

    /// A long RGBW run is split so that no packet exceeds what WLED accepts.
    #[test]
    fn long_rgbw_run_is_split_across_packets() {
        let (socket, node) = listening_node(Some(1));
        let mut realtime = Realtime::connect(socket.local_addr().unwrap(), 1).unwrap();
        realtime
            .show_rgbw(0, std::iter::repeat_n([7; 4], 400))
            .unwrap();
        let first = receive(&socket);
        assert_eq!(first.len(), 4 + 367 * 4);
        assert_eq!(first[..4], [5, 1, 0, 0]);
        let second = receive(&socket);
        assert_eq!(second.len(), 4 + 33 * 4);
        // The second packet starts where the first ended: LED 367.
        assert_eq!(second[..4], [5, 1, 1, 111]);
        drop(node);
    }

    /// A port saved before RGBW ports existed loads as an RGB port, and an
    /// RGBW port keeps its white channel through a show file.
    #[test]
    fn rgbw_port_survives_a_round_trip_through_a_show_file() {
        let port: Box<dyn DmxPort> =
            Box::new(WledPixelPort::for_rgbw_node(&node(Some(2)), 1).remove(0));
        let saved = serde_yaml::to_string(&port).unwrap();
        let loaded: Box<dyn DmxPort> = serde_yaml::from_str(&saved).unwrap();
        assert_eq!(loaded.to_string(), port.to_string());

        let rgb: Box<dyn DmxPort> = Box::new(WledPixelPort::for_node(&node(Some(2)), 1).remove(0));
        assert!(!serde_yaml::to_string(&rgb).unwrap().contains("white"));
    }

    /// WLED drops a DNRGB packet of seven bytes or fewer and a DNRGBW packet
    /// of eight or fewer, which is exactly a header and one LED. A lone LED
    /// is sent with a spare byte so that it lights.
    #[test]
    fn lone_led_packet_is_long_enough_for_wled_to_accept() {
        let (socket, _node) = listening_node(Some(1));
        let mut realtime = Realtime::connect(socket.local_addr().unwrap(), 1).unwrap();
        realtime.show(9, [[1, 2, 3]]).unwrap();
        assert_eq!(receive(&socket), [4, 1, 0, 9, 1, 2, 3, 0]);
        realtime.show_rgbw(9, [[1, 2, 3, 4]]).unwrap();
        assert_eq!(receive(&socket), [5, 1, 0, 9, 1, 2, 3, 4, 0]);

        // The last LED of a run that spills into a second packet.
        realtime.show(0, std::iter::repeat_n([7; 3], 490)).unwrap();
        assert_eq!(receive(&socket).len(), 4 + 489 * 3);
        assert_eq!(receive(&socket), [4, 1, 1, 233, 7, 7, 7, 0]);
    }

    /// A show file edited by hand can give a port no LEDs per pixel. It is
    /// treated as one rather than left to spin.
    #[test]
    fn port_with_no_leds_per_pixel_still_writes() {
        let (socket, node) = listening_node(Some(2));
        let mut port: WledPixelPort = serde_yaml::from_str(&format!(
            "{{name: n, address: 127.0.0.1, udp_port: {}, first_led: 0, led_count: 2, leds_per_pixel: 0}}",
            node.udp_port
        ))
        .unwrap();
        port.open().unwrap();
        port.write(&[1, 2, 3, 4, 5, 6]).unwrap();
        assert_eq!(receive(&socket), [4, 2, 0, 0, 1, 2, 3, 4, 5, 6]);
    }

    /// Failures that keep coming are reported once; one that follows a long
    /// enough spell of good sends is reported afresh.
    #[test]
    fn only_the_first_failure_of_a_run_is_reported() {
        let start = Instant::now();
        let mut failed = None;
        assert!(first_of_a_run(&mut failed, start));
        assert!(!first_of_a_run(&mut failed, start + Duration::from_secs(1)));
        // Still within the quiet period of the failure before it.
        assert!(!first_of_a_run(
            &mut failed,
            start + FAILURE_QUIET_PERIOD + Duration::from_millis(500)
        ));
        assert!(first_of_a_run(
            &mut failed,
            start + FAILURE_QUIET_PERIOD * 3
        ));
    }

    /// A show file edited by hand can name a run that ends past the last LED
    /// index; describing such a port must not overflow.
    #[test]
    fn port_at_the_end_of_the_index_range_can_be_displayed() {
        let port: WledPixelPort = serde_yaml::from_str(
            "{name: far, address: 10.0.0.7, udp_port: 21324, first_led: 65535, led_count: 65535}",
        )
        .unwrap();
        assert_eq!(port.to_string(), "WLED far at 10.0.0.7 LEDs 65536-131070");
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
