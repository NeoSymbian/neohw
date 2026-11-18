// SPDX-License-Identifier: Apache-2.0
use super::*;
use ::qrtr::wire::{self as qrtr, Header, PORT_CTRL};
use core::fmt;
use std::format;
const NODE: u32 = 1;

#[derive(Default)]
struct Fake {
    sent: Vec<Vec<u8>>,
    refuse: usize,
    logs: Vec<String>,
    bound: Vec<(u32, u32)>,
}

impl Bus for Fake {
    fn bind(&mut self, service: u32, port: u32) -> Result<(), i32> {
        self.bound.push((service, port));
        Ok(())
    }
    fn send(&mut self, port: u32, dst: (u32, u32), payload: &[u8], essential: bool) -> bool {
        if self.refuse > 0 {
            assert!(
                !essential,
                "essential retries are covered with the real router"
            );
            self.refuse -= 1;
            return false;
        }
        let h = Header {
            ty: qrtr::TYPE_DATA,
            src_node: NODE,
            src_port: port,
            dst_node: dst.0,
            dst_port: dst.1,
            ..Default::default()
        };
        self.sent.push(qrtr::encode(&h, payload));
        true
    }
    fn log(&mut self, args: fmt::Arguments) {
        self.logs.push(format!("{args}"));
    }
    fn utc_ms(&mut self) -> i64 {
        0
    }
}

fn fake() -> Fake {
    Fake::default()
}
fn modem() -> Cellular {
    let mut m = Cellular::new();
    m.state = State {
        radio_attached: true,
        radio_enabled: true,
        ..Default::default()
    };
    m
}
fn advance(m: &mut Cellular, f: &mut Fake, now: u64) {
    m.begin_tick(f, now);
    m.tick(f, now);
    m.poll(f, now);
}
impl Cellular {
    fn handle(&mut self, f: &mut Fake, packet: &[u8]) {
        let (h, payload) = qrtr::decode(packet).unwrap();
        let event = match h.ty {
            qrtr::TYPE_NEW_SERVER => {
                let (_, [service, instance, node, port]) = qrtr::ctrl_words(payload).unwrap();
                Event::ServerUp(Server {
                    service,
                    instance,
                    node,
                    port,
                })
            }
            qrtr::TYPE_DATA => Event::Packet {
                src: (h.src_node, h.src_port),
                dst_port: h.dst_port,
                payload: payload.to_vec(),
            },
            _ => panic!("unexpected cellular fixture"),
        };
        self.event(f, event);
    }
}
const HI_FROM: [u8; 6] = [0x73, 0x94, 0x21, 0x43, 0x65, 0xf7];
fn from_modem(ty: u32, src_port: u32, dst_port: u32, payload: &[u8]) -> Vec<u8> {
    let h = Header {
        ty,
        src_node: 0,
        src_port,
        dst_node: NODE,
        dst_port,
        ..Default::default()
    };
    qrtr::encode(&h, payload)
}

fn ns(service: u32, port: u32) -> Vec<u8> {
    from_modem(
        qrtr::TYPE_NEW_SERVER,
        PORT_CTRL,
        PORT_CTRL,
        &qrtr::ctrl(4, [service, 1, 0, port]),
    )
}

fn req(pkt: &[u8]) -> (u32, u32, u16, u16) {
    let (h, p) = qrtr::decode(pkt).unwrap();
    let m = qmi::Msg::parse(p).unwrap();
    (h.src_port, h.dst_port, m.id, m.txn)
}

fn ok(txn: u16, id: u16) -> qmi::Builder {
    qmi::Builder::new(qmi::RESPONSE, txn, id).result(0)
}

#[test]
fn nas_registration_name_and_bars() {
    let (mut m, mut f) = (modem(), fake());
    m.handle(&mut f, &ns(NAS, 51));
    let ids: Vec<_> = f.sent.iter().map(|s| req(s).2).collect();
    assert_eq!(
        ids,
        [
            nas::REGISTER_INDICATIONS,
            nas::GET_SERVING_SYSTEM,
            nas::GET_SIGNAL_INFO
        ]
    );
    let mut plmn = 283u16.to_le_bytes().to_vec();
    plmn.extend_from_slice(&10u16.to_le_bytes());
    plmn.push(0); // no description: ask for the name
    let ind = qmi::Builder::new(qmi::INDICATION, 1, nas::GET_SERVING_SYSTEM)
        .tlv(1, &[1, 1, 1, 2, 1, 8])
        .tlv(0x12, &plmn)
        .finish();
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 51, 0x22, &ind));
    assert_eq!(
        (m.status.reg, m.status.rat),
        (modem_ipc::reg::HOME, modem_ipc::rat::LTE)
    );
    // registered: voice-only (once), the network time (once), then the name
    let n = f.sent.len();
    let sel = &f.sent[n - 4];
    assert_eq!(req(sel).2, nas::SET_SYSTEM_SELECTION);
    let (_, pay) = qrtr::decode(sel).unwrap();
    let sel = qmi::Msg::parse(pay).unwrap();
    assert_eq!(
        (sel.tlv(0x11), sel.tlv(0x18), sel.tlv(0x17)),
        (Some(&[0x0c, 0][..]), Some(&[0u8; 4][..]), Some(&[0u8][..]))
    );
    assert_eq!(req(&f.sent[n - 3]).2, nas::GET_SYSTEM_SELECTION);
    assert_eq!(req(&f.sent[n - 2]).2, nas::GET_NETWORK_TIME);
    let (_, _, id, txn) = req(f.sent.last().unwrap());
    assert_eq!(id, nas::GET_PLMN_NAME);
    let mut t = alloc::vec![0, 0, 0, 0, 0, 4];
    t.extend_from_slice(b"Ucom");
    t.extend_from_slice(&[0, 0, 0, 7]);
    t.extend_from_slice(b"Ucom AM");
    let name = ok(txn, nas::GET_PLMN_NAME).tlv(0x10, &t).finish();
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 51, 0x22, &name));
    assert_eq!(m.status.operator.as_str(), "Ucom AM");
    let sig = qmi::Builder::new(qmi::INDICATION, 2, nas::SIGNAL_INFO_IND)
        .tlv(0x14, &[0xca, 0xee, 0xa3, 0xff, 0x24, 0x00])
        .finish();
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 51, 0x22, &sig));
    assert_eq!(m.status.bars, 3);
    let n = f.sent.len();
    advance(&mut m, &mut f, 10_000);
    assert_eq!(f.sent.len(), n);
    advance(&mut m, &mut f, 40_000);
    assert_eq!(f.sent.len(), n + 2);
}

