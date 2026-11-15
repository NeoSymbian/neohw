// SPDX-License-Identifier: Apache-2.0
//! SMS TPDUs for MODEMD and the Messages app (docs/telephony.md C9), written from 3GPP TS 23.040 (the TPDUs) and
//! 23.038 (the alphabets):
//! - [`count`]: how a text will be sent (GSM 7-bit or UCS-2, the parts, the characters left), for the composer;
//! - [`submit`]: SMS-SUBMIT TPDUs for a text, split into concatenated parts;
//! - [`parse`]: an incoming SMS-DELIVER or SMS-STATUS-REPORT.
//!
//! TPDUs here carry no SMSC address in front: QMI WMS's transfer route hands over bare TPDUs. [`parse`] also accepts
//! one with the SMSC address (stored messages).
#![no_std]

extern crate alloc;

pub mod gsm7;

use alloc::string::String;
use alloc::vec::Vec;

/// The most parts a text is sent in.
pub const MAX_PARTS: usize = 10;

/// How a text goes out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Count {
    /// UCS-2 (a character outside the GSM alphabet, such as Armenian, Cyrillic or emoji)
    pub unicode: bool,
    /// the SMS it takes (1 for an empty text)
    pub parts: usize,
    /// characters (septets or UTF-16 units) left in the last part
    pub left: usize,
}

impl Count {
    /// More than [`MAX_PARTS`]: too long to send.
    pub fn too_long(&self) -> bool {
        self.parts > MAX_PARTS
    }
}

/// The units of `text` (septets, else UTF-16 units) and whether it needs UCS-2.
enum Units {
    Gsm(Vec<u8>),
    Ucs2(Vec<u16>),
}

fn units(text: &str) -> Units {
    match gsm7::encode(text) {
        Some(s) => Units::Gsm(s),
        None => Units::Ucs2(text.encode_utf16().collect()),
    }
}

/// Split `n` units into parts of at most `room`, never between `keep(i)` and `i+1` (an escape and its character, a
/// surrogate pair): the end of each part.
fn split(n: usize, room: usize, joined: impl Fn(usize) -> bool) -> Vec<usize> {
    let mut ends = Vec::new();
    let mut start = 0;
    while n - start > room {
        let mut end = start + room;
        if joined(end - 1) {
            end -= 1;
        }
        ends.push(end);
        start = end;
    }
    ends.push(n);
    ends
}

/// The part ends for `u`: one part up to 160 septets / 70 units, else 153 / 67 per part (the concat header).
fn parts(u: &Units) -> Vec<usize> {
    match u {
        Units::Gsm(s) if s.len() <= 160 => alloc::vec![s.len()],
        Units::Gsm(s) => split(s.len(), 153, |i| {
            // an ESC whose character would start the next part; ESC ESC can't occur (encode never emits it)
            s[i] == gsm7::ESC && (i == 0 || s[i - 1] != gsm7::ESC)
        }),
        Units::Ucs2(w) if w.len() <= 70 => alloc::vec![w.len()],
        Units::Ucs2(w) => split(w.len(), 67, |i| (0xD800..0xDC00).contains(&w[i])),
    }
}

pub fn count(text: &str) -> Count {
    let u = units(text);
    let ends = parts(&u);
    let (unicode, n, single, multi) = match &u {
        Units::Gsm(s) => (false, s.len(), 160, 153),
        Units::Ucs2(w) => (true, w.len(), 70, 67),
    };
    let last = n - ends.len().checked_sub(2).map_or(0, |i| ends[i]);
    let room = if ends.len() == 1 { single } else { multi };
    Count {
        unicode,
        parts: ends.len(),
        left: room - last,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// not a number the network can take (digits, an optional leading +, at most 20 digits)
    BadNumber,
    /// empty, or more than [`MAX_PARTS`] parts
    BadLength,
}

/// TP-VP relative: 7 days.
const VALIDITY_7_DAYS: u8 = 173;

/// TP-DA / TP-OA type of address.
const TOA_INTERNATIONAL: u8 = 0x91;
const TOA_UNKNOWN: u8 = 0x81;
const TON_ALPHANUMERIC: u8 = 5;

/// `number` as a TP-DA (length in digits, type, semi-octets). Spaces, dashes and brackets are dropped.
fn address(number: &str) -> Result<Vec<u8>, Error> {
    let (toa, rest) = match number.trim().strip_prefix('+') {
        Some(r) => (TOA_INTERNATIONAL, r),
        None => (TOA_UNKNOWN, number.trim()),
    };
    let digits: Vec<u8> = rest
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '(' | ')' | '.'))
        .map(|c| c.to_digit(10).map(|d| d as u8).ok_or(Error::BadNumber))
        .collect::<Result<_, _>>()?;
    if digits.is_empty() || digits.len() > 20 {
        return Err(Error::BadNumber);
    }
    let mut out = alloc::vec![digits.len() as u8, toa];
    out.extend(
        digits
            .chunks(2)
            .map(|p| p[0] | (p.get(1).copied().unwrap_or(0xF) << 4)),
    );
    Ok(out)
}

