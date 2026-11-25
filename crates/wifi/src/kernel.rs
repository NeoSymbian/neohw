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
    /// the AP's channel (1-14: 2.4 GHz, above: 5 GHz) and its signal
    pub channel: u8,
    pub dbm: i8,
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
                channel: e[6],
                dbm: e[7] as i8,
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

    /// The access point to join for `ssid`: its strongest 5 GHz one while that is at least [`GOOD_5GHZ_DBM`] (more
    /// room and less interference, as Android and wpa_supplicant prefer it), else its strongest in either band.
    pub fn best(&self, ssid: &[u8]) -> Option<&Network> {
        self.networks
            .iter()
            .filter(|n| !ssid.is_empty() && n.ssid == ssid)
            .max_by_key(|n| (n.channel > 14 && n.dbm >= GOOD_5GHZ_DBM, n.dbm))
    }
}

/// A 5 GHz signal this strong or better is preferred to 2.4 GHz (a -75 dBm 5 GHz link dropped on beacon misses,
/// docs hardware/wifi.md)
pub const GOOD_5GHZ_DBM: i8 = -70;

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

    /// Forget a network; `false` if it wasn't remembered.
    pub fn forget(&mut self, ssid: &[u8]) -> bool {
        let before = self.0.len();
        self.0.retain(|(s, _)| s != ssid);
        self.0.len() != before
    }

    /// The names, newest first.
    pub fn names(&self) -> impl Iterator<Item = &[u8]> {
        self.0.iter().map(|(s, _)| s.as_slice())
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
        e[6] = 44;
        e[7] = (-46i8) as u8;
        e[8] = 1;
        e[9] = 3;
        e[10..13].copy_from_slice(b"net");
        b.extend_from_slice(&e);
        let s = Scan::decode(&b).unwrap();
        assert_eq!((s.state, s.scans), (2, 7));
        assert_eq!(s.networks[0].ssid, b"net");
        assert!(s.networks[0].secured);
        assert_eq!((s.networks[0].channel, s.networks[0].dbm), (44, -46));
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

    fn ap(last: u8, channel: u8, dbm: i8) -> Network {
        Network {
            bssid: [0x82, 0x99, 0xc1, 0xe9, 0x33, last],
            channel,
            dbm,
            secured: true,
            ssid: b"home".to_vec(),
        }
    }

    #[test]
    fn best_prefers_a_good_5ghz_ap() {
        let mut s = Scan {
            state: 2,
            scans: 1,
            networks: vec![ap(0x66, 1, -40), ap(0x6a, 44, -46)],
        };
        assert_eq!(s.best(b"home").unwrap().bssid[5], 0x6a); // the owner's dual-band AP
        s.networks[1].dbm = -78;
        assert_eq!(s.best(b"home").unwrap().bssid[5], 0x66); // weak 5 GHz: the stronger 2.4 GHz
        s.networks.push(ap(0x70, 1, -30));
        s.networks[1].dbm = -60;
        assert_eq!(s.best(b"home").unwrap().bssid[5], 0x6a);
        s.networks.remove(1);
        assert_eq!(s.best(b"home").unwrap().bssid[5], 0x70); // 2.4 GHz only: the strongest
        assert!(s.best(b"other").is_none() && s.best(b"").is_none());
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

    #[test]
    fn forget_says_whether_it_found_the_network() {
        let mut s = Saved::default();
        s.remember(b"a", [1; 32]);
        s.remember(b"b", [2; 32]);
        assert!(!s.forget(b"c"));
        assert_eq!(s.names().collect::<Vec<_>>(), [&b"b"[..], b"a"]);
        assert!(s.forget(b"a"));
        assert_eq!(s.names().collect::<Vec<_>>(), [&b"b"[..]]);
        assert_eq!(s.pmk(b"a"), None);
    }
}
