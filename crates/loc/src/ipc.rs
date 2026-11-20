// SPDX-License-Identifier: Apache-2.0
//! NeoLocation: GNSSD.EXE's server (docs hardware/gnss.md). Apps ask for fixes and read the latest one.
//! - [`op::STATUS`] arg 0 = writable des8 of [`SIZE`]: the [`Status`] record.
//! - [`op::START`]: this session wants fixes. The engine runs while any session wants them.
//! - [`op::STOP`]: it doesn't any more (also when the session closes).
pub const NAME: &str = "NeoLocation";
/// major, minor, build
pub const VERSION: (i8, i8, i16) = (1, 0, 0);

pub mod op {
    pub const STATUS: i32 = 1;
    pub const START: i32 = 2;
    pub const STOP: i32 = 3;
}

pub mod state {
    /// nobody wants fixes
    pub const OFF: u8 = 0;
    pub const SEARCHING: u8 = 1;
    pub const FIX: u8 = 2;
    /// no fix within the time limit; START again to retry
    pub const FAILED: u8 = 3;
    /// the modem's engine refuses our fixes (engine lock)
    pub const LOCKED: u8 = 4;
}

/// [`Status::flags`]
pub mod flag {
    /// the modem's LOC service is up
    pub const SERVICE: u8 = 1;
    /// the modem holds valid XTRA data
    pub const XTRA_VALID: u8 = 2;
}

pub const SIZE: usize = 40;
pub const RECORD_VERSION: u8 = 1;
/// [`Status::xtra_age_s`] when there is no XTRA file
pub const NO_XTRA: u32 = u32::MAX;

/// The latest state and fix, little-endian, [`SIZE`] bytes:
/// `{version, state, flags, used, seen, snr, 2 reserved, lat_e7 i32, lon_e7 i32, h_acc_dm u32, alt_dm i32, utc_ms u64,
/// ttff_ms u32, xtra_age_s u32}`. The fix fields keep the last fix (utc_ms 0 = none yet).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    pub state: u8,
    pub flags: u8,
    /// satellites used in the last fix
    pub used: u8,
    /// satellites the engine hears now (a signal-to-noise ratio above 0)
    pub seen: u8,
    /// the strongest of them, dB-Hz (decoding a satellite's orbit needs about 25+)
    pub snr: u8,
    /// degrees × 10^7
    pub lat_e7: i32,
    pub lon_e7: i32,
    /// horizontal accuracy (circular), decimetres
    pub h_acc_dm: u32,
    /// altitude above mean sea level, decimetres
    pub alt_dm: i32,
    /// the fix's UTC time, 0 = no fix yet
    pub utc_ms: u64,
    /// time to first fix of this session, 0 = none yet
    pub ttff_ms: u32,
    /// age of the injected XTRA file, [`NO_XTRA`] = none
    pub xtra_age_s: u32,
}

impl Default for Status {
    fn default() -> Self {
        Status {
            state: state::OFF,
            flags: 0,
            used: 0,
            seen: 0,
            snr: 0,
            lat_e7: 0,
            lon_e7: 0,
            h_acc_dm: 0,
            alt_dm: 0,
            utc_ms: 0,
            ttff_ms: 0,
            xtra_age_s: NO_XTRA,
        }
    }
}

impl Status {
    pub fn encode(&self) -> [u8; SIZE] {
        let mut b = [0u8; SIZE];
        b[..6].copy_from_slice(&[
            RECORD_VERSION,
            self.state,
            self.flags,
            self.used,
            self.seen,
            self.snr,
        ]);
        b[8..12].copy_from_slice(&self.lat_e7.to_le_bytes());
        b[12..16].copy_from_slice(&self.lon_e7.to_le_bytes());
        b[16..20].copy_from_slice(&self.h_acc_dm.to_le_bytes());
        b[20..24].copy_from_slice(&self.alt_dm.to_le_bytes());
        b[24..32].copy_from_slice(&self.utc_ms.to_le_bytes());
        b[32..36].copy_from_slice(&self.ttff_ms.to_le_bytes());
        b[36..40].copy_from_slice(&self.xtra_age_s.to_le_bytes());
        b
    }

    /// None for a short buffer or another record version.
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < SIZE || b[0] != RECORD_VERSION {
            return None;
        }
        let w = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        Some(Status {
            state: b[1],
            flags: b[2],
            used: b[3],
            seen: b[4],
            snr: b[5],
            lat_e7: w(8) as i32,
            lon_e7: w(12) as i32,
            h_acc_dm: w(16),
            alt_dm: w(20) as i32,
            utc_ms: u64::from_le_bytes(b[24..32].try_into().unwrap()),
            ttff_ms: w(32),
            xtra_age_s: w(36),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let s = Status {
            state: state::FIX,
            flags: flag::SERVICE | flag::XTRA_VALID,
            used: 9,
            seen: 21,
            snr: 31,
            lat_e7: 401_772_000,
            lon_e7: -445_036_000,
            h_acc_dm: 87,
            alt_dm: -12,
            utc_ms: 1_795_173_061_000,
            ttff_ms: 31_500,
            xtra_age_s: 3600,
        };
        let b = s.encode();
        assert_eq!(b.len(), SIZE);
        assert_eq!(Status::decode(&b), Some(s));
        assert_eq!(Status::decode(&b[..SIZE - 1]), None);
        let mut other = b;
        other[0] = 2;
        assert_eq!(Status::decode(&other), None);
        assert_eq!(
            Status::decode(&Status::default().encode()),
            Some(Status::default())
        );
    }
}
