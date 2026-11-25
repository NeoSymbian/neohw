// SPDX-License-Identifier: Apache-2.0
//! WIFID.EXE (docs hardware.md S5, hardware/wifi.md): Wi-Fi without the UI.
//! - The firmware's start: WLFW over NeoHub (HWHUB routes the modem's QRTR), the MSA, the board data, mission mode.
//! - Joining: a supplicant thread joins through the kernel (WlanLink) and runs the WPA2 4-way handshake (`wpa`).
//!   A network that reached "Connected" is remembered (SSID + PMK, never the password) and joined again when it is
//!   seen while nothing is joined; with a remembered network to find, it scans in the background once a minute.
//! - NeoWifi (`wifi::ipc`) for neoui's Settings: the scan and the link (the kernel's records), join, leave, the switch.
//! - It starts NEONET (the IP stack). neoui's SYSSTART starts it.
#![no_std]
extern crate alloc;

mod hal;

use alloc::rc::Rc;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::ops::ControlFlow;
use core::pin::pin;
use e32::fs::{File, Fs, mode};
use e32::ipc::{Session, TVersion};
use e32::rt::{self, Either};
use e32::server::{self, Message, Server, Service};
use e32::sync::Mutex;
use e32::sys::err;
use e32::thread::JoinHandle;
use e32::{dprintln, time};
use hub_ipc::Event;
use wifi::ipc::{Connect, fail, op};
use wifi::kernel::{Link, LinkState, Saved, Scan};

const LOOP_MS: u64 = 10;
/// The remembered networks, in WIFID's private directory
const SAVED: &str = "C:\\private\\e0004002\\wifi.dat";
/// The SSID the user disconnected from (empty: none), so a reboot doesn't rejoin it
const LEFT: &str = "C:\\private\\e0004002\\left.dat";
/// where SYSSTART kept them before S5 (read once, if ours is missing)
const OLD_SAVED: &str = "C:\\private\\e0002001\\wifi.dat";
/// A remembered network seen while nothing is joined is tried again after this long; and the background scan's period
const RETRY_MS: u64 = 60_000;
/// A 4-way handshake takes well under a second; give up after this. An AP that keeps resending message 1 without a
/// deauthentication left the WCN3990 firmware half-joined for a minute until its station health monitor asserted and
/// took the whole modem down (2026-11-13, `wlan_ps_sta_health_monitor_timeout`).
const HANDSHAKE_MS: u64 = 10_000;
/// The kernel's join (finishing a scan, then up to 3 tries for each frame) ends well within this
const JOIN_MS: u64 = 30_000;

/// Why the supplicant thread gave up (0 = it ended with the link); shared with the thread
type Failed = Option<Arc<Mutex<i32>>>;
/// a running supplicant thread, the SSID and PMK (none: an open network) it joins with, and when it started
type Join = (JoinHandle, Vec<u8>, Option<[u8; 32]>, u64);

struct Daemon {
    fw: wifi::Firmware,
    hub: Option<Rc<Session>>,
    next_connect: u64,
    /// the next WlanStart retry (once a second while the kernel refuses it)
    next_start: u64,
    saved: Saved,
    failed: Failed,
    join: Option<Join>,
    /// the SSID the user left: not rejoined on its own until CONNECT
    left: Option<Vec<u8>>,
    /// Settings' Wi-Fi switch
    radio_on: bool,
    /// a join saw no EAPOL at all (fail::NO_DATA): no automatic joins until the modem restarts or the user connects
    deaf: bool,
    link: Link,
    next_link: u64,
    last: Option<Scan>,
    /// when the latest scan completed (its counter changed), and when a background scan was asked
    listed_at: Option<u64>,
    asked_at: Option<u64>,
    /// when the kernel's scan record is read next (every 5 s, for anyone's scan)
    next_read: u64,
    next_session: u32,
}

/// The NeoHub client's view: WLFW's sends cross to HWHUB.
fn bus(session: Option<&Session>) -> sys::hub::HubBus<'_> {
    sys::hub::HubBus::new(session, "wifid")
}

