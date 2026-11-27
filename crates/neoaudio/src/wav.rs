// SPDX-License-Identifier: Apache-2.0
//! (Settings D6, SE2c-1) A WAV file into a sound of the user's: one ring period ([`crate::tones::BYTES`], 48 kHz mono
//! S16LE). PCM 8/16/24-bit, mono or stereo, 8–48 kHz: mixed to mono, resampled to 48 kHz, leading silence trimmed, cut
//! at 2 s with a 50 ms fade, padded with silence. Float, A-law, µ-law and the rest are refused with a reason.
use crate::Resampler;
use crate::tones::{BYTES, RATE};
use alloc::string::String;
use alloc::vec::Vec;

/// The largest WAV read (Settings reads no more of a file).
pub const MAX_INPUT: usize = 16 * 1024 * 1024;
/// A sample quieter than this (about -54 dBFS) is leading silence.
const SILENCE: i32 = 64;
/// The fade when the sound is cut: 50 ms.
const FADE: usize = 2_400;
/// The longest name kept from the file's INFO, in bytes (a profile name's limit).
pub const NAME_MAX: usize = 24;

/// Why a file can't be a sound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WavError {
    /// not RIFF/WAVE
    NotWav,
    /// a header or chunk out of bounds, no fmt or data, a block size that doesn't match
    Damaged,
    /// float, A-law, µ-law, compressed, or 32-bit
    Format,
    /// more than two channels
    Channels,
    /// under 8 kHz or over 48 kHz
    Rate,
    /// nothing but silence
    Silent,
    /// over [`MAX_INPUT`]
    TooLarge,
    /// no memory for the result
    NoMemory,
}

impl WavError {
    /// What Settings says.
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            WavError::NotWav => "Not a WAV file",
            WavError::Damaged => "The file is damaged",
            WavError::Format => "Use a 16-bit PCM WAV",
            WavError::Channels => "Use a mono or stereo WAV",
            WavError::Rate => "Use a WAV at 8 to 48 kHz",
            WavError::Silent => "The sound is silent",
            WavError::TooLarge => "The file is over 16 MB",
            WavError::NoMemory => "Not enough memory",
        }
    }
}

/// A converted sound: its PCM ([`BYTES`] long) and the name the file gives itself (INFO `INAM`), if any.
#[derive(Debug, PartialEq, Eq)]
pub struct Sound {
    pub pcm: Vec<u8>,
    pub name: Option<String>,
}