/// SMS-SUBMIT TPDUs for `text` to `to`. `report` asks for a status report; `reference` ties the parts of a long text
/// together (any byte, different from the last long text's). TP-MR is 0: the modem sets it and reports it back.
pub fn submit(to: &str, text: &str, report: bool, reference: u8) -> Result<Vec<Vec<u8>>, Error> {
    let da = address(to)?;
    if text.is_empty() {
        return Err(Error::BadLength);
    }
    let u = units(text);
    let ends = parts(&u);
    if ends.len() > MAX_PARTS {
        return Err(Error::BadLength);
    }
    let total = ends.len() as u8;
    let mut out = Vec::with_capacity(ends.len());
    let mut start = 0;
    for (i, &end) in ends.iter().enumerate() {
        let concat = total > 1;
        // TP-MTI submit, TP-VPF relative, TP-SRR, TP-UDHI
        let first = 0x01 | 0x10 | (u8::from(report) << 5) | (u8::from(concat) << 6);
        let mut p = alloc::vec![first, 0];
        p.extend_from_slice(&da);
        let udh: &[u8] = &[5, 0x00, 3, reference, total, i as u8 + 1];
        let udh = if concat { udh } else { &[] };
        match &u {
            Units::Gsm(s) => {
                p.extend_from_slice(&[0x00, 0x00, VALIDITY_7_DAYS]);
                // the header takes whole septets: 6 octets + 1 fill bit = 7 septets
                let pad = (udh.len() * 8).div_ceil(7);
                let mut septets = alloc::vec![0u8; pad];
                septets.extend_from_slice(&s[start..end]);
                let mut ud = gsm7::pack(&septets);
                ud[..udh.len()].copy_from_slice(udh);
                p.push(septets.len() as u8);
                p.extend_from_slice(&ud);
            }
            Units::Ucs2(w) => {
                p.extend_from_slice(&[0x00, 0x08, VALIDITY_7_DAYS]);
                let body: Vec<u8> = w[start..end].iter().flat_map(|c| c.to_be_bytes()).collect();
                p.push((udh.len() + body.len()) as u8);
                p.extend_from_slice(udh);
                p.extend_from_slice(&body);
            }
        }
        out.push(p);
        start = end;
    }
    Ok(out)
}

/// One part of a long message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Concat {
    pub reference: u16,
    pub total: u8,
    /// 1-based
    pub seq: u8,
}

/// An SMS-DELIVER.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deliver {
    /// `+` and digits, or an alphanumeric sender name
    pub from: String,
    /// the service centre's time stamp, UTC milliseconds since 1970 (0 if it doesn't parse)
    pub time_ms: u64,
    pub text: String,
    pub unicode: bool,
    pub concat: Option<Concat>,
    /// the message class (0 flash, 1 phone, 2 SIM, 3 terminal) when the sender gave one
    pub class: Option<u8>,
}

/// An SMS-STATUS-REPORT: what became of message `mr` sent to `to`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusReport {
    pub mr: u8,
    pub to: String,
    /// TP-ST
    pub status: u8,
    /// when it was delivered (TP-DT), UTC ms
    pub time_ms: u64,
}

impl StatusReport {
    /// Delivered (TP-ST 0x00-0x1F).
    pub fn delivered(&self) -> bool {
        self.status < 0x20
    }
    /// Failed for good (no further attempts).
    pub fn failed(&self) -> bool {
        self.status >= 0x40
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tpdu {
    Deliver(Deliver),
    Status(StatusReport),
}

struct Cursor<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn u8(&mut self) -> Option<u8> {
        let v = *self.b.get(self.at)?;
        self.at += 1;
        Some(v)
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let v = self.b.get(self.at..self.at.checked_add(n)?)?;
        self.at += n;
        Some(v)
    }
    fn rest(&self) -> &'a [u8] {
        &self.b[self.at..]
    }
}

