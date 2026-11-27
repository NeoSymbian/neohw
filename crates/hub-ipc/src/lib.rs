// SPDX-License-Identifier: Apache-2.0
//! Payload boundary between the QRTR hub and its clients.
#![no_std]
extern crate alloc;
use alloc::vec::Vec;
use core::fmt;

mod wire;
pub use wire::*;
mod queue;
pub use queue::SessionQueue;

#[cfg(test)]
mod tests;

/// Zero a buffer that held a secret (a SIM PIN in a QMI request) so the write isn't optimised away before it is
/// freed. Every copy MODEMD, HWHUB and QRTR make of an outbound packet is wiped this way after sending.
pub fn wipe(b: &mut [u8]) {
    for x in b.iter_mut() {
        // SAFETY: `x` is a valid, aligned, exclusive reference.
        unsafe { core::ptr::write_volatile(x, 0) };
    }
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
}

/// QMI UIM requests that carry a PIN (Set PIN Protection, Verify, Unblock, Change): never dumped in a trace.
pub fn secret_request(qmi_id: u16) -> bool {
    matches!(qmi_id, 0x25..=0x28)
}

/// Secure IDs (= UID3, `exes.txt`) of the NeoHub clients and of HWHUB itself.
pub mod sid {
    pub const MODEMD: u32 = 0xE000_3001;
    pub const HWHUB: u32 = 0xE000_3002;
    pub const GNSSD: u32 = 0xE000_3004;
    pub const WIFID: u32 = 0xE000_4002;
}

/// The daemon (by secure ID) that alone may bind, and so send to, a QRTR service through HWHUB: QMI UIM carries SIM
/// PINs, so no other process may reach it (or any modem service) by binding it first.
pub fn service_owner(service: u32) -> Option<u32> {
    match service {
        2 | 3 | 5 | 9 | 11 => Some(sid::MODEMD), // DMS, NAS, WMS, VOICE, UIM
        16 => Some(sid::GNSSD),                  // LOC
        69 => Some(sid::WIFID),                  // WLFW
        _ => None,
    }
}

/// A discovered QRTR server; instance packs version in the low byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Server {
    pub service: u32,
    pub instance: u32,
    pub node: u32,
    pub port: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Subsystem {
    Mss,
    All,
}

/// Hub-owned facts needed by cellular policy, independent of NeoModem status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct State {
    pub radio_attached: bool,
    pub radio_enabled: bool,
    pub radio_failed: bool,
    pub ipa_up: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Packet {
        src: (u32, u32),
        dst_port: u32,
        payload: Vec<u8>,
    },
    ServerUp(Server),
    ServerDown(Server),
    Reset {
        subsystem: Subsystem,
        generation: u32,
    },
    State(State),
    RadioStarted,
    RadioResponse,
    Status(Snapshot),
    Dropped(u32),
}

pub trait Bus {
    /// Bind a service's local port; a port cannot belong to two services.
    fn bind(&mut self, service: u32, port: u32) -> Result<(), i32>;
    /// True if physically sent. Essential payloads are accepted into the hub's shared ordered retry queue
    /// even when false; callers must not allocate a second transaction to retry them.
    fn send(&mut self, port: u32, dst: (u32, u32), payload: &[u8], essential: bool) -> bool;
    fn log(&mut self, args: fmt::Arguments);
    /// UTC Unix milliseconds, or zero before the clock is known.
    fn utc_ms(&mut self) -> i64;
}
