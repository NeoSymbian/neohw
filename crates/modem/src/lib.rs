// SPDX-License-Identifier: Apache-2.0
//! Cellular QMI clients and NeoModem state, over the hub payload boundary.
#![cfg_attr(not(test), no_std)]
extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;
pub use hub_ipc::{Bus, Event, Server, State};
pub use modem_ipc;
use modem_ipc::{Call, Status, Text, call};
pub use phone_rules;
pub use qrtr::qmi;
pub use sms;
pub mod calllog;
pub mod messages;
pub mod nas;
pub mod uim;
pub mod voice;
pub mod wms;
const MODEM_NODE: u32 = 0;
const DMS: u32 = 2;
const UIM: u32 = 11;
const NAS: u32 = nas::SERVICE;
const VOICE: u32 = voice::SERVICE;
const WMS: u32 = wms::SERVICE;
/// Our QMI clients: (service, our port)
const CLIENTS: [(u32, u32); 5] = [
    (DMS, 0x20),
    (UIM, 0x21),
    (NAS, 0x22),
    (VOICE, 0x23),
    (WMS, 0x27),
];
/// A part with the modem this long without an answer has failed.
const SMS_SEND_MS: u64 = 60_000;
const DMS_GET_REVISION: u16 = 0x23;
const DMS_GET_IDS: u16 = 0x25;
const DMS_SET_OPERATING_MODE: u16 = 0x2e;
const UIM_GET_CARD_STATUS: u16 = 0x2f;
/// NAS polls, as ModemManager does (indications cover the rest)
const POLL_MS: u64 = 30_000;
const POST_CALL_MS: u64 = 30_000;
pub const KERR_NOT_READY: i32 = -18;
pub const KERR_ARGUMENT: i32 = -6;
pub const KERR_TOO_BIG: i32 = -40;
pub const KERR_IN_USE: i32 = -14;

/// One QMI client: its server, our port, the transaction counter and the requests still unanswered (txn, id).
struct Client {
    service: u32,
    dst: (u32, u32),
    port: u32,
    txn: u16,
    pending: Vec<(u16, u16)>,
}

pub struct Cellular {
    pub revision: Option<String>,
    pub imei: Option<String>,
    /// The first card's state from UIM: 0 absent, 1 present, 2 error.
    pub card: Option<u8>,
    /// What NeoModem clients see (modem_ipc::op::STATUS)
    pub status: Status,
    clients: Vec<Client>,
    online_sent: bool,
    plmn_asked: Option<(u16, u16)>,
    /// Get Network Time was sent (once registered): the indication may have come before we subscribed
    time_asked: bool,
    hangup_pending: bool,
    /// the call watchdog: the (id, state) tick last saw, since when (ms), and when END was last tried
    call_seen: (u8, u8),
    call_since: u64,
    hangup_at: u64,
    last_poll: u64,
    /// (final END observed, next read-only NAS query), bounded to 30 seconds.
    post_call: Option<(u64, u64)>,
    /// NAS circuit-switched attach, for the log
    cs: u8,
    selection_sent: bool,
    command: Option<(u32, u16, u16, u64)>,
    now: u64,
    radio_request: Option<(u16, bool)>,
    radio_at: u64,
    /// refused Set Operating Mode requests for the current intent (bounded by RADIO_REFUSALS)
    radio_refusals: u8,
    pin_id: u8,
    answered: Vec<u8>,
    /// The call log (C8); MODEMD saves it when `log.dirty`.
    pub log: calllog::CallLog,
    /// Caller ID for our calls: modem_ipc::clir (C8).
    pub clir: i32,
    /// the call waiting request with the network (modem_ipc::waiting)
    waiting_request: i32,
    /// a call being rejected as busy (UDUB); END follows if the modem refuses it
    rejecting: Option<u8>,
    /// Incoming calls rejected before they ring (modem_ipc::op::BLOCK_SET, KNOWN_SET); MODEMD keeps it in block.db.
    pub block: phone_rules::block::List,
    /// The messages (C9); MODEMD saves them when `sms.dirty`, then calls [`Cellular::sms_saved`].
    pub sms: messages::Store,
    /// what waits until the store is saved: acknowledgements, deletions from the modem's storage
    sms_after_save: Vec<AfterSave>,
    /// messages to send: (id, TPDUs, the next part); one part is with the modem at a time
    sms_out: Vec<(u32, Vec<Vec<u8>>, usize)>,
    /// that part: (id, txn, last part, since)
    sms_in_flight: Option<(u32, u16, bool, u64)>,
    /// Raw Read requests (txn, storage, index) and List Messages requests (txn, storage)
    sms_reads: Vec<(u16, u8, u32)>,
    sms_lists: Vec<(u16, u8)>,
    /// the SMSC was read (and storage listed) once the SIM's subscription was ready; until then `tick` asks again
    sms_ready: bool,
    sms_retry_at: u64,
    response_seen: bool,
    heartbeat_at: u64,
    state: State,
}
/// What waits until the messages are saved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AfterSave {
    /// acknowledge a transfer-route message (its transaction)
    Ack(u32),
    /// delete a message the modem stored (storage, index)
    Delete(u8, u32),
}

const RADIO_REFUSALS: u8 = 3;
/// "350000000000012" -> "35xxxxxxxxxxx12": never log a whole IMEI.
pub fn mask(id: &str) -> String {
    id.char_indices()
        .map(|(i, c)| if i < 2 || i + 2 >= id.len() { c } else { 'x' })
        .collect()
}

impl Default for Cellular {
    fn default() -> Self {
        Self::new()
    }
}
impl Cellular {
    pub fn new() -> Self {
        Self {
            revision: None,
            imei: None,
            card: None,
            // 255: unknown until UIM reports it (0 would read as "no tries left")
            status: Status {
                pin_attempts: 255,
                ..Status::default()
            },
            clients: Vec::new(),
            online_sent: false,
            plmn_asked: None,
            time_asked: false,
            hangup_pending: false,
            call_seen: (0, 0),
            call_since: 0,
            hangup_at: 0,
            last_poll: 0,
            post_call: None,
            cs: 0,
            selection_sent: false,
            command: None,
            now: 0,
            radio_request: None,
            radio_at: 0,
            radio_refusals: 0,
            pin_id: 1,
            answered: Vec::new(),
            log: calllog::CallLog::default(),
            clir: modem_ipc::clir::NETWORK,
            waiting_request: 0,
            rejecting: None,
            block: phone_rules::block::List::default(),
            sms: messages::Store::default(),
            sms_after_save: Vec::new(),
            sms_out: Vec::new(),
            sms_in_flight: None,
            sms_reads: Vec::new(),
            sms_lists: Vec::new(),
            sms_ready: false,
            sms_retry_at: 0,
            response_seen: false,
            heartbeat_at: 0,
            state: State::default(),
        }
    }
    pub fn bind(&self, bus: &mut dyn Bus) -> Result<(), i32> {
        for (service, port) in CLIENTS {
            bus.bind(service, port)?;
        }
        Ok(())
    }
    pub fn event(&mut self, p: &mut dyn Bus, event: Event) {
        match event {
            Event::ServerUp(s) => self.client_up(p, s),
            // C5 discovery removal did not tear down an established client.
            Event::ServerDown(_) => {}
            Event::Packet {
                src,
                dst_port,
                payload,
            } => {
                if self
                    .clients
                    .iter()
                    .any(|c| c.port == dst_port && c.dst == src)
                {
                    self.client(p, dst_port, &payload);
                } else {
                    p.log(format_args!(
                        "qrtr: {} bytes for unserved port {dst_port} from {}:{}, dropped",
                        payload.len(),
                        src.0,
                        src.1
                    ));
                }
            }
            Event::State(s) => {
                self.state = s;
                if s.radio_failed {
                    self.status.modem = modem_ipc::modem::FAILED;
                }
            }
            Event::RadioResponse => self.response_seen = true,
            Event::RadioStarted => self.status.modem = modem_ipc::modem::RUNNING,
            Event::Reset { generation, .. } => self.reset(generation),
            // Process status and transport loss accounting belong to the NeoHub adapter.
            Event::Status(_) | Event::Dropped(_) => {}
        }
    }

