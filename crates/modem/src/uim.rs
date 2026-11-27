// SPDX-License-Identifier: Apache-2.0
//! UIM card/PIN facts from libqmi data/qmi-service-uim.json and qmi-enums-uim.h.
//! Only the primary GW application on slot 1 is used; no PIN value is retained.
use crate::qmi::Msg;
use modem_ipc::{pin, sim};

pub const REGISTER: u16 = 0x2e;
pub const GET_STATUS: u16 = 0x2f;
pub const STATUS_IND: u16 = 0x32;
pub const VERIFY_PIN: u16 = 0x26;
/// (D11) TLV 1 session [6, 0] (card slot 1, no AID, as VERIFY and ModemManager: UIM ignores an AID for a card
/// session), TLV 2 [pin id, on, n, pin[n]]
pub const SET_PIN_PROTECTION: u16 = 0x25;
/// (D11) TLV 1 as above, TLV 2 [pin id, n, old[n], m, new[m]]
pub const CHANGE_PIN: u16 = 0x28;

/// (D11) The application the card status names: its card slot (1-based) and AID. Not sent (the request's session is
/// card slot 1's, [6, 0]); any change of it is a new epoch, so a request is only for the card the requester read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Target {
    pub slot: u8,
    pub aid_len: u8,
    pub aid: [u8; 16],
}

pub struct Card {
    pub state: u8,
    pub attempts: u8,
    /// PIN1 or UPIN for the selected application; [`pin::ID_UNKNOWN`] when no application was found
    pub pin_id: u8,
    /// the raw UIM PIN state ([`modem_ipc::pin`]: 0 unknown .. 5 permanently blocked)
    pub pin: u8,
    /// the selected application; `None` without one (or with an AID over 16 bytes)
    pub target: Option<Target>,
}

