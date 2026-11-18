// SPDX-License-Identifier: Apache-2.0
//! Node-1 QRTR router and firmware support services.
#![cfg_attr(not(test), no_std)]
extern crate alloc;
pub use ::qrtr::{qmi, wire as qrtr};
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
pub use hub_ipc::{Event, Server, State, Subsystem};
use qrtr::{Header, NODE_BCAST, PORT_CTRL};
pub use wlfw;
pub mod ipa;
pub mod locator;
pub mod recovery;
pub mod rmtfs;
pub mod schedule;
pub mod ssctl;
pub mod tftp;
/// What the router needs from the system.
pub trait Platform {
    /// One whole QRTR packet to `node`: the modem (node 0) or the ADSP, each on its own GLINK IPCRTR.
    fn send(&mut self, node: u32, packet: &[u8]) -> Result<(), i32>;
    /// The rmtfs carve-out the modem reads its EFS from.
    fn rmtfs_read(&mut self, offset: u32, buf: &mut [u8]) -> Result<(), i32>;
    fn rmtfs_write(&mut self, offset: u32, data: &[u8]) -> Result<(), i32>;
    fn log(&mut self, args: fmt::Arguments);
    /// The size of a read-only file on the modem's firmware partition (modem_a, "IMAGE/WLANMDSP.MBN"), for TFTP.
    fn fw_size(&mut self, _path: &str) -> Result<usize, i32> {
        Err(-5) // KErrNotSupported
    }
    /// Its bytes at `offset` into `buf`: how many (fewer at its end).
    fn fw_read(&mut self, _path: &str, _offset: usize, _buf: &mut [u8]) -> Result<usize, i32> {
        Err(-5)
    }
    /// Hand the Wi-Fi firmware's MSA regions to its VMIDs (TrustZone; kernel HAL WlanMsa). Wi-Fi W1.
    fn wlan_msa(&mut self, _regions: &[wlfw::Region]) -> Result<(), i32> {
        Err(-5)
    }
    /// The WCN3990 board data file for `board_id` (vendor_a), or empty if there is none.
    fn board_data(&mut self, _board_id: u32) -> Vec<u8> {
        Vec::new()
    }
    /// The firmware is in mission mode: start the host's copy engines, HTC and WMI (kernel HAL WlanStart, W2).
    fn wlan_start(&mut self) -> Result<(), i32> {
        Err(-5)
    }
    /// The wall clock, UTC Unix milliseconds, for the call log; 0 = unknown.
    fn utc_ms(&mut self) -> i64 {
        0
    }
}

pub const NODE: u32 = 1;
const MODEM_NODE: u32 = 0;
const RMTFS_PORT: u32 = 0x10;
const LOCATOR_PORT: u32 = 0x11;
const TFTP_PORT: u32 = 0x12;
const IPA_PORT: u32 = 0x13;
/// our SSCTL client port (the "subsystem up" events, ssctl.rs)
const SSCTL_PORT: u32 = 0x25;
/// our WLFW client port (the Wi-Fi firmware's control service, wlfw.rs)
const WLFW_PORT: u32 = 0x26;
/// DTB wlan-msa@94d00000: the MSA carve-out the Wi-Fi firmware may use
const WLAN_MSA: (u64, u32) = (0x94d0_0000, 0x10_0000);
const FIRST_TRANSFER_PORT: u32 = 0x100;
/// (service, version, instance, our port); the IPA server is announced only once the kernel runs IPA
const SERVICES: [(u32, u32, u32, u32); 4] = [
    (rmtfs::SERVICE, rmtfs::VERSION, rmtfs::INSTANCE, RMTFS_PORT),
    (
        locator::SERVICE,
        locator::VERSION,
        locator::INSTANCE,
        LOCATOR_PORT,
    ),
    (tftp::SERVICE, tftp::VERSION, tftp::INSTANCE, TFTP_PORT),
    (ipa::SERVICE, ipa::VERSION, ipa::INSTANCE, IPA_PORT),
];
const IPA: u32 = ipa::SERVICE;
pub const KERR_NOT_READY: i32 = -18;
pub const KERR_ARGUMENT: i32 = -6;
pub const KERR_IN_USE: i32 = -14;
/// One QMI client: its server, our port, the transaction counter and the requests still unanswered (txn, id).
struct Client {
    dst: (u32, u32),
    port: u32,
    txn: u16,
    pending: Vec<(u16, u16)>,
}

struct WifiFw {
    dst: (u32, u32),
    client: wlfw::Client,
    board: u32,
}

