// SPDX-License-Identifier: Apache-2.0
//! MODEMD.EXE (C5, C6, C7): cellular policy over NeoHub, and NeoModem.
//! HWHUB owns QRTR and processor boot/recovery; AUDIOD owns APR and NeoAudio (S4): MODEMD tells it what the phone
//! wants (the call's voice path, the ringtone, Dev tools' test) over NeoAudio's op::CONTROL.
//! SYSSTART starts MODEMD, which starts HWHUB and AUDIOD while serving NeoModem.
#![no_std]

extern crate alloc;

mod audio;
mod client;

use alloc::vec;
use alloc::vec::Vec;
use client::{Hal, Modem};
use core::ops::ControlFlow;
use core::pin::pin;
use e32::rt::{self, Either};
use e32::server::{self, Message, Server, Service};
use e32::sys::err;
use e32::sys::ipc::TVersion;
use e32::{dprintln, time};
use hub_ipc::{Event, Snapshot};
use modem_ipc::{self, Status, op};
use sys::{HAPTIC, Io, hal_io, save_private};

const NOT_READY: i32 = -18;
/// NEOPHONE, NEOCONT, NEOMSG (NeoSymbian/phone) and NEOSET (neoui's Settings): who may clear the call log or every SMS.
const PHONE_SID: u32 = 0xE000_5001;
const CONTACTS_SID: u32 = 0xE000_5002;
const MESSAGES_SID: u32 = 0xE000_5003;
const NEOSET_SID: u32 = 0xE000_2007;
const LOOP_US: i32 = 10_000;

#[derive(Clone, Copy)]
enum Phase {
    Starting,
    Running,
    Recover { watchdog: Option<i32> },
    Failed,
    Disabled,
    Absent,
}

struct Daemon {
    phase: Phase,
    /// (SE2b) the ringtone and the message tone (`modem_ipc::tone`); RING_SETTINGS changes them only when it carries
    /// them, saved with the ring settings
    tones: (u8, u8),
    modem: Option<Modem>,
    /// before the modem exists (booting / failed)
    status: Status,
    /// Boot and recovery facts belong to HWHUB.
    hub: Snapshot,
    next_connect: u64,
    connect_backoff: u64,
    spawned: bool,
    /// AUDIOD: the call's audio path and the in-call buttons' settings (C6)
    audio: audio::Link,
    /// a keypad tone ends here (ms): one tap = one 200 ms tone
    dtmf_stop: Option<u64>,
    /// NOTIFY requests: session, message, (2.9) extended reply
    subscribers: Vec<(u32, Message, bool)>,
    /// (D11) the one SET_PIN_PROTECTION / CHANGE_PIN waiting for the card: its session and message
    pin_msg: Option<(u32, Message)>,
    /// (D11) the card epoch, modem generation and the card's tries of the last extended status each session was given
    seen: Vec<(u32, u32, u32, u8)>,
    next_session: u32,
    published: Status,
    haptic_at: u64,
    alert_dismissed: Option<u8>,
    /// the call Silence quietened (op::SILENCE): no tone or vibration until it stops ringing; the settings stay
    silenced: Option<u8>,
    waiting_alert: Option<u8>,
    haptic_error: i32,
    /// a new message's tone plays until here (ms), its second buzz comes here (0 = none)
    sms_tone_until: u64,
    sms_buzz_at: u64,
    /// the last ringtone error AUDIOD reported (NeoModem's result once)
    ring_error: i32,
    /// settings.bin couldn't be read at boot (an I/O error, not its absence): it isn't overwritten until a read
    /// resolves it, tried again here (ms); what changed meanwhile (airplane, ring and tones, C8) wins over the file
    settings_unread: Option<u64>,
    changed: Changed,
}

/// What a request changed since boot (only consulted while settings.bin is still unread).
#[derive(Clone, Copy, Default)]
struct Changed {
    airplane: bool,
    ring: bool,
    c8: bool,
}

const SETTINGS_PATH: &str = "C:\\private\\e0003001\\settings.bin";

impl Daemon {
    fn status(&self) -> Status {
        let mut s = self.modem.as_ref().map_or(self.status, |m| m.status);
        s.audio = self.audio.status();
        s.revision = self.published.revision;
        match self.phase {
            Phase::Recover { watchdog, .. } => {
                s.modem = if let Some(error) = watchdog {
                    s.result = error;
                    modem_ipc::modem::FAILED
                } else {
                    modem_ipc::modem::RECOVERING
                };
                s.reg = modem_ipc::reg::NONE;
                s.bars = 0;
            }
            Phase::Failed => s.modem = modem_ipc::modem::FAILED,
            _ => {}
        }
        s
    }

    fn publish(&mut self) {
        // (D11) a PIN request the card answered (and was read again), or whose answer was lost
        if let Some(r) = self.modem.as_mut().and_then(|m| m.take_pin_done()) {
            dprintln!("[modemd] SIM PIN request finished ({})", r);
            if let Some((_, msg)) = self.pin_msg.take() {
                msg.complete(r);
            }
        }
        let mut next = self.status();
        if next == self.published {
            return;
        }
        next.revision = self.published.revision.wrapping_add(1);
        self.published = next;
        for (session, msg, ext) in core::mem::take(&mut self.subscribers) {
            let r = self.write_status(session, &msg, &next, ext);
            msg.complete(r);
        }
    }

    /// STATUS/NOTIFY's reply: 533 bytes, or (2.9) with the protection, remembered as seen by that session.
    fn write_status(&mut self, session: u32, msg: &Message, s: &Status, ext: bool) -> i32 {
        if !ext {
            return msg.write8(0, &s.to_bytes(), 0).map_or_else(|e| e, |_| 0);
        }
        match msg.write8(0, &s.to_ext_bytes(), 0) {
            Ok(()) => {
                self.seen.retain(|(id, ..)| *id != session);
                self.seen
                    .push((session, s.protection.epoch, s.generation, s.pin_attempts));
                0
            }
            Err(e) => e,
        }
    }

    /// (2.9) NOTIFY/STATUS arg 2 asks for the extended reply; its buffer must hold it before anything is queued.
    fn extended(msg: &Message) -> Result<bool, i32> {
        if msg.int(2) != modem_ipc::pin::EXTENDED {
            return Ok(false);
        }
        match msg.des_max_len(0) {
            Ok(n) if n >= Status::EXT_SIZE => Ok(true),
            Ok(_) => Err(err::KERR_OVERFLOW),
            Err(e) => Err(e),
        }
    }

