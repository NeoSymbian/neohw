// SPDX-License-Identifier: Apache-2.0
//! Host scheduler for the S3 boundary: every client request and event crosses the NeoHub codec.
use alloc::vec::Vec;
use core::fmt;
use core::ops::{Deref, DerefMut};
use hub::{Platform, Router};
use hub_ipc::{Bus, Event, Send, decode_batch, encode_batch};
use modem::{Cellular, modem_ipc};

pub struct Modem {
    pub router: Router,
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
    pub fn support_only() -> Self {
        Self {
            router: Router::support_only(),
            cellular: Cellular::new(),
        }
    }
    #[cfg(test)]
    pub fn new(images: [Vec<u8>; 4], phys: u64) -> Self {
        Self {
            router: Router::new(images, phys),
            cellular: Cellular::new(),
        }
    }
    pub fn radio_attached(&self) -> bool {
        self.router.radio_attached()
    }
    pub fn attach_radio(&mut self, images: [Vec<u8>; 4], phys: u64) -> Result<(), i32> {
        self.router.attach_radio(images, phys)
    }
    fn sync(&mut self, p: &mut dyn Platform) {
        let state = self.router.state();
        {
            let mut bus = EncodedBus::new(&mut self.router, p);
            self.cellular.bind(&mut bus).expect("cellular ports");
            self.cellular
                .event(&mut bus, round_trip(&[Event::State(state)]).remove(0));
        }
        let mut events = Vec::new();
        while let Some(event) = self.router.next_event() {
            events.push(event);
        }
        for event in round_trip(&events) {
            self.cellular
                .event(&mut EncodedBus::new(&mut self.router, p), event);
        }
    }
    pub fn start(&mut self, p: &mut dyn Platform) {
        self.sync(p);
        self.router.start(p);
        self.sync(p);
    }
    pub fn handle(&mut self, p: &mut dyn Platform, packet: &[u8]) {
        self.sync(p);
        self.router.handle(p, packet);
        self.sync(p);
    }
    pub fn reset(&mut self, p: &mut dyn Platform, keep_adsp: bool) {
        self.router.reset(keep_adsp);
        self.sync(p);
    }
    pub fn lost(&mut self, p: &mut dyn Platform) {
        self.router.lost();
        self.sync(p);
    }
    pub fn tick(&mut self, p: &mut dyn Platform, now: u64) {
        self.sync(p);
        self.cellular
            .begin_tick(&mut EncodedBus::new(&mut self.router, p), now);
        self.router.retry(p, now);
        self.cellular
            .tick(&mut EncodedBus::new(&mut self.router, p), now);
        self.router.tick(p, now);
        self.sync(p);
        self.cellular
            .poll(&mut EncodedBus::new(&mut self.router, p), now);
    }
    pub fn airplane(&mut self, p: &mut dyn Platform, off: bool) -> Result<(), i32> {
        self.sync(p);
        self.cellular
            .airplane(&mut EncodedBus::new(&mut self.router, p), off)
    }
    pub fn verify_pin(&mut self, p: &mut dyn Platform, pin: &[u8]) -> Result<(), i32> {
        self.sync(p);
        self.cellular
            .verify_pin(&mut EncodedBus::new(&mut self.router, p), pin)
    }
    pub fn answer_call(&mut self, p: &mut dyn Platform, id: u8) -> Result<(), i32> {
        self.sync(p);
        self.cellular
            .answer_call(&mut EncodedBus::new(&mut self.router, p), id)
    }
    pub fn end_call(&mut self, p: &mut dyn Platform, id: u8) -> Result<(), i32> {
        self.sync(p);
        self.cellular
            .end_call(&mut EncodedBus::new(&mut self.router, p), id)
    }
    pub fn sms_send(&mut self, p: &mut dyn Platform, to: &str, text: &str) -> Result<u32, i32> {
        self.sync(p);
        self.cellular
            .sms_send(&mut EncodedBus::new(&mut self.router, p), to, text)
    }
    pub fn sms_edit(&mut self, p: &mut dyn Platform, action: i32, id: u32) -> Result<(), i32> {
        self.sync(p);
        self.cellular
            .sms_edit(&mut EncodedBus::new(&mut self.router, p), action, id)
    }
    pub fn sms_settings(&mut self, p: &mut dyn Platform, s: modem_ipc::SmsSettings) {
        self.sync(p);
        self.cellular
            .sms_settings(&mut EncodedBus::new(&mut self.router, p), s)
    }
    pub fn sms_saved(&mut self, p: &mut dyn Platform) {
        self.sync(p);
        self.cellular
            .sms_saved(&mut EncodedBus::new(&mut self.router, p))
    }
    pub fn manage(&mut self, p: &mut dyn Platform, action: i32, id: u8) -> Result<(), i32> {
        self.sync(p);
        self.cellular
            .manage(&mut EncodedBus::new(&mut self.router, p), action, id)
    }
    pub fn call_waiting(&mut self, p: &mut dyn Platform, action: i32) -> Result<(), i32> {
        self.sync(p);
        self.cellular
            .call_waiting(&mut EncodedBus::new(&mut self.router, p), action)
    }
    pub fn dial(&mut self, p: &mut dyn Platform, number: &str) -> Result<(), i32> {
        self.sync(p);
        self.cellular
            .dial(&mut EncodedBus::new(&mut self.router, p), number)
    }
    #[cfg(test)]
    pub fn answer(&mut self, p: &mut dyn Platform) -> Result<(), i32> {
        self.sync(p);
        self.cellular
            .answer(&mut EncodedBus::new(&mut self.router, p))
    }
    #[cfg(test)]
    pub fn hangup(&mut self, p: &mut dyn Platform) -> Result<(), i32> {
        self.sync(p);
        self.cellular
            .hangup(&mut EncodedBus::new(&mut self.router, p))
    }
    pub fn dtmf(&mut self, p: &mut dyn Platform, key: Option<u8>) -> Result<(), i32> {
        self.sync(p);
        self.cellular
            .dtmf(&mut EncodedBus::new(&mut self.router, p), key)
    }
}

