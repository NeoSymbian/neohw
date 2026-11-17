// SPDX-License-Identifier: Apache-2.0
//! NeoAudio (docs/audio.md): the audio service MODEMD.EXE serves next to NeoModem, for native apps and for the old
//! apps' DevSound bridge. One session = one stream:
//! - [`op::OPEN`] arg 0 = rate (8000..=96000 Hz), arg 1 = channels (1 or 2), arg 2 = [`flag`]s. Signed 16-bit
//!   little-endian PCM, channels interleaved.
//! - [`op::WRITE`] arg 0 = des8 of PCM (whole frames). Completes with the bytes taken once all of it is queued;
//!   while the stream's queue ([`QUEUE_MS`]) is full the request waits: that is the flow control.
//! - [`op::READ`] (a [`flag::RECORD`] stream) arg 0 = writable des8: completes with what was recorded once it is
//!   full, or with what there is at [`op::STOP`].
//! - [`op::VOLUME`] arg 0 = 0..=100; [`op::PAUSE`] arg 0 = 1 pause, 0 resume; [`op::STOP`] drops the queue and
//!   completes a waiting WRITE/READ (KErrCancel, or READ's bytes so far).
//! - [`op::INFO`] arg 0 = writable des8 of [`Info::SIZE`]: the stream's state, frames played/recorded, underruns.
//!   An underrun (the queue ran dry while playing) is how a player learns its data has ended.
//! - [`op::ROUTE`] arg 0 = [`route`]: where media plays when there is no call (all streams); [`op::MASTER`] arg 0 =
//!   0..=10, the media's amp volume (all streams).
//!
//! Everything is mixed to [`RATE`] mono, the ADSP's stream format: a linear resampler per stream, volume, and a
//! saturating sum. Recording is the reverse: the 48 kHz mono mic resampled to each stream's rate and channels.
//! During a call (or a ringtone) the media keeps being consumed in real time and is not heard.
#![no_std]
extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec::Vec;

pub const NAME: &str = "NeoAudio";
/// major, minor, build
pub const VERSION: (i8, i8, i16) = (1, 0, 0);
/// The device's rate: every stream is mixed to 48 kHz mono S16.
pub const RATE: u32 = 48_000;
/// How much audio a stream's queue holds before WRITE waits.
pub const QUEUE_MS: u32 = 250;

pub mod op {
    pub const OPEN: i32 = 1;
    pub const WRITE: i32 = 2;
    pub const READ: i32 = 3;
    pub const VOLUME: i32 = 4;
    pub const PAUSE: i32 = 5;
    pub const STOP: i32 = 6;
    pub const INFO: i32 = 7;
    pub const ROUTE: i32 = 8;
    /// arg 0 = the media's amp volume 0..=10 (all streams; the device's volume keys)
    pub const MASTER: i32 = 9;
}

pub mod flag {
    /// a recording stream (READ), else playback (WRITE)
    pub const RECORD: i32 = 1;
}

/// [`Info::state`]
pub mod state {
    pub const CLOSED: u8 = 0;
    pub const RUNNING: u8 = 1;
    pub const PAUSED: u8 = 2;
    /// a call or the ringtone has the speaker: the stream runs, unheard (recording gets silence)
    pub const PREEMPTED: u8 = 3;
}

/// [`Info::sink`]: what consumes the mix
pub mod sink {
    /// nothing audible: consumed in real time (no audio DSP, a call, the ringtone, starting up)
    pub const CLOCK: u8 = 0;
    /// the ADSP plays it
    pub const DSP: u8 = 1;
}

/// [`op::ROUTE`]
pub mod route {
    /// the loudspeaker (both amps)
    pub const SPEAKER: i32 = 0;
    /// the earpiece only
    pub const EARPIECE: i32 = 1;
    /// the bottom loudspeaker only
    pub const BOTTOM: i32 = 2;
}

pub mod err {
    pub const CANCEL: i32 = -3;
    pub const ARGUMENT: i32 = -6;
    pub const IN_USE: i32 = -14;
    pub const NOT_READY: i32 = -18;
}