fn sent_tlv(f: &Fake, t: u8) -> Vec<u8> {
    let (_, pay) = qrtr::decode(f.sent.last().unwrap()).unwrap();
    qmi::Msg::parse(pay).unwrap().tlv(t).unwrap().to_vec()
}

#[test]
fn caller_id_prefix() {
    let (mut m, mut f) = (modem(), fake());
    m.handle(&mut f, &ns(VOICE, 50));
    m.clir = modem_ipc::clir::HIDE;
    m.dial(&mut f, "+37400000012").unwrap();
    assert_eq!(sent_tlv(&f, 1), b"#31#+37400000012");
    assert_eq!(m.status.call.number.as_str(), "+37400000012");
    let (mut m, mut f) = (modem(), fake());
    m.handle(&mut f, &ns(VOICE, 50));
    m.clir = modem_ipc::clir::HIDE;
    m.dial(&mut f, "*31#+37400000012").unwrap();
    assert_eq!(sent_tlv(&f, 1), b"*31#+37400000012");
    assert_eq!(m.status.call.number.as_str(), "+37400000012");
}

#[test]
fn call_waiting_query_and_set() {
    use modem_ipc::waiting;
    let (mut m, mut f) = (modem(), fake());
    m.handle(&mut f, &ns(VOICE, 50));
    m.call_waiting(&mut f, waiting::QUERY).unwrap();
    assert_eq!(m.status.waiting, waiting::BUSY);
    let (_, _, id, txn) = req(f.sent.last().unwrap());
    assert_eq!(
        (id, sent_tlv(&f, 0x10)),
        (voice::GET_CALL_WAITING, alloc::vec![1])
    );
    let reply = ok(txn, id).u8(0x10, 0x01).finish();
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 50, 0x23, &reply));
    assert_eq!(m.status.waiting, waiting::ON);
    m.call_waiting(&mut f, waiting::DEACTIVATE).unwrap();
    let (_, _, id, txn) = req(f.sent.last().unwrap());
    assert_eq!(
        (id, sent_tlv(&f, 1)),
        (voice::SET_SUPS_SERVICE, alloc::vec![2, 0x0f])
    );
    m.handle(
        &mut f,
        &from_modem(qrtr::TYPE_DATA, 50, 0x23, &ok(txn, id).finish()),
    );
    assert_eq!(m.status.waiting, waiting::OFF);
    m.call_waiting(&mut f, waiting::ACTIVATE).unwrap();
    let (_, _, id, txn) = req(f.sent.last().unwrap());
    let refused = qmi::Builder::new(qmi::RESPONSE, txn, id).result(3).finish();
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 50, 0x23, &refused));
    assert_eq!(m.status.waiting, waiting::ERROR);
    assert_eq!(m.call_waiting(&mut f, 9), Err(KERR_ARGUMENT));
}

#[test]
fn a_hangup_the_modem_never_gets_still_ends_the_call() {
    let (mut m, mut f) = (modem(), fake());
    m.handle(&mut f, &ns(VOICE, 50));
    assert_eq!(m.dial(&mut f, "+37400000012"), Ok(()));
    let (_, _, _, txn) = req(f.sent.last().unwrap());
    let dial_ok = ok(txn, voice::DIAL).u8(0x10, 1).finish();
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 50, 0x23, &dial_ok));
    advance(&mut m, &mut f, 1000);
    f.refuse = usize::MAX; // the modem takes nothing
    assert_eq!(m.hangup(&mut f), Ok(()));
    assert_eq!(m.status.call.state, call::DISCONNECTING);
    advance(&mut m, &mut f, 3000);
    assert_eq!(m.status.call.state, call::DISCONNECTING);
    advance(&mut m, &mut f, 8100); // 5 s after tick first saw it disconnecting
    assert_eq!(m.status.call.state, call::END);
    f.refuse = 0;
    assert_eq!(m.dial(&mut f, "+37400000012"), Ok(()));
}

#[test]
fn a_refused_end_is_sent_again() {
    let (mut m, mut f) = (modem(), fake());
    m.handle(&mut f, &ns(VOICE, 50));
    m.dial(&mut f, "+37400000012").unwrap();
    let (_, _, _, txn) = req(f.sent.last().unwrap());
    let dial_ok = ok(txn, voice::DIAL).u8(0x10, 1).finish();
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 50, 0x23, &dial_ok));
    f.refuse = 1;
    m.hangup(&mut f).unwrap();
    let n = f.sent.len();
    advance(&mut m, &mut f, 1500);
    assert_eq!(f.sent.len(), n + 1);
    assert_eq!(req(f.sent.last().unwrap()).2, voice::END);
    advance(&mut m, &mut f, 1600);
    assert_eq!(f.sent.len(), n + 1); // sent: no more retries
}

#[test]
fn a_dial_that_goes_nowhere_ends() {
    let (mut m, mut f) = (modem(), fake());
    m.handle(&mut f, &ns(VOICE, 50));
    m.dial(&mut f, "+37400000012").unwrap();
    advance(&mut m, &mut f, 100);
    advance(&mut m, &mut f, 20_000);
    assert_eq!(m.status.call.state, call::ORIGINATION);
    advance(&mut m, &mut f, 31_000);
    assert_eq!(m.status.call.state, call::END);
    assert!(f.logs.iter().any(|l| l.contains("ended here")));
}

