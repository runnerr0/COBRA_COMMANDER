//! Use a thread to perform asynchronous communication with a WLED instance.
//!
//! State goes to the node over a WebSocket where the node has one, and over
//! HTTP otherwise.
use log::{debug, error, info, warn};
use std::{
    net::SocketAddr,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};

use reqwest::Url;
use serde::Deserialize;
use wled_json_api_library::{
    structures::state::{Seg, State},
    wled::Wled,
};

use crate::worker::Shutdown;
use discovery::DEFAULT_UDP_PORT;
use pixels::Realtime;
use socket::StateSocket;

pub mod discovery;
#[cfg(test)]
pub mod fake;
pub mod pixels;
mod socket;

/// How long to wait between attempts to reach a WLED instance at startup.
const CONNECT_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// How often an idle sender re-checks for shutdown.
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How long to wait before trying again to open a node's WebSocket.
const SOCKET_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// The least time between states sent to a node.
///
/// A control surface reports a moving fader far faster than a
/// microcontroller can parse JSON and repaint. This is short enough not to
/// be felt.
const MIN_SEND_INTERVAL: Duration = Duration::from_millis(20);

/// How long to wait before telling a node again what it failed to take.
const RESEND_INTERVAL: Duration = Duration::from_secs(5);

/// How much later than before a node must claim to have started for us to
/// take it that it restarted. Uptime comes in whole seconds and takes a
/// moment to fetch.
const RESTART_MARGIN_SECS: i64 = 5;