impl Daemon {
    fn connect_hub(&mut self, now: u64) {
        if self.hub.is_some() || now < self.next_connect {
            return;
        }
        self.next_connect = now + 500;
        let Some(s) = sys::hub::connect() else {
            return; // HWHUB not up yet (MODEMD starts it)
        };
        match self.fw.bind(&mut bus(Some(&s))) {
            Ok(()) => {
                dprintln!("[wifid] connected to NeoHub");
                self.hub = Some(Rc::new(s));
            }
            Err(e) => dprintln!("[wifid] NeoHub bind failed ({})", e),
        }
    }

    fn hub_events(&mut self, bytes: &[u8]) {
        let Some(events) = hub_ipc::decode_batch(bytes) else {
            dprintln!("[wifid] malformed NeoHub event batch");
            return;
        };
        for e in events {
            if let Event::Reset { generation, .. } = e {
                self.deaf = false; // the restarted firmware gets a fresh data path
                self.stop();
                set(&self.failed, 0); // else the old NO_DATA makes it deaf again at once
                if let Some(s) = &self.hub {
                    sys::hub::quiesced(s, generation);
                }
            }
            self.fw
                .event(&mut bus(self.hub.as_deref()), &mut hal::Fw, &e);
        }
    }

    /// Start joining `bssid` with this PMK (`None`: an open network); a running join is left first.
    fn start(&mut self, bssid: [u8; 6], ssid: &[u8], pmk: Option<[u8; 32]>, now: u64) {
        self.stop();
        self.set_left(None);
        set(&self.failed, 0);
        let failed = self.failed.clone();
        dprintln!("[wifid] joining {:02x?}", bssid);
        match e32::thread::spawn(move || supplicant(bssid, pmk, failed)) {
            Ok(h) => self.join = Some((h, ssid.to_vec(), pmk, now)),
            Err(e) => dprintln!("[wifid] no supplicant thread ({})", e),
        }
    }

    /// Remember the SSID the user left (or none), on C: too, so a reboot keeps it.
    fn set_left(&mut self, ssid: Option<Vec<u8>>) {
        if self.left != ssid {
            if let Err(e) = sys::save_private(
                "C:\\private\\e0004002\\",
                "left.dat",
                ssid.as_deref().unwrap_or(&[]),
            ) {
                dprintln!("[wifid] saving the left network failed ({})", e);
            }
            self.left = ssid;
        }
    }

    /// Leave the network and end the supplicant thread.
    fn stop(&mut self) {
        if let Some((h, ..)) = self.join.take() {
            let _ = hal::leave();
            h.join();
        }
    }

    /// The joined network's SSID: the join's, else the scan entry of the linked BSSID.
    fn joined_ssid(&self) -> Option<Vec<u8>> {
        if let Some((_, ssid, ..)) = &self.join {
            return Some(ssid.clone());
        }
        let b = self.link.bssid;
        self.last
            .as_ref()?
            .networks
            .iter()
            .find(|n| n.bssid == b)
            .map(|n| n.ssid.clone())
    }