    /// One QMI request from our client of `service`; false if the modem hasn't announced that service or the
    /// transport refused the packet.
    fn request(&mut self, p: &mut dyn Bus, service: u32, id: u16, tlvs: &[(u8, &[u8])]) -> bool {
        self.qmi_request(p, service, id, tlvs, false)
    }

    /// A request that must arrive (`send_must`): true once sent or queued, false only without the service.
    fn request_must(
        &mut self,
        p: &mut dyn Bus,
        service: u32,
        id: u16,
        tlvs: &[(u8, &[u8])],
    ) -> bool {
        self.qmi_request(p, service, id, tlvs, true)
    }

    fn qmi_request(
        &mut self,
        p: &mut dyn Bus,
        service: u32,
        id: u16,
        tlvs: &[(u8, &[u8])],
        must: bool,
    ) -> bool {
        let Some(c) = self.clients.iter_mut().find(|c| c.service == service) else {
            return false;
        };
        c.txn = c.txn.wrapping_add(1).max(1);
        let mut b = qmi::Builder::new(qmi::REQUEST, c.txn, id);
        for (t, v) in tlvs {
            b = b.tlv(*t, v);
        }
        if c.pending.len() >= 16 {
            c.pending.remove(0); // never answered: forget the oldest
        }
        c.pending.push((c.txn, id));
        let (port, dst) = (c.port, c.dst);
        if must {
            p.send(port, dst, &b.finish(), true);
            return true;
        }
        p.send(port, dst, &b.finish(), false)
    }

    /// The first requests once the modem announces a service we are a client of.
    fn client_up(&mut self, p: &mut dyn Bus, s: Server) {
        if s.node == MODEM_NODE
            && (!self.state.radio_attached || !self.state.radio_enabled || self.state.radio_failed)
        {
            return;
        }
        let Some(&(_, port)) = CLIENTS.iter().find(|c| c.0 == s.service) else {
            return;
        };
        let dst = (s.node, s.port);
        match self.clients.iter_mut().find(|c| c.service == s.service) {
            Some(c) => c.dst = dst,
            None => self.clients.push(Client {
                service: s.service,
                dst,
                port,
                txn: 0,
                pending: Vec::new(),
            }),
        }
        match s.service {
            DMS => {
                self.request_must(p, DMS, DMS_GET_REVISION, &[]);
                self.request_must(p, DMS, DMS_GET_IDS, &[]);
                self.maybe_online(p);
            }
            UIM => {
                self.request_must(p, UIM, uim::REGISTER, &[(1, &1u32.to_le_bytes())]);
                self.request_must(p, UIM, UIM_GET_CARD_STATUS, &[]);
            }
            NAS => {
                self.request_must(p, NAS, nas::REGISTER_INDICATIONS, &nas::INDICATIONS);
                self.request(p, NAS, nas::GET_SERVING_SYSTEM, &[]);
                self.request(p, NAS, nas::GET_SIGNAL_INFO, &[]);
            }
            VOICE => {
                self.request_must(p, VOICE, voice::INDICATION_REGISTER, &voice::INDICATIONS);
            }
            WMS => {
                self.request_must(p, WMS, wms::SET_EVENT_REPORT, &wms::EVENT_REPORT);
                self.request_must(
                    p,
                    WMS,
                    wms::SET_ROUTES,
                    &[(0x01, &wms::routes()), (0x10, &[1])],
                );
                self.sms_start_reads(p);
                self.sms_in_flight = None;
                self.send_next_sms(p);
            }
            _ => {}
        }
    }

    /// The radio on (DMS Set Operating Mode online) once there is a SIM: ModemManager does the same on enable.
    fn maybe_online(&mut self, p: &mut dyn Bus) {
        // a disabled, terminally failed or storage-less radio is never switched (ADSP-only service, PR #2)
        if !self.state.radio_enabled || self.state.radio_failed || !self.state.radio_attached {
            return;
        }
        // one request at a time, 2 s apart, and at most RADIO_REFUSALS refused ones per intent: the modem answers
        // "invalid transition" when it is already in the mode (C5), which must not become a resend every 2 s
        if self.radio_request.is_some()
            || (self.online_sent && self.now.saturating_sub(self.radio_at) < 2000)
            || self.radio_refusals >= RADIO_REFUSALS
        {
            return;
        }
        let online = !self.status.airplane;
        if (!online || self.status.sim == modem_ipc::sim::PRESENT)
            && (self.status.radio_online != online || !self.online_sent)
            && self.request_must(
                p,
                DMS,
                DMS_SET_OPERATING_MODE,
                &[(1, &[if online { 0 } else { 1 }])],
            )
        {
            self.online_sent = true;
            self.radio_at = self.now;
            let txn = self
                .clients
                .iter()
                .find(|c| c.service == DMS)
                .map_or(0, |c| c.txn);
            self.radio_request = Some((txn, online));
            self.status.pending = true;
            self.status.result = 0;
        }
    }

    /// The user's airplane intent: always recorded (the caller persists it); the radio follows as soon as it can
    /// (now, or from `tick` once there is a SIM and a DMS client). Only a user command in flight refuses it.
    pub fn airplane(&mut self, p: &mut dyn Bus, off: bool) -> Result<(), i32> {
        if self.command.is_some() {
            return Err(KERR_IN_USE);
        }
        if self.status.airplane != off {
            self.radio_refusals = 0; // a new intent may be asked for again
        }
        self.status.airplane = off;
        if off {
            self.post_call = None;
        }
        // Queue the latest intent behind an in-flight mode request; its confirmation is not the new intent.
        if self.radio_request.is_none() {
            self.radio_at = 0;
            self.online_sent = false;
        }
        self.maybe_online(p);
        Ok(())
    }