#[test]
fn keypad_tones_only_in_a_connected_call() {
    let (mut m, mut f) = (modem(), fake());
    m.handle(&mut f, &ns(VOICE, 50));
    assert_eq!(m.dtmf(&mut f, Some(b'5')), Err(KERR_NOT_READY));
    let conversation = qmi::Builder::new(qmi::INDICATION, 1, voice::ALL_CALL_STATUS_IND)
        .tlv(1, &[1, 3, 3, 0, 2, 3, 0, 0])
        .finish();
    m.handle(
        &mut f,
        &from_modem(qrtr::TYPE_DATA, 50, 0x23, &conversation),
    );
    assert_eq!(m.status.call.state, call::CONVERSATION);
    assert_eq!(m.dtmf(&mut f, Some(b'x')), Err(KERR_ARGUMENT));
    assert_eq!(m.dtmf(&mut f, Some(b'#')), Ok(()));
    let (_, pay) = qrtr::decode(f.sent.last().unwrap()).unwrap();
    let q = qmi::Msg::parse(pay).unwrap();
    assert_eq!(
        (q.id, q.tlv(1)),
        (voice::START_CONT_DTMF, Some(&[3u8, b'#'][..]))
    );
    assert_eq!(m.dtmf(&mut f, None), Ok(()));
    let (_, pay) = qrtr::decode(f.sent.last().unwrap()).unwrap();
    let q = qmi::Msg::parse(pay).unwrap();
    assert_eq!((q.id, q.tlv(1)), (voice::STOP_CONT_DTMF, Some(&[3u8][..])));
}

fn call_ind(calls: &[u8]) -> Vec<u8> {
    from_modem(
        qrtr::TYPE_DATA,
        77,
        0x23,
        &qmi::Builder::new(qmi::INDICATION, 1, voice::ALL_CALL_STATUS_IND)
            .tlv(1, calls)
            .finish(),
    )
}

#[test]
fn complete_call_list_survives_reorder_and_end_of_one_call() {
    let (mut m, mut f) = (modem(), fake());
    m.handle(&mut f, &ns(VOICE, 77));
    m.handle(
        &mut f,
        &call_ind(&[2, 1, 3, 0, 1, 3, 0, 0, 2, 7, 0, 2, 3, 0, 0]),
    );
    assert_eq!(m.status.calls.len, 2);
    assert_eq!(m.status.call.id, 2);
    m.handle(
        &mut f,
        &call_ind(&[2, 2, 7, 0, 2, 3, 0, 0, 1, 3, 0, 1, 3, 0, 0]),
    );
    assert_eq!(m.status.calls.find(1).unwrap().state, call::CONVERSATION);
    m.handle(
        &mut f,
        &call_ind(&[2, 2, 9, 0, 2, 3, 0, 0, 1, 3, 0, 1, 3, 0, 0]),
    );
    assert_eq!(m.status.call.id, 1);
    assert_eq!(m.status.missed, 1);
    m.handle(
        &mut f,
        &call_ind(&[2, 2, 9, 0, 2, 3, 0, 0, 1, 3, 0, 1, 3, 0, 0]),
    );
    assert_eq!(m.status.missed, 1);
}

#[test]
fn reject_is_busy_with_end_fallback() {
    let (mut m, mut f) = (modem(), fake());
    m.handle(&mut f, &ns(VOICE, 77));
    m.handle(&mut f, &call_ind(&[1, 1, 2, 0, 2, 3, 0, 0])); // call 1 incoming
    m.end_call(&mut f, 1).unwrap();
    let (_, _, id, txn) = req(f.sent.last().unwrap());
    assert_eq!((id, sent_tlv(&f, 1)), (voice::MANAGE_CALLS, alloc::vec![1]));
    let refused = qmi::Builder::new(qmi::RESPONSE, txn, id).result(3).finish();
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 77, 0x23, &refused));
    assert_eq!(req(f.sent.last().unwrap()).2, voice::END);
    assert_eq!(sent_tlv(&f, 1), alloc::vec![1]);
    m.handle(&mut f, &call_ind(&[1, 1, 9, 0, 2, 3, 0, 0]));
    assert_eq!(m.log.entries[0].kind, modem_ipc::log::REJECTED);
    assert_eq!(m.status.missed, 0);
    // accepted: no END afterwards
    let (mut m, mut f) = (modem(), fake());
    m.handle(&mut f, &ns(VOICE, 77));
    m.handle(&mut f, &call_ind(&[1, 1, 2, 0, 2, 3, 0, 0]));
    m.end_call(&mut f, 1).unwrap();
    let (_, _, id, txn) = req(f.sent.last().unwrap());
    let n = f.sent.len();
    m.handle(
        &mut f,
        &from_modem(qrtr::TYPE_DATA, 77, 0x23, &ok(txn, id).finish()),
    );
    assert_eq!(f.sent.len(), n);
}

fn call_ind_numbered(calls: &[u8], numbers: &[(u8, u8, &str)]) -> Vec<u8> {
    let mut r = alloc::vec![numbers.len() as u8];
    for (id, pres, n) in numbers {
        r.extend_from_slice(&[*id, *pres, n.len() as u8]);
        r.extend_from_slice(n.as_bytes());
    }
    from_modem(
        qrtr::TYPE_DATA,
        77,
        0x23,
        &qmi::Builder::new(qmi::INDICATION, 1, voice::ALL_CALL_STATUS_IND)
            .tlv(1, calls)
            .tlv(0x10, &r)
            .finish(),
    )
}

