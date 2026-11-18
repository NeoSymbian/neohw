// SPDX-License-Identifier: Apache-2.0
extern crate alloc;
use ::qrtr::qmi;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use hub::qrtr::{self, Header, PORT_CTRL};
use hub::{NODE, Platform, ipa};
use modem::{KERR_ARGUMENT, KERR_IN_USE, KERR_NOT_READY, modem_ipc, voice};
use modem_ipc::call;
const DMS: u32 = 2;
const UIM: u32 = 11;
const DMS_GET_REVISION: u16 = 0x23;
const DMS_GET_IDS: u16 = 0x25;
const DMS_SET_OPERATING_MODE: u16 = 0x2e;
const UIM_GET_CARD_STATUS: u16 = 0x2f;
const VOICE: u32 = voice::SERVICE;
const IPA_PORT: u32 = 0x13;
#[allow(dead_code)]
#[path = "support/encoded.rs"]
mod inprocess;
use inprocess::Modem;
#[derive(Default)]
struct Fake {
    sent: Vec<Vec<u8>>,
    nodes: Vec<u32>,
    /// refuse this many sends (no receive intent yet)
    refuse: usize,
    mem: Vec<u8>,
    logs: Vec<String>,
}

impl Platform for Fake {
    fn send(&mut self, node: u32, packet: &[u8]) -> Result<(), i32> {
        if self.refuse > 0 {
            self.refuse -= 1;
            return Err(KERR_NOT_READY);
        }
        self.sent.push(packet.to_vec());
        self.nodes.push(node);
        Ok(())
    }
    fn rmtfs_read(&mut self, offset: u32, buf: &mut [u8]) -> Result<(), i32> {
        buf.copy_from_slice(&self.mem[offset as usize..offset as usize + buf.len()]);
        Ok(())
    }
    fn rmtfs_write(&mut self, offset: u32, data: &[u8]) -> Result<(), i32> {
        self.mem[offset as usize..offset as usize + data.len()].copy_from_slice(data);
        Ok(())
    }
    fn log(&mut self, args: fmt::Arguments) {
        self.logs.push(format!("{args}"));
    }
}

fn fake() -> Fake {
    Fake {
        mem: alloc::vec![0; 0x20_0000],
        ..Default::default()
    }
}
fn modem() -> Modem {
    let img = |n: usize, fill: u8| alloc::vec![fill; n];
    Modem::new(
        [
            img(0x20_0000, 1),
            img(0x20_0000, 2),
            img(0x20_0000, 3),
            img(0x2_0000, 4),
        ],
        0xfc20_1000,
    )
}
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
fn dms_and_uim_answers_then_online() {
    let (mut m, mut f) = (modem(), fake());
    m.handle(&mut f, &ns(DMS, 68));
    assert_eq!(f.sent.len(), 2);
    assert_eq!(req(&f.sent[0]), (0x20, 68, DMS_GET_REVISION, 1));
    assert_eq!(req(&f.sent[1]), (0x20, 68, DMS_GET_IDS, 2));
    let rev = ok(1, DMS_GET_REVISION).tlv(1, b"MPSS.AT.4.0.2").finish();
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 68, 0x20, &rev));
    assert_eq!(m.revision.as_deref(), Some("MPSS.AT.4.0.2"));
    let ids = ok(2, DMS_GET_IDS).tlv(0x11, b"351234567890123").finish();
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 68, 0x20, &ids));
    assert!(f.logs.iter().any(|l| l == "dms: IMEI 35xxxxxxxxxxx23"));
    assert!(!f.logs.iter().any(|l| l.contains("351234567890123")));
    m.handle(&mut f, &ns(UIM, 55));
    assert_eq!(
        req(f.sent.last().unwrap()),
        (0x21, 55, UIM_GET_CARD_STATUS, 2)
    );
    // a response on a transaction we never sent is not ours
    let stray = ok(9, UIM_GET_CARD_STATUS)
        .tlv(0x10, &[0, 0, 0, 0, 0, 0, 0, 0, 1, 1])
        .finish();
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 55, 0x21, &stray));
    assert_eq!(m.card, None);
    let card = ok(2, UIM_GET_CARD_STATUS)
        .tlv(0x10, &[0, 0, 0, 0, 0, 0, 0, 0, 1, 1])
        .finish();
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 55, 0x21, &card));
    assert_eq!((m.card, m.status.sim), (Some(1), modem_ipc::sim::PRESENT));
    let online = f.sent.last().unwrap().clone();
    assert_eq!(req(&online), (0x20, 68, DMS_SET_OPERATING_MODE, 3));
    let (_, pay) = qrtr::decode(&online).unwrap();
    assert_eq!(qmi::Msg::parse(pay).unwrap().tlv(1), Some(&[0u8][..]));
    m.handle(
        &mut f,
        &from_modem(
            qrtr::TYPE_DATA,
            68,
            0x20,
            &ok(3, DMS_SET_OPERATING_MODE).finish(),
        ),
    );
    assert_eq!(m.status.modem, modem_ipc::modem::ONLINE);
}