    fn user_request(
        &mut self,
        p: &mut dyn Bus,
        service: u32,
        id: u16,
        tlvs: &[(u8, &[u8])],
    ) -> Result<(), i32> {
        if self.command.is_some() || self.radio_request.is_some() {
            return Err(KERR_IN_USE);
        }
        if !self.request(p, service, id, tlvs) {
            return Err(KERR_NOT_READY);
        }
        let txn = self
            .clients
            .iter()
            .find(|c| c.service == service)
            .map_or(0, |c| c.txn);
        self.command = Some((service, id, txn, self.now));
        self.status.pending = true;
        self.status.result = 0;
        Ok(())
    }

    pub fn verify_pin(&mut self, p: &mut dyn Bus, pin: &[u8]) -> Result<(), i32> {
        if self.status.sim != modem_ipc::sim::PIN_REQUIRED {
            return Err(KERR_NOT_READY);
        }
        if !(4..=8).contains(&pin.len()) || !pin.iter().all(u8::is_ascii_digit) {
            return Err(KERR_ARGUMENT);
        }
        let mut info = alloc::vec![self.pin_id, pin.len() as u8];
        info.extend_from_slice(pin);
        // Card slot 1 session, as qmicli-uim verify-pin; usable before GW provisioning.
        let result = self.user_request(p, UIM, uim::VERIFY_PIN, &[(1, &[6, 0]), (2, &info)]);
        info.fill(0);
        result
    }

    fn target(&self, id: u8) -> Option<Call> {
        if id == 0 {
            Some(self.status.call)
        } else {
            self.status
                .calls
                .find(id)
                .copied()
                .or_else(|| (self.status.call.id == id).then_some(self.status.call))
        }
    }

    pub fn answer_call(&mut self, p: &mut dyn Bus, id: u8) -> Result<(), i32> {
        let c = self.target(id).ok_or(KERR_NOT_READY)?;
        if !c.incoming || !matches!(c.state, call::INCOMING | call::WAITING) {
            return Err(KERR_NOT_READY);
        }
        if c.state == call::WAITING {
            self.user_request(p, VOICE, voice::MANAGE_CALLS, &[(1, &[3])])
        } else {
            self.user_request(p, VOICE, voice::ANSWER, &[(1, &[c.id])])
        }
    }

    pub fn end_call(&mut self, p: &mut dyn Bus, id: u8) -> Result<(), i32> {
        let c = self.target(id).ok_or(KERR_NOT_READY)?;
        if matches!(c.state, call::NONE | call::END) {
            return Err(KERR_NOT_READY);
        }
        if c.incoming && matches!(c.state, call::INCOMING | call::WAITING) {
            // Reject = User Determined User Busy (3GPP 22.030 CHLD=0, oFono's set_udub): the caller hears busy /
            // declined. A plain END released it like a lost call and the caller's network said "not reachable"
            // (2026-11-14). QMI VOICE Manage Calls type 1 = release held or UDUB the waiting/incoming call; END is the
            // fallback when the modem refuses it.
            self.log.rejected(c.id);
            self.user_request(p, VOICE, voice::MANAGE_CALLS, &[(1, &[1])])?;
            self.rejecting = Some(c.id);
            return Ok(());
        }
        // DIAL can publish its indication before its response. Cancellation bypasses the
        // user-command slot only for that selected outgoing call, never an existing held call.
        let dialing = self
            .command
            .is_some_and(|(s, msg, _, _)| s == VOICE && msg == voice::DIAL)
            && !c.incoming
            && c.id == self.status.call.id
            && matches!(
                c.state,
                call::ORIGINATION
                    | call::CC_IN_PROGRESS
                    | call::ALERTING
                    | call::CONVERSATION
                    | call::DISCONNECTING
            );
        if c.id == 0 || dialing {
            return self.hangup(p);
        }
        self.user_request(p, VOICE, voice::END, &[(1, &[c.id])])
    }

    /// Reject each new incoming or waiting call the blocklist matches as busy (UDUB, END if refused), before it
    /// rings. A call whose number hasn't arrived yet (shown, but not in this indication) is decided on a later one.
    fn block_calls(&mut self, p: &mut dyn Bus, calls: &modem_ipc::Calls) {
        use phone_rules::caller::presentation;
        for c in calls.as_slice() {
            let ringing = c.incoming && matches!(c.state, call::INCOMING | call::WAITING);
            let known = !c.number.as_str().is_empty()
                || matches!(
                    c.presentation,
                    presentation::RESTRICTED | presentation::UNAVAILABLE
                );
            if !ringing || !known || self.log.is_blocked(c.id) || self.answered.contains(&c.id) {
                continue;
            }
            let Some(why) = self.block.check_call(c.number.as_str(), c.presentation) else {
                continue;
            };
            p.log(format_args!("voice: call {} blocked (rule {why})", c.id));
            self.log.blocked(c.id, why);
            if self.request(p, VOICE, voice::MANAGE_CALLS, &[(1, &[1])]) {
                self.rejecting = Some(c.id);
            } else {
                self.request(p, VOICE, voice::END, &[(1, &[c.id])]);
            }
        }
    }

    /// The SMSC, and what the modem kept while nobody listened (read it, save it, then delete it there). Before the
    /// SIM's subscription is ready they fail (52 not provisioned, 47; phone 2026-11-16): `tick` asks again.
    fn sms_start_reads(&mut self, p: &mut dyn Bus) {
        self.sms_retry_at = self.now.saturating_add(10_000);
        self.request(p, WMS, wms::GET_SMSC, &[]);
        for storage in [wms::STORAGE_UIM, wms::STORAGE_NV] {
            for tag in [wms::TAG_MT_NOT_READ, wms::TAG_MT_READ] {
                if let Some(txn) = self.wms_request(
                    p,
                    wms::LIST_MESSAGES,
                    &[(0x01, &[storage]), (0x11, &[tag]), (0x12, &wms::MODE)],
                ) {
                    self.sms_lists.push((txn, storage));
                }
            }
        }
    }

    /// A WMS request; its transaction once sent.
    fn wms_request(&mut self, p: &mut dyn Bus, id: u16, tlvs: &[(u8, &[u8])]) -> Option<u16> {
        self.request(p, WMS, id, tlvs).then(|| {
            self.clients
                .iter()
                .find(|c| c.service == WMS)
                .map_or(0, |c| c.txn)
        })
    }

    fn read_stored(&mut self, p: &mut dyn Bus, storage: u8, index: u32) {
        if let Some(txn) = self.wms_request(
            p,
            wms::RAW_READ,
            &[(0x01, &wms::location(storage, index)), (0x10, &wms::MODE)],
        ) {
            self.sms_reads.push((txn, storage, index));
        }
    }

