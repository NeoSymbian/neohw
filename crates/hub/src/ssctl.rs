// SPDX-License-Identifier: Apache-2.0
//! SSCTL (QMI service 43, version 2): the subsystem-control service each remote processor runs. Linux's sysmon
//! (`drivers/remoteproc/qcom_sysmon.c`, reference only) tells every running processor when another one powers up:
//! the modem hears {"adsp", AFTER_POWERUP} once the ADSP starts, and the ADSP hears {"modem", AFTER_POWERUP}.
//! Without it the modem never joins the ADSP's voice session: START_VOICE stays unanswered (C6, 2026-11-09).
//! A modem restart is announced the same way ("modem" before/after shutdown, before power-up, to the ADSP), and a
//! running modem is first asked to shut down (SHUTDOWN_REQ, answered by a response, then SHUTDOWN_READY_IND).

use alloc::vec::Vec;

pub const SERVICE: u32 = 43;
pub const VERSION: u32 = 2;
/// The instances (the processors' ssctl ids): the modem's 0x12, the ADSP's 0x14
pub const MODEM: u32 = 0x12;
pub const ADSP: u32 = 0x14;
pub const SUBSYS_EVENT_REQ: u16 = 0x23;
/// SHUTDOWN_REQ (no TLVs) and SHUTDOWN_READY_IND share one id (Linux `qcom_sysmon.c`)
pub const SHUTDOWN: u16 = 0x21;
/// `ssctl_ssr_event_enum`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    BeforePowerup = 0,
    AfterPowerup = 1,
    BeforeShutdown = 2,
    AfterShutdown = 3,
}
const FORCED: u32 = 0;

/// The TLVs of SUBSYS_EVENT_REQ {`name`, `event`}: the name with a u8 length, the event, evt_driven = forced.
pub fn event_tlvs(name: &str, event: Event) -> [(u8, Vec<u8>); 3] {
    let mut n = Vec::with_capacity(1 + name.len());
    n.push(name.len() as u8);
    n.extend_from_slice(name.as_bytes());
    [
        (0x01, n),
        (0x02, (event as u32).to_le_bytes().to_vec()),
        (0x10, FORCED.to_le_bytes().to_vec()),
    ]
}
