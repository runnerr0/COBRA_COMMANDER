//! Drive a WLED node's LEDs directly, as a DMX port.
//!
//! Each frame goes out as one packet of WLED's UDP realtime protocol. WLED
//! shows realtime data in place of its own effect for as long as it keeps
//! arriving, so nothing on the node needs to be reconfigured.
use std::{
    fmt,
    net::{Ipv4Addr, UdpSocket},
};

use anyhow::Context;
use rust_dmx::{DmxPort, OpenError, WriteError};
use serde::{Deserialize, Serialize};

use super::discovery::WledNode;

/// How many RGB pixels fit in one DMX universe.
pub const PIXELS_PER_UNIVERSE: u16 = 170;

/// DMX channels per pixel.
const CHANNELS_PER_PIXEL: usize = 3;

/// The first byte of a DNRGB packet: RGB pixels from a given start index.
const PROTOCOL_DNRGB: u8 = 4;

/// Seconds WLED keeps showing realtime data after the last packet. This
/// bounds how long a node holds its last frame if we stop without saying so.
const TIMEOUT_SECS: u8 = 2;

/// A timeout of zero makes WLED leave realtime mode at once.
const RELEASE: [u8; 2] = [PROTOCOL_DNRGB, 0];

/// A run of up to [`PIXELS_PER_UNIVERSE`] LEDs on a WLED node.
///
/// Each LED takes three consecutive DMX channels, red, green then blue,
/// starting from channel 1.
#[derive(Serialize, Deserialize)]
pub struct WledPixelPort {
    /// The name of the node, for display.
    name: String,
    address: Ipv4Addr,
    udp_port: u16,
    /// Index of the first LED this port drives.
    first_led: u16,
    led_count: u16,
    #[serde(skip)]
    socket: Option<UdpSocket>,
    #[serde(skip)]
    packet: Vec<u8>,
}

impl WledPixelPort {
    /// The ports that together cover every LED of `node`.
    ///
    /// A node that has not reported its LED count gets one full universe.
    pub fn for_node(node: &WledNode) -> Vec<Self> {
        let total = node.led_count.unwrap_or(PIXELS_PER_UNIVERSE);
        (0..total)
            .step_by(usize::from(PIXELS_PER_UNIVERSE))
            .map(|first_led| Self {
                name: node.name.clone(),
                address: node.address,
                udp_port: node.udp_port,
                first_led,
                led_count: (total - first_led).min(PIXELS_PER_UNIVERSE),
                socket: None,
                packet: Vec::new(),
            })
            .collect()
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
        )
    }
}

#[typetag::serde]
impl DmxPort for WledPixelPort {
    fn open(&mut self) -> Result<(), OpenError> {
        if self.socket.is_some() {
            return Ok(());
        }
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .context("failed to bind a UDP socket for WLED")?;
        socket
            .connect((self.address, self.udp_port))
            .context("failed to address the WLED node")?;
        self.socket = Some(socket);
        Ok(())
    }

    fn close(&mut self) {
        if let Some(socket) = self.socket.take() {
            // Hand the LEDs back to WLED now rather than after the timeout.
            let _ = socket.send(&RELEASE);
        }
    }

    fn write(&mut self, frame: &[u8]) -> Result<(), WriteError> {
        let Some(socket) = &self.socket else {
            return Err(WriteError::Disconnected);
        };
        let [first_hi, first_lo] = self.first_led.to_be_bytes();
        let channels = usize::from(self.led_count) * CHANNELS_PER_PIXEL;
        self.packet.clear();
        self.packet
            .extend([PROTOCOL_DNRGB, TIMEOUT_SECS, first_hi, first_lo]);
        self.packet.extend(
            frame
                .iter()
                .copied()
                .chain(std::iter::repeat(0))
                .take(channels),
        );
        socket
            .send(&self.packet)
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
        let mut buf = [0; 1024];
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
        let names: Vec<String> = WledPixelPort::for_node(&node(Some(300)))
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
        let ports = WledPixelPort::for_node(&node(None));
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].led_count, PIXELS_PER_UNIVERSE);
    }

    #[test]
    fn write_sends_the_frame_as_dnrgb() {
        let (socket, node) = listening_node(Some(2));
        let mut ports = WledPixelPort::for_node(&node);
        let port = &mut ports[0];
        port.open().unwrap();
        port.write(&[1, 2, 3, 4, 5, 6, 7, 8, 9]).unwrap();

        // Protocol, timeout, start index, then only this port's two LEDs.
        assert_eq!(receive(&socket), [4, 2, 0, 0, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn second_port_starts_at_its_first_led() {
        let (socket, node) = listening_node(Some(171));
        let mut ports = WledPixelPort::for_node(&node);
        let port = &mut ports[1];
        port.open().unwrap();
        port.write(&[9, 8, 7]).unwrap();

        assert_eq!(receive(&socket), [4, 2, 0, 170, 9, 8, 7]);
    }

    #[test]
    fn short_frame_is_padded_with_black() {
        let (socket, node) = listening_node(Some(2));
        let mut ports = WledPixelPort::for_node(&node);
        let port = &mut ports[0];
        port.open().unwrap();
        port.write(&[1, 2, 3]).unwrap();

        assert_eq!(receive(&socket), [4, 2, 0, 0, 1, 2, 3, 0, 0, 0]);
    }

    #[test]
    fn closing_releases_the_node() {
        let (socket, node) = listening_node(Some(2));
        let mut ports = WledPixelPort::for_node(&node);
        let port = &mut ports[0];
        port.open().unwrap();
        port.close();

        assert_eq!(receive(&socket), RELEASE);
    }

    #[test]
    fn write_fails_until_opened() {
        let mut ports = WledPixelPort::for_node(&node(Some(2)));
        assert!(matches!(
            ports[0].write(&[1, 2, 3]),
            Err(WriteError::Disconnected)
        ));
    }

    /// A port saved in a show file comes back closed and reopens.
    #[test]
    fn port_survives_a_round_trip_through_a_show_file() {
        let (socket, node) = listening_node(Some(2));
        let port: Box<dyn DmxPort> = Box::new(WledPixelPort::for_node(&node).remove(0));
        let saved = serde_yaml::to_string(&port).unwrap();
        let mut loaded: Box<dyn DmxPort> = serde_yaml::from_str(&saved).unwrap();

        assert_eq!(loaded.to_string(), port.to_string());
        loaded.open().unwrap();
        loaded.write(&[1, 2, 3, 4, 5, 6]).unwrap();
        assert_eq!(receive(&socket), [4, 2, 0, 0, 1, 2, 3, 4, 5, 6]);
    }
}
