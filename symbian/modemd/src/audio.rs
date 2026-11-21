// SPDX-License-Identifier: Apache-2.0
//! MODEMD's link to AUDIOD (docs hardware.md S4): what the phone wants from the audio path (NeoAudio op::CONTROL),
//! and AUDIOD's report back. MODEMD starts AUDIOD, as it starts HWHUB.
use e32::ipc::{Session, TIpcArgs, TVersion};
use e32::sys::err;
use e32::sys::{TPtr8, TPtrC8};
use e32::{dprintln, time};
use neoaudio::control::{Intent, Report};

/// The intent goes to AUDIOD when it changes, and at least this often (AUDIOD's report comes back with it).
const RESEND_MS: u64 = 100;

pub struct Link {
    session: Option<Session>,
    pub intent: Intent,
    pub report: Report,
    sent: Option<(Intent, u64)>,
    next_connect: u64,
    spawned: bool,
}

impl Link {
    pub fn new() -> Self {
        Link {
            session: None,
            intent: Intent {
                volume: 10,
                ..Intent::default()
            },
            report: Report::default(),
            sent: None,
            next_connect: 0,
            spawned: false,
        }
    }

    /// Stop everything AUDIOD plays for the phone (recovery, a lost hub): a new epoch.
    pub fn clear(&mut self) {
        self.intent.epoch = self.intent.epoch.wrapping_add(1);
        self.intent.test = 0;
        self.intent.ring = false;
        self.intent.call = false;
    }

    /// The call's voice path as AUDIOD runs it, for `call_audio` (until AUDIOD has applied this epoch: none).
    pub fn is_call(&self) -> bool {
        self.report.epoch == self.intent.epoch && self.report.call
    }

    pub fn status(&self) -> modem_ipc::Audio {
        modem_ipc::Audio {
            on: self.report.epoch == self.intent.epoch && self.report.call_running,
            muted: self.intent.muted,
            speaker: self.intent.speaker,
            volume: self.intent.volume,
        }
    }

    pub fn sync(&mut self) {
        let now = time::millis();
        if self.session.is_none() {
            if now < self.next_connect {
                return;
            }
            self.next_connect = now + 200;
            let (major, minor, build) = neoaudio::VERSION;
            match Session::connect(
                neoaudio::NAME,
                TVersion {
                    major: major as u8,
                    minor: minor as u8,
                    build: build as u16,
                },
            ) {
                Ok(s) => {
                    dprintln!("[modemd] connected to AUDIOD");
                    self.session = Some(s);
                    self.sent = None;
                }
                Err(e) if e == err::KERR_NOT_FOUND && !self.spawned => {
                    self.spawned = true;
                    match e32::process::Process::spawn("AUDIOD.EXE", "") {
                        Ok(p) => {
                            p.resume();
                            dprintln!("[modemd] AUDIOD started");
                        }
                        Err(r) => dprintln!("[modemd] AUDIOD spawn failed ({})", r),
                    }
                    return;
                }
                Err(_) => return,
            }
        }
        if self
            .sent
            .is_some_and(|(i, at)| i == self.intent && now < at + RESEND_MS)
        {
            return;
        }
        let Some(s) = &self.session else { return };
        let bytes = self.intent.to_bytes();
        let des = TPtrC8::new(&bytes);
        let mut back = [0u8; Report::SIZE];
        let mut out = TPtr8::new(&mut back);
        let mut args = TIpcArgs::new();
        args.desc8(0, &des).des8(1, &mut out);
        match s.send_receive(neoaudio::op::CONTROL, &args) {
            Ok(_) => {
                if let Some(r) = Report::from_bytes(&back) {
                    self.report = r;
                }
                self.sent = Some((self.intent, now));
            }
            Err(e) => {
                dprintln!("[modemd] AUDIOD control failed ({}): reconnecting", e);
                self.session = None;
                self.spawned = false;
            }
        }
    }
}