    /// An incoming TPDU: a message (blocked or not) or a status report.
    fn sms_incoming(&mut self, p: &mut dyn Bus, tpdu: &[u8]) {
        let utc = p.utc_ms();
        match sms::parse(tpdu) {
            Some(sms::Tpdu::Deliver(d)) => {
                let why = self.block.check_message(&d.from);
                let part = d.concat.map(|c| (c.seq, c.total));
                match self.sms.receive(d, why, utc) {
                    Some(id) => p.log(format_args!(
                        "wms: message {id} received{}",
                        if why.is_some() { " (blocked)" } else { "" }
                    )),
                    None => p.log(format_args!("wms: part {:?} received", part)),
                }
            }
            Some(sms::Tpdu::Status(r)) => {
                p.log(format_args!(
                    "wms: status report {:#x} for reference {}",
                    r.status, r.mr
                ));
                self.sms.report(&r, utc);
            }
            None => p.log(format_args!(
                "wms: a TPDU of {} bytes didn't parse",
                tpdu.len()
            )),
        }
        self.sms_status();
    }

    /// Status.sms_* from the store.
    fn sms_status(&mut self) {
        self.status.sms_revision = self.sms.rev;
        (self.status.sms_unread, self.status.sms_senders) = self.sms.unread();
    }

    /// The next part to the modem, if none is with it.
    fn send_next_sms(&mut self, p: &mut dyn Bus) {
        if self.sms_in_flight.is_some() {
            return;
        }
        let Some((id, pdus, next)) = self.sms_out.first() else {
            return;
        };
        let (id, last) = (*id, *next + 1 == pdus.len());
        let raw = wms::raw_send(&pdus[*next]);
        if let Some(txn) = self.wms_request(p, wms::RAW_SEND, &[(0x01, &raw)]) {
            self.sms_in_flight = Some((id, txn, last, self.now));
        }
    }

    fn sms_sent(&mut self, p: &mut dyn Bus, m: &qmi::Msg) {
        let Some((id, txn, last, _)) = self.sms_in_flight else {
            return;
        };
        if txn != m.txn {
            return;
        }
        self.sms_in_flight = None;
        self.sms.sent(id, wms::sent_reference(m).unwrap_or(0), last);
        if let Some(front) = self.sms_out.first_mut() {
            front.2 += 1;
            if front.2 >= front.1.len() {
                self.sms_out.remove(0);
            }
        }
        if last {
            p.log(format_args!("wms: message {id} sent"));
        }
        self.sms_status();
        self.send_next_sms(p);
    }

    fn sms_send_failed(&mut self, p: &mut dyn Bus, id: u32, why: u8) {
        p.log(format_args!("wms: message {id} not sent ({why})"));
        self.sms_in_flight = None;
        self.sms_out.retain(|o| o.0 != id);
        self.sms.failed(id, why);
        self.sms_status();
        self.send_next_sms(p);
    }

    /// modem_ipc::op::SMS_SEND: the new message's id. It waits in the queue while the modem isn't up.
    pub fn sms_send(&mut self, p: &mut dyn Bus, to: &str, text: &str) -> Result<u32, i32> {
        if text.len() > modem_ipc::sms::MAX_TEXT {
            return Err(KERR_TOO_BIG);
        }
        let utc = p.utc_ms();
        let (id, pdus) = self.sms.send(to, text, utc).map_err(|e| match e {
            sms::Error::BadLength => KERR_TOO_BIG,
            sms::Error::BadNumber => KERR_ARGUMENT,
        })?;
        self.sms_out.push((id, pdus, 0));
        self.sms_status();
        self.send_next_sms(p);
        Ok(id)
    }

    /// modem_ipc::op::SMS_EDIT.
    pub fn sms_edit(&mut self, p: &mut dyn Bus, action: i32, id: u32) -> Result<(), i32> {
        if action == modem_ipc::sms::EDIT_RETRY {
            let pdus = self.sms.retry(id).ok_or(KERR_NOT_READY)?;
            self.sms_out.push((id, pdus, 0));
            self.send_next_sms(p);
        } else if !self.sms.edit(action, id) && action != modem_ipc::sms::EDIT_DELETE_ALL {
            // (deleting everything from an empty store is done, not refused: Settings' Clear data twice)
            return Err(KERR_NOT_READY);
        }
        self.sms_status();
        Ok(())
    }

    /// modem_ipc::op::SMS_SETTINGS set: a new message centre goes to the modem.
    pub fn sms_settings(&mut self, p: &mut dyn Bus, s: modem_ipc::SmsSettings) {
        let smsc = s.smsc.as_str();
        if !smsc.is_empty() && smsc != self.sms.settings.smsc.as_str() {
            let (digits, toa) = wms::set_smsc(smsc);
            let mut addr = alloc::vec![digits.len() as u8];
            addr.extend_from_slice(&digits);
            let mut ty = alloc::vec![toa.len() as u8];
            ty.extend_from_slice(toa);
            self.request(p, WMS, wms::SET_SMSC, &[(0x01, &addr), (0x10, &ty)]);
        }
        self.sms.settings.reports = s.reports;
        self.sms.settings.alert = s.alert;
        self.sms.dirty = true;
    }

    /// MODEMD saved the messages (or had nothing to save): now acknowledge them to the network and delete what the
    /// modem had stored.
    pub fn sms_saved(&mut self, p: &mut dyn Bus) {
        for a in core::mem::take(&mut self.sms_after_save) {
            match a {
                AfterSave::Ack(txn) => {
                    self.request_must(p, WMS, wms::SEND_ACK, &[(0x01, &wms::ack(txn))]);
                }
                AfterSave::Delete(storage, index) => {
                    self.request(
                        p,
                        WMS,
                        wms::DELETE,
                        &[
                            (0x01, &[storage]),
                            (0x10, &index.to_le_bytes()),
                            (0x12, &wms::MODE),
                        ],
                    );
                }
            }
        }
    }

    pub fn manage(&mut self, p: &mut dyn Bus, action: i32, id: u8) -> Result<(), i32> {
        use modem_ipc::manage;
        let c = self.target(id).ok_or(KERR_NOT_READY)?;
        let held = self
            .status
            .calls
            .as_slice()
            .iter()
            .any(|c| c.state == call::HOLD);
        let active = self
            .status
            .calls
            .as_slice()
            .iter()
            .any(|c| c.state == call::CONVERSATION);
        let waiting = self
            .status
            .calls
            .as_slice()
            .iter()
            .any(|c| c.state == call::WAITING);
        // QmiVoiceSupplementaryServiceType: hold/accept=3, select=4, conference=5.
        match action {
            manage::HOLD if c.state == call::CONVERSATION => {
                self.user_request(p, VOICE, voice::MANAGE_CALLS, &[(1, &[3])])
            }
            manage::RESUME if c.state == call::HOLD => {
                self.user_request(p, VOICE, voice::MANAGE_CALLS, &[(1, &[4]), (0x10, &[c.id])])
            }
            manage::SWAP if active && (held || waiting) => {
                self.user_request(p, VOICE, voice::MANAGE_CALLS, &[(1, &[3])])
            }
            manage::MERGE if active && held => {
                self.user_request(p, VOICE, voice::MANAGE_CALLS, &[(1, &[5])])
            }
            // type 2 = release the active calls and accept the held or waiting one (3GPP 22.030 CHLD=1)
            manage::END_ANSWER if active && waiting => {
                self.user_request(p, VOICE, voice::MANAGE_CALLS, &[(1, &[2])])
            }
            manage::HOLD | manage::RESUME | manage::SWAP | manage::MERGE | manage::END_ANSWER => {
                Err(KERR_NOT_READY)
            }
            _ => Err(KERR_ARGUMENT),
        }
    }

