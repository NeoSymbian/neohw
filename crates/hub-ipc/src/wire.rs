// SPDX-License-Identifier: Apache-2.0
//! NeoHub v1 uses little-endian u32 words and explicit lengths, never Rust layouts.
use crate::{Event, Server, State, Subsystem};
use alloc::vec::Vec;

pub const NAME: &str = "NeoHub";
/// major, minor, build, converted to e32's TVersion by each executable.
pub const VERSION: (i8, i8, i16) = (1, 0, 0);
pub const MAX_PAYLOAD: usize = 8192;
pub const MAX_EVENTS: usize = 64;

pub mod op {
    /// arg0 service, arg1 local port. The session owns the binding.
    pub const BIND: i32 = 0;
    /// arg0 Send descriptor; completion is physically sent (0 or 1), or a negative error.
    pub const SEND: i32 = 1;
    /// arg0 writable batch descriptor. Held when no events are available.
    pub const RECV: i32 = 2;
    pub const CANCEL_RECV: i32 = 3;
    /// arg0 writable Snapshot descriptor.
    pub const STATUS: i32 = 4;
    /// arg0 processor mask (bit 0 MSS, bit 1 ADSP).
    pub const RECOVER: i32 = 5;
    /// arg0 Reset generation, after the client's state has been cleared.
    pub const QUIESCED: i32 = 6;
}

pub mod phase {
    pub const ADSP: u8 = 0;
    pub const IPA: u8 = 1;
    pub const READ: u8 = 2;
    pub const WAIT: u8 = 3;
    pub const RUNNING: u8 = 4;
    pub const RECOVER: u8 = 5;
    pub const FAILED: u8 = 6;
    pub const DISABLED: u8 = 7;
    pub const ABSENT: u8 = 8;
}

pub mod adsp {
    pub const IDLE: u8 = 0;
    pub const LOADING: u8 = 1;
    pub const WAITING: u8 = 2;
    pub const UP: u8 = 3;
    pub const FAILED: u8 = 4;
}

/// Hub-owned boot and recovery facts; the NeoModem wire contract stays unchanged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub state: State,
    pub phase: u8,
    pub adsp: u8,
    pub mss: [u32; 4],
    pub adsp_state: [u32; 4],
    pub recovery_attempt: u8,
    pub recovery_error: i32,
    pub generation: u32,
    pub no_hardware: bool,
}

impl Snapshot {
    pub const SIZE: usize = 56;
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(Self::SIZE);
        word(
            &mut b,
            flags(self.state) | (u32::from(self.no_hardware) << 4),
        );
        word(&mut b, self.phase.into());
        word(&mut b, self.adsp.into());
        for v in self.mss.into_iter().chain(self.adsp_state) {
            word(&mut b, v);
        }
        word(&mut b, self.recovery_attempt.into());
        word(&mut b, self.recovery_error as u32);
        word(&mut b, self.generation);
        b
    }
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        let mut r = Reader(b);
        let flags = r.word()?;
        if flags & !31 != 0 {
            return None;
        }
        let phase = r.word()?;
        let adsp = r.word()?;
        if phase > u32::from(phase::ABSENT) || adsp > u32::from(adsp::FAILED) {
            return None;
        }
        let mss = [r.word()?, r.word()?, r.word()?, r.word()?];
        let adsp_state = [r.word()?, r.word()?, r.word()?, r.word()?];
        let recovery_attempt = u8::try_from(r.word()?).ok()?;
        let recovery_error = r.word()? as i32;
        let generation = r.word()?;
        r.done().then_some(Self {
            state: state(flags & 15)?,
            phase: phase as u8,
            adsp: adsp as u8,
            mss,
            adsp_state,
            recovery_attempt,
            recovery_error,
            generation,
            no_hardware: flags & 16 != 0,
        })
    }
}

/// SEND's descriptor: port, destination node/port, essential (0/1), length, payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Send<'a> {
    pub port: u32,
    pub dst: (u32, u32),
    pub essential: bool,
    pub payload: &'a [u8],
}
impl<'a> Send<'a> {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(20 + self.payload.len());
        for v in [self.port, self.dst.0, self.dst.1, u32::from(self.essential)] {
            word(&mut b, v);
        }
        word(&mut b, self.payload.len() as u32);
        b.extend_from_slice(self.payload);
        b
    }
    pub fn decode(b: &'a [u8]) -> Option<Self> {
        let mut r = Reader(b);
        let port = r.word()?;
        let dst = (r.word()?, r.word()?);
        let essential = match r.word()? {
            0 => false,
            1 => true,
            _ => return None,
        };
        let len = r.word()? as usize;
        if len > MAX_PAYLOAD {
            return None;
        }
        let payload = r.take(len)?;
        r.done().then_some(Self {
            port,
            dst,
            essential,
            payload,
        })
    }
}

