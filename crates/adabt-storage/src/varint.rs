//! LEB128 variable-length integers.
//!
//! Used only in the variable-length codecs. Fixed-layout records never contain
//! a varint: a value whose byte length depends on its magnitude would make
//! field offsets data-dependent, which is exactly what direct addressing
//! cannot tolerate.

use adabt_core::error::{Error, Result};

pub fn write_u64(v: u64, out: &mut Vec<u8>) {
    let mut v = v;
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Decode a varint, returning the value and the number of bytes consumed.
pub fn read_u64(buf: &[u8]) -> Result<(u64, usize)> {
    let mut result = 0u64;
    let mut shift = 0u32;
    for (i, &byte) in buf.iter().enumerate() {
        // 10 groups of 7 bits is the most a u64 can occupy; anything longer is
        // corruption, and without this check a hostile input loops shifting
        // past 64 and silently produces garbage.
        if i >= 10 {
            return Err(Error::Corruption("varint longer than 10 bytes".into()));
        }
        let payload = (byte & 0x7f) as u64;
        if shift >= 64 || (shift == 63 && payload > 1) {
            return Err(Error::Corruption("varint overflows u64".into()));
        }
        result |= payload << shift;
        if byte & 0x80 == 0 {
            return Ok((result, i + 1));
        }
        shift += 7;
    }
    Err(Error::Corruption("truncated varint".into()))
}

pub fn encoded_len(v: u64) -> usize {
    let mut n = 1;
    let mut v = v >> 7;
    while v != 0 {
        n += 1;
        v >>= 7;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_across_the_whole_range() {
        let mut cases: Vec<u64> = vec![0, 1, 127, 128, 300, u32::MAX as u64, u64::MAX];
        for bit in 0..64 {
            cases.push(1u64 << bit);
        }
        for v in cases {
            let mut buf = Vec::new();
            write_u64(v, &mut buf);
            assert_eq!(buf.len(), encoded_len(v), "length mismatch for {v}");
            let (got, used) = read_u64(&buf).unwrap();
            assert_eq!(got, v);
            assert_eq!(used, buf.len());
        }
    }

    #[test]
    fn small_values_take_one_byte() {
        for v in 0..128u64 {
            assert_eq!(encoded_len(v), 1);
        }
        assert_eq!(encoded_len(128), 2);
    }

    #[test]
    fn decoding_stops_at_the_terminator_leaving_the_rest() {
        let mut buf = Vec::new();
        write_u64(300, &mut buf);
        buf.extend_from_slice(b"trailing");
        let (v, used) = read_u64(&buf).unwrap();
        assert_eq!(v, 300);
        assert_eq!(&buf[used..], b"trailing");
    }

    #[test]
    fn truncated_input_is_corruption_not_a_panic() {
        assert!(read_u64(&[]).is_err());
        assert!(read_u64(&[0x80]).is_err());
        assert!(read_u64(&[0x80, 0x80, 0x80]).is_err());
    }

    #[test]
    fn overlong_encoding_is_rejected() {
        // Eleven continuation bytes: longer than any u64 can require.
        let buf = vec![0xff; 11];
        assert!(read_u64(&buf).is_err());
    }

    #[test]
    fn a_value_overflowing_u64_is_rejected() {
        // Ten bytes whose top group pushes past 64 bits.
        let buf = vec![0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f];
        assert!(read_u64(&buf).is_err());
    }
}