    /// Call waiting on the network (modem_ipc::op::CALL_WAITING): query it, or turn it on or off. The answer arrives
    /// in `status.waiting`.
    pub fn call_waiting(&mut self, p: &mut dyn Bus, action: i32) -> Result<(), i32> {
        use modem_ipc::waiting;
        let r = match action {
            waiting::QUERY => self.user_request(
                p,
                VOICE,
                voice::GET_CALL_WAITING,
                &[(0x10, &[voice::CLASS_VOICE])],
            ),
            waiting::ACTIVATE | waiting::DEACTIVATE => self.user_request(
                p,
                VOICE,
                voice::SET_SUPS_SERVICE,
                &[(1, &[action as u8, voice::REASON_CALL_WAITING])],
            ),
            _ => return Err(KERR_ARGUMENT),
        };
        if r.is_ok() {
            self.waiting_request = action;
            self.status.waiting = waiting::BUSY;
        }
        r
    }

    fn card_status(&mut self, p: &mut dyn Bus, m: &qmi::Msg) {
        if let Some(c) = uim::card(m) {
            self.status.sim = c.state;
            self.status.pin_attempts = c.attempts;
            self.pin_id = c.pin_id;
            self.card = Some(match c.state {
                modem_ipc::sim::ABSENT => 0,
                modem_ipc::sim::ERROR => 2,
                _ => 1,
            });
            self.maybe_online(p);
        }
    }

    /// The modem (re)started: the watchdog counts from the next tick, not from the boot wait before it.
    pub fn rearm_watchdog(&mut self) {
        self.heartbeat_at = 0;
    }

    /// Three missed 30-second NAS poll cycles: the transport may be alive while the modem is wedged.
    pub fn watchdog(&self) -> bool {
        !self.status.airplane
            && self.clients.iter().any(|c| c.service == NAS)
            && self.heartbeat_at != 0
            && self.now.saturating_sub(self.heartbeat_at) > 90_000
    }

    /// The call can't stick (C6 2026-11-09: a hangup that never reached the modem left it "dialling" for good, and
    /// every redial got -14): an END not yet sent goes again each second; a call stuck in a passing state ends here
    /// (disconnecting 5 s, dialling 30 s without a word from the modem).
    fn call_watch(&mut self, p: &mut dyn Bus, now_ms: u64) {
        let c = self.status.call;
        if (c.id, c.state) != self.call_seen {
            self.call_seen = (c.id, c.state);
            self.call_since = now_ms;
        }
        if self.hangup_pending && c.id != 0 && now_ms.saturating_sub(self.hangup_at) >= 1000 {
            self.hangup_at = now_ms;
            let _ = self.hangup(p);
        }
        let limit = match c.state {
            call::DISCONNECTING => 5_000,
            call::ORIGINATION | call::CC_IN_PROGRESS | call::SETUP => 30_000,
            _ => return,
        };
        if now_ms.saturating_sub(self.call_since) > limit {
            p.log(format_args!(
                "voice: call {} stuck in state {}: ended here",
                c.id, c.state
            ));
            self.status.call.state = call::END;
            self.hangup_pending = false;
        }
    }

    /// Place a call (modem_ipc::op::DIAL). KErrArgument for a number [`voice::dialable`] refuses, KErrInUse while a
    /// call is up, KErrNotReady before VOICE is there.
    pub fn dial(&mut self, p: &mut dyn Bus, number: &str) -> Result<(), i32> {
        // Caller ID (C8): a typed *31#/#31# wins, else the setting's prefix (Symbian^3 sends CLIR the same way).
        let shown = number
            .strip_prefix("*31#")
            .or_else(|| number.strip_prefix("#31#"))
            .unwrap_or(number);
        let prefix = match self.clir {
            _ if shown.len() != number.len() => "",
            modem_ipc::clir::HIDE => "#31#",
            modem_ipc::clir::SHOW => "*31#",
            _ => "",
        };
        let wire = alloc::format!("{prefix}{number}");
        let number = shown;
        if !voice::dialable(number) {
            return Err(KERR_ARGUMENT);
        }
        if self.status.airplane {
            return Err(KERR_NOT_READY);
        }
        if self
            .status
            .calls
            .as_slice()
            .iter()
            .any(|c| !matches!(c.state, call::NONE | call::END | call::HOLD))
            || (!matches!(self.status.call.state, call::NONE | call::END | call::HOLD))
        {
            return Err(KERR_IN_USE);
        }
        self.user_request(p, VOICE, voice::DIAL, &[(1, wire.as_bytes())])?;
        self.hangup_pending = false;
        self.status.call = Call {
            id: 0,
            state: call::ORIGINATION,
            incoming: false,
            number: Text::new(number),
            presentation: 0,
            ..Call::default()
        };
        p.log(format_args!("voice: dial {}{}", prefix, mask(number)));
        Ok(())
    }

    /// Answer the incoming call (modem_ipc::op::ANSWER); the modem refuses it before "incoming" (C1: during setup).
    pub fn answer(&mut self, p: &mut dyn Bus) -> Result<(), i32> {
        let c = self.status.call;
        if !(c.incoming && matches!(c.state, call::INCOMING | call::WAITING)) {
            return Err(KERR_NOT_READY);
        }
        self.request(p, VOICE, voice::ANSWER, &[(0x01, &[c.id])])
            .then_some(())
            .ok_or(KERR_NOT_READY)
    }

    /// End, reject or cancel the call (modem_ipc::op::HANGUP). It always ends: the call shows "disconnecting" at
    /// once; an END that can't go out yet (no call id, or the modem takes nothing) is retried by `tick`, whose
    /// watchdog ends the call if the modem never confirms.
    pub fn hangup(&mut self, p: &mut dyn Bus) -> Result<(), i32> {
        let c = self.status.call;
        if matches!(c.state, call::NONE | call::END) {
            return Err(KERR_NOT_READY);
        }
        self.status.call.state = call::DISCONNECTING;
        self.hangup_pending = c.id == 0 || !self.request(p, VOICE, voice::END, &[(0x01, &[c.id])]);
        Ok(())
    }