/// TP-OA / TP-RA.
fn read_address(c: &mut Cursor) -> Option<String> {
    let digits = usize::from(c.u8()?);
    let toa = c.u8()?;
    let bytes = c.take(digits.div_ceil(2))?;
    if (toa >> 4) & 7 == TON_ALPHANUMERIC {
        let septets = digits * 4 / 7;
        return Some(gsm7::decode(&gsm7::unpack(bytes, septets)?));
    }
    let mut s = String::new();
    if toa == TOA_INTERNATIONAL {
        s.push('+');
    }
    for i in 0..digits {
        let d = (bytes[i / 2] >> ((i % 2) * 4)) & 0xF;
        s.push(match d {
            0..=9 => char::from(b'0' + d),
            0xA => '*',
            0xB => '#',
            0xC => 'a',
            0xD => 'b',
            0xE => 'c',
            _ => continue,
        });
    }
    Some(s)
}

/// Days from 1970-01-01 to a civil date (H. Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// TP-SCTS / TP-DT (7 semi-octet pairs with a quarter-hour zone) → UTC ms, 0 if it doesn't parse.
fn read_time(b: &[u8]) -> u64 {
    let bcd = |v: u8| -> Option<i64> {
        let (lo, hi) = (v & 0xF, v >> 4);
        (lo < 10 && hi < 10).then_some(i64::from(lo) * 10 + i64::from(hi))
    };
    let Some([y, mo, d, h, mi, s]) = b[..6]
        .iter()
        .map(|&v| bcd(v))
        .collect::<Option<Vec<_>>>()
        .and_then(|v| <[i64; 6]>::try_from(v).ok())
    else {
        return 0;
    };
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 59 {
        return 0;
    }
    // the zone: semi-octets in quarter hours, bit 3 of the first digit the sign
    let z = b[6];
    let quarters = i64::from(z & 0x7) * 10 + i64::from(z >> 4);
    let zone = if z & 0x8 != 0 { -quarters } else { quarters } * 15 * 60;
    let t = days_from_civil(2000 + y, mo, d) * 86_400 + h * 3600 + mi * 60 + s - zone;
    u64::try_from(t).map_or(0, |t| t * 1000)
}

/// The data coding scheme: (alphabet 0 GSM / 1 8-bit / 2 UCS-2, class).
fn coding(dcs: u8) -> Option<(u8, Option<u8>)> {
    match dcs >> 4 {
        // general data coding; compressed text isn't supported
        0..=3 if dcs & 0x20 != 0 => None,
        0..=3 => Some(((dcs >> 2) & 3, (dcs & 0x10 != 0).then_some(dcs & 3))),
        // automatic deletion group: as general
        4..=7 if dcs & 0x20 != 0 => None,
        4..=7 => Some(((dcs >> 2) & 3, (dcs & 0x10 != 0).then_some(dcs & 3))),
        // message waiting: discard / store (GSM), store (UCS-2)
        0xC | 0xD => Some((0, None)),
        0xE => Some((2, None)),
        0xF => Some((u8::from(dcs & 4 != 0), Some(dcs & 3))),
        _ => None,
    }
}

/// The user data → (text, concat). `udl` is in septets for GSM, octets otherwise.
fn user_data(alphabet: u8, udhi: bool, udl: usize, ud: &[u8]) -> Option<(String, Option<Concat>)> {
    let (mut concat, mut header) = (None, 0);
    if udhi {
        let udhl = usize::from(*ud.first()?);
        let h = ud.get(1..1 + udhl)?;
        header = 1 + udhl;
        let mut i = 0;
        while i + 2 <= h.len() {
            let (iei, len) = (h[i], usize::from(h[i + 1]));
            let v = h.get(i + 2..i + 2 + len)?;
            match (iei, len) {
                (0x00, 3) if v[1] > 0 && v[2] > 0 => {
                    concat = Some(Concat {
                        reference: u16::from(v[0]),
                        total: v[1],
                        seq: v[2],
                    })
                }
                (0x08, 4) if v[2] > 0 && v[3] > 0 => {
                    concat = Some(Concat {
                        reference: u16::from_be_bytes([v[0], v[1]]),
                        total: v[2],
                        seq: v[3],
                    })
                }
                _ => {}
            }
            i += 2 + len;
        }
    }
    let text = match alphabet {
        0 => {
            let septets = gsm7::unpack(ud, udl)?;
            gsm7::decode(septets.get((header * 8).div_ceil(7)..)?)
        }
        2 => {
            let body = ud.get(header..udl)?;
            let w: Vec<u16> = body
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&p| u16::from_be_bytes(p))
                .collect();
            char::decode_utf16(w)
                .map(|c| c.unwrap_or('\u{FFFD}'))
                .collect()
        }
        // 8-bit data: shown as Latin-1 (a binary message has no text)
        _ => ud
            .get(header..udl)?
            .iter()
            .map(|&b| char::from(b))
            .collect(),
    };
    Some((text, concat))
}

