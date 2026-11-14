//! Physical storage.
//!
//! Everything here sits below the `LogicalStore` contract and may be replaced
//! wholesale without any logical signature changing. The layering is deliberate:
//! `codec` turns records into bytes, `page` arranges bytes into pages, and the
//! buffer pool (`pager`), the write-ahead log (`wal`) and the heap
//! representation (`heap`) are built on top.

pub mod base_image;
pub mod catalog;
pub mod codec;
pub mod compress;
pub mod derived;
pub mod directory;
pub mod heap;
pub mod metadata;
pub mod page;
pub mod pager;
pub mod superblock;
pub mod varint;
pub mod version;
pub mod wal;

/// Make a rename, creation or removal in `dir` durable, not only the contents
/// of the files involved.
///
/// An fsync of a file says nothing about the directory entry naming it; a
/// crash can keep the bytes and lose the name, or bring back a name that was
/// removed. Only needed where the *presence* of a file is itself a claim — a
/// base-image marker, a log segment moved out of the log.
pub(crate) fn sync_dir(dir: &std::path::Path) -> adabt_core::error::Result<()> {
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}