    /// (D11) SET_PIN_PROTECTION / CHANGE_PIN from NEOSET (checked before this): only against the card and modem
    /// this session last saw, one at a time, completed when the card has answered. The PINs live in fixed buffers
    /// that are wiped whatever happens, and are never logged.
    fn pin_request(&mut self, session: u32, msg: Message) {
        let (epoch, generation) = (msg.int(2) as u32, msg.int(3) as u32);
        let seen = self
            .seen
            .iter()
            .find(|v| (v.0, v.1, v.2) == (session, epoch, generation))
            .map(|v| v.3);
        let r = if self.pin_msg.is_some() {
            err::KERR_IN_USE
        } else if let Some(tries) = seen {
            let (mut old, mut new) = ([0u8; 8], [0u8; 8]);
            let r = match (msg.function(), read_pin(&msg, 0, &mut old)) {
                (_, Err(e)) => e,
                (op::SET_PIN_PROTECTION, Ok(n)) => self.modem.as_mut().map_or(NOT_READY, |m| {
                    let on = msg.int(1) != 0;
                    m.pin_protection(&mut Hal, &old[..n], on, epoch, generation, tries)
                        .err()
                        .unwrap_or(0)
                }),
                (_, Ok(n)) => match read_pin(&msg, 1, &mut new) {
                    Err(e) => e,
                    Ok(k) => self.modem.as_mut().map_or(NOT_READY, |m| {
                        m.change_pin(&mut Hal, &old[..n], &new[..k], epoch, generation, tries)
                            .err()
                            .unwrap_or(0)
                    }),
                },
            };
            hub_ipc::wipe(&mut old);
            hub_ipc::wipe(&mut new);
            r
        } else {
            modem_ipc::pin::STALE
        };
        dprintln!("[modemd] SIM PIN request {} -> {}", msg.function(), r);
        if r == 0 {
            self.pin_msg = Some((session, msg));
        } else {
            msg.complete(r);
        }
    }

    fn cancel_notify(&mut self, id: u32) {
        if let Some(i) = self.subscribers.iter().position(|(s, ..)| *s == id) {
            self.subscribers.remove(i).1.complete(-3);
        }
    }

    fn clear_audio(&mut self) {
        self.dtmf_stop = None;
        // Stop alerts before acknowledging a processor reset (C6).
        // SAFETY: HAPTIC uses scalar fields and no buffer.
        let haptic = unsafe { hal_io(HAPTIC, Io::default()) }.0;
        if haptic < 0 {
            dprintln!("[modemd] recovery vibration stop failed ({})", haptic);
        }
        self.audio.clear();
        self.sms_tone_until = 0;
        self.sms_buzz_at = 0;
        self.haptic_at = 0;
        self.alert_dismissed = None;
        self.silenced = None;
        self.waiting_alert = None;
    }

    fn recover(&mut self, mask: u32, reason: i32) {
        if matches!(self.phase, Phase::Recover { .. }) {
            return;
        }
        self.clear_audio();
        if let Some(m) = &self.modem {
            dprintln!(
                "[modemd] asking NeoHub for a recovery: {}",
                hub_ipc::reason::name(reason)
            );
            let r = m.recover(mask, reason);
            if r < 0 {
                dprintln!("[modemd] NeoHub recovery request failed ({})", r);
            } else {
                self.phase = Phase::Recover { watchdog: None };
            }
        }
    }

    fn snapshot(&mut self, s: Snapshot) {
        if self
            .modem
            .as_ref()
            .is_some_and(|m| (s.generation.wrapping_sub(m.status.generation) as i32) < 0)
        {
            return; // A pre-attach queued snapshot must not roll back a live generation.
        }
        if self
            .modem
            .as_ref()
            .is_some_and(|m| m.status.generation != s.generation)
        {
            self.reset_client(s.generation, hub_ipc::Subsystem::All);
        }
        if let Some(m) = &mut self.modem {
            m.event(Event::State(s.state));
        }
        self.hub = s;
        if s.phase == hub_ipc::phase::RUNNING
            && !matches!(self.phase, Phase::Running)
            && let Some(m) = &mut self.modem
        {
            m.rearm_watchdog(); // it counts from Running, not from the boot before it
        }
        self.phase = match s.phase {
            hub_ipc::phase::RUNNING => Phase::Running,
            hub_ipc::phase::RECOVER => Phase::Recover {
                watchdog: (s.recovery_error != 0).then_some(s.recovery_error),
            },
            hub_ipc::phase::FAILED => Phase::Failed,
            hub_ipc::phase::DISABLED => Phase::Disabled,
            hub_ipc::phase::ABSENT => Phase::Absent,
            _ => Phase::Starting,
        };
    }

    fn reset_client(&mut self, generation: u32, subsystem: hub_ipc::Subsystem) {
        self.clear_audio();
        // MSS-only recovery leaves the ADSP's APR handles and published tickets alive.
        // Retain their owner until Off teardown completes; a Reset is no shutdown proof.
        if let Some(m) = &mut self.modem {
            m.event(Event::Reset {
                subsystem,
                generation,
            });
            let r = m.quiesced(generation);
            if r < 0 {
                dprintln!("[modemd] NeoHub QUIESCED failed ({})", r);
            }
        }
    }

    fn hub_events(&mut self, bytes: &[u8]) {
        let Some(events) = hub_ipc::decode_batch(bytes) else {
            dprintln!("[modemd] malformed NeoHub event batch");
            return;
        };
        let mut dropped = false;
        for e in events {
            match e {
                Event::Status(s) => self.snapshot(s),
                Event::Dropped(n) => {
                    dropped = true;
                    dprintln!("[modemd] NeoHub dropped {} events", n);
                }
                Event::Reset {
                    generation,
                    subsystem,
                } => {
                    if self
                        .modem
                        .as_ref()
                        .is_some_and(|m| (generation.wrapping_sub(m.status.generation) as i32) < 0)
                    {
                        continue;
                    }
                    if self
                        .modem
                        .as_ref()
                        .is_some_and(|m| m.status.generation != generation)
                    {
                        self.reset_client(generation, subsystem);
                    } else if let Some(m) = &self.modem {
                        let _ = m.quiesced(generation);
                    }
                }
                _ => {
                    if let Some(m) = &mut self.modem {
                        m.event(e);
                    }
                }
            }
        }
        if dropped {
            // A Reset and its Status can both be evicted while the client stalls.
            if let Some(Ok(s)) = self.modem.as_ref().map(Modem::snapshot) {
                self.snapshot(s);
            }
        }
    }