pub fn encode_event(event: &Event) -> Vec<u8> {
    let mut b = Vec::new();
    match event {
        Event::Packet {
            src,
            dst_port,
            payload,
        } => {
            word(&mut b, 0);
            for v in [src.0, src.1, *dst_port, payload.len() as u32] {
                word(&mut b, v);
            }
            b.extend_from_slice(payload);
        }
        Event::ServerUp(s) | Event::ServerDown(s) => {
            word(
                &mut b,
                if matches!(event, Event::ServerUp(_)) {
                    1
                } else {
                    2
                },
            );
            for v in [s.service, s.instance, s.node, s.port] {
                word(&mut b, v);
            }
        }
        Event::Reset {
            subsystem,
            generation,
        } => {
            word(&mut b, 3);
            word(
                &mut b,
                match subsystem {
                    Subsystem::Mss => 1,
                    Subsystem::All => 3,
                },
            );
            word(&mut b, *generation);
        }
        Event::State(s) => {
            word(&mut b, 4);
            word(&mut b, flags(*s));
        }
        Event::RadioStarted => word(&mut b, 5),
        Event::RadioResponse => word(&mut b, 6),
        Event::Status(s) => {
            word(&mut b, 7);
            b.extend_from_slice(&s.to_bytes());
        }
        Event::Dropped(n) => {
            word(&mut b, 8);
            word(&mut b, *n);
        }
    }
    b
}

pub fn decode_event(b: &[u8]) -> Option<Event> {
    let mut r = Reader(b);
    let e = match r.word()? {
        0 => {
            let src = (r.word()?, r.word()?);
            let dst_port = r.word()?;
            let n = r.word()? as usize;
            if n > MAX_PAYLOAD {
                return None;
            }
            Event::Packet {
                src,
                dst_port,
                payload: r.take(n)?.to_vec(),
            }
        }
        kind @ (1 | 2) => {
            let s = Server {
                service: r.word()?,
                instance: r.word()?,
                node: r.word()?,
                port: r.word()?,
            };
            if kind == 1 {
                Event::ServerUp(s)
            } else {
                Event::ServerDown(s)
            }
        }
        3 => {
            let subsystem = match r.word()? {
                1 => Subsystem::Mss,
                3 => Subsystem::All,
                _ => return None,
            };
            Event::Reset {
                subsystem,
                generation: r.word()?,
            }
        }
        4 => Event::State(state(r.word()?)?),
        5 => Event::RadioStarted,
        6 => Event::RadioResponse,
        7 => Event::Status(Snapshot::from_bytes(r.take(Snapshot::SIZE)?)?),
        8 => Event::Dropped(r.word()?),
        _ => return None,
    };
    r.done().then_some(e)
}

/// A batch is count followed by (length, event) records. Never split an event at a descriptor boundary.
pub fn encode_batch(events: &[Event], max_len: usize) -> (Vec<u8>, usize) {
    if max_len < 4 {
        return (Vec::new(), 0);
    }
    let mut b = alloc::vec![0;4];
    let mut count = 0;
    for event in events.iter().take(MAX_EVENTS) {
        let e = encode_event(event);
        if e.len().saturating_add(4) > max_len.saturating_sub(b.len()) {
            break;
        }
        word(&mut b, e.len() as u32);
        b.extend_from_slice(&e);
        count += 1;
    }
    b[..4].copy_from_slice(&(count as u32).to_le_bytes());
    (b, count)
}

pub fn decode_batch(b: &[u8]) -> Option<Vec<Event>> {
    let mut r = Reader(b);
    let n = r.word()? as usize;
    if n > MAX_EVENTS {
        return None;
    }
    let mut events = Vec::with_capacity(n);
    for _ in 0..n {
        let len = r.word()? as usize;
        events.push(decode_event(r.take(len)?)?);
    }
    r.done().then_some(events)
}

fn word(b: &mut Vec<u8>, n: u32) {
    b.extend_from_slice(&n.to_le_bytes());
}
fn flags(s: State) -> u32 {
    u32::from(s.radio_attached)
        | (u32::from(s.radio_enabled) << 1)
        | (u32::from(s.radio_failed) << 2)
        | (u32::from(s.ipa_up) << 3)
}
fn state(flags: u32) -> Option<State> {
    (flags & !15 == 0).then_some(State {
        radio_attached: flags & 1 != 0,
        radio_enabled: flags & 2 != 0,
        radio_failed: flags & 4 != 0,
        ipa_up: flags & 8 != 0,
    })
}
struct Reader<'a>(&'a [u8]);
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let b = self.0.get(..n)?;
        self.0 = &self.0[n..];
        Some(b)
    }
    fn word(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn done(&self) -> bool {
        self.0.is_empty()
    }
}
