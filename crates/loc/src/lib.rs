// SPDX-License-Identifier: Apache-2.0
//! GNSS's user side (docs hardware/gnss.md), driven by GNSSD.EXE:
//! - [`Gnss`]: the modem's QMI LOC client over a NeoHub [`Bus`]: event registration, the engine lock, standalone
//!   mode, fix sessions, time and XTRA injection;
//! - [`proto`]: the LOC messages; [`http`]: the XTRA download's request and response;
//! - [`ipc`]: the NeoLocation contract (GNSSD <-> apps).
#![no_std]
extern crate alloc;

pub mod http;
pub mod ipc;
pub mod proto;

use alloc::string::String;
use alloc::vec::Vec;
use hub_ipc::{Bus, Event};
use ipc::{Status, flag, state};
use proto::id;
use qrtr::qmi::{INDICATION, Msg, RESPONSE};

/// our LOC client port (HWHUB allows it since GN1)
pub const PORT: u32 = 0x28;
const MODEM_NODE: u32 = 0;
/// a session with no fix this long gives up
pub const FIX_TIMEOUT_MS: u64 = 15 * 60_000;
const INTERVAL_MS: u32 = 1000;
/// how far our clock (NITZ or an HTTP Date) may be off
const TIME_UNCERTAINTY_MS: u32 = 5000;
/// XTRA with less validity left than this is fetched again
const XTRA_MARGIN_MS: u64 = 3_600_000;

struct Upload {
    data: Vec<u8>,
    part_size: usize,
    /// the part waiting for its acknowledgement, from 1
    part: u16,
    parts: u16,
}

pub struct Gnss {
    /// LOC's QRTR address, once the modem announced it
    loc: Option<(u32, u32)>,
    txn: u16,
    /// sessions that want fixes
    clients: u32,
    /// the running fix session's start (ms since boot)
    started: Option<u64>,
    status: Status,
    last_report: Option<u32>,
    engine: Option<i32>,
    urls: Vec<String>,
    max_file: usize,
    max_part: usize,
    upload: Option<Upload>,
    /// when the injected file was downloaded (UTC ms)
    xtra_utc: Option<i64>,
    /// the modem's XTRA validity end (UTC ms)
    valid_until: Option<u64>,
    /// The EXE should download XTRA (from [`Gnss::xtra_urls`]) and call [`Gnss::xtra`]; it clears this.
    pub want_xtra: bool,
}

impl Default for Gnss {
    fn default() -> Self {
        Self::new()
    }
}

impl Gnss {
    pub fn new() -> Self {
        Gnss {
            loc: None,
            txn: 0,
            clients: 0,
            started: None,
            status: Status::default(),
            last_report: None,
            engine: None,
            urls: Vec::new(),
            max_file: http::MAX_BODY,
            max_part: proto::PART_MAX,
            upload: None,
            xtra_utc: None,
            valid_until: None,
            want_xtra: false,
        }
    }

    pub fn bind(&self, bus: &mut dyn Bus) -> Result<(), i32> {
        bus.bind(proto::SERVICE, PORT)
    }

    /// The NeoLocation record at UTC `utc_ms` (0 = unknown).
    pub fn status(&self, utc_ms: i64) -> Status {
        let mut s = self.status;
        s.flags = 0;
        if self.loc.is_some() {
            s.flags |= flag::SERVICE;
        }
        if self
            .valid_until
            .is_some_and(|v| utc_ms > 0 && v > utc_ms as u64)
        {
            s.flags |= flag::XTRA_VALID;
        }
        if let (Some(at), true) = (self.xtra_utc, utc_ms > 0) {
            s.xtra_age_s = (utc_ms.saturating_sub(at).max(0) / 1000) as u32;
        }
        s
    }

    /// The XTRA URLs the modem named that we may fetch: `(host, path)`.
    pub fn xtra_urls(&self) -> Vec<(String, String)> {
        self.urls
            .iter()
            .filter_map(|u| http::split_url(u))
            .map(|(h, p)| (String::from(h), String::from(p)))
            .collect()
    }

    fn send(&mut self, bus: &mut dyn Bus, make: impl FnOnce(u16) -> Vec<u8>) {
        let Some(dst) = self.loc else { return };
        self.txn = self.txn.wrapping_add(1).max(1);
        let m = make(self.txn);
        bus.send(PORT, dst, &m, true);
    }

    /// A session wants fixes (NeoLocation START).
    pub fn start(&mut self, bus: &mut dyn Bus, now_ms: u64) {
        self.clients += 1;
        if self.started.is_none() {
            self.started = Some(now_ms);
            self.status.state = state::SEARCHING;
            self.status.ttff_ms = 0;
            self.last_report = None;
            bus.log(format_args!("loc: fixes wanted"));
            self.begin(bus);
        }
    }

