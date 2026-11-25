//! Find nodes by the mDNS service they advertise.
use std::{
    collections::BTreeMap,
    net::{Ipv4Addr, SocketAddr},
    time::{Duration, Instant},
};

use log::{debug, warn};
use mdns_sd::{Receiver, ResolvedService, ServiceDaemon, ServiceEvent};

use crate::{Error, Mac, NodeConfig, read_config};

/// The mDNS service type WLED advertises.
const SERVICE_TYPE: &str = "_wled._tcp.local.";

/// A node found on the network, as far as its advertisement describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// The name the node advertises. No two nodes on a network share one.
    pub name: String,
    pub address: Ipv4Addr,
    /// The port of the node's HTTP API.
    pub port: u16,
    /// The node's hardware address, if it advertises one.
    pub mac: Option<Mac>,
}

impl Node {
    /// The address of the node's HTTP API.
    pub fn http(&self) -> SocketAddr {
        (self.address, self.port).into()
    }

    /// Ask the node how it is configured; see [`read_config`].
    pub fn read_config(&self, timeout: Duration) -> Result<NodeConfig, Error> {
        read_config(self.http(), timeout)
    }

    fn from_service(service: &ResolvedService) -> Option<Self> {
        // A node with several addresses must resolve to the same one each time.
        let address = service.get_addresses_v4().into_iter().min()?;
        Some(Self {
            name: instance_name(&service.fullname).to_string(),
            address,
            port: service.port,
            mac: service.get_property_val_str("mac").and_then(Mac::parse),
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

/// A change in which nodes are on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A node has appeared, or one already found has changed its address.
    Found(Node),
    /// The node of this name has withdrawn its advertisement. A node that
    /// loses power says nothing, and is only noticed missing much later.
    Lost { name: String },
}

/// Browses for nodes for as long as it lives.
pub struct Browser {
    daemon: ServiceDaemon,
    events: Receiver<ServiceEvent>,
}

impl Browser {
    /// Start browsing. Nodes already on the network are reported as found
    /// over the next second or so, as they answer.
    pub fn start() -> Result<Self, Error> {
        let mdns_error = |err: mdns_sd::Error| Error::Mdns(err.to_string());
        let daemon = ServiceDaemon::new().map_err(mdns_error)?;
        let events = daemon.browse(SERVICE_TYPE).map_err(mdns_error)?;
        Ok(Self { daemon, events })
    }

    /// Wait up to `timeout` for a node to appear or leave.
    ///
    /// Return `Ok(None)` if none did, and an error once browsing has stopped
    /// and no more will.
    pub fn next_event(&self, timeout: Duration) -> Result<Option<Event>, Error> {
        let deadline = Instant::now() + timeout;
        loop {
            let event = match self.events.recv_deadline(deadline) {
                Ok(event) => event,
                Err(_) if self.events.is_disconnected() => {
                    return Err(Error::Mdns("browsing has stopped".to_string()));
                }
                Err(_) => return Ok(None),
            };
            match event {
                ServiceEvent::ServiceResolved(service) => match Node::from_service(&service) {
                    Some(node) => return Ok(Some(Event::Found(node))),
                    None => debug!("WLED node {} has no IPv4 address.", service.fullname),
                },
                ServiceEvent::ServiceRemoved(_, fullname) => {
                    return Ok(Some(Event::Lost {
                        name: instance_name(&fullname).to_string(),
                    }));
                }
                // Progress reports of the search itself.
                _ => {}
            }
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        if let Err(err) = self.daemon.shutdown() {
            warn!("failed to stop browsing for WLED nodes: {err}");
        }
    }
}

/// A node found by [`scan`], with its configuration or why it could not be
/// read.
#[derive(Debug)]
pub struct Found {
    pub node: Node,
    pub config: Result<NodeConfig, Error>,
}

/// Browse for `window`, then ask every node found how it is configured,
/// waiting at most `http_timeout` for each answer. The nodes come back in
/// order of name.
///
/// This blocks for the window and then for as long as the nodes take to
/// answer, one after another, so it belongs on a thread that can wait.
pub fn scan(window: Duration, http_timeout: Duration) -> Result<Vec<Found>, Error> {
    let browser = Browser::start()?;
    let deadline = Instant::now() + window;
    let mut nodes = BTreeMap::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match browser.next_event(remaining)? {
            Some(Event::Found(node)) => {
                nodes.insert(node.name.clone(), node);
            }
            Some(Event::Lost { name }) => {
                nodes.remove(&name);
            }
            None => break,
        }
    }
    Ok(nodes
        .into_values()
        .map(|node| Found {
            config: node.read_config(http_timeout),
            node,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use mdns_sd::ServiceInfo;

    use super::*;
    use crate::config::tests::fake_node;

    #[test]
    fn instance_name_is_the_part_before_the_service_type() {
        assert_eq!(
            instance_name("wled-kitchen._wled._tcp.local."),
            "wled-kitchen"
        );
        assert_eq!(instance_name("something else"), "something else");
    }

    /// Advertise a node the way WLED does. Withdraw it when dropped, or the
    /// name lingers on the network for anything else that browses.
    struct Advertised {
        daemon: ServiceDaemon,
        name: String,
    }

    impl Advertised {
        fn new(label: &str, port: u16) -> Self {
            let name = format!("wled-discovery-test-{label}-{}", std::process::id());
            let daemon = ServiceDaemon::new().unwrap();
            let service = ServiceInfo::new(
                SERVICE_TYPE,
                &name,
                &format!("{name}.local."),
                (),
                port,
                &[("mac", "a842e38d9828")][..],
            )
            .unwrap()
            .enable_addr_auto();
            daemon.register(service).unwrap();
            Self { daemon, name }
        }
    }

    impl Drop for Advertised {
        fn drop(&mut self) {
            if let Ok(withdrawn) = self
                .daemon
                .unregister(&format!("{}.{SERVICE_TYPE}", self.name))
            {
                let _ = withdrawn.recv_timeout(Duration::from_secs(2));
            }
            let _ = self.daemon.shutdown();
        }
    }

    /// Wait for the browser to find the node called `name`.
    fn find(browser: &Browser, name: &str) -> Node {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            assert!(Instant::now() < deadline, "node was never found");
            match browser.next_event(Duration::from_millis(100)).unwrap() {
                Some(Event::Found(node)) if node.name == name => return node,
                _ => {}
            }
        }
    }

    #[test]
    fn browser_finds_an_advertised_node_and_sees_it_leave() {
        let advertised = Advertised::new("browse", 8123);
        let name = advertised.name.clone();
        let browser = Browser::start().unwrap();

        let node = find(&browser, &name);
        assert_eq!(node.port, 8123);
        assert_eq!(node.mac, Mac::parse("a842e38d9828"));

        drop(advertised);
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            assert!(Instant::now() < deadline, "node was never seen to leave");
            match browser.next_event(Duration::from_millis(100)).unwrap() {
                Some(Event::Lost { name: lost }) if lost == name => break,
                _ => {}
            }
        }
    }

    /// The whole of it: a node advertised on the network is found and its
    /// configuration read from its HTTP API.
    #[test]
    fn scan_finds_a_node_and_reads_its_config() {
        let http = fake_node("200 OK");
        let advertised = Advertised::new("scan", http.port());

        // Other WLED nodes on the network are found too; look for ours.
        let deadline = Instant::now() + Duration::from_secs(20);
        let found = loop {
            assert!(Instant::now() < deadline, "node was never found");
            let found = scan(Duration::from_secs(1), Duration::from_secs(5))
                .unwrap()
                .into_iter()
                .find(|found| found.node.name == advertised.name);
            if let Some(found) = found {
                break found;
            }
        };
        assert_eq!(found.node.port, http.port());
        let config = found.config.unwrap();
        assert_eq!(config.name, "Stage Left");
        assert_eq!(config.led_count, 300);
    }
}