#[test]
fn private_stays_private_since_and_multiparty() {
    let (mut m, mut f) = (modem(), fake());
    m.handle(&mut f, &ns(VOICE, 77));
    m.now = 4_000;
    m.handle(
        &mut f,
        &call_ind_numbered(&[1, 1, 2, 0, 2, 3, 0, 0], &[(1, 1, "")]),
    );
    let c = *m.status.calls.find(1).unwrap();
    assert_eq!((c.presentation, c.since), (1, 0));
    m.now = 9_000;
    m.handle(&mut f, &call_ind(&[1, 1, 3, 0, 2, 3, 0, 0]));
    let c = *m.status.calls.find(1).unwrap();
    assert_eq!(
        (c.state, c.presentation, c.since),
        (call::CONVERSATION, 1, 9_000)
    );
    m.now = 20_000;
    // a second call joins: both multiparty; call 1 keeps its connect time
    m.handle(
        &mut f,
        &call_ind(&[2, 1, 3, 0, 2, 3, 1, 0, 2, 3, 0, 1, 3, 1, 0]),
    );
    let (a, b) = (
        *m.status.calls.find(1).unwrap(),
        *m.status.calls.find(2).unwrap(),
    );
    assert_eq!((a.since, a.multiparty, a.presentation), (9_000, true, 1));
    assert_eq!((b.since, b.multiparty), (20_000, true));
}

#[test]
fn blocked_call_is_rejected_before_it_rings() {
    use phone_rules::block::{Rule, why};
    let (mut m, mut f) = (modem(), fake());
    m.block.rules.push(Rule {
        prefix: true,
        calls: true,
        messages: false,
        number: String::from("+1900"),
    });
    m.handle(&mut f, &ns(VOICE, 77));
    // setup without a number: nothing decided yet, nothing sent
    let n = f.sent.len();
    m.handle(&mut f, &call_ind(&[1, 1, 10, 0, 2, 3, 0, 0]));
    assert_eq!(f.sent.len(), n);
    m.handle(
        &mut f,
        &call_ind_numbered(&[1, 1, 2, 0, 2, 3, 0, 0], &[(1, 0, "+19005550101")]),
    );
    let (_, _, id, _) = req(f.sent.last().unwrap());
    assert_eq!((id, sent_tlv(&f, 1)), (voice::MANAGE_CALLS, alloc::vec![1]));
    assert_eq!(m.status.calls.len, 0, "clients never see a blocked call");
    assert_eq!(m.status.call.state, call::END);
    m.handle(
        &mut f,
        &call_ind_numbered(&[1, 1, 9, 0, 2, 3, 0, 0], &[(1, 0, "+19005550101")]),
    );
    let e = m.log.entries[0];
    assert_eq!((e.kind, e.why), (modem_ipc::log::BLOCKED, why::PREFIX));
    assert_eq!(m.status.missed, 0);
    // another number rings normally
    m.handle(
        &mut f,
        &call_ind_numbered(&[1, 2, 2, 0, 2, 3, 0, 0], &[(2, 0, "+37491000000")]),
    );
    assert_eq!(
        (m.status.calls.len, m.status.call.state),
        (1, call::INCOMING)
    );
}

#[test]
fn private_callers_blocked_by_switch() {
    let (mut m, mut f) = (modem(), fake());
    m.block.private = true;
    m.handle(&mut f, &ns(VOICE, 77));
    m.handle(
        &mut f,
        &call_ind_numbered(&[1, 1, 2, 0, 2, 3, 0, 0], &[(1, 1, "")]),
    );
    assert_eq!(m.status.calls.len, 0);
    assert_eq!(req(f.sent.last().unwrap()).2, voice::MANAGE_CALLS);
}

#[test]
fn end_and_answer_releases_active_for_waiting() {
    let (mut m, mut f) = (modem(), fake());
    m.handle(&mut f, &ns(VOICE, 77));
    m.handle(
        &mut f,
        &call_ind(&[2, 1, 3, 0, 1, 3, 0, 0, 2, 7, 0, 2, 3, 0, 0]),
    );
    m.manage(&mut f, modem_ipc::manage::END_ANSWER, 0).unwrap();
    let (_, _, id, _) = req(f.sent.last().unwrap());
    assert_eq!((id, sent_tlv(&f, 1)), (voice::MANAGE_CALLS, alloc::vec![2]));
    let (mut m, mut f) = (modem(), fake());
    m.handle(&mut f, &ns(VOICE, 77));
    m.handle(&mut f, &call_ind(&[1, 1, 3, 0, 1, 3, 0, 0]));
    assert_eq!(
        m.manage(&mut f, modem_ipc::manage::END_ANSWER, 0),
        Err(KERR_NOT_READY)
    );
}

fn nas_reads(f: &Fake) -> usize {
    f.sent
        .iter()
        .filter(|packet| {
            let (h, payload) = qrtr::decode(packet).unwrap();
            h.src_port == 0x22
                && qmi::Msg::parse(payload).is_some_and(|m| m.id == nas::GET_SERVING_SYSTEM)
        })
        .count()
}

#[test]
fn final_end_observes_actual_lte_once_per_second_without_radio_changes() {
    let (mut m, mut f) = (modem(), fake());
    m.state.ipa_up = true;
    m.time_asked = true; // only the serving-system reads are counted below
    m.handle(&mut f, &ns(NAS, 51));
    m.handle(&mut f, &ns(VOICE, 77));
    advance(&mut m, &mut f, 1000);
    m.handle(&mut f, &call_ind(&[1, 1, 3, 0, 1, 3, 0, 0]));
    advance(&mut m, &mut f, 2000);
    m.handle(&mut f, &call_ind(&[1, 1, 9, 0, 1, 3, 0, 0]));
    f.sent.clear();
    advance(&mut m, &mut f, 2000);
    advance(&mut m, &mut f, 2999);
    assert_eq!(nas_reads(&f), 1);
    advance(&mut m, &mut f, 3000);
    assert_eq!(nas_reads(&f), 2);
    let lte = qmi::Builder::new(qmi::INDICATION, 1, nas::GET_SERVING_SYSTEM)
        .tlv(1, &[1, 1, 1, 2, 1, 8])
        .finish();
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 51, 0x22, &lte));
    assert!(m.post_call.is_none());
    advance(&mut m, &mut f, 4000);
    assert_eq!(nas_reads(&f), 2);
    assert!(
        f.sent
            .iter()
            .all(|packet| req(packet).2 == nas::GET_SERVING_SYSTEM)
    );
    assert_eq!(m.status.generation, 0);
    assert!(
        f.logs
            .iter()
            .any(|line| line.contains("LTE confirmed after 1000ms"))
    );
}