#[test]
fn ipa_handshake_through_the_router() {
    let (mut m, mut f) = (modem(), fake());
    m.router.ipa_up = true;
    m.start(&mut f);
    assert_eq!(f.sent.len(), 5); // HELLO + rmtfs, locator, TFTP and IPA servers
    let n = f.sent.len();
    m.handle(
        &mut f,
        &from_modem(
            qrtr::TYPE_NEW_SERVER,
            PORT_CTRL,
            PORT_CTRL,
            &qrtr::ctrl(4, [ipa::SERVICE, qrtr::instance_word(1, 2), 0, 30]),
        ),
    );
    assert_eq!(f.sent.len(), n + 1);
    let (_, dst, id, txn) = req(f.sent.last().unwrap());
    assert_eq!((dst, id), (30, ipa::INIT_DRIVER));
    m.handle(
        &mut f,
        &from_modem(
            qrtr::TYPE_DATA,
            30,
            0x24,
            &ok(txn, ipa::INIT_DRIVER).finish(),
        ),
    );
    for id in [ipa::DRIVER_INIT_COMPLETE, ipa::INDICATION_REGISTER] {
        let q = qmi::Builder::new(qmi::REQUEST, 3, id).u8(1, 0).finish();
        m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 41, IPA_PORT, &q));
    }
    let (h, pay) = qrtr::decode(f.sent.last().unwrap()).unwrap();
    let ind = qmi::Msg::parse(pay).unwrap();
    assert_eq!((h.src_port, h.dst_port), (IPA_PORT, 30)); // to the modem's server, not the requester
    assert_eq!((ind.ty, ind.id), (qmi::INDICATION, ipa::INIT_COMPLETE));
    assert!(f.logs.iter().any(|l| l == "ipa: handshake complete"));
}

#[test]
fn calls_out_and_in() {
    let (mut m, mut f) = (modem(), fake());
    assert_eq!(m.dial(&mut f, "+37400000012"), Err(KERR_NOT_READY));
    m.handle(&mut f, &ns(VOICE, 50));
    assert_eq!(req(f.sent.last().unwrap()).2, voice::INDICATION_REGISTER);
    assert_eq!(m.dial(&mut f, "112"), Err(KERR_ARGUMENT));
    assert_eq!(m.dial(&mut f, "+37400000012"), Ok(()));
    assert!(f.logs.iter().all(|l| !l.contains("37400000012")));
    let (_, _, id, txn) = req(f.sent.last().unwrap());
    assert_eq!(id, voice::DIAL);
    assert_eq!(m.dial(&mut f, "+37400000099"), Err(KERR_IN_USE));
    // hang up before the modem numbered the call: sent once it has the id
    assert_eq!(m.hangup(&mut f), Ok(()));
    let n = f.sent.len();
    m.handle(
        &mut f,
        &from_modem(
            qrtr::TYPE_DATA,
            50,
            0x23,
            &ok(txn, voice::DIAL).u8(0x10, 1).finish(),
        ),
    );
    assert_eq!(f.sent.len(), n + 1);
    assert_eq!(req(f.sent.last().unwrap()).2, voice::END);
    let status = |calls: &[u8]| {
        qmi::Builder::new(qmi::INDICATION, 1, voice::ALL_CALL_STATUS_IND)
            .tlv(1, calls)
            .finish()
    };
    m.handle(
        &mut f,
        &from_modem(
            qrtr::TYPE_DATA,
            50,
            0x23,
            &status(&[1, 1, 9, 0, 1, 3, 0, 0]),
        ),
    );
    assert_eq!(
        (m.status.call.state, m.status.call.number.as_str()),
        (call::END, "+37400000012")
    );
    // incoming: answer refused during setup, accepted when incoming
    m.handle(
        &mut f,
        &from_modem(
            qrtr::TYPE_DATA,
            50,
            0x23,
            &status(&[1, 2, 0x0a, 0, 2, 3, 0, 0]),
        ),
    );
    assert_eq!(m.answer(&mut f), Err(KERR_NOT_READY));
    m.handle(
        &mut f,
        &from_modem(
            qrtr::TYPE_DATA,
            50,
            0x23,
            &status(&[1, 2, 2, 0, 2, 3, 0, 0]),
        ),
    );
    assert!(m.status.call.incoming);
    assert_eq!(m.answer(&mut f), Ok(()));
    let (_, pay) = qrtr::decode(f.sent.last().unwrap()).unwrap();
    let a = qmi::Msg::parse(pay).unwrap();
    assert_eq!((a.id, a.tlv(1)), (voice::ANSWER, Some(&[2u8][..])));
}

