// SPDX-License-Identifier: Apache-2.0
use super::*;
use alloc::vec;
use alloc::vec::Vec;

fn snapshot() -> Snapshot {
    Snapshot {
        state: State {
            radio_attached: true,
            radio_enabled: true,
            radio_failed: false,
            ipa_up: true,
        },
        phase: phase::RECOVER,
        adsp: adsp::UP,
        mss: [8, 0, 2, 1],
        adsp_state: [4, 0, 6, 3],
        recovery_attempt: 3,
        recovery_error: -33,
        generation: u32::MAX,
        no_hardware: true,
    }
}
fn events() -> Vec<Event> {
    let server = Server {
        service: 2,
        instance: 0x201,
        node: 0,
        port: 68,
    };
    vec![
        Event::Packet {
            src: (0, 68),
            dst_port: 32,
            payload: vec![1, 2, 3],
        },
        Event::ServerUp(server),
        Event::ServerDown(server),
        Event::Reset {
            subsystem: Subsystem::Mss,
            generation: 7,
        },
        Event::Reset {
            subsystem: Subsystem::All,
            generation: 8,
        },
        Event::State(snapshot().state),
        Event::RadioStarted,
        Event::RadioResponse,
        Event::Status(snapshot()),
        Event::Dropped(14),
    ]
}
#[test]
fn events_and_snapshot_round_trip_with_exact_lengths() {
    let s = snapshot();
    assert_eq!(s.to_bytes().len(), Snapshot::SIZE);
    assert_eq!(Snapshot::from_bytes(&s.to_bytes()), Some(s));
    for e in events() {
        let b = encode_event(&e);
        assert_eq!(decode_event(&b), Some(e));
        for n in 0..b.len() {
            assert!(decode_event(&b[..n]).is_none());
        }
        let mut trailing = b;
        trailing.push(0);
        assert!(decode_event(&trailing).is_none());
    }
}
#[test]
fn send_round_trip_and_rejects_invalid_lengths_flags_and_truncation() {
    for essential in [false, true] {
        let send = Send {
            port: 35,
            dst: (0, 50),
            essential,
            payload: &[1, 2, 3],
        };
        let b = send.encode();
        assert_eq!(Send::decode(&b), Some(send));
        for n in 0..b.len() {
            assert!(Send::decode(&b[..n]).is_none());
        }
        let mut bad = b.clone();
        bad[12..16].copy_from_slice(&2u32.to_le_bytes());
        assert!(Send::decode(&bad).is_none());
        let mut bad = b.clone();
        bad[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(Send::decode(&bad).is_none());
        let mut bad = b;
        bad.push(0);
        assert!(Send::decode(&bad).is_none());
    }
    let payload = vec![0; MAX_PAYLOAD + 1];
    assert!(
        Send::decode(
            &Send {
                port: 1,
                dst: (0, 1),
                essential: true,
                payload: &payload
            }
            .encode()
        )
        .is_none()
    );
}
#[test]
fn malformed_discriminants_and_status_are_rejected() {
    assert!(decode_event(&99u32.to_le_bytes()).is_none());
    let mut reset = encode_event(&Event::Reset {
        subsystem: Subsystem::Mss,
        generation: 0,
    });
    reset[4..8].copy_from_slice(&2u32.to_le_bytes());
    assert!(decode_event(&reset).is_none());
    let mut state = encode_event(&Event::State(State::default()));
    state[4] = 16;
    assert!(decode_event(&state).is_none());
    for (offset, value) in [(0, 32), (4, 9), (8, 5), (44, 256)] {
        let mut b = snapshot().to_bytes();
        b[offset..offset + 4].copy_from_slice(&u32::to_le_bytes(value));
        assert!(Snapshot::from_bytes(&b).is_none());
    }
    let packet = Event::Packet {
        src: (0, 1),
        dst_port: 2,
        payload: vec![0; MAX_PAYLOAD + 1],
    };
    assert!(decode_event(&encode_event(&packet)).is_none());
}
#[test]
fn batch_preserves_boundaries_and_rejects_truncation_count_and_trailing_data() {
    let events = events();
    let (all, n) = encode_batch(&events, usize::MAX);
    assert_eq!(n, events.len());
    assert_eq!(decode_batch(&all), Some(events.clone()));
    for n in 0..all.len() {
        assert!(decode_batch(&all[..n]).is_none());
    }
    for limit in 0..=all.len() {
        let (b, n) = encode_batch(&events, limit);
        assert!(b.len() <= limit);
        if limit >= 4 {
            assert_eq!(decode_batch(&b), Some(events[..n].to_vec()));
        } else {
            assert!(b.is_empty());
        }
    }
    let mut bad = all.clone();
    bad[0..4].copy_from_slice(&65u32.to_le_bytes());
    assert!(decode_batch(&bad).is_none());
    let mut bad = all.clone();
    bad[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(decode_batch(&bad).is_none());
    let mut bad = all;
    bad.push(0);
    assert!(decode_batch(&bad).is_none());
}
#[test]
fn queue_drops_oldest_and_reports_exact_count_before_survivors() {
    let mut q = SessionQueue::new();
    for generation in 0..67 {
        assert_eq!(
            q.push(Event::Reset {
                subsystem: Subsystem::Mss,
                generation
            }),
            generation >= 64
        );
    }
    assert_eq!(q.len(), 64);
    assert_eq!(q.dropped(), 3);
    assert!(q.batch(15).is_none());
    assert_eq!(q.dropped(), 3);
    let b = q.batch(16).unwrap();
    assert_eq!(decode_batch(&b), Some(vec![Event::Dropped(3)]));
    assert_eq!(q.dropped(), 0);
    assert_eq!(q.len(), 64);
    let b = q.batch(usize::MAX).unwrap();
    let events = decode_batch(&b).unwrap();
    assert_eq!(events.len(), 64);
    assert_eq!(
        events[0],
        Event::Reset {
            subsystem: Subsystem::Mss,
            generation: 3
        }
    );
    assert!(q.is_empty());
    assert!(q.batch(8192).is_none());
}
#[test]
fn queue_batch_limit_never_consumes_an_event_that_did_not_fit() {
    let mut q = SessionQueue::new();
    q.push(Event::RadioStarted);
    q.push(Event::RadioResponse);
    assert!(q.batch(11).is_none());
    assert_eq!(q.len(), 2);
    assert_eq!(
        decode_batch(&q.batch(12).unwrap()),
        Some(vec![Event::RadioStarted])
    );
    assert_eq!(q.len(), 1);
    assert_eq!(
        decode_batch(&q.batch(12).unwrap()),
        Some(vec![Event::RadioResponse])
    );
    assert!(q.is_empty());
}

#[test]
fn only_the_owning_daemon_reaches_a_service() {
    assert_eq!(service_owner(11), Some(sid::MODEMD)); // UIM: SIM PINs
    assert_eq!(service_owner(2), Some(sid::MODEMD));
    assert_eq!(service_owner(16), Some(sid::GNSSD));
    assert_eq!(service_owner(69), Some(sid::WIFID));
    assert_eq!(service_owner(0x1234), None);
}

// D11: what HWHUB lets each client do (access.rs)
const UIM_SRV: Server = Server {
    service: 11,
    instance: 1,
    node: 0,
    port: 40,
};
const WLFW_SRV: Server = Server {
    service: 69,
    instance: 1,
    node: 0,
    port: 41,
};

#[test]
fn only_the_owner_binds_and_a_taken_port_stays_taken() {
    assert_eq!(access::bind(sid::MODEMD, 11, false), Ok(()));
    assert_eq!(access::bind(0xE000_5001, 11, false), Err(access::DENIED)); // NEOPHONE: no UIM
    assert_eq!(access::bind(sid::WIFID, 11, false), Err(access::DENIED)); // another daemon's service
    assert_eq!(
        access::bind(sid::MODEMD, 0x1234, false),
        Err(access::DENIED)
    ); // nobody's
    assert_eq!(access::bind(sid::MODEMD, 11, true), Err(access::IN_USE));
    assert_eq!(
        access::bind(0xE000_5001, 11, true),
        Err(access::DENIED),
        "the SID first"
    );
}

#[test]
fn a_send_goes_from_a_bound_port_to_a_server_of_that_service_only() {
    let servers = [UIM_SRV, WLFW_SRV];
    let modemd = [(11, 0x4000)];
    assert_eq!(access::send(&modemd, &servers, 0x4000, (0, 40)), Ok(()));
    // a port it didn't bind
    assert_eq!(
        access::send(&modemd, &servers, 0x4001, (0, 40)),
        Err(access::DENIED)
    );
    assert_eq!(
        access::send(&[], &servers, 0x4000, (0, 40)),
        Err(access::DENIED)
    );
    // MODEMD's UIM port to WLFW's server
    assert_eq!(
        access::send(&modemd, &servers, 0x4000, (0, 41)),
        Err(access::DENIED)
    );
    // the server went down (recovery): not ready, not a refusal
    assert_eq!(
        access::send(&modemd, &[WLFW_SRV], 0x4000, (0, 40)),
        Err(access::NOT_READY)
    );
    assert_eq!(
        access::send(&modemd, &[], 0x4000, (0, 40)),
        Err(access::NOT_READY)
    );
}

#[test]
fn only_modemd_recovers_and_only_daemons_quiesce() {
    assert!(access::recover(sid::MODEMD));
    for s in [sid::WIFID, sid::GNSSD, 0xE000_5001, 0] {
        assert!(!access::recover(s), "{s:#x}");
    }
    for s in [sid::MODEMD, sid::WIFID, sid::GNSSD] {
        assert!(access::quiesced(s));
    }
    assert!(!access::quiesced(0xE000_2007));
}

#[test]
fn other_processes_cant_use_up_the_daemons_sessions() {
    let other = 0xE000_5001;
    assert_eq!(access::connect(other, 0, 0), Ok(()));
    assert_eq!(
        access::connect(other, 0, access::OTHERS),
        Err(access::IN_USE)
    );
    // every slot others may have is taken: each daemon still connects
    for s in [sid::MODEMD, sid::WIFID, sid::GNSSD] {
        assert_eq!(access::connect(s, 0, access::OTHERS), Ok(()));
        assert_eq!(
            access::connect(s, access::PER_DAEMON - 1, access::OTHERS),
            Ok(())
        );
        assert_eq!(
            access::connect(s, access::PER_DAEMON, 0),
            Err(access::IN_USE)
        );
    }
}