#[test]
fn post_call_queries_are_bounded_and_end_of_one_call_does_not_arm_them() {
    let (mut m, mut f) = (modem(), fake());
    m.handle(&mut f, &ns(NAS, 51));
    m.handle(&mut f, &ns(VOICE, 77));
    advance(&mut m, &mut f, 1000);
    m.handle(
        &mut f,
        &call_ind(&[2, 1, 3, 0, 1, 3, 0, 0, 2, 7, 0, 2, 3, 0, 0]),
    );
    m.handle(
        &mut f,
        &call_ind(&[2, 1, 9, 0, 1, 3, 0, 0, 2, 7, 0, 2, 3, 0, 0]),
    );
    assert!(m.post_call.is_none());
    advance(&mut m, &mut f, 2000);
    m.handle(&mut f, &call_ind(&[0]));
    assert!(m.post_call.is_some());
    f.sent.clear();
    for now in (2000..=32_000).step_by(1000) {
        advance(&mut m, &mut f, now);
    }
    assert_eq!(nas_reads(&f), 30);
    assert!(m.post_call.is_none());
    assert!(f.sent.iter().all(|packet| matches!(
        req(packet).2,
        nas::GET_SERVING_SYSTEM | nas::GET_SIGNAL_INFO
    )));
    m.handle(&mut f, &call_ind(&[1, 3, 3, 0, 1, 3, 0, 0]));
    m.handle(&mut f, &call_ind(&[0]));
    assert!(m.post_call.is_some());
    m.handle(&mut f, &call_ind(&[1, 4, 2, 0, 2, 3, 0, 0]));
    assert!(m.post_call.is_none());
}

#[test]
fn routine_poll_does_not_insert_an_off_cadence_post_call_query() {
    let (mut m, mut f) = (modem(), fake());
    m.handle(&mut f, &ns(NAS, 51));
    m.handle(&mut f, &ns(VOICE, 77));
    advance(&mut m, &mut f, 29_500);
    m.handle(&mut f, &call_ind(&[1, 1, 3, 0, 1, 3, 0, 0]));
    m.handle(&mut f, &call_ind(&[0]));
    f.sent.clear();
    advance(&mut m, &mut f, 29_500);
    assert_eq!(nas_reads(&f), 1);
    advance(&mut m, &mut f, 30_000); // the routine 30s poll is due halfway between post-call queries
    advance(&mut m, &mut f, 30_499);
    assert_eq!(nas_reads(&f), 1);
    advance(&mut m, &mut f, 30_500);
    assert_eq!(nas_reads(&f), 2);
}

#[test]
fn airplane_cancels_post_call_observation() {
    let (mut m, mut f) = (modem(), fake());
    m.handle(&mut f, &ns(VOICE, 77));
    m.handle(&mut f, &call_ind(&[1, 1, 3, 0, 1, 3, 0, 0]));
    m.handle(&mut f, &call_ind(&[0]));
    assert!(m.post_call.is_some());
    let _ = m.airplane(&mut f, true);
    assert!(m.post_call.is_none());
}

#[test]
fn manage_error_and_timeout_do_not_change_modem_reported_state() {
    let (mut m, mut f) = (modem(), fake());
    m.handle(&mut f, &ns(VOICE, 77));
    m.handle(&mut f, &call_ind(&[1, 1, 3, 0, 1, 3, 0, 0]));
    m.manage(&mut f, modem_ipc::manage::HOLD, 1).unwrap();
    let (_, _, id, txn) = req(f.sent.last().unwrap());
    assert_eq!(id, voice::MANAGE_CALLS);
    assert!(m.status.pending);
    assert_eq!(m.status.call.state, call::CONVERSATION);
    let failure = qmi::Builder::new(qmi::RESPONSE, txn, id).result(3).finish();
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 77, 0x23, &failure));
    assert!(!m.status.pending);
    assert_eq!(m.status.result, -1003);
    assert_eq!(m.status.call.state, call::CONVERSATION);
    m.manage(&mut f, modem_ipc::manage::HOLD, 1).unwrap();
    advance(&mut m, &mut f, 15_000);
    assert!(!m.status.pending);
    assert_eq!(m.status.result, -33);
}

#[test]
fn airplane_confirmation_never_claims_online_and_survives_sim_events() {
    let (mut m, mut f) = (modem(), fake());
    m.status.airplane = true;
    m.handle(&mut f, &ns(DMS, 68));
    let (_, _, id, txn) = req(f.sent.last().unwrap());
    assert_eq!(id, DMS_SET_OPERATING_MODE);
    let (_, payload) = qrtr::decode(f.sent.last().unwrap()).unwrap();
    assert_eq!(qmi::Msg::parse(payload).unwrap().tlv(1), Some(&[1][..]));
    m.handle(
        &mut f,
        &from_modem(qrtr::TYPE_DATA, 68, 0x20, &ok(txn, id).finish()),
    );
    assert!(!m.status.radio_online);
    assert!(!m.status.pending);
    m.handle(&mut f, &ns(UIM, 55));
    let c = qmi::Builder::new(qmi::INDICATION, 1, uim::STATUS_IND)
        .tlv(0x10, &[0, 0, 0, 0, 0, 0, 0, 0, 1, 1])
        .finish();
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 55, 0x21, &c));
    advance(&mut m, &mut f, 5000);
    assert!(!m.status.radio_online && m.status.airplane);
    m.event(
        &mut fake(),
        Event::Reset {
            subsystem: hub_ipc::Subsystem::Mss,
            generation: m.status.generation.wrapping_add(1),
        },
    );
    assert!(m.status.airplane);
}

