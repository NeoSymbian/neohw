// SPDX-License-Identifier: Apache-2.0
//! GNSSD.EXE (docs hardware/gnss.md): location from the modem's GNSS engine.
//! - QMI LOC over NeoHub (`loc::Gnss`): event registration, engine unlock, standalone mode, fix sessions, time.
//! - XTRA (predicted orbits): downloaded over Wi-Fi through NEONET on a worker thread, cached on C:, injected.
//! - NeoLocation (`loc::ipc`) for apps: the latest fix, START and STOP. MODEMD starts it.
#![no_std]
extern crate alloc;

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::String;
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
use hub_ipc::{Bus, Event};
use loc::ipc::op;
use sys::hub::HubBus;

const LOOP_MS: u64 = 100;
const DIR: &str = "C:\\private\\e0003004\\";
const XTRA: &str = "C:\\private\\e0003004\\xtra.bin";
/// the XTRA file's download time, UTC ms, 8 bytes little-endian
const XTRA_TIME: &str = "C:\\private\\e0003004\\xtra.utc";
/// a cached XTRA file younger than this is injected without a download
const XTRA_FRESH_MS: i64 = 24 * 3_600_000;
/// a failed download is tried again after this long
const XTRA_RETRY_MS: u64 = 10 * 60_000;
/// read buffer per NeoNet RECV (its maximum)
const CHUNK: usize = 8 * 1024;

/// The download thread's result, shared with it (leaked: GNSSD lives for ever, and Arc needs compare-and-swap,
/// which ARMv5TE lacks)
type Slot = &'static Mutex<Option<Result<Vec<u8>, i32>>>;

struct Daemon {
    gnss: loc::Gnss,
    hub: Option<Rc<Session>>,
    next_connect: u64,
    download: Option<JoinHandle>,
    slot: Option<Slot>,
    next_download: u64,
}

fn bus(session: Option<&Session>) -> HubBus<'_> {
    HubBus::new(session, "gnssd")
}

impl Daemon {
    fn connect_hub(&mut self, now: u64) {
        if self.hub.is_some() || now < self.next_connect {
            return;
        }
        self.next_connect = now + 500;
        let Some(s) = sys::hub::connect() else {
            return; // HWHUB not up yet (MODEMD starts it); on Syborg it never has LOC
        };
        match self.gnss.bind(&mut bus(Some(&s))) {
            Ok(()) => {
                dprintln!("[gnssd] connected to NeoHub");
                self.hub = Some(Rc::new(s));
            }
            Err(e) => dprintln!("[gnssd] NeoHub bind failed ({})", e),
        }
    }

    fn hub_events(&mut self, bytes: &[u8]) {
        let Some(events) = hub_ipc::decode_batch(bytes) else {
            dprintln!("[gnssd] malformed NeoHub event batch");
            return;
        };
        let now = time::millis();
        for e in events {
            if let Event::Reset { generation, .. } = e
                && let Some(s) = &self.hub
            {
                sys::hub::quiesced(s, generation);
            }
            self.gnss.event(&mut bus(self.hub.as_deref()), &e, now);
        }
    }

    fn step(&mut self, now: u64) {
        self.connect_hub(now);
        let hub = self.hub.clone();
        let mut b = bus(hub.as_deref());
        self.gnss.tick(&mut b, now);
        // a finished download: cache and inject it
        let done = self
            .slot
            .and_then(|s| s.lock().take())
            .filter(|_| self.download.is_some());
        if let Some(r) = done {
            if let Some(h) = self.download.take() {
                h.join();
            }
            match r {
                Ok(data) => {
                    let utc = b.utc_ms();
                    save(&data, utc);
                    self.gnss.xtra(&mut b, data, utc);
                }
                Err(e) => dprintln!("[gnssd] XTRA download failed ({})", e),
            }
        }
        if !self.gnss.want_xtra || self.download.is_some() || now < self.next_download {
            return;
        }
        let utc = b.utc_ms();
        if let Some((data, at)) = cached().filter(|(_, at)| utc > 0 && utc - at < XTRA_FRESH_MS) {
            dprintln!("[gnssd] XTRA from the cache ({} bytes)", data.len());
            self.gnss.xtra(&mut b, data, at);
            return;
        }
        let urls = self.gnss.xtra_urls();
        if urls.is_empty() {
            dprintln!("[gnssd] no XTRA server to fetch from");
            self.gnss.want_xtra = false;
            return;
        }
        let Some(slot) = self.slot else { return };
        // only a real download attempt waits before the next (the modem's own request brings the URLs later)
        self.next_download = now + XTRA_RETRY_MS;
        match e32::thread::spawn_with_stack(32 << 10, move || {
            let r = download(&urls);
            *slot.lock() = Some(r);
            0
        }) {
            Ok(h) => self.download = Some(h),
            Err(e) => dprintln!("[gnssd] no download thread ({})", e),
        }
    }
}

