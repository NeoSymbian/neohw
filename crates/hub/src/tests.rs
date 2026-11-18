// SPDX-License-Identifier: Apache-2.0
use super::*;
use std::format;
#[allow(dead_code)]
#[path = "../../modem/tests/support/encoded.rs"]
mod inprocess;
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
fn modem() -> Router {
    let img = |n: usize, fill: u8| alloc::vec![fill; n];
    Router::new(
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
fn reply(f: &Fake) -> qmi::Msg<'_> {
    let (_, pay) = qrtr::decode(f.sent.last().unwrap()).unwrap();
    qmi::Msg::parse(pay).unwrap()
}
fn ssctl_ns(node: u32, instance: u32, port: u32) -> Vec<u8> {
    let h = Header {
        ty: qrtr::TYPE_NEW_SERVER,
        src_node: node,
        src_port: PORT_CTRL,
        dst_node: NODE,
        dst_port: PORT_CTRL,
        ..Default::default()
    };
    let w = [
        ssctl::SERVICE,
        qrtr::instance_word(ssctl::VERSION, instance),
        node,
        port,
    ];
    qrtr::encode(&h, &qrtr::ctrl(qrtr::TYPE_NEW_SERVER, w))
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
fn support_router_serves_adsp_without_advertising_radio_storage() {
    let (mut m, mut f) = (Router::support_only(), fake());
    m.ipa_up = true; // even a prematurely set IPA flag must not expose unprepared radio resources
    assert!(m.services().all(|s| s.0 != rmtfs::SERVICE && s.0 != IPA));
    m.start(&mut f);
    assert!(f.sent.is_empty());
    let hello = Header {
        ty: qrtr::TYPE_HELLO,
        src_node: 5,
        src_port: PORT_CTRL,
        dst_node: NODE,
        dst_port: PORT_CTRL,
        ..Default::default()
    };
    m.handle(
        &mut f,
        &qrtr::encode(&hello, &qrtr::ctrl(qrtr::TYPE_HELLO, [0; 4])),
    );
    assert_eq!(f.sent.len(), 3); // HELLO, locator, TFTP
    assert!(f.nodes.iter().all(|&node| node == 5));
    let q = qmi::Builder::new(qmi::REQUEST, 3, 0x21)
        .tlv(1, b"avs/audio")
        .finish();
    let h = Header {
        ty: qrtr::TYPE_DATA,
        dst_port: LOCATOR_PORT,
        ..hello
    };
    m.handle(&mut f, &qrtr::encode(&h, &q));
    assert_eq!(reply(&f).result(), Some(Ok(())));
    let before = f.sent.len();
    let alloc = qmi::Builder::new(qmi::REQUEST, 4, 4).finish();
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 17, RMTFS_PORT, &alloc));
    assert_eq!(f.sent.len(), before); // no reply containing an invented address
}

#[test]
fn attaching_storage_preserves_adsp_discovery_and_retry_queue() {
    let (mut m, mut f) = (Router::support_only(), fake());
    f.refuse = 1;
    let h = Header {
        ty: qrtr::TYPE_HELLO,
        src_node: 5,
        src_port: PORT_CTRL,
        dst_node: NODE,
        dst_port: PORT_CTRL,
        ..Default::default()
    };
    let hello = qrtr::encode(&h, &qrtr::ctrl(qrtr::TYPE_HELLO, [0; 4]));
    m.handle(&mut f, &hello);
    let queued = m.outbox.clone();
    assert_eq!(queued.len(), 3);
    assert_eq!(m.attach_radio(Default::default(), 0), Err(KERR_ARGUMENT));
    assert!(m.rmtfs.is_none());
    let storage = || core::array::from_fn(|_| alloc::vec![1; 512]);
    assert_eq!(m.attach_radio(storage(), 0xfc20_1000), Ok(()));
    assert_eq!(m.outbox, queued);
    assert_eq!(m.hello_seen, [5]);
    assert_eq!(m.attach_radio(storage(), 0xfc20_1000), Err(KERR_IN_USE));
    m.handle(&mut f, &hello); // existing ADSP HELLO stays suppressed after attachment
    assert_eq!(m.outbox, queued);
    m.retry(&mut f, 1000);
    assert!(m.outbox.is_empty());
    assert!(f.nodes.iter().all(|&node| node == 5));
    assert!(m.services().any(|s| s.0 == rmtfs::SERVICE));
}

