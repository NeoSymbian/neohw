// SPDX-License-Identifier: Apache-2.0
//! The messages (docs/telephony.md C9): every SMS received or sent, kept by MODEMD in `C:\private\e0003001\sms.db`
//! ([`Store::encode`]). Clients read them with modem_ipc::op::SMS_READ: each change gets a new store revision, so a
//! client asks only for what changed since the revision it has. Deleted messages stay as `DELETED` tombstones for a
//! while, so clients drop their copies too.
//!
//! The parts of a long message are kept (saved too) until all have come, or a day has passed: then it is shown with
//! "…" where parts are missing.

use alloc::string::String;
use alloc::vec::Vec;
use modem_ipc::{SmsEntry, SmsSettings, Text, sms as st};
use phone_rules::number;

const MAGIC: &[u8; 4] = b"NSM1";
/// The most messages kept; the oldest go first.
// shortcut: one file rewritten whole on every change (2000 × ~200 bytes); per-thread files if it grows
pub const MAX_MESSAGES: usize = 2000;
/// Deleted messages remembered for clients; older deletions make a client read everything again.
const MAX_TOMBSTONES: usize = 200;
const DAY_MS: i64 = 86_400_000;
/// Blocked messages are kept this long.
const BLOCKED_MS: i64 = 7 * DAY_MS;
/// How long the parts of a long message wait for the rest.
const PART_WAIT_MS: i64 = DAY_MS;
/// The most parts waiting at once (from senders that never finish).
const MAX_PARTS_WAITING: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub entry: SmsEntry,
    pub text: String,
    /// sent: the network's message reference (TP-MR) of each part sent so far
    pub refs: Vec<u8>,
    /// sent: the parts the status reports said were delivered
    pub delivered: u8,
}

/// One part of a long message, waiting for the others.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Part {
    from: String,
    reference: u16,
    total: u8,
    seq: u8,
    utc: i64,
    unicode: bool,
    /// phone_rules::block::why, 0 = not blocked
    why: u8,
    text: String,
}

/// The same sender: equal, or the same subscriber's number.
pub fn same(a: &str, b: &str) -> bool {
    a == b || number::matches(a, b)
}

#[derive(Clone, Debug)]
pub struct Store {
    /// oldest first (by id), tombstones included
    pub messages: Vec<Message>,
    next_id: u32,
    /// the store revision: grows with every change
    pub rev: u32,
    /// deletions up to this revision are forgotten
    floor: u32,
    parts: Vec<Part>,
    /// delivery reports and the alert (the SMSC lives in the modem)
    pub settings: SmsSettings,
    /// changed since MODEMD last saved it
    pub dirty: bool,
    /// a new message to alert for (MODEMD clears it)
    pub alert: bool,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            messages: Vec::new(),
            next_id: 1,
            rev: 0,
            floor: 0,
            parts: Vec::new(),
            settings: SmsSettings::default(),
            dirty: false,
            alert: false,
        }
    }
}

impl Store {
    fn bump(&mut self) -> u32 {
        self.rev = self.rev.wrapping_add(1).max(1);
        self.dirty = true;
        self.rev
    }

    fn find(&mut self, id: u32) -> Option<&mut Message> {
        self.messages
            .iter_mut()
            .find(|m| m.entry.id == id && m.entry.state != st::DELETED)
    }

    fn push(&mut self, mut entry: SmsEntry, text: String) -> u32 {
        entry.id = self.next_id;
        entry.rev = self.bump();
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.messages.push(Message {
            entry,
            text,
            refs: Vec::new(),
            delivered: 0,
        });
        let live = self
            .messages
            .iter()
            .filter(|m| m.entry.state != st::DELETED);
        if live.count() > MAX_MESSAGES
            && let Some(oldest) = self
                .messages
                .iter()
                .find(|m| m.entry.state != st::DELETED)
                .map(|m| m.entry.id)
        {
            self.delete(|m| m.entry.id == oldest);
        }
        entry.id
    }

