//! Read how a node is configured, over its HTTP API.
use std::{net::SocketAddr, time::Duration};

use serde::{Deserialize, de::DeserializeOwned};

use crate::{Error, Mac};

/// The UDP port Art-Net is sent to.
pub const ARTNET_PORT: u16 = 6454;

/// The UDP port E1.31 (sACN) is sent to.
pub const E131_PORT: u16 = 5568;

/// How a node is set up, as far as driving its LEDs from outside goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeConfig {
    /// The name the node was given in its settings.
    pub name: String,
    /// The firmware version.
    pub version: String,
    pub mac: Option<Mac>,
    /// How many LEDs the node drives in all.
    pub led_count: u16,
    /// Whether any of the LEDs has a white channel.
    pub white_channel: bool,
    /// The LED outputs wired to the node, in the order it lists them.
    pub outputs: Vec<LedOutput>,
    /// How the node takes pixel data over the network, if it said.
    pub realtime: Option<Realtime>,
}

/// One run of LEDs wired to a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedOutput {
    /// Index of the run's first LED among all the node's LEDs.
    pub start: u16,
    /// How many LEDs are in the run.
    pub len: u16,
    /// WLED's number for the kind of LED.
    pub led_type: u8,
    /// Whether the run is wired from its far end.
    pub reversed: bool,
    /// How many LEDs at the start of the run are wired but left dark.
    pub skipped: u8,
}

/// How a node takes Art-Net or E1.31 data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Realtime {
    /// Whether the node takes realtime data at all.
    pub enabled: bool,
    /// The UDP port the node listens on. The node tells Art-Net from E1.31 by
    /// what arrives, but it listens on this one port, so the port decides
    /// which of the two can reach it.
    pub port: u16,
    pub multicast: bool,
    /// How long the node keeps showing realtime data after it stops.
    pub timeout: Duration,
    /// The first universe the node takes.
    pub universe: u16,
    /// The DMX channel its first LED starts on, counting from 1.
    pub dmx_address: u16,
    pub mode: DmxMode,
}

impl Realtime {
    /// Whether Art-Net sent to its usual port drives this node's LEDs.
    pub fn takes_artnet(&self) -> bool {
        self.enabled && self.port == ARTNET_PORT && self.mode != DmxMode::Disabled
    }

    /// Whether E1.31 sent to its usual port drives this node's LEDs.
    pub fn takes_e131(&self) -> bool {
        self.enabled && self.port == E131_PORT && self.mode != DmxMode::Disabled
    }
}

/// What a node does with the DMX channels it is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmxMode {
    Disabled,
    /// Three channels colour every LED alike.
    SingleRgb,
    /// As [`Self::SingleRgb`], after a dimmer channel.
    SingleDrgb,
    /// Channels work the node's own effect controls.
    Effect,
    /// Three channels for each LED.
    MultipleRgb,
    /// As [`Self::MultipleRgb`], after one dimmer channel in the first
    /// universe.
    MultipleDrgb,
    /// Four channels for each LED, white last.
    MultipleRgbw,
    /// As [`Self::Effect`], with a white channel.
    EffectWhite,
    /// As [`Self::Effect`], for each of the node's segments.
    EffectSegment,
    /// As [`Self::EffectSegment`], with a white channel.
    EffectSegmentWhite,
    /// One channel picks a preset.
    Preset,
    /// A mode this crate does not know, by WLED's number for it.
    Other(u8),
}

impl DmxMode {
    fn from_number(mode: u8) -> Self {
        match mode {
            0 => Self::Disabled,
            1 => Self::SingleRgb,
            2 => Self::SingleDrgb,
            3 => Self::Effect,
            4 => Self::MultipleRgb,
            5 => Self::MultipleDrgb,
            6 => Self::MultipleRgbw,
            7 => Self::EffectWhite,
            8 => Self::EffectSegment,
            9 => Self::EffectSegmentWhite,
            10 => Self::Preset,
            other => Self::Other(other),
        }
    }