    fn step(&mut self, now: u64) {
        self.connect_hub(now);
        if now >= self.next_link {
            self.next_link = now + 200;
            let mut b = [0u8; wifi::kernel::LINK_SIZE];
            let link = hal::link(&mut b)
                .ok()
                .and_then(|n| Link::decode(&b[..n]))
                .unwrap_or_default();
            if link.state == LinkState::Connected
                && self.link.state != LinkState::Connected
                && let Some((_, ssid, Some(pmk), _)) = &self.join
            {
                self.saved.remember(ssid, *pmk);
                store(&self.saved);
            }
            if link.state != self.link.state {
                dprintln!("[wifid] link {:?}", link.state);
            }
            self.link = link;
        }
        if self
            .failed
            .as_ref()
            .is_some_and(|m| *m.lock() == fail::NO_DATA)
            && !self.deaf
        {
            dprintln!(
                "[wifid] no EAPOL in a whole handshake: the data path is down; no rejoins until a modem restart"
            );
            self.deaf = true;
        }
        if now >= self.next_start {
            self.next_start = now + 1000;
            self.fw
                .retry_start(&mut bus(self.hub.as_deref()), &mut hal::Fw);
        }
        // no joins while the firmware isn't in mission mode (the modem restarting takes it down)
        if !self.radio_on || !self.link.idle() || self.deaf || !self.fw.enabled {
            return;
        }
        // a remembered network in the list while nothing is joined: try it (once a minute at most)
        let quiet = self
            .join
            .as_ref()
            .is_none_or(|j| now.saturating_sub(j.3) >= RETRY_MS);
        if quiet
            && let Some((bssid, pmk, ssid)) = self.last.as_ref().and_then(|s| {
                s.networks
                    .iter()
                    .filter(|n| !n.ssid.is_empty() && self.left.as_deref() != Some(&n.ssid[..]))
                    .find_map(|n| {
                        let pmk = self.saved.pmk(&n.ssid)?;
                        Some((s.best(&n.ssid)?.bssid, pmk, n.ssid.clone())) // its best AP, 5 GHz first
                    })
            })
        {
            self.start(bssid, &ssid, Some(pmk), now);
            return;
        }
        // in the background: a remembered network to look for and no fresh list (a scan a minute; the kernel never
        // scans while joined), so a saved network comes back after a reboot without opening Settings
        let wanted = self
            .saved
            .0
            .iter()
            .any(|(ssid, _)| self.left.as_deref() != Some(&ssid[..]));
        let old = self
            .listed_at
            .is_none_or(|t| now.saturating_sub(t) >= RETRY_MS);
        let waiting = self
            .asked_at
            .is_some_and(|t| now.saturating_sub(t) < RETRY_MS);
        if wanted && old && !waiting {
            self.asked_at = Some(now);
            let mut b = vec![0u8; wifi::kernel::SCAN_SIZE];
            if let Ok(n) = hal::scan(true, &mut b) {
                self.seen(&b[..n], now);
            }
        } else if now >= self.next_read {
            self.next_read = now + 5000;
            let mut b = vec![0u8; wifi::kernel::SCAN_SIZE];
            if let Ok(n) = hal::scan(false, &mut b) {
                self.seen(&b[..n], now);
            }
        }
    }

    /// A scan record read for anyone: remembered for joining.
    fn seen(&mut self, b: &[u8], now: u64) {
        let Some(scan) = Scan::decode(b) else { return };
        if self.last.as_ref().is_none_or(|l| l.scans != scan.scans) && scan.state != 0 {
            self.listed_at = Some(now);
        }
        self.last = Some(scan);
    }
}

impl Service for Daemon {
    type Session = u32;

    fn connect(&mut self, v: TVersion) -> Result<u32, i32> {
        if v.major as i8 != wifi::ipc::VERSION.0 {
            return Err(err::KERR_NOT_SUPPORTED);
        }
        self.next_session = self.next_session.wrapping_add(1).max(1);
        Ok(self.next_session)
    }

    fn message(&mut self, _session: &mut u32, msg: Message) {
        let now = time::millis();
        let r = match msg.function() {
            op::SCAN => {
                let mut b = vec![0u8; wifi::kernel::SCAN_SIZE];
                match hal::scan(msg.int(0) != 0, &mut b) {
                    Ok(n) => {
                        self.seen(&b[..n], now);
                        msg.write8(1, &b[..n], 0)
                            .map_or_else(|e| e, |_| err::KERR_NONE)
                    }
                    Err(e) => e,
                }
            }
            op::LINK => {
                let mut b = [0u8; wifi::kernel::LINK_SIZE];
                match hal::link(&mut b) {
                    Ok(n) => msg
                        .write8(0, &b[..n], 0)
                        .map_or_else(|e| e, |_| self.failed.as_ref().map_or(0, |m| *m.lock())),
                    Err(e) => e,
                }
            }
            op::CONNECT => match msg.read8_vec(0).ok().and_then(|b| Connect::decode(&b)) {
                Some(c) => {
                    self.deaf = false; // the user asked: try once more
                    let pmk = (!c.password.is_empty()).then(|| wpa::psk(&c.password, &c.ssid));
                    // the user picks the network, WIFID its access point (5 GHz when good); the row's AP
                    // when the latest scan lacks the name (a hidden network)
                    let bssid = self
                        .last
                        .as_ref()
                        .and_then(|s| s.best(&c.ssid))
                        .map_or(c.bssid, |n| n.bssid);
                    self.start(bssid, &c.ssid, pmk, now);
                    err::KERR_NONE
                }
                None => err::KERR_ARGUMENT,
            },
            op::LEAVE => {
                let ssid = self.joined_ssid();
                self.stop();
                if let Some(ssid) = ssid {
                    if msg.int(0) != 0 {
                        self.saved.forget(&ssid);
                        store(&self.saved);
                    }
                    self.set_left(Some(ssid));
                }
                self.next_link = 0;
                err::KERR_NONE
            }
            op::RADIO => {
                self.radio_on = msg.int(0) != 0;
                if !self.radio_on {
                    // off leaves the network too (the radio itself stays up); on rejoins a remembered one
                    self.stop();
                    let _ = hal::leave();
                    self.next_link = 0;
                }
                err::KERR_NONE
            }
            _ => err::KERR_NOT_SUPPORTED,
        };
        msg.complete(r);
    }