    /// Turn the messages `pick` chooses into tombstones; drop the oldest tombstones beyond [`MAX_TOMBSTONES`].
    fn delete(&mut self, pick: impl Fn(&Message) -> bool) -> bool {
        let ids: Vec<u32> = self
            .messages
            .iter()
            .filter(|m| m.entry.state != st::DELETED && pick(m))
            .map(|m| m.entry.id)
            .collect();
        for id in &ids {
            let rev = self.bump();
            if let Some(m) = self.messages.iter_mut().find(|m| m.entry.id == *id) {
                m.entry.state = st::DELETED;
                m.entry.rev = rev;
                m.text.clear();
                m.refs.clear();
            }
        }
        let tombs = self
            .messages
            .iter()
            .filter(|m| m.entry.state == st::DELETED)
            .count();
        if tombs > MAX_TOMBSTONES {
            let mut drop = tombs - MAX_TOMBSTONES;
            let mut floor = self.floor;
            self.messages.retain(|m| {
                if drop > 0 && m.entry.state == st::DELETED {
                    drop -= 1;
                    floor = floor.max(m.entry.rev);
                    return false;
                }
                true
            });
            self.floor = floor;
        }
        !ids.is_empty()
    }

    /// An incoming SMS-DELIVER, blocked for `why` or not. The new message's id once it is whole.
    pub fn receive(&mut self, d: sms::Deliver, why: Option<u8>, utc_ms: i64) -> Option<u32> {
        let utc = if d.time_ms > 0 {
            d.time_ms as i64
        } else {
            utc_ms
        };
        let why = why.unwrap_or(0);
        let Some(c) = d.concat.filter(|c| c.total > 1 && c.seq <= c.total) else {
            return Some(self.add_incoming(&d.from, utc, d.unicode, why, 1, d.text));
        };
        let mine =
            |p: &Part| p.reference == c.reference && p.total == c.total && same(&p.from, &d.from);
        if !self.parts.iter().any(|p| mine(p) && p.seq == c.seq) {
            if self.parts.len() >= MAX_PARTS_WAITING {
                self.parts.remove(0);
            }
            self.parts.push(Part {
                from: d.from.clone(),
                reference: c.reference,
                total: c.total,
                seq: c.seq,
                utc,
                unicode: d.unicode,
                why,
                text: d.text,
            });
            self.dirty = true;
        }
        let have = self.parts.iter().filter(|p| mine(p)).count();
        (have == usize::from(c.total)).then(|| self.assemble(&d.from, c.reference, c.total))
    }

    /// The waiting parts of (`from`, `reference`, `total`) as one message, "…" for missing parts.
    fn assemble(&mut self, from: &str, reference: u16, total: u8) -> u32 {
        let mine = |p: &Part| p.reference == reference && p.total == total && same(&p.from, from);
        let mut got: Vec<Part> = self.parts.iter().filter(|p| mine(p)).cloned().collect();
        self.parts.retain(|p| !mine(p));
        got.sort_by_key(|p| p.seq);
        let mut text = String::new();
        for seq in 1..=total {
            match got.iter().find(|p| p.seq == seq) {
                Some(p) => text.push_str(&p.text),
                None => text.push('…'),
            }
        }
        let first = &got[0];
        let (utc, unicode, why) = (first.utc, got.iter().any(|p| p.unicode), first.why);
        let from = String::from(&first.from);
        self.add_incoming(&from, utc, unicode, why, total, text)
    }

    fn add_incoming(
        &mut self,
        from: &str,
        utc: i64,
        unicode: bool,
        why: u8,
        parts: u8,
        text: String,
    ) -> u32 {
        let blocked = why != 0;
        self.alert |= !blocked;
        // a new message brings its archived thread back
        if !blocked {
            self.archive(
                |m| m.entry.archived && same(m.entry.number.as_str(), from),
                false,
            );
        }
        self.push(
            SmsEntry {
                utc,
                state: if blocked { st::BLOCKED } else { st::UNREAD },
                why,
                parts,
                unicode,
                incoming: true,
                number: Text::new(from),
                ..SmsEntry::default()
            },
            text,
        )
    }