#[test]
fn failed_radio_cannot_block_queued_adsp_replies() {
    let (mut m, mut f) = (modem(), fake());
    f.refuse = 2;
    m.ctrl(&mut f, qrtr::TYPE_HELLO, (MODEM_NODE, PORT_CTRL), [0; 4]);
    m.ctrl(&mut f, qrtr::TYPE_HELLO, (5, PORT_CTRL), [0; 4]);
    m.lost();
    assert_eq!(m.outbox.len(), 1);
    assert_eq!(m.outbox[0].2.0, 5);
    m.retry(&mut f, 1000); // second refusal remains queued
    assert_eq!(m.outbox.len(), 1);
    m.retry(&mut f, 2000);
    assert!(m.outbox.is_empty());
    assert_eq!(f.nodes, [5]);
}

#[test]
fn refused_storage_and_locator_replies_keep_their_transactions() {
    for (port, id) in [(RMTFS_PORT, 4), (LOCATOR_PORT, 0x21)] {
        let (mut m, mut f) = (modem(), fake());
        let request = qmi::Builder::new(qmi::REQUEST, 7, id)
            .tlv(1, b"avs/audio")
            .finish();
        f.refuse = 1;
        m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 17, port, &request));
        assert!(f.sent.is_empty());
        assert_eq!(m.outbox.len(), 1);
        let queued = m.outbox[0].3.clone();
        m.retry(&mut f, 1000);
        m.retry(&mut f, 2000);
        assert_eq!(f.sent.len(), 1);
        let (header, actual) = qrtr::decode(&f.sent[0]).unwrap();
        assert_eq!((header.src_port, header.dst_port), (port, 17));
        assert_eq!(actual, queued);
        let reply = qmi::Msg::parse(actual).unwrap();
        assert_eq!((reply.ty, reply.txn, reply.id), (qmi::RESPONSE, 7, id));
        assert_eq!(reply.result(), Some(Ok(())));
    }
}

#[test]
fn start_says_hello_and_announces_three_services() {
    let (mut m, mut f) = (modem(), fake());
    m.start(&mut f);
    assert_eq!(f.sent.len(), 4);
    let (h, p) = qrtr::decode(&f.sent[1]).unwrap();
    assert_eq!(
        (h.ty, h.dst_node, h.dst_port),
        (qrtr::TYPE_NEW_SERVER, 0, PORT_CTRL)
    );
    assert_eq!(qrtr::ctrl_words(p), Some((4, [14, 1, 1, RMTFS_PORT])));
}

#[test]
fn the_adsp_gets_hello_services_and_the_locator() {
    let (mut m, mut f) = (modem(), fake());
    let adsp = |ty, src_port, dst_port, payload: &[u8]| {
        let h = Header {
            ty,
            src_node: 5,
            src_port,
            dst_node: NODE,
            dst_port,
            ..Default::default()
        };
        qrtr::encode(&h, payload)
    };
    m.handle(
        &mut f,
        &adsp(
            qrtr::TYPE_HELLO,
            PORT_CTRL,
            PORT_CTRL,
            &qrtr::ctrl(qrtr::TYPE_HELLO, [0; 4]),
        ),
    );
    assert!(f.sent.len() >= 4 && f.nodes.iter().all(|&n| n == 5));
    let (h, _) = qrtr::decode(&f.sent[0]).unwrap();
    assert_eq!((h.ty, h.dst_node), (qrtr::TYPE_HELLO, 5));
    let before = f.sent.len();
    m.handle(
        &mut f,
        &adsp(
            qrtr::TYPE_HELLO,
            PORT_CTRL,
            PORT_CTRL,
            &qrtr::ctrl(qrtr::TYPE_HELLO, [0; 4]),
        ),
    );
    assert_eq!(f.sent.len(), before); // one HELLO per node
    let q = qmi::Builder::new(qmi::REQUEST, 3, 0x21)
        .tlv(1, b"avs/audio")
        .finish();
    m.handle(&mut f, &adsp(qrtr::TYPE_DATA, 0x4001, LOCATOR_PORT, &q));
    assert_eq!(*f.nodes.last().unwrap(), 5);
    assert_eq!(reply(&f).result(), Some(Ok(())));
}