    fn disconnect(&mut self, _session: u32) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }
}

/// The supplicant thread: join, then answer the AP's EAPOL-Key frames until the link ends (a 10 ms poll: the
/// driver is polled too, and the AP waits about a second per handshake message).
fn supplicant(bssid: [u8; 6], pmk: Option<[u8; 32]>, failed: Failed) -> i32 {
    let fail = |why: i32| {
        set(&failed, why);
        let _ = hal::leave();
        why
    };
    let rsn: &[u8] = if pmk.is_some() { &wpa::RSN_IE } else { &[] };
    if hal::join(&bssid, rsn).is_err() {
        return fail(fail::HAL);
    }
    let mut hs: Option<wpa::Handshake> = None;
    let mut frame = [0u8; 512];
    let mut handshake_since: Option<u64> = None;
    let joined_at = time::millis();
    let mut eapol = 0u32;
    loop {
        time::sleep(10_000);
        let mut b = [0u8; wifi::kernel::LINK_SIZE];
        let Some(link) = hal::link(&mut b).ok().and_then(|n| Link::decode(&b[..n])) else {
            return fail(fail::HAL);
        };
        match link.state {
            LinkState::Idle | LinkState::Lost | LinkState::Error => return 0,
            // never wait on the kernel's join for ever: a reset or a leave waits on this thread
            LinkState::Joining if time::millis().saturating_sub(joined_at) > JOIN_MS => {
                return fail(fail::HAL);
            }
            LinkState::Joining => continue,
            LinkState::Handshake => {
                let now = time::millis();
                if now.saturating_sub(*handshake_since.get_or_insert(now)) > HANDSHAKE_MS {
                    dprintln!(
                        "[wifid] no message 3 in {} ms ({} EAPOL frames): leaving",
                        HANDSHAKE_MS,
                        eapol
                    );
                    // a wrong password still brings message 1 again and again; nothing at all is a dead data path
                    return fail(if eapol == 0 {
                        fail::NO_DATA
                    } else {
                        fail::HANDSHAKE
                    });
                }
            }
            LinkState::Connected => {}
        }
        let Some(pmk) = pmk else {
            continue; // an open network: no handshake, the kernel reports Connected after association
        };
        let hs = match &mut hs {
            Some(h) => h,
            None => {
                if !wpa::Rsn::parse(&link.rsn).is_some_and(|r| r.supported()) {
                    return fail(fail::SECURITY);
                }
                let mut snonce = [0u8; 32];
                if hal::random(&mut snonce).is_err() {
                    return fail(fail::RANDOM);
                }
                hs.insert(wpa::Handshake::new(pmk, bssid, link.mac, &link.rsn, snonce))
            }
        };
        while let Ok(Some(n)) = hal::eapol_recv(&mut frame) {
            eapol += 1;
            let actions = match hs.on_eapol(&frame[..n]) {
                Ok(a) => a,
                Err(wpa::Error::RsnMismatch | wpa::Error::NoGtk) => return fail(fail::DOWNGRADE),
                Err(_) => continue, // malformed, replayed or forged: ignored, as the standard says
            };
            for a in actions {
                let r = match a {
                    wpa::Action::Send(f) => hal::eapol_send(&f),
                    wpa::Action::InstallPtk(k) => hal::install_key(false, 0, &k),
                    wpa::Action::InstallGtk(i, k) => hal::install_key(true, i, &k),
                    wpa::Action::Authorize => hal::authorize(),
                };
                if r.is_err() {
                    return fail(fail::HAL);
                }
            }
        }
    }
}