#[test]
fn rejected_pin_updates_attempts_and_is_never_retried() {
    let (mut m, mut f) = (modem(), fake());
    m.handle(&mut f, &ns(UIM, 55));
    m.status.sim = modem_ipc::sim::PIN_REQUIRED;
    assert_eq!(m.verify_pin(&mut f, b"123"), Err(KERR_ARGUMENT));
    m.verify_pin(&mut f, b"1234").unwrap();
    let (_, _, id, txn) = req(f.sent.last().unwrap());
    assert_eq!(id, uim::VERIFY_PIN);
    let (_, payload) = qrtr::decode(f.sent.last().unwrap()).unwrap();
    assert_eq!(qmi::Msg::parse(payload).unwrap().tlv(1), Some(&[6, 0][..]));
    let rejected = qmi::Builder::new(qmi::RESPONSE, txn, id)
        .result(12)
        .tlv(0x10, &[0, 10])
        .finish();
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 55, 0x21, &rejected));
    assert_eq!(m.status.sim, modem_ipc::sim::BLOCKED);
    assert_eq!(m.status.pin_attempts, 0);
    let count = f
        .sent
        .iter()
        .filter(|p| req(p).2 == uim::VERIFY_PIN)
        .count();
    advance(&mut m, &mut f, 60_000);
    assert_eq!(
        f.sent
            .iter()
            .filter(|p| qmi::Msg::parse(qrtr::decode(p).unwrap().1)
                .is_some_and(|q| q.id == uim::VERIFY_PIN))
            .count(),
        count
    );
    assert_eq!(m.verify_pin(&mut f, b"1234"), Err(KERR_NOT_READY));
}

#[test]
fn cancel_before_delayed_dial_response_sends_end_with_assigned_id() {
    let (mut m, mut f) = (modem(), fake());
    m.handle(&mut f, &ns(VOICE, 77));
    m.dial(&mut f, "+37400000001").unwrap();
    let (_, _, _, txn) = req(f.sent.last().unwrap());
    assert_eq!(m.status.call.id, 0);
    assert!(m.status.pending);
    assert_eq!(m.end_call(&mut f, 0), Ok(()));
    assert!(m.hangup_pending);
    let response = ok(txn, voice::DIAL).u8(0x10, 7).finish();
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 77, 0x23, &response));
    let (_, _, id, _) = req(f.sent.last().unwrap());
    assert_eq!(id, voice::END);
    let (_, p) = qrtr::decode(f.sent.last().unwrap()).unwrap();
    assert_eq!(qmi::Msg::parse(p).unwrap().tlv(1), Some(&[7][..]));
    assert_eq!(m.status.call.state, call::DISCONNECTING);
}

#[test]
fn watchdog_detects_three_lost_poll_cycles_but_not_airplane_idle() {
    let (mut m, mut f) = (modem(), fake());
    m.handle(&mut f, &ns(NAS, 70));
    advance(&mut m, &mut f, 1);
    assert!(!m.watchdog());
    advance(&mut m, &mut f, 90_002);
    assert!(m.watchdog());
    m.status.airplane = true;
    assert!(!m.watchdog());
}

#[test]
fn cancel_after_dial_indication_before_response_sends_end() {
    let (mut m, mut f) = (modem(), fake());
    m.handle(&mut f, &ns(VOICE, 77));
    m.dial(&mut f, "+37400000001").unwrap();
    m.handle(&mut f, &call_ind(&[1, 7, 1, 0, 1, 3, 0, 0]));
    assert!(m.status.pending);
    assert_eq!(m.status.call.id, 7);
    assert_eq!(m.end_call(&mut f, 7), Ok(()));
    assert_eq!(req(f.sent.last().unwrap()).2, voice::END);
}

fn port_of(f: &Fake, their_port: u32) -> u32 {
    f.sent
        .iter()
        .map(|p| req(p))
        .find(|r| r.1 == their_port)
        .unwrap()
        .0
}

#[test]
fn refused_online_is_asked_three_times_then_registration_means_online() {
    let (mut m, mut f) = (modem(), fake());
    m.handle(&mut f, &ns(DMS, 68));
    let dms = port_of(&f, 68);
    m.status.sim = modem_ipc::sim::PRESENT;
    let mut asked = 0;
    for i in 0..10u64 {
        f.sent.clear();
        advance(&mut m, &mut f, 1 + i * 2500);
        let mode: Vec<_> = f
            .sent
            .iter()
            .map(|p| req(p))
            .filter(|r| r.2 == DMS_SET_OPERATING_MODE)
            .collect();
        for (_, _, id, txn) in mode {
            asked += 1;
            // "invalid transition": the modem is already online (C5)
            let refused = qmi::Builder::new(qmi::RESPONSE, txn, id).result(3).finish();
            m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 68, dms, &refused));
        }
    }
    assert_eq!(
        asked, RADIO_REFUSALS as usize,
        "bounded, not a resend every 2 s"
    );
    assert!(!m.status.pending);
    // NAS says registered: the radio is online whatever Set Operating Mode said
    m.handle(&mut f, &ns(NAS, 51));
    let nas_port = port_of(&f, 51);
    let home = qmi::Builder::new(qmi::INDICATION, 1, nas::GET_SERVING_SYSTEM)
        .tlv(1, &[1, 1, 1, 2, 1, 8])
        .finish();
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 51, nas_port, &home));
    assert!(m.status.radio_online);
    // a new intent may be asked for again
    f.sent.clear();
    m.airplane(&mut f, true).unwrap();
    assert!(f.sent.iter().any(|p| req(p).2 == DMS_SET_OPERATING_MODE));
}

#[test]
fn airplane_intent_is_always_recorded() {
    // no SIM and no DMS client yet: the intent still holds (the caller saves it), the radio follows later
    let (mut m, mut f) = (modem(), fake());
    assert_eq!(m.airplane(&mut f, true), Ok(()));
    assert!(m.status.airplane);
    assert_eq!(m.airplane(&mut f, false), Ok(()));
    assert!(!m.status.airplane);
}