    /// How many DMX channels each LED takes, in the modes that address LEDs
    /// one by one.
    pub fn channels_per_led(&self) -> Option<u16> {
        match self {
            Self::MultipleRgb | Self::MultipleDrgb => Some(3),
            Self::MultipleRgbw => Some(4),
            _ => None,
        }
    }

    /// How many LEDs a full universe holds, in the modes that address LEDs
    /// one by one. WLED does not split an LED across universes.
    pub fn leds_per_universe(&self) -> Option<u16> {
        self.channels_per_led().map(|channels| 512 / channels)
    }
}

/// Ask the node whose HTTP API is at `http` how it is configured, waiting at
/// most `timeout` for each of the two answers it takes.
pub fn read_config(http: SocketAddr, timeout: Duration) -> Result<NodeConfig, Error> {
    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|source| Error::Http {
            what: "anything",
            source,
        })?;
    let info: Info = get(&client, http, "/json/info", "its description")?;
    let cfg: Cfg = get(&client, http, "/json/cfg", "its settings")?;
    Ok(NodeConfig::from_parts(info, cfg))
}

fn get<T: DeserializeOwned>(
    client: &reqwest::blocking::Client,
    http: SocketAddr,
    path: &str,
    what: &'static str,
) -> Result<T, Error> {
    let http_error = |source| Error::Http { what, source };
    let response = client
        .get(format!("http://{http}{path}"))
        .send()
        .map_err(http_error)?;
    if !response.status().is_success() {
        return Err(Error::Status {
            what,
            status: response.status().as_u16(),
        });
    }
    response.json().map_err(http_error)
}

impl NodeConfig {
    fn from_parts(info: Info, cfg: Cfg) -> Self {
        let leds = info.leds;
        let hardware = cfg.hw.led;
        Self {
            name: info.name.or(cfg.id.name).unwrap_or_default(),
            version: info.ver.unwrap_or_default(),
            mac: info.mac.as_deref().and_then(Mac::parse),
            led_count: leds.count.or(hardware.total).unwrap_or(0),
            // Newer firmware gives the capabilities of its LEDs as bits, of
            // which the second is a white channel; older gives a flag.
            white_channel: leds
                .lc
                .map(|capabilities| capabilities & 0x02 != 0)
                .or(leds.rgbw)
                .unwrap_or(false),
            outputs: hardware
                .ins
                .into_iter()
                .map(|output| LedOutput {
                    start: output.start,
                    len: output.len,
                    led_type: output.led_type,
                    reversed: output.rev,
                    skipped: output.skip,
                })
                .collect(),
            realtime: cfg.interfaces.live.map(|live| Realtime {
                enabled: live.en,
                port: live.port,
                multicast: live.mc,
                // The node counts its timeout in tenths of a second.
                timeout: Duration::from_millis(u64::from(live.timeout) * 100),
                universe: live.dmx.uni,
                dmx_address: live.dmx.addr,
                mode: DmxMode::from_number(live.dmx.mode),
            }),
        }
    }
}

// What follows is the part of WLED's JSON this crate reads. Everything is
// optional or has a default, since firmware versions differ in what they send.

#[derive(Deserialize, Default)]
struct Info {
    ver: Option<String>,
    name: Option<String>,
    mac: Option<String>,
    #[serde(default)]
    leds: InfoLeds,
}

#[derive(Deserialize, Default)]
struct InfoLeds {
    count: Option<u16>,
    rgbw: Option<bool>,
    lc: Option<u8>,
}

#[derive(Deserialize, Default)]
struct Cfg {
    #[serde(default)]
    id: CfgId,
    #[serde(default)]
    hw: CfgHardware,
    #[serde(default, rename = "if")]
    interfaces: CfgInterfaces,
}

#[derive(Deserialize, Default)]
struct CfgId {
    name: Option<String>,
}

#[derive(Deserialize, Default)]
struct CfgHardware {
    #[serde(default)]
    led: CfgLeds,
}

#[derive(Deserialize, Default)]
struct CfgLeds {
    total: Option<u16>,
    #[serde(default)]
    ins: Vec<CfgOutput>,
}