    fn connect_hub(&mut self) {
        let now = time::millis();
        if self.modem.as_ref().is_some_and(|m| m.session().is_some()) || now < self.next_connect {
            return;
        }
        match client::connect() {
            Ok(session) => {
                if let Some(m) = &mut self.modem {
                    match m.attach(session) {
                        Ok(s) => {
                            self.snapshot(s);
                            self.connect_backoff = 100;
                            dprintln!("[modemd] connected to NeoHub");
                            return;
                        }
                        Err(e) => dprintln!("[modemd] NeoHub attach failed ({})", e),
                    }
                }
            }
            Err(e) => {
                if e == err::KERR_NOT_FOUND && !self.spawned {
                    match e32::process::Process::spawn("HWHUB.EXE", "") {
                        Ok(p) => {
                            p.resume();
                            self.spawned = true;
                            dprintln!("[modemd] HWHUB started");
                        }
                        Err(r) => dprintln!("[modemd] HWHUB spawn failed ({})", r),
                    }
                } else if e != err::KERR_NOT_FOUND {
                    dprintln!("[modemd] NeoHub connect failed ({})", e);
                }
            }
        }
        self.next_connect = now.saturating_add(self.connect_backoff);
        self.connect_backoff = (self.connect_backoff * 2).min(2000);
    }

    fn hub_lost(&mut self, error: i32) {
        dprintln!("[modemd] NeoHub disconnected ({})", error);
        self.clear_audio();
        if let Some(m) = &mut self.modem {
            m.detach();
        }
        self.phase = Phase::Starting;
        // A lost hub session does not acknowledge an ADSP stop: keep draining APR Off.
        self.next_connect = time::millis() + 100;
        self.spawned = false;
    }

    /// settings.bin: the C8 record (C7's plus caller ID and the log's days); C7 files still load.
    fn save_settings(&mut self) -> Result<(), i32> {
        // never over a file that couldn't be read: the request stays in memory, the caller tries again
        if self.settings_unread.is_some() {
            self.load_settings()?;
        }
        let s = self.status();
        let (clir, days) = self
            .modem
            .as_ref()
            .map_or((0, 30), |m| (m.clir as u8, m.log.days));
        // C8, plus the tones' byte (SE2b) once a tone isn't the built-in one: an older MODEMD reads 9 bytes, so it
        // keeps reading the file;
        // written aside and renamed, so a cut mid-save keeps the last good file
        let record = modem_ipc::saved::encode(&modem_ipc::saved::Saved {
            airplane: s.airplane,
            ring: s.ring,
            c8: Some((clir, days)),
            // (2.8) the user's own sounds as the built-in tone: a 2.7 MODEMD refuses the record otherwise; the shell
            // sends them again after a restart
            tones: Some(modem_ipc::tone::persisted(self.tones)),
        });
        sys::replace_private(
            "C:\\private\\e0003001\\",
            "settings.bin",
            &record.0[..record.1],
        )
    }

    /// calls.db, when the log changed (C8).
    fn save_log(&mut self) {
        let Some(m) = &mut self.modem else { return };
        if !m.log.dirty {
            return;
        }
        m.log.dirty = false;
        if let Err(e) = save_private("C:\\private\\e0003001\\", "calls.db", &m.log.encode()) {
            dprintln!("[modemd] calls.db not saved ({})", e);
        }
    }

    /// sms.db, when the messages changed (C9); then what waited for it (acknowledgements, deletions in the modem).
    fn save_sms(&mut self) {
        let Some(m) = &mut self.modem else { return };
        if m.sms.dirty {
            m.sms.dirty = false;
            if let Err(e) = save_private("C:\\private\\e0003001\\", "sms.db", &m.sms.encode()) {
                // not acknowledged: the network sends it again
                dprintln!("[modemd] sms.db not saved ({})", e);
                m.sms.dirty = true;
                return;
            }
        }
        m.sms_saved(&mut Hal);
    }

    /// block.db: the blocklist with the known-number hashes (phone_rules::block, "NBL1").
    fn save_block(&self) -> i32 {
        let Some(m) = &self.modem else {
            return NOT_READY;
        };
        save_private("C:\\private\\e0003001\\", "block.db", &m.block.encode(true))
            .err()
            .unwrap_or(0)
    }

    /// settings.bin onto what is in memory, but what a request already changed. `Err`: unreadable still.
    fn load_settings(&mut self) -> Result<(), i32> {
        let read = sys::load_private(SETTINGS_PATH, 16);
        let saved = modem_ipc::saved::resolve(read.as_deref().map_err(|e| *e))?;
        self.settings_unread = None;
        let Some(saved) = saved else { return Ok(()) };
        let c = self.changed;
        if !c.airplane {
            self.status.airplane = saved.airplane;
        }
        if !c.ring {
            self.status.ring = saved.ring;
            if let Some(t) = saved.tones {
                self.tones = t;
            }
            if let Some(m) = &mut self.modem {
                m.status.ring = saved.ring;
            }
        }
        if let (false, Some((clir, days)), Some(m)) = (c.c8, saved.c8, &mut self.modem) {
            m.clir = i32::from(clir);
            m.log.days = days;
        }
        Ok(())
    }

    fn step(&mut self) {
        if self.settings_unread.is_some_and(|at| time::millis() >= at) {
            match self.load_settings() {
                Ok(()) => dprintln!("[modemd] settings.bin read"),
                Err(_) => self.settings_unread = Some(time::millis() + 2_000),
            }
        }
        self.connect_hub();
        self.run();
        // AUDIOD runs on every machine (NeoAudio's clock sink without an ADSP), so it is started and fed always
        self.audio.intent.adsp_up = self.hub.adsp == hub_ipc::adsp::UP;
        self.audio.sync();
        self.save_log();
        self.save_sms();
    }

    /// Dev tools' audio test (no call): 1 loudspeaker, 2 earpiece, the mic looped back to the amps; 0 stops it.
    /// AUDIOD runs it; pressing the other route while it runs only switches the amps.
    fn audio_test(&mut self, what: i32) -> i32 {
        if !(self.hub.adsp == hub_ipc::adsp::UP) {
            return NOT_READY;
        }
        if self.talking() {
            return err::KERR_IN_USE;
        }
        if !(0..=2).contains(&what) {
            return err::KERR_ARGUMENT;
        }
        dprintln!("[modemd] audio test {}", what);
        self.audio.intent.test = what as u8;
        err::KERR_NONE
    }

