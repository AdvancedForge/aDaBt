//! Physical storage.
//!
//! Everything here sits below the `LogicalStore` contract and may be replaced
//! wholesale without any logical signature changing. The layering is deliberate:
//! `codec` turns records into bytes, `page` arranges bytes into pages, and later
//! milestones add the buffer pool, the write-ahead log, and the heap
//! representation on top.

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
