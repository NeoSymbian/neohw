// SPDX-License-Identifier: Apache-2.0
//! The NeoModem server's contract (docs/hardware/modem.md C5): MODEMD.EXE serves it, neoui's SYSSTART is its client.
//! - [`op::STATUS`]: the server writes [`Status::SIZE`] bytes ([`Status::to_bytes`]) into the des8 in arg 0;
//! - [`op::DIAL`]: arg 0 = the number (des8, ASCII digits, `+`, `*`, `#`);
//! - [`op::ANSWER`], [`op::HANGUP`]: arg 0 = call ID, or 0 for the selected call;
//! - in a call (C6): [`op::MUTE`] arg 0 = 1/0 (the mic), [`op::SPEAKER`] arg 0 = 1/0 (loudspeaker or earpiece),
//!   [`op::VOLUME`] arg 0 = 0..=10, [`op::DTMF`] arg 0 = the key (`0-9`, `*`, `#`) or -1 to stop its tone;
//! - no call (Dev tools): [`op::AUDIO_TEST`] arg 0 = 1 loudspeaker, 2 earpiece (the mic looped back to the
//!   amps), 0 stop; KErrInUse (-14) during a call.
//! - the call log (C8, v2.1): [`op::CALL_LOG`] writes up to [`MAX_LOG`] [`LogEntry`]s, newest first, into the des8 in
//!   arg 0; [`Status::log_revision`] changes whenever the log does. [`op::CLEAR_LOG`] arg 0 = entry id or 0 for
//!   every entry, arg 1 = a [`log`] kind or 0 for any. [`op::CALL_SETTINGS`] arg 0 = [`clir`], arg 1 = log days.
//! - call waiting (C8): [`op::CALL_WAITING`] arg 0 = [`waiting`] request; the network's answer arrives as
//!   [`Status::waiting`].
//! - blocking (v2.2): [`op::BLOCK_GET`] writes the blocklist (`phone_rules::block::List::encode(false)`) into the
//!   des8 in arg 0, [`op::BLOCK_SET`] replaces it from arg 0, [`op::KNOWN_SET`] replaces the contact-number hashes
//!   (u32 LE each, `phone_rules::block::known_key`) from arg 0. A blocked call is rejected as busy before it rings and
//!   logged as [`log::BLOCKED`] with [`LogEntry::why`].
//! - SMS (C9, v2.3): MODEMD receives, keeps and sends the messages ([`SmsEntry`], [`sms`]). [`op::SMS_SEND`] queues
//!   one text to one number; [`op::SMS_READ`] writes the records changed since a revision; [`op::SMS_EDIT`] marks
//!   read, deletes, retries; [`op::SMS_SETTINGS`] gets or sets [`SmsSettings`]. [`Status::sms_revision`] changes with
//!   every change, [`Status::sms_unread`] counts the unread ones. Blocked senders' messages are kept as
//!   [`sms::BLOCKED`] (7 days) and never alert.
//! - v2.6: each [`Call`] says whether it is part of a conference ([`Call::multiparty`]) and when it connected
//!   ([`Call::since`], MODEMD's tick clock, so every app shows the same timer); [`Status::imei`]; [`op::SILENCE`]
//!   quietens one ringing call without touching the ring settings.
//!
//! Each op completes with KErrNone or an error: KErrNotReady (-18) without a modem/call to act on, KErrArgument (-6)
//! for a number it refuses.
#![no_std]

pub const NAME: &str = "NeoModem";
/// major, minor, build
pub const VERSION: (i8, i8, i16) = (2, 7, 0);