pub fn card(m: &Msg) -> Option<Card> {
    let b = m.tlv(0x10)?;
    let count = *b.get(8)?;
    if count == 0 {
        return Some(Card {
            state: sim::ABSENT,
            attempts: 255,
            pin_id: pin::ID_UNKNOWN,
            pin: pin::UNKNOWN,
            target: None,
        });
    }
    let mut card = Card {
        state: match *b.get(9)? {
            0 => sim::ABSENT,
            1 => sim::PRESENT,
            2 => sim::ERROR,
            _ => sim::UNKNOWN,
        },
        attempts: 255,
        pin_id: pin::ID_UNKNOWN,
        pin: pin::UNKNOWN,
        target: None,
    };
    if card.state != sim::PRESENT {
        return Some(card);
    }
    let primary = u16::from_le_bytes(b.get(..2)?.try_into().ok()?);
    // A provisioning index is {slot, application}; before PIN entry it may be 0xffff.
    // Then select the first SIM/USIM application on slot 1, rather than treating it as unlocked.
    // Older status vectors include only card presence. No app/PIN state can be inferred.
    if b.len() == 10 {
        return Some(card);
    }
    let mut at = 15;
    for app in 0..usize::from(*b.get(14)?) {
        let aid = usize::from(*b.get(at + 6)?);
        let tail = at + 7 + aid;
        let info = b.get(tail..tail + 7)?;
        let gsm = matches!(*b.get(at)?, 1 | 2);
        if (primary >> 8 == 0 && app == usize::from(primary & 255)) || (primary == 0xffff && gsm) {
            let upin = info[0] != 0;
            let (pin, attempts) = if upin {
                (*b.get(10)?, *b.get(11)?)
            } else {
                (info[1], info[2])
            };
            card.pin_id = if upin { pin::ID_UPIN } else { pin::ID_PIN1 };
            // this loop reads the first card only: slot 1
            card.target = b.get(at + 7..tail).filter(|a| a.len() <= 16).map(|a| {
                let mut aid = [0; 16];
                aid[..a.len()].copy_from_slice(a);
                Target {
                    slot: 1,
                    aid_len: a.len() as u8,
                    aid,
                }
            });
            card.attempts = attempts;
            // UIM's PIN states are modem-ipc's pin::* (1 required, 2 verified, 3 disabled, 4 blocked, 5 for good);
            // without one, the application state says what it can (2 PIN needed, 3 PUK needed, 5 PIN1 blocked)
            (card.state, card.pin) = match pin {
                1 => (sim::PIN_REQUIRED, pin),
                4 | 5 => (sim::BLOCKED, pin),
                2 | 3 => (sim::PRESENT, pin),
                _ => match *b.get(at + 1)? {
                    2 => (sim::PIN_REQUIRED, pin::REQUIRED),
                    3 => (sim::BLOCKED, pin::BLOCKED),
                    5 => (sim::BLOCKED, pin::PERM_BLOCKED),
                    7 => (sim::PRESENT, pin::UNKNOWN),
                    _ => (sim::UNKNOWN, pin::UNKNOWN),
                },
            };
            return Some(card);
        }
        at = tail + 7;
    }
    Some(card)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qmi::{Builder, RESPONSE};
    fn status(app_pin: u8, upin: bool) -> Card {
        let mut b = alloc::vec![255, 255, 0, 0, 0, 0, 0, 0, 1, 1, 1, 2, 10, 0, 1];
        b.extend_from_slice(&[2, 2, 0, 0, 0, 0, 0, upin as u8, app_pin, 3, 10, 3, 3, 10]);
        let bytes = Builder::new(RESPONSE, 1, GET_STATUS)
            .result(0)
            .tlv(0x10, &b)
            .finish();
        card(&Msg::parse(&bytes).unwrap()).unwrap()
    }
    #[test]
    fn pin_state_before_provisioning_and_upin_replacement() {
        let c = status(1, false);
        assert_eq!((c.state, c.attempts, c.pin_id), (sim::PIN_REQUIRED, 3, 1));
        assert_eq!(c.pin, pin::REQUIRED);
        let c = status(4, false);
        assert_eq!((c.state, c.pin), (sim::BLOCKED, pin::BLOCKED));
        let c = status(5, false);
        assert_eq!((c.state, c.pin), (sim::BLOCKED, pin::PERM_BLOCKED));
        let c = status(2, false);
        assert_eq!((c.state, c.pin), (sim::PRESENT, pin::VERIFIED));
        let c = status(3, false);
        assert_eq!(
            (c.state, c.pin, c.attempts),
            (sim::PRESENT, pin::DISABLED, 3)
        );
        // UPIN replaces PIN1: its state (byte 10) and attempts (byte 11) count
        let c = status(2, true);
        assert_eq!((c.state, c.attempts, c.pin_id), (sim::PIN_REQUIRED, 2, 3));
        assert_eq!(c.pin, pin::REQUIRED);
    }
    #[test]
    fn no_card_or_no_application_says_nothing_about_the_pin() {
        for tlv in [
            &[0u8, 0, 0, 0, 0, 0, 0, 0, 0][..],
            &[0, 0, 0, 0, 0, 0, 0, 0, 1, 1],
        ] {
            let bytes = Builder::new(RESPONSE, 1, GET_STATUS)
                .result(0)
                .tlv(0x10, tlv)
                .finish();
            let c = card(&Msg::parse(&bytes).unwrap()).unwrap();
            assert_eq!(
                (c.pin, c.pin_id, c.attempts),
                (pin::UNKNOWN, pin::ID_UNKNOWN, 255)
            );
        }
    }
    #[test]
    fn truncated_app_is_rejected() {
        let bytes = Builder::new(RESPONSE, 1, GET_STATUS)
            .result(0)
            .tlv(0x10, &[0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 3, 10, 0, 1, 2])
            .finish();
        assert!(card(&Msg::parse(&bytes).unwrap()).is_none());
    }
}