struct Format {
    channels: usize,
    rate: u32,
    bytes: usize,
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

fn format(c: &[u8]) -> Result<Format, WavError> {
    if c.len() < 16 {
        return Err(WavError::Damaged);
    }
    let mut tag = u16_at(c, 0);
    // WAVE_FORMAT_EXTENSIBLE: the sub-format GUID's first two bytes are the tag
    if tag == 0xfffe {
        if c.len() < 26 {
            return Err(WavError::Damaged);
        }
        tag = u16_at(c, 24);
    }
    let (channels, rate, align, bits) = (
        usize::from(u16_at(c, 2)),
        u32_at(c, 4),
        usize::from(u16_at(c, 12)),
        usize::from(u16_at(c, 14)),
    );
    if tag != 1 || !matches!(bits, 8 | 16 | 24) {
        return Err(WavError::Format);
    }
    if !(1..=2).contains(&channels) {
        return Err(WavError::Channels);
    }
    if !(8_000..=RATE).contains(&rate) {
        return Err(WavError::Rate);
    }
    if align != channels * bits / 8 {
        return Err(WavError::Damaged);
    }
    Ok(Format {
        channels,
        rate,
        bytes: bits / 8,
    })
}

/// The name in a `LIST` `INFO` chunk's `INAM`: trimmed, cut to [`NAME_MAX`] bytes at a character, control
/// characters dropped; `None` when there is none or it is blank.
fn info_name(c: &[u8]) -> Option<String> {
    let mut rest = c.strip_prefix(b"INFO")?;
    while rest.len() >= 8 {
        let len = u32_at(rest, 4) as usize;
        let body = rest[8..].get(..len)?;
        if &rest[..4] == b"INAM" {
            let text = String::from_utf8_lossy(body);
            let clean: String = text
                .trim_matches(|ch: char| ch == '\0' || ch.is_whitespace())
                .chars()
                .filter(|ch| !ch.is_control())
                .collect();
            let mut end = clean.len().min(NAME_MAX);
            while !clean.is_char_boundary(end) {
                end -= 1;
            }
            let name = clean[..end].trim_end();
            return (!name.is_empty()).then(|| String::from(name));
        }
        rest = rest.get(8 + len + (len & 1)..)?;
    }
    None
}

/// Convert a WAV file's bytes.
///
/// # Errors
/// [`WavError`]: why the file can't be a sound.
pub fn convert(file: &[u8]) -> Result<Sound, WavError> {
    if file.len() > MAX_INPUT {
        return Err(WavError::TooLarge);
    }
    if file.len() < 12 || &file[..4] != b"RIFF" || &file[8..12] != b"WAVE" {
        return Err(WavError::NotWav);
    }
    let (mut fmt, mut data, mut name) = (None, None, None);
    // walk what is left of the file, never an offset: a 32-bit length can't overflow the cursor
    let mut rest = &file[12..];
    while let Some((head, body)) = rest.split_first_chunk::<8>() {
        let id = &head[..4];
        let len = u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as usize;
        if id == b"data" {
            // a writer that never went back to fill the size (or a cut file): what is there, and nothing after it
            data = Some(&body[..len.min(body.len())]);
            if len >= body.len() {
                break;
            }
        } else if len > body.len() {
            return Err(WavError::Damaged);
        } else if id == b"fmt " {
            fmt = Some(format(&body[..len])?);
        } else if id == b"LIST" {
            name = info_name(&body[..len]).or(name);
        }
        // chunks are padded to an even length (len < body.len() here, so len + 1 can't overflow)
        rest = body.get(len + (len & 1)..).unwrap_or(&[]);
    }
    let (Some(f), Some(data)) = (fmt, data) else {
        return Err(WavError::Damaged);
    };
    let frame = f.channels * f.bytes;
    let frames = data.len() / frame;
    if frames == 0 {
        return Err(WavError::Silent);
    }
    // one frame mixed to mono, as a 16-bit value
    let mono = |i: usize| -> i32 {
        let mut sum = 0;
        for ch in 0..f.channels {
            let s = &data[i * frame + ch * f.bytes..];
            sum += match f.bytes {
                1 => (i32::from(s[0]) - 128) << 8,
                2 => i32::from(i16::from_le_bytes([s[0], s[1]])),
                _ => i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 16,
            };
        }
        sum / f.channels as i32
    };
    let start = (0..frames)
        .find(|&i| mono(i).abs() >= SILENCE)
        .ok_or(WavError::Silent)?;
    let n = BYTES / 2;
    let mut out: Vec<i16> = Vec::new();
    out.try_reserve_exact(n).map_err(|_| WavError::NoMemory)?;
    let mut r = Resampler::new(f.rate, RATE);
    let mut next = start;
    let mut src = || {
        let v = (next < frames).then(|| mono(next));
        next += 1;
        v
    };
    // the resampler trails by one sample: its first output is the silence before the sound
    let _ = r.next(&mut src);
    while out.len() < n {
        let Some(v) = r.next(&mut src) else { break };
        out.push(v.clamp(-32_768, 32_767) as i16);
    }
    // cut short: fade the last 50 ms out, so the loop doesn't click
    if out.len() == n && next <= frames {
        for (k, s) in out[n - FADE..].iter_mut().enumerate() {
            *s = (i32::from(*s) * (FADE - k) as i32 / FADE as i32) as i16;
        }
    }
    let mut pcm = Vec::new();
    pcm.try_reserve_exact(BYTES)
        .map_err(|_| WavError::NoMemory)?;
    for s in &out {
        pcm.extend_from_slice(&s.to_le_bytes());
    }
    pcm.resize(BYTES, 0);
    Ok(Sound { pcm, name })
}
