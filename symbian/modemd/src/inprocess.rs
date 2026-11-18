// SPDX-License-Identifier: Apache-2.0
//! S2 composition; every cellular send still traverses the hub's ordered outbox.
use alloc::vec::Vec;
use core::ops::{Deref, DerefMut};
use hub::{Platform, Router};
use hub_ipc::Event;
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
            let mut bus = self.router.bus(p);
            self.cellular.bind(&mut bus).expect("cellular ports");
            self.cellular.event(&mut bus, Event::State(state));
        }
        while let Some(event) = self.router.next_event() {
            self.cellular.event(&mut self.router.bus(p), event);
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
        self.cellular.begin_tick(&mut self.router.bus(p), now);
        self.router.retry(p, now);
        self.cellular.tick(&mut self.router.bus(p), now);
        self.router.tick(p, now);
        self.sync(p);
        self.cellular.poll(&mut self.router.bus(p), now);
    }
    pub fn airplane(&mut self, p: &mut dyn Platform, off: bool) -> Result<(), i32> {
        self.sync(p);
        self.cellular.airplane(&mut self.router.bus(p), off)
    }
    pub fn verify_pin(&mut self, p: &mut dyn Platform, pin: &[u8]) -> Result<(), i32> {
        self.sync(p);
        self.cellular.verify_pin(&mut self.router.bus(p), pin)
    }
    pub fn answer_call(&mut self, p: &mut dyn Platform, id: u8) -> Result<(), i32> {
        self.sync(p);
        self.cellular.answer_call(&mut self.router.bus(p), id)
    }
    pub fn end_call(&mut self, p: &mut dyn Platform, id: u8) -> Result<(), i32> {
        self.sync(p);
        self.cellular.end_call(&mut self.router.bus(p), id)
    }
    pub fn sms_send(&mut self, p: &mut dyn Platform, to: &str, text: &str) -> Result<u32, i32> {
        self.sync(p);
        self.cellular.sms_send(&mut self.router.bus(p), to, text)
    }
    pub fn sms_edit(&mut self, p: &mut dyn Platform, action: i32, id: u32) -> Result<(), i32> {
        self.sync(p);
        self.cellular.sms_edit(&mut self.router.bus(p), action, id)
    }
    pub fn sms_settings(&mut self, p: &mut dyn Platform, s: modem_ipc::SmsSettings) {
        self.sync(p);
        self.cellular.sms_settings(&mut self.router.bus(p), s)
    }
    pub fn sms_saved(&mut self, p: &mut dyn Platform) {
        self.sync(p);
        self.cellular.sms_saved(&mut self.router.bus(p))
    }
    pub fn manage(&mut self, p: &mut dyn Platform, action: i32, id: u8) -> Result<(), i32> {
        self.sync(p);
        self.cellular.manage(&mut self.router.bus(p), action, id)
    }
    pub fn call_waiting(&mut self, p: &mut dyn Platform, action: i32) -> Result<(), i32> {
        self.sync(p);
        self.cellular.call_waiting(&mut self.router.bus(p), action)
    }
    pub fn dial(&mut self, p: &mut dyn Platform, number: &str) -> Result<(), i32> {
        self.sync(p);
        self.cellular.dial(&mut self.router.bus(p), number)
    }
    #[cfg(test)]
    pub fn answer(&mut self, p: &mut dyn Platform) -> Result<(), i32> {
        self.sync(p);
        self.cellular.answer(&mut self.router.bus(p))
    }
    #[cfg(test)]
    pub fn hangup(&mut self, p: &mut dyn Platform) -> Result<(), i32> {
        self.sync(p);
        self.cellular.hangup(&mut self.router.bus(p))
    }
    pub fn dtmf(&mut self, p: &mut dyn Platform, key: Option<u8>) -> Result<(), i32> {
        self.sync(p);
        self.cellular.dtmf(&mut self.router.bus(p), key)
    }
}
