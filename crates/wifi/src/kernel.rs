// SPDX-License-Identifier: Apache-2.0
//! The kernel's Wi-Fi records (kernel qcom_wcn3990.cpp, docs hardware/wifi.md W3/W4), as WIFID needs them: which
//! networks a scan found and where the join is. neoui decodes the same bytes for its pages (neoui-proto `wifi`), so
//! NeoWifi passes them through unchanged. And the saved networks' file format.
use alloc::vec::Vec;

/// HAL WlanScan's record: `{version 1, state, count, scans % 256}` then per network
/// `{bssid 6, channel, dBm (i8), flags (1 secured), ssid length, ssid 32}`.
pub const SCAN_SIZE: usize = 4 + 32 * ENTRY;
const ENTRY: usize = 42;
/// HAL WlanLink op 3's record: 29 bytes and the AP's RSN element (up to 64).
pub const LINK_SIZE: usize = 29 + 64;

/// One network of a scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Network {
    pub bssid: [u8; 6],
    pub secured: bool,
    /// as the beacon carried it; empty = hidden
    pub ssid: Vec<u8>,
}

/// A scan: the radio's state (0 off, 1 starting, 2 ready, 3 scanning, 4 error), its scan counter, the networks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scan {
    pub state: u8,
    pub scans: u8,
    pub networks: Vec<Network>,
}

impl Scan {
    pub fn decode(b: &[u8]) -> Option<Self> {
        let (&[version, state, count, scans], rest) = b.split_first_chunk::<4>()?;
        if version != 1 || state > 4 || count as usize > 32 || rest.len() != count as usize * ENTRY
        {
            return None;
        }
        let mut networks = Vec::new();
        for e in rest.as_chunks::<ENTRY>().0 {
            let len = e[9] as usize;
            if len > 32 {
                return None;
            }
            networks.push(Network {
                bssid: e[..6].try_into().ok()?,
                secured: e[8] & 1 != 0,
                ssid: e[10..10 + len].to_vec(),
            });
        }
        Some(Scan {
            state,
            scans,
            networks,
        })
    }
}

/// The join: idle, joining, the key handshake, connected, lost, error.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LinkState {
    #[default]
    Idle,
    Joining,
    Handshake,
    Connected,
    Lost,
    Error,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Link {
    pub state: LinkState,
    pub bssid: [u8; 6],
    /// our MAC address
    pub mac: [u8; 6],
    /// the AP's RSN element (for the handshake)
    pub rsn: Vec<u8>,
}

impl Link {
    pub fn decode(b: &[u8]) -> Option<Self> {
        let (&[version, state], _) = b.split_first_chunk::<2>()?;
        if version != 1 || b.len() < 29 || b.len() != 29 + b[28] as usize || b[28] > 64 {
            return None;
        }
        let state = match state {
            0 => LinkState::Idle,
            1 => LinkState::Joining,
            2 => LinkState::Handshake,
            3 => LinkState::Connected,
            4 => LinkState::Lost,
            5 => LinkState::Error,
            _ => return None,
        };
        Some(Link {
            state,
            bssid: b[8..14].try_into().ok()?,
            mac: b[14..20].try_into().ok()?,
            rsn: b[29..].to_vec(),
        })
    }

    /// Nothing joined, nothing being joined.
    pub fn idle(&self) -> bool {
        matches!(
            self.state,
            LinkState::Idle | LinkState::Lost | LinkState::Error
        )
    }
}

/// The remembered networks (SSID + PMK, never the password), newest first: `{ssid length, ssid, pmk 32}`*.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Saved(pub Vec<(Vec<u8>, [u8; 32])>);

impl Saved {
    pub const MAX: usize = 8;

    pub fn decode(mut b: &[u8]) -> Self {
        let mut v = Vec::new();
        while let Some((&n, rest)) = b.split_first() {
            let n = n as usize;
            if n == 0 || n > 32 || rest.len() < n + 32 || v.len() == Self::MAX {
                break; // a damaged tail is dropped, the networks before it kept
            }
            let mut pmk = [0; 32];
            pmk.copy_from_slice(&rest[n..n + 32]);
            v.push((rest[..n].to_vec(), pmk));
            b = &rest[n + 32..];
        }
        Saved(v)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::new();
        for (ssid, pmk) in &self.0 {
            b.push(ssid.len() as u8);
            b.extend_from_slice(ssid);
            b.extend_from_slice(pmk);
        }
        b
    }

    pub fn pmk(&self, ssid: &[u8]) -> Option<[u8; 32]> {
        self.0.iter().find(|(s, _)| s == ssid).map(|(_, p)| *p)
    }

    pub fn forget(&mut self, ssid: &[u8]) {
        self.0.retain(|(s, _)| s != ssid);
    }

    /// Remember (or refresh) a network as the newest.
    pub fn remember(&mut self, ssid: &[u8], pmk: [u8; 32]) {
        if ssid.is_empty() || ssid.len() > 32 {
            return;
        }
        self.0.retain(|(s, _)| s != ssid);
        self.0.insert(0, (ssid.to_vec(), pmk));
        self.0.truncate(Self::MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn scan_and_link_records() {
        let mut b = vec![1, 2, 1, 7];
        let mut e = vec![0u8; ENTRY];
        e[..6].copy_from_slice(&[1, 2, 3, 4, 5, 6]);
        e[8] = 1;
        e[9] = 3;
        e[10..13].copy_from_slice(b"net");
        b.extend_from_slice(&e);
        let s = Scan::decode(&b).unwrap();
        assert_eq!((s.state, s.scans), (2, 7));
        assert_eq!(s.networks[0].ssid, b"net");
        assert!(s.networks[0].secured);
        assert_eq!(Scan::decode(&b[..b.len() - 1]), None);

        let mut l = vec![0u8; 29];
        l[0] = 1;
        l[1] = 3;
        l[8..14].copy_from_slice(&[9; 6]);
        l[28] = 2;
        l.extend_from_slice(&[0x30, 0]);
        let k = Link::decode(&l).unwrap();
        assert_eq!(k.state, LinkState::Connected);
        assert_eq!(k.rsn, [0x30, 0]);
        assert!(!k.idle());
        l[1] = 9;
        assert_eq!(Link::decode(&l), None);
    }

    #[test]
    fn saved_round_trip_newest_first() {
        let mut s = Saved::default();
        s.remember(b"a", [1; 32]);
        s.remember(b"b", [2; 32]);
        s.remember(b"a", [3; 32]);
        let d = Saved::decode(&s.encode());
        assert_eq!(d, s);
        assert_eq!(d.0[0].0, b"a");
        assert_eq!(d.pmk(b"a"), Some([3; 32]));
        let mut damaged = s.encode();
        damaged.push(40);
        assert_eq!(Saved::decode(&damaged).0.len(), 2);
    }
}
