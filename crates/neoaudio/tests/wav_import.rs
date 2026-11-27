// SPDX-License-Identifier: Apache-2.0
//! Settings D6 (SE2c-1): WAV files into one ring period.
use neoaudio::tones::BYTES;
use neoaudio::wav::{MAX_INPUT, WavError, convert};

fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut c = id.to_vec();
    c.extend_from_slice(&(body.len() as u32).to_le_bytes());
    c.extend_from_slice(body);
    if body.len() % 2 == 1 {
        c.push(0);
    }
    c
}

fn fmt(tag: u16, channels: u16, rate: u32, bits: u16) -> Vec<u8> {
    let align = channels * bits / 8;
    let mut b = Vec::new();
    b.extend_from_slice(&tag.to_le_bytes());
    b.extend_from_slice(&channels.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * u32::from(align)).to_le_bytes());
    b.extend_from_slice(&align.to_le_bytes());
    b.extend_from_slice(&bits.to_le_bytes());
    chunk(b"fmt ", &b)
}

fn wav(parts: &[Vec<u8>]) -> Vec<u8> {
    let body: Vec<u8> = parts.concat();
    let mut f = b"RIFF".to_vec();
    f.extend_from_slice(&(body.len() as u32 + 4).to_le_bytes());
    f.extend_from_slice(b"WAVE");
    f.extend_from_slice(&body);
    f
}

/// `n` frames of a square wave at full level, 16-bit
fn square16(n: usize, channels: usize) -> Vec<u8> {
    (0..n)
        .flat_map(|i| {
            let v: i16 = if (i / 20) % 2 == 0 { 20_000 } else { -20_000 };
            std::iter::repeat_n(v.to_le_bytes(), channels).flatten()
        })
        .collect()
}