#[test]
fn subsystems_hear_each_other_is_up() {
    let (mut m, mut f) = (modem(), fake());
    let ssctl = ssctl_ns;
    m.handle(&mut f, &ssctl(0, ssctl::MODEM, 4));
    assert!(f.sent.is_empty()); // the ADSP isn't up yet
    m.handle(&mut f, &ssctl(5, ssctl::ADSP, 5));
    assert_eq!(f.sent.len(), 2);
    for (pkt, (node, port, name)) in f
        .sent
        .iter()
        .zip([(0, 4, &b"\x04adsp"[..]), (5, 5, &b"\x05modem"[..])])
    {
        let (h, p) = qrtr::decode(pkt).unwrap();
        assert_eq!(
            (h.dst_node, h.dst_port, h.src_port),
            (node, port, SSCTL_PORT)
        );
        let q = qmi::Msg::parse(p).unwrap();
        assert_eq!((q.ty, q.id), (qmi::REQUEST, ssctl::SUBSYS_EVENT_REQ));
        assert_eq!(q.tlv(0x01), Some(name));
        assert_eq!((q.u32(0x02), q.u32(0x10)), (Some(1), Some(0)));
    }
    assert_eq!(f.nodes, [0, 5]);
    // the answer is logged
    let txn = qmi::Msg::parse(qrtr::decode(&f.sent[1]).unwrap().1)
        .unwrap()
        .txn;
    let h = Header {
        ty: qrtr::TYPE_DATA,
        src_node: 5,
        src_port: 5,
        dst_node: NODE,
        dst_port: SSCTL_PORT,
        ..Default::default()
    };
    let r = ok(txn, ssctl::SUBSYS_EVENT_REQ).finish();
    m.handle(&mut f, &qrtr::encode(&h, &r));
    assert!(
        f.logs
            .iter()
            .any(|l| l.contains("adsp took \"modem up\" -> Ok"))
    );
}

#[test]
fn rmtfs_open_alloc_and_read() {
    let (mut m, mut f) = (modem(), fake());
    let open = qmi::Builder::new(qmi::REQUEST, 1, 1)
        .tlv(1, b"/boot/modem_fs1")
        .finish();
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 9, RMTFS_PORT, &open));
    assert_eq!(reply(&f).u32(0x10), Some(0));
    let alloc = qmi::Builder::new(qmi::REQUEST, 2, 4)
        .u32(1, 0)
        .u32(2, 0x20_0000)
        .finish();
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 17, RMTFS_PORT, &alloc));
    assert_eq!(reply(&f).tlv(0x10), Some(&0xfc20_1000u64.to_le_bytes()[..]));
    let mut iov = alloc::vec![1u8];
    for w in [1u32, 0xfc20_1200, 1] {
        iov.extend_from_slice(&w.to_le_bytes());
    }
    let read = qmi::Builder::new(qmi::REQUEST, 3, 3)
        .u32(1, 0)
        .u8(2, 0)
        .tlv(3, &iov)
        .u8(4, 0)
        .finish();
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 18, RMTFS_PORT, &read));
    assert_eq!(reply(&f).result(), Some(Ok(())));
    assert!(f.mem[0x200..0x400].iter().all(|&b| b == 1));
    assert_eq!(m.rmtfs.as_ref().unwrap().reads, 1);
}

#[test]
fn rmtfs_unknown_partition_is_refused() {
    let (mut m, mut f) = (modem(), fake());
    let open = qmi::Builder::new(qmi::REQUEST, 1, 1)
        .tlv(1, b"/boot/modem_fsg_oem_1")
        .finish();
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 14, RMTFS_PORT, &open));
    assert!(matches!(reply(&f).result(), Some(Err(_))));
}

#[test]
fn rmtfs_write_lands_in_ram_only() {
    let (mut m, mut f) = (modem(), fake());
    f.mem[0..512].fill(9);
    let mut iov = alloc::vec![1u8];
    for w in [3u32, 0xfc20_1000, 1] {
        iov.extend_from_slice(&w.to_le_bytes());
    }
    let w = qmi::Builder::new(qmi::REQUEST, 4, 3)
        .u32(1, 1)
        .u8(2, 1)
        .tlv(3, &iov)
        .u8(4, 0)
        .finish();
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 20, RMTFS_PORT, &w));
    assert!(
        m.rmtfs.as_ref().unwrap().images[1][3 * 512..4 * 512]
            .iter()
            .all(|&b| b == 9)
    );
    assert_eq!(m.rmtfs.as_ref().unwrap().writes, 1);
}