    /// A call is connected (its audio runs or is wanted).
    fn talking(&self) -> bool {
        self.modem.as_ref().is_some_and(|m| {
            m.status
                .calls
                .as_slice()
                .iter()
                .any(|c| !matches!(c.state, modem_ipc::call::NONE | modem_ipc::call::END))
                || call_audio(m.status.call.state, self.audio.is_call())
        })
    }

    /// Cellular and Router have independent 10 ms clocks across NeoHub. HWHUB alone
    /// retries its ordered outbox; MODEMD runs begin_tick/tick/poll on its own deadline.
    fn run(&mut self) {
        if matches!(self.phase, Phase::Recover { .. })
            || !self.modem.as_ref().is_some_and(|m| m.session().is_some())
        {
            self.audio.intent.hold = true;
            return;
        }
        if matches!(self.phase, Phase::Absent) {
            return;
        }
        // an audio (ADSP) failure no longer restarts the modem: AUDIOD retries, NeoHub keeps the modem up and the
        // ADSP state shows as failed (docs hardware/modem.md C7, Linux-grade recovery)
        let running = matches!(self.phase, Phase::Running);
        let Some(modem) = &mut self.modem else { return };
        // the audio follows what is wanted now: a connected call's voice path, else the test, else nothing (the
        // state machine finishes a running sequence first)
        let talking = if modem.status.calls.len > 0 {
            modem
                .status
                .calls
                .as_slice()
                .iter()
                .any(|c| call_audio(c.state, self.audio.is_call()))
        } else {
            call_audio(modem.status.call.state, self.audio.is_call())
        };
        if self.alert_dismissed.is_some_and(|id| {
            !modem.status.calls.as_slice().iter().any(|c| {
                c.id == id
                    && matches!(
                        c.state,
                        modem_ipc::call::INCOMING | modem_ipc::call::WAITING
                    )
            })
        }) || (!modem.status.pending && modem.status.result < 0)
        {
            self.alert_dismissed = None;
        }
        if self.silenced.is_some_and(|id| {
            !modem.status.calls.as_slice().iter().any(|c| {
                c.id == id
                    && matches!(
                        c.state,
                        modem_ipc::call::INCOMING | modem_ipc::call::WAITING
                    )
            })
        }) {
            self.silenced = None;
        }
        let ringing = modem.status.calls.as_slice().iter().any(|c| {
            c.incoming
                && c.state == modem_ipc::call::INCOMING
                && self.alert_dismissed != Some(c.id)
                && self.silenced != Some(c.id)
        });
        let now = time::millis();
        if ringing && !talking && !modem.status.airplane && modem.status.ring.vibrate {
            if now >= self.haptic_at {
                // SAFETY: HAPTIC uses scalar duration/amplitude fields and no buffer.
                let r = unsafe {
                    hal_io(
                        HAPTIC,
                        Io {
                            a: 700,
                            b: 70,
                            ..Io::default()
                        },
                    )
                }
                .0;
                if self.haptic_at == 0 || r != self.haptic_error {
                    dprintln!("[modemd] t={}ms incoming vibration start -> {}", now, r);
                    self.haptic_error = r;
                }
                self.haptic_at = now + 1500;
            }
        } else if self.haptic_at != 0 {
            // SAFETY: HAPTIC uses scalar duration/amplitude fields and no buffer.
            let r = unsafe { hal_io(HAPTIC, Io::default()) }.0;
            dprintln!("[modemd] t={}ms incoming vibration stop -> {}", now, r);
            self.haptic_error = r;
            self.haptic_at = 0;
        }
        let waiting = modem
            .status
            .calls
            .as_slice()
            .iter()
            .find(|c| c.state == modem_ipc::call::WAITING)
            .map(|c| c.id);
        if waiting != self.waiting_alert {
            self.waiting_alert = waiting;
            if waiting.is_some() && talking && !modem.status.airplane && modem.status.ring.vibrate {
                // SAFETY: HAPTIC uses scalar duration/amplitude fields and no buffer.
                let r = unsafe {
                    hal_io(
                        HAPTIC,
                        Io {
                            a: 150,
                            b: 60,
                            ..Io::default()
                        },
                    )
                }
                .0;
                if r < 0 {
                    dprintln!("[modemd] waiting-call vibration failed ({})", r);
                }
            }
        }
        // a new message (C9): two short buzzes and/or a short tone, never during a call or while it rings
        if modem.sms.alert {
            modem.sms.alert = false;
            let mode = modem.sms.settings.alert;
            if !talking
                && !ringing
                && !modem.status.airplane
                && mode != modem_ipc::sms::ALERT_SILENT
            {
                // SAFETY: HAPTIC uses scalar duration/amplitude fields and no buffer.
                let r = unsafe {
                    hal_io(
                        HAPTIC,
                        Io {
                            a: 150,
                            b: 60,
                            ..Io::default()
                        },
                    )
                }
                .0;
                if r < 0 {
                    dprintln!("[modemd] message vibration failed ({})", r);
                }
                self.sms_buzz_at = now + 300;
                if mode == modem_ipc::sms::ALERT_TONE_VIBRATE
                    && !modem.status.ring.silent
                    && modem.status.ring.volume > 0
                {
                    self.sms_tone_until = now + 800;
                }
            }
        }
        if self.sms_buzz_at != 0 && now >= self.sms_buzz_at {
            self.sms_buzz_at = 0;
            // SAFETY: HAPTIC uses scalar duration/amplitude fields and no buffer.
            let _ = unsafe {
                hal_io(
                    HAPTIC,
                    Io {
                        a: 150,
                        b: 60,
                        ..Io::default()
                    },
                )
            };
        }
        let chime = now < self.sms_tone_until && !talking && !modem.status.airplane;
        if talking {
            self.audio.intent.test = 0;
        }
        // the message tone is a tone's start, cut short; AUDIOD prepares and plays it. A tone of NONE: no sound (a
        // call still vibrates, a message still buzzes)
        let (ring_tone, message_tone) = self.tones;
        let ring_sound = ringing
            && !talking
            && !modem.status.airplane
            && !modem.status.ring.silent
            && modem.status.ring.volume > 0
            && ring_tone != modem_ipc::tone::NONE;
        let chime = chime && message_tone != modem_ipc::tone::NONE;
        let i = &mut self.audio.intent;
        i.hold = false;
        i.call = talking;
        i.ring = ring_sound || chime;
        i.ring_tone = if ring_sound { ring_tone } else { message_tone };
        i.ring_volume = modem.status.ring.volume;
        let e = self.audio.report.ring_error;
        if e != self.ring_error {
            self.ring_error = e;
            if e < 0 {
                // the vibration still alerts; AUDIOD retries the tone
                modem.status.result = e;
            }
        }
        if self.dtmf_stop.is_some_and(|t| time::millis() >= t) {
            self.dtmf_stop = None;
            let _ = modem.dtmf(&mut Hal, None);
        }
        let now = time::millis();
        modem.tick(&mut Hal, now);
        if running && modem.watchdog() {
            self.recover(1, hub_ipc::reason::WATCHDOG);
        }
    }
}

