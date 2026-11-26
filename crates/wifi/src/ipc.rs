// SPDX-License-Identifier: Apache-2.0
//! NeoWifi: WIFID.EXE's server (docs hardware.md S5). WIFID joins networks, runs the WPA2 handshake, remembers
//! networks and rejoins them on its own; neoui's Settings shows the scan and the link and asks to join or leave.
//! - [`op::SCAN`] arg 0 = 1 to start a new scan; arg 1 = writable des8 of [`crate::kernel::SCAN_SIZE`]: the kernel's
//!   scan record, unchanged.
//! - [`op::LINK`] arg 0 = writable des8 of [`crate::kernel::LINK_SIZE`]: the kernel's link record; completes with
//!   why the last join gave up ([`fail`], 0 = it didn't).
//! - [`op::CONNECT`] arg 0 = des8 [`Connect`]: join that network of the latest scan (a password of 8..=63
//!   characters, or none for an open network); a running join is left first.
//! - [`op::LEAVE`] arg 0 = 1 to forget the network too. It isn't rejoined on its own until CONNECT.
//! - [`op::RADIO`] arg 0 = Settings' Wi-Fi switch: off, nothing is joined or scanned on its own.
//! - [`op::SAVED`] (1.1) arg 0 = writable des8, [`SAVED_SIZE`] holds any list: the remembered networks' names, newest
//!   first ([`encode_names`]; never their keys); completes with the count, `KErrOverflow` if this list doesn't fit.
//! - [`op::FORGET`] (1.1) arg 0 = des8 SSID (1..=32 bytes, else `KErrArgument`): forget that network, leaving it
//!   first if it is joined or being joined (as LEAVE 1); `KErrNotFound` if it isn't remembered, and nothing changes.
//! - [`op::RESET`] (1.2, Settings' Reset network settings): leave, forget every network (`wifi.dat` becomes a valid
//!   empty file, so the old SYSSTART copy is never moved in again) and the left network; completes with the save's
//!   error, if any (the networks stay then).
use alloc::vec::Vec;

pub const NAME: &str = "NeoWifi";
/// major, minor, build
pub const VERSION: (i8, i8, i16) = (1, 2, 0);

/// [`op::SAVED`]'s buffer: a count, then at most [`crate::kernel::Saved::MAX`] names of up to 32 bytes.
pub const SAVED_SIZE: usize = 1 + crate::kernel::Saved::MAX * 33;

pub mod op {
    pub const SCAN: i32 = 1;
    pub const LINK: i32 = 2;
    pub const CONNECT: i32 = 3;
    pub const LEAVE: i32 = 4;
    pub const RADIO: i32 = 5;
    pub const SAVED: i32 = 6;
    pub const FORGET: i32 = 7;
    pub const RESET: i32 = 8;
}

/// [`op::SAVED`]'s record: `{count, {ssid length, ssid}*}`.
pub fn encode_names<'a>(names: impl Iterator<Item = &'a [u8]>) -> Vec<u8> {
    let mut b = alloc::vec![0];
    for n in names {
        b[0] += 1;
        b.push(n.len() as u8);
        b.extend_from_slice(n);
    }
    b
}

/// The names in an [`op::SAVED`] record; `None` if it is damaged or longer than [`SAVED_SIZE`] allows.
pub fn decode_names(b: &[u8]) -> Option<Vec<Vec<u8>>> {
    let (&count, mut rest) = b.split_first()?;
    if usize::from(count) > crate::kernel::Saved::MAX {
        return None;
    }
    let mut v = Vec::new();
    for _ in 0..count {
        let (&n, r) = rest.split_first()?;
        let n = usize::from(n);
        if n == 0 || n > 32 || r.len() < n {
            return None;
        }
        v.push(r[..n].to_vec());
        rest = &r[n..];
    }
    rest.is_empty().then_some(v)
}

/// Why a join gave up ([`op::LINK`]'s result)
pub mod fail {
    /// not WPA2-PSK with CCMP
    pub const SECURITY: i32 = 1;
    /// no random numbers for the handshake
    pub const RANDOM: i32 = 2;
    /// the network's security changed during the handshake
    pub const DOWNGRADE: i32 = 3;
    /// the kernel refused (no Wi-Fi)
    pub const HAL: i32 = 4;
    /// the AP kept resending message 1: a wrong password
    pub const HANDSHAKE: i32 = 5;
    /// associated, but not one EAPOL frame arrived: the firmware's data path is down (docs known-issues, the
    /// reason-4 drops). No automatic rejoin until the modem restarts, since each such join makes the firmware assert.
    pub const NO_DATA: i32 = 6;
}

/// [`op::CONNECT`]'s argument: `{bssid 6, ssid length, ssid, password length, password}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connect {
    pub bssid: [u8; 6],
    pub ssid: Vec<u8>,
    /// empty = an open network
    pub password: Vec<u8>,
}

impl Connect {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::from(&self.bssid[..]);
        b.push(self.ssid.len() as u8);
        b.extend_from_slice(&self.ssid);
        b.push(self.password.len() as u8);
        b.extend_from_slice(&self.password);
        b
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        let (bssid, rest) = b.split_first_chunk::<6>()?;
        let (&n, rest) = rest.split_first()?;
        let n = n as usize;
        if n > 32 || rest.len() <= n {
            return None;
        }
        let (ssid, rest) = rest.split_at(n);
        let (&p, password) = rest.split_first()?;
        if password.len() != p as usize || !(p == 0 || (8..=63).contains(&p)) {
            return None;
        }
        Some(Connect {
            bssid: *bssid,
            ssid: ssid.to_vec(),
            password: password.to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn connect_round_trip_and_password_lengths() {
        let c = Connect {
            bssid: [1, 2, 3, 4, 5, 6],
            ssid: b"home".to_vec(),
            password: b"12345678".to_vec(),
        };
        assert_eq!(Connect::decode(&c.encode()), Some(c.clone()));
        let open = Connect {
            password: vec![],
            ..c.clone()
        };
        assert_eq!(Connect::decode(&open.encode()), Some(open));
        let short = Connect {
            password: b"1234567".to_vec(),
            ..c.clone()
        };
        assert_eq!(Connect::decode(&short.encode()), None);
        assert_eq!(Connect::decode(&c.encode()[..10]), None);
    }

    #[test]
    fn saved_names_round_trip_and_limits() {
        let names: Vec<&[u8]> = vec![b"home", b"Cafe Guest", &[b'x'; 32]];
        let b = encode_names(names.iter().copied());
        assert_eq!(b[0], 3);
        assert_eq!(
            decode_names(&b),
            Some(names.iter().map(|n| n.to_vec()).collect())
        );
        assert_eq!(decode_names(&[0]), Some(vec![]));
        // a full list fits the buffer
        let full: Vec<Vec<u8>> = (0..8).map(|_| vec![b'y'; 32]).collect();
        assert_eq!(
            encode_names(full.iter().map(|n| n.as_slice())).len(),
            SAVED_SIZE
        );
        // damaged: short, trailing bytes, an empty name, too many
        assert_eq!(decode_names(&b[..b.len() - 1]), None);
        assert_eq!(decode_names(&[b.as_slice(), &[0]].concat()), None);
        assert_eq!(decode_names(&[1, 0]), None);
        assert_eq!(decode_names(&[9]), None);
        assert_eq!(decode_names(&[]), None);
    }
}
