// SPDX-License-Identifier: Apache-2.0
//! The GSM 7-bit default alphabet and its extension table (3GPP TS 23.038 §6.2.1), and septet packing.

/// Septet → character. 0x1B is the escape to [`EXT`].
const BASIC: [char; 128] = [
    '@', '£', '$', '¥', 'è', 'é', 'ù', 'ì', 'ò', 'Ç', '\n', 'Ø', 'ø', '\r', 'Å', 'å', //
    'Δ', '_', 'Φ', 'Γ', 'Λ', 'Ω', 'Π', 'Ψ', 'Σ', 'Θ', 'Ξ', '\u{1b}', 'Æ', 'æ', 'ß', 'É', //
    ' ', '!', '"', '#', '¤', '%', '&', '\'', '(', ')', '*', '+', ',', '-', '.', '/', //
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', ':', ';', '<', '=', '>', '?', //
    '¡', 'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', //
    'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X', 'Y', 'Z', 'Ä', 'Ö', 'Ñ', 'Ü', '§', //
    '¿', 'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o', //
    'p', 'q', 'r', 's', 't', 'u', 'v', 'w', 'x', 'y', 'z', 'ä', 'ö', 'ñ', 'ü', 'à', //
];

pub const ESC: u8 = 0x1B;

/// The extension table: (septet after ESC, character).
const EXT: [(u8, char); 10] = [
    (0x0A, '\u{c}'),
    (0x14, '^'),
    (0x28, '{'),
    (0x29, '}'),
    (0x2F, '\\'),
    (0x3C, '['),
    (0x3D, '~'),
    (0x3E, ']'),
    (0x40, '|'),
    (0x65, '€'),
];

/// The septets for `c`: one, two (ESC + extension), or `None` when the alphabet lacks it.
pub fn encode_char(c: char) -> Option<([u8; 2], usize)> {
    if c != '\u{1b}'
        && let Some(i) = BASIC.iter().position(|&b| b == c)
    {
        return Some(([i as u8, 0], 1));
    }
    EXT.iter()
        .find(|&&(_, e)| e == c)
        .map(|&(s, _)| ([ESC, s], 2))
}

/// `text` as septets, or `None` if a character needs UCS-2.
pub fn encode(text: &str) -> Option<alloc::vec::Vec<u8>> {
    let mut out = alloc::vec::Vec::with_capacity(text.len());
    for c in text.chars() {
        let (s, n) = encode_char(c)?;
        out.extend_from_slice(&s[..n]);
    }
    Some(out)
}

/// Septets → text. An unknown extension shows as a space, as 23.038 asks.
pub fn decode(septets: &[u8]) -> alloc::string::String {
    let mut out = alloc::string::String::with_capacity(septets.len());
    let mut it = septets.iter();
    while let Some(&s) = it.next() {
        if s == ESC {
            let Some(&e) = it.next() else { break };
            out.push(EXT.iter().find(|&&(x, _)| x == e).map_or(' ', |&(_, c)| c));
        } else {
            out.push(BASIC[usize::from(s & 0x7F)]);
        }
    }
    out
}

/// Pack septets LSB first (23.038 §6.1.2.1.1).
pub fn pack(septets: &[u8]) -> alloc::vec::Vec<u8> {
    let mut out = alloc::vec![0u8; (septets.len() * 7).div_ceil(8)];
    for (i, &s) in septets.iter().enumerate() {
        let bit = i * 7;
        let (byte, shift) = (bit / 8, bit % 8);
        out[byte] |= (s & 0x7F) << shift;
        if shift > 1 {
            out[byte + 1] |= (s & 0x7F) >> (8 - shift);
        }
    }
    out
}

/// Unpack `n` septets.
pub fn unpack(octets: &[u8], n: usize) -> Option<alloc::vec::Vec<u8>> {
    if octets.len() < (n * 7).div_ceil(8) {
        return None;
    }
    Some(
        (0..n)
            .map(|i| {
                let bit = i * 7;
                let (byte, shift) = (bit / 8, bit % 8);
                let mut v = u16::from(octets[byte]) >> shift;
                if shift > 1 {
                    v |= u16::from(octets[byte + 1]) << (8 - shift);
                }
                (v & 0x7F) as u8
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_packs_as_23038_shows() {
        // "hellohello" is the spec's example: E8 32 9B FD 46 97 D9 EC 37
        let s = encode("hellohello").unwrap();
        assert_eq!(
            pack(&s),
            [0xE8, 0x32, 0x9B, 0xFD, 0x46, 0x97, 0xD9, 0xEC, 0x37]
        );
        assert_eq!(decode(&unpack(&pack(&s), s.len()).unwrap()), "hellohello");
    }

    #[test]
    fn extension_characters_take_two_septets() {
        let t = "€[]~^{}\\|";
        let s = encode(t).unwrap();
        assert_eq!(s.len(), 18);
        assert_eq!(decode(&s), t);
    }

    #[test]
    fn armenian_and_cyrillic_are_not_gsm() {
        assert!(encode("Բարև").is_none());
        assert!(encode("Привет").is_none());
        assert!(encode("ok 👍").is_none());
    }
}
