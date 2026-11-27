// SPDX-License-Identifier: Apache-2.0
extern crate std;

use super::*;
use alloc::vec;
use core::fmt;
use hub_ipc::Server;
use qrtr::qmi::Builder;

const LOC: (u32, u32) = (0, 74);

#[derive(Default)]
struct Fake {
    binds: Vec<(u32, u32)>,
    sent: Vec<Vec<u8>>,
    utc: i64,
}

impl Bus for Fake {
    fn bind(&mut self, service: u32, port: u32) -> Result<(), i32> {
        self.binds.push((service, port));
        Ok(())
    }
    fn send(&mut self, port: u32, dst: (u32, u32), payload: &[u8], _: bool) -> bool {
        assert_eq!((port, dst), (PORT, LOC));
        self.sent.push(payload.to_vec());
        true
    }
    fn log(&mut self, args: fmt::Arguments) {
        std::println!("{args}");
    }
    fn utc_ms(&mut self) -> i64 {
        self.utc
    }
}

impl Fake {
    /// the ids sent since the last call
    fn ids(&mut self) -> Vec<u16> {
        self.sent
            .drain(..)
            .map(|m| Msg::parse(&m).unwrap().id)
            .collect()
    }
}

fn up() -> Event {
    Event::ServerUp(Server {
        service: proto::SERVICE,
        instance: 2,
        node: LOC.0,
        port: LOC.1,
    })
}

fn ind(b: Builder) -> Event {
    Event::Packet {
        src: LOC,
        dst_port: PORT,
        payload: b.finish(),
    }
}

fn indication(id: u16) -> Builder {
    Builder::new(INDICATION, 0, id)
}

fn ready() -> (Gnss, Fake) {
    let mut g = Gnss::new();
    let mut f = Fake::default();
    g.bind(&mut f).unwrap();
    g.event(&mut f, &up(), 0);
    f.sent.clear();
    (g, f)
}

/// An SV Info list: one entry per SNR (0 = searched for, not heard)
fn sv_list(snrs: &[f32]) -> Vec<u8> {
    let mut v = vec![snrs.len() as u8];
    for (i, snr) in snrs.iter().enumerate() {
        v.extend_from_slice(&0xffu32.to_le_bytes()); // every field valid
        v.extend_from_slice(&1u32.to_le_bytes()); // GPS
        v.extend_from_slice(&(i as u16 + 1).to_le_bytes());
        v.push(1);
        v.extend_from_slice(&2u32.to_le_bytes()); // tracking
        v.push(1);
        v.extend_from_slice(&45f32.to_le_bytes());
        v.extend_from_slice(&180f32.to_le_bytes());
        v.extend_from_slice(&snr.to_le_bytes());
    }
    v
}

fn fix(lat: f64, lon: f64) -> Builder {
    indication(id::POSITION_REPORT)
        .u32(0x01, proto::session::SUCCESS)
        .u8(0x02, proto::SESSION)
        .tlv(0x10, &lat.to_le_bytes())
        .tlv(0x11, &lon.to_le_bytes())
        .tlv(0x12, &8.5f32.to_le_bytes())
        .tlv(0x1b, &(-1.2f32).to_le_bytes())
        .u64(0x25, 1_795_173_061_000)
        .tlv(0x2c, &[7, 1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0, 7, 0])
}

#[test]
fn service_up_sets_the_engine_up() {
    let mut g = Gnss::new();
    let mut f = Fake::default();
    g.bind(&mut f).unwrap();
    assert_eq!(f.binds, vec![(16, 0x28)]);
    assert_eq!(g.status(0).flags & flag::SERVICE, 0);
    g.event(&mut f, &up(), 0);
    assert_eq!(
        f.ids(),
        vec![
            id::REGISTER_EVENTS,
            id::GET_ENGINE_LOCK,
            id::SET_OPERATION_MODE,
            id::ORBITS_SOURCE
        ]
    );
    assert_eq!(g.status(0).flags & flag::SERVICE, flag::SERVICE);
    assert_eq!(g.status(0).state, state::OFF);
}

#[test]
fn register_and_start_carry_the_documented_tlvs() {
    let r = proto::register_events(1);
    let m = Msg::parse(&r).unwrap();
    assert_eq!(proto::u64_of(&m, 0x01), Some(0b1011_0011));
    let s = proto::start(2, 1000);
    let m = Msg::parse(&s).unwrap();
    assert_eq!(
        (m.u8(0x01), m.u32(0x10), m.u32(0x12), m.u32(0x13)),
        (Some(1), Some(1), Some(1), Some(1000))
    );
}

#[test]
fn an_mi_lock_is_lifted() {
    let (mut g, mut f) = ready();
    g.event(
        &mut f,
        &ind(indication(id::GET_ENGINE_LOCK)
            .u32(0x01, 0)
            .u32(0x10, proto::lock::MI)),
        0,
    );
    let m = f.sent.pop().unwrap();
    let m = Msg::parse(&m).unwrap();
    assert_eq!(
        (m.id, m.u32(0x01)),
        (id::SET_ENGINE_LOCK, Some(proto::lock::NONE))
    );
    g.event(
        &mut f,
        &ind(indication(id::GET_ENGINE_LOCK)
            .u32(0x01, 0)
            .u32(0x10, proto::lock::NONE)),
        0,
    );
    assert!(f.sent.is_empty());
}