#[test]
fn pin_attempts_unknown_until_uim_reports() {
    let mut m = modem();
    assert_eq!(m.status.pin_attempts, 255);
    m.status.pin_attempts = 2;
    m.event(
        &mut fake(),
        Event::Reset {
            subsystem: hub_ipc::Subsystem::Mss,
            generation: m.status.generation.wrapping_add(1),
        },
    );
    assert_eq!(m.status.pin_attempts, 255);
    assert_eq!(Cellular::new().status.pin_attempts, 255);
}

fn deliver_hi(from_digits: &[u8], toa: u8, ndigits: u8) -> Vec<u8> {
    let mut p = alloc::vec![0x04, ndigits, toa];
    p.extend_from_slice(from_digits);
    p.extend_from_slice(&[
        0x00, 0x00, 0x62, 0x11, 0x51, 0x01, 0x50, 0x00, 0x61, 2, 0xC8, 0x34,
    ]);
    p
}

fn transfer(txn: u32, tpdu: &[u8]) -> Vec<u8> {
    let mut t = alloc::vec![0];
    t.extend_from_slice(&txn.to_le_bytes());
    t.push(6);
    t.extend_from_slice(&(tpdu.len() as u16).to_le_bytes());
    t.extend_from_slice(tpdu);
    from_modem(
        qrtr::TYPE_DATA,
        88,
        0x27,
        &qmi::Builder::new(qmi::INDICATION, 9, wms::EVENT_REPORT_IND)
            .tlv(0x11, &t)
            .finish(),
    )
}

#[test]
fn wms_comes_up_and_drains_storage() {
    let (mut m, mut f) = (modem(), fake());
    m.handle(&mut f, &ns(WMS, 88));
    let ids: Vec<u16> = f.sent.iter().map(|p| req(p).2).collect();
    assert_eq!(
        ids,
        [
            wms::SET_EVENT_REPORT,
            wms::SET_ROUTES,
            wms::GET_SMSC,
            wms::LIST_MESSAGES,
            wms::LIST_MESSAGES,
            wms::LIST_MESSAGES,
            wms::LIST_MESSAGES
        ]
    );
    // the first list (UIM, not read) has index 3: it is read, saved, then deleted
    let (_, _, _, txn) = req(&f.sent[3]);
    let list = ok(txn, wms::LIST_MESSAGES)
        .tlv(1, &[1, 0, 0, 0, 3, 0, 0, 0, 1])
        .finish();
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 88, 0x27, &list));
    let (_, _, id, txn) = req(f.sent.last().unwrap());
    assert_eq!(
        (id, sent_tlv(&f, 1)),
        (wms::RAW_READ, alloc::vec![0, 3, 0, 0, 0])
    );
    let tpdu = deliver_hi(&HI_FROM, 0x91, 11);
    let mut raw = alloc::vec![1, 6];
    raw.extend_from_slice(&(tpdu.len() as u16 + 1).to_le_bytes());
    raw.push(0); // stored messages may carry the SMSC address
    raw.extend_from_slice(&tpdu);
    let read = ok(txn, wms::RAW_READ).tlv(1, &raw).finish();
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 88, 0x27, &read));
    assert_eq!(m.status.sms_unread, 1);
    assert_ne!(
        req(f.sent.last().unwrap()).2,
        wms::DELETE,
        "not before the store is saved"
    );
    m.sms_saved(&mut f);
    assert_eq!(req(f.sent.last().unwrap()).2, wms::DELETE);
}

#[test]
fn smsc_and_storage_asked_again_once_registered() {
    let (mut m, mut f) = (modem(), fake());
    m.handle(&mut f, &ns(WMS, 88));
    let (_, _, _, txn) = req(&f.sent[2]);
    let refused = qmi::Builder::new(qmi::RESPONSE, txn, wms::GET_SMSC)
        .result(52)
        .finish();
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 88, 0x27, &refused));
    let n = f.sent.len();
    advance(&mut m, &mut f, 20_000);
    assert!(
        f.sent[n..].iter().all(|p| req(p).2 != wms::GET_SMSC),
        "not before registration"
    );
    m.status.reg = modem_ipc::reg::HOME;
    advance(&mut m, &mut f, 21_000);
    let at = n + f.sent[n..]
        .iter()
        .position(|p| req(p).2 == wms::GET_SMSC)
        .expect("asked again");
    let (_, _, _, txn) = req(&f.sent[at]);
    let mut t = b"145".to_vec();
    t.push(4);
    t.extend_from_slice(b"+374");
    let answer = ok(txn, wms::GET_SMSC).tlv(1, &t).finish();
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 88, 0x27, &answer));
    assert_eq!(m.sms.settings.smsc.as_str(), "+374");
    let n = f.sent.len();
    advance(&mut m, &mut f, 60_000);
    assert!(
        f.sent[n..].iter().all(|p| req(p).2 != wms::GET_SMSC),
        "not again once it worked"
    );
}

#[test]
fn incoming_message_is_acked_only_after_saving() {
    let (mut m, mut f) = (modem(), fake());
    m.handle(&mut f, &ns(WMS, 88));
    let before = m.status.sms_revision;
    m.handle(&mut f, &transfer(5, &deliver_hi(&HI_FROM, 0x91, 11)));
    assert!(m.sms.dirty && m.sms.alert);
    assert_ne!(m.status.sms_revision, before);
    assert_eq!((m.status.sms_unread, m.status.sms_senders), (1, 1));
    let msg = m.sms.messages.last().unwrap();
    assert_eq!(
        (msg.entry.number.as_str(), msg.text.as_str()),
        ("+37491234567", "Hi")
    );
    assert_ne!(req(f.sent.last().unwrap()).2, wms::SEND_ACK);
    m.sms_saved(&mut f);
    assert_eq!(req(f.sent.last().unwrap()).2, wms::SEND_ACK);
    assert_eq!(sent_tlv(&f, 1), wms::ack(5));
}

