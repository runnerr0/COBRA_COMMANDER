//! Find WLED nodes on the local network.
//!
//! WLED advertises itself over mDNS as a `_wled._tcp` service. Each node found
//! is then asked over HTTP for the details mDNS does not carry.
use std::{
    collections::BTreeMap,
    net::Ipv4Addr,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};
use log::{debug, warn};
use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent};
use reqwest::Url;
use wled_json_api_library::wled::Wled;

/// The mDNS service type WLED advertises.
const SERVICE_TYPE: &str = "_wled._tcp.local.";

/// How often the browsing worker re-checks for shutdown.
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The UDP port WLED listens on for realtime data unless configured otherwise.
pub const DEFAULT_UDP_PORT: u16 = 21324;

/// A WLED node found on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WledNode {
    /// The name the node advertises.
    pub name: String,
    pub address: Ipv4Addr,
    /// The port of the node's HTTP API.
    pub port: u16,
    /// The port the node listens on for realtime data.
    pub udp_port: u16,
    /// How many LEDs the node drives, if it has told us.
    pub led_count: Option<u16>,
}

impl WledNode {
    /// The URL of this node's HTTP API.
    pub fn url(&self) -> Option<Url> {
        Url::parse(&format!("http://{}:{}", self.address, self.port)).ok()
    }

    fn from_service(service: &ResolvedService) -> Option<Self> {
        // A node with several addresses must resolve to the same one each time.
        let address = service.get_addresses_v4().into_iter().min()?;
        Some(Self {
            name: instance_name(&service.fullname).to_string(),
            address,
            port: service.port,
            udp_port: DEFAULT_UDP_PORT,
            led_count: None,
        })
    }

    /// Ask the node for the details mDNS does not carry.
    ///
    /// The node keeps its defaults if it does not answer.
    fn describe(&mut self) {
        let Some(url) = self.url() else {
            return;
        };
        let info = Wled::try_from_url(&url).and_then(|mut wled| {
            wled.get_info_from_wled()?;
            Ok(wled.info)
        });
        match info {
            Ok(Some(info)) => {
                self.udp_port = info.udpport.unwrap_or(self.udp_port);
                self.led_count = info.leds.and_then(|leds| leds.count);
            }
            Ok(None) => {}
            Err(err) => debug!("WLED node {} did not describe itself: {err}", self.name),
        }
    }
}

/// The instance part of an mDNS service's full name.
fn instance_name(fullname: &str) -> &str {
    fullname
        .strip_suffix(SERVICE_TYPE)
        .and_then(|name| name.strip_suffix('.'))
        .unwrap_or(fullname)
}

/// A source of the WLED nodes currently on the network.
pub trait WledBrowser: Send {
    /// The nodes currently known, ordered by name.
    fn nodes(&self) -> Vec<WledNode>;
}

/// Browse for WLED nodes over mDNS.
pub struct MdnsWledBrowser {
    daemon: ServiceDaemon,
    /// Nodes keyed by their full mDNS service name.
    nodes: Arc<Mutex<BTreeMap<String, WledNode>>>,
}