fn set(failed: &Failed, why: i32) {
    if let Some(m) = failed {
        *m.lock() = why;
    }
}

fn read(path: &str) -> Result<Vec<u8>, i32> {
    let fs = Fs::connect()?;
    let f = File::open(&fs, path, mode::READ | mode::SHARE_READERS_ONLY)?;
    let mut b = vec![0u8; 1024];
    let n = f.read_at(0, &mut b)?;
    b.truncate(n);
    Ok(b)
}

fn store(saved: &Saved) {
    if let Err(e) = sys::save_private("C:\\private\\e0004002\\", "wifi.dat", &saved.encode()) {
        dprintln!("[wifid] saving the networks failed ({})", e);
    }
}

fn main() -> i32 {
    let server = match Server::create(wifi::ipc::NAME) {
        Ok(s) => s,
        Err(e) => {
            dprintln!("[wifid] NeoWifi name taken ({}): exiting", e);
            return 0;
        }
    };
    // the networks SYSSTART remembered before S5 move here once
    let saved = match read(SAVED) {
        Ok(b) => Saved::decode(&b),
        Err(_) => {
            let old = read(OLD_SAVED)
                .map(|b| Saved::decode(&b))
                .unwrap_or_default();
            if !old.0.is_empty() {
                dprintln!(
                    "[wifid] {} remembered networks moved from SYSSTART",
                    old.0.len()
                );
                store(&old);
            }
            old
        }
    };
    match e32::process::Process::spawn("NEONET.EXE", "") {
        Ok(p) => p.resume(),
        Err(e) => dprintln!("[wifid] no network daemon ({})", e),
    }
    dprintln!(
        "[wifid] start: serving NeoWifi, {} remembered networks",
        saved.0.len()
    );
    let mut d = Daemon {
        fw: wifi::Firmware::new(),
        hub: None,
        next_connect: 0,
        next_start: 0,
        saved,
        failed: Mutex::new(0).ok().map(Arc::new),
        join: None,
        left: read(LEFT).ok().filter(|b| !b.is_empty()),
        radio_on: true,
        deaf: false,
        link: Link::default(),
        next_link: 0,
        last: None,
        listed_at: None,
        asked_at: None,
        next_read: 0,
        next_session: 0,
    };
    rt::block_on(async {
        let mut receive = pin!(server.receive());
        let mut hub_bytes = vec![0u8; 0x4000];
        let mut next = time::millis();
        loop {
            let hub = d.hub.clone();
            // the receive's descriptor outlives its pinned future and is read only after it completed
            let completed = {
                let mut hub_rx = pin!(sys::hub::receive(hub.as_deref(), &mut hub_bytes));
                loop {
                    let now = time::millis();
                    if now >= next {
                        d.step(now);
                        next = now.max(next) + LOOP_MS;
                    }
                    if hub.is_none() && d.hub.is_some() {
                        break None; // just connected: receive on the new session
                    }
                    let wait_us = (next.saturating_sub(time::millis()) * 1000).max(1) as i32;
                    match rt::select(
                        receive.as_mut(),
                        rt::select(hub_rx.as_mut(), time::delay(wait_us)),
                    )
                    .await
                    {
                        Either::Left(Ok(msg)) => {
                            receive.set(server.receive());
                            let _ = server::dispatch(&mut d, msg);
                        }
                        Either::Left(Err(e)) => {
                            dprintln!("[wifid] receive failed ({})", e);
                            receive.set(server.receive());
                        }
                        Either::Right(Either::Left(r)) => break Some(r),
                        Either::Right(Either::Right(())) => {}
                    }
                }
            };
            match completed {
                Some(Ok(n)) => d.hub_events(&hub_bytes[..n]),
                Some(Err(e)) => {
                    dprintln!("[wifid] NeoHub disconnected ({})", e);
                    d.hub = None;
                    d.fw = wifi::Firmware::new();
                }
                None => {}
            }
        }
    })
}

e32::main!(main);
