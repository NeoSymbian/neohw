// SPDX-License-Identifier: Apache-2.0
//! Shared device HAL and firmware/file access for the neohw Symbian processes.
#![no_std]

extern crate alloc;

pub mod hub;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use e32::sys::err;

pub const GROUP_VARIANT: i32 = 1;
pub const BOOT: i32 = 0x4e0d;
pub const STATE: i32 = 0x4e0e;
pub const SEND: i32 = 0x4e0f;
pub const RECV: i32 = 0x4e10;
pub const PART_READ: i32 = 0x4e11;
pub const RMTFS_MEM: i32 = 0x4e12;
pub const IPA_START: i32 = 0x4e13;
pub const ADSP_LOAD: i32 = 0x4e14;
pub const ADSP_STATE: i32 = 0x4e15;
pub const CHAN_SEND: i32 = 0x4e16;
pub const CHAN_RECV: i32 = 0x4e17;
pub const AUDIO: i32 = 0x4e18;
/// a file of modem_a's FAT16 (kernel qcom_telhal.cpp FwRead): TFTP's /readonly/firmware/image/ (Wi-Fi W0)
pub const FW_READ: i32 = 0x4e1b;
/// the Wi-Fi firmware's MSA regions to its VMIDs (kernel qcom_telhal.cpp WlanMsa, Wi-Fi W1)
pub const WLAN_MSA: i32 = 0x4e1c;
/// the copy engines, HTC and WMI on the kernel's own thread (qcom_wcn3990.cpp, Wi-Fi W2)
pub const WLAN_START: i32 = 0x4e1d;
// C7's HALs (kernel qcom_telhal.cpp): after Wi-Fi's 0x4e1b-0x4e1f, UsbNet's reserved 0x4e20-0x4e22 and Prng 0x4e23
/// stop the modem and/or the ADSP {mask 1 MSS, 2 ADSP}, asynchronously
pub const RPROC_STOP: i32 = 0x4e24;
/// that stop's progress {phase, error, mss generation, adsp generation}
pub const RPROC_STATE: i32 = 0x4e25;
/// the ringtone's audio memory for the ADSP (adsp.rs)
pub const AUDIO_MEM: i32 = 0x4e26;
/// the vibration motor {ms, amplitude}; 0 stops it
pub const HAPTIC: i32 = 0x4e27;
/// QRTR send to the modem, one attempt and no waiting (kernel `Glink_TrySend`): KErrNotReady with no fitting
/// receive intent (the kernel asked for one), KErrOverflow with no FIFO space; HWHUB's TFTP window retries it
pub const TRY_SEND: i32 = 0x4e28;
/// PartRead's index of vendor_a (the IPA firmware, read-only through ext4ro)
const VENDOR: u32 = 4;
const SECTOR: u64 = 512;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Io {
    pub a: u32,
    pub b: u32,
    pub len: u32,
    pub buf: u32,
}

/// Invoke the selected device HAL function.
///
/// # Safety
/// Arguments must follow the selected function's ABI. Every referenced buffer must remain valid for
/// the synchronous call, writable for output, and free of conflicting accesses while the HAL uses it.
pub unsafe fn hal(function: i32, a1: *mut u8, a2: *mut u8) -> i32 {
    // SAFETY: the caller guarantees the selected HAL ABI and buffer validity for this call.
    unsafe { e32::sys::ui::hal_function(GROUP_VARIANT, function, a1, a2) }
}

/// Invoke a device HAL function taking an `Io` descriptor.
///
/// # Safety
/// The function must accept `Io` as its first argument and no second argument. Its fields must follow
/// that function's ABI; any addresses must describe valid buffers of the stated length for the entire
/// synchronous call, with writable output and no conflicting accesses.
pub unsafe fn hal_io(function: i32, mut io: Io) -> (i32, Io) {
    // SAFETY: The Io lives through the call; the caller guarantees its function-specific buffer ABI.
    let r = unsafe { hal(function, (&mut io as *mut Io).cast(), core::ptr::null_mut()) };
    (r, io)
}

pub fn ext4_err(e: ext4ro::Error) -> i32 {
    match e {
        ext4ro::Error::Io(e) => e,
        ext4ro::Error::NotFound => -1, // KErrNotFound
        _ => -20,                      // KErrCorrupt
    }
}

