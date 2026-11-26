// SPDX-License-Identifier: Apache-2.0
//! AUDIOD.EXE (docs hardware.md S4): the audio path. It owns APR (the ADSP's ring 2; HWHUB owns the ADSP's boot and its
//! QRTR ring 1), the AudioMem periods, the codecs (HAL Audio: routes, amps, mic, the 3.5 mm jack) and serves NeoAudio.
//! MODEMD says what the phone wants (a call's voice path, the ringtone, Dev tools' test) with NeoAudio's op::CONTROL
//! and gets a report back; the PCM never leaves this process. Without the ADSP (Syborg) media plays on the clock sink.
#![no_std]
extern crate alloc;

mod adsp;
mod sound;

use alloc::vec;
use alloc::vec::Vec;
use apr::Mode;
use core::pin::pin;
use e32::rt::{self, Either};
use e32::server::{self, Server};
use e32::{dprintln, time};
use neoaudio::control::{Intent, Report};

const LOOP_MS: u64 = 10;

struct Daemon {
    audio: adsp::CallAudio,
    sound: sound::Sound,
    /// APR receive scratch (8 KB: not on the stack)
    rx: Vec<u8>,
    intent: Intent,
    /// the voice path or a codec control failed for good (until MODEMD's next epoch)
    failed: bool,
    ring_error: i32,
    /// a failed ringtone preparation is retried from here (ms)
    ring_retry_at: u64,
    audio_seen: Option<(Mode, Mode, bool)>,
    /// the last amp protection poll (ms)
    amps_checked: u64,
    /// the last jack poll (ms), and whether the codecs were probed for it
    jack_checked: u64,
    jack_probed: bool,
}

impl Daemon {
    /// MODEMD's latest intent: a new epoch stops everything first; the in-call buttons reach a running call now.
    fn take_intent(&mut self) {
        let Some(i) = self.sound.intent.take() else {
            return;
        };
        if i.epoch != self.intent.epoch {
            self.audio.apr.set(Mode::Off);
            self.audio.test = None;
            self.failed = false;
            self.ring_error = 0;
        }
        let a = &mut self.audio;
        if a.apr.is() == Mode::Call {
            for (op, now, was) in [
                (5, u32::from(i.muted), u32::from(a.muted)),
                (3, u32::from(i.speaker), u32::from(a.speaker)),
                (4, u32::from(i.volume), u32::from(a.volume)),
            ] {
                if now != was && adsp::audio(op, now) < 0 {
                    // the board driver stops the amps on a failed control: MODEMD rebuilds both processors (C6)
                    a.apr.codec_fault();
                    self.failed = true;
                }
            }
        }
        (a.muted, a.speaker, a.volume) = (i.muted, i.speaker, i.volume);
        let test = match i.test {
            1 if !i.call => Some(true),
            2 if !i.call => Some(false),
            _ => None,
        };
        // the other route while the test runs only switches the amps
        if let (Some(spk), Mode::Test) = (test, a.apr.is())
            && test != a.test
        {
            let _ = adsp::audio(3, spk as u32);
        }
        a.test = test;
        a.ring.volume = i.ring_volume;
        a.ring_tone = i.ring_tone;
        self.intent = i;
    }

    fn step(&mut self) {
        self.take_intent();
        let now = time::millis();
        let i = self.intent;
        let up = i.adsp_up;
        let ring_ready = if i.ring
            && !i.hold
            && self.audio.apr.is() == Mode::Off
            && self.audio.apr.idle()
            && now >= self.ring_retry_at
        {
            match self.audio.prepare_ring() {
                Ok(()) => {
                    self.ring_error = 0;
                    true
                }
                Err(e) => {
                    // MODEMD's vibration still alerts; the tone is tried again in 2 s, not every step
                    self.ring_error = e;
                    self.ring_retry_at = now + 2000;
                    false
                }
            }
        } else {
            // a ring stream plays the buffer as it is: only if it holds what MODEMD wants now
            i.ring && self.audio.prepared == Some((i.ring_tone, i.ring_volume))
        };
        // a ring wanted while another stream (media, or a ring with another tone: a call during a message's chime)
        // holds the speaker: let that one go first, then prepare this one
        let clear_first = i.ring && !i.hold && !ring_ready && self.audio.apr.is() != Mode::Off;
        // NeoAudio's media plays when nothing else needs the speaker; a call, the ringtone or the test preempt it
        let media = self.sound.wants_media(now)
            && match self.audio.prepare_region() {
                Ok(()) => true,
                Err(e) => {
                    dprintln!("[audiod] media: no audio region ({})", e);
                    false
                }
            };
        let wanted = match (up && !i.hold, i.call, self.audio.test, ring_ready) {
            (true, false, _, false) if clear_first => Mode::Off,
            (false, _, _, _) => Mode::Off,
            (true, true, _, _) => Mode::Call,
            (true, false, _, true) => Mode::Ring,
            (true, false, Some(_), false) => Mode::Test,
            (true, false, None, false) if media && self.sound.wants_record() => Mode::Duplex,
            (true, false, None, false) if media => Mode::Media,
            (true, false, None, false) => Mode::Off,
        };
        self.sound.preempted = media && !matches!(wanted, Mode::Media | Mode::Duplex);
        let seen = (wanted, self.audio.apr.is(), self.audio.apr.quiescent());
        if self.audio_seen != Some(seen) {
            dprintln!(
                "[audiod] t={}ms audio wanted {:?} current {:?} quiescent {}",
                now,
                seen.0,
                seen.1,
                seen.2
            );
            self.audio_seen = Some(seen);
        }
        self.audio.apr.set(wanted);
        if up {
            self.audio.pump(&mut self.rx);
        }
        if self.audio.apr.failed_hard() {
            self.failed = true;
        }
        self.sound_step(now, up);
        self.sound.report = Report {
            call: self.audio.apr.is() == Mode::Call,
            call_running: self.audio.apr.is() == Mode::Call && self.audio.apr.running(),
            failed: self.failed,
            ring_error: self.ring_error,
            epoch: self.intent.epoch,
        };
    }

