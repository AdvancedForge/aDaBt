//! Slotted pages.
//!
//! A page is a fixed-size byte array holding variable-length payloads. Slots at
//! the front grow upward, payloads at the back grow downward, and the gap in
//! between is free space. Deleting a payload leaves a dead slot behind so that
//! surviving slot indices never shift — a `SlotId` handed out to the page
//! directory must stay valid until the record itself moves.
//!
//! Every page carries a checksum and the LSN of the last change applied to it.
//! The checksum turns silent disk corruption into a loud error; the LSN is what
//! lets recovery skip changes the page already contains.

use adabt_core::error::{Error, Result};
use adabt_core::ids::Lsn;

pub const PAGE_SIZE: usize = 8192;
const HEADER_SIZE: usize = 24;
const SLOT_SIZE: usize = 4;

/// Largest payload a single page can hold, with one slot and nothing else.
pub const MAX_PAYLOAD: usize = PAGE_SIZE - HEADER_SIZE - SLOT_SIZE;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PageId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SlotId(pub u16);

/// Where a record lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordLocation {
    pub page: PageId,
    pub slot: SlotId,
}

#[derive(Clone)]
pub struct Page {
    bytes: Box<[u8; PAGE_SIZE]>,
}

impl std::fmt::Debug for Page {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Page")
            .field("lsn", &self.lsn())
            .field("slots", &self.slot_count())
            .field("live", &self.live_count())
            .field("free", &self.free_space())
            .finish()
    }
}

impl Default for Page {
    fn default() -> Self {
        Self::new()
    }
}

impl Page {
    pub fn new() -> Self {
        let mut p = Page {
            bytes: Box::new([0u8; PAGE_SIZE]),
        };
        p.set_free_end(PAGE_SIZE as u16);
        p
    }

    // -- header accessors --------------------------------------------------

    fn u16_at(&self, at: usize) -> u16 {
        u16::from_le_bytes([self.bytes[at], self.bytes[at + 1]])
    }
    fn set_u16_at(&mut self, at: usize, v: u16) {
        self.bytes[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }

    pub fn lsn(&self) -> Lsn {
        let mut a = [0u8; 8];
        a.copy_from_slice(&self.bytes[4..12]);
        Lsn(u64::from_le_bytes(a))
    }
    pub fn set_lsn(&mut self, lsn: Lsn) {
        self.bytes[4..12].copy_from_slice(&lsn.0.to_le_bytes());
    }

    pub fn slot_count(&self) -> u16 {
        self.u16_at(12)
    }
    fn set_slot_count(&mut self, v: u16) {
        self.set_u16_at(12, v)
    }

    fn free_end(&self) -> u16 {
        self.u16_at(14)
    }
    fn set_free_end(&mut self, v: u16) {
        self.set_u16_at(14, v)
    }

    fn free_start(&self) -> usize {
        HEADER_SIZE + self.slot_count() as usize * SLOT_SIZE
    }

    /// Bytes available for a payload *and* its slot.
    pub fn free_space(&self) -> usize {
        (self.free_end() as usize).saturating_sub(self.free_start())
    }

    // -- slots -------------------------------------------------------------

    fn slot_at(&self, i: u16) -> (u16, u16) {
        let at = HEADER_SIZE + i as usize * SLOT_SIZE;
        (self.u16_at(at), self.u16_at(at + 2))
    }

    fn set_slot(&mut self, i: u16, off: u16, len: u16) {
        let at = HEADER_SIZE + i as usize * SLOT_SIZE;
        self.set_u16_at(at, off);
        self.set_u16_at(at + 2, len);
    }

    /// A slot with offset 0 is dead: offset 0 is inside the header, so no live
    /// payload can ever start there.
    fn is_live(&self, i: u16) -> bool {
        self.slot_at(i).0 != 0
    }

    pub fn live_count(&self) -> usize {
        (0..self.slot_count()).filter(|i| self.is_live(*i)).count()
    }

    pub fn slots(&self) -> impl Iterator<Item = SlotId> + '_ {
        (0..self.slot_count())
            .filter(|i| self.is_live(*i))
            .map(SlotId)
    }

    // -- payload operations ------------------------------------------------

    /// Space consumed by storing `len` bytes as a brand-new slot.
    pub fn cost_of(len: usize) -> usize {
        len + SLOT_SIZE
    }

    pub fn can_fit(&self, len: usize) -> bool {
        // Reusing a dead slot avoids the slot cost, but assuming that here
        // would make `can_fit` optimistic and `insert` fallible in a way
        // callers do not expect.
        self.free_space() >= Self::cost_of(len)
    }