#[test]
fn confirmed_control_packets_get_resume_tx_too() {
    let (mut m, mut f) = (modem(), fake());
    let mut pkt = ns(15, 22);
    pkt[16] = 1;
    m.handle(&mut f, &pkt);
    let (h, p) = qrtr::decode(&f.sent[0]).unwrap();
    assert_eq!((h.ty, h.dst_port), (qrtr::TYPE_RESUME_TX, PORT_CTRL));
    assert_eq!(qrtr::ctrl_words(p), Some((7, [NODE, PORT_CTRL, 0, 0])));
    assert_eq!(m.servers.len(), 1);
}

#[test]
fn confirm_rx_gets_resume_tx() {
    let (mut m, mut f) = (modem(), fake());
    let q = qmi::Builder::new(qmi::REQUEST, 1, 0x24).finish();
    let mut pkt = from_modem(qrtr::TYPE_DATA, 30, LOCATOR_PORT, &q);
    pkt[16] = 1;
    m.handle(&mut f, &pkt);
    assert_eq!(f.sent.len(), 2); // RESUME_TX, then the locator's reply
    let (h, p) = qrtr::decode(&f.sent[0]).unwrap();
    assert_eq!((h.ty, h.dst_port), (qrtr::TYPE_RESUME_TX, 30));
    assert_eq!(qrtr::ctrl_words(p), Some((7, [NODE, LOCATOR_PORT, 0, 0])));
}

#[test]
fn a_refused_resume_tx_is_sent_again() {
    let (mut m, mut f) = (modem(), fake());
    let mut pkt = ns(15, 22);
    pkt[16] = 1;
    f.refuse = 1;
    m.handle(&mut f, &pkt);
    assert!(f.sent.is_empty());
    m.retry(&mut f, 500); // retried at most every second
    assert!(f.sent.is_empty());
    m.retry(&mut f, 1000);
    m.retry(&mut f, 2000);
    let resumes = f
        .sent
        .iter()
        .filter(|p| qrtr::decode(p).unwrap().0.ty == qrtr::TYPE_RESUME_TX)
        .count();
    assert_eq!(resumes, 1);
}

#[test]
fn refused_ssctl_events_go_out_on_tick_in_order() {
    let (mut m, mut f) = (modem(), fake());
    m.handle(&mut f, &ssctl_ns(0, ssctl::MODEM, 4));
    f.refuse = 1;
    m.handle(&mut f, &ssctl_ns(5, ssctl::ADSP, 5));
    assert!(f.sent.is_empty()); // the second waits behind the refused first
    assert!(f.logs.iter().any(|l| l.contains("ssctl: queued for")));
    assert!(!f.logs.iter().any(|l| l.contains("ssctl: told")));
    m.retry(&mut f, 1000);
    m.retry(&mut f, 2000);
    assert_eq!(f.nodes, [0, 5]);
}

#[test]
fn init_driver_is_retried_until_it_goes_out() {
    let (mut m, mut f) = (modem(), fake());
    m.ipa_up = true;
    f.refuse = 1;
    m.handle(
        &mut f,
        &from_modem(
            qrtr::TYPE_NEW_SERVER,
            PORT_CTRL,
            PORT_CTRL,
            &qrtr::ctrl(4, [ipa::SERVICE, qrtr::instance_word(1, 2), 0, 30]),
        ),
    );
    assert!(f.sent.iter().all(|p| req(p).2 != ipa::INIT_DRIVER));
    m.tick(&mut f, 2000);
    assert_eq!(req(f.sent.last().unwrap()).2, ipa::INIT_DRIVER);
    let n = f.sent.len();
    m.tick(&mut f, 4000);
    assert!(f.sent[n..].iter().all(|p| req(p).2 != ipa::INIT_DRIVER)); // once
}

#[test]
fn tftp_write_then_read_refused() {
    let (mut m, mut f) = (modem(), fake());
    let mut wrq = 2u16.to_be_bytes().to_vec();
    wrq.extend_from_slice(b"/readwrite/server_check.txt\0octet\0");
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 32, TFTP_PORT, &wrq));
    let (h, p) = qrtr::decode(f.sent.last().unwrap()).unwrap();
    assert_eq!((h.src_port, p), (FIRST_TRANSFER_PORT, &[0u8, 4, 0, 0][..]));
    let mut d = 3u16.to_be_bytes().to_vec();
    d.extend_from_slice(&1u16.to_be_bytes());
    d.extend_from_slice(b"hi");
    m.handle(
        &mut f,
        &from_modem(qrtr::TYPE_DATA, 32, FIRST_TRANSFER_PORT, &d),
    );
    assert_eq!(
        m.files,
        alloc::vec![(String::from("/readwrite/server_check.txt"), b"hi".to_vec())]
    );
    let mut rrq = 1u16.to_be_bytes().to_vec();
    rrq.extend_from_slice(b"/readwrite/modem_efs\0octet\0");
    m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 33, TFTP_PORT, &rrq));
    let (_, p) = qrtr::decode(f.sent.last().unwrap()).unwrap();
    assert_eq!(&p[..4], &[0, 5, 0, 1]);
}