/// [`op::INFO`]'s record, little-endian.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Info {
    pub state: u8,
    pub sink: u8,
    /// bytes waiting in the queue
    pub queued: u32,
    /// frames played (or recorded) since OPEN
    pub frames: u64,
    pub underruns: u32,
    pub overruns: u32,
}

impl Info {
    pub const SIZE: usize = 24;

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        b[0] = self.state;
        b[1] = self.sink;
        b[4..8].copy_from_slice(&self.queued.to_le_bytes());
        b[8..16].copy_from_slice(&self.frames.to_le_bytes());
        b[16..20].copy_from_slice(&self.underruns.to_le_bytes());
        b[20..24].copy_from_slice(&self.overruns.to_le_bytes());
        b
    }

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        let b: &[u8; Self::SIZE] = b.get(..Self::SIZE)?.try_into().ok()?;
        let u32_at = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        Some(Info {
            state: b[0],
            sink: b[1],
            queued: u32_at(4),
            frames: u64::from_le_bytes(b[8..16].try_into().unwrap()),
            underruns: u32_at(16),
            overruns: u32_at(20),
        })
    }
}

const ONE: u32 = 1 << 16;

/// Linear interpolation from one rate to another, one output sample per call. The output trails the input by one
/// sample.
#[derive(Clone, Debug)]
struct Resampler {
    step: u32,
    phase: u32,
    prev: i32,
    cur: i32,
}

impl Resampler {
    fn new(from: u32, to: u32) -> Self {
        Resampler {
            step: ((u64::from(from) << 16) / u64::from(to)) as u32,
            phase: ONE,
            prev: 0,
            cur: 0,
        }
    }

    /// The next output sample, or None when `src` runs dry (the state stays as it was: call again later).
    fn next(&mut self, mut src: impl FnMut() -> Option<i32>) -> Option<i32> {
        while self.phase >= ONE {
            let s = src()?;
            self.prev = self.cur;
            self.cur = s;
            self.phase -= ONE;
        }
        let v =
            self.prev + ((i64::from(self.cur - self.prev) * i64::from(self.phase)) >> 16) as i32;
        self.phase += self.step;
        Some(v)
    }
}

/// One client's stream: its format, its queue of interleaved samples, its resampler and counters.
#[derive(Debug)]
pub struct Stream {
    pub rate: u32,
    pub channels: u8,
    pub record: bool,
    /// 0..=100
    pub volume: u8,
    pub paused: bool,
    q: VecDeque<i16>,
    cap: usize,
    rs: Resampler,
    pub frames: u64,
    pub underruns: u32,
    pub overruns: u32,
    started: bool,
    starving: bool,
}

impl Stream {
    pub fn new(rate: u32, channels: u32, record: bool) -> Result<Self, i32> {
        if !(8_000..=96_000).contains(&rate) || !(1..=2).contains(&channels) {
            return Err(err::ARGUMENT);
        }
        let cap = (rate * QUEUE_MS / 1000) as usize * channels as usize;
        Ok(Stream {
            rate,
            channels: channels as u8,
            record,
            volume: 100,
            paused: false,
            q: VecDeque::with_capacity(cap),
            cap,
            rs: if record {
                Resampler::new(RATE, rate)
            } else {
                Resampler::new(rate, RATE)
            },
            frames: 0,
            underruns: 0,
            overruns: 0,
            started: false,
            starving: false,
        })
    }

    fn frame_bytes(&self) -> usize {
        2 * self.channels as usize
    }

    /// Bytes the queue can still take (whole frames).
    pub fn space(&self) -> usize {
        let free = (self.cap - self.q.len()) * 2;
        free - free % self.frame_bytes()
    }

    /// Bytes waiting in the queue.
    pub fn queued(&self) -> usize {
        self.q.len() * 2
    }

