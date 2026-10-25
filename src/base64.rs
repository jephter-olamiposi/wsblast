//! Standard base64 (RFC 4648 section 4, with `=` padding), used for binary
//! frames in capture files. Small enough that it is not worth a dependency.

#[cfg(feature = "capture")]
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encodes `input` as padded standard base64.
#[cfg(feature = "capture")]
pub(crate) fn encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = (u32::from(chunk[0]) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        let sextet = |shift: u32| char::from(ALPHABET[((n >> shift) & 63) as usize]);
        out.push(sextet(18));
        out.push(sextet(12));
        out.push(if chunk.len() > 1 { sextet(6) } else { '=' });
        out.push(if chunk.len() > 2 { sextet(0) } else { '=' });
    }
    out
}

/// Decodes padded standard base64, as written by `encode`. Returns `None`
/// for any other input: a wrong length, a character outside the alphabet or
/// misplaced padding.
#[cfg(feature = "mock")]
pub(crate) fn decode(input: &str) -> Option<Vec<u8>> {
    let input = input.as_bytes();
    if !input.len().is_multiple_of(4) {
        return None;
    }
    let groups = input.len() / 4;
    let mut out = Vec::with_capacity(groups * 3);
    for (index, group) in input.chunks(4).enumerate() {
        let padding = group.iter().rev().take_while(|&&c| c == b'=').count();
        if padding > 2 || (padding > 0 && index + 1 != groups) {
            return None;
        }
        let mut n = 0u32;
        for &c in &group[..4 - padding] {
            n = (n << 6) | sextet(c)?;
        }
        n <<= 6 * padding as u32;
        out.push((n >> 16) as u8);
        if padding < 2 {
            out.push((n >> 8) as u8);
        }
        if padding < 1 {
            out.push(n as u8);
        }
    }
    Some(out)
}

#[cfg(feature = "mock")]
fn sextet(c: u8) -> Option<u32> {
    let value = match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    };
    Some(u32::from(value))
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    #[cfg(feature = "mock")]
    #[test]
    fn decodes_and_rejects_malformed_input() {
        assert_eq!(decode("").unwrap(), b"");
        assert_eq!(decode("Zg==").unwrap(), b"f");
        assert_eq!(decode("Zm8=").unwrap(), b"fo");
        assert_eq!(decode("Zm9vYmFy").unwrap(), b"foobar");
        assert_eq!(decode("AP/+gA==").unwrap(), [0x00, 0xff, 0xfe, 0x80]);
        for bad in ["Zg=", "Zg===", "Z===", "Zg==Zg==", "Zm9-", "Zm=v", "Z m9"] {
            assert_eq!(decode(bad), None, "{bad:?}");
        }
    }

    #[cfg(all(feature = "capture", feature = "mock"))]
    proptest::proptest! {
        #[test]
        fn decode_inverts_encode(bytes in proptest::collection::vec(proptest::num::u8::ANY, 0..300)) {
            proptest::prop_assert_eq!(decode(&encode(&bytes)), Some(bytes));
        }
    }

    #[cfg(feature = "capture")]
    #[test]
    fn encodes_rfc4648_test_vectors() {
        let vectors = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ];
        for (input, expected) in vectors {
            assert_eq!(encode(input.as_bytes()), expected, "{input:?}");
        }
        assert_eq!(encode(&[0x00, 0xff, 0xfe, 0x80]), "AP/+gA==");
    }
}