/// Fetch the first XTRA file that downloads (blocking NeoNet calls: on the worker thread).
fn download(urls: &[(String, String)]) -> Result<Vec<u8>, i32> {
    let net = net::client::Net::connect()?; // KErrNotFound: no NEONET (no Wi-Fi daemon)
    let mut last = err::KERR_NOT_FOUND;
    for (host, path) in urls {
        let r = (|| {
            let ip = net.resolve(host)?;
            let mut s = net.tcp_connect(ip, 80)?;
            s.write_all(&loc::http::request(host, path))?;
            let mut resp = Vec::new();
            let mut buf = vec![0u8; CHUNK];
            loop {
                let n = s.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                if resp.len() + n > loc::http::MAX_BODY + CHUNK {
                    return Err(err::KERR_OVERFLOW);
                }
                resp.extend_from_slice(&buf[..n]);
            }
            match loc::http::body(&resp) {
                Ok(b) => Ok(b.to_vec()),
                Err(e) => {
                    dprintln!("[gnssd] XTRA from {}: {:?}", host, e);
                    Err(err::KERR_GENERAL)
                }
            }
        })();
        match r {
            Ok(b) => {
                dprintln!("[gnssd] XTRA downloaded from {}: {} bytes", host, b.len());
                return Ok(b);
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn read(path: &str, max: usize) -> Result<Vec<u8>, i32> {
    let fs = Fs::connect()?;
    let f = File::open(&fs, path, mode::READ | mode::SHARE_READERS_ONLY)?;
    let mut b = vec![0u8; max];
    let mut n = 0;
    while n < max {
        let r = f.read_at(n as i32, &mut b[n..])?;
        if r == 0 {
            break;
        }
        n += r;
    }
    b.truncate(n);
    Ok(b)
}

/// The cached XTRA file and its download time.
fn cached() -> Option<(Vec<u8>, i64)> {
    let t = read(XTRA_TIME, 8).ok()?;
    let at = i64::from_le_bytes(t.get(..8)?.try_into().ok()?);
    let data = read(XTRA, loc::http::MAX_BODY)
        .ok()
        .filter(|d| !d.is_empty())?;
    Some((data, at))
}

fn save(data: &[u8], utc: i64) {
    // the time last, so a torn write leaves no fresh-looking cache
    let r = sys::save_private(DIR, "xtra.utc", &0i64.to_le_bytes())
        .and_then(|_| sys::save_private(DIR, "xtra.bin", data))
        .and_then(|_| sys::save_private(DIR, "xtra.utc", &utc.to_le_bytes()));
    if let Err(e) = r {
        dprintln!("[gnssd] caching XTRA failed ({})", e);
    }
}

impl Service for Daemon {
    /// whether this session wants fixes
    type Session = bool;

    fn connect(&mut self, v: TVersion) -> Result<bool, i32> {
        if v.major as i8 != loc::ipc::VERSION.0 {
            return Err(err::KERR_NOT_SUPPORTED);
        }
        Ok(false)
    }

    fn message(&mut self, wants: &mut bool, msg: Message) {
        let hub = self.hub.clone();
        let mut b = bus(hub.as_deref());
        let r = match msg.function() {
            op::STATUS => {
                let s = self.gnss.status(b.utc_ms()).encode();
                msg.write8(0, &s, 0).map_or_else(|e| e, |_| err::KERR_NONE)
            }
            op::START => {
                if !*wants {
                    *wants = true;
                    self.gnss.start(&mut b, time::millis());
                }
                err::KERR_NONE
            }
            op::STOP => {
                if *wants {
                    *wants = false;
                    self.gnss.stop(&mut b);
                }
                err::KERR_NONE
            }
            _ => err::KERR_NOT_SUPPORTED,
        };
        msg.complete(r);
    }

    fn disconnect(&mut self, wants: bool) -> ControlFlow<()> {
        if wants {
            let hub = self.hub.clone();
            self.gnss.stop(&mut bus(hub.as_deref()));
        }
        ControlFlow::Continue(())
    }
}

fn main() -> i32 {
    let server = match Server::create(loc::ipc::NAME) {
        Ok(s) => s,
        Err(e) => {
            dprintln!("[gnssd] NeoLocation name taken ({}): exiting", e);
            return 0;
        }
    };
    dprintln!("[gnssd] start: serving NeoLocation");
    let mut d = Daemon {
        gnss: loc::Gnss::new(),
        hub: None,
        next_connect: 0,
        download: None,
        slot: Mutex::new(None).ok().map(|m| &*Box::leak(Box::new(m))),
        next_download: 0,
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
                            dprintln!("[gnssd] receive failed ({})", e);
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
                    dprintln!("[gnssd] NeoHub disconnected ({})", e);
                    d.hub = None;
                    // keeps who wants fixes: they resume once LOC is up again
                    let reset = Event::Reset {
                        subsystem: hub_ipc::Subsystem::All,
                        generation: 0,
                    };
                    d.gnss.event(&mut bus(None), &reset, time::millis());
                }
                None => {}
            }
        }
    })
}

e32::main!(main);