fn parse_tpdu(b: &[u8]) -> Option<Tpdu> {
    let mut c = Cursor { b, at: 0 };
    let first = c.u8()?;
    match first & 3 {
        0 => {
            let from = read_address(&mut c)?;
            let _pid = c.u8()?;
            let (alphabet, class) = coding(c.u8()?)?;
            let time_ms = read_time(c.take(7)?);
            let udl = usize::from(c.u8()?);
            let need = if alphabet == 0 {
                (udl * 7).div_ceil(8)
            } else {
                udl
            };
            let ud = c.rest();
            if ud.len() != need {
                return None;
            }
            let (text, concat) = user_data(alphabet, first & 0x40 != 0, udl, ud)?;
            Some(Tpdu::Deliver(Deliver {
                from,
                time_ms,
                text,
                unicode: alphabet == 2,
                concat,
                class,
            }))
        }
        2 => {
            let mr = c.u8()?;
            let to = read_address(&mut c)?;
            let _scts = c.take(7)?;
            let time_ms = read_time(c.take(7)?);
            let status = c.u8()?;
            // optional TP-PI and what it announces may follow
            Some(Tpdu::Status(StatusReport {
                mr,
                to,
                status,
                time_ms,
            }))
        }
        _ => None,
    }
}

/// An incoming TPDU, bare or after an SMSC address.
pub fn parse(raw: &[u8]) -> Option<Tpdu> {
    parse_tpdu(raw).or_else(|| {
        let smsc = usize::from(*raw.first()?);
        parse_tpdu(raw.get(1 + smsc..)?)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    /// An SMS-DELIVER for `text` from `from`, built by turning our own SUBMIT around (same user data layout).
    fn deliver_of(submit: &[u8], from: &str) -> Vec<u8> {
        let first = submit[0];
        let da_len = 2 + usize::from(submit[2]).div_ceil(2);
        let rest = &submit[2 + da_len..]; // PID, DCS, VP, UDL, UD
        let mut p = alloc::vec![first & 0x40]; // MTI deliver, keep UDHI
        p.extend_from_slice(&address(from).unwrap());
        p.extend_from_slice(&rest[..2]);
        // 2026-11-15 10:05:00 +04:00 (16 quarters)
        p.extend_from_slice(&[0x62, 0x11, 0x51, 0x01, 0x50, 0x00, 0x61]);
        p.extend_from_slice(&rest[3..]);
        p
    }

    fn roundtrip(text: &str) -> Vec<Deliver> {
        let s = submit("+37491234567", text, true, 7).unwrap();
        s.iter()
            .map(|p| match parse(&deliver_of(p, "+37491234567")).unwrap() {
                Tpdu::Deliver(d) => d,
                t => panic!("{t:?}"),
            })
            .collect()
    }

    #[test]
    fn counts_follow_the_part_sizes() {
        assert_eq!(
            count(""),
            Count {
                unicode: false,
                parts: 1,
                left: 160
            }
        );
        assert_eq!(
            count(&"a".repeat(160)),
            Count {
                unicode: false,
                parts: 1,
                left: 0
            }
        );
        assert_eq!(
            count(&"a".repeat(161)),
            Count {
                unicode: false,
                parts: 2,
                left: 145
            }
        );
        assert_eq!(
            count(&"a".repeat(300)),
            Count {
                unicode: false,
                parts: 2,
                left: 6
            }
        );
        assert_eq!(count("€").left, 158);
        assert_eq!(
            count(&"Ա".repeat(70)),
            Count {
                unicode: true,
                parts: 1,
                left: 0
            }
        );
        assert_eq!(
            count(&"Ա".repeat(300)),
            Count {
                unicode: true,
                parts: 5,
                left: 35
            }
        );
        assert_eq!(
            count("Привет"),
            Count {
                unicode: true,
                parts: 1,
                left: 64
            }
        );
        assert!(count(&"a".repeat(1531)).too_long());
    }

    #[test]
    fn an_escape_never_splits_across_parts() {
        let text = alloc::format!("{}€tail and more", "a".repeat(152));
        let parts = roundtrip(&text);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].text, "a".repeat(152));
        assert_eq!(parts[1].text, "€tail and more");
    }

    #[test]
    fn short_gsm_text_round_trips() {
        let p = roundtrip("Hi! See you at 7 [ok] €5");
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].text, "Hi! See you at 7 [ok] €5");
        assert_eq!(p[0].from, "+37491234567");
        assert!(!p[0].unicode && p[0].concat.is_none());
        // 2026-11-15 10:05:00 +04:00 = 06:05:00 UTC
        assert_eq!(p[0].time_ms, 1_794_722_700_000);
    }

    #[test]
    fn long_texts_split_and_carry_the_concat_header() {
        let text = "x".repeat(300);
        let p = roundtrip(&text);
        assert_eq!(p.len(), 2);
        assert_eq!(
            p[0].concat,
            Some(Concat {
                reference: 7,
                total: 2,
                seq: 1
            })
        );
        assert_eq!(
            p[1].concat,
            Some(Concat {
                reference: 7,
                total: 2,
                seq: 2
            })
        );
        assert_eq!(alloc::format!("{}{}", p[0].text, p[1].text), text);

        let hy = "Բարև, ինչպե՞ս ես։ ".repeat(17);
        let p = roundtrip(&hy);
        assert!(p.iter().all(|d| d.unicode));
        assert_eq!(p.iter().map(|d| d.text.as_str()).collect::<String>(), hy);
    }

    #[test]
    fn surrogate_pairs_stay_in_one_part() {
        let text = alloc::format!("{}👍{}", "я".repeat(66), "я".repeat(5));
        let p = roundtrip(&text);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].text, "я".repeat(66));
        assert_eq!(p[1].text, alloc::format!("👍{}", "я".repeat(5)));
    }

    #[test]
    fn submit_layout() {
        let s = submit("+37491", "hellohello", true, 0).unwrap();
        assert_eq!(
            s[0],
            [
                0x31, 0x00, 0x05, 0x91, 0x73, 0x94, 0xF1, 0x00, 0x00, 173, 10, 0xE8, 0x32, 0x9B,
                0xFD, 0x46, 0x97, 0xD9, 0xEC, 0x37
            ]
        );
        assert_eq!(
            submit("091 23-45", "x", false, 0).unwrap()[0][2..6],
            [7, 0x81, 0x90, 0x21]
        );
        assert_eq!(submit("+1 555 abc", "x", false, 0), Err(Error::BadNumber));
        assert_eq!(submit("+1555", "", false, 0), Err(Error::BadLength));
    }

    #[test]
    fn alphanumeric_senders_and_smsc_prefix() {
        // OA "Ucom": 4 septets = 28 bits = 7 semi-octets
        let mut p = alloc::vec![0x04, 7, 0xD0];
        p.extend_from_slice(&gsm7::pack(&gsm7::encode("Ucom").unwrap()));
        p.extend_from_slice(&[0x00, 0x00, 0x62, 0x11, 0x51, 0x01, 0x50, 0x00, 0x00, 2]);
        p.extend_from_slice(&gsm7::pack(&gsm7::encode("Hi").unwrap()));
        let Some(Tpdu::Deliver(d)) = parse(&p) else {
            panic!()
        };
        assert_eq!((d.from.as_str(), d.text.as_str()), ("Ucom", "Hi"));
        // the same after an SMSC address (+37495…)
        let mut with = alloc::vec![0x07, 0x91, 0x73, 0x94, 0x95, 0x08, 0x51, 0xF0];
        with.extend_from_slice(&p);
        assert_eq!(parse(&with), Some(Tpdu::Deliver(d)));
    }

    #[test]
    fn status_reports() {
        let mut p = alloc::vec![0x06, 42];
        p.extend_from_slice(&address("+37491234567").unwrap());
        p.extend_from_slice(&[0x62, 0x11, 0x51, 0x01, 0x50, 0x00, 0x61]);
        p.extend_from_slice(&[0x62, 0x11, 0x51, 0x01, 0x50, 0x20, 0x61]);
        p.push(0x00);
        let Some(Tpdu::Status(r)) = parse(&p) else {
            panic!()
        };
        assert_eq!(
            (r.mr, r.to.as_str(), r.delivered()),
            (42, "+37491234567", true)
        );
        assert_eq!(r.time_ms, 1_794_722_702_000);
        *p.last_mut().unwrap() = 0x41;
        let Some(Tpdu::Status(r)) = parse(&p) else {
            panic!()
        };
        assert!(!r.delivered() && r.failed());
        assert_eq!(r.to.to_string(), "+37491234567");
    }
}