pub mod op {
    pub const STATUS: i32 = 1;
    pub const DIAL: i32 = 2;
    pub const ANSWER: i32 = 3;
    pub const HANGUP: i32 = 4;
    pub const MUTE: i32 = 5;
    pub const SPEAKER: i32 = 6;
    pub const VOLUME: i32 = 7;
    pub const DTMF: i32 = 8;
    pub const AUDIO_TEST: i32 = 9;
    /// arg0 writable Status, arg1 last revision. One outstanding request per session.
    pub const NOTIFY: i32 = 10;
    pub const CANCEL_NOTIFY: i32 = 11;
    /// arg0 manage action; arg1 target call ID (0 = primary).
    pub const MANAGE: i32 = 12;
    /// arg0 1 = airplane, 0 = online.
    pub const AIRPLANE: i32 = 13;
    /// arg0 ASCII PIN descriptor. Never persisted or logged.
    pub const VERIFY_PIN: i32 = 14;
    /// arg0 volume 0..10, arg1 silent, arg2 vibration; (2.7) arg3 the tones ([`super::tone::pack`]): only when
    /// [`super::tone::PRESENT`] is set, so a caller that sends three arguments leaves the tones as they are.
    pub const RING_SETTINGS: i32 = 15;
    pub const RETRY: i32 = 16;
    /// arg0 writable des8 for up to MAX_LOG LogEntry records, newest first.
    pub const CALL_LOG: i32 = 17;
    /// arg0 entry id (0 = all), arg1 log kind (0 = any).
    pub const CLEAR_LOG: i32 = 18;
    /// arg0 clir::*, arg1 days the log keeps calls (0 = no log, 1, 10, 30).
    pub const CALL_SETTINGS: i32 = 19;
    /// arg0 waiting::QUERY / ACTIVATE / DEACTIVATE.
    pub const CALL_WAITING: i32 = 20;
    /// arg0 writable des8 for the encoded blocklist (without known hashes).
    pub const BLOCK_GET: i32 = 21;
    /// arg0 des8: the encoded blocklist; its known hashes are ignored.
    pub const BLOCK_SET: i32 = 22;
    /// arg0 des8: known-number hashes, u32 LE each.
    pub const KNOWN_SET: i32 = 23;
    /// arg0 des8: the recipient (digits, an optional leading +); arg1 des8: the text (UTF-8, at most
    /// [`super::sms::MAX_TEXT`] bytes). Completes with the new message's id (> 0), KErrArgument (-6) for a number it
    /// can't send to, or (2.5) KErrTooBig (-40) for a text over 10 parts or empty. A message that then fails says
    /// why in its record ([`super::sms`] `FAIL_*`).
    pub const SMS_SEND: i32 = 24;
    /// arg0 writable des8; arg1 the revision the client has (0 = everything); arg2 [`super::sms`] `READ_*`. Writes
    /// the records changed since that revision, oldest change first, as many as fit (whole [`super::SmsEntry`]s).
    /// KErrNotFound (-1): deletions since then are forgotten, read again from 0.
    pub const SMS_READ: i32 = 25;
    /// arg0 [`super::sms`] `EDIT_*`; arg1 the message id (any message of the thread for the thread edits).
    pub const SMS_EDIT: i32 = 26;
    /// arg0 0 = get, 1 = set; arg1 des8 [`super::SmsSettings`] (written on get, read on set).
    pub const SMS_SETTINGS: i32 = 27;
    /// (2.6) arg0 call ID (0 = primary): stop ringing and vibrating for that call only (Silence). The ring settings
    /// stay as they are.
    pub const SILENCE: i32 = 28;
}

/// SMS (C9): [`SmsEntry::state`], the [`op::SMS_READ`] modes, the [`op::SMS_EDIT`] actions, [`SmsSettings::alert`].
pub mod sms {
    pub const UNREAD: u8 = 1;
    pub const READ: u8 = 2;
    pub const SENDING: u8 = 3;
    pub const SENT: u8 = 4;
    /// the network's status report said so ([`super::SmsEntry::done_utc`])
    pub const DELIVERED: u8 = 5;
    pub const FAILED: u8 = 6;
    /// from a sender the blocklist matched ([`super::SmsEntry::why`]): kept 7 days, never alerts
    pub const BLOCKED: u8 = 7;
    /// deleted: only the id and revision are left, so clients drop their copy
    pub const DELETED: u8 = 8;

    /// the records changed since arg1
    pub const READ_CHANGES: i32 = 0;
    /// only the newest unread message (the shade's line)
    pub const READ_NEWEST_UNREAD: i32 = 1;

    pub const EDIT_READ: i32 = 1;
    /// every message with that message's number
    pub const EDIT_THREAD_READ: i32 = 2;
    pub const EDIT_DELETE: i32 = 3;
    pub const EDIT_DELETE_THREAD: i32 = 4;
    /// send a failed message again
    pub const EDIT_RETRY: i32 = 5;
    /// every blocked message (the id is ignored)
    pub const EDIT_DELETE_BLOCKED: i32 = 6;
    /// every message read (the id is ignored)
    pub const EDIT_ALL_READ: i32 = 7;
    /// (2.4) every message with that message's number archived: Messages lists the thread under Archived; a new
    /// message from that number brings it back
    pub const EDIT_ARCHIVE_THREAD: i32 = 8;
    pub const EDIT_UNARCHIVE_THREAD: i32 = 9;
    /// (2.5) that message's sender: its blocked messages become read ones in its conversation (Unblock sender)
    pub const EDIT_UNBLOCK_SENDER: i32 = 10;
    /// (2.5) every message but the blocked ones (Delete conversations; the id is ignored)
    pub const EDIT_DELETE_ALL: i32 = 11;

    /// (2.5) why a [`FAILED`] message failed, in [`super::SmsEntry::why`] (0: unknown, a 2.4 record)
    pub const FAIL_NO_ANSWER: u8 = 1;
    /// the network or the modem refused it (no service, barred, the SIM not ready)
    pub const FAIL_REFUSED: u8 = 2;
    /// MODEMD restarted while it was being sent
    pub const FAIL_RESTART: u8 = 3;
    /// a status report said it could not be delivered
    pub const FAIL_UNDELIVERED: u8 = 4;

    pub const ALERT_TONE_VIBRATE: u8 = 0;
    pub const ALERT_VIBRATE: u8 = 1;
    pub const ALERT_SILENT: u8 = 2;

    /// the longest text (UTF-8 bytes): 10 parts of 67 UCS-2 units, 4 bytes each at worst
    pub const MAX_TEXT: usize = 2680;
    /// a buffer [`super::op::SMS_READ`] always makes progress with
    pub const READ_ROOM: usize = 16 * 1024;
}