impl Service for Daemon {
    type Session = u32;

    fn connect(&mut self, v: TVersion) -> Result<u32, i32> {
        if matches!(self.phase, Phase::Absent) {
            return Err(err::KERR_NOT_FOUND); // as if there were no NeoModem server
        }
        if v.major as i8 == modem_ipc::VERSION.0 {
            self.next_session = self.next_session.wrapping_add(1).max(1);
            Ok(self.next_session)
        } else {
            Err(err::KERR_NOT_SUPPORTED)
        }
    }

    fn message(&mut self, session: &mut u32, msg: Message) {
        // the bulk deletes only for the apps whose data it is, and the Settings app's Clear data (SE4a)
        let bulk = match msg.function() {
            op::CLEAR_LOG => Some(&[PHONE_SID, CONTACTS_SID, MESSAGES_SID, NEOSET_SID][..]),
            op::SMS_EDIT if msg.int(0) == modem_ipc::sms::EDIT_DELETE_ALL => {
                Some(&[MESSAGES_SID, NEOSET_SID][..])
            }
            _ => None,
        };
        // (D11) the SIM PIN's protection and a new PIN: NEOSET only, checked before either PIN is read
        let bulk = match msg.function() {
            op::SET_PIN_PROTECTION | op::CHANGE_PIN => Some(&[NEOSET_SID][..]),
            _ => bulk,
        };
        if let Some(allowed) = bulk
            && !allowed.contains(&msg.client_sid())
        {
            dprintln!(
                "[modemd] {} refused to {:08x}",
                msg.function(),
                msg.client_sid()
            );
            msg.complete(err::KERR_PERMISSION_DENIED);
            return;
        }
        if msg.function() == op::NOTIFY {
            if self.subscribers.iter().any(|(s, ..)| s == session) || self.subscribers.len() >= 16 {
                msg.complete(err::KERR_IN_USE);
                return;
            }
            let ext = match Self::extended(&msg) {
                Ok(ext) => ext,
                Err(e) => {
                    msg.complete(e);
                    return;
                }
            };
            if msg.int(1) as u32 == self.published.revision {
                self.subscribers.push((*session, msg, ext));
                return;
            }
            let published = self.published;
            let r = self.write_status(*session, &msg, &published, ext);
            msg.complete(r);
            return;
        }
        let audio_op = matches!(
            msg.function(),
            op::MUTE | op::SPEAKER | op::VOLUME | op::AUDIO_TEST
        );
        if !matches!(
            msg.function(),
            op::STATUS
                | op::CANCEL_NOTIFY
                | op::RETRY
                | op::RING_SETTINGS
                | op::AIRPLANE
                | op::CALL_LOG
                | op::CLEAR_LOG
                | op::CALL_SETTINGS
                | op::BLOCK_GET
                | op::BLOCK_SET
                | op::KNOWN_SET
                | op::SMS_SEND
                | op::SMS_READ
                | op::SMS_EDIT
                | op::SMS_SETTINGS
        ) && !matches!(self.phase, Phase::Running)
            && !(audio_op && (self.hub.adsp == hub_ipc::adsp::UP))
        {
            msg.complete(NOT_READY);
            return;
        }
        if matches!(msg.function(), op::SET_PIN_PROTECTION | op::CHANGE_PIN) {
            self.pin_request(*session, msg);
            return;
        }
        let target = msg.int(0);
        if matches!(msg.function(), op::ANSWER | op::HANGUP | op::SILENCE)
            && !(0..=255).contains(&target)
        {
            msg.complete(err::KERR_ARGUMENT);
            return;
        }

        let r = match msg.function() {
            op::STATUS => match Self::extended(&msg) {
                Ok(ext) => {
                    let s = self.status();
                    self.write_status(*session, &msg, &s, ext)
                }
                Err(e) => e,
            },
            op::DIAL => {
                let read = msg.des_len(0).and_then(|n| {
                    if n <= 32 {
                        msg.read8_vec(0)
                    } else {
                        Err(err::KERR_ARGUMENT)
                    }
                });
                let r = match (&mut self.modem, &read) {
                    (Some(m), Ok(n)) => match core::str::from_utf8(n) {
                        Ok(n) => m.dial(&mut Hal, n).err().unwrap_or(err::KERR_NONE),
                        Err(_) => err::KERR_ARGUMENT,
                    },
                    (_, Err(e)) => *e,
                    (None, _) => NOT_READY,
                };
                if r != err::KERR_NONE {
                    // what came in, without the number: its length, digit count and any non-digit bytes
                    let (len, digits, other) = read.as_ref().map_or((0, 0, Vec::new()), |n| {
                        let other: Vec<u8> =
                            n.iter().copied().filter(|c| !c.is_ascii_digit()).collect();
                        (n.len(), n.len() - other.len(), other)
                    });
                    dprintln!(
                        "[modemd] dial refused ({}): read {:?}, {} bytes, {} digits, other {:02x?}",
                        r,
                        read.as_ref().map(|_| ()),
                        len,
                        digits,
                        other
                    );
                }
                r
            }
            // only the call's alert goes quiet (below); the ring settings stay
            op::SILENCE => self.modem.as_ref().map_or(NOT_READY, |_| err::KERR_NONE),
            op::ANSWER => self.modem.as_mut().map_or(NOT_READY, |m| {
                m.answer_call(&mut Hal, target as u8)
                    .err()
                    .unwrap_or(err::KERR_NONE)
            }),
            op::HANGUP => self.modem.as_mut().map_or(NOT_READY, |m| {
                m.end_call(&mut Hal, target as u8)
                    .err()
                    .unwrap_or(err::KERR_NONE)
            }),
            // the in-call buttons (C6): remembered for the next call too, applied now if the audio runs
            op::MUTE => {
                self.audio.intent.muted = msg.int(0) != 0;
                err::KERR_NONE
            }
            op::SPEAKER => {
                self.audio.intent.speaker = msg.int(0) != 0;
                err::KERR_NONE
            }
            op::VOLUME => {
                let want = msg.int(0);
                if !(0..=10).contains(&want) {
                    err::KERR_ARGUMENT
                } else {
                    self.audio.intent.volume = want as u8;
                    err::KERR_NONE
                }
            }
            op::DTMF => {
                let k = msg.int(0);
                let key = (k >= 0).then_some(k as u8);
                let r = self.modem.as_mut().map_or(NOT_READY, |m| {
                    m.dtmf(&mut Hal, key).err().unwrap_or(err::KERR_NONE)
                });
                if r == err::KERR_NONE && key.is_some() {
                    self.dtmf_stop = Some(time::millis() + 200);
                }
                r
            }
            op::AUDIO_TEST => self.audio_test(msg.int(0)),
            op::CANCEL_NOTIFY => {
                self.cancel_notify(*session);
                0
            }
            op::MANAGE => {
                let id = msg.int(1);
                if !(0..=255).contains(&id) {
                    err::KERR_ARGUMENT
                } else {
                    self.modem.as_mut().map_or(NOT_READY, |m| {
                        m.manage(&mut Hal, msg.int(0), id as u8).err().unwrap_or(0)
                    })
                }
            }
            op::AIRPLANE => {
                let r = self.modem.as_mut().map_or(NOT_READY, |m| {
                    m.airplane(&mut Hal, msg.int(0) != 0).err().unwrap_or(0)
                });
                if r == 0 {
                    self.changed.airplane = true;
                    self.save_settings().err().unwrap_or(0)
                } else {
                    r
                }
            }
            op::VERIFY_PIN => match msg.des_len(0).and_then(|n| {
                if (4..=8).contains(&n) {
                    msg.read8_vec(0)
                } else {
                    Err(err::KERR_ARGUMENT)
                }
            }) {
                Ok(mut pin) => {
                    let r = self.modem.as_mut().map_or(NOT_READY, |m| {
                        m.verify_pin(&mut Hal, &pin).err().unwrap_or(0)
                    });
                    pin.fill(0);
                    r
                }
                Err(e) => e,
            },
            // only the shell (any form) and NEOPHONE (without tones): they persist in settings.bin
            op::RING_SETTINGS if !modem_ipc::tone::caller_allowed(msg.client_sid(), msg.int(3)) => {
                dprintln!("[modemd] ring settings refused to {:08x}", msg.client_sid());
                err::KERR_PERMISSION_DENIED
            }
            op::RING_SETTINGS => {
                if !(0..=10).contains(&msg.int(0)) {
                    err::KERR_ARGUMENT
                } else {
                    let ring = modem_ipc::Ring {
                        volume: msg.int(0) as u8,
                        silent: msg.int(1) != 0,
                        vibrate: msg.int(2) != 0,
                    };
                    // an older caller (three arguments: NEOPHONE, a 1.x shell) leaves the tones as they are
                    match modem_ipc::tone::apply(self.tones, msg.int(3)) {
                        Err(_) => err::KERR_ARGUMENT,
                        Ok(tones) => {
                            self.changed.ring = true;
                            self.tones = tones;
                            self.status.ring = ring;
                            if let Some(m) = &mut self.modem {
                                m.status.ring = ring;
                            }
                            self.save_settings().err().unwrap_or(0)
                        }
                    }
                }
            }
            op::RETRY => {
                if !matches!(self.phase, Phase::Failed) {
                    err::KERR_IN_USE
                } else {
                    self.modem
                        .as_ref()
                        .map_or(NOT_READY, |m| m.recover(1, hub_ipc::reason::USER))
                }
            }
            // the call log (C8)
            op::CALL_LOG => match (&self.modem, msg.des_max_len(0)) {
                (Some(m), Ok(room)) => msg
                    .write8(0, &m.log.to_wire(room), 0)
                    .map_or_else(|e| e, |_| 0),
                (None, _) => NOT_READY,
                (_, Err(e)) => e,
            },
            op::CLEAR_LOG => self.modem.as_mut().map_or(NOT_READY, |m| {
                if m.log.clear(msg.int(0) as u32, msg.int(1) as u8) {
                    m.status.log_revision = m.status.log_revision.wrapping_add(1);
                }
                0
            }),
            op::CALL_SETTINGS => {
                let (clir, days) = (msg.int(0), msg.int(1));
                if !(0..=2).contains(&clir) || !matches!(days, 0 | 1 | 10 | 30) {
                    err::KERR_ARGUMENT
                } else if let Some(m) = &mut self.modem {
                    m.clir = clir;
                    if m.log.set_days(days as u8, Hal.utc_ms()) {
                        m.status.log_revision = m.status.log_revision.wrapping_add(1);
                    }
                    self.changed.c8 = true;
                    self.save_settings().err().unwrap_or(0)
                } else {
                    NOT_READY
                }
            }
            op::CALL_WAITING => self.modem.as_mut().map_or(NOT_READY, |m| {
                m.call_waiting(&mut Hal, msg.int(0)).err().unwrap_or(0)
            }),
            // the blocklist (v2.2)
            op::BLOCK_GET => match (&self.modem, msg.des_max_len(0)) {
                (Some(m), Ok(room)) => {
                    let b = m.block.encode(false);
                    if b.len() > room {
                        err::KERR_OVERFLOW
                    } else {
                        msg.write8(0, &b, 0).map_or_else(|e| e, |_| 0)
                    }
                }
                (None, _) => NOT_READY,
                (_, Err(e)) => e,
            },
            op::BLOCK_SET | op::KNOWN_SET => {
                let max = if msg.function() == op::BLOCK_SET {
                    modem_ipc::MAX_BLOCK_BYTES
                } else {
                    modem_ipc::MAX_KNOWN_BYTES
                };
                let read = msg.des_len(0).and_then(|n| {
                    if n <= max {
                        msg.read8_vec(0)
                    } else {
                        Err(err::KERR_ARGUMENT)
                    }
                });
                match (&mut self.modem, read) {
                    (None, _) => NOT_READY,
                    (_, Err(e)) => e,
                    (Some(m), Ok(b)) if msg.function() == op::BLOCK_SET => {
                        match modem::phone_rules::block::List::decode(&b) {
                            Some(list) => {
                                let known = core::mem::take(&mut m.block.known);
                                m.block = list;
                                m.block.known = known;
                                dprintln!(
                                    "[modemd] blocklist: {} rules, private {}, unknown {}",
                                    m.block.rules.len(),
                                    m.block.private,
                                    m.block.unknown
                                );
                                self.save_block()
                            }
                            None => err::KERR_ARGUMENT,
                        }
                    }
                    (Some(m), Ok(b)) => {
                        if b.len() % 4 != 0 {
                            err::KERR_ARGUMENT
                        } else {
                            let keys = b
                                .as_chunks::<4>()
                                .0
                                .iter()
                                .map(|k| u32::from_le_bytes(*k))
                                .collect();
                            m.block.set_known(keys);
                            self.save_block()
                        }
                    }
                }
            }
            // SMS (v2.3)
            op::SMS_SEND => {
                let read = |i: usize, max: usize| {
                    msg.des_len(i).and_then(|n| {
                        if n <= max {
                            msg.read8_vec(i)
                        } else {
                            Err(err::KERR_ARGUMENT)
                        }
                    })
                };
                match (
                    &mut self.modem,
                    read(0, 32),
                    read(1, modem_ipc::sms::MAX_TEXT),
                ) {
                    (None, _, _) => NOT_READY,
                    (_, Err(e), _) | (_, _, Err(e)) => e,
                    (Some(m), Ok(to), Ok(text)) => {
                        match (core::str::from_utf8(&to), core::str::from_utf8(&text)) {
                            (Ok(to), Ok(text)) => m
                                .sms_send(&mut Hal, to, text)
                                .map_or_else(|e| e, |id| id.min(i32::MAX as u32) as i32),
                            _ => err::KERR_ARGUMENT,
                        }
                    }
                }
            }
            op::SMS_READ => match (&self.modem, msg.des_max_len(0)) {
                (Some(m), Ok(room)) => {
                    let mut b = vec![0u8; room.min(64 * 1024)];
                    match m.sms.read(msg.int(1) as u32, msg.int(2), &mut b) {
                        Ok(n) => msg.write8(0, &b[..n], 0).map_or_else(|e| e, |_| 0),
                        Err(e) => e,
                    }
                }
                (None, _) => NOT_READY,
                (_, Err(e)) => e,
            },
            op::SMS_EDIT => self.modem.as_mut().map_or(NOT_READY, |m| {
                m.sms_edit(&mut Hal, msg.int(0), msg.int(1) as u32)
                    .err()
                    .unwrap_or(0)
            }),
            op::SMS_SETTINGS => match &mut self.modem {
                None => NOT_READY,
                Some(m) if msg.int(0) == 0 => msg
                    .write8(1, &m.sms.settings.to_bytes(), 0)
                    .map_or_else(|e| e, |_| 0),
                Some(m) => match msg
                    .read8_vec(1)
                    .ok()
                    .and_then(|b| modem_ipc::SmsSettings::from_bytes(&b))
                {
                    Some(s) => {
                        m.sms_settings(&mut Hal, s);
                        0
                    }
                    None => err::KERR_ARGUMENT,
                },
            },
            _ => err::KERR_NOT_SUPPORTED,
        };
        if r == 0 && msg.function() == op::SILENCE {
            self.silenced = self.modem.as_ref().map(|m| {
                if target == 0 {
                    m.status.call.id
                } else {
                    target as u8
                }
            });
        }
        if r == 0 && matches!(msg.function(), op::ANSWER | op::HANGUP | op::SILENCE) {
            self.alert_dismissed = self.modem.as_ref().map(|m| {
                if target == 0 {
                    m.status.call.id
                } else {
                    target as u8
                }
            });
            // SAFETY: HAPTIC uses scalar duration/amplitude fields and no buffer.
            // SAFETY: HAPTIC uses scalar fields and no buffer.
            let haptic = unsafe { hal_io(HAPTIC, Io::default()) }.0;
            dprintln!(
                "[modemd] t={}ms answered/ended/silenced vibration stop -> {}",
                time::millis(),
                haptic
            );
            self.haptic_at = 0;
        }
        msg.complete(r);
    }

