//! Use a thread to perform asynchronous communication with a WLED instance.
use log::{debug, error, info, warn};
use std::{
    net::SocketAddr,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};

use reqwest::Url;
use serde::Deserialize;
use wled_json_api_library::{structures::state::State, wled::Wled};

use crate::worker::Shutdown;
use discovery::DEFAULT_UDP_PORT;
use pixels::Realtime;

pub mod discovery;
#[cfg(test)]
pub mod fake;
pub mod pixels;

/// How long to wait between attempts to reach a WLED instance at startup.
const CONNECT_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// How often an idle sender re-checks for shutdown.
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The crossfade WLED applies to each update, in units of 100 ms.
///
/// WLED otherwise fades every change over its configured transition time,
/// which makes a live control lag behind the hand moving it.
const TRANSITION: u8 = 0;

/// Seconds a node stays lit if the end of a flash never reaches it.
const FLASH_TIMEOUT_SECS: u8 = 1;

/// The URL of a WLED node's HTTP API.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct WledUrl(pub Url);

#[derive(Debug)]
pub struct WledController {
    url: Url,
    outbox: Arc<Outbox>,
    /// Set by the sender once the node has told us how to light its LEDs
    /// directly; until then the node cannot flash.
    flasher: Arc<Mutex<Option<Flasher>>>,
    /// Whether pixel ports may drive this node; see [`Self::allow_pixels`].
    pixels_allowed: Arc<AtomicBool>,
}

impl WledController {
    /// Run a thread to handle sending WLED control messages.
    ///
    /// Polls once every 5 seconds for initial configuration until the client
    /// responds.
    pub fn run(url: Url) -> Self {
        let outbox = Arc::new(Outbox::default());
        let flasher = Arc::new(Mutex::new(None));
        let pixels_allowed = Arc::new(AtomicBool::new(true));

        let sender_pixels_allowed = pixels_allowed.clone();
        let sender_flasher = flasher.clone();
        let sender_outbox = outbox.clone();
        let sender_url = url.clone();
        crate::worker::spawn("wled-send", move |shutdown| {
            let Some(mut wled) = initialize(&sender_url, CONNECT_RETRY_INTERVAL, &shutdown) else {
                return;
            };
            match Flasher::connect(&mut wled) {
                Ok(connected) => {
                    if let Ok(mut flasher) = sender_flasher.lock() {
                        // Apply a choice made before we knew the node's
                        // address. Read under the lock so a change made now
                        // is not lost.
                        let allowed = sender_pixels_allowed.load(Ordering::Acquire);
                        pixels::hold_pixels(connected.node, !allowed);
                        *flasher = Some(connected);
                    }
                }
                Err(err) => warn!("WLED at {sender_url} will not flash: {err:#}"),
            }
            while let Some(state) = sender_outbox.take(&shutdown) {
                wled.state = Some(state);
                debug!("Sending WLED state to {sender_url}.");
                if let Err(err) = wled.flush_state() {
                    warn!("failed to send WLED state update: {err}");
                }
            }
            info!("WLED sender thread for {sender_url} shutting down.");
        });

        Self {
            url,
            outbox,
            flasher,
            pixels_allowed,
        }
    }

    /// Let pixel ports drive this node, or make them stand down so the node
    /// shows its own effect.
    pub fn allow_pixels(&self, allowed: bool) {
        self.pixels_allowed.store(allowed, Ordering::Release);
        self.with_flasher(|flasher| {
            pixels::hold_pixels(flasher.node, !allowed);
            Ok(())
        });
    }

    /// Light every LED white at `level`, over whatever the node is showing.
    ///
    /// Call once per frame for as long as the flash lasts, then
    /// [`Self::end_flash`].
    pub fn flash(&self, level: u8) {
        self.with_flasher(|flasher| flasher.flash(level));
    }

    /// Return the node to what it was showing before a flash.
    pub fn end_flash(&self) {
        self.with_flasher(Flasher::end_flash);
    }

    fn with_flasher(&self, action: impl FnOnce(&mut Flasher) -> std::io::Result<()>) {
        let Ok(mut flasher) = self.flasher.lock() else {
            return;
        };
        let Some(flasher) = flasher.as_mut() else {
            return;
        };
        if let Err(err) = action(flasher) {
            debug!("failed to flash WLED at {}: {err}", self.url);
        }
    }

    /// Send a WLED control message.
    pub fn send(&self, msg: WledControlMessage) {
        match msg {
            WledControlMessage::SetState(mut state) => {
                state.tt = Some(TRANSITION);
                if !self.outbox.post(state) {
                    error!(
                        "WLED outbox is unusable, unable to send message to {}.",
                        self.url
                    );
                }
            }
            WledControlMessage::GetEffectMetadata => {
                // TODO
                debug!("fxdata not implemented");
            }
        }
    }
}

impl Drop for WledController {
    fn drop(&mut self) {
        self.outbox.close();
        // A fixture that no longer exists must not keep the ports held.
        self.with_flasher(|flasher| {
            pixels::hold_pixels(flasher.node, false);
            Ok(())
        });
    }
}