    /// A keypad tone during a connected call (modem_ipc::op::DTMF): `Some(key)` starts it (`0-9`, `*`, `#`), `None`
    /// stops it.
    pub fn dtmf(&mut self, p: &mut dyn Bus, key: Option<u8>) -> Result<(), i32> {
        let c = self.status.call;
        if c.state != call::CONVERSATION || c.id == 0 {
            return Err(KERR_NOT_READY);
        }
        let sent = match key {
            Some(k) if k.is_ascii_digit() || k == b'*' || k == b'#' => {
                self.request(p, VOICE, voice::START_CONT_DTMF, &[(0x01, &[c.id, k])])
            }
            Some(_) => return Err(KERR_ARGUMENT),
            None => self.request_must(p, VOICE, voice::STOP_CONT_DTMF, &[(0x01, &[c.id])]),
        };
        sent.then_some(()).ok_or(KERR_NOT_READY)
    }

    fn client(&mut self, p: &mut dyn Bus, port: u32, payload: &[u8]) {
        let Some(m) = qmi::Msg::parse(payload) else {
            return;
        };
        let Some(c) = self.clients.iter_mut().find(|c| c.port == port) else {
            return;
        };
        let service = c.service;
        match m.ty {
            qmi::RESPONSE => {
                let Some(k) = c.pending.iter().position(|&r| r == (m.txn, m.id)) else {
                    p.log(format_args!(
                        "qmi: service {service} response {:#x} txn {} not ours",
                        m.id, m.txn
                    ));
                    return;
                };
                c.pending.remove(k);
                self.response_seen = true;
                self.response(p, service, &m);
            }
            qmi::INDICATION => self.indication(p, service, &m),
            _ => {}
        }
    }

    fn response(&mut self, p: &mut dyn Bus, service: u32, m: &qmi::Msg) {
        if self
            .command
            .is_some_and(|(s, id, txn, _)| s == service && id == m.id && txn == m.txn)
        {
            self.command = None;
            self.status.pending = false;
            self.status.result = match m.result() {
                Some(Ok(())) => 0,
                Some(Err(e)) => -1000 - i32::from(e),
                None => -20,
            };
        }
        if (service, m.id) == (UIM, uim::VERIFY_PIN) {
            if let Some(n) = m.u8(0x10) {
                self.status.pin_attempts = n;
                if n == 0 {
                    self.status.sim = modem_ipc::sim::BLOCKED;
                }
            }
            self.request(p, UIM, UIM_GET_CARD_STATUS, &[]);
        }
        if (service, m.id) == (DMS, DMS_SET_OPERATING_MODE)
            && self.radio_request.is_some_and(|(txn, _)| txn == m.txn)
        {
            let (_, online) = self.radio_request.take().unwrap();
            self.status.pending = self.command.is_some();
            if m.result() == Some(Ok(())) {
                self.radio_refusals = 0;
                self.status.radio_online = online;
                p.log(format_args!(
                    "dms: confirmed online {} desired airplane {}",
                    online, self.status.airplane
                ));
                self.status.modem = if online {
                    modem_ipc::modem::ONLINE
                } else {
                    modem_ipc::modem::RUNNING
                };
                if !online {
                    self.status.reg = modem_ipc::reg::NONE;
                    self.status.rat = modem_ipc::rat::NONE;
                    self.status.bars = 0;
                }
            } else {
                self.radio_refusals = self.radio_refusals.saturating_add(1);
                // QMI 26 no effect / 52 invalid transition: the modem is already there or getting there (it
                // refuses "online" ~20 s into every boot while it brings itself online). Not a failed action for
                // the user: the result would stay on Settings -> Sounds as "Action failed (-2)".
                if !matches!(m.result(), Some(Err(26 | 52))) {
                    self.status.result = -2;
                }
            }
        }
        let text = |t: Option<&[u8]>| t.map(|b| String::from_utf8_lossy(b).into_owned());
        let Some(Ok(())) = m.result() else {
            p.log(format_args!(
                "qmi: service {service} request {:#x} failed {:?}",
                m.id,
                m.result()
            ));
            if (service, m.id) == (VOICE, voice::DIAL) {
                self.status.call.state = call::END;
            }
            if service == VOICE && matches!(m.id, voice::GET_CALL_WAITING | voice::SET_SUPS_SERVICE)
            {
                self.status.waiting = modem_ipc::waiting::ERROR;
            }
            if (service, m.id) == (VOICE, voice::MANAGE_CALLS)
                && let Some(id) = self.rejecting.take()
            {
                p.log(format_args!("voice: busy reject refused; ending call {id}"));
                self.request(p, VOICE, voice::END, &[(1, &[id])]);
            }
            if (service, m.id) == (WMS, wms::RAW_SEND)
                && let Some((id, txn, _, _)) = self.sms_in_flight
                && txn == m.txn
            {
                self.sms_send_failed(p, id, modem_ipc::sms::FAIL_REFUSED);
            }
            if service == WMS {
                self.sms_reads.retain(|r| r.0 != m.txn);
                self.sms_lists.retain(|r| r.0 != m.txn);
            }
            return;
        };
        match (service, m.id) {
            (DMS, DMS_GET_REVISION) => {
                self.revision = text(m.tlv(0x01));
                p.log(format_args!(
                    "dms: revision {}",
                    self.revision.as_deref().unwrap_or("?")
                ));
            }
            (DMS, DMS_GET_IDS) => {
                self.imei = text(m.tlv(0x11));
                self.status.imei = Text::new(self.imei.as_deref().unwrap_or(""));
                p.log(format_args!(
                    "dms: IMEI {}",
                    self.imei.as_deref().map(mask).unwrap_or_default()
                ));
            }
            (DMS, DMS_SET_OPERATING_MODE) => self.maybe_online(p),
            (UIM, UIM_GET_CARD_STATUS) => self.card_status(p, m),
            (NAS, nas::GET_SERVING_SYSTEM) => self.serving(p, m),
            (NAS, nas::GET_SIGNAL_INFO) => self.signal(m),
            (NAS, nas::SET_SYSTEM_SELECTION) => p.log(format_args!("nas: voice-only mode set")),
            (NAS, nas::GET_SYSTEM_SELECTION) => p.log(format_args!(
                "nas: mode preference {:#x}",
                nas::mode_preference(m).unwrap_or(0)
            )),
            (NAS, nas::GET_NETWORK_TIME) => self.network_time(p, m),
            (NAS, nas::GET_PLMN_NAME) => {
                if let Some(n) = nas::plmn_name(m) {
                    let (mcc, mnc) = self.plmn_asked.unwrap_or_default();
                    p.log(format_args!("nas: operator {n} (PLMN {mcc}-{mnc:02})"));
                    self.status.operator = Text::new(&n);
                }
            }
            (VOICE, voice::MANAGE_CALLS) => self.rejecting = None,
            (WMS, wms::RAW_SEND) => self.sms_sent(p, m),
            (WMS, wms::LIST_MESSAGES) => {
                let Some(k) = self.sms_lists.iter().position(|r| r.0 == m.txn) else {
                    return;
                };
                let (_, storage) = self.sms_lists.remove(k);
                for index in wms::listed(m) {
                    self.read_stored(p, storage, index);
                }
            }
            (WMS, wms::RAW_READ) => {
                let Some(k) = self.sms_reads.iter().position(|r| r.0 == m.txn) else {
                    return;
                };
                let (_, storage, index) = self.sms_reads.remove(k);
                if let Some(raw) = wms::read_raw(m) {
                    self.sms_incoming(p, &raw);
                }
                self.sms_after_save.push(AfterSave::Delete(storage, index));
            }
            (WMS, wms::GET_SMSC) => {
                self.sms_ready = true;
                if let Some(n) = wms::smsc(m) {
                    self.sms.settings.smsc = Text::new(&n);
                }
            }
            (WMS, wms::SET_SMSC) => {
                self.request(p, WMS, wms::GET_SMSC, &[]);
            }
            (VOICE, voice::GET_CALL_WAITING) => {
                let on = m.u8(0x10).is_some_and(|c| c & voice::CLASS_VOICE != 0);
                self.status.waiting = if on {
                    modem_ipc::waiting::ON
                } else {
                    modem_ipc::waiting::OFF
                };
            }
            (VOICE, voice::SET_SUPS_SERVICE) => {
                self.status.waiting = if self.waiting_request == modem_ipc::waiting::ACTIVATE {
                    modem_ipc::waiting::ON
                } else {
                    modem_ipc::waiting::OFF
                };
            }
            (VOICE, voice::DIAL) => {
                self.status.call.id = m.u8(0x10).unwrap_or(0);
                if self.hangup_pending {
                    self.hangup_pending = false;
                    let _ = self.hangup(p);
                }
            }
            _ => {} // the indication registrations, answer, end: their indications tell the rest
        }
    }

