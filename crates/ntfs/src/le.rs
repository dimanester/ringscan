//! Bounds-checked little-endian field readers and small conversions.
//!
//! Every on-disk field goes through these helpers, so a truncated or hostile
//! buffer yields `None` instead of a panic.

#[inline]
pub(crate) fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    let s = b.get(off..off.checked_add(2)?)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

#[inline]
pub(crate) fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

#[inline]
pub(crate) fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    let s: [u8; 8] = b.get(off..off.checked_add(8)?)?.try_into().ok()?;
    Some(u64::from_le_bytes(s))
}

/// Seconds between 1601-01-01 and 1970-01-01.
const FILETIME_UNIX_DELTA: i64 = 11_644_473_600;

/// Converts a Windows FILETIME (100 ns ticks since 1601-01-01 UTC) to unix
/// seconds. 0 ("not set") maps to 0, which consumers treat as unknown.
pub fn filetime_to_unix(ft: u64) -> i64 {
    if ft == 0 {
        return 0;
    }
    // ft / 10^7 <= 1.9e12, always fits an i64.
    (ft / 10_000_000) as i64 - FILETIME_UNIX_DELTA
}

/// Decodes a UTF-16LE byte slice (odd trailing byte ignored). Unpaired
/// surrogates, which NTFS allows in names, become U+FFFD.
pub(crate) fn utf16le_to_string(b: &[u8]) -> String {
    let (pairs, _) = b.as_chunks::<2>();
    if pairs.iter().all(|&[lo, hi]| hi == 0 && lo < 0x80) {
        // ASCII fast path: no surrogate handling, one allocation.
        let mut s = String::with_capacity(pairs.len());
        s.extend(pairs.iter().map(|&[lo, _]| lo as char));
        return s;
    }
    let units = pairs.iter().map(|&p| u16::from_le_bytes(p));
    let mut s = String::with_capacity(b.len() / 2 * 3);
    s.extend(char::decode_utf16(units).map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER)));
    s
}

/// True if UTF-16LE bytes `b` spell exactly the ASCII string `s`.
pub(crate) fn utf16le_eq_ascii(b: &[u8], s: &str) -> bool {
    b.len() == s.len() * 2
        && b.as_chunks::<2>()
            .0
            .iter()
            .zip(s.bytes())
            .all(|(&[lo, hi], a)| lo == a && hi == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readers_are_bounds_checked() {
        let b = [1u8, 2, 3, 4, 5, 6, 7, 8, 9];
        assert_eq!(u16_at(&b, 0), Some(0x0201));
        assert_eq!(u32_at(&b, 5), Some(0x0908_0706));
        assert_eq!(u64_at(&b, 1), Some(0x0908_0706_0504_0302));
        assert_eq!(u16_at(&b, 8), None);
        assert_eq!(u32_at(&b, 6), None);
        assert_eq!(u64_at(&b, 2), None);
        assert_eq!(u64_at(&b, usize::MAX - 3), None);
    }

    #[test]
    fn filetime() {
        assert_eq!(filetime_to_unix(0), 0);
        assert_eq!(filetime_to_unix(116_444_736_000_000_000), 0);
        // 2026-11-14 00:00:00 UTC
        assert_eq!(
            filetime_to_unix(116_444_736_000_000_000 + 1_794_614_400 * 10_000_000),
            1_794_614_400
        );
        assert_eq!(filetime_to_unix(1), -FILETIME_UNIX_DELTA);
        assert!(filetime_to_unix(u64::MAX) > 0);
    }

    #[test]
    fn utf16() {
        let enc = |s: &str| {
            s.encode_utf16()
                .flat_map(|u| u.to_le_bytes())
                .collect::<Vec<u8>>()
        };
        for s in ["", "abc.txt", "Привіт світ", "😀🎉 файл", "日本語"] {
            assert_eq!(utf16le_to_string(&enc(s)), s);
        }
        // Lone surrogate -> replacement char; odd byte ignored.
        assert_eq!(utf16le_to_string(&[0x00, 0xD8, b'a', 0]), "\u{FFFD}a");
        assert_eq!(utf16le_to_string(&[b'a', 0, b'b']), "a");
        assert!(utf16le_eq_ascii(&enc("$Bad"), "$Bad"));
        assert!(!utf16le_eq_ascii(&enc("$Bad"), "$BadX"));
        assert!(!utf16le_eq_ascii(&enc("$bad"), "$Bad"));
    }
}
