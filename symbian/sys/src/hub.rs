// SPDX-License-Identifier: Apache-2.0
//! The small NeoHub client of WIFID and GNSSD (MODEMD keeps its own, `modemd/src/client.rs`): a session with HWHUB,
//! a [`Bus`] whose sends cross to it, the asynchronous receive, and the answer to a reset.
use core::fmt;
use e32::ipc::{Session, TIpcArgs, TVersion};
use e32::sys::{TPtr8, TPtrC8};
use e32::{dprintln, time};
use hub_ipc::Bus;

/// A session with HWHUB, or `None` while it isn't up (MODEMD starts it).
pub fn connect() -> Option<Session> {
    let (major, minor, build) = hub_ipc::VERSION;
    Session::connect(
        hub_ipc::NAME,
        TVersion {
            major: major as u8,
            minor: minor as u8,
            build: build as u16,
        },
    )
    .ok()
}

/// Tell HWHUB this client let go of the restarting subsystem.
pub fn quiesced(s: &Session, generation: u32) {
    let mut args = TIpcArgs::new();
    args.int(0, generation as i32);
    let _ = s.send_receive(hub_ipc::op::QUIESCED, &args);
}

/// The next batch of events into `bytes`; pending for ever without a session.
pub async fn receive(session: Option<&Session>, bytes: &mut [u8]) -> Result<usize, i32> {
    let Some(s) = session else {
        return core::future::pending().await;
    };
    let mut des = TPtr8::new(bytes);
    let mut args = TIpcArgs::new();
    args.des8(0, &mut des);
    s.send(hub_ipc::op::RECV, &args).await?;
    Ok(des.len())
}

/// The client's view of the hub: sends cross to HWHUB, logs go to the debug port as `[tag] t=..ms ...`.
pub struct HubBus<'a> {
    pub session: Option<&'a Session>,
    pub tag: &'static str,
}

impl<'a> HubBus<'a> {
    pub fn new(session: Option<&'a Session>, tag: &'static str) -> Self {
        HubBus { session, tag }
    }
}

impl Bus for HubBus<'_> {
    fn bind(&mut self, service: u32, port: u32) -> Result<(), i32> {
        let s = self.session.ok_or(-18)?;
        let mut args = TIpcArgs::new();
        args.int(0, service as i32).int(1, port as i32);
        s.send_receive(hub_ipc::op::BIND, &args).map(|_| ())
    }
    fn send(&mut self, port: u32, dst: (u32, u32), payload: &[u8], essential: bool) -> bool {
        let Some(s) = self.session else { return false };
        let bytes = hub_ipc::Send {
            port,
            dst,
            essential,
            payload,
        }
        .encode();
        let des = TPtrC8::new(&bytes);
        let mut args = TIpcArgs::new();
        args.desc8(0, &des);
        matches!(s.send_receive(hub_ipc::op::SEND, &args), Ok(n) if n != 0)
    }
    fn log(&mut self, args: fmt::Arguments) {
        dprintln!("[{}] t={}ms {}", self.tag, time::millis(), args);
    }
    /// UTC ms once the clock was set (NITZ, an HTTP Date or the RTC after a set), else 0.
    fn utc_ms(&mut self) -> i64 {
        match time::utc_now().0 {
            u if u < 1_600_000_000 => 0,
            u => u * 1000,
        }
    }
}