#[test]
fn blocked_sender_is_kept_without_alert() {
    use phone_rules::block::Rule;
    let (mut m, mut f) = (modem(), fake());
    m.block.rules.push(Rule {
        prefix: true,
        calls: false,
        messages: true,
        number: String::from("+374 91"),
    });
    m.handle(&mut f, &ns(WMS, 88));
    m.handle(&mut f, &transfer(6, &deliver_hi(&HI_FROM, 0x91, 11)));
    assert!(!m.sms.alert);
    assert_eq!(m.status.sms_unread, 0);
    assert_eq!(m.sms.messages[0].entry.state, modem_ipc::sms::BLOCKED);
    m.sms_saved(&mut f);
    assert_eq!(
        req(f.sent.last().unwrap()).2,
        wms::SEND_ACK,
        "the network still gets its ack"
    );
}

#[test]
fn send_then_delivery_report() {
    let (mut m, mut f) = (modem(), fake());
    m.handle(&mut f, &ns(WMS, 88));
    let id = m.sms_send(&mut f, "+37491234567", "Hi").unwrap();
    let (_, _, msg, txn) = req(f.sent.last().unwrap());
    assert_eq!(msg, wms::RAW_SEND);
    let raw = sent_tlv(&f, 1);
    assert_eq!(
        &raw[..4],
        &[6, raw.len() as u8 - 3, 0, 0],
        "format, length, empty SMSC"
    );
    assert_eq!(raw[4] & 0x23, 0x21, "SMS-SUBMIT asking for a report");
    let sent = ok(txn, wms::RAW_SEND).tlv(1, &[42, 0]).finish();
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 88, 0x27, &sent));
    assert_eq!(m.sms.messages[0].entry.state, modem_ipc::sms::SENT);
    // SMS-STATUS-REPORT for reference 42: delivered
    let mut r = alloc::vec![0x06, 42, 11, 0x91];
    r.extend_from_slice(&HI_FROM);
    r.extend_from_slice(&[0x62, 0x11, 0x51, 0x01, 0x50, 0x00, 0x61]);
    r.extend_from_slice(&[0x62, 0x11, 0x51, 0x01, 0x50, 0x20, 0x61]);
    r.push(0);
    m.handle(&mut f, &transfer(7, &r));
    let e = m.sms.messages[0].entry;
    assert_eq!((e.id, e.state), (id, modem_ipc::sms::DELIVERED));
    assert_eq!(m.status.sms_unread, 0);
}

#[test]
fn a_refused_send_fails_and_can_be_retried() {
    let (mut m, mut f) = (modem(), fake());
    assert_eq!(m.sms_send(&mut f, "+1 555 abc", "x"), Err(KERR_ARGUMENT));
    // queued while WMS isn't up, sent once it is
    let id = m.sms_send(&mut f, "+37491234567", "x").unwrap();
    assert!(f.sent.is_empty());
    m.handle(&mut f, &ns(WMS, 88));
    let (_, _, msg, txn) = req(f.sent.last().unwrap());
    assert_eq!(msg, wms::RAW_SEND);
    let refused = qmi::Builder::new(qmi::RESPONSE, txn, wms::RAW_SEND)
        .result(0x30)
        .finish();
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 88, 0x27, &refused));
    assert_eq!(m.sms.messages[0].entry.state, modem_ipc::sms::FAILED);
    m.sms_edit(&mut f, modem_ipc::sms::EDIT_RETRY, id).unwrap();
    assert_eq!(req(f.sent.last().unwrap()).2, wms::RAW_SEND);
    assert_eq!(m.sms.messages[0].entry.state, modem_ipc::sms::SENDING);
    // no answer for a minute: failed again
    advance(&mut m, &mut f, 61_000);
    assert_eq!(m.sms.messages[0].entry.state, modem_ipc::sms::FAILED);
}

#[test]
fn bus_packet_requires_bound_client_port_and_server_source() {
    let (mut m, mut f) = (modem(), fake());
    m.bind(&mut f).unwrap();
    assert_eq!(f.bound, CLIENTS);
    m.handle(&mut f, &ns(DMS, 68));
    let (_, _, id, txn) = req(&f.sent[0]);
    let payload = ok(txn, id).tlv(1, b"revision").finish();
    for (src, dst_port) in [((1, 68), 0x20), ((0, 69), 0x20), ((0, 68), 0x21)] {
        m.event(
            &mut f,
            Event::Packet {
                src,
                dst_port,
                payload: payload.clone(),
            },
        );
        assert!(m.revision.is_none());
        assert!(m.clients[0].pending.contains(&(txn, id)));
    }
    m.event(
        &mut f,
        Event::Packet {
            src: (0, 68),
            dst_port: 0x20,
            payload,
        },
    );
    assert_eq!(m.revision.as_deref(), Some("revision"));
    assert!(!m.clients[0].pending.contains(&(txn, id)));
}

#[test]
fn server_down_preserves_client_until_reset_invalidates_generation() {
    let (mut m, mut f) = (modem(), fake());
    m.handle(&mut f, &ns(DMS, 68));
    let (_, _, id, txn) = req(&f.sent[0]);
    m.event(
        &mut f,
        Event::ServerDown(Server {
            service: DMS,
            instance: 1,
            node: 0,
            port: 68,
        }),
    );
    let payload = ok(txn, id).tlv(1, b"revision").finish();
    m.event(
        &mut f,
        Event::Packet {
            src: (0, 68),
            dst_port: 0x20,
            payload: payload.clone(),
        },
    );
    assert_eq!(m.revision.as_deref(), Some("revision"));
    m.status.airplane = true;
    m.event(
        &mut f,
        Event::Reset {
            subsystem: hub_ipc::Subsystem::Mss,
            generation: 9,
        },
    );
    assert_eq!(m.status.generation, 9);
    assert!(m.status.airplane);
    assert!(m.clients.is_empty());
    assert_eq!(m.status.pin_attempts, 255);
    m.event(
        &mut f,
        Event::Packet {
            src: (0, 68),
            dst_port: 0x20,
            payload,
        },
    );
    assert!(m.revision.is_none());
}