impl MdnsWledBrowser {
    /// Start browsing. Nodes appear in [`WledBrowser::nodes`] as they resolve.
    pub fn start() -> Result<Self> {
        let daemon = ServiceDaemon::new().context("failed to start the mDNS daemon")?;
        let events = daemon
            .browse(SERVICE_TYPE)
            .context("failed to browse for WLED nodes")?;

        let nodes = Arc::new(Mutex::new(BTreeMap::new()));
        let found = nodes.clone();
        crate::worker::spawn("wled-browse", move |shutdown| {
            while !shutdown.triggered() {
                let Ok(event) = events.recv_timeout(SHUTDOWN_POLL_INTERVAL) else {
                    // The daemon stops when the browser is dropped.
                    if events.is_disconnected() {
                        return;
                    }
                    continue;
                };
                match event {
                    ServiceEvent::ServiceResolved(service) => {
                        let Some(mut node) = WledNode::from_service(&service) else {
                            debug!("WLED node {} has no IPv4 address.", service.fullname);
                            continue;
                        };
                        // Not under the lock: an unreachable node takes
                        // seconds to give up on.
                        node.describe();
                        let Ok(mut found) = found.lock() else {
                            return;
                        };
                        found.insert(service.fullname.clone(), node);
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => {
                        let Ok(mut found) = found.lock() else {
                            return;
                        };
                        found.remove(&fullname);
                    }
                    _ => {}
                }
            }
        });

        Ok(Self { daemon, nodes })
    }
}

impl WledBrowser for MdnsWledBrowser {
    fn nodes(&self) -> Vec<WledNode> {
        let Ok(nodes) = self.nodes.lock() else {
            return Vec::new();
        };
        let mut nodes: Vec<WledNode> = nodes.values().cloned().collect();
        nodes.sort_by(|a, b| a.name.cmp(&b.name));
        nodes
    }
}

impl Drop for MdnsWledBrowser {
    fn drop(&mut self) {
        if let Err(err) = self.daemon.shutdown() {
            warn!("failed to stop the mDNS daemon: {err}");
        }
    }
}

/// A shared handle on the WLED nodes on the network.
///
/// Browsing starts only once something asks for the nodes, so a show with no
/// WLED fixtures never opens an mDNS socket. Clones share one browser.
#[derive(Clone, Default)]
pub struct WledDiscovery(Arc<Mutex<Browsing>>);

#[derive(Default)]
enum Browsing {
    #[default]
    Idle,
    Active(Box<dyn WledBrowser>),
    /// Browsing could not start; do not retry on every frame.
    Unavailable,
}

impl WledDiscovery {
    /// The nodes currently known, starting to browse if we have not yet.
    pub fn nodes(&self) -> Vec<WledNode> {
        let Ok(mut browsing) = self.0.lock() else {
            return Vec::new();
        };
        if matches!(*browsing, Browsing::Idle) {
            *browsing = match MdnsWledBrowser::start() {
                Ok(browser) => Browsing::Active(Box::new(browser)),
                Err(err) => {
                    warn!("WLED discovery is unavailable: {err:#}");
                    Browsing::Unavailable
                }
            };
        }
        match &*browsing {
            Browsing::Active(browser) => browser.nodes(),
            Browsing::Idle | Browsing::Unavailable => Vec::new(),
        }
    }

    /// Discovery that reports the nodes of `browser`.
    #[cfg(test)]
    pub fn with_browser(browser: impl WledBrowser + 'static) -> Self {
        Self(Arc::new(Mutex::new(Browsing::Active(Box::new(browser)))))
    }
}

#[cfg(test)]
pub mod mock {
    use super::*;

    /// A browser that reports a fixed set of nodes.
    pub struct FixedWledBrowser(pub Vec<WledNode>);

    impl WledBrowser for FixedWledBrowser {
        fn nodes(&self) -> Vec<WledNode> {
            self.0.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use mdns_sd::ServiceInfo;

    use super::*;
    use crate::wled::fake::{self, FakeWled};

    #[test]
    fn instance_name_strips_the_service_type() {
        assert_eq!(
            instance_name("wled-kitchen._wled._tcp.local."),
            "wled-kitchen"
        );
    }

    #[test]
    fn instance_name_keeps_a_name_of_another_shape() {
        assert_eq!(instance_name("wled-kitchen"), "wled-kitchen");
    }

    #[test]
    fn node_url_omits_the_default_port() {
        let node = WledNode {
            name: "wled-kitchen".to_string(),
            address: Ipv4Addr::new(10, 0, 0, 7),
            port: 80,
            udp_port: DEFAULT_UDP_PORT,
            led_count: None,
        };
        assert_eq!(node.url().unwrap().as_str(), "http://10.0.0.7/");
    }

    /// Advertise a node the way WLED does, find it by browsing, and learn
    /// its details from its HTTP API.
    #[test]
    fn browser_finds_and_describes_an_advertised_node() {
        let fake = FakeWled::start();
        let name = format!("cobra-test-{}", std::process::id());
        let advertiser = ServiceDaemon::new().unwrap();
        let service = ServiceInfo::new(
            SERVICE_TYPE,
            &name,
            &format!("{name}.local."),
            (),
            fake.port,
            None,
        )
        .unwrap()
        .enable_addr_auto();
        advertiser.register(service).unwrap();

        let browser = MdnsWledBrowser::start().unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let node = loop {
            if let Some(node) = browser.nodes().into_iter().find(|n| n.name == name) {
                break node;
            }
            assert!(Instant::now() < deadline, "node was never discovered");
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(node.port, fake.port);
        assert_eq!(node.udp_port, fake.udp_port);
        assert_eq!(node.led_count, Some(fake::LED_COUNT));

        advertiser.shutdown().unwrap();
    }
}
