// SPDX-License-Identifier: Apache-2.0
//! NeoAudio (docs/audio.md): the audio service MODEMD serves next to NeoModem, because MODEMD owns the ADSP. One
//! session is one stream ([`neoaudio`]); every step the streams are mixed into the ADSP's media periods
//! ([`crate::adsp::CallAudio::media_feed`]) or, when nothing plays them (no audio DSP, a call, the ringtone, a start
//! that failed), consumed in real time by the clock sink so players keep their pace. A WRITE waits while its stream's
//! queue is full; a READ waits until it is full.
use alloc::vec;
use alloc::vec::Vec;
use core::ops::ControlFlow;
use e32::dprintln;
use e32::server::{Message, Service};
use e32::sys::err;
use e32::sys::ipc::TVersion;
use modem_proto::neoaudio::{self, Info, Stream, op, sink, state};

/// Media stays wanted this long after the last sound, so short gaps don't power the amps down and up.
const LINGER_MS: u64 = 2_000;
/// A media start that has not reached the ADSP by then plays on the clock sink instead.
const START_MS: u64 = 3_000;
/// The clock sink consumes at most this much at once (after a stall).
const CLOCK_MAX_MS: u64 = 100;

struct Slot {
    session: u32,
    stream: Option<Stream>,
    /// a waiting WRITE and how many of its bytes were taken
    write: Option<(Message, usize)>,
    /// a waiting READ and how many bytes it has
    read: Option<(Message, usize)>,
}

pub struct Sound {
    slots: Vec<Slot>,
    next_session: u32,
    /// neoaudio::route
    pub route: i32,
    /// the media's amp volume 0..=10
    pub master: u8,
    /// media wanted until (ms)
    active_until: u64,
    /// when media was first wanted, while it is not playing on the ADSP yet
    wanted_since: Option<u64>,
    clock_ms: u64,
    pub sink: u8,
    /// a call or the ringtone has the speaker
    pub preempted: bool,
    /// a route or volume change for the running media path, for the daemon to apply
    pub changed: bool,
}

impl Sound {
    pub fn new() -> Self {
        Sound {
            slots: Vec::new(),
            next_session: 0,
            route: neoaudio::route::SPEAKER,
            master: 7,
            active_until: 0,
            wanted_since: None,
            clock_ms: 0,
            sink: sink::CLOCK,
            preempted: false,
            changed: false,
        }
    }

    fn streams(&mut self) -> impl Iterator<Item = &mut Stream> {
        self.slots.iter_mut().filter_map(|s| s.stream.as_mut())
    }

    /// Whether the media path should run: something plays now, or did within [`LINGER_MS`].
    pub fn wants_media(&mut self, now: u64) -> bool {
        if self.streams().any(|s| s.active()) {
            self.active_until = now + LINGER_MS;
        }
        now < self.active_until
    }

    /// One step: the mix goes to the ADSP when `dsp` (media runs there) is given, else to the clock sink, unless
    /// media is starting (`starting`: the streams wait, briefly, so the start of a sound is not lost). Then the waiting
    /// WRITEs and READs move on.
    pub fn step(&mut self, now: u64, dsp: Option<&mut crate::adsp::CallAudio>, starting: bool) {
        match dsp {
            Some(a) => {
                self.wanted_since = None;
                self.sink = sink::DSP;
                self.clock_ms = now;
                let slots = &mut self.slots;
                a.media_feed(|out| {
                    neoaudio::mix(slots.iter_mut().filter_map(|s| s.stream.as_mut()), out);
                });
            }
            None => {
                let since = *self.wanted_since.get_or_insert(now);
                let hold = starting && now.saturating_sub(since) < START_MS;
                if !starting {
                    self.wanted_since = None;
                }
                self.sink = sink::CLOCK;
                let ms = now.saturating_sub(self.clock_ms).min(CLOCK_MAX_MS);
                self.clock_ms = now;
                if !hold && ms > 0 {
                    let mut out = vec![0i16; (ms * u64::from(neoaudio::RATE) / 1000) as usize];
                    neoaudio::mix(self.streams(), &mut out);
                    // shortcut: the mic is silence until recording through the ADSP (A5)
                    out.fill(0);
                    neoaudio::capture(self.streams(), &out);
                }
            }
        }
        for s in self.slots.iter_mut() {
            service(s);
        }
    }

    fn slot(&mut self, session: u32) -> Option<&mut Slot> {
        self.slots.iter_mut().find(|s| s.session == session)
    }
}

/// Move a slot's waiting WRITE into its queue and its READ out of it; complete what is done.
fn service(s: &mut Slot) {
    let Some(st) = s.stream.as_mut() else {
        return;
    };
    if let Some((msg, done)) = s.write.take() {
        let total = msg.des_len(0).unwrap_or(0);
        let mut done = done;
        let mut buf = [0u8; 4096];
        let mut failed = None;
        while done < total {
            let want = (total - done).min(st.space()).min(buf.len());
            if want == 0 {
                break;
            }
            match msg.read8(0, &mut buf[..want], done) {
                Ok(n) if n > 0 => {
                    let took = st.push(&buf[..n]);
                    done += took;
                    if took < n {
                        break;
                    }
                }
                Ok(_) => break,
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            }
        }
        match failed {
            Some(e) => msg.complete(e),
            None if done >= total => msg.complete(done as i32),
            None => s.write = Some((msg, done)),
        }
    }
    if let Some((msg, done)) = s.read.take() {
        let total = msg.des_max_len(0).unwrap_or(0);
        let mut buf = vec![0u8; (total - done).min(8192)];
        let n = st.pop(&mut buf);
        let mut done = done;
        let r = if n > 0 {
            msg.write8(0, &buf[..n], done)
        } else {
            Ok(())
        };
        done += n;
        match r {
            Err(e) => msg.complete(e),
            Ok(()) if done >= total => msg.complete(done as i32),
            Ok(()) => s.read = Some((msg, done)),
        }
    }
}