/// Lights all of a node's LEDs directly, bypassing its JSON API.
#[derive(Debug)]
struct Flasher {
    realtime: Realtime,
    /// The address the node takes realtime data on.
    node: SocketAddr,
    led_count: u16,
}

impl Flasher {
    /// Ask the node how many LEDs it has and where it takes realtime data.
    fn connect(wled: &mut Wled) -> Result<Self> {
        wled.get_info_from_wled()
            .context("failed to read the node's info")?;
        let info = wled.info.as_ref().context("the node sent no info")?;
        let led_count = info
            .leds
            .as_ref()
            .and_then(|leds| leds.count)
            .context("the node did not report its LED count")?;
        let host = wled.url.host_str().context("the node's URL has no host")?;
        let udp_port = info.udpport.unwrap_or(DEFAULT_UDP_PORT);
        let realtime = Realtime::connect((host, udp_port), FLASH_TIMEOUT_SECS)
            .context("failed to open a UDP socket to the node")?;
        let node = realtime
            .node()
            .context("failed to resolve the node's address")?;
        Ok(Self {
            realtime,
            node,
            led_count,
        })
    }

    /// A pixel port repaints the node every frame, so a flash sent alongside
    /// would only fight it; pixel fixtures flash for themselves.
    fn flash(&mut self, level: u8) -> std::io::Result<()> {
        if pixels::pixels_streaming(self.node) {
            return Ok(());
        }
        let white = std::iter::repeat_n([level; 3], usize::from(self.led_count));
        self.realtime.show(0, white)
    }

    fn end_flash(&mut self) -> std::io::Result<()> {
        if pixels::pixels_streaming(self.node) {
            return Ok(());
        }
        self.realtime.release()
    }
}

/// The state waiting to be sent to a WLED instance.
///
/// A request is in flight for much longer than the interval between control
/// changes, so updates posted in the meantime collapse into one: the sender
/// always transmits the newest state rather than working through a backlog.
#[derive(Debug, Default)]
struct Outbox {
    pending: Mutex<Pending>,
    wake: Condvar,
}

#[derive(Debug, Default)]
struct Pending {
    state: Option<State>,
    /// Set once the controller is dropped; the sender then stops.
    closed: bool,
}

impl Outbox {
    /// Queue a state for sending, superseding any state not yet sent.
    ///
    /// Return false if the outbox can no longer accept states.
    fn post(&self, state: State) -> bool {
        let Ok(mut pending) = self.pending.lock() else {
            return false;
        };
        pending.state = Some(supersede(pending.state.take(), state));
        self.wake.notify_one();
        true
    }

    /// Stop the sender once it finishes any request in flight.
    fn close(&self) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.closed = true;
        }
        self.wake.notify_one();
    }

    /// Block until a state is posted and take it.
    ///
    /// Return None once the outbox is closed or shutdown is triggered.
    fn take(&self, shutdown: &Shutdown) -> Option<State> {
        let mut pending = self.pending.lock().ok()?;
        loop {
            if pending.closed || shutdown.triggered() {
                return None;
            }
            if let Some(state) = pending.state.take() {
                return Some(state);
            }
            (pending, _) = self
                .wake
                .wait_timeout(pending, SHUTDOWN_POLL_INTERVAL)
                .ok()?;
        }
    }
}

/// Fold an unsent state into the newer one that replaces it.
///
/// The newer state wins wherever it sets a field. A field it leaves unset
/// keeps the unsent value, so an update to one control (a preset selection,
/// say) does not discard a different control's change still waiting to go out.
fn supersede(unsent: Option<State>, mut newer: State) -> State {
    let Some(unsent) = unsent else {
        return newer;
    };
    newer.on = newer.on.or(unsent.on);
    newer.bri = newer.bri.or(unsent.bri);
    newer.ps = newer.ps.or(unsent.ps);
    newer.seg = newer.seg.or(unsent.seg);
    newer
}

/// Poll the WLED API until we get a good config back.
///
/// Return None if shutdown is triggered first.
fn initialize(url: &Url, poll_interval: Duration, shutdown: &Shutdown) -> Option<Wled> {
    info!("Initializing WLED for {url}...");
    loop {
        match Wled::try_from_url(url) {
            Ok(wled) => {
                info!("WLED client connection successful.");
                return Some(wled);
            }
            Err(err) => {
                warn!("Failed to initialize WLED at {url}: {err}");
            }
        }
        if shutdown.sleep_or_shutdown(poll_interval) {
            return None;
        }
    }
}

pub enum WledControlMessage {
    SetState(State),
    #[allow(unused)]
    GetEffectMetadata,
}

#[cfg(test)]
mod tests {
    use rust_dmx::DmxPort;

    use super::{
        discovery::WledNode,
        fake::{self, FakeWled},
        pixels::WledPixelPort,
        *,
    };

