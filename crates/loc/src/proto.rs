// SPDX-License-Identifier: Apache-2.0
//! QMI LOC (service 16, v2): the requests GNSSD sends and the indications it reads (docs hardware/gnss.md).
//! Ids, TLVs and enums from libqmi's `data/qmi-service-loc.json`, `qmi-enums-loc.h` and `qmi-flags64-loc.h`.
use alloc::string::String;
use alloc::vec::Vec;
use qrtr::qmi::{Builder, Msg, REQUEST};

pub const SERVICE: u32 = 16;
/// our only fix session
pub const SESSION: u8 = 1;

pub mod id {
    pub const REGISTER_EVENTS: u16 = 0x21;
    pub const START: u16 = 0x22;
    pub const STOP: u16 = 0x23;
    pub const POSITION_REPORT: u16 = 0x24;
    pub const SV_INFO: u16 = 0x25;
    pub const INJECT_TIME_REQUEST: u16 = 0x28;
    pub const ORBITS_REQUEST: u16 = 0x29;
    pub const ENGINE_STATE: u16 = 0x2b;
    pub const INJECT_ORBITS: u16 = 0x35;
    pub const ORBITS_SOURCE: u16 = 0x36;
    pub const ORBITS_VALIDITY: u16 = 0x37;
    pub const INJECT_UTC_TIME: u16 = 0x38;
    pub const SET_ENGINE_LOCK: u16 = 0x3a;
    pub const GET_ENGINE_LOCK: u16 = 0x3b;
    pub const SET_OPERATION_MODE: u16 = 0x4a;
}

/// Register Events: position report (bit 0), satellites (1), inject time request (4), predicted orbits request (5),
/// engine state (7)
pub const EVENTS: u64 = 1 | 1 << 1 | 1 << 4 | 1 << 5 | 1 << 7;

pub mod lock {
    pub const NONE: u32 = 1;
    /// mobile-initiated fixes (ours) locked
    pub const MI: u32 = 2;
    pub const MT: u32 = 3;
    pub const ALL: u32 = 4;
}

pub const MODE_STANDALONE: u32 = 4;

/// Position Report session status
pub mod session {
    pub const SUCCESS: u32 = 0;
    pub const IN_PROGRESS: u32 = 1;
    pub const ENGINE_LOCKED: u32 = 6;
}

/// The largest XTRA part the request format allows (TLV 0x04's max-size)
pub const PART_MAX: usize = 1024;

fn req(txn: u16, id: u16) -> Builder {
    Builder::new(REQUEST, txn, id)
}

pub fn register_events(txn: u16) -> Vec<u8> {
    req(txn, id::REGISTER_EVENTS).u64(0x01, EVENTS).finish()
}

/// Periodic fixes, intermediate reports on, every `interval_ms`.
pub fn start(txn: u16, interval_ms: u32) -> Vec<u8> {
    req(txn, id::START)
        .u8(0x01, SESSION)
        .u32(0x10, 1)
        .u32(0x12, 1)
        .u32(0x13, interval_ms)
        .finish()
}

pub fn stop(txn: u16) -> Vec<u8> {
    req(txn, id::STOP).u8(0x01, SESSION).finish()
}

pub fn get_engine_lock(txn: u16) -> Vec<u8> {
    req(txn, id::GET_ENGINE_LOCK).finish()
}

pub fn set_engine_lock(txn: u16, lock: u32) -> Vec<u8> {
    req(txn, id::SET_ENGINE_LOCK).u32(0x01, lock).finish()
}

pub fn set_operation_mode(txn: u16, mode: u32) -> Vec<u8> {
    req(txn, id::SET_OPERATION_MODE).u32(0x01, mode).finish()
}

pub fn orbits_source(txn: u16) -> Vec<u8> {
    req(txn, id::ORBITS_SOURCE).finish()
}

pub fn orbits_validity(txn: u16) -> Vec<u8> {
    req(txn, id::ORBITS_VALIDITY).finish()
}

pub fn inject_utc_time(txn: u16, utc_ms: u64, uncertainty_ms: u32) -> Vec<u8> {
    req(txn, id::INJECT_UTC_TIME)
        .u64(0x01, utc_ms)
        .u32(0x02, uncertainty_ms)
        .finish()
}