pub struct Router {
    /// Log every QRTR packet both ways (bring-up).
    pub trace: bool,
    rmtfs: Option<rmtfs::Rmtfs>,
    /// Startup policy: false prevents automatic online requests (not a runtime power-off command).
    pub radio_enabled: bool,
    /// Terminal radio failure; late packets cannot recreate clients without a reboot.
    radio_failed: bool,
    pub servers: Vec<Server>,
    pub files: Vec<(String, Vec<u8>)>,
    /// The kernel runs IPA (MODEMD's HAL IpaStart succeeded): then the IPA handshake runs and LTE stays allowed.
    /// Without it, voice-only: 2G/3G circuit-switched (NAS system selection, until power cycle), because on LTE data
    /// the modem asserts in its IPA path ("IPA Assert: new_free_space > 0", C5).
    pub ipa_up: bool,
    ipa: ipa::Handshake,
    /// INIT_DRIVER couldn't go out yet: tick retries it (last tried at ipa_init_at)
    ipa_init_due: bool,
    ipa_init_at: u64,
    transfers: Vec<tftp::Transfer>,
    /// firmware reads in progress (the modem's WLAN domain image)
    readers: Vec<tftp::Reader>,
    /// the Wi-Fi firmware's control service once it appears: its address and our client (W1)
    wlfw: Option<WifiFw>,
    next_port: u32,
    /// the nodes whose HELLO we answered (the modem, the ADSP)
    hello_seen: Vec<u32>,
    /// the SSCTL servers (node, port): the modem's, the ADSP's
    ssctl: [Option<(u32, u32)>; 2],
    ssctl_txn: u16,
    /// essential packets the transport refused, sent again by `tick` in order
    outbox: Vec<Unsent>,
    outbox_at: u64,
    ipa_client: Option<Client>,
    bindings: Vec<(u32, u32)>,
    events: Vec<Event>,
    generation: u32,
}
/// (type, our port, destination, payload)
type Unsent = (u32, u32, (u32, u32), Vec<u8>);
/// essential packets kept for a retry at most; an older one gives way (logged)
const OUTBOX: usize = 16;
/// how often `tick` retries them (a refused send can take ~200 ms in the kernel)
const OUTBOX_RETRY_MS: u64 = 1000;

impl Router {
    pub fn new(images: [Vec<u8>; 4], rmtfs_phys: u64) -> Self {
        Self::with_rmtfs(Some(rmtfs::Rmtfs::new(images, rmtfs_phys)))
    }

    /// ADSP support before radio partitions and carve-out are available. No RMTFS is advertised or served.
    pub fn support_only() -> Self {
        Self::with_rmtfs(None)
    }

    /// The radio storage is attached (a recovery can boot the modem again without reading the partitions).
    pub fn radio_attached(&self) -> bool {
        self.rmtfs.is_some()
    }

    /// Attach real radio storage without resetting ADSP discovery, transactions or queued replies.
    pub fn attach_radio(&mut self, images: [Vec<u8>; 4], phys: u64) -> Result<(), i32> {
        if self.rmtfs.is_some() {
            return Err(KERR_IN_USE);
        }
        if phys == 0
            || phys % 512 != 0
            || phys.checked_add(0x20_0000).is_none()
            || images.iter().any(Vec::is_empty)
        {
            return Err(KERR_ARGUMENT);
        }
        self.rmtfs = Some(rmtfs::Rmtfs::new(images, phys));
        Ok(())
    }
    fn with_rmtfs(rmtfs: Option<rmtfs::Rmtfs>) -> Self {
        Self {
            trace: false,
            rmtfs,
            radio_enabled: true,
            radio_failed: false,
            servers: Vec::new(),
            files: Vec::new(),
            ipa_up: false,
            ipa: ipa::Handshake::default(),
            ipa_init_due: false,
            ipa_init_at: 0,
            transfers: Vec::new(),
            readers: Vec::new(),
            wlfw: None,
            next_port: FIRST_TRANSFER_PORT,
            hello_seen: Vec::new(),
            ssctl: [None; 2],
            ssctl_txn: 0,
            outbox: Vec::new(),
            outbox_at: 0,
            ipa_client: None,
            bindings: Vec::new(),
            events: Vec::new(),
            generation: 0,
        }
    }

    /// One packet out; false if the transport refused it (logged).
    fn send(
        &mut self,
        p: &mut dyn Platform,
        ty: u32,
        src_port: u32,
        dst: (u32, u32),
        payload: &[u8],
    ) -> bool {
        let h = Header {
            ty,
            src_node: NODE,
            src_port,
            confirm_rx: 0,
            size: 0,
            dst_node: dst.0,
            dst_port: dst.1,
        };
        if self.trace {
            trace(p, ">", &h, payload);
        }
        if let Err(e) = p.send(dst.0, &qrtr::encode(&h, payload)) {
            p.log(format_args!(
                "qrtr: send type {ty} to {}:{} failed ({e})",
                dst.0, dst.1 as i32
            ));
            return false;
        }
        true
    }