pub fn fs_size(_: &mut ext4ro::Fs<Vendor>, f: &ext4ro::Inode) -> usize {
    f.size as usize
}

/// The vendor_a file the modem is reading, kept open between its TFTP blocks (one lookup per file, not per block)
struct OpenFile(core::cell::RefCell<Option<(String, ext4ro::Fs<Vendor>, ext4ro::Inode)>>);
// SAFETY: vendor_file requires one daemon thread or external serialization for every cache access.
unsafe impl Sync for OpenFile {}
static OPEN: OpenFile = OpenFile(core::cell::RefCell::new(None));

/// Access the cached vendor file on the daemon's thread.
///
/// # Safety
/// All calls in this process must run on one daemon thread or hold the same external lock for the
/// entire call, including the closure, because the shared cache uses a non-atomic `RefCell`.
pub unsafe fn vendor_file<T>(
    path: &str,
    f: impl FnOnce(&mut ext4ro::Fs<Vendor>, &ext4ro::Inode) -> Result<T, i32>,
) -> Result<T, i32> {
    let mut open = OPEN.0.borrow_mut();
    if open.as_ref().is_none_or(|o| o.0 != path) {
        *open = None;
        let mut fs = ext4ro::Fs::open(Vendor).map_err(ext4_err)?;
        let file = fs.find(path).map_err(ext4_err)?;
        *open = Some((String::from(path), fs, file));
    }
    let o = open.as_mut().ok_or(-1)?;
    f(&mut o.1, &o.2)
}

/// HAL FwRead: `path` ("IMAGE/WLANMDSP.MBN") as a NUL-terminated 32-byte buffer; an empty `buf` asks for the size
pub fn fw_read(path: &str, offset: usize, buf: &mut [u8]) -> Result<usize, i32> {
    let mut name = [0u8; 32];
    if path.len() >= name.len() {
        return Err(-6); // KErrArgument
    }
    name[..path.len()].copy_from_slice(path.as_bytes());
    let mut io = Io {
        a: 0,
        b: offset as u32,
        len: buf.len() as u32,
        buf: buf.as_mut_ptr() as u32,
    };
    // SAFETY: FW_READ receives a live Io, NUL-terminated name, and writable buffer of io.len bytes.
    match unsafe { hal(FW_READ, (&mut io as *mut Io).cast(), name.as_mut_ptr()) } {
        r if r < 0 => Err(r),
        r => Ok(r as usize),
    }
}

/// vendor_a through the kernel's read-only, sector-aligned PartRead.
pub struct Vendor;

impl ext4ro::Disk for Vendor {
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), i32> {
        let first = offset / SECTOR * SECTOR;
        let end = (offset + buf.len() as u64).div_ceil(SECTOR) * SECTOR;
        let mut tmp = vec![0u8; (end - first) as usize];
        // SAFETY: PART_READ writes only the live buffer described by this Io.
        let (r, _) = unsafe {
            hal_io(
                PART_READ,
                Io {
                    a: VENDOR,
                    b: first as u32,
                    len: tmp.len() as u32,
                    buf: tmp.as_mut_ptr() as u32,
                },
            )
        };
        if r < 0 {
            return Err(r);
        }
        let at = (offset - first) as usize;
        buf.copy_from_slice(&tmp[at..at + buf.len()]);
        Ok(())
    }
}