    fn disconnect(&mut self, session: u32) -> ControlFlow<()> {
        self.cancel_notify(session);
        self.seen.retain(|(id, ..)| *id != session);
        // the card may still answer; until then no other PIN request starts (Cellular keeps it)
        if self.pin_msg.as_ref().is_some_and(|(id, _)| *id == session)
            && let Some((_, msg)) = self.pin_msg.take()
        {
            msg.complete(err::KERR_CANCEL);
        }
        ControlFlow::Continue(())
    }
}

/// A PIN from descriptor `slot` into `buf`: 4-8 bytes (the digits are checked by Cellular), nothing else read.
fn read_pin(msg: &Message, slot: usize, buf: &mut [u8; 8]) -> Result<usize, i32> {
    match msg.des_len(slot)? {
        n @ 4..=8 => msg.read8(slot, &mut buf[..n], 0),
        _ => Err(err::KERR_ARGUMENT),
    }
}

/// The call's voice path runs from "connected" until the modem says the call ended (END/NONE), through hold and
/// disconnecting, as Linux (it stops on "terminated"). Stopping it at "disconnecting" overlapped the modem's own
/// teardown with the ADSP: the ADSP stopped answering and the modem stayed on 3G (C6 2026-11-09).
fn call_audio(state: u8, is_call: bool) -> bool {
    use modem_ipc::call;
    state == call::CONVERSATION || (is_call && matches!(state, call::HOLD | call::DISCONNECTING))
}