    /// A packet that must arrive (flow control, control, SSCTL, the subscriptions): refused, or behind others that
    /// were, it waits in the outbox and `tick` sends it in order. True if it went now. One lost RESUME_TX stalls
    /// the sender's flow for good (2026-11-09 review F07).
    fn send_must(
        &mut self,
        p: &mut dyn Platform,
        ty: u32,
        src_port: u32,
        dst: (u32, u32),
        payload: &[u8],
    ) -> bool {
        if self.outbox.is_empty() && self.send(p, ty, src_port, dst, payload) {
            return true;
        }
        if self.outbox.len() >= OUTBOX {
            let (ty, ..) = self.outbox.remove(0);
            p.log(format_args!("qrtr: outbox full: type {ty} dropped"));
        }
        self.outbox.push((ty, src_port, dst, payload.to_vec()));
        false
    }

    /// The outbox, oldest first, until the transport refuses one.
    fn flush(&mut self, p: &mut dyn Platform) {
        while let Some((ty, port, dst, data)) = self.outbox.first().cloned() {
            if !self.send(p, ty, port, dst, &data) {
                return;
            }
            self.outbox.remove(0);
        }
    }

    fn ctrl(&mut self, p: &mut dyn Platform, ty: u32, dst: (u32, u32), words: [u32; 4]) {
        self.send_must(p, ty, PORT_CTRL, dst, &qrtr::ctrl(ty, words));
    }

    /// Our servers: IPA's only while the kernel runs IPA.
    fn services(&self) -> impl Iterator<Item = (u32, u32, u32, u32)> {
        let ipa_up = self.ipa_up;
        let radio = self.rmtfs.is_some();
        SERVICES
            .into_iter()
            .filter(move |s| (radio || s.0 != rmtfs::SERVICE) && (ipa_up && radio || s.0 != IPA))
    }

    fn announce(&mut self, p: &mut dyn Platform, dst: (u32, u32)) {
        let services: Vec<_> = self.services().collect();
        for (service, version, instance, port) in services {
            self.ctrl(
                p,
                qrtr::TYPE_NEW_SERVER,
                dst,
                [service, qrtr::instance_word(version, instance), NODE, port],
            );
        }
    }

    /// Say HELLO to the modem's router and announce our services.
    pub fn start(&mut self, p: &mut dyn Platform) {
        if self.rmtfs.is_none() || !self.radio_enabled || self.radio_failed {
            return;
        }
        self.events.push(Event::RadioStarted);
        self.ctrl(p, qrtr::TYPE_HELLO, (MODEM_NODE, PORT_CTRL), [0; 4]);
        self.announce(p, (MODEM_NODE, PORT_CTRL));
    }

    /// A processor's SSCTL server appeared. Once both are known, tell each that the other is up, in Linux's order
    /// (sysmon: the running modem hears of the ADSP, then the ADSP gets the replay of the modem). An ADSP restart
    /// re-announces its server and so sends both again.
    fn ssctl_up(&mut self, p: &mut dyn Platform, s: Server) {
        let k = match (s.instance & 0xff, s.instance >> 8) {
            (ssctl::VERSION, ssctl::MODEM) => 0,
            (ssctl::VERSION, ssctl::ADSP) => 1,
            _ => return,
        };
        self.ssctl[k] = Some((s.node, s.port));
        let [Some(modem), Some(adsp)] = self.ssctl else {
            return;
        };
        let mut now = true;
        for (dst, name) in [(modem, "adsp"), (adsp, "modem")] {
            self.ssctl_txn = self.ssctl_txn.wrapping_add(1).max(1);
            let mut b = qmi::Builder::new(qmi::REQUEST, self.ssctl_txn, ssctl::SUBSYS_EVENT_REQ);
            for (t, v) in ssctl::event_tlvs(name) {
                b = b.tlv(t, &v);
            }
            now &= self.send_must(p, qrtr::TYPE_DATA, SSCTL_PORT, dst, &b.finish());
        }
        p.log(format_args!(
            "ssctl: {} the modem \"adsp up\" and the adsp \"modem up\"",
            if now { "told" } else { "queued for" }
        ));
    }