#[test]
fn modem_lookup_of_our_rmtfs() {
    let (mut m, mut f) = (modem(), fake());
    m.handle(
        &mut f,
        &from_modem(
            qrtr::TYPE_NEW_LOOKUP,
            40,
            PORT_CTRL,
            &qrtr::ctrl(10, [14, 0, 0, 0]),
        ),
    );
    assert_eq!(f.sent.len(), 2);
    let (h, p) = qrtr::decode(&f.sent[0]).unwrap();
    assert_eq!((h.dst_node, h.dst_port), (0, 40));
    assert_eq!(qrtr::ctrl_words(p), Some((4, [14, 1, NODE, RMTFS_PORT])));
    assert_eq!(
        qrtr::ctrl_words(qrtr::decode(&f.sent[1]).unwrap().1),
        Some((4, [0; 4]))
    );
}

mod composed {
    use super::*;
    use modem::modem_ipc;
    use modem::voice;
    const DMS: u32 = 2;
    const VOICE: u32 = voice::SERVICE;
    const DMS_SET_OPERATING_MODE: u16 = 0x2e;
    use super::inprocess::Modem;
    fn modem() -> Modem {
        Modem::new(core::array::from_fn(|_| alloc::vec![1; 512]), 0xfc20_1000)
    }

    #[test]
    fn late_radio_packets_after_failure_cannot_recreate_head_of_line_blocking() {
        let (mut m, mut f) = (modem(), fake());
        m.handle(&mut f, &ns(DMS, 68));
        m.lost(&mut f);
        f.sent.clear();
        f.nodes.clear();
        f.refuse = 1;
        let mut late = ns(DMS, 68);
        late[16] = 1; // even its flow-control response must not requeue dead-radio traffic
        m.handle(&mut f, &late);
        assert!(m.router.servers.is_empty());
        assert!(m.router.outbox.is_empty());
        let h = Header {
            ty: qrtr::TYPE_HELLO,
            src_node: 5,
            src_port: PORT_CTRL,
            dst_node: NODE,
            dst_port: PORT_CTRL,
            ..Default::default()
        };
        m.handle(
            &mut f,
            &qrtr::encode(&h, &qrtr::ctrl(qrtr::TYPE_HELLO, [0; 4])),
        );
        assert_eq!(m.router.outbox.len(), 4); // HELLO and the still-attached support services
        m.tick(&mut f, 1000);
        assert!(m.router.outbox.is_empty());
        assert_eq!(f.nodes, [5, 5, 5, 5]);
        assert_eq!(m.status.modem, modem_ipc::modem::FAILED);
    }

    #[test]
    fn refused_online_transition_retries_the_same_transaction_once() {
        let (mut m, mut f) = (modem(), fake());
        m.handle(&mut f, &ns(DMS, 68));
        f.sent.clear();
        f.nodes.clear();
        m.status.sim = modem_ipc::sim::PRESENT;
        f.refuse = 1;
        m.tick(&mut f, 0);
        assert!(m.status.pending); // accepted for ordered delivery, not yet reported online
        assert!(f.sent.is_empty());
        let queued = m.router.outbox[0].3.clone();
        m.tick(&mut f, 0); // no duplicate transaction while queued
        assert_eq!(m.router.outbox.len(), 1);
        m.tick(&mut f, 500);
        assert!(f.sent.is_empty());
        m.tick(&mut f, 1000);
        m.tick(&mut f, 2000);
        assert_eq!(f.sent.len(), 1);
        let (_, actual) = qrtr::decode(&f.sent[0]).unwrap();
        assert_eq!(actual, queued);
        assert_eq!(qmi::Msg::parse(actual).unwrap().id, DMS_SET_OPERATING_MODE);
        assert_ne!(m.status.modem, modem_ipc::modem::ONLINE);
    }

    #[test]
    fn disabled_radio_does_not_send_boot_announcements_or_online_requests() {
        let (mut m, mut f) = (modem(), fake());
        m.router.radio_enabled = false;
        m.status.sim = modem_ipc::sim::PRESENT;
        m.start(&mut f);
        m.tick(&mut f, 0);
        m.tick(&mut f, 30000);
        assert!(f.sent.is_empty());
        assert!(!m.status.pending);
    }

