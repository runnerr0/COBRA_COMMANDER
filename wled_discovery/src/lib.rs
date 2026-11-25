//! Find WLED nodes on the local network and read how each is configured.
//!
//! WLED advertises itself over mDNS as a `_wled._tcp` service, and describes
//! itself over HTTP. This crate does those two things and nothing else: it
//! sends no pixel data and changes nothing on a node.
//!
//! It starts no threads of its own to drive. [`Browser::next_event`] and
//! [`Node::read_config`] block for at most the time they are given, so the
//! caller chooses the thread they run on. [`scan`] is both in one call, for a
//! caller that only wants the answer.
//!
//! ```no_run
//! use std::time::Duration;
//!
//! for found in wled_discovery::scan(Duration::from_secs(2), Duration::from_secs(2))? {
//!     match found.config {
//!         Ok(config) => println!("{}: {} LEDs", found.node.name, config.led_count),
//!         Err(err) => println!("{}: {err}", found.node.name),
//!     }
//! }
//! # Ok::<(), wled_discovery::Error>(())
//! ```
mod browse;
mod config;
mod mac;

pub use browse::{Browser, Event, Found, Node, scan};
pub use config::{ARTNET_PORT, DmxMode, E131_PORT, LedOutput, NodeConfig, Realtime, read_config};
pub use mac::Mac;

/// Why a node could not be found or read.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Browsing for nodes could not start or stopped working.
    #[error("mDNS browsing failed: {0}")]
    Mdns(String),
    /// The node did not answer, or what it sent could not be read.
    #[error("failed to read {what} from the node: {source}")]
    Http {
        /// What was being asked for.
        what: &'static str,
        source: reqwest::Error,
    },
    /// The node answered with something other than success.
    #[error("the node answered {status} when asked for {what}")]
    Status {
        /// What was being asked for.
        what: &'static str,
        status: u16,
    },
}