    /// A session doesn't want fixes any more (NeoLocation STOP, or it closed).
    pub fn stop(&mut self, bus: &mut dyn Bus) {
        self.clients = self.clients.saturating_sub(1);
        if self.clients == 0 {
            self.end(bus, state::OFF);
        }
    }

    /// The time limit; call it at least once a second.
    pub fn tick(&mut self, bus: &mut dyn Bus, now_ms: u64) {
        if let Some(t) = self.started
            && self.status.ttff_ms == 0
            && now_ms.saturating_sub(t) >= FIX_TIMEOUT_MS
        {
            bus.log(format_args!(
                "loc: no fix in {} s; stopped",
                FIX_TIMEOUT_MS / 1000
            ));
            self.end(bus, state::FAILED);
            self.clients = 0;
        }
    }

    /// The XTRA file the EXE downloaded at `downloaded_utc`: inject it part by part.
    pub fn xtra(&mut self, bus: &mut dyn Bus, data: Vec<u8>, downloaded_utc: i64) {
        self.want_xtra = false;
        if self.loc.is_none() || data.is_empty() || data.len() > self.max_file {
            bus.log(format_args!(
                "loc: XTRA of {} bytes not injected",
                data.len()
            ));
            return;
        }
        let part_size = self.max_part.clamp(1, proto::PART_MAX);
        let parts = data.len().div_ceil(part_size);
        let Ok(parts) = u16::try_from(parts) else {
            return;
        };
        bus.log(format_args!(
            "loc: injecting XTRA, {} bytes in {parts} parts",
            data.len()
        ));
        self.xtra_utc = Some(downloaded_utc);
        self.upload = Some(Upload {
            data,
            part_size,
            part: 1,
            parts,
        });
        self.send_part(bus);
    }

    fn send_part(&mut self, bus: &mut dyn Bus) {
        let Some(u) = &self.upload else { return };
        let from = (u.part as usize - 1) * u.part_size;
        let chunk = u.data[from..(from + u.part_size).min(u.data.len())].to_vec();
        let (total, parts, part) = (u.data.len() as u32, u.parts, u.part);
        self.send(bus, |t| proto::inject_orbits(t, total, parts, part, &chunk));
    }

    fn begin(&mut self, bus: &mut dyn Bus) {
        if self.loc.is_some() {
            self.send(bus, proto::orbits_validity);
            self.send(bus, |t| proto::start(t, INTERVAL_MS));
        }
    }

    fn end(&mut self, bus: &mut dyn Bus, to: u8) {
        if self.started.take().is_some() {
            self.send(bus, proto::stop);
            bus.log(format_args!("loc: fixes stopped"));
        }
        self.status.state = to;
    }

    fn lost(&mut self) {
        self.loc = None;
        self.upload = None;
        self.valid_until = None;
        self.engine = None;
        self.status.seen = 0;
        self.status.snr = 0;
    }

    pub fn event(&mut self, bus: &mut dyn Bus, e: &Event, now_ms: u64) {
        match e {
            Event::ServerUp(s) if s.service == proto::SERVICE && s.node == MODEM_NODE => {
                self.loc = Some((s.node, s.port));
                bus.log(format_args!(
                    "loc: the modem's LOC service is up at {}:{}",
                    s.node, s.port
                ));
                self.send(bus, proto::register_events);
                self.send(bus, proto::get_engine_lock);
                self.send(bus, |t| {
                    proto::set_operation_mode(t, proto::MODE_STANDALONE)
                });
                self.send(bus, proto::orbits_source);
                if self.started.is_some() {
                    self.begin(bus);
                }
            }
            Event::ServerDown(s) if self.loc == Some((s.node, s.port)) => {
                bus.log(format_args!("loc: the LOC service went"));
                self.lost();
            }
            Event::Reset { .. } => self.lost(),
            Event::Packet {
                src,
                dst_port: PORT,
                payload,
            } if self.loc == Some(*src) => {
                if let Some(m) = Msg::parse(payload) {
                    self.message(bus, &m, now_ms);
                }
            }
            _ => {}
        }
    }