impl Service for Sound {
    type Session = u32;

    fn connect(&mut self, v: TVersion) -> Result<u32, i32> {
        if v.major as i8 != neoaudio::VERSION.0 {
            return Err(err::KERR_NOT_SUPPORTED);
        }
        if self.slots.len() >= 16 {
            return Err(neoaudio::err::IN_USE);
        }
        self.next_session = self.next_session.wrapping_add(1).max(1);
        self.slots.push(Slot {
            session: self.next_session,
            stream: None,
            write: None,
            read: None,
        });
        Ok(self.next_session)
    }

    fn message(&mut self, session: &mut u32, msg: Message) {
        let (route, master) = (self.route, self.master);
        let sink = self.sink;
        let preempted = self.preempted;
        let mut new_route = None;
        let mut new_master = None;
        let Some(s) = self.slot(*session) else {
            msg.complete(neoaudio::err::NOT_READY);
            return;
        };
        let r = match msg.function() {
            op::OPEN => {
                if s.stream.is_some() {
                    neoaudio::err::IN_USE
                } else {
                    let record = msg.int(2) & neoaudio::flag::RECORD != 0;
                    match Stream::new(msg.int(0) as u32, msg.int(1) as u32, record) {
                        Ok(st) => {
                            dprintln!(
                                "[neoaudio] session {} opens {} Hz x{} {}",
                                session,
                                st.rate,
                                st.channels,
                                if record { "record" } else { "play" }
                            );
                            s.stream = Some(st);
                            err::KERR_NONE
                        }
                        Err(e) => e,
                    }
                }
            }
            op::WRITE | op::READ => {
                let write = msg.function() == op::WRITE;
                let ok = s.stream.as_ref().is_some_and(|st| st.record != write);
                if !ok {
                    neoaudio::err::NOT_READY
                } else if (write && s.write.is_some()) || (!write && s.read.is_some()) {
                    neoaudio::err::IN_USE
                } else {
                    if write {
                        s.write = Some((msg, 0));
                    } else {
                        s.read = Some((msg, 0));
                    }
                    service(s);
                    return;
                }
            }
            op::VOLUME => match s.stream.as_mut() {
                Some(st) if (0..=100).contains(&msg.int(0)) => {
                    st.volume = msg.int(0) as u8;
                    err::KERR_NONE
                }
                Some(_) => neoaudio::err::ARGUMENT,
                None => neoaudio::err::NOT_READY,
            },
            op::PAUSE => match s.stream.as_mut() {
                Some(st) => {
                    st.paused = msg.int(0) != 0;
                    err::KERR_NONE
                }
                None => neoaudio::err::NOT_READY,
            },
            op::STOP => {
                if let Some(st) = s.stream.as_mut() {
                    st.flush();
                }
                if let Some((m, _)) = s.write.take() {
                    m.complete(neoaudio::err::CANCEL);
                }
                if let Some((m, n)) = s.read.take() {
                    m.complete(n as i32);
                }
                err::KERR_NONE
            }
            op::INFO => {
                let info = match s.stream.as_ref() {
                    Some(st) => st.info(
                        if preempted && !st.record {
                            state::PREEMPTED
                        } else if st.paused {
                            state::PAUSED
                        } else {
                            state::RUNNING
                        },
                        sink,
                    ),
                    None => Info::default(),
                };
                msg.write8(0, &info.to_bytes(), 0)
                    .map_or_else(|e| e, |_| err::KERR_NONE)
            }
            op::ROUTE => {
                let r = msg.int(0);
                if (neoaudio::route::SPEAKER..=neoaudio::route::BOTTOM).contains(&r) {
                    new_route = (r != route).then_some(r);
                    err::KERR_NONE
                } else {
                    neoaudio::err::ARGUMENT
                }
            }
            op::MASTER => {
                let v = msg.int(0);
                if (0..=10).contains(&v) {
                    new_master = (v as u8 != master).then_some(v as u8);
                    err::KERR_NONE
                } else {
                    neoaudio::err::ARGUMENT
                }
            }
            _ => neoaudio::err::ARGUMENT,
        };
        if let Some(r) = new_route {
            self.route = r;
            self.changed = true;
        }
        if let Some(v) = new_master {
            self.master = v;
            self.changed = true;
        }
        msg.complete(r);
    }

    fn disconnect(&mut self, session: u32) -> ControlFlow<()> {
        if let Some(i) = self.slots.iter().position(|s| s.session == session) {
            let s = self.slots.remove(i);
            if let Some((m, _)) = s.write {
                m.complete(neoaudio::err::CANCEL);
            }
            if let Some((m, n)) = s.read {
                m.complete(n as i32);
            }
        }
        ControlFlow::Continue(())
    }
}