    /// INIT_DRIVER to the modem's IPA server: true once sent.
    fn init_driver(&mut self, p: &mut dyn Platform) -> bool {
        let tlvs = ipa::init_driver_tlvs();
        let refs: Vec<(u8, &[u8])> = tlvs.iter().map(|(t, v)| (*t, v.as_slice())).collect();
        let sent = self.ipa_request(p, ipa::INIT_DRIVER, &refs);
        p.log(format_args!(
            "ipa: INIT_DRIVER {}",
            if sent { "sent" } else { "not sent yet" }
        ));
        sent
    }

    /// IPA's INIT_COMPLETE indication, from our server to the modem's IPA server, once the handshake is in.
    fn ipa_complete(&mut self, p: &mut dyn Platform) {
        if !self.ipa.complete_due() {
            return;
        }
        let Some(dst) = self.ipa_client.as_ref().map(|c| c.dst) else {
            return;
        };
        let ind = qmi::Builder::new(qmi::INDICATION, 1, ipa::INIT_COMPLETE)
            .result(0)
            .finish();
        self.send_must(p, qrtr::TYPE_DATA, IPA_PORT, dst, &ind);
        p.log(format_args!("ipa: handshake complete"));
    }

    /// One QRTR packet from the modem.
    pub fn handle(&mut self, p: &mut dyn Platform, packet: &[u8]) {
        let Some((h, payload)) = qrtr::decode(packet) else {
            p.log(format_args!(
                "qrtr: not a v1 packet ({} bytes)",
                packet.len()
            ));
            return;
        };
        if self.trace {
            trace(p, "<", &h, payload);
        }
        if h.dst_node != NODE && h.dst_node != NODE_BCAST {
            return;
        }
        if h.src_node == MODEM_NODE
            && (self.radio_failed || !self.radio_enabled || self.rmtfs.is_none())
        {
            return; // no SSR yet: late packets must not resurrect a terminally failed radio
        }
        let from = (h.src_node, h.src_port);
        // flow control: a sender marks a packet confirm_rx and waits for RESUME_TX before it sends that port more.
        // Every type, control included: unconfirmed NEW_SERVERs stalled the modem's announcements, and DMS or UIM
        // never arrived (C5, 2026-11-09). Linux confirms whatever a socket receives.
        if h.confirm_rx != 0 && h.ty != qrtr::TYPE_RESUME_TX {
            self.send_must(
                p,
                qrtr::TYPE_RESUME_TX,
                h.dst_port,
                from,
                &qrtr::ctrl(qrtr::TYPE_RESUME_TX, [NODE, h.dst_port, 0, 0]),
            );
        }
        match h.ty {
            qrtr::TYPE_DATA => self.data(p, &h, payload),
            qrtr::TYPE_HELLO if !self.hello_seen.contains(&h.src_node) => {
                self.hello_seen.push(h.src_node);
                self.ctrl(p, qrtr::TYPE_HELLO, (h.src_node, PORT_CTRL), [0; 4]);
                self.announce(p, (h.src_node, PORT_CTRL));
            }
            qrtr::TYPE_NEW_SERVER | qrtr::TYPE_DEL_SERVER => {
                let Some((_, [service, instance, node, port])) = qrtr::ctrl_words(payload) else {
                    return;
                };
                let s = Server {
                    service,
                    instance,
                    node,
                    port,
                };
                self.servers.retain(|x| *x != s);
                if h.ty == qrtr::TYPE_DEL_SERVER || (service == 0 && port == 0) {
                    self.events.push(Event::ServerDown(s));
                    return;
                }
                self.servers.push(s);
                if service == ssctl::SERVICE {
                    self.ssctl_up(p, s);
                }
                p.log(format_args!(
                    "qrtr: server {service} v{} i{} at {node}:{port}",
                    instance & 0xff,
                    instance >> 8
                ));
                if service == IPA {
                    self.ipa_client_up(p, s);
                } else if self.bindings.iter().any(|b| b.0 == service) {
                    self.events.push(Event::ServerUp(s));
                }
                if service == wlfw::SERVICE && node == MODEM_NODE {
                    let mut c = wlfw::Client::new(WLAN_MSA.0, WLAN_MSA.1);
                    let steps = c.start();
                    let board = self.wlfw.as_ref().map_or(0xff, |wifi| wifi.board);
                    self.wlfw = Some(WifiFw {
                        dst: (node, port),
                        client: c,
                        board,
                    });
                    p.log(format_args!(
                        "wlfw: the Wi-Fi firmware's service is up; starting"
                    ));
                    self.wlfw_steps(p, steps);
                }
            }
            qrtr::TYPE_NEW_LOOKUP => {
                // the remote looks up our services: matching NEW_SERVERs, then an empty one ends the list
                let Some((_, [service, instance, _, _])) = qrtr::ctrl_words(payload) else {
                    return;
                };
                // what the remote expects to find here (Wi-Fi W0: services the modem waits for)
                p.log(format_args!(
                    "qrtr: lookup {service} i{} from {}:{}",
                    instance >> 8,
                    h.src_node,
                    h.src_port
                ));
                let services: Vec<_> = self.services().collect();
                for (s, v, i, port) in services {
                    if (service == 0 || service == s)
                        && (instance == 0 || instance == qrtr::instance_word(v, i))
                    {
                        self.ctrl(
                            p,
                            qrtr::TYPE_NEW_SERVER,
                            from,
                            [s, qrtr::instance_word(v, i), NODE, port],
                        );
                    }
                }
                self.ctrl(p, qrtr::TYPE_NEW_SERVER, from, [0; 4]);
            }
            _ => {} // BYE, DEL_CLIENT, RESUME_TX: nothing to undo yet
        }
    }