    pub fn insert(&mut self, payload: &[u8]) -> Result<SlotId> {
        if payload.is_empty() {
            return Err(Error::Corruption(
                "refusing to store an empty payload: offset 0 marks a dead slot".into(),
            ));
        }
        if payload.len() > MAX_PAYLOAD {
            return Err(Error::Corruption(format!(
                "payload of {} bytes exceeds page capacity {MAX_PAYLOAD}",
                payload.len()
            )));
        }

        // Prefer a dead slot: it costs no additional slot space.
        let reuse = (0..self.slot_count()).find(|i| !self.is_live(*i));
        let needs_slot = reuse.is_none();
        let need = payload.len() + if needs_slot { SLOT_SIZE } else { 0 };
        if self.free_space() < need {
            return Err(Error::Corruption(format!(
                "page has {} free bytes, needs {need}",
                self.free_space()
            )));
        }

        let off = self.free_end() as usize - payload.len();
        self.bytes[off..off + payload.len()].copy_from_slice(payload);
        self.set_free_end(off as u16);

        let slot = match reuse {
            Some(i) => i,
            None => {
                let i = self.slot_count();
                self.set_slot_count(i + 1);
                i
            }
        };
        self.set_slot(slot, off as u16, payload.len() as u16);
        Ok(SlotId(slot))
    }

    pub fn get(&self, slot: SlotId) -> Result<&[u8]> {
        if slot.0 >= self.slot_count() {
            return Err(Error::Corruption(format!(
                "slot {} out of range (page has {})",
                slot.0,
                self.slot_count()
            )));
        }
        let (off, len) = self.slot_at(slot.0);
        if off == 0 {
            return Err(Error::Corruption(format!("slot {} is dead", slot.0)));
        }
        let (off, len) = (off as usize, len as usize);
        // Offsets come off disk, so they are untrusted even after the checksum
        // passes: a torn write can produce a valid-looking page.
        if off < HEADER_SIZE || off + len > PAGE_SIZE {
            return Err(Error::Corruption(format!(
                "slot {} points outside the page ({off}..{})",
                slot.0,
                off + len
            )));
        }
        Ok(&self.bytes[off..off + len])
    }

    pub fn delete(&mut self, slot: SlotId) -> Result<()> {
        if slot.0 >= self.slot_count() || !self.is_live(slot.0) {
            return Err(Error::Corruption(format!("slot {} is not live", slot.0)));
        }
        // The payload stays where it is until compaction; only the slot dies,
        // so surviving slot ids keep their meaning.
        self.set_slot(slot.0, 0, 0);
        Ok(())
    }

    /// Replace a payload in place when the new one fits the old footprint,
    /// otherwise report that the caller must relocate the record.
    pub fn update(&mut self, slot: SlotId, payload: &[u8]) -> Result<UpdateOutcome> {
        let (off, len) = {
            if slot.0 >= self.slot_count() || !self.is_live(slot.0) {
                return Err(Error::Corruption(format!("slot {} is not live", slot.0)));
            }
            self.slot_at(slot.0)
        };
        if payload.len() <= len as usize {
            let off = off as usize;
            self.bytes[off..off + payload.len()].copy_from_slice(payload);
            self.set_slot(slot.0, off as u16, payload.len() as u16);
            return Ok(UpdateOutcome::InPlace);
        }
        // Growing: try to place it in free space and repoint the slot. The old
        // payload becomes garbage reclaimed by the next compaction.
        if self.free_space() >= payload.len() {
            let new_off = self.free_end() as usize - payload.len();
            self.bytes[new_off..new_off + payload.len()].copy_from_slice(payload);
            self.set_free_end(new_off as u16);
            self.set_slot(slot.0, new_off as u16, payload.len() as u16);
            return Ok(UpdateOutcome::Moved);
        }
        Ok(UpdateOutcome::DoesNotFit)
    }

    /// Reclaim space left by deleted and overwritten payloads.
    ///
    /// Slot indices are preserved, so this is safe to run behind an existing
    /// page directory.
    pub fn compact(&mut self) {
        let mut live: Vec<(u16, Vec<u8>)> = Vec::new();
        for i in 0..self.slot_count() {
            if self.is_live(i) {
                if let Ok(b) = self.get(SlotId(i)) {
                    live.push((i, b.to_vec()));
                }
            }
        }
        // Drop trailing dead slots so a page that has been fully emptied does
        // not keep paying for its slot array forever.
        let highest_live = live.iter().map(|(i, _)| *i + 1).max().unwrap_or(0);
        let lsn = self.lsn();
        self.bytes[HEADER_SIZE..].fill(0);
        self.set_slot_count(highest_live);
        self.set_free_end(PAGE_SIZE as u16);
        self.set_lsn(lsn);
        for (i, payload) in live {
            let off = self.free_end() as usize - payload.len();
            self.bytes[off..off + payload.len()].copy_from_slice(&payload);
            self.set_free_end(off as u16);
            self.set_slot(i, off as u16, payload.len() as u16);
        }
    }