    fn message(&mut self, bus: &mut dyn Bus, m: &Msg, now_ms: u64) {
        if m.ty == RESPONSE {
            if let Some(Err(e)) = m.result() {
                bus.log(format_args!("loc: request {:#x} refused ({e:#x})", m.id));
                if m.id == id::INJECT_ORBITS {
                    self.upload = None;
                }
            }
            return;
        }
        if m.ty != INDICATION {
            return;
        }
        match m.id {
            id::POSITION_REPORT => self.report(bus, m, now_ms),
            id::SV_INFO => {
                let (listed, heard, snr) = proto::satellites(m).unwrap_or_default();
                if (heard, snr / 5) != (self.status.seen, self.status.snr / 5) {
                    bus.log(format_args!(
                        "loc: {listed} satellites listed, {heard} heard, best {snr} dB-Hz"
                    ));
                }
                (self.status.seen, self.status.snr) = (heard, snr);
            }
            id::INJECT_TIME_REQUEST => {
                let utc = bus.utc_ms();
                if utc > 0 {
                    self.send(bus, |t| {
                        proto::inject_utc_time(t, utc as u64, TIME_UNCERTAINTY_MS)
                    });
                    bus.log(format_args!("loc: time injected"));
                }
            }
            id::ORBITS_REQUEST => {
                if let Some((file, part)) = m.tlv(0x01).and_then(proto::sizes) {
                    (self.max_file, self.max_part) = (file as usize, part as usize);
                }
                if let Some(v) = m.tlv(0x10) {
                    self.urls = proto::servers(v);
                }
                self.want_xtra |= self.upload.is_none();
                bus.log(format_args!(
                    "loc: the modem asks for XTRA from {:?}",
                    self.urls
                ));
            }
            id::ORBITS_SOURCE if proto::ind_ok(m) => {
                if let Some((file, part)) = m.tlv(0x10).and_then(proto::sizes) {
                    (self.max_file, self.max_part) = (file as usize, part as usize);
                }
                if let Some(v) = m.tlv(0x11) {
                    self.urls = proto::servers(v);
                }
                bus.log(format_args!("loc: XTRA source {:?}", self.urls));
            }
            id::ORBITS_VALIDITY => {
                self.valid_until = m.tlv(0x10).and_then(proto::valid_until);
                let utc = bus.utc_ms();
                let fresh = self
                    .valid_until
                    .is_some_and(|v| utc > 0 && v > utc as u64 + XTRA_MARGIN_MS);
                if !fresh && self.upload.is_none() {
                    self.want_xtra = true;
                }
            }
            id::GET_ENGINE_LOCK => {
                let lock = m.u32(0x10);
                if matches!(lock, Some(proto::lock::MI | proto::lock::ALL)) {
                    bus.log(format_args!("loc: engine lock {lock:?}; unlocking"));
                    self.send(bus, |t| proto::set_engine_lock(t, proto::lock::NONE));
                }
            }
            id::INJECT_ORBITS => {
                let ok = proto::ind_ok(m);
                let Some(u) = &mut self.upload else { return };
                if !ok {
                    bus.log(format_args!("loc: XTRA part {} refused", u.part));
                    self.upload = None;
                } else if u.part < u.parts {
                    u.part += 1;
                    self.send_part(bus);
                } else {
                    self.upload = None;
                    bus.log(format_args!("loc: XTRA injected"));
                    self.send(bus, proto::orbits_validity);
                }
            }
            id::ENGINE_STATE => {
                let s = m.u32(0x01).map(|v| v as i32);
                if s != self.engine {
                    self.engine = s;
                    bus.log(format_args!(
                        "loc: engine {}",
                        if s == Some(1) { "on" } else { "off" }
                    ));
                }
            }
            id::SET_ENGINE_LOCK
            | id::SET_OPERATION_MODE
            | id::INJECT_UTC_TIME
            | id::REGISTER_EVENTS
                if !proto::ind_ok(m) =>
            {
                bus.log(format_args!("loc: {:#x} failed ({:?})", m.id, m.u32(0x01)));
            }
            _ => {}
        }
    }

    fn report(&mut self, bus: &mut dyn Bus, m: &Msg, now_ms: u64) {
        let Some(r) = proto::Report::parse(m) else {
            return;
        };
        if self.started.is_none() {
            return;
        }
        if self.last_report != Some(r.status) {
            self.last_report = Some(r.status);
            if r.status != proto::session::SUCCESS {
                bus.log(format_args!("loc: session status {}", r.status));
            }
        }
        match (r.status, r.lat, r.lon) {
            (proto::session::SUCCESS, Some(lat), Some(lon)) => {
                let s = &mut self.status;
                s.state = state::FIX;
                s.lat_e7 = (lat * 1e7) as i32;
                s.lon_e7 = (lon * 1e7) as i32;
                s.h_acc_dm = r.h_unc.map_or(0, |h| (h * 10.0) as u32);
                s.alt_dm = r.alt.map_or(0, |a| (a * 10.0) as i32);
                s.utc_ms = r.utc_ms.unwrap_or(0);
                s.used = r.used.unwrap_or(0);
                if s.ttff_ms == 0 {
                    let t = self.started.map_or(0, |t| now_ms.saturating_sub(t));
                    s.ttff_ms = (t as u32).max(1);
                    bus.log(format_args!(
                        "loc: first fix after {} ms, ±{} m, {} satellites",
                        s.ttff_ms,
                        s.h_acc_dm / 10,
                        s.used
                    ));
                }
            }
            (proto::session::ENGINE_LOCKED, ..) => self.status.state = state::LOCKED,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests;