    fn indication(&mut self, p: &mut dyn Bus, service: u32, m: &qmi::Msg) {
        match (service, m.id) {
            (NAS, nas::GET_SERVING_SYSTEM) => self.serving(p, m),
            (NAS, nas::SIGNAL_INFO_IND) => self.signal(m),
            (NAS, nas::NETWORK_TIME_IND) => self.network_time(p, m),
            (UIM, uim::STATUS_IND) => self.card_status(p, m),
            (WMS, wms::EVENT_REPORT_IND) => match wms::event(m) {
                Some(wms::Event::Transfer { ack, txn, tpdu }) => {
                    if !tpdu.is_empty() {
                        self.sms_incoming(p, &tpdu);
                    }
                    if ack {
                        self.sms_after_save.push(AfterSave::Ack(txn));
                    }
                }
                Some(wms::Event::Stored { storage, index }) => self.read_stored(p, storage, index),
                None => {}
            },
            (WMS, wms::SMSC_IND) => {
                if let Some(n) = wms::smsc(m) {
                    self.sms.settings.smsc = Text::new(&n);
                }
            }
            (VOICE, voice::ALL_CALL_STATUS_IND) => {
                let Some(calls) = voice::all_calls(m) else {
                    return;
                };
                let old = self.status.calls;
                let mut next = modem_ipc::Calls {
                    len: calls.len() as u8,
                    ..Default::default()
                };
                for (i, c) in calls.into_iter().enumerate() {
                    let previous = old.find(c.id).copied().or_else(|| {
                        (self.status.call.id == c.id || self.status.call.id == 0)
                            .then_some(self.status.call)
                    });
                    // an indication without the numbers TLV keeps the number and how it was presented (a
                    // private caller stays private)
                    let (number, presentation) = match &c.number {
                        Some(n) => (Text::new(n), c.presentation),
                        None => previous.map_or((Text::default(), c.presentation), |x| {
                            (x.number, x.presentation)
                        }),
                    };
                    let up = matches!(c.state, call::CONVERSATION | call::HOLD);
                    next.entries[i] = Call {
                        id: c.id,
                        state: c.state,
                        incoming: c.incoming,
                        number,
                        presentation,
                        multiparty: c.multiparty,
                        since: match previous.map_or(0, |x| x.since) {
                            0 if up => self.now.max(1),
                            s => s,
                        },
                    };
                    if previous.is_none_or(|old| (old.id, old.state) != (c.id, c.state)) {
                        p.log(format_args!(
                            "voice: call {} state {} incoming {}",
                            c.id, c.state, c.incoming
                        ));
                    }
                    if c.state == call::CONVERSATION && !self.answered.contains(&c.id) {
                        self.answered.push(c.id);
                    }
                }
                for c in old.as_slice() {
                    let ended = next.find(c.id).is_none_or(|n| n.state == call::END);
                    if ended
                        && !matches!(c.state, call::NONE | call::END)
                        && next.find(c.id).is_none()
                    {
                        p.log(format_args!(
                            "voice: call {} absent from new list (ended)",
                            c.id
                        ));
                    }
                    if ended
                        && c.incoming
                        && !matches!(c.state, call::NONE | call::END)
                        && !self.answered.contains(&c.id)
                        && !self.log.is_rejected(c.id)
                    {
                        self.status.missed = self.status.missed.wrapping_add(1);
                    }
                }
                self.answered
                    .retain(|id| next.find(*id).is_some_and(|c| c.state != call::END));
                let active = |c: &Call| !matches!(c.state, call::NONE | call::END);
                let old_active = old.as_slice().iter().any(active)
                    || (old.len == 0 && active(&self.status.call));
                if next.as_slice().iter().any(active) {
                    self.post_call = None;
                } else if old_active && !self.status.airplane {
                    self.post_call = Some((self.now, self.now));
                    p.log(format_args!(
                        "nas: final call END at {}ms; observing LTE return",
                        self.now
                    ));
                }
                let utc = p.utc_ms();
                if self.log.observe(next.as_slice(), self.now, utc) {
                    self.status.log_revision = self.status.log_revision.wrapping_add(1);
                }
                self.block_calls(p, &next);
                // Blocked calls are never shown or rung: clients and the alerts see the list without them.
                let mut kept = 0;
                for i in 0..usize::from(next.len) {
                    if !self.log.is_blocked(next.entries[i].id) {
                        next.entries[kept] = next.entries[i];
                        kept += 1;
                    }
                }
                next.len = kept as u8;
                self.status.calls = next;
                // Incoming/waiting gets UI priority; audio independently scans the complete list.
                self.status.call = next
                    .as_slice()
                    .iter()
                    .min_by_key(|c| match c.state {
                        call::INCOMING | call::WAITING => 0,
                        call::CONVERSATION => 1,
                        call::ORIGINATION | call::ALERTING | call::CC_IN_PROGRESS => 2,
                        call::HOLD => 3,
                        call::DISCONNECTING => 4,
                        _ => 5,
                    })
                    .copied()
                    .unwrap_or(Call {
                        state: if self.status.call.state != call::NONE {
                            call::END
                        } else {
                            call::NONE
                        },
                        ..self.status.call
                    });
                if self.hangup_pending && self.status.call.id != 0 {
                    self.hangup_pending = false;
                    let _ = self.hangup(p);
                }
            }
            _ => {}
        }
    }

