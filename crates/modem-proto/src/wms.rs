// SPDX-License-Identifier: Apache-2.0
//! QMI WMS (service 5): SMS (docs/hardware/modem.md C9). Message ids and TLV layouts from libqmi's `qmi-service-wms.json`
//! (facts), checked against the C1 ModemManager capture where it had them: Set Event Report, Set/Get Routes, the
//! SMSC Address indication, List Messages (which needs a message tag), the transfer-route Event Report (a bare
//! TPDU, no SMSC address in front) and Send Ack. Raw Send, Raw Read and Delete are from libqmi only.

use crate::qmi::Msg;
use alloc::vec::Vec;

pub const SERVICE: u32 = 5;
pub const SET_EVENT_REPORT: u16 = 0x01;
/// the indication of Set Event Report
pub const EVENT_REPORT_IND: u16 = 0x01;
pub const RAW_SEND: u16 = 0x20;
pub const RAW_READ: u16 = 0x22;
pub const DELETE: u16 = 0x24;
pub const LIST_MESSAGES: u16 = 0x31;
pub const SET_ROUTES: u16 = 0x32;
pub const GET_SMSC: u16 = 0x34;
pub const SET_SMSC: u16 = 0x35;
pub const SEND_ACK: u16 = 0x37;
pub const SMSC_IND: u16 = 0x46;

/// QmiWmsStorageType
pub const STORAGE_UIM: u8 = 0;
pub const STORAGE_NV: u8 = 1;
const STORAGE_NONE: u8 = 0xFF;
/// QmiWmsMessageTag: what List Messages asks for (incoming, read or not)
pub const TAG_MT_READ: u8 = 0;
pub const TAG_MT_NOT_READ: u8 = 1;
/// QmiWmsMessageMode / QmiWmsMessageFormat / QmiWmsMessageProtocol for GSM/WCDMA (and LTE's SGs)
const MODE_GW: u8 = 1;
const FORMAT_GW_PP: u8 = 6;
const PROTOCOL_WCDMA: u8 = 1;
/// QmiWmsReceiptAction
const STORE_AND_NOTIFY: u8 = 1;
const TRANSFER_ONLY: u8 = 2;

/// Set Event Report: new MT message indications (TLV 0x10) on.
pub const EVENT_REPORT: [(u8, &[u8]); 1] = [(0x10, &[1])];

/// Set Routes: point-to-point classes 0, 1, 3 and none come to us without being stored (we acknowledge each once
/// MODEMD saved it); class 2 (SIM data) stays with the SIM. Status reports come to us too (TLV 0x10).
pub fn routes() -> Vec<u8> {
    let mut b = alloc::vec![5, 0];
    for (class, storage, action) in [
        (0, STORAGE_NONE, TRANSFER_ONLY),
        (1, STORAGE_NONE, TRANSFER_ONLY),
        (2, STORAGE_UIM, STORE_AND_NOTIFY),
        (3, STORAGE_NONE, TRANSFER_ONLY),
        (4, STORAGE_NONE, TRANSFER_ONLY),
    ] {
        b.extend_from_slice(&[0, class, storage, action]);
    }
    b
}

/// What an Event Report indication brought.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// a message handed over (TLV 0x11): to acknowledge with `txn` when `ack`
    Transfer { ack: bool, txn: u32, tpdu: Vec<u8> },
    /// a message stored by the modem (TLV 0x10): read it, then delete it
    Stored { storage: u8, index: u32 },
}

pub fn event(m: &Msg) -> Option<Event> {
    if let Some(t) = m.tlv(0x11) {
        // {ack indicator (0 = send an ack), transaction id u32, format, raw length u16, raw}
        let txn = u32::from_le_bytes(t.get(1..5)?.try_into().ok()?);
        if *t.get(5)? != FORMAT_GW_PP {
            return Some(Event::Transfer {
                ack: t[0] == 0,
                txn,
                tpdu: Vec::new(),
            });
        }
        let n = usize::from(u16::from_le_bytes(t.get(6..8)?.try_into().ok()?));
        return Some(Event::Transfer {
            ack: t[0] == 0,
            txn,
            tpdu: t.get(8..8 + n)?.to_vec(),
        });
    }
    let t = m.tlv(0x10)?;
    Some(Event::Stored {
        storage: *t.first()?,
        index: u32::from_le_bytes(t.get(1..5)?.try_into().ok()?),
    })
}

/// Send Ack TLV 0x01: {transaction id, protocol, success}.
pub fn ack(txn: u32) -> [u8; 6] {
    let t = txn.to_le_bytes();
    [t[0], t[1], t[2], t[3], PROTOCOL_WCDMA, 1]
}

/// Raw Send TLV 0x01: {format, length u16, raw}. The raw message is the TPDU after an empty SMSC address (0x00:
/// the SIM's message centre), as ModemManager sends it.
pub fn raw_send(tpdu: &[u8]) -> Vec<u8> {
    let mut b = alloc::vec![FORMAT_GW_PP];
    b.extend_from_slice(&((tpdu.len() + 1) as u16).to_le_bytes());
    b.push(0);
    b.extend_from_slice(tpdu);
    b
}

/// Raw Send's answer: the message reference (TLV 0x01, u16; TP-MR is its low byte).
pub fn sent_reference(m: &Msg) -> Option<u8> {
    m.tlv(0x01).and_then(|t| t.first().copied())
}