    /// Queue as much of `pcm` as fits, in whole frames; the bytes taken.
    pub fn push(&mut self, pcm: &[u8]) -> usize {
        let fb = self.frame_bytes();
        let n = pcm.len().min(self.space()) / fb * fb;
        for s in pcm[..n].as_chunks::<2>().0 {
            self.q.push_back(i16::from_le_bytes(*s));
        }
        if n > 0 {
            self.started = true;
            self.starving = false;
        }
        n
    }

    /// Take recorded PCM into `out`, whole frames; the bytes written.
    pub fn pop(&mut self, out: &mut [u8]) -> usize {
        let fb = self.frame_bytes();
        let n = out.len().min(self.queued()) / fb * fb;
        for d in out[..n].as_chunks_mut::<2>().0 {
            d.copy_from_slice(&self.q.pop_front().unwrap_or(0).to_le_bytes());
        }
        n
    }

    /// Drop what is queued and start the resampler afresh (STOP).
    pub fn flush(&mut self) {
        self.q.clear();
        self.rs = if self.record {
            Resampler::new(RATE, self.rate)
        } else {
            Resampler::new(self.rate, RATE)
        };
        self.started = false;
        self.starving = false;
    }

    pub fn info(&self, state: u8, sink: u8) -> Info {
        Info {
            state,
            sink,
            queued: self.queued() as u32,
            frames: self.frames,
            underruns: self.underruns,
            overruns: self.overruns,
        }
    }

    /// Whether this stream has anything to play now.
    pub fn active(&self) -> bool {
        !self.record && !self.paused && !self.q.is_empty()
    }

    fn gain(&self) -> i32 {
        i32::from(self.volume.min(100)) * 256 / 100
    }

    /// The next output sample at [`RATE`] mono, or None (an underrun if the stream had started).
    fn next_out(&mut self) -> Option<i32> {
        let (q, ch, frames) = (&mut self.q, self.channels, &mut self.frames);
        let v = self.rs.next(|| {
            if q.len() < ch as usize {
                return None;
            }
            *frames += 1;
            let mut sum = 0i32;
            for _ in 0..ch {
                sum += i32::from(q.pop_front().unwrap());
            }
            Some(sum / i32::from(ch))
        });
        match v {
            Some(v) => Some(v),
            None => {
                if self.started && !self.starving {
                    self.starving = true;
                    self.underruns += 1;
                }
                None
            }
        }
    }
}

/// Mix every playing stream into `out` ([`RATE`] mono, overwritten). Whether any stream contributed.
pub fn mix<'a>(streams: impl Iterator<Item = &'a mut Stream>, out: &mut [i16]) -> bool {
    let mut acc: Vec<i32> = alloc::vec![0; out.len()];
    let mut any = false;
    for st in streams {
        if st.record || st.paused {
            continue;
        }
        let gain = st.gain();
        for a in acc.iter_mut() {
            match st.next_out() {
                Some(v) => {
                    *a += (v * gain) >> 8;
                    any = true;
                }
                None => break,
            }
        }
    }
    for (o, a) in out.iter_mut().zip(acc) {
        *o = a.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
    }
    any
}

/// Feed `input` ([`RATE`] mono, the mic) to every recording stream: resampled, volume applied, copied to each
/// channel. A full queue drops the rest (an overrun).
pub fn capture<'a>(streams: impl Iterator<Item = &'a mut Stream>, input: &[i16]) {
    for st in streams {
        if !st.record || st.paused {
            continue;
        }
        let gain = st.gain();
        let mut i = 0;
        let mut dropped = false;
        while let Some(v) = st.rs.next(|| {
            let s = input.get(i).map(|&s| i32::from(s));
            i += 1;
            s
        }) {
            let v = ((v * gain) >> 8).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
            if st.q.len() + st.channels as usize > st.cap {
                dropped = true;
                continue;
            }
            for _ in 0..st.channels {
                st.q.push_back(v);
            }
            st.frames += 1;
        }
        if dropped {
            st.overruns += 1;
        }
    }
}