#[test]
fn a_session_finds_a_fix() {
    let (mut g, mut f) = ready();
    g.start(&mut f, 1000);
    assert_eq!(f.ids(), vec![id::ORBITS_VALIDITY, id::START]);
    assert_eq!(g.status(0).state, state::SEARCHING);
    let progress = indication(id::POSITION_REPORT)
        .u32(0x01, proto::session::IN_PROGRESS)
        .u8(0x02, 1);
    g.event(&mut f, &ind(progress), 5000);
    assert_eq!(g.status(0).state, state::SEARCHING);
    g.event(
        &mut f,
        &ind(indication(id::SV_INFO).tlv(0x10, &sv_list(&[0.0, 18.5, 31.2]))),
        6000,
    );
    g.event(&mut f, &ind(fix(40.1772, 44.5036)), 32_500);
    let s = g.status(0);
    assert_eq!(s.state, state::FIX);
    assert_eq!((s.lat_e7, s.lon_e7), (401_772_000, 445_036_000));
    assert_eq!(
        (s.h_acc_dm, s.alt_dm, s.used, s.seen, s.snr),
        (85, -12, 7, 2, 31)
    );
    assert_eq!(s.ttff_ms, 31_500);
    // later fixes keep the first fix time
    g.event(&mut f, &ind(fix(40.1773, 44.5037)), 33_500);
    assert_eq!(g.status(0).ttff_ms, 31_500);
    // past the limit, a session that has a fix keeps running
    g.tick(&mut f, FIX_TIMEOUT_MS * 2);
    assert!(f.sent.is_empty());
}

#[test]
fn a_session_started_before_the_service_starts_with_it() {
    let mut g = Gnss::new();
    let mut f = Fake::default();
    g.start(&mut f, 0);
    assert!(f.sent.is_empty());
    g.event(&mut f, &up(), 10);
    assert_eq!(
        f.ids(),
        vec![
            id::REGISTER_EVENTS,
            id::GET_ENGINE_LOCK,
            id::SET_OPERATION_MODE,
            id::ORBITS_SOURCE,
            id::ORBITS_VALIDITY,
            id::START
        ]
    );
}

#[test]
fn sessions_are_counted() {
    let (mut g, mut f) = ready();
    g.start(&mut f, 0);
    g.start(&mut f, 0);
    f.sent.clear();
    g.stop(&mut f);
    assert!(f.sent.is_empty());
    g.stop(&mut f);
    assert_eq!(f.ids(), vec![id::STOP]);
    assert_eq!(g.status(0).state, state::OFF);
    g.stop(&mut f);
    assert!(f.sent.is_empty());
}

#[test]
fn no_fix_in_time_gives_up() {
    let (mut g, mut f) = ready();
    g.start(&mut f, 1000);
    f.sent.clear();
    g.tick(&mut f, FIX_TIMEOUT_MS);
    assert!(f.sent.is_empty());
    g.tick(&mut f, FIX_TIMEOUT_MS + 1000);
    assert_eq!(f.ids(), vec![id::STOP]);
    assert_eq!(g.status(0).state, state::FAILED);
    // a new START tries again
    g.start(&mut f, FIX_TIMEOUT_MS + 2000);
    assert_eq!(f.ids(), vec![id::ORBITS_VALIDITY, id::START]);
}

#[test]
fn a_locked_engine_shows() {
    let (mut g, mut f) = ready();
    g.start(&mut f, 0);
    let locked = indication(id::POSITION_REPORT)
        .u32(0x01, proto::session::ENGINE_LOCKED)
        .u8(0x02, 1);
    g.event(&mut f, &ind(locked), 100);
    assert_eq!(g.status(0).state, state::LOCKED);
}

#[test]
fn a_modem_reset_resumes_the_session() {
    let (mut g, mut f) = ready();
    g.start(&mut f, 0);
    f.sent.clear();
    g.event(
        &mut f,
        &Event::Reset {
            subsystem: hub_ipc::Subsystem::Mss,
            generation: 2,
        },
        100,
    );
    assert_eq!(g.status(0).flags & flag::SERVICE, 0);
    g.start(&mut f, 200); // another client: nothing sent while LOC is down
    assert!(f.sent.is_empty());
    g.event(&mut f, &up(), 300);
    let ids = f.ids();
    assert_eq!(&ids[4..], &[id::ORBITS_VALIDITY, id::START]);
}

#[test]
fn time_is_injected_only_with_a_known_clock() {
    let (mut g, mut f) = ready();
    g.event(&mut f, &ind(indication(id::INJECT_TIME_REQUEST)), 0);
    assert!(f.sent.is_empty());
    f.utc = 1_795_173_061_000;
    g.event(&mut f, &ind(indication(id::INJECT_TIME_REQUEST)), 0);
    let m = f.sent.pop().unwrap();
    let m = Msg::parse(&m).unwrap();
    assert_eq!(m.id, id::INJECT_UTC_TIME);
    assert_eq!(proto::u64_of(&m, 0x01), Some(1_795_173_061_000));
    assert_eq!(m.u32(0x02), Some(5000));
}

