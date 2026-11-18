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