    /// Bytes reclaimable by compaction.
    pub fn fragmentation(&self) -> usize {
        let used: usize = (0..self.slot_count())
            .filter(|i| self.is_live(*i))
            .map(|i| self.slot_at(i).1 as usize)
            .sum();
        let occupied = PAGE_SIZE - self.free_end() as usize;
        occupied.saturating_sub(used)
    }

    // -- serialisation -----------------------------------------------------

    fn compute_checksum(bytes: &[u8]) -> u32 {
        // FNV-1a: no dependency, adequate for detecting torn writes and bit rot.
        let mut h: u32 = 0x811c_9dc5;
        for &b in &bytes[4..] {
            h ^= b as u32;
            h = h.wrapping_mul(0x0100_0193);
        }
        h
    }

    pub fn seal(&mut self) {
        let c = Self::compute_checksum(&self.bytes[..]);
        self.bytes[0..4].copy_from_slice(&c.to_le_bytes());
    }

    pub fn as_bytes(&self) -> &[u8; PAGE_SIZE] {
        &self.bytes
    }

    pub fn from_bytes(bytes: [u8; PAGE_SIZE]) -> Result<Self> {
        let stored = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let actual = Self::compute_checksum(&bytes);
        if stored != actual {
            return Err(Error::Corruption(format!(
                "page checksum mismatch: stored {stored:#010x}, computed {actual:#010x}"
            )));
        }
        let p = Page {
            bytes: Box::new(bytes),
        };
        // A checksum proves the bytes are as written, not that they are sane:
        // validate the header before anything indexes with it.
        let slots_end = HEADER_SIZE + p.slot_count() as usize * SLOT_SIZE;
        if slots_end > PAGE_SIZE || (p.free_end() as usize) > PAGE_SIZE {
            return Err(Error::Corruption("page header is self-inconsistent".into()));
        }
        if (p.free_end() as usize) < slots_end {
            return Err(Error::Corruption(
                "page slot array overlaps its payload region".into(),
            ));
        }
        Ok(p)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateOutcome {
    InPlace,
    Moved,
    DoesNotFit,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_page_is_empty_and_almost_entirely_free() {
        let p = Page::new();
        assert_eq!(p.slot_count(), 0);
        assert_eq!(p.live_count(), 0);
        assert_eq!(p.free_space(), PAGE_SIZE - HEADER_SIZE);
    }

    #[test]
    fn insert_then_get_round_trips() {
        let mut p = Page::new();
        let a = p.insert(b"hello").unwrap();
        let b = p.insert(b"world!").unwrap();
        assert_eq!(p.get(a).unwrap(), b"hello");
        assert_eq!(p.get(b).unwrap(), b"world!");
        assert_eq!(p.live_count(), 2);
    }

    #[test]
    fn free_space_shrinks_by_payload_plus_slot() {
        let mut p = Page::new();
        let before = p.free_space();
        p.insert(&[7u8; 100]).unwrap();
        assert_eq!(p.free_space(), before - Page::cost_of(100));
    }

    #[test]
    fn deleting_frees_the_slot_for_reuse_without_shifting_others() {
        let mut p = Page::new();
        let a = p.insert(b"aaaa").unwrap();
        let b = p.insert(b"bbbb").unwrap();
        p.delete(a).unwrap();
        assert!(p.get(a).is_err());
        assert_eq!(
            p.get(b).unwrap(),
            b"bbbb",
            "surviving slot id must stay valid"
        );
        // The dead slot is reused rather than growing the slot array.
        let count_before = p.slot_count();
        let c = p.insert(b"cccc").unwrap();
        assert_eq!(c, a, "a dead slot should be reused");
        assert_eq!(p.slot_count(), count_before);
    }

    #[test]
    fn an_empty_payload_is_rejected() {
        // Offset zero is the dead-slot marker, so a zero-length payload would
        // be indistinguishable from a deleted one.
        assert!(Page::new().insert(b"").is_err());
    }

    #[test]
    fn a_payload_larger_than_the_page_is_rejected() {
        let mut p = Page::new();
        assert!(p.insert(&vec![0u8; MAX_PAYLOAD + 1]).is_err());
        assert!(p.insert(&vec![1u8; MAX_PAYLOAD]).is_ok());
    }

    #[test]
    fn filling_a_page_reports_exhaustion_rather_than_corrupting() {
        let mut p = Page::new();
        let mut n = 0;
        while p.can_fit(64) {
            p.insert(&[1u8; 64]).unwrap();
            n += 1;
        }
        assert!(n > 100, "expected many 64-byte records per page, got {n}");
        assert!(p.insert(&[1u8; 64]).is_err());
        // Everything already stored is still readable.
        for s in p.slots().collect::<Vec<_>>() {
            assert_eq!(p.get(s).unwrap().len(), 64);
        }
    }

    #[test]
    fn shrinking_update_stays_in_place() {
        let mut p = Page::new();
        let s = p.insert(b"aaaaaaaaaa").unwrap();
        assert_eq!(p.update(s, b"bb").unwrap(), UpdateOutcome::InPlace);
        assert_eq!(p.get(s).unwrap(), b"bb");
    }

    #[test]
    fn growing_update_moves_within_the_page() {
        let mut p = Page::new();
        let s = p.insert(b"aa").unwrap();
        assert_eq!(p.update(s, b"bbbbbbbbbb").unwrap(), UpdateOutcome::Moved);
        assert_eq!(p.get(s).unwrap(), b"bbbbbbbbbb");
    }

    #[test]
    fn an_update_that_cannot_fit_says_so_instead_of_failing() {
        let mut p = Page::new();
        let s = p.insert(b"aa").unwrap();
        while p.can_fit(64) {
            p.insert(&[1u8; 64]).unwrap();
        }
        assert_eq!(
            p.update(s, &vec![9u8; 1000]).unwrap(),
            UpdateOutcome::DoesNotFit
        );
        assert_eq!(
            p.get(s).unwrap(),
            b"aa",
            "a failed update must not damage the old value"
        );
    }

    #[test]
    fn compaction_reclaims_space_and_preserves_slot_ids() {
        let mut p = Page::new();
        let ids: Vec<_> = (0..20)
            .map(|i| p.insert(&[i as u8; 200]).unwrap())
            .collect();
        for s in ids.iter().step_by(2) {
            p.delete(*s).unwrap();
        }
        assert!(p.fragmentation() > 0);
        let free_before = p.free_space();
        p.compact();
        assert!(p.free_space() > free_before, "compaction reclaimed nothing");
        assert_eq!(p.fragmentation(), 0);
        for (i, s) in ids.iter().enumerate() {
            if i % 2 == 0 {
                assert!(p.get(*s).is_err(), "deleted slot came back");
            } else {
                assert_eq!(
                    p.get(*s).unwrap(),
                    &[i as u8; 200],
                    "slot {i} changed identity"
                );
            }
        }
    }

    #[test]
    fn compaction_preserves_the_lsn() {
        let mut p = Page::new();
        p.set_lsn(Lsn(1234));
        p.insert(b"x").unwrap();
        p.compact();
        assert_eq!(p.lsn(), Lsn(1234));
    }

    #[test]
    fn serialisation_round_trips() {
        let mut p = Page::new();
        p.set_lsn(Lsn(99));
        let s = p.insert(b"durable").unwrap();
        p.seal();
        let back = Page::from_bytes(*p.as_bytes()).unwrap();
        assert_eq!(back.lsn(), Lsn(99));
        assert_eq!(back.get(s).unwrap(), b"durable");
    }

    #[test]
    fn a_single_flipped_bit_is_caught_by_the_checksum() {
        let mut p = Page::new();
        p.insert(b"important").unwrap();
        p.seal();
        for i in [4usize, 100, PAGE_SIZE - 1] {
            let mut bytes = *p.as_bytes();
            bytes[i] ^= 0x01;
            assert!(
                Page::from_bytes(bytes).is_err(),
                "corruption at byte {i} went undetected"
            );
        }
    }

    #[test]
    fn a_self_inconsistent_header_is_rejected_even_with_a_valid_checksum() {
        let mut p = Page::new();
        p.insert(b"x").unwrap();
        // Claim more slots than could possibly fit, then re-checksum so the
        // only thing standing between us and a bad index is the header check.
        let mut bytes = *p.as_bytes();
        bytes[12..14].copy_from_slice(&u16::MAX.to_le_bytes());
        let mut tmp = Page {
            bytes: Box::new(bytes),
        };
        tmp.seal();
        assert!(Page::from_bytes(*tmp.as_bytes()).is_err());
    }

    #[test]
    fn reading_a_slot_beyond_the_array_is_an_error() {
        let p = Page::new();
        assert!(p.get(SlotId(0)).is_err());
        assert!(p.get(SlotId(9999)).is_err());
    }
}