    fn serving(&mut self, p: &mut dyn Bus, m: &qmi::Msg) {
        let Some(s) = nas::serving(m) else {
            return;
        };
        if (s.reg, s.rat, s.cs) != (self.status.reg, self.status.rat, self.cs) {
            p.log(format_args!(
                "nas: registration {} rat {} cs {} ps {}",
                s.reg, s.rat, s.cs, s.ps
            ));
        }
        self.cs = s.cs;
        self.status.reg = s.reg;
        self.status.rat = s.rat;
        let registered = matches!(s.reg, modem_ipc::reg::HOME | modem_ipc::reg::ROAMING);
        if registered
            && s.rat == modem_ipc::rat::LTE
            && let Some((start, _)) = self.post_call.take()
        {
            p.log(format_args!(
                "nas: post-call LTE confirmed after {}ms",
                self.now.saturating_sub(start)
            ));
        }
        if registered {
            // online, whether or not our Set Operating Mode was needed (C5: "invalid transition" when it already was)
            self.status.modem = modem_ipc::modem::ONLINE;
            self.status.radio_online = true;
            // voice-only once registered: sent as soon as NAS appeared, the modem refused it (QMI error 25) and its
            // SIM service never came up (C5, 2026-11-09)
            if !self.state.ipa_up && !self.selection_sent {
                self.selection_sent = true;
                self.request(p, NAS, nas::SET_SYSTEM_SELECTION, &nas::VOICE_ONLY);
                self.request(p, NAS, nas::GET_SYSTEM_SELECTION, &[]);
            }
            // the network's time (NITZ) once: its indication may have come before we subscribed
            if !self.time_asked {
                self.time_asked = true;
                self.request(p, NAS, nas::GET_NETWORK_TIME, &[]);
            }
        }
        if !registered {
            self.status.bars = 0;
            self.status.operator = Text::default();
            self.plmn_asked = None;
            return;
        }
        if let Some(n) = s.name {
            self.status.operator = Text::new(&n);
        } else if let Some((mcc, mnc)) = s.plmn.filter(|&q| self.plmn_asked != Some(q)) {
            self.plmn_asked = Some((mcc, mnc));
            let mut q = [0u8; 4];
            q[..2].copy_from_slice(&mcc.to_le_bytes());
            q[2..].copy_from_slice(&mnc.to_le_bytes());
            self.request(p, NAS, nas::GET_PLMN_NAME, &[(0x01, &q)]);
        }
    }

    /// The network's time (NITZ) for the phone's automatic time: a new value each time the network sends one.
    fn network_time(&mut self, p: &mut dyn Bus, m: &qmi::Msg) {
        let Some(t) = nas::network_time(m) else {
            p.log(format_args!("nas: no network time in {:#x}", m.id));
            return;
        };
        p.log(format_args!(
            "nas: network time {} UTC, zone {} quarter hours, dst {}",
            t.utc, t.tz, t.dst
        ));
        self.status.nitz = modem_ipc::Nitz {
            utc: t.utc,
            tz: t.tz,
            dst: t.dst,
            seq: self.status.nitz.seq.wrapping_add(1),
        };
    }

    fn signal(&mut self, m: &qmi::Msg) {
        if let Some(b) = nas::bars(m, self.status.rat) {
            let registered = matches!(
                self.status.reg,
                modem_ipc::reg::HOME | modem_ipc::reg::ROAMING
            );
            self.status.bars = if registered { b } else { 0 };
        }
    }

    /// The periodic work: NAS serving system and signal every 30 s. `now_ms` is any monotonic clock.
    pub fn begin_tick(&mut self, p: &mut dyn Bus, now_ms: u64) {
        self.now = now_ms;
        if self.response_seen || self.heartbeat_at == 0 {
            self.heartbeat_at = now_ms;
            self.response_seen = false;
        }
        if self.radio_request.is_some() && now_ms.saturating_sub(self.radio_at) >= 15_000 {
            self.radio_request = None;
            self.status.pending = false;
            self.status.result = -33;
        }
        if self
            .command
            .is_some_and(|(_, _, _, t)| now_ms.saturating_sub(t) >= 15_000)
        {
            self.command = None;
            self.status.pending = false;
            self.status.result = -33;
            if self.status.waiting == modem_ipc::waiting::BUSY {
                self.status.waiting = modem_ipc::waiting::ERROR;
            }
        }
        self.maybe_online(p);
    }
    pub fn tick(&mut self, p: &mut dyn Bus, now_ms: u64) {
        self.call_watch(p, now_ms);
        if let Some((id, _, _, since)) = self.sms_in_flight
            && now_ms.saturating_sub(since) >= SMS_SEND_MS
        {
            p.log(format_args!("wms: message {id} got no answer"));
            self.sms_send_failed(p, id, modem_ipc::sms::FAIL_NO_ANSWER);
        }
        let registered = matches!(
            self.status.reg,
            modem_ipc::reg::HOME | modem_ipc::reg::ROAMING
        );
        if !self.sms_ready
            && registered
            && now_ms >= self.sms_retry_at
            && self.clients.iter().any(|c| c.service == WMS)
        {
            self.sms_start_reads(p);
        }
        let utc = p.utc_ms();
        if self.sms.expire(utc) {
            self.sms_status();
        }
    }
    pub fn poll(&mut self, p: &mut dyn Bus, now_ms: u64) {
        let mut post_queried = false;
        if let Some((start, next)) = self.post_call {
            if self.status.airplane || now_ms.saturating_sub(start) >= POST_CALL_MS {
                p.log(format_args!(
                    "nas: post-call observation ended after {}ms rat {}",
                    now_ms.saturating_sub(start),
                    self.status.rat
                ));
                self.post_call = None;
            } else if now_ms >= next {
                self.post_call = Some((start, now_ms.saturating_add(1000)));
                self.request(p, NAS, nas::GET_SERVING_SYSTEM, &[]);
                post_queried = true;
            }
        }
        if now_ms.saturating_sub(self.last_poll) < POLL_MS {
            return;
        }
        self.last_poll = now_ms;
        if !post_queried && self.post_call.is_none() {
            self.request(p, NAS, nas::GET_SERVING_SYSTEM, &[]);
        }
        self.request(p, NAS, nas::GET_SIGNAL_INFO, &[]);
    }
    fn reset(&mut self, generation: u32) {
        let old = self.status;
        self.log.observe(&[], self.now, 0);
        let (log, clir) = (core::mem::take(&mut self.log), self.clir);
        let (sms, sms_out) = (
            core::mem::take(&mut self.sms),
            core::mem::take(&mut self.sms_out),
        );
        let state = self.state;
        *self = Self::new();
        self.state = state;
        self.sms = sms;
        self.sms_out = sms_out;
        self.log = log;
        self.clir = clir;
        self.status.airplane = old.airplane;
        self.status.ring = old.ring;
        self.status.missed = old.missed;
        self.status.log_revision = old.log_revision.wrapping_add(1);
        self.sms_status();
        self.status.generation = generation;
        self.status.revision = old.revision;
    }
}

#[cfg(test)]
mod tests;
