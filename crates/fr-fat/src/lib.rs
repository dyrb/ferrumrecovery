//! # fr-fat
//!
//! Finds deleted files on FAT12, FAT16, FAT32 ([`fat`]) and exFAT ([`exfat`]) volumes:
//! flash drives and memory cards. Directories are walked from the root, deleted folders
//! included; every deleted entry becomes a catalog entry with its data location and a
//! state from the allocation table or bitmap. The source is only read.

pub mod exfat;
pub mod fat;
mod io;

use std::time::Duration;

use fr_core::model::{Catalog, Data, State};
use fr_core::part::FsKind;
use fr_core::source::Source;

#[derive(Clone, Debug, Default)]
pub struct ScanOptions {
    /// Keep files that are not deleted too.
    pub existing: bool,
    /// Also read free space for lost folders (cut off from the tree, or left by a quick
    /// format).
    pub deep: bool,
    pub progress: Option<std::sync::Arc<fr_core::sweep::SweepProgress>>,
}

#[derive(Clone, Debug, Default)]
pub struct FatReport {
    pub dirs: u64,
    pub deleted_files: u64,
    pub deleted_dirs: u64,
    /// Folders found by the deep search.
    pub lost_dirs: u64,
    /// Files found in free space by their signatures.
    pub carved: u64,
    pub bad_bytes: u64,
    /// Allocation metadata is missing or had unreadable sectors.
    pub no_allocation: bool,
    pub t: Duration,
}

pub struct FatScan {
    pub catalog: Catalog,
    pub report: FatReport,
}

/// Scan the FAT or exFAT volume at `base` of `src`.
pub fn scan(src: &Source, base: u64, kind: FsKind, opts: &ScanOptions) -> Result<FatScan, fr_core::error::Error> {
    match kind {
        FsKind::ExFat => exfat::scan(src, base, opts),
        FsKind::Fat32 | FsKind::Fat16 => fat::scan(src, base, opts),
        other => Err(fr_core::error::Error::value("{} is not a FAT file system", other.name())),
    }
}

/// States of deleted files from the clusters other files took (`Entry::lost`).
pub(crate) fn assign_states(cat: &mut Catalog, allocation_known: bool) {
    for i in 0..cat.entries.len() {
        let e = cat.entries[i];
        let real = cat.real_clusters(i as u32);
        cat.entries[i].state = if !e.is_deleted() || e.is_dir() || e.size == 0 {
            State::Good
        } else if e.data == Data::None {
            State::NoData
        } else if !allocation_known {
            State::Unknown
        } else if real > 0 && e.lost >= real {
            State::Overwritten
        } else if e.lost > 0 || e.flags & fr_core::ef::INCOMPLETE != 0 || e.state == State::Partial {
            State::Partial
        } else {
            State::Good
        };
    }
}
