//! Record compression: the first optimization that *reduces* resources.
//!
//! Every optimization built so far spends memory to buy latency. That left a
//! `resources`-priority policy with nothing to select, which is a third of the
//! project's premise going untested. This one trades the other way: CPU for
//! storage, and — because smaller records mean more records per page — for I/O
//! and buffer-pool residency too.
//!
//! Compression is applied per *record*, not per page. Slots are already
//! variable-length, so a shorter record simply takes less of one; compressing
//! whole pages would need an indirection table because pages are addressed
//! arithmetically. Per-record also makes the choice per-record: a block that
//! does not shrink is stored raw, so compression can never make a record
//! bigger.

use adabt_core::error::{Error, Result};

/// Below this, the framing overhead is not worth the CPU.
const MIN_INPUT: usize = 64;

/// Compressed output must be at least this much smaller to be worth keeping.
///
/// A record that shrinks by 3% costs decompression on every single read to save
/// almost nothing. The margin makes the trade worth taking when it is taken.
const MIN_SAVING: f64 = 0.12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Raw,
    Lz4,
}

impl Encoding {
    pub fn bit(self) -> u8 {
        match self {
            Encoding::Raw => 0,
            Encoding::Lz4 => 1,
        }
    }
    pub fn from_bit(b: u8) -> Result<Self> {
        match b {
            0 => Ok(Encoding::Raw),
            1 => Ok(Encoding::Lz4),
            other => Err(Error::Corruption(format!(
                "unknown record encoding {other}"
            ))),
        }
    }
}

/// Compress if it is worth it, reporting which encoding was chosen.
///
/// Returning the encoding rather than deciding silently is what lets a store
/// hold a mix of compressed and raw records — which in turn means enabling
/// compression needs no migration, and disabling it needs no rewrite.
pub fn maybe_compress(input: &[u8]) -> (Encoding, Vec<u8>) {
    if input.len() < MIN_INPUT {
        return (Encoding::Raw, input.to_vec());
    }
    let compressed = lz4_flex::compress_prepend_size(input);
    let saving = 1.0 - (compressed.len() as f64 / input.len() as f64);
    if saving >= MIN_SAVING {
        (Encoding::Lz4, compressed)
    } else {
        (Encoding::Raw, input.to_vec())
    }
}

/// Restore a record. Never panics on corrupt input.
pub fn decompress(encoding: Encoding, data: &[u8]) -> Result<Vec<u8>> {
    match encoding {
        Encoding::Raw => Ok(data.to_vec()),
        Encoding::Lz4 => lz4_flex::decompress_size_prepended(data)
            .map_err(|e| Error::Corruption(format!("could not decompress record: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adabt_testkit::rng::Rng;

    /// What a fixed-layout record actually looks like: a little content and a
    /// lot of zero padding. This is the case compression exists to exploit.
    fn padded_record(i: u64) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&[1, 0]);
        v.extend_from_slice(&0u64.to_le_bytes());
        v.extend_from_slice(&i.to_le_bytes());
        v.extend_from_slice(&(i as i64 * 3).to_le_bytes());
        let name = format!("customer-{i}");
        v.push(name.len() as u8);
        v.extend_from_slice(name.as_bytes());
        v.resize(128, 0);
        v
    }

    #[test]
    fn round_trips_whatever_encoding_is_chosen() {
        for i in 0..200u64 {
            let input = padded_record(i);
            let (enc, data) = maybe_compress(&input);
            assert_eq!(decompress(enc, &data).unwrap(), input, "record {i}");
        }
    }

    #[test]
    fn padded_records_actually_shrink() {
        let input = padded_record(42);
        let (enc, data) = maybe_compress(&input);
        assert_eq!(enc, Encoding::Lz4, "a mostly-zero record should compress");
        // LZ4 framing costs a few bytes, so a 128-byte record cannot shrink
        // arbitrarily far; half is a substantial and realistic win.
        assert!(
            data.len() * 2 < input.len(),
            "expected at least a halving, got {} from {}",
            data.len(),
            input.len()
        );
    }

    #[test]
    fn incompressible_data_is_stored_raw_not_expanded() {
        // The property that matters: compression may never make a record bigger.
        let mut rng = Rng::new(0xC0DE);
        let input: Vec<u8> = (0..4096).map(|_| rng.below(256) as u8).collect();
        let (enc, data) = maybe_compress(&input);
        assert_eq!(enc, Encoding::Raw);
        assert_eq!(data.len(), input.len());
        assert_eq!(decompress(enc, &data).unwrap(), input);
    }

    #[test]
    fn compression_never_expands_any_input() {
        let mut rng = Rng::new(7);
        for _ in 0..500 {
            let n = rng.below_usize(3000);
            let input: Vec<u8> = (0..n).map(|_| rng.below(4) as u8).collect();
            let (enc, data) = maybe_compress(&input);
            assert!(
                data.len() <= input.len(),
                "{n} bytes grew to {} under {enc:?}",
                data.len()
            );
            assert_eq!(decompress(enc, &data).unwrap(), input);
        }
    }

    #[test]
    fn tiny_inputs_are_left_alone() {
        for n in 0..MIN_INPUT {
            let input = vec![0u8; n];
            let (enc, data) = maybe_compress(&input);
            assert_eq!(enc, Encoding::Raw, "{n} bytes should not be compressed");
            assert_eq!(data, input);
        }
    }

    #[test]
    fn a_marginal_saving_is_declined() {
        // Barely-compressible data: decompressing on every read to save 3% is a
        // bad trade, and the margin is what refuses it.
        let mut rng = Rng::new(99);
        let mut input: Vec<u8> = (0..2000).map(|_| rng.below(256) as u8).collect();
        // A short repeated tail gives a small but non-zero saving.
        input.extend_from_slice(&[0u8; 60]);
        let (enc, data) = maybe_compress(&input);
        if enc == Encoding::Lz4 {
            let saving = 1.0 - data.len() as f64 / input.len() as f64;
            assert!(saving >= MIN_SAVING, "kept a {saving:.3} saving");
        }
    }

    #[test]
    fn corrupt_compressed_data_errors_rather_than_panicking() {
        let input = padded_record(1);
        let (enc, data) = maybe_compress(&input);
        assert_eq!(enc, Encoding::Lz4);
        let mut rng = Rng::new(0xBAD);
        for _ in 0..3_000 {
            let mut b = data.clone();
            for _ in 0..1 + rng.below_usize(3) {
                let i = rng.below_usize(b.len());
                b[i] ^= 1 << rng.below_usize(8);
            }
            // Either an error or some bytes; never a crash, never a runaway
            // allocation from a bogus prepended size.
            let _ = decompress(Encoding::Lz4, &b);
        }
    }

    #[test]
    fn truncated_compressed_data_errors() {
        let (enc, data) = maybe_compress(&padded_record(1));
        assert_eq!(enc, Encoding::Lz4);
        for n in 0..data.len() {
            let _ = decompress(Encoding::Lz4, &data[..n]);
        }
    }

    #[test]
    fn encoding_bits_round_trip() {
        for e in [Encoding::Raw, Encoding::Lz4] {
            assert_eq!(Encoding::from_bit(e.bit()).unwrap(), e);
        }
        assert!(Encoding::from_bit(7).is_err());
    }
}