    fn data(&mut self, p: &mut dyn Platform, h: &Header, payload: &[u8]) {
        let from = (h.src_node, h.src_port);
        match h.dst_port {
            IPA_PORT if self.ipa_up => {
                let Some(m) = qmi::Msg::parse(payload) else {
                    return;
                };
                if m.ty != qmi::REQUEST {
                    return;
                }
                let resp = self.ipa.request(&m);
                p.log(format_args!(
                    "ipa: request {:#x} from the modem{}",
                    m.id,
                    if resp.is_some() {
                        ""
                    } else {
                        " (no handler: unanswered, as Linux)"
                    }
                ));
                if let Some(resp) = resp {
                    self.send_must(p, qrtr::TYPE_DATA, IPA_PORT, from, &resp);
                }
                self.ipa_complete(p);
            }
            RMTFS_PORT | LOCATOR_PORT => {
                let Some(m) = qmi::Msg::parse(payload) else {
                    return;
                };
                let resp = if h.dst_port == RMTFS_PORT {
                    let Some(rmtfs) = &mut self.rmtfs else {
                        return;
                    };
                    rmtfs.handle(p, &m)
                } else {
                    locator::handle(&m)
                };
                if h.dst_port == LOCATOR_PORT {
                    let name = m.tlv(1).unwrap_or(&[]);
                    p.log(format_args!(
                        "locator: {:?}",
                        core::str::from_utf8(name).unwrap_or("?")
                    ));
                }
                self.send_must(p, qrtr::TYPE_DATA, h.dst_port, from, &resp);
            }
            SSCTL_PORT => {
                let Some(m) = qmi::Msg::parse(payload).filter(|m| m.ty == qmi::RESPONSE) else {
                    return; // the answers only (the processors send other messages on this port too)
                };
                let (who, what) = if h.src_node == MODEM_NODE {
                    ("modem", "adsp")
                } else {
                    ("adsp", "modem")
                };
                p.log(format_args!(
                    "ssctl: the {who} took \"{what} up\" -> {:?}",
                    m.result().unwrap_or(Err(u16::MAX))
                ));
            }
            TFTP_PORT => match tftp::request(payload, from, self.next_port) {
                tftp::Action::Reply(r) => {
                    p.log(format_args!("tftp: refused {:?}", tftp_name(payload)));
                    self.send_must(p, qrtr::TYPE_DATA, TFTP_PORT, from, &r);
                }
                tftp::Action::Start(t, first) => {
                    p.log(format_args!("tftp: write {:?}", t.name));
                    self.next_port += 1;
                    let port = t.port;
                    self.transfers.push(t);
                    self.send_must(p, qrtr::TYPE_DATA, port, from, &first);
                }
                tftp::Action::Read(q) => match p.fw_size(&q.path) {
                    Ok(size) => {
                        p.log(format_args!(
                            "tftp: read {:?} ({size} bytes), blksize {} wsize {} rsize {} seek {:?}",
                            tftp_name(payload),
                            q.blksize,
                            q.wsize,
                            q.rsize,
                            q.seek
                        ));
                        let port = self.next_port;
                        self.next_port += 1;
                        let (r, first) = tftp::start_read(q, from, port, size, &mut |f, at, b| {
                            p.fw_read(f, at, b)
                        });
                        if self.readers.len() >= 8 {
                            self.readers.remove(0); // abandoned reads: keep the newest
                        }
                        self.readers.push(r);
                        self.send_must(p, qrtr::TYPE_DATA, port, from, &first);
                    }
                    Err(e) => {
                        p.log(format_args!("tftp: refused {:?} ({e})", tftp_name(payload)));
                        let r = tftp::error(1, "File not found");
                        self.send_must(p, qrtr::TYPE_DATA, TFTP_PORT, from, &r);
                    }
                },
                tftp::Action::Ignore => {}
            },
            WLFW_PORT => {
                let steps = match &mut self.wlfw {
                    Some(wifi) if wifi.dst == from => wifi.client.message(payload),
                    _ => return,
                };
                self.wlfw_steps(p, steps);
            }
            port if self
                .readers
                .iter()
                .any(|r| r.port == port && r.peer == from) =>
            {
                let i = self
                    .readers
                    .iter()
                    .position(|r| r.port == port)
                    .unwrap_or(0);
                let (out, done) = tftp::reader(&self.readers[i], payload, &mut |f, at, b| {
                    p.fw_read(f, at, b)
                });
                for d in &out {
                    self.send_must(p, qrtr::TYPE_DATA, port, from, d);
                }
                if done {
                    let r = self.readers.remove(i);
                    p.log(format_args!("tftp: read of {} done", r.path));
                }
            }
            0x24 if self.ipa_client.as_ref().is_some_and(|c| c.dst == from) => {
                self.ipa_response(p, payload);
            }
            port if self.bindings.iter().any(|b| b.1 == port) => {
                self.events.push(Event::Packet {
                    src: from,
                    dst_port: port,
                    payload: payload.to_vec(),
                });
            }
            port => {
                let Some(i) = self.transfers.iter().position(|t| t.port == port) else {
                    p.log(format_args!(
                        "qrtr: {} bytes for unserved port {port} from {}:{}, dropped",
                        payload.len(),
                        from.0,
                        from.1
                    ));
                    return;
                };
                let (reply, done) = tftp::transfer(&mut self.transfers[i], payload);
                if let Some(r) = reply {
                    self.send_must(p, qrtr::TYPE_DATA, port, from, &r);
                }
                if done {
                    let t = self.transfers.remove(i);
                    p.log(format_args!(
                        "tftp: {:?} {} bytes kept in RAM",
                        t.name,
                        t.data.len()
                    ));
                    self.files.retain(|f| f.0 != t.name);
                    self.files.push((t.name, t.data));
                }
            }
        }
    }