/// Synchronous test server dispatches the same descriptor and scalar layouts as NeoHub.
pub struct EncodedBus<'a> {
    router: &'a mut Router,
    platform: &'a mut dyn Platform,
}
impl<'a> EncodedBus<'a> {
    pub fn new(router: &'a mut Router, platform: &'a mut dyn Platform) -> Self {
        Self { router, platform }
    }
}
impl Bus for EncodedBus<'_> {
    fn bind(&mut self, service: u32, port: u32) -> Result<(), i32> {
        // BIND uses integer arguments rather than a descriptor.
        let args = [service.to_le_bytes(), port.to_le_bytes()];
        self.router
            .bus(self.platform)
            .bind(u32::from_le_bytes(args[0]), u32::from_le_bytes(args[1]))
    }
    fn send(&mut self, port: u32, dst: (u32, u32), payload: &[u8], essential: bool) -> bool {
        let descriptor = Send {
            port,
            dst,
            payload,
            essential,
        }
        .encode();
        let request = Send::decode(&descriptor).expect("valid SEND descriptor");
        self.router.bus(self.platform).send(
            request.port,
            request.dst,
            request.payload,
            request.essential,
        )
    }
    fn log(&mut self, args: fmt::Arguments) {
        self.platform.log(args);
    }
    fn utc_ms(&mut self) -> i64 {
        self.platform.utc_ms()
    }
}
pub fn round_trip(events: &[Event]) -> Vec<Event> {
    let mut out = Vec::new();
    let mut offset = 0;
    while offset < events.len() {
        let (batch, count) = encode_batch(&events[offset..], 0x4000);
        assert!(count > 0, "every event fits the production RECV buffer");
        out.extend(decode_batch(&batch).expect("valid RECV batch"));
        offset += count;
    }
    out
}
