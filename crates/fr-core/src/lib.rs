//! # fr-core
//!
//! The parts of FerrumRecovery that do not depend on a file system:
//!
//! * [`source`]  - read-only access to a volume, a physical disk or an image file;
//! * [`part`]    - partition tables and file system detection;
//! * [`model`]   - the catalog of found files every scanner produces;
//! * [`filter`]  - selection by name, path, type, size, date and state;
//! * [`data`]    - reading the contents of a found file, including NTFS compression;
//! * [`check`]   - sampling zeros and type signatures (not an integrity guarantee);
//! * [`recover`] - writing files to a target on another volume;
//! * [`sweep`]   - one sequential pass over free space for the deep searches;
//! * [`carve`]   - files found by their signatures;
//! * [`session`] - saved scans;
//! * [`image`]   - imaging of failing drives in two passes, resumable.

pub mod carve;
pub mod check;
pub mod control;
pub mod data;
pub mod filter;
pub mod image;
pub mod lznt1;
pub mod merge;
pub mod model;
pub mod part;
pub mod recover;
pub mod session;
pub mod source;
pub mod sweep;
pub mod time;
pub mod types;

pub use model::{Catalog, Data, Entry, FsInfo, NONE, Run, State, ef};
pub use part::{FsKind, Part};
pub use source::Source;
pub mod devices;
pub mod error;
