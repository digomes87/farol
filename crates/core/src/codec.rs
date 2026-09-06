//! Integer codecs for posting lists.
//!
//! A posting list is a sorted run of document ids, and storing them raw wastes
//! most of every word: `[1024, 1031, 1044]` needs 12 bytes as `u32`s, but the
//! *gaps* between them — `[1024, 7, 13]` — fit in 4. Two transforms do the
//! work:
//!
//! * **delta encoding** replaces each value with its distance from the previous
//!   one, which is what turns large ids into small numbers;
//! * **varint (LEB128)** then spends one byte per 7 bits of magnitude, so those
//!   small numbers actually occupy less space.
//!
//! The pair only works because posting lists are sorted, which is exactly the
//! invariant [`Index::finish`](crate::Index::finish) establishes.
//!
//! ```
//! use farol_core::codec::{encode_sorted, decode_sorted};
//!
//! let docs = [1024, 1031, 1044, 9000];
//! let mut bytes = Vec::new();
//! encode_sorted(&docs, &mut bytes);
//!
//! assert!(bytes.len() < docs.len() * 4, "compression should pay off");
//! assert_eq!(decode_sorted(&bytes, docs.len()).0, docs);
//! ```

/// Appends `value` to `out` in LEB128: seven bits per byte, high bit set on
/// every byte but the last.
pub fn write_varint(mut value: u32, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// Reads one varint from `bytes`, returning it and how many bytes it used.
///
/// A truncated or over-long sequence yields `(0, bytes.len())`: the decoder
/// never reads past the slice and never loops forever on malformed input, which
/// matters because these bytes come from a file on disk.
pub fn read_varint(bytes: &[u8]) -> (u32, usize) {
    let mut value: u32 = 0;
    let mut shift = 0;
    for (idx, &byte) in bytes.iter().enumerate() {
        value |= u32::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return (value, idx + 1);
        }
        shift += 7;
        if shift >= 35 {
            break;
        }
    }
    (0, bytes.len())
}

/// Encodes a strictly increasing sequence as deltas, appending to `out`.
///
/// The first value is written as is; every later one as its distance from its
/// predecessor.
pub fn encode_sorted(values: &[u32], out: &mut Vec<u8>) {
    let mut previous = 0u32;
    for &value in values {
        write_varint(value - previous, out);
        previous = value;
    }
}

/// Decodes `count` delta encoded values, returning them and the bytes consumed.
pub fn decode_sorted(bytes: &[u8], count: usize) -> (Vec<u32>, usize) {
    let mut values = Vec::with_capacity(count);
    let mut consumed = 0;
    let mut previous = 0u32;
    for _ in 0..count {
        if consumed >= bytes.len() {
            break;
        }
        let (delta, used) = read_varint(&bytes[consumed..]);
        consumed += used;
        previous = previous.saturating_add(delta);
        values.push(previous);
    }
    (values, consumed)
}

/// Encodes values that are not necessarily sorted, one varint each.
pub fn encode_plain(values: &[u32], out: &mut Vec<u8>) {
    for &value in values {
        write_varint(value, out);
    }
}

/// Decodes `count` plain varints, returning them and the bytes consumed.
pub fn decode_plain(bytes: &[u8], count: usize) -> (Vec<u32>, usize) {
    let mut values = Vec::with_capacity(count);
    let mut consumed = 0;
    for _ in 0..count {
        if consumed >= bytes.len() {
            break;
        }
        let (value, used) = read_varint(&bytes[consumed..]);
        consumed += used;
        values.push(value);
    }
    (values, consumed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_round_trip_across_every_size_boundary() {
        let interesting = [
            0,
            1,
            127,
            128,
            255,
            16_383,
            16_384,
            2_097_151,
            2_097_152,
            u32::MAX,
        ];
        for value in interesting {
            let mut bytes = Vec::new();
            write_varint(value, &mut bytes);
            assert_eq!(read_varint(&bytes), (value, bytes.len()), "value {value}");
        }
    }

    #[test]
    fn varint_length_grows_with_magnitude() {
        let len = |value| {
            let mut bytes = Vec::new();
            write_varint(value, &mut bytes);
            bytes.len()
        };
        assert_eq!(len(0), 1);
        assert_eq!(len(127), 1);
        assert_eq!(len(128), 2);
        assert_eq!(len(u32::MAX), 5);
    }

    #[test]
    fn sorted_sequences_round_trip() {
        let values: Vec<u32> = (0..1_000).map(|i| i * 37 + 5).collect();
        let mut bytes = Vec::new();
        encode_sorted(&values, &mut bytes);
        let (decoded, consumed) = decode_sorted(&bytes, values.len());
        assert_eq!(decoded, values);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn delta_encoding_beats_fixed_width_on_dense_lists() {
        // A dense posting list is the common case: consecutive ids compress to
        // one byte each instead of four.
        let values: Vec<u32> = (100_000..101_000).collect();
        let mut bytes = Vec::new();
        encode_sorted(&values, &mut bytes);
        assert!(
            bytes.len() < values.len() * 4 / 3,
            "expected strong compression, got {} bytes for {} values",
            bytes.len(),
            values.len()
        );
    }

    #[test]
    fn a_single_value_and_an_empty_slice_are_handled() {
        let mut bytes = Vec::new();
        encode_sorted(&[], &mut bytes);
        assert!(bytes.is_empty());
        assert_eq!(decode_sorted(&bytes, 0).0, Vec::<u32>::new());

        encode_sorted(&[u32::MAX], &mut bytes);
        assert_eq!(decode_sorted(&bytes, 1).0, [u32::MAX]);
    }

    #[test]
    fn plain_values_round_trip_without_the_sorted_assumption() {
        let values = [9, 1, 300, 0, 7];
        let mut bytes = Vec::new();
        encode_plain(&values, &mut bytes);
        assert_eq!(decode_plain(&bytes, values.len()).0, values);
    }

    #[test]
    fn truncated_input_stops_instead_of_looping_or_panicking() {
        let mut bytes = Vec::new();
        encode_sorted(&[1, 2, 3], &mut bytes);
        bytes.truncate(1);

        let (decoded, _) = decode_sorted(&bytes, 3);
        assert!(decoded.len() <= 3, "decoder invented values");
    }

    #[test]
    fn an_unterminated_varint_does_not_read_past_the_slice() {
        // Every byte has its continuation bit set: malformed on purpose.
        let bytes = vec![0xFF; 4];
        let (_, consumed) = read_varint(&bytes);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn decoding_more_values_than_encoded_returns_what_exists() {
        let mut bytes = Vec::new();
        encode_sorted(&[5, 10], &mut bytes);
        assert_eq!(decode_sorted(&bytes, 10).0, [5, 10]);
    }
}