#[derive(Deserialize)]
struct CfgOutput {
    #[serde(default)]
    start: u16,
    #[serde(default)]
    len: u16,
    #[serde(default, rename = "type")]
    led_type: u8,
    #[serde(default)]
    rev: bool,
    #[serde(default)]
    skip: u8,
}

#[derive(Deserialize, Default)]
struct CfgInterfaces {
    live: Option<CfgLive>,
}

#[derive(Deserialize)]
struct CfgLive {
    #[serde(default)]
    en: bool,
    #[serde(default)]
    port: u16,
    #[serde(default)]
    mc: bool,
    #[serde(default)]
    timeout: u16,
    #[serde(default)]
    dmx: CfgDmx,
}

#[derive(Deserialize, Default)]
struct CfgDmx {
    #[serde(default)]
    uni: u16,
    #[serde(default)]
    addr: u16,
    #[serde(default)]
    mode: u8,
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{
        io::{BufRead, BufReader, Write},
        net::{Ipv4Addr, TcpListener},
    };

    use super::*;

    /// What a WLED 0.14 node answers `/json/info` with, trimmed of the parts
    /// this crate does not read.
    pub const INFO: &str = r#"{"ver":"0.14.0","vid":2310130,"leds":{"count":300,"pwr":0,"fps":5,
        "maxpwr":0,"maxseg":32,"seglc":[3],"lc":3,"rgbw":true,"wv":2,"cct":0},"str":false,
        "name":"Stage Left","udpport":21324,"live":false,"lm":"","lip":"","ws":0,
        "arch":"esp32","uptime":6,"mac":"a842e38d9828","ip":"192.168.1.40"}"#;

    /// The parts of `/json/cfg` this crate reads, with the keys WLED's
    /// `cfg.cpp` writes them under.
    pub const CFG: &str = r#"{"id":{"mdns":"wled-8d9828","name":"Stage Left"},
        "hw":{"led":{"total":300,"rgbwm":255,"ins":[
            {"start":0,"len":180,"pin":[16],"order":0,"rev":false,"skip":0,"type":30},
            {"start":180,"len":120,"pin":[17],"order":0,"rev":true,"skip":1,"type":22}]}},
        "if":{"sync":{"port0":21324},"live":{"en":true,"mso":false,"port":6454,"mc":false,
            "dmx":{"uni":3,"seqskip":false,"e131prio":0,"addr":1,"dss":0,"mode":6},
            "timeout":25,"maxbri":false,"no-gc":true,"offset":0}}}"#;

    fn config(info: &str, cfg: &str) -> NodeConfig {
        NodeConfig::from_parts(
            serde_json::from_str(info).unwrap(),
            serde_json::from_str(cfg).unwrap(),
        )
    }

    #[test]
    fn config_is_read_from_what_a_node_sends() {
        let config = config(INFO, CFG);
        assert_eq!(config.name, "Stage Left");
        assert_eq!(config.version, "0.14.0");
        assert_eq!(config.mac, Mac::parse("a842e38d9828"));
        assert_eq!(config.led_count, 300);
        assert!(config.white_channel);
        assert_eq!(
            config.outputs,
            vec![
                LedOutput {
                    start: 0,
                    len: 180,
                    led_type: 30,
                    reversed: false,
                    skipped: 0,
                },
                LedOutput {
                    start: 180,
                    len: 120,
                    led_type: 22,
                    reversed: true,
                    skipped: 1,
                },
            ]
        );
        assert_eq!(
            config.realtime,
            Some(Realtime {
                enabled: true,
                port: ARTNET_PORT,
                multicast: false,
                timeout: Duration::from_millis(2500),
                universe: 3,
                dmx_address: 1,
                mode: DmxMode::MultipleRgbw,
            })
        );
    }

    /// A node that says little is still described, with what it left out
    /// reading as absent rather than failing the whole description.
    #[test]
    fn config_is_read_from_a_node_that_says_little() {
        let config = config(
            r#"{"leds":{"count":6,"rgbw":false}}"#,
            r#"{"id":{"name":"Old"}}"#,
        );
        assert_eq!(config.name, "Old");
        assert_eq!(config.led_count, 6);
        assert!(!config.white_channel);
        assert_eq!(config.mac, None);
        assert!(config.outputs.is_empty());
        assert_eq!(config.realtime, None);

        // With nothing in its description, the count comes from its settings.
        let config = config_from_settings_alone();
        assert_eq!(config.led_count, 12);
    }

    fn config_from_settings_alone() -> NodeConfig {
        config("{}", r#"{"hw":{"led":{"total":12}}}"#)
    }

    #[test]
    fn white_channel_is_read_from_capabilities_before_the_older_flag() {
        let colour_only = config(r#"{"leds":{"lc":1,"rgbw":true}}"#, "{}");
        assert!(!colour_only.white_channel);
        let with_white = config(r#"{"leds":{"lc":3}}"#, "{}");
        assert!(with_white.white_channel);
    }

    fn realtime(enabled: bool, port: u16, mode: DmxMode) -> Realtime {
        Realtime {
            enabled,
            port,
            multicast: false,
            timeout: Duration::ZERO,
            universe: 1,
            dmx_address: 1,
            mode,
        }
    }

    #[test]
    fn node_takes_the_protocol_whose_port_it_listens_on() {
        let artnet = realtime(true, ARTNET_PORT, DmxMode::MultipleRgb);
        assert!(artnet.takes_artnet());
        assert!(!artnet.takes_e131());

        let e131 = realtime(true, E131_PORT, DmxMode::MultipleRgb);
        assert!(e131.takes_e131());
        assert!(!e131.takes_artnet());

        assert!(!realtime(false, ARTNET_PORT, DmxMode::MultipleRgb).takes_artnet());
        assert!(!realtime(true, ARTNET_PORT, DmxMode::Disabled).takes_artnet());
    }

    #[test]
    fn universe_holds_whole_leds_only() {
        assert_eq!(DmxMode::MultipleRgb.leds_per_universe(), Some(170));
        assert_eq!(DmxMode::MultipleDrgb.leds_per_universe(), Some(170));
        assert_eq!(DmxMode::MultipleRgbw.leds_per_universe(), Some(128));
        assert_eq!(DmxMode::Effect.leds_per_universe(), None);
        assert_eq!(DmxMode::from_number(42), DmxMode::Other(42));
    }

    /// Serve `/json/info` and `/json/cfg` as a node would until the test
    /// process exits, and return its address on the loopback interface. It
    /// listens on every interface, as a node found by browsing is reached
    /// at this machine's address on the network. `status` is what every
    /// request is answered with.
    pub fn fake_node(status: &'static str) -> SocketAddr {
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
        let address: SocketAddr =
            (Ipv4Addr::LOCALHOST, listener.local_addr().unwrap().port()).into();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(&stream);
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    if header.trim().is_empty() {
                        break;
                    }
                }
                let body = if request.starts_with("GET /json/info") {
                    INFO
                } else if request.starts_with("GET /json/cfg") {
                    CFG
                } else {
                    "{}"
                };
                let _ = write!(
                    &stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        address
    }

    #[test]
    fn config_is_read_over_http() {
        let node = fake_node("200 OK");
        let config = read_config(node, Duration::from_secs(5)).unwrap();
        assert_eq!(config.name, "Stage Left");
        assert_eq!(config.led_count, 300);
        assert!(config.realtime.unwrap().takes_artnet());
    }

    #[test]
    fn node_that_refuses_is_an_error_not_an_empty_config() {
        let node = fake_node("500 Internal Server Error");
        let err = read_config(node, Duration::from_secs(5)).unwrap_err();
        assert!(
            matches!(err, Error::Status { status: 500, .. }),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn node_that_is_not_there_is_an_error() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let err = read_config(address, Duration::from_secs(5)).unwrap_err();
        assert!(matches!(err, Error::Http { .. }), "unexpected error: {err}");
    }
}