/// IPA's GSI firmware as one ELF image: ipa_fws.mdt (headers + hash) with each ipa_fws.bNN at its p_offset, as
/// Linux's mdt loader reads the split files; and the .mdt's length (the metadata TrustZone checks).
pub fn ipa_firmware() -> Result<(Vec<u8>, usize), ext4ro::Error> {
    let mut fs = ext4ro::Fs::open(Vendor)?;
    let mdt = fs.find("firmware/ipa_fws.mdt")?;
    let mdt = fs.read(&mdt)?;
    let word =
        |b: &[u8], at: usize| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]) as usize;
    if mdt.len() < 0x34 || &mdt[..4] != b"\x7fELF" {
        return Err(ext4ro::Error::Unsupported);
    }
    let (phoff, phnum) = (
        word(&mdt, 28),
        u16::from_le_bytes([mdt[44], mdt[45]]) as usize,
    );
    if phoff + 32 * phnum > mdt.len() {
        return Err(ext4ro::Error::Unsupported);
    }
    // (p_type, p_offset, p_filesz, p_flags) of program header i
    let ph = |i: usize| {
        let at = phoff + 32 * i;
        (
            word(&mdt, at),
            word(&mdt, at + 4),
            word(&mdt, at + 16),
            word(&mdt, at + 24),
        )
    };
    // only PT_LOAD segments that aren't the hash: those come from .bNN at their p_offset, past the .mdt. The hash
    // segment's p_offset falls inside the .mdt's bytes (its copy is packed there); writing .b01 over it corrupted
    // the metadata TrustZone checks (C5b: init_image refused, -0x300045).
    let loadable = |i: usize| ph(i).0 == 1 && ph(i).3 & (7 << 24) != 2 << 24 && ph(i).2 > 0;
    let len = (0..phnum)
        .filter(|&i| loadable(i))
        .map(|i| ph(i).1 + ph(i).2)
        .fold(mdt.len(), usize::max);
    let mut image = vec![0u8; len];
    image[..mdt.len()].copy_from_slice(&mdt);
    for i in (0..phnum).filter(|&i| loadable(i)) {
        let (_, off, size, _) = ph(i);
        if off < mdt.len() {
            return Err(ext4ro::Error::Unsupported); // would overwrite the metadata
        }
        let seg = fs.find(&alloc::format!("firmware/ipa_fws.b{i:02}"))?;
        let seg = fs.read(&seg)?;
        let n = seg.len().min(size);
        image[off..off + n].copy_from_slice(&seg[..n]);
    }
    Ok((image, mdt.len()))
}

/// Replace `name` in the caller's private directory with `data`, never leaving it empty or half written: write
/// `name.new`, flush it, delete `name`, rename. A cut between the delete and the rename leaves only `name.new`,
/// which [`load_private`] falls back to (neoui's settings store does the same).
pub fn replace_private(directory: &str, name: &str, data: &[u8]) -> Result<(), i32> {
    use e32::fs::{File, Fs, mode};
    let fs = Fs::connect()?;
    for path in ["C:\\private\\", directory] {
        if let Err(e) = fs.mkdir(path)
            && e != err::KERR_ALREADY_EXISTS
        {
            return Err(e);
        }
    }
    let (path, new) = (
        alloc::format!("{directory}{name}"),
        alloc::format!("{directory}{name}.new"),
    );
    {
        // closed (dropped) before the rename
        let f = File::replace(&fs, &new, mode::SHARE_EXCLUSIVE)?;
        f.write(data)?;
        f.flush()?;
    }
    match fs.delete(&path) {
        Ok(()) | Err(err::KERR_NOT_FOUND) => {}
        Err(e) => return Err(e),
    }
    fs.rename(&new, &path)
}

/// The file [`replace_private`] wrote: `path`, else `path.new` (a cut between its delete and its rename), at most
/// `max` bytes.
pub fn load_private(path: &str, max: usize) -> Result<Vec<u8>, i32> {
    use e32::fs::{File, Fs, mode};
    let fs = Fs::connect()?;
    let read = |p: &str| -> Result<Vec<u8>, i32> {
        let f = File::open(&fs, p, mode::READ | mode::SHARE_READERS_ONLY)?;
        let mut b = alloc::vec![0u8; max];
        let n = f.read_at(0, &mut b)?;
        b.truncate(n);
        Ok(b)
    };
    match read(path) {
        Err(err::KERR_NOT_FOUND) => read(&alloc::format!("{path}.new")),
        r => r,
    }
}

/// Replace `name` in the caller's private directory with `data`.
pub fn save_private(directory: &str, name: &str, data: &[u8]) -> Result<(), i32> {
    use e32::fs::{File, Fs, mode};
    let fs = Fs::connect()?;
    for path in ["C:\\private\\", directory] {
        if let Err(e) = fs.mkdir(path)
            && e != err::KERR_ALREADY_EXISTS
        {
            return Err(e);
        }
    }
    let path = alloc::format!("{directory}{name}");
    File::replace(&fs, &path, mode::SHARE_EXCLUSIVE)?.write(data)
}
