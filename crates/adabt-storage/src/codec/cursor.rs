//! The bounds-checked byte reader every decode path is built on.

use adabt_core::error::{Error, Result};

use crate::varint;

/// Bounds-checked reader. Every decode path goes through this so that a
/// truncated or corrupt buffer yields an error rather than a panic.
pub(super) struct Cursor<'a> {
    buf: &'a [u8],
    pub(super) pos: usize,
}

impl<'a> Cursor<'a> {
    pub(super) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub(super) fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or_else(|| {
            Error::Corruption("length overflow while decoding record".to_string())
        })?;
        if end > self.buf.len() {
            return Err(Error::Corruption(format!(
                "record truncated: wanted {n} bytes at {}, {} available",
                self.pos,
                self.buf.len().saturating_sub(self.pos)
            )));
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    pub(super) fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub(super) fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }

    pub(super) fn varint(&mut self) -> Result<u64> {
        let (v, used) = varint::read_u64(&self.buf[self.pos..])?;
        self.pos += used;
        Ok(v)
    }

    /// The same bytes as `string`, without owning them.
    ///
    /// A `Dynamic` record carries its own field names, so decoding one used
    /// to allocate a `String` per field per row before anything looked at it.
    /// Borrowing lets the caller decide whether a fresh allocation is needed
    /// at all — usually it is not, because the same names repeat on every row.
    pub(super) fn str_ref(&mut self, n: usize) -> Result<&'a str> {
        let b = self.take(n)?;
        std::str::from_utf8(b)
            .map_err(|e| Error::Corruption(format!("invalid utf-8 in record: {e}")))
    }

    pub(super) fn string(&mut self, n: usize) -> Result<String> {
        let b = self.take(n)?;
        String::from_utf8(b.to_vec())
            .map_err(|e| Error::Corruption(format!("invalid utf-8 in record: {e}")))
    }

    pub(super) fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }
}