/// Raw Read / Delete TLV 0x01: {storage, index u32}.
pub fn location(storage: u8, index: u32) -> [u8; 5] {
    let i = index.to_le_bytes();
    [storage, i[0], i[1], i[2], i[3]]
}

/// The GSM/WCDMA message mode TLV (0x10 on Raw Read, 0x12 on Delete and List Messages).
pub const MODE: [u8; 1] = [MODE_GW];

/// Raw Read's answer TLV 0x01: {tag, format, length u16, raw}.
pub fn read_raw(m: &Msg) -> Option<Vec<u8>> {
    let t = m.tlv(0x01)?;
    if *t.get(1)? != FORMAT_GW_PP {
        return None;
    }
    let n = usize::from(u16::from_le_bytes(t.get(2..4)?.try_into().ok()?));
    Some(t.get(4..4 + n)?.to_vec())
}

/// List Messages' answer TLV 0x01: {count u32, then {index u32, tag}}: the indexes.
pub fn listed(m: &Msg) -> Vec<u32> {
    let Some(t) = m.tlv(0x01) else {
        return Vec::new();
    };
    let n = t
        .get(..4)
        .map_or(0, |c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]) as usize);
    t.get(4..)
        .unwrap_or(&[])
        .as_chunks::<5>()
        .0
        .iter()
        .take(n)
        .map(|e| u32::from_le_bytes([e[0], e[1], e[2], e[3]]))
        .collect()
}

/// The SMSC (Get SMSC Address's answer, the SMSC Address indication): TLV 0x01 {type: 3 ASCII digits, length,
/// digits}. "145" (international) numbers get a +.
pub fn smsc(m: &Msg) -> Option<alloc::string::String> {
    let t = m.tlv(0x01)?;
    let n = usize::from(*t.get(3)?);
    let digits = core::str::from_utf8(t.get(4..4 + n)?).ok()?;
    let plus = t.get(..3) == Some(b"145") && !digits.starts_with('+');
    Some(alloc::format!("{}{digits}", if plus { "+" } else { "" }))
}

/// Set SMSC Address: TLV 0x01 the digits, TLV 0x10 the type ("145" for a + number, else "129").
pub fn set_smsc(number: &str) -> (Vec<u8>, &'static [u8]) {
    let digits: Vec<u8> = number.bytes().filter(u8::is_ascii_digit).collect();
    let toa: &'static [u8] = if number.trim().starts_with('+') {
        b"145"
    } else {
        b"129"
    };
    (digits, toa)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qmi::{Builder, INDICATION, RESPONSE};

    #[test]
    fn transfer_route_event() {
        let tpdu = [0x04, 0x04, 0x81, 0x10, 0x10];
        let mut t = alloc::vec![0, 1, 0, 0, 0, FORMAT_GW_PP, 5, 0];
        t.extend_from_slice(&tpdu);
        let b = Builder::new(INDICATION, 3, EVENT_REPORT_IND)
            .tlv(0x11, &t)
            .finish();
        let m = Msg::parse(&b).unwrap();
        assert_eq!(
            event(&m),
            Some(Event::Transfer {
                ack: true,
                txn: 1,
                tpdu: tpdu.to_vec()
            })
        );
        // the capture's ack: transaction 1, WCDMA, success
        assert_eq!(ack(1), [1, 0, 0, 0, 1, 1]);
    }

    #[test]
    fn stored_event_list_and_read() {
        let b = Builder::new(INDICATION, 3, EVENT_REPORT_IND)
            .tlv(0x10, &location(STORAGE_NV, 7))
            .finish();
        assert_eq!(
            event(&Msg::parse(&b).unwrap()),
            Some(Event::Stored {
                storage: STORAGE_NV,
                index: 7
            })
        );
        let b = Builder::new(RESPONSE, 4, LIST_MESSAGES)
            .result(0)
            .tlv(0x01, &[2, 0, 0, 0, 3, 0, 0, 0, 1, 9, 0, 0, 0, 0])
            .finish();
        assert_eq!(listed(&Msg::parse(&b).unwrap()), [3, 9]);
        let b = Builder::new(RESPONSE, 5, RAW_READ)
            .result(0)
            .tlv(0x01, &[1, FORMAT_GW_PP, 2, 0, 0xAB, 0xCD])
            .finish();
        assert_eq!(
            read_raw(&Msg::parse(&b).unwrap()),
            Some(alloc::vec![0xAB, 0xCD])
        );
    }

    #[test]
    fn smsc_and_routes() {
        let mut t = b"145".to_vec();
        t.push(12);
        t.extend_from_slice(b"+37495980150");
        let b = Builder::new(INDICATION, 1, SMSC_IND).tlv(0x01, &t).finish();
        assert_eq!(
            smsc(&Msg::parse(&b).unwrap()).as_deref(),
            Some("+37495980150")
        );
        assert_eq!(
            set_smsc("+374 95 980150"),
            (b"37495980150".to_vec(), &b"145"[..])
        );
        let r = routes();
        assert_eq!((r.len(), r[0]), (2 + 5 * 4, 5));
        assert_eq!(raw_send(&[0x11, 0x22]), [FORMAT_GW_PP, 3, 0, 0, 0x11, 0x22]);
    }
}