#[test]
fn a_refused_voice_subscription_is_sent_again() {
    let (mut m, mut f) = (modem(), fake());
    f.refuse = 1;
    m.handle(&mut f, &ns(VOICE, 50));
    assert!(f.sent.is_empty());
    m.tick(&mut f, 1000);
    assert_eq!(f.sent.len(), 1);
    assert_eq!(req(&f.sent[0]).2, voice::INDICATION_REGISTER);
}

#[test]
fn cellular_packet_keeps_its_destination_port_and_expected_peer() {
    let (mut m, mut f) = (modem(), fake());
    m.handle(&mut f, &ns(DMS, 68));
    let revision = ok(1, DMS_GET_REVISION).tlv(1, b"boundary").finish();
    for (src_port, dst_port) in [(68, 0x21), (69, 0x20), (68, 0x28)] {
        m.handle(
            &mut f,
            &from_modem(qrtr::TYPE_DATA, src_port, dst_port, &revision),
        );
        assert!(m.revision.is_none());
    }
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 68, 0x20, &revision));
    assert_eq!(m.revision.as_deref(), Some("boundary"));
}

#[test]
fn server_down_and_reset_cross_the_router_boundary() {
    use hub_ipc::{Event, Subsystem};
    let mut r = hub::Router::new(core::array::from_fn(|_| vec![1; 512]), 0xfc20_1000);
    let (mut c, mut f) = (modem::Cellular::new(), fake());
    c.bind(&mut inprocess::EncodedBus::new(&mut r, &mut f))
        .unwrap();
    let state = r.state();
    c.event(
        &mut inprocess::EncodedBus::new(&mut r, &mut f),
        inprocess::round_trip(&[Event::State(state)]).remove(0),
    );
    r.handle(&mut f, &ns(DMS, 68));
    let event = r.next_event().unwrap();
    assert!(matches!(event, Event::ServerUp(s) if s.service == DMS));
    c.event(
        &mut inprocess::EncodedBus::new(&mut r, &mut f),
        inprocess::round_trip(&[event]).remove(0),
    );
    let mut down = ns(DMS, 68);
    down[4..8].copy_from_slice(&qrtr::TYPE_DEL_SERVER.to_le_bytes());
    down[32..36].copy_from_slice(&qrtr::TYPE_DEL_SERVER.to_le_bytes());
    r.handle(&mut f, &down);
    let event = r.next_event().unwrap();
    assert!(matches!(event, Event::ServerDown(s) if s.port == 68));
    c.event(
        &mut inprocess::EncodedBus::new(&mut r, &mut f),
        inprocess::round_trip(&[event]).remove(0),
    );
    assert!(r.servers.is_empty());
    // Existing C5 semantics: discovery removal leaves the QMI client available until reset.
    assert_eq!(
        c.airplane(&mut inprocess::EncodedBus::new(&mut r, &mut f), true),
        Ok(())
    );
    assert_eq!(req(f.sent.last().unwrap()).2, DMS_SET_OPERATING_MODE);
    r.reset(true);
    let event = r.next_event().unwrap();
    assert_eq!(
        event,
        Event::Reset {
            subsystem: Subsystem::Mss,
            generation: 1
        }
    );
    c.event(
        &mut inprocess::EncodedBus::new(&mut r, &mut f),
        inprocess::round_trip(&[event]).remove(0),
    );
    assert_eq!(c.status.generation, 1);
    assert!(c.status.airplane);
    let sent = f.sent.len();
    c.begin_tick(&mut inprocess::EncodedBus::new(&mut r, &mut f), 30000);
    assert_eq!(f.sent.len(), sent); // the old client's port cannot transmit after reset
}