    /// Long messages that waited too long are shown as they are; blocked messages older than 7 days go. True when
    /// something changed.
    pub fn expire(&mut self, utc_ms: i64) -> bool {
        if utc_ms <= 0 {
            return false;
        }
        let mut changed = false;
        while let Some(p) = self.parts.iter().find(|p| utc_ms - p.utc > PART_WAIT_MS) {
            let (from, reference, total) = (p.from.clone(), p.reference, p.total);
            self.assemble(&from, reference, total);
            changed = true;
        }
        changed | self.delete(|m| m.entry.state == st::BLOCKED && utc_ms - m.entry.utc > BLOCKED_MS)
    }

    /// A text to send to `to`: its id and the TPDUs to send (modem_ipc::op::SMS_SEND).
    pub fn send(
        &mut self,
        to: &str,
        text: &str,
        utc_ms: i64,
    ) -> Result<(u32, Vec<Vec<u8>>), sms::Error> {
        let to = to.trim();
        let pdus = sms::submit(to, text, self.settings.reports, self.next_id as u8)?;
        let id = self.push(
            SmsEntry {
                utc: utc_ms,
                state: st::SENDING,
                parts: pdus.len() as u8,
                unicode: sms::count(text).unicode,
                incoming: false,
                number: Text::new(to),
                ..SmsEntry::default()
            },
            String::from(text),
        );
        Ok((id, pdus))
    }

    /// The TPDUs for sending message `id` again (modem_ipc::sms::EDIT_RETRY): it is `SENDING` again.
    pub fn retry(&mut self, id: u32) -> Option<Vec<Vec<u8>>> {
        let reports = self.settings.reports;
        let m = self.find(id)?;
        if m.entry.incoming || m.entry.state != st::FAILED {
            return None;
        }
        let pdus = sms::submit(m.entry.number.as_str(), &m.text, reports, id as u8 ^ 0x80).ok()?;
        let rev = self.bump();
        let m = self.find(id)?;
        m.entry.state = st::SENDING;
        m.refs.clear();
        m.delivered = 0;
        m.entry.rev = rev;
        Some(pdus)
    }

    /// Part of message `id` went out with reference `mr`; `last` when it was the last part.
    pub fn sent(&mut self, id: u32, mr: u8, last: bool) {
        let rev = self.bump();
        let Some(m) = self.find(id) else { return };
        m.refs.push(mr);
        if last && m.entry.state == st::SENDING {
            m.entry.state = st::SENT;
            m.entry.rev = rev;
        }
    }

    /// Message `id` could not be sent.
    pub fn failed(&mut self, id: u32) {
        let rev = self.bump();
        if let Some(m) = self.find(id)
            && !m.entry.incoming
        {
            m.entry.state = st::FAILED;
            m.entry.rev = rev;
        }
    }

    /// A status report: the newest message sent to its number with its reference. True when it changed.
    pub fn report(&mut self, r: &sms::StatusReport, utc_ms: i64) -> bool {
        if r.status >= 0x20 && !r.failed() {
            return false; // the service centre is still trying
        }
        let Some(m) = self.messages.iter_mut().rev().find(|m| {
            !m.entry.incoming
                && matches!(m.entry.state, st::SENT | st::SENDING | st::DELIVERED)
                && m.refs.contains(&r.mr)
                && same(m.entry.number.as_str(), &r.to)
        }) else {
            return false;
        };
        if r.failed() {
            m.entry.state = st::FAILED;
        } else {
            m.delivered = m.delivered.saturating_add(1);
            if m.delivered < m.entry.parts {
                return false;
            }
            m.entry.state = st::DELIVERED;
            m.entry.done_utc = if r.time_ms > 0 {
                r.time_ms as i64
            } else {
                utc_ms
            };
        }
        let rev = self.bump();
        if let Some(m) = self
            .messages
            .iter_mut()
            .rev()
            .find(|m| m.refs.contains(&r.mr))
        {
            m.entry.rev = rev;
        }
        true
    }