/// One message as [`op::SMS_READ`] sends it: [`SmsEntry::HEAD`] bytes, the text's length (u16 LE), the text.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct SmsEntry {
    /// never 0; grows with every message
    pub id: u32,
    /// the store revision it last changed at
    pub rev: u32,
    /// received (the service centre's time) or written: UTC, Unix milliseconds
    pub utc: i64,
    /// sent: when the status report said delivered; 0 otherwise
    pub done_utc: i64,
    /// [`sms`] state
    pub state: u8,
    /// why a [`sms::BLOCKED`] message was blocked (`phone_rules::block::why`), or (2.5) why a [`sms::FAILED`] one
    /// failed (`sms::FAIL_*`)
    pub why: u8,
    /// the SMS it took
    pub parts: u8,
    pub unicode: bool,
    pub incoming: bool,
    /// (2.4) its thread is archived ([`sms::EDIT_ARCHIVE_THREAD`]); on the wire, bit 1 of the `incoming` byte
    pub archived: bool,
    /// the other side: digits with an optional +, or an alphanumeric sender
    pub number: Text,
}

impl SmsEntry {
    pub const HEAD: usize = 4 + 4 + 8 + 8 + 5 + 33;

    /// This entry and `text` into `out`; the bytes written, or `None` if it doesn't fit.
    pub fn write(&self, text: &str, out: &mut [u8]) -> Option<usize> {
        let t = &text.as_bytes()[..text.len().min(sms::MAX_TEXT)];
        let n = Self::HEAD + 2 + t.len();
        let b = out.get_mut(..n)?;
        b[..4].copy_from_slice(&self.id.to_le_bytes());
        b[4..8].copy_from_slice(&self.rev.to_le_bytes());
        b[8..16].copy_from_slice(&self.utc.to_le_bytes());
        b[16..24].copy_from_slice(&self.done_utc.to_le_bytes());
        b[24..29].copy_from_slice(&[
            self.state,
            self.why,
            self.parts,
            self.unicode as u8,
            self.incoming as u8 | (self.archived as u8) << 1,
        ]);
        b[29] = self.number.len;
        b[30..62].copy_from_slice(&self.number.buf);
        b[62..64].copy_from_slice(&(t.len() as u16).to_le_bytes());
        b[64..].copy_from_slice(t);
        Some(n)
    }

    /// One entry from the front of `b`: it, its text, the bytes it took.
    pub fn read(b: &[u8]) -> Option<(Self, &str, usize)> {
        let h = b.get(..Self::HEAD + 2)?;
        let len = h[29];
        let mut buf = [0; TEXT];
        buf.copy_from_slice(&h[30..62]);
        if usize::from(len) > TEXT || core::str::from_utf8(&buf[..usize::from(len)]).is_err() {
            return None;
        }
        let n = usize::from(u16::from_le_bytes([h[62], h[63]]));
        let text = core::str::from_utf8(b.get(Self::HEAD + 2..Self::HEAD + 2 + n)?).ok()?;
        let e = Self {
            id: u32::from_le_bytes(h[..4].try_into().ok()?),
            rev: u32::from_le_bytes(h[4..8].try_into().ok()?),
            utc: i64::from_le_bytes(h[8..16].try_into().ok()?),
            done_utc: i64::from_le_bytes(h[16..24].try_into().ok()?),
            state: h[24],
            why: h[25],
            parts: h[26],
            unicode: h[27] != 0,
            incoming: h[28] & 1 != 0,
            archived: h[28] & 2 != 0,
            number: Text { len, buf },
        };
        Some((e, text, Self::HEAD + 2 + n))
    }
}

/// [`op::SMS_SETTINGS`]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SmsSettings {
    /// ask the network for a status report on each message sent (✓✓)
    pub reports: bool,
    /// [`sms`] `ALERT_*`
    pub alert: u8,
    /// the message centre, from the SIM (empty while unknown); setting it writes the modem's
    pub smsc: Text,
}

impl Default for SmsSettings {
    fn default() -> Self {
        Self {
            reports: true,
            alert: sms::ALERT_TONE_VIBRATE,
            smsc: Text::default(),
        }
    }
}

impl SmsSettings {
    pub const SIZE: usize = 2 + 33;

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        b[0] = self.reports as u8;
        b[1] = self.alert;
        b[2] = self.smsc.len;
        b[3..].copy_from_slice(&self.smsc.buf);
        b
    }

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        let b = b.get(..Self::SIZE)?;
        let len = b[2];
        let mut buf = [0; TEXT];
        buf.copy_from_slice(&b[3..]);
        if usize::from(len) > TEXT || core::str::from_utf8(&buf[..usize::from(len)]).is_err() {
            return None;
        }
        Some(Self {
            reports: b[0] != 0,
            alert: b[1],
            smsc: Text { len, buf },
        })
    }
}

/// The largest encoded blocklist (`phone_rules::block`: 100 rules of up to 32 bytes).
pub const MAX_BLOCK_BYTES: usize = 4 + 2 + 100 * 34 + 2;
/// The largest [`op::KNOWN_SET`] payload.
pub const MAX_KNOWN_BYTES: usize = 2000 * 4;

/// Caller ID for our calls ([`op::CALL_SETTINGS`]), the Symbian^3 values: the network's default, or a `#31#`/`*31#`
/// prefix on every call.
pub mod clir {
    pub const NETWORK: i32 = 0;
    pub const HIDE: i32 = 1;
    pub const SHOW: i32 = 2;
}

/// Call waiting: the requests of [`op::CALL_WAITING`] and the states of [`Status::waiting`].
pub mod waiting {
    pub const QUERY: i32 = 0;
    pub const ACTIVATE: i32 = 1;
    pub const DEACTIVATE: i32 = 2;