/// How long a node is given to load a preset before it is told anything
/// that the preset must not override.
const PRESET_LOAD_TIME: Duration = Duration::from_millis(250);

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
        Self::run_retrying_every(url, CONNECT_RETRY_INTERVAL)
    }

    /// As [`Self::run`], waiting `retry_interval` between attempts to reach
    /// the node.
    fn run_retrying_every(url: Url, retry_interval: Duration) -> Self {
        let outbox = Arc::new(Outbox::default());
        let flasher = Arc::new(Mutex::new(None));
        let pixels_allowed = Arc::new(AtomicBool::new(true));

        let sender_pixels_allowed = pixels_allowed.clone();
        let sender_flasher = flasher.clone();
        let sender_outbox = outbox.clone();
        let sender_url = url.clone();
        crate::worker::spawn("wled-send", move |shutdown| {
            let Some(mut wled) = initialize(&sender_url, retry_interval, &shutdown) else {
                return;
            };
            // Until this succeeds the node can neither flash nor have its
            // pixel ports held, so keep trying while there is nothing to send.
            let connect_flasher = |wled: &mut Wled, complain: bool| match Flasher::connect(wled) {
                Ok(connected) => {
                    let Ok(mut flasher) = sender_flasher.lock() else {
                        return false;
                    };
                    // Apply a choice made before we knew the node's
                    // address. Read under the lock so a change made now
                    // is not lost.
                    let allowed = sender_pixels_allowed.load(Ordering::Acquire);
                    pixels::hold_pixels(connected.node, !allowed);
                    *flasher = Some(connected);
                    true
                }
                Err(err) if complain => {
                    warn!("WLED at {sender_url} cannot flash yet, will keep trying: {err:#}");
                    false
                }
                Err(_) => false,
            };
            let mut can_flash = connect_flasher(&mut wled, true);
            let mut flasher_tried = Instant::now();
            let mut link = Link::new(wled);
            loop {
                match sender_outbox.wait(&shutdown) {
                    Wake::State(state) => {
                        debug!("Sending WLED state to {sender_url}.");
                        link.send(state);
                        // Changes made in the meantime collapse into one, so
                        // a fader in motion costs the node a bounded number
                        // of messages and the last one still arrives.
                        if shutdown.sleep_or_shutdown(MIN_SEND_INTERVAL) {
                            break;
                        }
                    }
                    Wake::Idle => {
                        link.keep_up();
                        if !can_flash && flasher_tried.elapsed() >= retry_interval {
                            can_flash = connect_flasher(&mut link.wled, false);
                            flasher_tried = Instant::now();
                        }
                    }
                    Wake::Stop => break,
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
        // A fixture that no longer exists must not keep the ports held, nor
        // leave the node lit by a flash it was in the middle of.
        self.with_flasher(|flasher| {
            pixels::hold_pixels(flasher.node, false);
            flasher.end_flash()
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
    /// Whether the node is showing a flash of ours.
    flashing: bool,
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
            flashing: false,
        })
    }

    /// A pixel port repaints the node every frame, so a flash sent alongside
    /// would only fight it; pixel fixtures flash for themselves.
    fn flash(&mut self, level: u8) -> std::io::Result<()> {
        if pixels::pixels_streaming(self.node) {
            return Ok(());
        }
        let white = std::iter::repeat_n([level; 3], usize::from(self.led_count));
        self.flashing = true;
        self.realtime.show(0, white)
    }

    /// Hand the LEDs back if we lit them. A node we did not flash is left
    /// alone: something else may be sending it realtime data.
    fn end_flash(&mut self) -> std::io::Result<()> {
        if !std::mem::take(&mut self.flashing) || pixels::pixels_streaming(self.node) {
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

    /// Wait a short while for a state to be posted, and take it.
    fn wait(&self, shutdown: &Shutdown) -> Wake {
        let Ok(pending) = self.pending.lock() else {
            return Wake::Stop;
        };
        let Ok((mut pending, _)) =
            self.wake
                .wait_timeout_while(pending, SHUTDOWN_POLL_INTERVAL, |pending| {
                    pending.state.is_none() && !pending.closed
                })
        else {
            return Wake::Stop;
        };
        if pending.closed || shutdown.triggered() {
            return Wake::Stop;
        }
        match pending.state.take() {
            Some(state) => Wake::State(state),
            None => Wake::Idle,
        }
    }
}

/// Why a sender stopped waiting on its [`Outbox`].
enum Wake {
    /// A state is ready to send.
    State(State),
    /// Nothing was posted; a chance to look after the connection.
    Idle,
    /// The outbox is closed or the app is shutting down.
    Stop,
}

/// The connection state is sent over.
struct Link {
    wled: Wled,
    socket: Option<StateSocket>,
    /// When we last tried to open the socket, so that a node without one is
    /// not asked again on every send.
    socket_tried: Option<Instant>,
    /// Whether we have already said that the node has no socket.
    socket_missing: bool,
    /// Everything the node has been told and should still be showing.
    told: Option<State>,
    /// When to tell the node again what it may have missed. None while the
    /// node is up to date as far as we know.
    resend_at: Option<Instant>,
    /// When a node that was sent a preset will have loaded it and can be
    /// told the rest.
    after_preset: Option<Instant>,
    /// Whether the preset in `told` has yet to reach the node.
    preset_unsent: bool,
    /// Whether we have already said that the node is not answering.
    unreachable: bool,
    /// When this link was made; the clock boot times are kept on.
    created: Instant,
    /// When the node started, in seconds since `created`. Negative for a
    /// node that was up before we were.
    booted: Option<i64>,
}

/// What became of a state we tried to send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sent {
    /// The node took it.
    Reached,
    /// The node took it over HTTP after its WebSocket failed. What went into
    /// the socket before that may never have arrived.
    ReachedAfterLosingSocket,
    /// The node did not take it.
    Missed,
}

impl Link {
    fn new(wled: Wled) -> Self {
        let mut link = Self::unacquainted(wled);
        // The node has usually described itself by now, to the flasher.
        link.booted = link.boot_time();
        link
    }

    fn unacquainted(wled: Wled) -> Self {
        Self {
            wled,
            socket: None,
            socket_tried: None,
            socket_missing: false,
            told: None,
            resend_at: None,
            after_preset: None,
            preset_unsent: false,
            unreachable: false,
            created: Instant::now(),
            booted: None,
        }
    }

    /// Send `state` over the WebSocket, or over HTTP if there is none.
    ///
    /// A preset overrides a level that arrives in the same message, which
    /// happens when a preset is picked while a fader is moving. The level is
    /// then sent again once the node has had time to load the preset.
    fn send(&mut self, state: State) {
        let picks_preset = state.ps.is_some();
        let sets_level = state.on.is_some() || state.bri.is_some();
        if picks_preset {
            // A newly picked preset replaces one the node was being sent
            // back to.
            self.after_preset = None;
        }
        let told = remember(self.told.take(), &state);
        self.preset_unsent = picks_preset || (self.preset_unsent && told.ps.is_some());
        self.told = Some(told);
        if self.resend_at.is_some() {
            // The node has missed something, so bring it fully up to date.
            self.resync();
        } else {
            let sent = self.transmit(state);
            self.note(sent);
            if picks_preset && sent != Sent::Missed {
                self.preset_unsent = false;
                self.after_preset = sets_level.then(|| Instant::now() + PRESET_LOAD_TIME);
            }
        }
        self.finish_resync();
    }

    /// Look after the connection between sends: keep the socket open so that
    /// the next state does not wait on it, and bring a node that dropped out
    /// back to what it should be showing.
    fn keep_up(&mut self) {
        self.open_socket();
        if let Some(socket) = &mut self.socket
            && let Err(err) = socket.keep_alive()
        {
            warn!("WLED WebSocket to {} failed: {err:#}", self.wled.url);
            self.socket = None;
            self.resend_at = Some(Instant::now());
        }
        if self.resend_at.is_some_and(|at| at <= Instant::now()) {
            self.resync();
        }
        self.finish_resync();
    }

    /// Tell the node what it should be showing.
    ///
    /// A node that only dropped its connection still has its preset, and
    /// loading it again would show for a moment on stage, so the preset is
    /// sent only to a node that has restarted or has yet to be told of it.
    /// WLED applies the other fields
    /// of a message before it loads the preset the message names, and the
    /// preset then overrides them, so the preset goes alone and the rest
    /// follows once the node has had time to load it.
    fn resync(&mut self) {
        let Some(told) = self.told.clone() else {
            self.resend_at = None;
            return;
        };
        let Some(restarted) = self.node_restarted() else {
            self.missed("the node did not say how long it has been up");
            self.resend_at = Some(Instant::now() + RESEND_INTERVAL);
            return;
        };
        let (first, follows) = if (restarted || self.preset_unsent) && told.ps.is_some() {
            let preset = State {
                ps: told.ps,
                tt: told.tt,
                ..Default::default()
            };
            (preset, true)
        } else {
            (without_preset(told), false)
        };
        if has_content(&first) && self.transmit(first) == Sent::Missed {
            self.resend_at = Some(Instant::now() + RESEND_INTERVAL);
            return;
        }
        self.resend_at = None;
        self.preset_unsent = false;
        self.after_preset = follows.then(|| Instant::now() + PRESET_LOAD_TIME);
    }

    /// Whether the node has restarted since we last asked, or None if it did
    /// not answer.
    fn node_restarted(&mut self) -> Option<bool> {
        self.wled.get_info_from_wled().ok()?;
        let booted = self.boot_time();
        let restarted = match (self.booted, booted) {
            (Some(before), Some(now)) => now > before.saturating_add(RESTART_MARGIN_SECS),
            // A node that does not report its uptime is assumed to have lost
            // everything.
            _ => true,
        };
        self.booted = booted.or(self.booted);
        Some(restarted)
    }

    /// When the node started, by the info it last sent, in seconds on our
    /// own clock.
    fn boot_time(&self) -> Option<i64> {
        let uptime = self.wled.info.as_ref()?.uptime?;
        let now = i64::try_from(self.created.elapsed().as_secs()).ok()?;
        Some(now.saturating_sub(i64::from(uptime)))
    }

    /// Send what was held back for a preset to load, once it has.
    fn finish_resync(&mut self) {
        if self.after_preset.is_none_or(|at| at > Instant::now()) {
            return;
        }
        self.after_preset = None;
        let Some(rest) = self.told.clone().map(without_preset) else {
            return;
        };
        if has_content(&rest) {
            let sent = self.transmit(rest);
            self.note(sent);
        }
    }

    /// Arrange to tell the node again whatever a send may have cost it.
    fn note(&mut self, sent: Sent) {
        match sent {
            Sent::Reached => {}
            Sent::ReachedAfterLosingSocket => self.resend_at = Some(Instant::now()),
            Sent::Missed => self.resend_at = Some(Instant::now() + RESEND_INTERVAL),
        }
    }

    fn transmit(&mut self, state: State) -> Sent {
        self.open_socket();
        let mut lost_socket = false;
        if let Some(socket) = &mut self.socket {
            match socket.send(&state) {
                Ok(()) => {
                    self.reached();
                    return Sent::Reached;
                }
                Err(err) => {
                    warn!("WLED WebSocket to {} failed: {err:#}", self.wled.url);
                    self.socket = None;
                    lost_socket = true;
                }
            }
        }
        self.wled.state = Some(state);
        let failure = match self.wled.flush_state() {
            Ok(response) if response.status().is_success() => None,
            Ok(response) => Some(format!("the node answered {}", response.status())),
            Err(err) => Some(err.to_string()),
        };
        match failure {
            None => {
                self.reached();
                if lost_socket {
                    Sent::ReachedAfterLosingSocket
                } else {
                    Sent::Reached
                }
            }
            Some(why) => {
                self.missed(&why);
                Sent::Missed
            }
        }
    }

    /// The node took what we sent.
    fn reached(&mut self) {
        if self.unreachable {
            info!("WLED at {} is answering again.", self.wled.url);
        }
        self.unreachable = false;
    }

    /// The node did not take what we sent.
    fn missed(&mut self, why: &str) {
        if self.unreachable {
            debug!("WLED at {} is still not answering: {why}", self.wled.url);
        } else {
            warn!(
                "failed to send state to WLED at {}, will keep trying: {why}",
                self.wled.url
            );
        }
        self.unreachable = true;
    }

    fn open_socket(&mut self) {
        if self.socket.is_some()
            || self
                .socket_tried
                .is_some_and(|tried| tried.elapsed() < SOCKET_RETRY_INTERVAL)
        {
            return;
        }
        self.socket_tried = Some(Instant::now());
        match StateSocket::connect(&self.wled.url) {
            Ok(socket) => {
                info!("WLED WebSocket to {} is open.", self.wled.url);
                self.socket = Some(socket);
                self.socket_missing = false;
            }
            Err(err) if !self.socket_missing => {
                info!(
                    "WLED at {} has no WebSocket, using HTTP: {err:#}",
                    self.wled.url
                );
                self.socket_missing = true;
            }
            Err(_) => {}
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
    // An effect, palette or colour picked after a preset replaces it. Sent
    // together, the node would load the preset last and lose the choice.
    if !sets_look(&newer) {
        newer.ps = newer.ps.or(unsent.ps);
    }
    newer.seg = match (newer.seg.take(), unsent.seg) {
        (Some(mut newer_segs), Some(unsent_segs)) => {
            // Fixtures only ever address the first segment.
            if let (Some(newer_seg), Some(unsent_seg)) =
                (newer_segs.first_mut(), unsent_segs.into_iter().next())
            {
                supersede_seg(newer_seg, unsent_seg);
            }
            Some(newer_segs)
        }
        (newer_segs, unsent_segs) => newer_segs.or(unsent_segs),
    };
    newer
}

/// Fold an unsent segment into the newer one that replaces it, field by field
/// as [`supersede`] does.
fn supersede_seg(newer: &mut Seg, unsent: Seg) {
    newer.fx = newer.fx.or(unsent.fx);
    newer.sx = newer.sx.or(unsent.sx);
    newer.ix = newer.ix.or(unsent.ix);
    newer.pal = newer.pal.or(unsent.pal);
    newer.col = newer.col.take().or(unsent.col);
}

/// Add `state` to what a node has been told, giving what to send a node that
/// has lost track: one that restarted, or dropped off the network for a while.
///
/// A preset brings its own effect, palette, colour, speed and size, and
/// overrides any sent along with it, so picking one leaves only the preset
/// and the level to remember. Picking an effect, palette or colour afterwards
/// replaces the preset.
fn remember(told: Option<State>, state: &State) -> State {
    if state.ps.is_some() {
        let told = told.unwrap_or_default();
        return State {
            ps: state.ps,
            tt: state.tt,
            on: state.on.or(told.on),
            bri: state.bri.or(told.bri),
            ..Default::default()
        };
    }
    let mut told = supersede(told, state.clone());
    if sets_look(state) {
        told.ps = None;
    }
    told
}

/// Whether `state` chooses an effect, palette or colour.
fn sets_look(state: &State) -> bool {
    state
        .seg
        .as_ref()
        .and_then(|segs| segs.first())
        .is_some_and(|seg| seg.fx.is_some() || seg.pal.is_some() || seg.col.is_some())
}

/// `state` with no preset selection.
fn without_preset(mut state: State) -> State {
    state.ps = None;
    state
}

/// Whether `state` would change anything on a node.
fn has_content(state: &State) -> bool {
    state.ps.is_some() || state.on.is_some() || state.bri.is_some() || state.seg.is_some()
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
        fake::{self, FakeWled, Transport},
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
            preset_count: None,
            rgbw: false,
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
            Some(vec![4, 2, 0, 0, 1, 2, 3, 0])
        );
    }

    #[test]
    fn controller_sends_state_without_a_transition() {
        let fake = FakeWled::start();
        let controller = WledController::run(fake.url.clone());
        controller.send(WledControlMessage::SetState(level(10)));

        let state = fake.next_state();
        assert_eq!(state.bri, Some(10));
        assert_eq!(state.tt, Some(TRANSITION));
    }

    #[test]
    fn controller_sends_state_over_the_websocket() {
        let fake = FakeWled::start();
        let controller = WledController::run(fake.url.clone());
        controller.send(WledControlMessage::SetState(level(10)));
        assert_eq!(fake.next_posted().0, Transport::WebSocket);

        // The socket stays open for the states that follow.
        controller.send(WledControlMessage::SetState(level(20)));
        let (transport, state) = fake.next_posted();
        assert_eq!(transport, Transport::WebSocket);
        assert_eq!(state.bri, Some(20));
    }

    #[test]
    fn controller_falls_back_to_http_without_a_websocket() {
        let fake = FakeWled::start_without_websocket();
        let controller = WledController::run(fake.url.clone());
        controller.send(WledControlMessage::SetState(level(10)));

        let (transport, state) = fake.next_posted();
        assert_eq!(transport, Transport::Http);
        assert_eq!(state.bri, Some(10));
    }

    /// Wait for the next state, or None once the outbox stops.
    fn take(outbox: &Outbox) -> Option<State> {
        loop {
            match outbox.wait(&Shutdown::default()) {
                Wake::State(state) => return Some(state),
                Wake::Idle => {}
                Wake::Stop => return None,
            }
        }
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

        let state = take(&outbox).unwrap();
        assert_eq!(state.bri, Some(20));
    }

    /// A preset selected just before a fader moves must still reach WLED.
    #[test]
    fn unsent_preset_survives_a_newer_state() {
        let outbox = Outbox::default();
        assert!(outbox.post(preset(3)));
        assert!(outbox.post(level(20)));

        let state = take(&outbox).unwrap();
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

    /// An effect picked just before a fader moved must still reach WLED.
    #[test]
    fn unsent_effect_survives_a_newer_speed() {
        let effect = State {
            seg: Some(vec![Seg {
                fx: Some(108),
                sx: Some(10),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let speed = State {
            seg: Some(vec![Seg {
                sx: Some(200),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let seg = supersede(Some(effect), speed).seg.unwrap().remove(0);
        assert_eq!(seg.fx, Some(108));
        assert_eq!(seg.sx, Some(200));
    }

    #[test]
    fn newer_preset_replaces_an_unsent_preset() {
        let state = supersede(Some(preset(3)), preset(4));
        assert_eq!(state.ps, Some(4));
    }

    #[test]
    fn sent_preset_is_not_repeated() {
        let outbox = Outbox::default();
        assert!(outbox.post(preset(3)));
        assert_eq!(take(&outbox).unwrap().ps, Some(3));

        assert!(outbox.post(level(20)));
        assert_eq!(take(&outbox).unwrap().ps, None);
    }

    #[test]
    fn take_wakes_for_a_state_posted_while_waiting() {
        let outbox = Arc::new(Outbox::default());
        let waiter = {
            let outbox = outbox.clone();
            std::thread::spawn(move || take(&outbox))
        };
        assert!(outbox.post(level(10)));

        let state = waiter.join().unwrap().unwrap();
        assert_eq!(state.bri, Some(10));
    }

    fn look(fx: u16) -> State {
        State {
            seg: Some(vec![Seg {
                fx: Some(fx),
                ..Default::default()
            }]),
            ..Default::default()
        }
    }

    /// A node that restarts comes back showing its boot preset. Without being
    /// asked, the controller tells it again what it should be showing.
    #[test]
    fn node_that_drops_its_socket_is_told_everything_again() {
        let fake = FakeWled::start();
        let controller = WledController::run(fake.url.clone());
        controller.send(WledControlMessage::SetState(look(108)));
        assert_eq!(fake.next_posted().0, Transport::WebSocket);
        controller.send(WledControlMessage::SetState(level(20)));
        assert_eq!(fake.next_state().bri, Some(20));

        // Let the sender finish with that state, so that it is an idle sender
        // that finds the socket gone.
        std::thread::sleep(Duration::from_millis(50));
        fake.drop_websockets();

        let state = fake.next_state();
        assert_eq!(state.bri, Some(20));
        assert_eq!(state.seg.unwrap().remove(0).fx, Some(108));
    }

    /// A fader in motion posts states far faster than a node should be
    /// sent them. Only some go out, and the last one always does.
    #[test]
    fn rapid_changes_are_paced_and_the_last_one_arrives() {
        let fake = FakeWled::start();
        let controller = WledController::run(fake.url.clone());
        controller.send(WledControlMessage::SetState(level(1)));
        assert_eq!(fake.next_state().bri, Some(1));

        let started = Instant::now();
        for bri in 2..=201 {
            controller.send(WledControlMessage::SetState(level(bri)));
            std::thread::sleep(Duration::from_millis(1));
        }
        let mut received = 0;
        loop {
            received += 1;
            if fake.next_state().bri == Some(201) {
                break;
            }
        }
        let most = started.elapsed().as_millis() / MIN_SEND_INTERVAL.as_millis() + 2;
        assert!(
            received <= most,
            "{received} states sent where at most {most} should have been"
        );
    }

    /// A quiet socket is checked with WLED's heartbeat, which is what finds
    /// a node that restarted while nothing was being sent.
    #[test]
    fn idle_socket_is_kept_alive_with_heartbeats() {
        let fake = FakeWled::start();
        let controller = WledController::run(fake.url.clone());
        controller.send(WledControlMessage::SetState(level(10)));
        assert_eq!(fake.next_posted().0, Transport::WebSocket);

        let deadline = Instant::now() + Duration::from_secs(10);
        while fake.heartbeats() == 0 {
            assert!(Instant::now() < deadline, "no heartbeat arrived");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// WLED loads a preset after applying the rest of the message that names
    /// it, and the preset overrides them. A node sent back to a preset is
    /// therefore told the preset first and the level afterwards.
    #[test]
    fn node_sent_back_to_a_preset_is_told_the_level_afterwards() {
        let fake = FakeWled::start();
        let controller = WledController::run(fake.url.clone());
        controller.send(WledControlMessage::SetState(preset(3)));
        assert_eq!(fake.next_state().ps, Some(3));
        controller.send(WledControlMessage::SetState(level(20)));
        assert_eq!(fake.next_state().bri, Some(20));

        std::thread::sleep(Duration::from_millis(50));
        fake.restart();

        let first = fake.next_state();
        assert_eq!(first.ps, Some(3));
        assert_eq!(first.bri, None);
        let asked_for_preset = Instant::now();
        let second = fake.next_state();
        assert_eq!(second.ps, None);
        assert_eq!(second.bri, Some(20));
        assert!(asked_for_preset.elapsed() >= PRESET_LOAD_TIME / 2);
    }

    /// A preset picked while a fader is moving goes out with the level, and
    /// overrides it. The level is sent again once the preset has loaded.
    #[test]
    fn level_sent_with_a_preset_is_sent_again_afterwards() {
        let fake = FakeWled::start();
        let controller = WledController::run(fake.url.clone());
        controller.send(WledControlMessage::SetState(State {
            ps: Some(3),
            bri: Some(20),
            ..Default::default()
        }));
        assert_eq!(fake.next_state().ps, Some(3));
        let asked_for_preset = Instant::now();

        let second = fake.next_state();
        assert_eq!(second.ps, None);
        assert_eq!(second.bri, Some(20));
        assert!(asked_for_preset.elapsed() >= PRESET_LOAD_TIME / 2);
    }

    /// A preset picked on its own leaves the level to the preset.
    #[test]
    fn preset_sent_alone_is_not_followed_by_anything() {
        let fake = FakeWled::start();
        let controller = WledController::run(fake.url.clone());
        controller.send(WledControlMessage::SetState(level(20)));
        assert_eq!(fake.next_state().bri, Some(20));
        controller.send(WledControlMessage::SetState(preset(3)));
        assert_eq!(fake.next_state().ps, Some(3));

        assert!(fake.posted_within(PRESET_LOAD_TIME * 3).is_none());
    }

    /// A preset picked while a node is out of reach is one the node never
    /// had. It is sent once the node is back, restarted or not.
    #[test]
    fn preset_picked_while_the_node_is_behind_still_reaches_it() {
        let fake = FakeWled::start();
        let controller = WledController::run(fake.url.clone());
        controller.send(WledControlMessage::SetState(level(20)));
        assert_eq!(fake.next_state().bri, Some(20));

        // The sender finds the socket gone, cannot learn whether the node
        // restarted, and puts off telling it again.
        std::thread::sleep(Duration::from_millis(50));
        fake.fail_info_requests(1);
        fake.drop_websockets();
        std::thread::sleep(Duration::from_millis(500));

        controller.send(WledControlMessage::SetState(preset(3)));
        let first = fake.next_state();
        assert_eq!(first.ps, Some(3));
        assert_eq!(first.bri, None);
        assert_eq!(fake.next_state().bri, Some(20));
    }

    /// A node that only dropped its connection still has its preset. Loading
    /// it again would show on stage, so only the level is sent.
    #[test]
    fn node_that_only_lost_its_socket_is_not_sent_its_preset_again() {
        let fake = FakeWled::start();
        let controller = WledController::run(fake.url.clone());
        controller.send(WledControlMessage::SetState(preset(3)));
        assert_eq!(fake.next_state().ps, Some(3));
        controller.send(WledControlMessage::SetState(level(20)));
        assert_eq!(fake.next_state().bri, Some(20));

        std::thread::sleep(Duration::from_millis(50));
        fake.drop_websockets();

        let state = fake.next_state();
        assert_eq!(state.ps, None);
        assert_eq!(state.bri, Some(20));
    }

    /// A node that loses power closes nothing: what we send is accepted by
    /// our own side and goes nowhere. Its silence is what gives it away.
    #[test]
    fn node_that_goes_silent_is_noticed_and_told_again() {
        let fake = FakeWled::start();
        let controller = WledController::run(fake.url.clone());
        controller.send(WledControlMessage::SetState(level(20)));
        assert_eq!(fake.next_posted().0, Transport::WebSocket);
        // Silence only counts against a node known to answer heartbeats.
        let deadline = Instant::now() + Duration::from_secs(10);
        while fake.heartbeats() == 0 {
            assert!(Instant::now() < deadline, "no heartbeat arrived");
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(50));

        fake.go_silent();

        let posted = fake
            .posted_within(Duration::from_secs(10))
            .expect("the node was never told again");
        assert_eq!(posted.0, Transport::Http);
        assert_eq!(posted.1.bri, Some(20));
    }

    /// An effect picked after a preset replaces it. Sent in one message the
    /// node would load the preset last, so the preset is dropped.
    #[test]
    fn unsent_preset_gives_way_to_a_newer_look() {
        let state = supersede(Some(preset(3)), look(108));
        assert_eq!(state.ps, None);
        assert_eq!(state.seg.unwrap().remove(0).fx, Some(108));
    }

    /// A preset overrides the speed and size sent along with it, so they
    /// are not what the node is showing and are not remembered.
    #[test]
    fn remembered_preset_forgets_the_speed_sent_with_it() {
        let with_speed = State {
            ps: Some(3),
            seg: Some(vec![Seg {
                sx: Some(200),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let told = remember(Some(level(20)), &with_speed);
        assert_eq!(told.ps, Some(3));
        assert_eq!(told.bri, Some(20));
        assert_eq!(told.seg, None);
    }

    /// Picking an effect replaces a preset's look, so a node brought up to
    /// date must not be sent back to the preset.
    #[test]
    fn remembered_preset_gives_way_to_a_newer_look() {
        let told = remember(Some(preset(3)), &look(108));
        assert_eq!(told.ps, None);
        assert_eq!(told.seg.unwrap().remove(0).fx, Some(108));
    }

    /// Picking a preset replaces the look chosen before it, but not the
    /// level.
    #[test]
    fn remembered_look_gives_way_to_a_newer_preset() {
        let told = remember(Some(look(108)), &level(20));
        let told = remember(Some(told), &preset(3));
        assert_eq!(told.ps, Some(3));
        assert_eq!(told.bri, Some(20));
        assert_eq!(told.seg, None);
    }

    /// A node that cannot describe itself when first reached learns to flash
    /// once it can.
    #[test]
    fn controller_learns_to_flash_once_the_node_describes_itself() {
        let fake = FakeWled::start();
        fake.fail_info_requests(2);
        let controller =
            WledController::run_retrying_every(fake.url.clone(), Duration::from_millis(20));

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            controller.flash(200);
            if fake
                .next_realtime_packet(Duration::from_millis(50))
                .is_some()
            {
                break;
            }
            assert!(Instant::now() < deadline, "no flash arrived");
        }
    }

    /// A fixture removed in the middle of a flash does not leave its node lit.
    #[test]
    fn dropping_the_controller_ends_a_flash() {
        let fake = FakeWled::start();
        let controller = WledController::run(fake.url.clone());
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            controller.flash(200);
            if fake
                .next_realtime_packet(Duration::from_millis(50))
                .is_some()
            {
                break;
            }
            assert!(Instant::now() < deadline, "no flash arrived");
        }
        drop(controller);

        let deadline = Instant::now() + Duration::from_secs(5);
        while fake.next_realtime_packet(Duration::from_millis(50)) != Some(vec![4, 0]) {
            assert!(Instant::now() < deadline, "the node was never released");
        }
    }

    /// A node we never flashed may be showing someone else's realtime data.
    #[test]
    fn ending_a_flash_that_never_started_sends_nothing() {
        let fake = FakeWled::start();
        let controller = WledController::run(fake.url.clone());
        // Wait for the controller to have heard from the node.
        let deadline = Instant::now() + Duration::from_secs(5);
        while controller.flasher.lock().unwrap().is_none() {
            assert!(Instant::now() < deadline, "the node was never described");
            std::thread::sleep(Duration::from_millis(10));
        }
        controller.end_flash();
        assert_eq!(fake.next_realtime_packet(Duration::from_millis(100)), None);
    }

    #[test]
    fn take_returns_none_once_closed() {
        let outbox = Outbox::default();
        assert!(outbox.post(level(10)));
        outbox.close();

        assert!(take(&outbox).is_none());
    }
}