    #[test]
    fn reset_retains_ram_efs_intent_and_healthy_adsp_ssctl() {
        let (mut m, mut f) = (modem(), fake());
        m.router.rmtfs.as_mut().unwrap().images[0][0] = 99;
        m.status.airplane = true;
        m.status.ring.silent = true;
        m.handle(&mut f, &ssctl_ns(2, ssctl::ADSP, 90));
        m.reset(&mut f, true);
        assert_eq!(m.router.rmtfs.as_ref().unwrap().images[0][0], 99);
        assert!(m.status.airplane && m.status.ring.silent);
        assert_eq!(m.status.generation, 1);
        f.sent.clear();
        m.handle(&mut f, &ssctl_ns(0, ssctl::MODEM, 91));
        assert_eq!(f.sent.len(), 2); // retained ADSP need not announce again
        assert_eq!(
            f.nodes.iter().rev().take(2).copied().collect::<Vec<_>>(),
            [2, 0]
        );
        m.reset(&mut f, false);
        assert!(m.router.servers.is_empty());
        assert!(m.router.ssctl.iter().all(Option::is_none));
    }

    #[test]
    fn lost_modem_keeps_adsp_traffic_and_reset_clears_the_failure() {
        let (mut m, mut f) = (modem(), fake());
        m.handle(&mut f, &ssctl_ns(5, ssctl::ADSP, 90));
        m.handle(&mut f, &ns(DMS, 68));
        m.router.hello_seen = alloc::vec![0, 5];
        m.router
            .outbox
            .push((qrtr::TYPE_DATA, 1, (5, 7), alloc::vec![1]));
        m.router
            .outbox
            .push((qrtr::TYPE_DATA, 1, (MODEM_NODE, 7), alloc::vec![2]));
        m.router.wlfw = Some(WifiFw {
            dst: (MODEM_NODE, 83),
            client: wlfw::Client::new(WLAN_MSA.0, WLAN_MSA.1),
            board: 0xff,
        });
        m.lost(&mut f);
        assert_eq!(m.status.modem, modem_ipc::modem::FAILED);
        assert!(m.router.radio_failed);
        assert!(m.router.servers.iter().all(|s| s.node == 5) && !m.router.servers.is_empty());
        assert_eq!(m.router.outbox.len(), 1);
        assert_eq!(m.router.outbox[0].2.0, 5);
        assert_eq!(m.router.hello_seen, [5]);
        assert!(
            m.router.wlfw.is_none(),
            "the Wi-Fi client goes with the modem"
        );
        assert!(m.router.rmtfs.is_some(), "RAM EFS survives");
        // a packet from the dead modem is ignored until a recovery resets
        m.handle(&mut f, &ns(VOICE, 77));
        assert!(!m.router.servers.iter().any(|s| s.node == MODEM_NODE));
        m.reset(&mut f, true);
        assert!(!m.router.radio_failed);
        m.handle(&mut f, &ns(VOICE, 77));
        assert!(m.router.servers.iter().any(|s| s.node == MODEM_NODE));
    }
}

#[test]
fn closed_session_unbinds_ports_and_removes_its_outbox_requests() {
    use hub_ipc::Bus;
    let (mut r, mut f) = (modem(), fake());
    r.bus(&mut f).bind(2, 0x20).unwrap();
    f.refuse = 1;
    assert!(!r.bus(&mut f).send(0x20, (0, 68), b"old session", true));
    r.unbind(0x20);
    r.retry(&mut f, 1000);
    assert!(f.sent.is_empty());
    assert!(!r.bus(&mut f).send(0x20, (0, 68), b"closed", true));
    r.bus(&mut f).bind(11, 0x20).unwrap();
    assert!(r.bus(&mut f).send(0x20, (0, 55), b"new session", true));
}

#[test]
fn acknowledged_recovery_reopens_without_a_second_reset_generation() {
    let mut r = modem();
    r.lost();
    let generation = r.generation();
    assert!(matches!(r.next_event(), Some(Event::Reset { generation: g, .. }) if g == generation));
    while r.next_event().is_some() {}
    assert!(r.state().radio_failed);
    r.restart_quiesced();
    assert_eq!(r.generation(), generation);
    assert!(!r.state().radio_failed);
    while let Some(event) = r.next_event() {
        assert!(!matches!(event, Event::Reset { .. }));
    }
}