/// A sine tone as S16LE PCM (Dev tools' test and the old apps' tone player): `freq` Hz for `ms`, at `rate` with
/// `channels`, amplitude `amp` (0..=32767), 5 ms fades at both ends.
pub fn tone(freq: u32, ms: u32, rate: u32, channels: u32, amp: i16) -> Vec<u8> {
    let n = (rate as u64 * ms as u64 / 1000) as usize;
    let fade = (rate / 200).max(1) as usize;
    let mut v = Vec::with_capacity(n * channels as usize * 2);
    // phase in 1/65536 turns: a quarter-wave table would be faster; this runs once per test
    let step = ((u64::from(freq) << 32) / u64::from(rate)) as u32;
    let mut phase: u32 = 0;
    for i in 0..n {
        let s = sin_turns(phase);
        phase = phase.wrapping_add(step);
        let env = (i.min(n - 1 - i).min(fade) as i32 * 1024 / fade as i32).min(1024);
        let x = (((s * i32::from(amp)) >> 15) * env) >> 10;
        for _ in 0..channels {
            v.extend_from_slice(&(x as i16).to_le_bytes());
        }
    }
    v
}

/// sin(2 pi * phase / 2^32) in Q15, from a parabola fit (error under 0.1 %).
fn sin_turns(phase: u32) -> i32 {
    // x in [-1, 1) as Q15 over half a turn
    let x = ((phase as i32) >> 16) as i64; // -32768..32767 = -pi..pi
    let y = (4 * x * 32768 - 4 * x * x.abs()) >> 15; // 4x(1-|x|) in Q15
    // the extra precision step: y = 0.225 (y|y| - y) + y
    let p = ((y * y.abs()) >> 15) - y;
    (y + ((p * 7373) >> 15)) as i32 // 0.225 * 32768 = 7373
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    #[test]
    fn info_round_trips() {
        let i = Info {
            state: state::RUNNING,
            sink: sink::DSP,
            queued: 1234,
            frames: 5_000_000_000,
            underruns: 3,
            overruns: 4,
        };
        assert_eq!(Info::from_bytes(&i.to_bytes()), Some(i));
        assert_eq!(Info::from_bytes(&[0; 3]), None);
    }

    #[test]
    fn bad_formats_are_refused() {
        assert_eq!(Stream::new(7_999, 1, false).unwrap_err(), err::ARGUMENT);
        assert_eq!(Stream::new(48_000, 3, false).unwrap_err(), err::ARGUMENT);
        assert_eq!(Stream::new(48_000, 0, false).unwrap_err(), err::ARGUMENT);
    }

    #[test]
    fn same_rate_passes_through_one_sample_late() {
        let mut st = Stream::new(48_000, 1, false).unwrap();
        st.push(&pcm(&[100, 200, 300, 400]));
        let mut out = [0i16; 4];
        assert!(mix(core::iter::once(&mut st), &mut out));
        assert_eq!(out, [0, 100, 200, 300]);
        assert_eq!(st.frames, 4);
    }

    #[test]
    fn stereo_is_averaged_to_mono() {
        let mut st = Stream::new(48_000, 2, false).unwrap();
        st.push(&pcm(&[100, 300, -50, -150, 0, 0]));
        let mut out = [0i16; 3];
        mix(core::iter::once(&mut st), &mut out);
        assert_eq!(out, [0, 200, -100]);
    }

    #[test]
    fn half_rate_doubles_with_interpolation() {
        let mut st = Stream::new(24_000, 1, false).unwrap();
        st.push(&pcm(&[0, 1000, 2000, 3000]));
        let mut out = [0i16; 6];
        mix(core::iter::once(&mut st), &mut out);
        assert_eq!(out, [0, 0, 0, 500, 1000, 1500]);
    }

    #[test]
    fn rate_conversion_keeps_duration() {
        // 1 s at 44.1 kHz gives about 1 s at 48 kHz
        let mut st = Stream::new(44_100, 2, false).unwrap();
        let mut made = 0usize;
        let mut out = alloc::vec![0i16; 480];
        let mut fed = 0usize;
        let chunk = pcm(&[1000i16; 882]); // 441 stereo frames
        while fed < 44_100 || st.queued() > 0 {
            if fed < 44_100 && st.space() >= chunk.len() {
                st.push(&chunk);
                fed += 441;
            }
            mix(core::iter::once(&mut st), &mut out);
            if st.underruns > 0 {
                break;
            }
            made += 480;
        }
        assert!((47_000..=49_000).contains(&made), "{made}");
    }

    #[test]
    fn volume_scales_and_the_sum_saturates() {
        let mut a = Stream::new(48_000, 1, false).unwrap();
        let mut b = Stream::new(48_000, 1, false).unwrap();
        a.push(&pcm(&[0, 30_000, 30_000]));
        b.push(&pcm(&[0, 30_000, -1000]));
        b.volume = 50;
        let mut out = [0i16; 3];
        mix([&mut a, &mut b].into_iter(), &mut out);
        // one sample late; then 30000 + 15000 saturates
        assert_eq!(out, [0, 0, i16::MAX]);
    }

    #[test]
    fn an_underrun_is_counted_once_per_dry_spell() {
        let mut st = Stream::new(48_000, 1, false).unwrap();
        let mut out = [0i16; 4];
        mix(core::iter::once(&mut st), &mut out);
        assert_eq!(st.underruns, 0, "never started: no underrun");
        st.push(&pcm(&[1, 2]));
        mix(core::iter::once(&mut st), &mut out);
        mix(core::iter::once(&mut st), &mut out);
        assert_eq!(st.underruns, 1);
        st.push(&pcm(&[3]));
        mix(core::iter::once(&mut st), &mut out);
        assert_eq!(st.underruns, 2);
    }

    #[test]
    fn a_paused_stream_keeps_its_queue() {
        let mut st = Stream::new(48_000, 1, false).unwrap();
        st.push(&pcm(&[5, 6]));
        st.paused = true;
        let mut out = [9i16; 2];
        assert!(!mix(core::iter::once(&mut st), &mut out));
        assert_eq!(out, [0, 0]);
        assert_eq!(st.queued(), 4);
        assert_eq!(st.underruns, 0);
    }

    #[test]
    fn push_takes_whole_frames_up_to_the_queue() {
        let mut st = Stream::new(8_000, 2, false).unwrap();
        assert_eq!(st.space(), 8_000 * 250 / 1000 * 4);
        assert_eq!(st.push(&[1, 2, 3, 4, 5]), 4); // a frame is 4 bytes
        let big = alloc::vec![0u8; 100_000];
        assert_eq!(st.push(&big), st.cap * 2 - 4);
        assert_eq!(st.space(), 0);
        st.flush();
        assert_eq!(st.queued(), 0);
    }

    #[test]
    fn capture_resamples_to_stereo_and_counts_overruns() {
        let mut st = Stream::new(24_000, 2, true).unwrap();
        capture(core::iter::once(&mut st), &[0, 100, 200, 300, 400, 500]);
        let mut out = [0u8; 64];
        let n = st.pop(&mut out);
        let got: Vec<i16> = out[..n]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b))
            .collect();
        assert_eq!(got, [0, 0, 100, 100, 300, 300]);
        let mic = alloc::vec![7i16; 48_000];
        capture(core::iter::once(&mut st), &mic);
        assert_eq!(st.overruns, 1);
        assert_eq!(st.queued(), st.cap * 2);
    }

    #[test]
    fn the_tone_is_a_sine_with_fades() {
        let t = tone(1000, 100, 48_000, 1, 16_000);
        let s: Vec<i16> = t
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b))
            .collect();
        assert_eq!(s.len(), 4800);
        assert_eq!(s[0], 0);
        let peak = s.iter().map(|v| v.unsigned_abs()).max().unwrap();
        assert!((15_500..=16_100).contains(&peak), "{peak}");
        // 1 kHz at 48 kHz: a period is 48 samples, its quarter the peak
        assert!(
            (15_900..=16_100).contains(&s[2400 + 12]),
            "{}",
            s[2400 + 12]
        );
        assert!(s[2400 + 24].abs() < 200);
        let st = tone(500, 20, 8_000, 2, 1000);
        assert_eq!(st.len(), 160 * 4);
    }
}
