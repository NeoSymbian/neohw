// SPDX-License-Identifier: Apache-2.0
//! S3 cellular facade; every send crosses NeoHub and its shared ordered outbox.
use alloc::rc::Rc;
use core::fmt;
use core::ops::{Deref, DerefMut};
use e32::ipc::{Session, TIpcArgs, TVersion};
use e32::sys::{TPtr8, TPtrC8};
use e32::{dprintln, time};

use hub_ipc::{Bus, Event, Snapshot};
use modem::{Cellular, modem_ipc};

pub struct Modem {
    session: Option<Rc<Session>>,
    cellular: Cellular,
}

impl Deref for Modem {
    type Target = Cellular;
    fn deref(&self) -> &Cellular {
        &self.cellular
    }
}
impl DerefMut for Modem {
    fn deref_mut(&mut self) -> &mut Cellular {
        &mut self.cellular
    }
}

impl Modem {
    pub fn new() -> Self {
        Self {
            session: None,
            cellular: Cellular::new(),
        }
    }
    pub fn session(&self) -> Option<Rc<Session>> {
        self.session.clone()
    }
    pub fn attach(&mut self, session: Session) -> Result<Snapshot, i32> {
        let s = snapshot(&session)?;
        let mut bus = RemoteBus(Some(&session));
        // Fresh startup and detach already have no QMI clients. Keep loaded C7-C9 stores.
        self.cellular.status.generation = s.generation;
        self.cellular.event(&mut bus, Event::State(s.state));
        self.cellular.bind(&mut bus)?;
        if s.phase == hub_ipc::phase::RUNNING {
            self.cellular.event(&mut bus, Event::RadioStarted);
        }
        self.session = Some(Rc::new(session));
        Ok(s)
    }
    pub fn detach(&mut self) {
        self.session = None;
        self.event(Event::Reset {
            subsystem: hub_ipc::Subsystem::All,
            generation: self.cellular.status.generation.wrapping_add(1),
        });
        self.cellular
            .event(&mut RemoteBus(None), Event::State(Default::default()));
    }
    pub fn event(&mut self, e: Event) {
        self.cellular
            .event(&mut RemoteBus(self.session.as_deref()), e);
    }
    pub fn snapshot(&self) -> Result<Snapshot, i32> {
        snapshot(self.session.as_deref().ok_or(-18)?)
    }
    pub fn recover(&self, mask: u32, reason: i32) -> i32 {
        let Some(s) = &self.session else { return -18 };
        let mut args = TIpcArgs::new();
        args.int(0, mask as i32);
        args.int(1, reason);
        s.send_receive(hub_ipc::op::RECOVER, &args)
            .unwrap_or_else(|e| e)
    }
    pub fn quiesced(&self, generation: u32) -> i32 {
        self.scalar(hub_ipc::op::QUIESCED, generation)
    }
    fn scalar(&self, op: i32, value: u32) -> i32 {
        let Some(s) = &self.session else { return -18 };
        let mut args = TIpcArgs::new();
        args.int(0, value as i32);
        s.send_receive(op, &args).unwrap_or_else(|e| e)
    }
    pub fn tick(&mut self, _p: &mut Hal, now: u64) {
        let mut bus = RemoteBus(self.session.as_deref());
        self.cellular.begin_tick(&mut bus, now);
        self.cellular.tick(&mut bus, now);
        self.cellular.poll(&mut bus, now);
    }
    pub fn airplane(&mut self, _p: &mut Hal, off: bool) -> Result<(), i32> {
        self.cellular
            .airplane(&mut RemoteBus(self.session.as_deref()), off)
    }
    pub fn verify_pin(&mut self, _p: &mut Hal, pin: &[u8]) -> Result<(), i32> {
        self.cellular
            .verify_pin(&mut RemoteBus(self.session.as_deref()), pin)
    }
    pub fn pin_protection(
        &mut self,
        _p: &mut Hal,
        pin: &[u8],
        on: bool,
        epoch: u32,
        generation: u32,
    ) -> Result<(), i32> {
        self.cellular.pin_protection(
            &mut RemoteBus(self.session.as_deref()),
            pin,
            on,
            epoch,
            generation,
        )
    }
    pub fn change_pin(
        &mut self,
        _p: &mut Hal,
        old: &[u8],
        new: &[u8],
        epoch: u32,
        generation: u32,
    ) -> Result<(), i32> {
        self.cellular.change_pin(
            &mut RemoteBus(self.session.as_deref()),
            old,
            new,
            epoch,
            generation,
        )
    }
    pub fn answer_call(&mut self, _p: &mut Hal, id: u8) -> Result<(), i32> {
        self.cellular
            .answer_call(&mut RemoteBus(self.session.as_deref()), id)
    }
    pub fn end_call(&mut self, _p: &mut Hal, id: u8) -> Result<(), i32> {
        self.cellular
            .end_call(&mut RemoteBus(self.session.as_deref()), id)
    }
    pub fn sms_send(&mut self, _p: &mut Hal, to: &str, text: &str) -> Result<u32, i32> {
        self.cellular
            .sms_send(&mut RemoteBus(self.session.as_deref()), to, text)
    }
    pub fn sms_edit(&mut self, _p: &mut Hal, action: i32, id: u32) -> Result<(), i32> {
        self.cellular
            .sms_edit(&mut RemoteBus(self.session.as_deref()), action, id)
    }
    pub fn sms_settings(&mut self, _p: &mut Hal, s: modem_ipc::SmsSettings) {
        self.cellular
            .sms_settings(&mut RemoteBus(self.session.as_deref()), s)
    }
    pub fn sms_saved(&mut self, _p: &mut Hal) {
        self.cellular
            .sms_saved(&mut RemoteBus(self.session.as_deref()))
    }
    pub fn manage(&mut self, _p: &mut Hal, action: i32, id: u8) -> Result<(), i32> {
        self.cellular
            .manage(&mut RemoteBus(self.session.as_deref()), action, id)
    }
    pub fn call_waiting(&mut self, _p: &mut Hal, action: i32) -> Result<(), i32> {
        self.cellular
            .call_waiting(&mut RemoteBus(self.session.as_deref()), action)
    }
    pub fn dial(&mut self, _p: &mut Hal, number: &str) -> Result<(), i32> {
        self.cellular
            .dial(&mut RemoteBus(self.session.as_deref()), number)
    }
    #[cfg(test)]
    pub fn answer(&mut self, _p: &mut Hal) -> Result<(), i32> {
        self.cellular
            .answer(&mut RemoteBus(self.session.as_deref()))
    }
    #[cfg(test)]
    pub fn hangup(&mut self, _p: &mut Hal) -> Result<(), i32> {
        self.cellular
            .hangup(&mut RemoteBus(self.session.as_deref()))
    }
    pub fn dtmf(&mut self, _p: &mut Hal, key: Option<u8>) -> Result<(), i32> {
        self.cellular
            .dtmf(&mut RemoteBus(self.session.as_deref()), key)
    }
}