fn source(max_file: u32, max_part: u32) -> Builder {
    let mut list = vec![3u8];
    for u in [
        "http://xtrapath1.izatcloud.net/xtra3grcej.bin",
        "http://evil.example/xtra3grcej.bin",
        "http://xtrapath3.izatcloud.net/xtra3grcej.bin",
    ] {
        list.push(u.len() as u8);
        list.extend_from_slice(u.as_bytes());
    }
    let mut sizes = max_file.to_le_bytes().to_vec();
    sizes.extend_from_slice(&max_part.to_le_bytes());
    indication(id::ORBITS_SOURCE)
        .u32(0x01, 0)
        .tlv(0x10, &sizes)
        .tlv(0x11, &list)
}

#[test]
fn stale_xtra_is_asked_for_from_allowed_hosts_only() {
    let (mut g, mut f) = ready();
    g.event(&mut f, &ind(source(100_000, 1000)), 0);
    assert_eq!(
        g.xtra_urls(),
        vec![
            ("xtrapath1.izatcloud.net".into(), "/xtra3grcej.bin".into()),
            ("xtrapath3.izatcloud.net".into(), "/xtra3grcej.bin".into())
        ]
    );
    f.utc = 1_795_173_061_000;
    // no validity: needed
    g.event(
        &mut f,
        &ind(indication(id::ORBITS_VALIDITY).u32(0x01, 0)),
        0,
    );
    assert!(g.want_xtra);
    g.want_xtra = false;
    // valid for 2 more days: not needed
    let mut v = (f.utc as u64 - 3_600_000).to_le_bytes().to_vec();
    v.extend_from_slice(&49u16.to_le_bytes());
    g.event(
        &mut f,
        &ind(indication(id::ORBITS_VALIDITY).u32(0x01, 0).tlv(0x10, &v)),
        0,
    );
    assert!(!g.want_xtra);
    assert_eq!(g.status(f.utc).flags & flag::XTRA_VALID, flag::XTRA_VALID);
    // the modem asking itself also counts
    g.event(&mut f, &ind(indication(id::ORBITS_REQUEST)), 0);
    assert!(g.want_xtra);
}

#[test]
fn xtra_goes_in_acknowledged_parts() {
    let (mut g, mut f) = ready();
    g.event(&mut f, &ind(source(100_000, 1000)), 0);
    let data: Vec<u8> = (0..2500u32).map(|i| i as u8).collect();
    g.xtra(&mut f, data.clone(), 1_795_000_000_000);
    let mut got = Vec::new();
    for part in 1..=3u16 {
        let m = f.sent.pop().unwrap();
        assert!(f.sent.is_empty());
        let m = Msg::parse(&m).unwrap();
        assert_eq!(m.id, id::INJECT_ORBITS);
        assert_eq!(
            (
                m.u32(0x01),
                proto::u16_of(&m, 0x02),
                proto::u16_of(&m, 0x03)
            ),
            (Some(2500), Some(3), Some(part))
        );
        let d = m.tlv(0x04).unwrap();
        let n = u16::from_le_bytes([d[0], d[1]]) as usize;
        assert_eq!(n, d.len() - 2);
        assert!(n <= 1000);
        got.extend_from_slice(&d[2..]);
        g.event(
            &mut f,
            &ind(indication(id::INJECT_ORBITS).u32(0x01, 0).u16(0x10, part)),
            0,
        );
    }
    assert_eq!(got, data);
    assert_eq!(f.ids(), vec![id::ORBITS_VALIDITY]);
    assert_eq!(g.status(1_795_000_060_000).xtra_age_s, 60);
}

#[test]
fn a_refused_part_ends_the_upload() {
    let (mut g, mut f) = ready();
    g.xtra(&mut f, vec![1; 3000], 0);
    f.sent.clear();
    g.event(&mut f, &ind(indication(id::INJECT_ORBITS).u32(0x01, 1)), 0);
    assert!(f.sent.is_empty());
    // and an oversized file never starts
    g.event(&mut f, &ind(source(1000, 1000)), 0);
    g.xtra(&mut f, vec![1; 1001], 0);
    assert!(f.sent.is_empty());
}

#[test]
fn packets_from_elsewhere_are_ignored() {
    let (mut g, mut f) = ready();
    g.start(&mut f, 0);
    let mut e = ind(fix(1.0, 2.0));
    if let Event::Packet { src, .. } = &mut e {
        *src = (1, 5);
    }
    g.event(&mut f, &e, 100);
    assert_eq!(g.status(0).state, state::SEARCHING);
}

#[test]
fn hwhub_lets_gnssd_bind_loc() {
    assert_eq!(
        hub_ipc::service_owner(proto::SERVICE),
        Some(hub_ipc::sid::GNSSD)
    );
}