    pub const UNKNOWN: u8 = 0;
    /// a request is with the network
    pub const BUSY: u8 = 1;
    pub const ON: u8 = 2;
    pub const OFF: u8 = 3;
    /// the network refused or didn't answer
    pub const ERROR: u8 = 4;
}

/// [`LogEntry::kind`]
pub mod log {
    pub const DIALLED: u8 = 1;
    pub const RECEIVED: u8 = 2;
    pub const MISSED: u8 = 3;
    /// an incoming call we hung up while it rang
    pub const REJECTED: u8 = 4;
    /// an incoming call MODEMD rejected because the blocklist matched ([`LogEntry::why`])
    pub const BLOCKED: u8 = 5;
}

/// The most calls the log keeps (the Symbian^3 log kept 20 per list: missed, received, dialled).
pub const MAX_LOG: usize = 60;

/// One call of the log (C8).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct LogEntry {
    /// never 0; grows with every entry
    pub id: u32,
    /// when the call started: UTC, Unix milliseconds
    pub utc: i64,
    /// connected time; 0 for missed and rejected calls
    pub duration_s: u32,
    pub kind: u8,
    /// the network's presentation of the number (phone_rules::caller::presentation)
    pub presentation: u8,
    pub number: Text,
    /// why a [`log::BLOCKED`] call was blocked (`phone_rules::block::why`); 0 otherwise
    pub why: u8,
}

impl LogEntry {
    pub const SIZE: usize = 4 + 8 + 4 + 2 + 33 + 1;

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        b[..4].copy_from_slice(&self.id.to_le_bytes());
        b[4..12].copy_from_slice(&self.utc.to_le_bytes());
        b[12..16].copy_from_slice(&self.duration_s.to_le_bytes());
        b[16] = self.kind;
        b[17] = self.presentation;
        b[18] = self.number.len;
        b[19..51].copy_from_slice(&self.number.buf);
        b[51] = self.why;
        b
    }

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        let b = b.get(..Self::SIZE)?;
        let len = b[18];
        let mut buf = [0; TEXT];
        buf.copy_from_slice(&b[19..51]);
        if len as usize > TEXT || core::str::from_utf8(&buf[..len as usize]).is_err() {
            return None;
        }
        Some(Self {
            id: u32::from_le_bytes(b[..4].try_into().ok()?),
            utc: i64::from_le_bytes(b[4..12].try_into().ok()?),
            duration_s: u32::from_le_bytes(b[12..16].try_into().ok()?),
            kind: b[16],
            presentation: b[17],
            number: Text { len, buf },
            why: b[51],
        })
    }
}

/// [`Status::modem`]
pub mod modem {
    pub const BOOTING: u8 = 0;
    /// booted and serving, not yet online (radio off)
    pub const RUNNING: u8 = 1;
    pub const ONLINE: u8 = 2;
    pub const FAILED: u8 = 3;
    pub const RECOVERING: u8 = 4;
}

/// [`Status::sim`]
pub mod sim {
    pub const UNKNOWN: u8 = 0;
    pub const ABSENT: u8 = 1;
    pub const PRESENT: u8 = 2;
    pub const ERROR: u8 = 3;
    pub const PIN_REQUIRED: u8 = 4;
    pub const BLOCKED: u8 = 5;
}

/// Manage-calls actions. Answer/end additionally accept arg0 call ID (0 selects primary).
pub mod manage {
    pub const HOLD: i32 = 1;
    pub const RESUME: i32 = 2;
    pub const SWAP: i32 = 3;
    pub const MERGE: i32 = 4;
    /// end the active call(s) and answer the waiting one (v2.2)
    pub const END_ANSWER: i32 = 5;
}

pub const MAX_CALLS: usize = 8;

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Calls {
    pub len: u8,
    pub entries: [Call; MAX_CALLS],
}

impl Calls {
    pub fn as_slice(&self) -> &[Call] {
        &self.entries[..usize::from(self.len).min(MAX_CALLS)]
    }
    pub fn find(&self, id: u8) -> Option<&Call> {
        self.as_slice().iter().find(|c| c.id == id)
    }
}

/// (2.7, Settings SE2b) The ring's tones, NeoAudio's `tones` ids: a call's ringtone and a message's tone.
pub mod tone {
    pub const BUILT_IN: u8 = 0;
    pub const BEEP: u8 = 1;
    pub const TWO_BEEPS: u8 = 2;
    /// no sound: the call vibrates (if vibrate is on), a message buzzes
    pub const NONE: u8 = 3;
    /// [`super::op::RING_SETTINGS`]' arg3 carries tones (bit 8); without it MODEMD keeps the ones it has.
    pub const PRESENT: i32 = 0x100;

    /// arg3 for these tones.
    #[must_use]
    pub fn pack(ring: u8, message: u8) -> i32 {
        PRESENT | i32::from(ring & 0xf) | i32::from(message & 0xf) << 4
    }

    /// The tones after a [`super::op::RING_SETTINGS`] with this arg3: the new ones, or `current` when it carries none.
    ///
    /// # Errors
    /// [`Unknown`] as [`unpack`].
    pub fn apply(current: (u8, u8), arg: i32) -> Result<(u8, u8), Unknown> {
        Ok(unpack(arg)?.unwrap_or(current))
    }