    /// NeoAudio's step: route/volume changes reach a running media path, then the mix goes to the ADSP's periods
    /// (media runs) or the clock sink.
    fn sound_step(&mut self, now: u64, up: bool) {
        let a = &mut self.audio;
        let playing = up && matches!(a.apr.is(), Mode::Media | Mode::Duplex) && a.apr.running();
        if self.sound.changed {
            self.sound.changed = false;
            a.media_route = match self.sound.route {
                neoaudio::route::EARPIECE => 0,
                neoaudio::route::BOTTOM => 2,
                _ => 1,
            };
            a.media_volume = self.sound.master;
            // the mic choice is the kernel's own state: set it now, running or not (a call keeps DMIC1)
            if a.apr.is() != Mode::Call {
                let r = adsp::audio(6, self.sound.mic as u32);
                dprintln!("[audiod] media: mic DMIC{} -> {}", self.sound.mic + 1, r);
            }
            if playing && !a.apr.jack_is() {
                let r = adsp::audio(3, a.media_route);
                let v = adsp::audio(4, u32::from(a.media_volume));
                dprintln!(
                    "[audiod] media: route {} volume {} -> {} {}",
                    a.media_route,
                    a.media_volume,
                    r,
                    v
                );
            }
        }
        // the 3.5 mm jack (A8): the kernel debounces the PM660L's detection; media follows the headphones
        if up && now >= self.jack_checked + 250 {
            self.jack_checked = now;
            if !self.jack_probed {
                self.jack_probed = true;
                let r = adsp::audio(0, 0);
                dprintln!("[audiod] audio: codecs probed for the jack -> {}", r);
            }
            let j = adsp::audio(8, 0);
            if j >= 0 {
                a.apr.headset = j & 2 != 0;
            }
            if j >= 0 && (j & 1 != 0) != a.apr.jack {
                a.apr.jack = j & 1 != 0;
                dprintln!(
                    "[audiod] audio: headphones {} ({:#x}: {})",
                    if a.apr.jack { "in" } else { "out" },
                    j,
                    if j & 2 != 0 { "headset mic" } else { "no mic" }
                );
            }
        }
        // amp protection (A7): the kernel polls the amps' IRQ lines; a protection event switches that amp off
        if up && a.apr.running() && now >= self.amps_checked + 250 {
            self.amps_checked = now;
            let r = adsp::audio(7, 0);
            if r > 0 {
                dprintln!(
                    "[audiod] audio: amp protection event (mask {}): that amp is off until the next start",
                    r
                );
            }
        }
        let starting = up
            && !playing
            && matches!(a.apr.wanted(), Mode::Media | Mode::Duplex)
            && !a.apr.failed_hard();
        self.sound
            .step(now, if playing { Some(a) } else { None }, starting);
    }
}

fn main() -> i32 {
    let server = match Server::create(neoaudio::NAME) {
        Ok(s) => s,
        Err(e) => {
            dprintln!("[audiod] NeoAudio name taken ({}): exiting", e);
            return 0;
        }
    };
    dprintln!("[audiod] start: serving NeoAudio");
    let mut d = Daemon {
        audio: adsp::CallAudio::new(),
        sound: sound::Sound::new(),
        rx: vec![0u8; 0x2000],
        intent: Intent::default(),
        failed: false,
        ring_error: 0,
        ring_retry_at: 0,
        audio_seen: None,
        amps_checked: 0,
        jack_checked: 0,
        jack_probed: false,
    };
    rt::block_on(async {
        let mut receive = pin!(server.receive());
        let mut next = time::millis();
        loop {
            let now = time::millis();
            if now >= next {
                d.step();
                // a late step doesn't queue up: the next one is a whole period after this one
                next = now.max(next) + LOOP_MS;
            }
            let wait_us = (next.saturating_sub(time::millis()) * 1000).max(1) as i32;
            match rt::select(receive.as_mut(), time::delay(wait_us)).await {
                Either::Left(Ok(msg)) => {
                    receive.set(server.receive());
                    let _ = server::dispatch(&mut d.sound, msg);
                }
                Either::Left(Err(e)) => {
                    dprintln!("[audiod] receive failed ({})", e);
                    receive.set(server.receive());
                }
                Either::Right(()) => {}
            }
        }
    })
}

e32::main!(main);