/// One part (`part` counts from 1) of an XTRA file of `total` bytes in `parts` parts.
pub fn inject_orbits(txn: u16, total: u32, parts: u16, part: u16, data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(2 + data.len());
    v.extend_from_slice(&(data.len() as u16).to_le_bytes());
    v.extend_from_slice(data);
    req(txn, id::INJECT_ORBITS)
        .u32(0x01, total)
        .u16(0x02, parts)
        .u16(0x03, part)
        .tlv(0x04, &v)
        .u32(0x10, 0) // format: XTRA
        .finish()
}

fn bytes<const N: usize>(m: &Msg, ty: u8) -> Option<[u8; N]> {
    m.tlv(ty)?.get(..N)?.try_into().ok()
}

pub fn u16_of(m: &Msg, ty: u8) -> Option<u16> {
    bytes(m, ty).map(u16::from_le_bytes)
}

pub fn u64_of(m: &Msg, ty: u8) -> Option<u64> {
    bytes(m, ty).map(u64::from_le_bytes)
}

fn f32_of(m: &Msg, ty: u8) -> Option<f32> {
    bytes(m, ty).map(f32::from_le_bytes)
}

fn f64_of(m: &Msg, ty: u8) -> Option<f64> {
    bytes(m, ty).map(f64::from_le_bytes)
}

/// An indication's status (TLV 0x01): true = success.
pub fn ind_ok(m: &Msg) -> bool {
    m.u32(0x01) == Some(0)
}

/// A Position Report (0x24).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Report {
    pub status: u32,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    /// horizontal uncertainty, circular, metres
    pub h_unc: Option<f32>,
    /// metres above mean sea level
    pub alt: Option<f32>,
    pub utc_ms: Option<u64>,
    /// satellites used in the fix
    pub used: Option<u8>,
}

impl Report {
    pub fn parse(m: &Msg) -> Option<Self> {
        Some(Report {
            status: m.u32(0x01)?,
            lat: f64_of(m, 0x10),
            lon: f64_of(m, 0x11),
            h_unc: f32_of(m, 0x12),
            alt: f32_of(m, 0x1b),
            utc_ms: u64_of(m, 0x25),
            used: m.u8(0x2c),
        })
    }
}

/// One GNSS SV Info (0x25) list entry: `{valid u32, system u32, id u16, health u8, status u32, navigation u8,
/// elevation f32, azimuth f32, SNR f32}`, packed.
const SV_SIZE: usize = 28;
const SV_SNR_VALID: u32 = 1 << 7;

/// GNSS SV Info (0x25): `(listed, heard, best SNR in dB-Hz)`. The list holds every satellite the engine works on,
/// searched for or tracked; "heard" = a valid SNR above 0.
pub fn satellites(m: &Msg) -> Option<(u8, u8, u8)> {
    let v = m.tlv(0x10)?;
    let (&n, rest) = v.split_first()?;
    let (mut heard, mut best) = (0u8, 0f32);
    let (entries, _) = rest.as_chunks::<SV_SIZE>();
    for e in entries.iter().take(n as usize) {
        let valid = u32::from_le_bytes(e[..4].try_into().ok()?);
        let snr = f32::from_le_bytes(e[24..28].try_into().ok()?);
        if valid & SV_SNR_VALID != 0 && snr > 0.0 {
            heard = heard.saturating_add(1);
            best = best.max(snr);
        }
    }
    Some((n, heard, best.min(255.0) as u8))
}

/// `{max file size, max part size}`
pub fn sizes(v: &[u8]) -> Option<(u32, u32)> {
    Some((
        u32::from_le_bytes(v.get(..4)?.try_into().ok()?),
        u32::from_le_bytes(v.get(4..8)?.try_into().ok()?),
    ))
}

/// A server list: a u8 count of strings, each with a u8 length. Stops at the first malformed entry.
pub fn servers(v: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let Some((&n, mut rest)) = v.split_first() else {
        return out;
    };
    for _ in 0..n {
        let Some((&len, r)) = rest.split_first() else {
            break;
        };
        let Some(s) = r.get(..len as usize) else {
            break;
        };
        if let Ok(s) = core::str::from_utf8(s) {
            out.push(String::from(s.trim_end_matches('\0')));
        }
        rest = &r[len as usize..];
    }
    out
}

/// Predicted orbits validity: `{start UTC ms u64, duration hours u16}` -> the end, UTC ms.
pub fn valid_until(v: &[u8]) -> Option<u64> {
    let start = u64::from_le_bytes(v.get(..8)?.try_into().ok()?);
    let hours = u16::from_le_bytes(v.get(8..10)?.try_into().ok()?);
    Some(start + hours as u64 * 3_600_000)
}