    /// An arg3 with a tone this MODEMD doesn't know.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Unknown;

    /// The tones in arg3: `Ok(None)` when it carries none (an older caller).
    ///
    /// # Errors
    /// [`Unknown`] for a tone above [`NONE`] or stray bits.
    pub fn unpack(arg: i32) -> Result<Option<(u8, u8)>, Unknown> {
        if arg & PRESENT == 0 {
            return Ok(None);
        }
        let (ring, message) = ((arg & 0xf) as u8, (arg >> 4 & 0xf) as u8);
        if ring > NONE || message > NONE || arg & !0x1ff != 0 {
            return Err(Unknown);
        }
        Ok(Some((ring, message)))
    }
}

/// MODEMD's `settings.bin`: C7 (airplane, the ring), C8 (+ caller ID, the log's days), C9 (+ the tones, SE2b).
pub mod saved {
    use super::Ring;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Saved {
        pub airplane: bool,
        pub ring: Ring,
        /// C8 on: caller ID (0..=2) and the call log's days; `None` from a C7 file
        pub c8: Option<(u8, u8)>,
        /// C9 on: the ringtone and the message tone; `None` before
        pub tones: Option<(u8, u8)>,
    }

    /// The C9 record.
    #[must_use]
    pub fn encode(s: &Saved) -> [u8; 10] {
        let (clir, days) = s.c8.unwrap_or((0, 30));
        let (ring, message) = s.tones.unwrap_or((0, 0));
        [
            b'C',
            b'9',
            1,
            u8::from(s.airplane),
            s.ring.volume,
            u8::from(s.ring.silent),
            u8::from(s.ring.vibrate),
            clir,
            days,
            ring | message << 4,
        ]
    }

    /// A C7, C8 or C9 record; `None` if it is none of them, or damaged.
    #[must_use]
    pub fn decode(b: &[u8]) -> Option<Saved> {
        let (kind, n) = match b.get(..3)? {
            [b'C', b'7', 1] => (7, 7),
            [b'C', b'8', 1] => (8, 9),
            [b'C', b'9', 1] => (9, 10),
            _ => return None,
        };
        if b.len() != n || b[4] > 10 || (kind >= 8 && b[7] > 2) {
            return None;
        }
        let tones = if kind == 9 {
            Some(super::tone::unpack(super::tone::PRESENT | i32::from(b[9])).ok()??)
        } else {
            None
        };
        Some(Saved {
            airplane: b[3] != 0,
            ring: Ring {
                volume: b[4],
                silent: b[5] != 0,
                vibrate: b[6] != 0,
            },
            c8: (kind >= 8).then(|| (b[7], b[8])),
            tones,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ring {
    pub volume: u8,
    pub silent: bool,
    pub vibrate: bool,
}
impl Default for Ring {
    fn default() -> Self {
        Self {
            volume: 7,
            silent: false,
            vibrate: true,
        }
    }
}

/// [`Status::reg`]
pub mod reg {
    pub const NONE: u8 = 0;
    pub const SEARCHING: u8 = 1;
    pub const HOME: u8 = 2;
    pub const ROAMING: u8 = 3;
    pub const DENIED: u8 = 4;
}

/// [`Status::rat`]
pub mod rat {
    pub const NONE: u8 = 0;
    pub const GSM: u8 = 1;
    pub const UMTS: u8 = 2;
    pub const LTE: u8 = 3;
}

/// [`Call::state`]: QMI VOICE call states (libqmi `QmiVoiceCallState`).
pub mod call {
    pub const NONE: u8 = 0;
    pub const ORIGINATION: u8 = 1;
    pub const INCOMING: u8 = 2;
    pub const CONVERSATION: u8 = 3;
    pub const CC_IN_PROGRESS: u8 = 4;
    pub const ALERTING: u8 = 5;
    pub const HOLD: u8 = 6;
    pub const WAITING: u8 = 7;
    pub const DISCONNECTING: u8 = 8;
    pub const END: u8 = 9;
    pub const SETUP: u8 = 10;
}

const TEXT: usize = 32;

/// Short text in a fixed buffer (operator names, phone numbers); longer input is cut at 32 bytes.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Text {
    len: u8,
    buf: [u8; TEXT],
}

impl Text {
    pub fn new(s: &str) -> Self {
        let mut t = Text::default();
        let mut n = s.len().min(TEXT);
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        t.buf[..n].copy_from_slice(&s.as_bytes()[..n]);
        t.len = n as u8;
        t
    }

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len as usize]).unwrap_or("")
    }
}

/// One modem call. Status.call is the selected UI call; Status.calls is authoritative.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Call {
    pub id: u8,
    pub state: u8,
    pub incoming: bool,
    pub number: Text,
    /// the network's presentation of `number` (phone_rules::caller::presentation; 0 allowed)
    pub presentation: u8,
    /// (2.6) part of a conference (the network's multiparty flag)
    pub multiparty: bool,
    /// (2.6) when it connected, in MODEMD's `e32::time::millis()` (the kernel tick clock every process shares); 0:
    /// not yet. A call's timer is `millis() - since`, the same in every app.
    pub since: u64,
}

/// The call's audio (C6): what the in-call buttons show.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Audio {
    /// the audio path runs (a call is connected and the ADSP carries it)
    pub on: bool,
    pub muted: bool,
    pub speaker: bool,
    /// 0..=10
    pub volume: u8,
}