    /// modem_ipc::op::SMS_EDIT (not RETRY: [`Store::retry`]). True when something changed.
    pub fn edit(&mut self, action: i32, id: u32) -> bool {
        let thread = self
            .messages
            .iter()
            .find(|m| m.entry.id == id && m.entry.state != st::DELETED)
            .map(|m| m.entry.number);
        let in_thread = |m: &Message| {
            thread.is_some_and(|t| {
                m.entry.state != st::BLOCKED && same(m.entry.number.as_str(), t.as_str())
            })
        };
        match action {
            st::EDIT_READ => self.mark_read(|m| m.entry.id == id),
            st::EDIT_THREAD_READ => self.mark_read(in_thread),
            st::EDIT_ALL_READ => self.mark_read(|_| true),
            st::EDIT_DELETE => self.delete(|m| m.entry.id == id),
            st::EDIT_DELETE_THREAD => self.delete(in_thread),
            st::EDIT_DELETE_BLOCKED => self.delete(|m| m.entry.state == st::BLOCKED),
            st::EDIT_ARCHIVE_THREAD => self.archive(in_thread, true),
            st::EDIT_UNARCHIVE_THREAD => self.archive(in_thread, false),
            _ => false,
        }
    }

    /// Archive (or bring back) the messages `pick` chooses. True when something changed.
    fn archive(&mut self, pick: impl Fn(&Message) -> bool, on: bool) -> bool {
        let ids: Vec<u32> = self
            .messages
            .iter()
            .filter(|m| m.entry.state != st::DELETED && m.entry.archived != on && pick(m))
            .map(|m| m.entry.id)
            .collect();
        for id in &ids {
            let rev = self.bump();
            if let Some(m) = self.find(*id) {
                m.entry.archived = on;
                m.entry.rev = rev;
            }
        }
        !ids.is_empty()
    }

    fn mark_read(&mut self, pick: impl Fn(&Message) -> bool) -> bool {
        let ids: Vec<u32> = self
            .messages
            .iter()
            .filter(|m| m.entry.state == st::UNREAD && pick(m))
            .map(|m| m.entry.id)
            .collect();
        for id in &ids {
            let rev = self.bump();
            if let Some(m) = self.find(*id) {
                m.entry.state = st::READ;
                m.entry.rev = rev;
            }
        }
        !ids.is_empty()
    }

    /// (unread messages, their senders) for modem_ipc::Status.
    pub fn unread(&self) -> (u16, u8) {
        let mut senders: Vec<&str> = Vec::new();
        let mut n = 0u16;
        for m in self.messages.iter().filter(|m| m.entry.state == st::UNREAD) {
            n = n.saturating_add(1);
            let from = m.entry.number.as_str();
            if !senders.iter().any(|s| same(s, from)) {
                senders.push(from);
            }
        }
        (n, senders.len().min(255) as u8)
    }

    /// modem_ipc::op::SMS_READ into `out`: the bytes written, or KErrNotFound (-1) when `since` is older than the
    /// deletions the store remembers.
    pub fn read(&self, since: u32, mode: i32, out: &mut [u8]) -> Result<usize, i32> {
        if mode == st::READ_NEWEST_UNREAD {
            let m = self
                .messages
                .iter()
                .rev()
                .find(|m| m.entry.state == st::UNREAD);
            return Ok(m.and_then(|m| m.entry.write(&m.text, out)).unwrap_or(0));
        }
        if since != 0 && since < self.floor {
            return Err(-1);
        }
        let mut changed: Vec<&Message> = self
            .messages
            .iter()
            .filter(|m| m.entry.rev > since && (since != 0 || m.entry.state != st::DELETED))
            .collect();
        changed.sort_by_key(|m| m.entry.rev);
        let mut at = 0;
        for m in changed {
            match m.entry.write(&m.text, &mut out[at..]) {
                Some(n) => at += n,
                None => break,
            }
        }
        Ok(at)
    }

    /// After a restart: what was being sent has failed (the user can retry it).
    pub fn restarted(&mut self) {
        let sending: Vec<u32> = self
            .messages
            .iter()
            .filter(|m| m.entry.state == st::SENDING)
            .map(|m| m.entry.id)
            .collect();
        for id in sending {
            self.failed(id);
        }
    }