/// Logging and UTC retain the Cellular API's local context; transport belongs to NeoHub.
pub struct Hal;
impl Hal {
    pub fn utc_ms(&mut self) -> i64 {
        utc_ms()
    }
}
fn utc_ms() -> i64 {
    match time::utc_now().0 {
        u if u < 1_600_000_000 => 0,
        u => u * 1000,
    }
}
struct RemoteBus<'a>(Option<&'a Session>);
impl Bus for RemoteBus<'_> {
    fn bind(&mut self, service: u32, port: u32) -> Result<(), i32> {
        let s = self.0.ok_or(-18)?;
        let mut args = TIpcArgs::new();
        args.int(0, service as i32).int(1, port as i32);
        s.send_receive(hub_ipc::op::BIND, &args).map(|_| ())
    }
    fn send(&mut self, port: u32, dst: (u32, u32), payload: &[u8], essential: bool) -> bool {
        let Some(s) = self.0 else { return false };
        let mut bytes = hub_ipc::Send {
            port,
            dst,
            essential,
            payload,
        }
        .encode();
        let sent = {
            let des = TPtrC8::new(&bytes);
            let mut args = TIpcArgs::new();
            args.desc8(0, &des);
            matches!(s.send_receive(hub_ipc::op::SEND, &args), Ok(n) if n != 0)
        };
        hub_ipc::wipe(&mut bytes); // it may carry a SIM PIN
        sent
    }
    fn log(&mut self, args: fmt::Arguments) {
        dprintln!("[modemd] t={}ms {}", time::millis(), args);
    }
    fn utc_ms(&mut self) -> i64 {
        utc_ms()
    }
}
fn snapshot(session: &Session) -> Result<Snapshot, i32> {
    let mut bytes = [0; Snapshot::SIZE];
    let mut des = TPtr8::new(&mut bytes);
    let mut args = TIpcArgs::new();
    args.des8(0, &mut des);
    session.send_receive(hub_ipc::op::STATUS, &args)?;
    Snapshot::from_bytes(bytes.get(..des.len()).ok_or(-20)?).ok_or(-20)
}

pub fn connect() -> Result<Session, i32> {
    let (major, minor, build) = hub_ipc::VERSION;
    Session::connect(
        hub_ipc::NAME,
        TVersion {
            major: major as u8,
            minor: minor as u8,
            build: build as u16,
        },
    )
}

/// This future and its descriptors stay pinned in main across every server/timer wakeup.
/// A completed receive is dropped before its buffer is decoded or a session is replaced.
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