/// The network's time (NITZ: QMI NAS Network Time), for the phone's automatic time.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Nitz {
    /// UTC as Unix seconds when the network sent it; 0 = none yet
    pub utc: u32,
    /// the local zone in quarter hours east of UTC, daylight saving included (3GPP TS 24.008)
    pub tz: i8,
    /// the daylight saving part of `tz`, in hours
    pub dst: u8,
    /// counts the network times received: a new one is a new value even if `utc` didn't move
    pub seq: u16,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Status {
    pub modem: u8,
    pub sim: u8,
    pub reg: u8,
    pub rat: u8,
    /// 0..=4
    pub bars: u8,
    pub operator: Text,
    pub call: Call,
    pub audio: Audio,
    pub calls: Calls,
    /// Changes only when a new snapshot is published. NOTIFY compares this word.
    pub revision: u32,
    pub generation: u32,
    /// One user command outstanding; result is Symbian error or negative QMI error.
    pub pending: bool,
    pub result: i32,
    pub airplane: bool,
    pub radio_online: bool,
    /// 255 means unavailable.
    pub pin_attempts: u8,
    pub ring: Ring,
    pub missed: u32,
    pub nitz: Nitz,
    /// Changes whenever the call log does (v2.1): fetch it again with [`op::CALL_LOG`].
    pub log_revision: u32,
    /// Call waiting on the network as far as we know ([`waiting`] states, v2.1).
    pub waiting: u8,
    /// Changes whenever a message does (v2.3): read the changes with [`op::SMS_READ`].
    pub sms_revision: u32,
    /// Unread messages (blocked ones never count), and from how many senders.
    pub sms_unread: u16,
    pub sms_senders: u8,
    /// (2.6) the IMEI (empty until the modem reports it); `*#06#` shows it. Never log it whole.
    pub imei: Text,
}

/// v2.1's tail: the calls' presentations, the log revision, call waiting, the selected call's presentation.
const V21: usize = MAX_CALLS + 4 + 1 + 1;
const TAIL: usize = 110 + MAX_CALLS * 36;
/// v2.3's tail: the SMS revision, the unread count and senders.
const V23: usize = 4 + 2 + 1;
/// v2.6's tail: per call (then the selected call) a flags byte (bit 0: multiparty) and `since` (u64 LE); the IMEI.
const V26: usize = (MAX_CALLS + 1) * 9 + 1 + TEXT;

