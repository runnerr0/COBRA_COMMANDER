//! Find WLED nodes on the local network.
//!
//! WLED advertises itself over mDNS as a `_wled._tcp` service.
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

/// The mDNS service type WLED advertises.
const SERVICE_TYPE: &str = "_wled._tcp.local.";

/// How often the browsing worker re-checks for shutdown.
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// A WLED node found on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WledNode {
    /// The name the node advertises.
    pub name: String,
    pub address: Ipv4Addr,
    /// The port of the node's HTTP API.
    pub port: u16,
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
        })
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
pub trait WledBrowser {
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
                let Ok(mut found) = found.lock() else {
                    return;
                };
                match event {
                    ServiceEvent::ServiceResolved(service) => {
                        let Some(node) = WledNode::from_service(&service) else {
                            debug!("WLED node {} has no IPv4 address.", service.fullname);
                            continue;
                        };
                        found.insert(service.fullname.clone(), node);
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => {
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

/// Browsing for WLED nodes that starts only once something asks for them, so
/// a show with no WLED fixtures never opens an mDNS socket.
pub enum WledDiscovery {
    Idle,
    Browsing(Box<dyn WledBrowser>),
    /// Browsing could not start; do not retry on every frame.
    Unavailable,
}

impl WledDiscovery {
    /// The nodes currently known, starting to browse if we have not yet.
    pub fn nodes(&mut self) -> Vec<WledNode> {
        if matches!(self, Self::Idle) {
            *self = match MdnsWledBrowser::start() {
                Ok(browser) => Self::Browsing(Box::new(browser)),
                Err(err) => {
                    warn!("WLED discovery is unavailable: {err:#}");
                    Self::Unavailable
                }
            };
        }
        match self {
            Self::Browsing(browser) => browser.nodes(),
            Self::Idle | Self::Unavailable => Vec::new(),
        }
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
        };
        assert_eq!(node.url().unwrap().as_str(), "http://10.0.0.7/");
    }

    /// Advertise a node the way WLED does and find it by browsing.
    #[test]
    fn browser_finds_an_advertised_node() {
        let name = format!("cobra-test-{}", std::process::id());
        let advertiser = ServiceDaemon::new().unwrap();
        let service = ServiceInfo::new(
            SERVICE_TYPE,
            &name,
            &format!("{name}.local."),
            (),
            8080,
            None,
        )
        .unwrap()
        .enable_addr_auto();
        advertiser.register(service).unwrap();

        let browser = MdnsWledBrowser::start().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let node = loop {
            if let Some(node) = browser.nodes().into_iter().find(|n| n.name == name) {
                break node;
            }
            assert!(Instant::now() < deadline, "node was never discovered");
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(node.port, 8080);

        advertiser.shutdown().unwrap();
    }
}