    /// Carry out what the WLFW client asks for (wlfw.rs): sends, the MSA assignment, the board data
    fn wlfw_steps(&mut self, p: &mut dyn Platform, mut steps: Vec<wlfw::Step>) {
        while !steps.is_empty() {
            let mut next = Vec::new();
            for s in steps {
                let Some(wifi) = &self.wlfw else {
                    return;
                };
                let dst = wifi.dst;
                match s {
                    wlfw::Step::Send(m) => {
                        let id = qmi::Msg::parse(&m).map_or(0, |q| q.id);
                        p.log(format_args!("wlfw: request {id:#x}"));
                        self.send_must(p, qrtr::TYPE_DATA, WLFW_PORT, dst, &m);
                    }
                    wlfw::Step::AssignMsa(regions) => {
                        for r in &regions {
                            p.log(format_args!(
                                "wlfw: MSA region {:#x} + {:#x}{}",
                                r.addr,
                                r.size,
                                if r.secure { " (secure)" } else { "" }
                            ));
                        }
                        let r = p.wlan_msa(&regions);
                        p.log(format_args!("wlfw: MSA assigned -> {r:?}"));
                        if let Some(wifi) = &mut self.wlfw {
                            next.extend(wifi.client.msa_assigned(r.is_ok()));
                        }
                    }
                    wlfw::Step::LoadBoardData => {
                        let data = p.board_data(wifi.board);
                        p.log(format_args!("wlfw: board data {} bytes", data.len()));
                        if let Some(wifi) = &mut self.wlfw {
                            next.extend(wifi.client.board_data(data));
                        }
                    }
                    wlfw::Step::Caps(k) => {
                        if let Some(wifi) = &mut self.wlfw {
                            wifi.board = k.board_id;
                        }
                        p.log(format_args!(
                            "wlfw: chip {:#x} family {:#x} board {:#x} soc {:#x} fw {:#x} {} {}",
                            k.chip_id,
                            k.chip_family,
                            k.board_id,
                            k.soc_id,
                            k.fw_version,
                            k.fw_timestamp,
                            k.fw_build_id
                        ));
                    }
                    wlfw::Step::Ready => p.log(format_args!("wlfw: firmware ready (W1 done)")),
                    wlfw::Step::Enabled => {
                        let r = p.wlan_start();
                        p.log(format_args!(
                            "wlfw: mission mode; host copy engines start -> {r:?}"
                        ));
                    }
                    wlfw::Step::Failed(what, e) => p.log(format_args!("wlfw: {what} failed ({e})")),
                }
            }
            steps = next;
        }
    }