impl Status {
    pub const SIZE: usize = 110 + MAX_CALLS * 36 + V21 + V23 + V26;

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        b[..8].copy_from_slice(&[
            self.modem,
            self.sim,
            self.reg,
            self.rat,
            self.bars,
            self.call.id,
            self.call.state,
            self.call.incoming as u8,
        ]);
        for (i, t) in [self.operator, self.call.number].iter().enumerate() {
            let at = 8 + i * 33;
            b[at] = t.len;
            b[at + 1..at + 33].copy_from_slice(&t.buf);
        }
        b[74..78].copy_from_slice(&[
            self.audio.on as u8,
            self.audio.muted as u8,
            self.audio.speaker as u8,
            self.audio.volume,
        ]);
        b[78..82].copy_from_slice(&self.revision.to_le_bytes());
        b[82..86].copy_from_slice(&self.generation.to_le_bytes());
        b[86..90].copy_from_slice(&self.result.to_le_bytes());
        b[90..98].copy_from_slice(&[
            self.pending as u8,
            self.airplane as u8,
            self.radio_online as u8,
            self.pin_attempts,
            self.ring.volume,
            self.ring.silent as u8,
            self.ring.vibrate as u8,
            self.calls.len,
        ]);
        b[98..102].copy_from_slice(&self.missed.to_le_bytes());
        b[102..106].copy_from_slice(&self.nitz.utc.to_le_bytes());
        b[106] = self.nitz.tz as u8;
        b[107] = self.nitz.dst;
        b[108..110].copy_from_slice(&self.nitz.seq.to_le_bytes());
        for (i, c) in self.calls.as_slice().iter().enumerate() {
            let at = 110 + i * 36;
            b[at..at + 4].copy_from_slice(&[c.id, c.state, c.incoming as u8, c.number.len]);
            b[at + 4..at + 36].copy_from_slice(&c.number.buf);
            b[TAIL + i] = c.presentation;
        }
        let at = TAIL + MAX_CALLS;
        b[at..at + 4].copy_from_slice(&self.log_revision.to_le_bytes());
        b[at + 4] = self.waiting;
        b[at + 5] = self.call.presentation;
        let at = TAIL + V21;
        b[at..at + 4].copy_from_slice(&self.sms_revision.to_le_bytes());
        b[at + 4..at + 6].copy_from_slice(&self.sms_unread.to_le_bytes());
        b[at + 6] = self.sms_senders;
        let at = TAIL + V21 + V23;
        let calls = self.calls.as_slice().iter().enumerate();
        for (i, c) in calls.chain(core::iter::once((MAX_CALLS, &self.call))) {
            let at = at + i * 9;
            b[at] = c.multiparty as u8;
            b[at + 1..at + 9].copy_from_slice(&c.since.to_le_bytes());
        }
        let at = at + (MAX_CALLS + 1) * 9;
        b[at] = self.imei.len;
        b[at + 1..at + 1 + TEXT].copy_from_slice(&self.imei.buf);
        b
    }

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        let b = b.get(..Self::SIZE)?;
        let text = |at: usize| {
            let len = b[at];
            let mut buf = [0; 32];
            buf.copy_from_slice(&b[at + 1..at + 33]);
            (len <= 32 && core::str::from_utf8(&buf[..usize::from(len.min(32))]).is_ok())
                .then_some(Text { len, buf })
        };
        let v26 = TAIL + V21 + V23;
        let flags = |i: usize| -> Option<(bool, u64)> {
            let at = v26 + i * 9;
            Some((
                b[at] & 1 != 0,
                u64::from_le_bytes(b[at + 1..at + 9].try_into().ok()?),
            ))
        };
        let mut calls = Calls {
            len: b[97],
            ..Default::default()
        };
        if usize::from(calls.len) > MAX_CALLS {
            return None;
        }
        for i in 0..usize::from(calls.len) {
            let at = 110 + i * 36;
            calls.entries[i] = Call {
                id: b[at],
                state: b[at + 1],
                incoming: b[at + 2] != 0,
                number: text(at + 3)?,
                presentation: b[TAIL + i],
                multiparty: flags(i)?.0,
                since: flags(i)?.1,
            };
        }
        Some(Self {
            modem: b[0],
            sim: b[1],
            reg: b[2],
            rat: b[3],
            bars: b[4],
            operator: text(8)?,
            call: Call {
                id: b[5],
                state: b[6],
                incoming: b[7] != 0,
                number: text(41)?,
                presentation: b[TAIL + MAX_CALLS + 5],
                multiparty: flags(MAX_CALLS)?.0,
                since: flags(MAX_CALLS)?.1,
            },
            audio: Audio {
                on: b[74] != 0,
                muted: b[75] != 0,
                speaker: b[76] != 0,
                volume: b[77],
            },
            revision: u32::from_le_bytes(b[78..82].try_into().ok()?),
            generation: u32::from_le_bytes(b[82..86].try_into().ok()?),
            result: i32::from_le_bytes(b[86..90].try_into().ok()?),
            pending: b[90] != 0,
            airplane: b[91] != 0,
            radio_online: b[92] != 0,
            pin_attempts: b[93],
            ring: Ring {
                volume: b[94],
                silent: b[95] != 0,
                vibrate: b[96] != 0,
            },
            calls,
            missed: u32::from_le_bytes(b[98..102].try_into().ok()?),
            nitz: Nitz {
                utc: u32::from_le_bytes(b[102..106].try_into().ok()?),
                tz: b[106] as i8,
                dst: b[107],
                seq: u16::from_le_bytes(b[108..110].try_into().ok()?),
            },
            log_revision: u32::from_le_bytes(
                b[TAIL + MAX_CALLS..TAIL + MAX_CALLS + 4].try_into().ok()?,
            ),
            waiting: b[TAIL + MAX_CALLS + 4],
            sms_revision: u32::from_le_bytes(b[TAIL + V21..TAIL + V21 + 4].try_into().ok()?),
            sms_unread: u16::from_le_bytes([b[TAIL + V21 + 4], b[TAIL + V21 + 5]]),
            sms_senders: b[TAIL + V21 + 6],
            imei: text(v26 + (MAX_CALLS + 1) * 9)?,
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn settings_bin_reads_c7_c8_and_c9() {
        use super::saved::*;
        let s = Saved {
            airplane: true,
            ring: super::Ring {
                volume: 4,
                silent: false,
                vibrate: true,
            },
            c8: Some((2, 90)),
            tones: Some((super::tone::TWO_BEEPS, super::tone::NONE)),
        };
        assert_eq!(decode(&encode(&s)), Some(s));
        // an older file: C8 and C7 still load, without the tones
        let c8 = [b'C', b'8', 1, 0, 7, 1, 0, 1, 30];
        let d = decode(&c8).unwrap();
        assert_eq!((d.tones, d.c8, d.ring.silent), (None, Some((1, 30)), true));
        let c7 = [b'C', b'7', 1, 1, 10, 0, 1];
        assert_eq!(decode(&c7).unwrap().c8, None);
        // damaged: a tone over NONE, a volume over 10, a wrong length
        let mut bad = encode(&s);
        bad[9] = 4;
        assert_eq!(decode(&bad), None);
        assert_eq!(decode(&[b'C', b'9', 1, 0, 11, 0, 0, 0, 30, 0]), None);
        assert_eq!(decode(&encode(&s)[..9]), None);
    }

    #[test]
    fn a_three_argument_call_keeps_the_tones() {
        use super::tone::*;
        let set = apply((BUILT_IN, BUILT_IN), pack(BEEP, TWO_BEEPS)).unwrap();
        assert_eq!(set, (BEEP, TWO_BEEPS));
        // NEOPHONE's and an older shell's RING_SETTINGS: arg3 = 0
        assert_eq!(apply(set, 0), Ok(set));
        assert_eq!(apply(set, PRESENT | 9), Err(Unknown));
    }

    #[test]
    fn tones_only_when_present() {
        use super::tone::*;
        // a three-argument caller (arg3 = 0) carries no tones: MODEMD keeps its own
        assert_eq!(unpack(0), Ok(None));
        assert_eq!(unpack(pack(TWO_BEEPS, NONE)), Ok(Some((TWO_BEEPS, NONE))));
        assert_eq!(
            unpack(pack(BUILT_IN, BUILT_IN)),
            Ok(Some((BUILT_IN, BUILT_IN)))
        );
        assert_eq!(unpack(PRESENT | 4), Err(Unknown));
        assert_eq!(unpack(PRESENT | 4 << 4), Err(Unknown));
        assert_eq!(unpack(PRESENT | 0x200), Err(Unknown));
    }

    use super::*;

    #[test]
    fn round_trip() {
        let s = Status {
            modem: modem::ONLINE,
            sim: sim::PRESENT,
            reg: reg::HOME,
            rat: rat::LTE,
            bars: 3,
            operator: Text::new("Ucom AM"),
            call: Call {
                id: 1,
                state: call::INCOMING,
                incoming: true,
                number: Text::new("+37400000012"),
                presentation: 0,
                ..Call::default()
            },
            audio: Audio {
                on: true,
                muted: true,
                speaker: false,
                volume: 7,
            },
            ..Status::default()
        };
        let b = s.to_bytes();
        assert_eq!(Status::from_bytes(&b), Some(s));
        assert_eq!(Status::from_bytes(&b[..10]), None);
        assert_eq!(Status::from_bytes(&b[..78]), None);
    }

    #[test]
    fn text_is_cut_on_a_char_boundary() {
        let long = "ééééééééééééééééééééé"; // 21 x 2 bytes
        assert_eq!(Text::new(long).as_str().len(), 32);
        assert_eq!(Text::new("Ucom").as_str(), "Ucom");
    }
    #[test]
    fn v2_round_trip_preserves_call_ids_and_radio_result() {
        let mut s = Status {
            revision: 3,
            generation: 8,
            pending: true,
            result: -1012,
            airplane: true,
            pin_attempts: 2,
            missed: 7,
            nitz: Nitz {
                utc: 1_794_513_007,
                tz: -14,
                dst: 1,
                seq: 3,
            },
            log_revision: 41,
            waiting: waiting::ON,
            sms_revision: 77,
            sms_unread: 3,
            sms_senders: 2,
            ..Default::default()
        };
        s.calls.len = 2;
        s.calls.entries[0] = Call {
            id: 8,
            state: call::CONVERSATION,
            incoming: false,
            number: Text::new("+37400000001"),
            presentation: 0,
            multiparty: true,
            since: 1_234_567_890_123,
        };
        s.calls.entries[1] = Call {
            id: 2,
            state: call::WAITING,
            incoming: true,
            number: Text::new("+37400000002"),
            presentation: 1,
            ..Call::default()
        };
        s.call = s.calls.entries[0];
        s.imei = Text::new("350000000000023");
        let mut bytes = s.to_bytes();
        assert_eq!(Status::from_bytes(&bytes), Some(s));
        bytes[97] = 9;
        assert!(Status::from_bytes(&bytes).is_none());
        bytes[97] = 2;
        bytes[113] = 33;
        assert!(Status::from_bytes(&bytes).is_none());
    }

    #[test]
    fn sms_entry_and_settings_round_trip() {
        let e = SmsEntry {
            id: 9,
            rev: 31,
            utc: 1_794_722_700_000,
            done_utc: 1_794_722_702_000,
            state: sms::DELIVERED,
            why: 0,
            parts: 2,
            unicode: true,
            incoming: false,
            archived: false,
            number: Text::new("+37491234567"),
        };
        let mut buf = [0u8; 200];
        let n = e.write("Բարև", &mut buf).unwrap();
        assert_eq!(n, SmsEntry::HEAD + 2 + 8);
        assert_eq!(SmsEntry::read(&buf[..n]), Some((e, "Բարև", n)));
        // 2.4: archived rides in bit 1 of the incoming byte; a 2.3 record (0 or 1 there) reads as not archived
        for (incoming, archived) in [(true, true), (false, true), (true, false)] {
            let a = SmsEntry {
                incoming,
                archived,
                ..e
            };
            let mut b = [0u8; 80];
            a.write("x", &mut b).unwrap();
            assert_eq!(b[28], incoming as u8 | (archived as u8) << 1);
            assert_eq!(SmsEntry::read(&b).map(|r| r.0), Some(a));
        }
        assert_eq!(SmsEntry::read(&buf[..n - 1]), None);
        assert_eq!(e.write("Բարև", &mut buf[..n - 1]), None);
        let s = SmsSettings {
            smsc: Text::new("+37495980150"),
            ..SmsSettings::default()
        };
        assert_eq!(SmsSettings::from_bytes(&s.to_bytes()), Some(s));
    }

    #[test]
    fn log_entry_round_trip() {
        let e = LogEntry {
            id: 7,
            utc: 1_794_513_007_123,
            duration_s: 151,
            kind: log::BLOCKED,
            presentation: 0,
            number: Text::new("+37491234567"),
            why: 2,
        };
        let b = e.to_bytes();
        assert_eq!(LogEntry::from_bytes(&b), Some(e));
        assert_eq!(LogEntry::from_bytes(&b[..51]), None);
        let mut bad = b;
        bad[18] = 33;
        assert_eq!(LogEntry::from_bytes(&bad), None);
    }
}
