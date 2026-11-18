// SPDX-License-Identifier: Apache-2.0
//! WIFID's kernel calls (kernel qcom_wcn3990.cpp, qcom_telhal.cpp; docs hardware/wifi.md): the firmware's start
//! (WlanMsa, WlanStart, the board data on vendor_a), the scan (WlanScan 0x4e1e), the link (WlanLink 0x4e1f) and the
//! SoC's random numbers (Prng 0x4e23). Moved from HWHUB and from neoui's `neoapp::hal` (S5).
use alloc::string::String;
use alloc::vec::Vec;
use e32::dprintln;
use e32::sys::TPtr8;
use sys::{Io, WLAN_MSA, WLAN_START, ext4_err, hal, hal_io, vendor_file};

const WLAN_SCAN: i32 = 0x4e1e;
const WLAN_LINK: i32 = 0x4e1f;
const PRNG: i32 = 0x4e23;

/// The firmware's start (wifi::Firmware's device side).
pub struct Fw;

impl wifi::Hal for Fw {
    fn wlan_start(&mut self) -> Result<(), i32> {
        // SAFETY: WLAN_START takes no arguments or buffers.
        match unsafe { hal(WLAN_START, core::ptr::null_mut(), core::ptr::null_mut()) } {
            r if r < 0 => Err(r),
            _ => Ok(()),
        }
    }

    fn wlan_msa(&mut self, regions: &[wifi::wlfw::Region]) -> Result<(), i32> {
        for r in regions {
            let io = Io {
                a: 1,
                b: r.addr as u32,
                len: r.size,
                buf: r.secure as u32,
            };
            // SAFETY: WLAN_MSA uses scalar region address/size/security fields.
            let e = unsafe { hal_io(WLAN_MSA, io) }.0;
            if e < 0 {
                return Err(e);
            }
        }
        Ok(())
    }

    /// The board data on vendor_a: Pixel 3a XL's `firmware/bdwlan-bonito.bin` (the factory image also has -ROW,
    /// -USA, -JPN, -VZW regional variants and the 3a's -sargo set), else the generic names.
    /// shortcut: the board name and the default (non-regional) file are fixed; pick by device and region before release.
    fn board_data(&mut self, board_id: u32) -> Vec<u8> {
        let names = [
            String::from("firmware/bdwlan-bonito.bin"),
            alloc::format!("firmware/bdwlan.b{:02x}", board_id & 0xff),
            String::from("firmware/bdwlan.bin"),
        ];
        for n in &names {
            // SAFETY: Only the daemon's one loop thread accesses the vendor cache.
            let got = unsafe {
                vendor_file(n, |fs, f| {
                    if f.size > 0x10_0000 {
                        return Err(-9); // KErrOverflow: a board file is tens of KB
                    }
                    fs.read(f).map_err(ext4_err)
                })
            };
            match got {
                Ok(d) => {
                    dprintln!("[wifid] wlfw: board data from {n}");
                    return d;
                }
                Err(e) => dprintln!("[wifid] wlfw: no {n} ({e})"),
            }
        }
        Vec::new()
    }
}

/// The latest scan as the kernel's record (`rescan` asks the driver for a new one).
pub fn scan(rescan: bool, buf: &mut [u8]) -> Result<usize, i32> {
    let mut des = TPtr8::new(buf);
    let des_ptr: *mut u8 = (&mut des as *mut TPtr8).cast();
    // SAFETY: the kernel writes the TDes8 (its length is checked) and reads a2 only as a flag.
    let r = unsafe { hal(WLAN_SCAN, des_ptr, usize::from(rescan) as *mut u8) };
    if r < 0 { Err(r) } else { Ok(des.len()) }
}

/// One WlanLink operation: `buf` holds the input (`input`), or is the output's room.
fn link_op(op: usize, buf: &mut [u8], input: bool) -> Result<usize, i32> {
    let mut des = if input {
        TPtr8::new_full(buf)
    } else {
        TPtr8::new(buf)
    };
    let des_ptr: *mut u8 = (&mut des as *mut TPtr8).cast();
    // SAFETY: the kernel reads or writes the TDes8 within its length/max length; a1 is only a number.
    let r = unsafe { hal(WLAN_LINK, op as *mut u8, des_ptr) };
    if r < 0 { Err(r) } else { Ok(des.len()) }
}

/// Join the scanned network `bssid`, sending our RSN element in the Association Request.
pub fn join(bssid: &[u8; 6], rsn: &[u8]) -> Result<(), i32> {
    let mut b = Vec::from(&bssid[..]);
    b.extend_from_slice(rsn);
    link_op(1, &mut b, true).map(drop)
}

/// Leave the network (or stop a join) and go back to scanning.
pub fn leave() -> Result<(), i32> {
    link_op(2, &mut [], true).map(drop)
}

/// The link's state as the kernel's record.
pub fn link(buf: &mut [u8]) -> Result<usize, i32> {
    link_op(3, buf, false)
}

/// The next EAPOL frame from the AP into `buf`: its length, or `None` if none is waiting.
pub fn eapol_recv(buf: &mut [u8]) -> Result<Option<usize>, i32> {
    match link_op(4, buf, false) {
        Ok(n) => Ok(Some(n)),
        Err(-18) => Ok(None), // KErrNotReady
        Err(e) => Err(e),
    }
}

/// Send one EAPOL frame to the AP; returns once the chip reported it sent.
pub fn eapol_send(frame: &[u8]) -> Result<(), i32> {
    let mut b = Vec::from(frame);
    link_op(5, &mut b, true).map(drop)
}

/// Install a CCMP key: the pairwise key, or the group key with its key index.
pub fn install_key(group: bool, index: u8, key: &[u8; 16]) -> Result<(), i32> {
    let mut b = [0u8; 18];
    b[0] = u8::from(group);
    b[1] = index;
    b[2..].copy_from_slice(key);
    link_op(6, &mut b, true).map(drop)
}

/// The handshake is done: let data through.
pub fn authorize() -> Result<(), i32> {
    link_op(7, &mut [], true).map(drop)
}

/// Fill `buf` from the SoC's PRNG.
pub fn random(buf: &mut [u8]) -> Result<(), i32> {
    let mut des = TPtr8::new_full(buf);
    let des_ptr: *mut u8 = (&mut des as *mut TPtr8).cast();
    // SAFETY: the kernel fills the TDes8 to its current length.
    let r = unsafe { hal(PRNG, des_ptr, core::ptr::null_mut()) };
    if r < 0 { Err(r) } else { Ok(()) }
}