    /// `sms.db`: "NSM1", next id, rev, floor (u32 each), reports, alert, the messages (u32 count; each an
    /// SmsEntry record, its references, the parts delivered), the waiting parts (u16 count).
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(32 + self.messages.len() * 160);
        b.extend_from_slice(MAGIC);
        for v in [self.next_id, self.rev, self.floor] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&[self.settings.reports as u8, self.settings.alert]);
        b.extend_from_slice(&(self.messages.len() as u32).to_le_bytes());
        let mut rec = alloc::vec![0u8; SmsEntry::HEAD + 2 + st::MAX_TEXT];
        for m in &self.messages {
            let n = m.entry.write(&m.text, &mut rec).unwrap_or(0);
            b.extend_from_slice(&rec[..n]);
            b.push(m.refs.len() as u8);
            b.extend_from_slice(&m.refs);
            b.push(m.delivered);
        }
        b.extend_from_slice(&(self.parts.len() as u16).to_le_bytes());
        for p in &self.parts {
            push_str(&mut b, &p.from);
            b.extend_from_slice(&p.reference.to_le_bytes());
            b.extend_from_slice(&[p.total, p.seq, p.unicode as u8, p.why]);
            b.extend_from_slice(&p.utc.to_le_bytes());
            push_str(&mut b, &p.text);
        }
        b
    }

    /// A saved store; `None` for a file that isn't one (MODEMD then starts empty).
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.get(..4)? != MAGIC {
            return None;
        }
        let mut c = Reader { b, at: 4 };
        let (next_id, rev, floor) = (c.u32()?.max(1), c.u32()?, c.u32()?);
        let settings = SmsSettings {
            reports: c.u8()? != 0,
            alert: c.u8()?,
            smsc: Text::default(),
        };
        let n = c.u32()? as usize;
        let mut messages = Vec::with_capacity(n.min(MAX_MESSAGES + MAX_TOMBSTONES));
        for _ in 0..n {
            let (entry, text, used) = SmsEntry::read(c.rest())?;
            c.at += used;
            let k = usize::from(c.u8()?);
            let refs = c.take(k)?.to_vec();
            messages.push(Message {
                entry,
                text: String::from(text),
                refs,
                delivered: c.u8()?,
            });
        }
        let k = usize::from(c.u16()?);
        let mut parts = Vec::with_capacity(k.min(MAX_PARTS_WAITING));
        for _ in 0..k {
            let from = c.str()?;
            let reference = c.u16()?;
            let [total, seq, unicode, why] = c.take(4)?.try_into().ok()?;
            let utc = i64::from_le_bytes(c.take(8)?.try_into().ok()?);
            parts.push(Part {
                from,
                reference,
                total,
                seq,
                utc,
                unicode: unicode != 0,
                why,
                text: c.str()?,
            });
        }
        Some(Self {
            messages,
            next_id,
            rev,
            floor,
            parts,
            settings,
            ..Self::default()
        })
    }
}