fn samples(b: &[u8]) -> Vec<i16> {
    b.chunks(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

#[test]
fn a_plain_wav_fills_one_period() {
    let f = wav(&[fmt(1, 1, 48_000, 16), chunk(b"data", &square16(24_000, 1))]);
    let s = convert(&f).unwrap();
    assert_eq!(s.pcm.len(), BYTES);
    assert_eq!(s.name, None);
    let pcm = samples(&s.pcm);
    // half a second of sound (one sample late), then silence
    assert!(pcm[..23_000].iter().any(|v| v.unsigned_abs() > 15_000));
    assert!(pcm[24_100..].iter().all(|&v| v == 0));
}

#[test]
fn every_pcm_format_is_mixed_and_resampled() {
    // stereo 44.1 kHz 24-bit: 1 s becomes about 48,000 frames
    let s24: Vec<u8> = (0..44_100)
        .flat_map(|i| {
            let v: i32 = if (i / 30) % 2 == 0 {
                4_000_000
            } else {
                -4_000_000
            };
            let b = v.to_le_bytes();
            [b[0], b[1], b[2], b[0], b[1], b[2]]
        })
        .collect();
    let s = convert(&wav(&[fmt(1, 2, 44_100, 24), chunk(b"data", &s24)])).unwrap();
    let pcm = samples(&s.pcm);
    let last = pcm.iter().rposition(|&v| v != 0).unwrap();
    assert!((47_900..=48_100).contains(&last), "{last}");
    assert!(pcm.iter().any(|&v| v > 15_000)); // 4,000,000 >> 8 = 15,625
    // 8-bit unsigned at 8 kHz: 0.25 s becomes 12,000 frames
    let s8: Vec<u8> = (0..2_000)
        .map(|i| if (i / 4) % 2 == 0 { 228 } else { 28 })
        .collect();
    let s = convert(&wav(&[fmt(1, 1, 8_000, 8), chunk(b"data", &s8)])).unwrap();
    let last = samples(&s.pcm).iter().rposition(|&v| v != 0).unwrap();
    assert!((11_900..=12_100).contains(&last), "{last}");
    // a stereo file with one silent channel: the mix is half as loud
    let one_side: Vec<u8> = (0..4_800)
        .flat_map(|_| {
            let mut f = 20_000i16.to_le_bytes().to_vec();
            f.extend_from_slice(&0i16.to_le_bytes());
            f
        })
        .collect();
    let s = convert(&wav(&[fmt(1, 2, 48_000, 16), chunk(b"data", &one_side)])).unwrap();
    assert_eq!(samples(&s.pcm)[100], 10_000);
}

#[test]
fn leading_silence_goes_and_a_long_sound_is_cut_with_a_fade() {
    let mut d = vec![0u8; 48_000 * 2]; // 1 s of silence first
    d.extend(square16(48_000 * 3, 1));
    let s = convert(&wav(&[fmt(1, 1, 48_000, 16), chunk(b"data", &d)])).unwrap();
    let pcm = samples(&s.pcm);
    assert_eq!(pcm.len() * 2, BYTES);
    // the sound starts at once (one sample late)
    assert!(pcm[1..20].iter().all(|v| v.unsigned_abs() == 20_000));
    // and fades out over its last 50 ms instead of stopping mid-wave
    assert!(
        pcm[90_000..93_000]
            .iter()
            .any(|v| v.unsigned_abs() == 20_000)
    );
    assert!(pcm[95_990..].iter().all(|v| v.unsigned_abs() < 200));
}

#[test]
fn the_name_padding_and_an_unfinished_size() {
    // LIST INFO INAM with an odd length (padded), before data; data's size never filled in (0xFFFFFFFF)
    let inam = chunk(b"INAM", b"Morning bell\0");
    let mut list = b"INFO".to_vec();
    list.extend_from_slice(&inam);
    let mut data = b"data".to_vec();
    data.extend_from_slice(&u32::MAX.to_le_bytes());
    data.extend(square16(1_000, 1));
    let f = wav(&[
        fmt(1, 1, 48_000, 16),
        chunk(b"LIST", &list),
        chunk(b"junk", b"x"),
        data,
    ]);
    let s = convert(&f).unwrap();
    assert_eq!(s.name.as_deref(), Some("Morning bell"));
    assert!(samples(&s.pcm)[500] != 0);
    // a name over 24 bytes is cut at a character
    let long = chunk(b"INAM", "Ձայն ձայն ձայն ձայն ձայն".as_bytes());
    let mut list = b"INFO".to_vec();
    list.extend_from_slice(&long);
    let f = wav(&[
        fmt(1, 1, 48_000, 16),
        chunk(b"LIST", &list),
        chunk(b"data", &square16(100, 1)),
    ]);
    let name = convert(&f).unwrap().name.unwrap();
    assert!(name.len() <= 24 && name.starts_with("Ձայն"));
}

#[test]
fn what_is_refused_and_why() {
    let data = chunk(b"data", &square16(100, 1));
    let refused = |f: Vec<u8>| convert(&f).unwrap_err();
    assert_eq!(refused(b"RIFX0000WAVE".to_vec()), WavError::NotWav);
    assert_eq!(refused(b"RIFF".to_vec()), WavError::NotWav);
    assert_eq!(
        refused(wav(&[fmt(3, 1, 48_000, 32), data.clone()])),
        WavError::Format
    ); // float
    assert_eq!(
        refused(wav(&[fmt(6, 1, 8_000, 8), data.clone()])),
        WavError::Format
    ); // A-law
    assert_eq!(
        refused(wav(&[fmt(7, 1, 8_000, 8), data.clone()])),
        WavError::Format
    ); // µ-law
    assert_eq!(
        refused(wav(&[fmt(1, 1, 48_000, 32), data.clone()])),
        WavError::Format
    );
    assert_eq!(
        refused(wav(&[fmt(1, 3, 48_000, 16), data.clone()])),
        WavError::Channels
    );
    assert_eq!(
        refused(wav(&[fmt(1, 1, 96_000, 16), data.clone()])),
        WavError::Rate
    );
    assert_eq!(
        refused(wav(&[fmt(1, 1, 6_000, 16), data.clone()])),
        WavError::Rate
    );
    assert_eq!(refused(wav(std::slice::from_ref(&data))), WavError::Damaged); // no fmt
    assert_eq!(refused(wav(&[fmt(1, 1, 48_000, 16)])), WavError::Damaged); // no data
    // a chunk longer than the file
    let mut bad = wav(&[fmt(1, 1, 48_000, 16), data.clone()]);
    bad[16] = 0xff;
    assert_eq!(refused(bad), WavError::Damaged);
    // a block size that doesn't match the format
    let mut bad = wav(&[fmt(1, 1, 48_000, 16), data.clone()]);
    bad[32] = 4;
    assert_eq!(refused(bad), WavError::Damaged);
    assert_eq!(
        refused(wav(&[fmt(1, 1, 48_000, 16), chunk(b"data", &[0; 4_000])])),
        WavError::Silent
    );
    assert_eq!(
        refused(wav(&[fmt(1, 1, 48_000, 16), chunk(b"data", &[])])),
        WavError::Silent
    );
    assert_eq!(refused(vec![0; MAX_INPUT + 1]), WavError::TooLarge);
    assert_eq!(WavError::Format.reason(), "Use a 16-bit PCM WAV");
}

#[test]
fn extensible_pcm_is_pcm() {
    let mut b = Vec::new();
    for v in [0xfffeu16, 1] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b.extend_from_slice(&48_000u32.to_le_bytes());
    b.extend_from_slice(&96_000u32.to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(&22u16.to_le_bytes()); // cbSize
    b.extend_from_slice(&16u16.to_le_bytes()); // valid bits
    b.extend_from_slice(&4u32.to_le_bytes()); // channel mask
    b.extend_from_slice(&1u16.to_le_bytes()); // KSDATAFORMAT_SUBTYPE_PCM's first bytes
    b.extend_from_slice(&[0; 14]);
    let f = wav(&[chunk(b"fmt ", &b), chunk(b"data", &square16(100, 1))]);
    assert!(convert(&f).is_ok());
}

#[test]
fn unfinished_sizes_end_the_walk_without_overflow() {
    // data's size never filled in (0xFFFFFFFF) and a chunk after it: the data is what is there, the walk ends
    let mut data = b"data".to_vec();
    data.extend_from_slice(&u32::MAX.to_le_bytes());
    data.extend(square16(500, 1));
    let mut f = wav(&[fmt(1, 1, 48_000, 16), data]);
    f.extend_from_slice(b"LIST");
    f.extend_from_slice(&u32::MAX.to_le_bytes());
    assert!(convert(&f).is_ok());
    // an INFO entry claiming 4 GiB: no name, still a sound
    let mut list = b"INFO".to_vec();
    list.extend_from_slice(b"INAM");
    list.extend_from_slice(&u32::MAX.to_le_bytes());
    let f = wav(&[
        fmt(1, 1, 48_000, 16),
        chunk(b"LIST", &list),
        chunk(b"data", &square16(100, 1)),
    ]);
    assert_eq!(convert(&f).unwrap().name, None);
    // a non-data chunk claiming 4 GiB: damaged, not a panic
    let mut big = b"junk".to_vec();
    big.extend_from_slice(&u32::MAX.to_le_bytes());
    let f = wav(&[fmt(1, 1, 48_000, 16), big]);
    assert_eq!(convert(&f), Err(WavError::Damaged));
    // a header cut inside a chunk header: no fmt/data, damaged
    let mut f = wav(&[fmt(1, 1, 48_000, 16)]);
    f.extend_from_slice(b"da");
    assert_eq!(convert(&f), Err(WavError::Damaged));
}