#[test]
fn shared_outbox_retries_router_and_cellular_packets_in_order() {
    use hub_ipc::Event;
    let mut r = hub::Router::new(core::array::from_fn(|_| vec![1; 512]), 0xfc20_1000);
    let (c, mut f) = (modem::Cellular::new(), fake());
    c.bind(&mut inprocess::EncodedBus::new(&mut r, &mut f))
        .unwrap();
    let mut c = c;
    let state = r.state();
    c.event(
        &mut inprocess::EncodedBus::new(&mut r, &mut f),
        inprocess::round_trip(&[Event::State(state)]).remove(0),
    );
    f.refuse = 1;
    r.handle(
        &mut f,
        &from_modem(
            qrtr::TYPE_HELLO,
            PORT_CTRL,
            PORT_CTRL,
            &qrtr::ctrl(qrtr::TYPE_HELLO, [0; 4]),
        ),
    );
    r.handle(&mut f, &ns(VOICE, 50));
    while let Some(e) = r.next_event() {
        c.event(
            &mut inprocess::EncodedBus::new(&mut r, &mut f),
            inprocess::round_trip(&[e]).remove(0),
        );
    }
    let request = qmi::Builder::new(qmi::REQUEST, 9, 0x24).finish();
    r.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 60, 0x11, &request));
    assert!(f.sent.is_empty());
    r.retry(&mut f, 999);
    assert!(f.sent.is_empty());
    r.retry(&mut f, 1000);
    let headers: Vec<_> = f.sent.iter().map(|p| qrtr::decode(p).unwrap().0).collect();
    assert_eq!(
        headers
            .iter()
            .map(|h| (h.ty, h.src_port))
            .collect::<Vec<_>>(),
        [
            (qrtr::TYPE_HELLO, PORT_CTRL),
            (qrtr::TYPE_NEW_SERVER, PORT_CTRL),
            (qrtr::TYPE_NEW_SERVER, PORT_CTRL),
            (qrtr::TYPE_NEW_SERVER, PORT_CTRL),
            (qrtr::TYPE_DATA, 0x23),
            (qrtr::TYPE_DATA, 0x11),
        ]
    );
    assert_eq!(req(&f.sent[4]), (0x23, 50, voice::INDICATION_REGISTER, 1));
    assert_eq!(req(&f.sent[5]).3, 9);
    r.retry(&mut f, 2000);
    assert_eq!(f.sent.len(), 6);
}

#[test]
fn ipa_responses_still_refresh_the_cellular_watchdog() {
    let (mut m, mut f) = (modem(), fake());
    m.router.ipa_up = true;
    m.handle(&mut f, &ns(modem::nas::SERVICE, 51));
    m.tick(&mut f, 1000);
    m.tick(&mut f, 91001);
    assert!(m.watchdog());
    m.handle(&mut f, &ns(ipa::SERVICE, 30));
    let txn = req(f.sent.last().unwrap()).3;
    m.handle(
        &mut f,
        &from_modem(
            qrtr::TYPE_DATA,
            30,
            0x24,
            &ok(txn, ipa::INIT_DRIVER).finish(),
        ),
    );
    m.tick(&mut f, 92000);
    assert!(!m.watchdog());
}