fn push_str(b: &mut Vec<u8>, s: &str) {
    let s = &s.as_bytes()[..s.len().min(st::MAX_TEXT)];
    b.extend_from_slice(&(s.len() as u16).to_le_bytes());
    b.extend_from_slice(s);
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let v = self.b.get(self.at..self.at.checked_add(n)?)?;
        self.at += n;
        Some(v)
    }
    fn rest(&self) -> &'a [u8] {
        &self.b[self.at..]
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn str(&mut self) -> Option<String> {
        let n = usize::from(self.u16()?);
        Some(String::from(core::str::from_utf8(self.take(n)?).ok()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_794_722_700_000;

    fn deliver(from: &str, text: &str, concat: Option<(u16, u8, u8)>) -> sms::Deliver {
        sms::Deliver {
            from: String::from(from),
            time_ms: T0 as u64,
            text: String::from(text),
            unicode: false,
            concat: concat.map(|(reference, total, seq)| sms::Concat {
                reference,
                total,
                seq,
            }),
            class: None,
        }
    }

    fn all(s: &Store, since: u32) -> Vec<(SmsEntry, String)> {
        let mut buf = alloc::vec![0u8; st::READ_ROOM];
        let n = s.read(since, st::READ_CHANGES, &mut buf).unwrap();
        let mut out = Vec::new();
        let mut at = 0;
        while at < n {
            let (e, t, used) = SmsEntry::read(&buf[at..n]).unwrap();
            out.push((e, String::from(t)));
            at += used;
        }
        out
    }

    #[test]
    fn a_message_arrives_unread_and_alerts() {
        let mut s = Store::default();
        let id = s
            .receive(deliver("+37491234567", "Hi", None), None, T0)
            .unwrap();
        assert!(s.alert && s.dirty);
        assert_eq!(s.unread(), (1, 1));
        let got = all(&s, 0);
        assert_eq!(got.len(), 1);
        assert_eq!(
            (got[0].0.id, got[0].0.state, got[0].1.as_str()),
            (id, st::UNREAD, "Hi")
        );
        assert!(got[0].0.incoming);
        let rev = s.rev;
        assert!(s.edit(st::EDIT_READ, id));
        assert_eq!(s.unread(), (0, 0));
        let changed = all(&s, rev);
        assert_eq!((changed.len(), changed[0].0.state), (1, st::READ));
    }

    #[test]
    fn blocked_messages_never_alert_and_go_after_7_days() {
        let mut s = Store::default();
        s.receive(deliver("+19005550101", "WIN", None), Some(2), T0)
            .unwrap();
        assert!(!s.alert);
        assert_eq!(s.unread(), (0, 0));
        assert_eq!(all(&s, 0)[0].0.state, st::BLOCKED);
        assert!(!s.expire(T0 + 6 * DAY_MS));
        assert!(s.expire(T0 + 8 * DAY_MS));
        assert!(all(&s, 0).is_empty());
    }

    #[test]
    fn long_messages_wait_for_their_parts() {
        let mut s = Store::default();
        assert_eq!(
            s.receive(deliver("+37491234567", "world", Some((9, 2, 2))), None, T0),
            None
        );
        // a repeated part changes nothing
        assert_eq!(
            s.receive(deliver("091234567", "world", Some((9, 2, 2))), None, T0),
            None
        );
        // the parts survive a restart
        let mut s = Store::decode(&s.encode()).unwrap();
        let id = s
            .receive(deliver("+37491234567", "hello ", Some((9, 2, 1))), None, T0)
            .unwrap();
        let got = all(&s, 0);
        assert_eq!(
            (got[0].0.id, got[0].0.parts, got[0].1.as_str()),
            (id, 2, "hello world")
        );
        // one that never completes is shown after a day
        s.receive(
            deliver("+37491234567", "first ", Some((10, 3, 1))),
            None,
            T0,
        );
        assert!(s.expire(T0 + 2 * DAY_MS));
        assert_eq!(all(&s, 0)[1].1, "first ……");
    }

    #[test]
    fn sending_then_delivered() {
        let mut s = Store::default();
        let (id, pdus) = s.send("+37491234567", &"x".repeat(200), T0).unwrap();
        assert_eq!(pdus.len(), 2);
        assert_eq!(pdus[0][0] & 0x20, 0x20, "a status report is asked for");
        s.sent(id, 41, false);
        s.sent(id, 42, true);
        assert_eq!(all(&s, 0)[0].0.state, st::SENT);
        let report = |mr, status| sms::StatusReport {
            mr,
            to: String::from("091234567"),
            status,
            time_ms: (T0 + 5000) as u64,
        };
        // still trying: nothing yet; one part: not yet delivered
        assert!(!s.report(&report(41, 0x20), T0));
        assert!(!s.report(&report(41, 0), T0));
        assert!(s.report(&report(42, 0), T0));
        let e = all(&s, 0)[0].0;
        assert_eq!((e.state, e.done_utc), (st::DELIVERED, T0 + 5000));
    }

    #[test]
    fn failures_retry_and_restarts() {
        let mut s = Store::default();
        assert_eq!(s.send("+1 555 abc", "x", T0), Err(sms::Error::BadNumber));
        let (id, _) = s.send("+37491234567", "x", T0).unwrap();
        assert!(s.retry(id).is_none(), "only a failed message is sent again");
        s.failed(id);
        assert_eq!(all(&s, 0)[0].0.state, st::FAILED);
        assert_eq!(s.retry(id).map(|p| p.len()), Some(1));
        assert_eq!(all(&s, 0)[0].0.state, st::SENDING);
        let mut s = Store::decode(&s.encode()).unwrap();
        s.restarted();
        assert_eq!(all(&s, 0)[0].0.state, st::FAILED);
    }

    #[test]
    fn threads_archive_and_a_new_message_brings_them_back() {
        let mut s = Store::default();
        let a = s
            .receive(deliver("+37491234567", "hi", None), None, 1)
            .unwrap();
        let b = s
            .receive(deliver("+37491234567", "again", None), None, 2)
            .unwrap();
        let other = s.receive(deliver("+15550142", "x", None), None, 3).unwrap();
        let rev = s.rev;
        assert!(s.edit(st::EDIT_ARCHIVE_THREAD, a));
        let got = all(&s, rev);
        assert_eq!(got.len(), 2, "both messages of the thread changed");
        assert!(
            got.iter()
                .all(|(e, _)| e.archived && (e.id == a || e.id == b))
        );
        assert!(!s.edit(st::EDIT_ARCHIVE_THREAD, b), "already archived");
        // saved and read back
        let back = Store::decode(&s.encode()).unwrap();
        assert!(
            back.messages
                .iter()
                .filter(|m| m.entry.id != other)
                .all(|m| m.entry.archived)
        );
        assert!(
            !back
                .messages
                .iter()
                .any(|m| m.entry.id == other && m.entry.archived)
        );
        // a new message from that number brings the thread back
        let rev = s.rev;
        s.receive(deliver("+37491234567", "news", None), None, 4)
            .unwrap();
        assert!(all(&s, rev).iter().all(|(e, _)| !e.archived));
        assert!(s.edit(st::EDIT_ARCHIVE_THREAD, a));
        assert!(s.edit(st::EDIT_UNARCHIVE_THREAD, a));
        assert!(s.messages.iter().all(|m| !m.entry.archived));
    }

    #[test]
    fn deletions_reach_clients_as_tombstones() {
        let mut s = Store::default();
        let a = s
            .receive(deliver("+37491234567", "a", None), None, T0)
            .unwrap();
        s.receive(deliver("091234567", "b", None), None, T0);
        let c = s
            .receive(deliver("+37455000000", "c", None), None, T0)
            .unwrap();
        let rev = s.rev;
        assert!(s.edit(st::EDIT_DELETE_THREAD, a));
        let changed = all(&s, rev);
        assert_eq!(changed.len(), 2);
        assert!(
            changed
                .iter()
                .all(|(e, t)| e.state == st::DELETED && t.is_empty())
        );
        // a full read leaves them out
        assert_eq!(all(&s, 0).len(), 1);
        assert_eq!(all(&s, 0)[0].0.id, c);
        // forgotten deletions: read everything again
        for i in 0..(MAX_TOMBSTONES + 2) {
            let id = s
                .receive(deliver("+1555", &alloc::format!("{i}"), None), None, T0)
                .unwrap();
            s.edit(st::EDIT_DELETE, id);
        }
        let mut buf = [0u8; 64];
        assert_eq!(s.read(rev, st::READ_CHANGES, &mut buf), Err(-1));
    }

    #[test]
    fn newest_unread_and_partial_reads() {
        let mut s = Store::default();
        s.receive(deliver("+37491234567", "old", None), None, T0);
        s.receive(deliver("Ucom", "new", None), None, T0);
        assert_eq!(s.unread(), (2, 2));
        let mut buf = [0u8; 200];
        let n = s.read(0, st::READ_NEWEST_UNREAD, &mut buf).unwrap();
        assert_eq!(SmsEntry::read(&buf[..n]).unwrap().1, "new");
        // a buffer with room for one record gets one; the client asks again from its revision
        let mut small = [0u8; SmsEntry::HEAD + 2 + 3];
        let n = s.read(0, st::READ_CHANGES, &mut small).unwrap();
        let (e, t, _) = SmsEntry::read(&small[..n]).unwrap();
        assert_eq!(t, "old");
        let n = s.read(e.rev, st::READ_CHANGES, &mut small).unwrap();
        assert_eq!(SmsEntry::read(&small[..n]).unwrap().1, "new");
    }
}