    /// A flash lights every LED the node reports, then hands them back.
    #[test]
    fn controller_flashes_every_led_then_releases() {
        let fake = FakeWled::start();
        let controller = WledController::run(fake.url.clone());

        // The controller cannot flash until it has heard from the node.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let packet = loop {
            controller.flash(200);
            if let Some(packet) = fake.next_realtime_packet(Duration::from_millis(50)) {
                break packet;
            }
            assert!(std::time::Instant::now() < deadline, "no flash arrived");
        };
        assert_eq!(packet[..4], [4, FLASH_TIMEOUT_SECS, 0, 0]);
        let leds = &packet[4..];
        assert_eq!(leds.len(), usize::from(fake::LED_COUNT) * 3);
        assert!(leds.iter().all(|&channel| channel == 200));

        // Drain any flashes sent while the first was in flight.
        while fake
            .next_realtime_packet(Duration::from_millis(50))
            .is_some()
        {}
        controller.end_flash();
        assert_eq!(
            fake.next_realtime_packet(Duration::from_secs(5)),
            Some(vec![4, 0])
        );
    }

    /// Disallowing pixels makes the node's pixel port let go, even when the
    /// choice is made before the controller has heard from the node.
    #[test]
    fn disallowing_pixels_holds_the_nodes_port() {
        let fake = FakeWled::start();
        let controller = WledController::run(fake.url.clone());
        controller.allow_pixels(false);

        let node = WledNode {
            name: "fake".to_string(),
            address: std::net::Ipv4Addr::LOCALHOST,
            port: fake.port,
            udp_port: fake.udp_port,
            led_count: Some(1),
        };
        let mut port = WledPixelPort::for_node(&node, 1).remove(0);
        port.open().unwrap();

        // The port streams until the controller learns the node's address,
        // then releases the node.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            port.write(&[1, 2, 3]).unwrap();
            if fake.next_realtime_packet(Duration::from_millis(50)) == Some(vec![4, 0]) {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "port never let go");
        }
        port.write(&[1, 2, 3]).unwrap();
        assert_eq!(fake.next_realtime_packet(Duration::from_millis(50)), None);

        controller.allow_pixels(true);
        port.write(&[1, 2, 3]).unwrap();
        assert_eq!(
            fake.next_realtime_packet(Duration::from_secs(5)),
            Some(vec![4, 2, 0, 0, 1, 2, 3])
        );
    }

    #[test]
    fn controller_posts_state_without_a_transition() {
        let fake = FakeWled::start();
        let controller = WledController::run(fake.url.clone());
        controller.send(WledControlMessage::SetState(level(10)));

        let state = fake.next_state();
        assert_eq!(state.bri, Some(10));
        assert_eq!(state.tt, Some(TRANSITION));
    }

    fn level(bri: u8) -> State {
        State {
            bri: Some(bri),
            ..Default::default()
        }
    }

    fn preset(ps: i32) -> State {
        State {
            ps: Some(ps),
            ..Default::default()
        }
    }

    #[test]
    fn take_returns_only_the_newest_state() {
        let outbox = Outbox::default();
        assert!(outbox.post(level(10)));
        assert!(outbox.post(level(20)));

        let state = outbox.take(&Shutdown::default()).unwrap();
        assert_eq!(state.bri, Some(20));
    }

    /// A preset selected just before a fader moves must still reach WLED.
    #[test]
    fn unsent_preset_survives_a_newer_state() {
        let outbox = Outbox::default();
        assert!(outbox.post(preset(3)));
        assert!(outbox.post(level(20)));

        let state = outbox.take(&Shutdown::default()).unwrap();
        assert_eq!(state.ps, Some(3));
        assert_eq!(state.bri, Some(20));
    }

    /// A fader moved just before a preset is selected must still reach WLED.
    #[test]
    fn unsent_level_survives_a_newer_preset() {
        let state = supersede(Some(level(20)), preset(3));
        assert_eq!(state.ps, Some(3));
        assert_eq!(state.bri, Some(20));
    }

    #[test]
    fn newer_preset_replaces_an_unsent_preset() {
        let state = supersede(Some(preset(3)), preset(4));
        assert_eq!(state.ps, Some(4));
    }

    #[test]
    fn sent_preset_is_not_repeated() {
        let outbox = Outbox::default();
        let shutdown = Shutdown::default();
        assert!(outbox.post(preset(3)));
        assert_eq!(outbox.take(&shutdown).unwrap().ps, Some(3));

        assert!(outbox.post(level(20)));
        assert_eq!(outbox.take(&shutdown).unwrap().ps, None);
    }

    #[test]
    fn take_wakes_for_a_state_posted_while_waiting() {
        let outbox = Arc::new(Outbox::default());
        let waiter = {
            let outbox = outbox.clone();
            std::thread::spawn(move || outbox.take(&Shutdown::default()))
        };
        assert!(outbox.post(level(10)));

        let state = waiter.join().unwrap().unwrap();
        assert_eq!(state.bri, Some(10));
    }

    #[test]
    fn take_returns_none_once_closed() {
        let outbox = Outbox::default();
        assert!(outbox.post(level(10)));
        outbox.close();

        assert!(outbox.take(&Shutdown::default()).is_none());
    }
}