    /// Rebuild volatile state after a confirmed kernel stop. RAM EFS and user intent survive.
    /// `keep_adsp`: the ADSP kept running, so its servers, queued replies, HELLO and TFTP traffic stay. The radio
    /// storage (RAM EFS), the startup policy, the trace switch and the port counter always do; the Wi-Fi firmware's
    /// client goes with the modem (it lives on the modem node) and starts again when its service reappears.
    pub fn reset(&mut self, keep_adsp: bool) {
        let rmtfs = self
            .rmtfs
            .take()
            .map(|mut r| rmtfs::Rmtfs::new(core::mem::take(&mut r.images), r.phys));
        let ipa_up = self.ipa_up;
        let not_modem = |node: u32| keep_adsp && node != MODEM_NODE;
        let retained: Vec<Server> = self
            .servers
            .iter()
            .copied()
            .filter(|s| not_modem(s.node))
            .collect();
        let mut outbox = core::mem::take(&mut self.outbox);
        outbox.retain(|(_, _, dst, _)| not_modem(dst.0));
        let mut hello = core::mem::take(&mut self.hello_seen);
        hello.retain(|&n| not_modem(n));
        let mut transfers = core::mem::take(&mut self.transfers);
        transfers.retain(|t| not_modem(t.peer.0));
        let mut readers = core::mem::take(&mut self.readers);
        readers.retain(|r| not_modem(r.peer.0));
        let (trace, radio_enabled, next_port) = (self.trace, self.radio_enabled, self.next_port);
        let bindings = core::mem::take(&mut self.bindings);
        let generation = self.generation.wrapping_add(1);
        *self = Self::with_rmtfs(rmtfs);
        self.bindings = bindings;
        self.generation = generation;
        self.events.push(Event::Reset {
            subsystem: if keep_adsp {
                Subsystem::Mss
            } else {
                Subsystem::All
            },
            generation,
        });
        self.trace = trace;
        self.radio_enabled = radio_enabled;
        self.next_port = next_port;
        self.outbox = outbox;
        self.hello_seen = hello;
        self.transfers = transfers;
        self.readers = readers;
        self.ipa_up = ipa_up;
        self.servers = retained;
        for s in &self.servers {
            if s.service == ssctl::SERVICE
                && s.instance == qrtr::instance_word(ssctl::VERSION, ssctl::ADSP)
            {
                self.ssctl[1] = Some((s.node, s.port));
            }
        }
        self.events.push(Event::State(self.state()));
    }
    pub fn lost(&mut self) {
        self.reset(true);
        self.radio_failed = true;
        self.events.push(Event::State(self.state()));
    }
    pub fn state(&self) -> State {
        State {
            radio_attached: self.radio_attached(),
            radio_enabled: self.radio_enabled,
            radio_failed: self.radio_failed,
            ipa_up: self.ipa_up,
        }
    }
    pub fn next_event(&mut self) -> Option<Event> {
        if self.events.is_empty() {
            None
        } else {
            Some(self.events.remove(0))
        }
    }
    pub fn bus<'a>(&'a mut self, platform: &'a mut dyn Platform) -> Connection<'a> {
        Connection {
            router: self,
            platform,
        }
    }
    /// Run between cellular intent and command timers to preserve the shared retry order.
    pub fn retry(&mut self, p: &mut dyn Platform, now: u64) {
        if !self.outbox.is_empty() && now.saturating_sub(self.outbox_at) >= OUTBOX_RETRY_MS {
            self.outbox_at = now;
            self.flush(p);
        }
    }
    /// Run after cellular SMS timers, before NAS polls (C5/C6 ordering).
    pub fn tick(&mut self, p: &mut dyn Platform, now: u64) {
        if self.ipa_init_due && now.saturating_sub(self.ipa_init_at) >= 2000 {
            self.ipa_init_at = now;
            self.ipa_init_due = !self.init_driver(p);
        }
    }
    fn ipa_client_up(&mut self, p: &mut dyn Platform, s: Server) {
        if s.node == MODEM_NODE
            && (self.rmtfs.is_none() || !self.radio_enabled || self.radio_failed)
        {
            return;
        }
        match &mut self.ipa_client {
            Some(c) => c.dst = (s.node, s.port),
            None => {
                self.ipa_client = Some(Client {
                    dst: (s.node, s.port),
                    port: 0x24,
                    txn: 0,
                    pending: Vec::new(),
                })
            }
        }
        if self.ipa_up {
            self.ipa_init_due = !self.init_driver(p);
        }
    }
    fn ipa_request(&mut self, p: &mut dyn Platform, id: u16, tlvs: &[(u8, &[u8])]) -> bool {
        let Some(c) = &mut self.ipa_client else {
            return false;
        };
        c.txn = c.txn.wrapping_add(1).max(1);
        let mut b = qmi::Builder::new(qmi::REQUEST, c.txn, id);
        for (t, v) in tlvs {
            b = b.tlv(*t, v);
        }
        if c.pending.len() >= 16 {
            c.pending.remove(0);
        }
        c.pending.push((c.txn, id));
        let (port, dst) = (c.port, c.dst);
        self.send(p, qrtr::TYPE_DATA, port, dst, &b.finish())
    }
    fn ipa_response(&mut self, p: &mut dyn Platform, payload: &[u8]) {
        let Some(m) = qmi::Msg::parse(payload).filter(|m| m.ty == qmi::RESPONSE) else {
            return;
        };
        let Some(c) = &mut self.ipa_client else {
            return;
        };
        let Some(k) = c.pending.iter().position(|&r| r == (m.txn, m.id)) else {
            p.log(format_args!(
                "qmi: service {} response {:#x} txn {} not ours",
                IPA, m.id, m.txn
            ));
            return;
        };
        c.pending.remove(k);
        self.events.push(Event::RadioResponse);
        if m.result() != Some(Ok(())) {
            p.log(format_args!(
                "qmi: service {} request {:#x} failed {:?}",
                IPA,
                m.id,
                m.result()
            ));
        } else if m.id == ipa::INIT_DRIVER {
            p.log(format_args!("ipa: the modem took INIT_DRIVER"));
            self.ipa.modem_ready = true;
            self.ipa_complete(p);
        }
    }
}