enum Wake {
    Modem(Result<Message, i32>),
    Hub(Result<usize, i32>),
    Timer,
}

/// `select` is left-biased: alternate first priority so neither IPC floods nor QRTR starve the other.
async fn next_wake<M, H>(
    modem: core::pin::Pin<&mut M>,
    hub: core::pin::Pin<&mut H>,
    wait_us: i32,
    turn: u8,
) -> Wake
where
    M: core::future::Future<Output = Result<Message, i32>>,
    H: core::future::Future<Output = Result<usize, i32>>,
{
    if turn == 0 {
        match rt::select(modem, rt::select(hub, time::delay(wait_us))).await {
            Either::Left(r) => Wake::Modem(r),
            Either::Right(Either::Left(r)) => Wake::Hub(r),
            Either::Right(Either::Right(())) => Wake::Timer,
        }
    } else {
        match rt::select(hub, rt::select(modem, time::delay(wait_us))).await {
            Either::Left(r) => Wake::Hub(r),
            Either::Right(Either::Left(r)) => Wake::Modem(r),
            Either::Right(Either::Right(())) => Wake::Timer,
        }
    }
}

fn main() -> i32 {
    let server = match Server::create(modem_ipc::NAME) {
        Ok(s) => s,
        Err(e) => {
            dprintln!("[modemd] already running ({}): exiting", e);
            return 0;
        }
    };
    dprintln!("[modemd] start: serving NeoModem, connecting to NeoHub and AUDIOD");
    // GNSSD (docs hardware/gnss.md) waits for NeoHub on its own; a second copy exits
    match e32::process::Process::spawn("GNSSD.EXE", "") {
        Ok(p) => p.resume(),
        Err(e) => dprintln!("[modemd] GNSSD spawn failed ({})", e),
    }
    let mut d = Daemon {
        phase: Phase::Starting,
        modem: Some(Modem::new()),
        status: Status::default(),
        hub: Snapshot::default(),
        next_connect: 0,
        connect_backoff: 100,
        spawned: false,
        audio: audio::Link::new(),
        dtmf_stop: None,
        subscribers: Vec::new(),
        pin_msg: None,
        seen: Vec::new(),
        next_session: 0,
        published: Status::default(),
        haptic_at: 0,
        alert_dismissed: None,
        silenced: None,
        waiting_alert: None,
        haptic_error: 0,
        sms_tone_until: 0,
        tones: (modem_ipc::tone::BUILT_IN, modem_ipc::tone::BUILT_IN),
        sms_buzz_at: 0,
        ring_error: 0,
        settings_unread: None,
        changed: Changed::default(),
    };
    // Ordinary C: settings only; modem EFS remains RAM-backed (G4).
    if let Ok(fs) = e32::fs::Fs::connect() {
        // C7 or C8, with or without the tones' byte (and the `.new` a cut mid-save left, sys::load_private); an
        // unreadable one is tried again and never overwritten meanwhile
        if let Err(e) = d.load_settings() {
            dprintln!(
                "[modemd] settings.bin unreadable ({}): kept, read again later",
                e
            );
            d.settings_unread = Some(0);
        }
        // the call log (C8): a missing or unreadable calls.db starts an empty one
        if let Some(m) = &mut d.modem
            && let Ok(f) = e32::fs::File::open(
                &fs,
                "C:\\private\\e0003001\\calls.db",
                e32::fs::mode::READ | e32::fs::mode::SHARE_READERS_ONLY,
            )
        {
            let mut b = vec![0u8; 10 + modem_ipc::MAX_LOG * modem_ipc::LogEntry::SIZE];
            let n = f.read_at(0, &mut b).unwrap_or(0);
            if let Some(mut log) = modem::calllog::CallLog::decode(&b[..n]) {
                log.days = m.log.days;
                m.log = log;
            }
        }
        // the messages (C9): a missing or unreadable sms.db starts empty; what was being sent has failed
        if let Some(m) = &mut d.modem
            && let Ok(f) = e32::fs::File::open(
                &fs,
                "C:\\private\\e0003001\\sms.db",
                e32::fs::mode::READ | e32::fs::mode::SHARE_READERS_ONLY,
            )
        {
            let size = f.size().unwrap_or(0).clamp(0, 4 << 20) as usize;
            let mut b = vec![0u8; size];
            let n = f.read_at(0, &mut b).unwrap_or(0);
            match modem::messages::Store::decode(&b[..n]) {
                Some(mut store) => {
                    store.restarted();
                    m.sms = store;
                }
                None if n > 0 => {
                    dprintln!("[modemd] sms.db unreadable ({} bytes): starting empty", n)
                }
                None => {}
            }
        }
        // the blocklist (v2.2): a missing or unreadable block.db blocks nothing
        if let Some(m) = &mut d.modem
            && let Ok(f) = e32::fs::File::open(
                &fs,
                "C:\\private\\e0003001\\block.db",
                e32::fs::mode::READ | e32::fs::mode::SHARE_READERS_ONLY,
            )
        {
            let mut b = vec![0u8; modem_ipc::MAX_BLOCK_BYTES + modem_ipc::MAX_KNOWN_BYTES];
            let n = f.read_at(0, &mut b).unwrap_or(0);
            if let Some(list) = modem::phone_rules::block::List::decode(&b[..n]) {
                m.block = list;
            }
        }
    }
    // the modem exists from startup (ADSP service first), so the saved intent goes to it before any radio work
    if let Some(m) = &mut d.modem {
        m.status.airplane = d.status.airplane;
        m.status.ring = d.status.ring;
    }
    d.publish();
    rt::block_on(async {
        let mut receive = pin!(server.receive());
        let mut pump = hub::schedule::PumpDeadline::new(time::millis(), (LOOP_US / 1000) as u64);
        let mut hub_bytes = vec![0; 0x8000];
        let mut receive_turn = 0;
        loop {
            let hub_session = d.modem.as_ref().and_then(Modem::session);
            // Descriptor storage outlives the pinned SendFuture and is read only after completion.
            let completed = {
                let mut hub_receive = pin!(client::receive(hub_session.as_deref(), &mut hub_bytes));
                loop {
                    if pump.due(time::millis()) {
                        d.step();
                        pump.completed(time::millis());
                    }
                    if hub_session.is_none()
                        && d.modem.as_ref().is_some_and(|m| m.session().is_some())
                    {
                        break None;
                    }
                    // C6: delays are microseconds. IPC traffic never postpones the 10 ms pump.
                    let wake = next_wake(
                        receive.as_mut(),
                        hub_receive.as_mut(),
                        pump.wait_us(time::millis()).max(1),
                        receive_turn,
                    )
                    .await;
                    receive_turn = (receive_turn + 1) % 2;
                    match wake {
                        Wake::Modem(Ok(msg)) => {
                            receive.set(server.receive());
                            let _ = server::dispatch(&mut d, msg);
                        }
                        Wake::Modem(Err(e)) => {
                            dprintln!("[modemd] receive failed ({})", e);
                            receive.set(server.receive());
                        }
                        Wake::Hub(result) => break Some(result),
                        Wake::Timer => {}
                    }
                    d.publish();
                }
            };
            match completed {
                Some(Ok(n)) => d.hub_events(&hub_bytes[..n]),
                Some(Err(e)) => d.hub_lost(e),
                None => {}
            }
            d.publish();
        }
    })
}

e32::main!(main);
