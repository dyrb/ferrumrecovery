//! # fr-ntfs
//!
//! Finds deleted files on NTFS by reading every record of the master file table, used or
//! free, straight from the volume.
//!
//! * [`boot`]    - NTFS boot sector;
//! * [`runlist`] - data runs (mapping pairs);
//! * [`record`]  - file record fixup and attribute decoding;
//! * [`mft`]     - where the MFT lies and the parallel reader of its records;
//! * [`scan`]    - turning records into a catalog of deleted files with their state;
//! * extras      - Recycle Bin names, change journal, folder index slack;
//! * deep        - file records found in free space (an old MFT after a quick format).
//!
//! The source is opened read-only; this crate writes nothing.

pub mod boot;
mod bytes;
mod deep;
pub mod error;
mod extras;
pub mod loose;
pub mod mft;
pub mod record;
pub mod runlist;
pub mod scan;

pub use error::{Error, Result};
pub use mft::Progress;
pub use scan::{Owner, Scan, ScanOptions, ScanReport, owners_of, scan};