pub struct Connection<'a> {
    router: &'a mut Router,
    platform: &'a mut dyn Platform,
}
impl hub_ipc::Bus for Connection<'_> {
    fn bind(&mut self, service: u32, port: u32) -> Result<(), i32> {
        if let Some(b) = self.router.bindings.iter().find(|b| b.1 == port) {
            return if b.0 == service {
                Ok(())
            } else {
                Err(KERR_IN_USE)
            };
        }
        if !(0x20..=0x27).contains(&port) || matches!(port, 0x24..=0x26) {
            return Err(KERR_ARGUMENT);
        }
        self.router.bindings.push((service, port));
        Ok(())
    }
    fn send(&mut self, port: u32, dst: (u32, u32), payload: &[u8], essential: bool) -> bool {
        if !self.router.bindings.iter().any(|b| b.1 == port) {
            return false;
        }
        if essential {
            self.router
                .send_must(self.platform, qrtr::TYPE_DATA, port, dst, payload)
        } else {
            self.router
                .send(self.platform, qrtr::TYPE_DATA, port, dst, payload)
        }
    }
    fn log(&mut self, args: fmt::Arguments) {
        self.platform.log(args);
    }
    fn utc_ms(&mut self) -> i64 {
        self.platform.utc_ms()
    }
}
struct Hex<'a>(&'a [u8]);

impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0.iter().take(40) {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// One line per packet: direction, type, src -> dst, size, and the QMI header or control words.
fn trace(p: &mut dyn Platform, dir: &str, h: &Header, payload: &[u8]) {
    let (src, dst) = (h.src_port as i32, h.dst_port as i32);
    if h.ty != qrtr::TYPE_DATA {
        let w = qrtr::ctrl_words(payload).map(|c| c.1).unwrap_or_default();
        p.log(format_args!(
            "{dir} ctrl {} {}:{src} -> {}:{dst} {w:?}",
            h.ty, h.src_node, h.dst_node
        ));
        return;
    }
    let confirm = if h.confirm_rx != 0 { " confirm" } else { "" };
    match qmi::Msg::parse(payload) {
        Some(m) => p.log(format_args!(
            "{dir} data {}:{src} -> {}:{dst} {} B qmi type {} id {:#x} txn {}{confirm} {}",
            h.src_node,
            h.dst_node,
            payload.len(),
            m.ty,
            m.id,
            m.txn,
            Hex(payload)
        )),
        None => p.log(format_args!(
            "{dir} data {}:{src} -> {}:{dst} {} B{confirm} {}",
            h.src_node,
            h.dst_node,
            payload.len(),
            Hex(payload)
        )),
    }
}

fn tftp_name(b: &[u8]) -> &str {
    b.get(2..)
        .and_then(|s| s.split(|&c| c == 0).next())
        .and_then(|s| core::str::from_utf8(s).ok())
        .unwrap_or("?")
}

#[cfg(test)]
extern crate self as hub;
#[cfg(test)]
mod tests;
