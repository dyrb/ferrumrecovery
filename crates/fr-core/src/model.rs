//! Catalog of found files shared by NTFS, FAT, exFAT and signature scanners,
//! and used by filtering, integrity checks and recovery.

use std::collections::HashMap;

use crate::part::FsKind;

/// No entry (no parent, no owner).
pub const NONE: u32 = u32::MAX;

/// Display name of the folder that holds files whose parent folder is unknown. `<` cannot
/// appear in a real name, so it never mixes with a real folder.
pub const LOST_DIR: &str = "<Lost files>";
pub const FOUND_MFT_ROOT: &str = "<Found MFT records>";

/// Entry flags.
pub mod ef {
    pub const DELETED: u16 = 1 << 0;
    pub const DIR: u16 = 1 << 1;
    /// Contents stored inside the file record itself.
    pub const RESIDENT: u16 = 1 << 2;
    pub const COMPRESSED: u16 = 1 << 3;
    pub const SPARSE: u16 = 1 << 4;
    pub const ENCRYPTED: u16 = 1 << 5;
    /// Parent folder unknown or reused by another file.
    pub const ORPHAN: u16 = 1 << 6;
    /// Has named data streams.
    pub const HAS_ADS: u16 = 1 << 7;
    /// File system metadata (`$MFT`, `$Extend\...`).
    pub const SYSTEM: u16 = 1 << 8;
    /// Part of the data location is lost (its extension record was reused).
    pub const INCOMPLETE: u16 = 1 << 9;
    /// In the Recycle Bin, not deleted yet.
    pub const RECYCLED: u16 = 1 << 10;
    /// Only the name survived (change journal, folder index): nothing to recover.
    pub const TRACE: u16 = 1 << 11;
    /// Found by a deep search of free space (a lost folder, a file record outside the MFT).
    pub const FOUND: u16 = 1 << 12;
    /// A named data stream of a file (NTFS alternate data stream), named `file:stream`.
    pub const STREAM: u16 = 1 << 13;
    /// Also found by a structurally delimited file signature at the same location.
    pub const SIGNATURE: u16 = 1 << 14;
    /// Reconstructed using an older file-system layout.
    pub const HISTORICAL: u16 = 1 << 15;
}

/// Ids of trace entries start here, above any MFT record number.
pub const TRACE_ID_BASE: u64 = 10_000_000_000;
/// Ids of entries found by a deep search start here, above trace ids.
pub const FOUND_ID_BASE: u64 = 20_000_000_000;
/// Ids of named data streams start here, above every other kind.
pub const STREAM_ID_BASE: u64 = 40_000_000_000;

/// How much of a file can come back.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum State {
    /// Every cluster is free and claimed by no newer file.
    #[default]
    Good,
    /// Some clusters are in use by other files, or part of the location is lost.
    Partial,
    /// All clusters are in use by other files.
    Overwritten,
    /// Sampled data ranges contain zeros; the cause is unknown.
    Zeroed,
    /// No data location survived.
    NoData,
    /// EFS encrypted: the raw data can be read but not decrypted.
    Encrypted,
    /// The data where the file was does not start like its type: another file took the
    /// place (found by `--check`).
    Mismatch,
    /// Data locations are known, but allocation metadata could not be read reliably.
    Unknown,
}

impl State {
    pub const ALL: [State; 8] =
        [State::Good, State::Partial, State::Overwritten, State::Zeroed, State::NoData, State::Encrypted, State::Mismatch, State::Unknown];

    pub fn name(self) -> &'static str {
        match self {
            State::Good => "good",
            State::Partial => "partial",
            State::Overwritten => "overwritten",
            State::Zeroed => "zeroed",
            State::NoData => "no-data",
            State::Encrypted => "encrypted",
            State::Mismatch => "mismatch",
            State::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Option<State> {
        State::ALL.into_iter().find(|st| st.name().eq_ignore_ascii_case(s))
    }
}

/// A run of clusters of a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    /// First cluster, or [`Run::SPARSE`] / [`Run::MISSING`].
    pub lcn: u64,
    pub len: u64,
}

impl Run {
    /// Never allocated: reads as zeros.
    pub const SPARSE: u64 = u64::MAX;
    /// Location lost: reads as zeros and makes the file partial.
    pub const MISSING: u64 = u64::MAX - 1;

    #[inline]
    pub fn is_real(&self) -> bool {
        self.lcn < Run::MISSING
    }
}

/// Where the contents of an entry are.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Data {
    #[default]
    None,
    /// Inside the file record `Entry::id`, at byte `off`, `len` bytes.
    Resident { off: u16, len: u16 },
    /// Inside a file record found outside the MFT at volume byte `at` (deep search).
    Inline { at: u64, off: u16, len: u16, record_size: u32 },
    /// Resident data in a FILE record split across physical extents.
    RecordRuns { start: u32, count: u32, record_size: u32, off: u16, len: u16 },
    /// `count` runs from `Catalog::runs[start..]`.
    Runs { start: u32, count: u32 },
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Entry {
    /// File system id: MFT record number on NTFS. Stable between scans of one volume.
    pub id: u64,
    /// Sequence number of the record (NTFS).
    pub seq: u16,
    pub flags: u16,
    pub state: State,
    /// NTFS compression unit as a power of two in clusters, 0 if not compressed.
    pub cu: u8,
    /// Win32 file attributes.
    pub attrs: u32,
    /// Parent entry index, or [`NONE`].
    pub parent: u32,
    pub name_off: u32,
    pub name_len: u16,
    pub size: u64,
    /// Bytes from the start that hold written data; the rest reads as zeros.
    pub valid: u64,
    /// FILETIME values (100 ns since 1601, UTC).
    pub created: u64,
    pub modified: u64,
    pub accessed: u64,
    pub changed: u64,
    /// When the file was deleted (FILETIME), 0 if unknown.
    pub deleted_at: u64,
    pub data: Data,
    /// Clusters of this file now in use by something else.
    pub lost: u64,
}

impl Entry {
    #[inline]
    pub fn is_deleted(&self) -> bool {
        self.flags & ef::DELETED != 0
    }
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.flags & ef::DIR != 0
    }
    /// Deleted, recycled, or found independently of the current directory tree.
    #[inline]
    pub fn is_gone(&self) -> bool {
        self.flags & (ef::DELETED | ef::RECYCLED | ef::FOUND) != 0
    }
    /// Most recent of the timestamps: which of two files claiming a cluster is newer.
    pub fn newest_time(&self) -> u64 {
        self.created.max(self.modified).max(self.changed)
    }
}

/// The scanned file system.
#[derive(Clone, Debug)]
pub struct FsInfo {
    pub kind: FsKind,
    /// Byte offset of the volume inside the source.
    pub offset: u64,
    pub size: u64,
    pub cluster_size: u32,
    /// NTFS: size of a file record and where the MFT lies (to re-read resident data).
    pub record_size: u32,
    pub mft: Vec<Run>,
    pub serial: u64,
    /// Id of the root folder.
    pub root: u64,
    /// Byte offset, inside the volume, of cluster number 0 of the runs (NTFS: 0; FAT and
    /// exFAT: the start of the data area, whose first cluster is numbered 2 on disk).
    pub data_offset: u64,
}

impl FsInfo {
    /// Byte offset inside the source of cluster `lcn` of the runs.
    #[inline]
    pub fn cluster_pos(&self, lcn: u64) -> u64 {
        self.offset + self.data_offset + lcn * self.cluster_size as u64
    }
}

/// Collects entries in any order with parents given as builder indices; [`Builder::finish`]
/// sorts them by id and rewrites the parent links.
#[derive(Default)]
pub struct Builder {
    entries: Vec<Entry>,
    names: Vec<u8>,
    runs: Vec<Run>,
}

impl Builder {
    /// Add `e` named `name` with data `runs` (ignored when empty); `e.parent` is a value
    /// returned by an earlier `add`, or [`NONE`]. Returns the new entry's builder index.
    pub fn add(&mut self, mut e: Entry, name: &str, runs: &[Run]) -> u32 {
        let mut end = name.len().min(u16::MAX as usize);
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        e.name_off = self.names.len() as u32;
        e.name_len = end as u16;
        self.names.extend_from_slice(&name.as_bytes()[..end]);
        if !runs.is_empty() {
            e.data = Data::Runs { start: self.runs.len() as u32, count: runs.len() as u32 };
            self.runs.extend_from_slice(runs);
        }
        self.entries.push(e);
        (self.entries.len() - 1) as u32
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entry_mut(&mut self, i: u32) -> &mut Entry {
        &mut self.entries[i as usize]
    }

    pub fn finish(self, fs: FsInfo) -> Catalog {
        let mut order: Vec<u32> = (0..self.entries.len() as u32).collect();
        order.sort_by_key(|&i| self.entries[i as usize].id);
        let mut new_of = vec![NONE; self.entries.len()];
        for (new, &old) in order.iter().enumerate() {
            new_of[old as usize] = new as u32;
        }
        let entries = order
            .iter()
            .map(|&old| {
                let mut e = self.entries[old as usize];
                if e.parent != NONE {
                    e.parent = new_of[e.parent as usize];
                }
                e
            })
            .collect();
        Catalog::new(fs, entries, self.names, self.runs)
    }
}

pub struct Catalog {
    pub fs: FsInfo,
    /// Address unit of `runs` and `fs.mft`; may differ from the current FS cluster size.
    pub unit: u32,
    /// Sorted by id.
    pub entries: Vec<Entry>,
    /// UTF-8 names.
    pub names: Vec<u8>,
    pub runs: Vec<Run>,
    /// Original paths of entries that were moved before deletion (Recycle Bin), from the
    /// root: `\Users\Me\Desktop\photo.jpg`. Applies to the entry and everything below it.
    pub alias: HashMap<u32, String>,
}

impl Catalog {
    pub fn new(fs: FsInfo, entries: Vec<Entry>, names: Vec<u8>, runs: Vec<Run>) -> Catalog {
        let unit = fs.cluster_size;
        Catalog { fs, unit, entries, names, runs, alias: HashMap::new() }
    }

    /// NTFS bookkeeping is hidden by default, but $Extend/$Deleted holds user data.
    /// Reapply on session load too, so older catalogs do not need another disk scan.
    pub fn classify_ntfs_system(&mut self) {
        if self.fs.kind != FsKind::Ntfs {
            return;
        }
        let mut verdicts = vec![0u8; self.entries.len()];
        let mut chain = Vec::new();
        for i in 0..self.entries.len() {
            chain.clear();
            let mut cur = i;
            let mut verdict = 2;
            for _ in 0..1024 {
                if verdicts[cur] != 0 {
                    verdict = verdicts[cur];
                    break;
                }
                let e = &self.entries[cur];
                chain.push(cur);
                // Imported layouts already have their own classification and IDs.
                if e.flags & (ef::FOUND | ef::HISTORICAL) != 0 {
                    verdict = 3;
                    break;
                }
                if e.is_dir()
                    && e.parent != NONE
                    && self.entries[e.parent as usize].id == 11
                    && self.name(cur as u32).eq_ignore_ascii_case("$Deleted")
                {
                    verdict = 4;
                    break;
                }
                if e.id < 24 && e.id != 5 {
                    verdict = 1;
                    break;
                }
                if e.parent == NONE {
                    break;
                }
                cur = e.parent as usize;
            }
            for &k in &chain {
                verdicts[k] = verdict;
            }
        }
        for (e, verdict) in self.entries.iter_mut().zip(verdicts) {
            match verdict {
                1 => e.flags |= ef::SYSTEM,
                4 => e.flags &= !ef::SYSTEM,
                _ => {}
            }
        }
    }

    /// Append a name to the name table; returns (offset, length).
    pub fn add_name(&mut self, name: &str) -> (u32, u16) {
        let mut end = name.len().min(u16::MAX as usize);
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        let off = self.names.len() as u32;
        self.names.extend_from_slice(&name.as_bytes()[..end]);
        (off, end as u16)
    }

    pub fn name(&self, i: u32) -> &str {
        let e = &self.entries[i as usize];
        std::str::from_utf8(&self.names[e.name_off as usize..e.name_off as usize + e.name_len as usize]).unwrap_or("")
    }

    pub fn runs_of(&self, i: u32) -> &[Run] {
        match self.entries[i as usize].data {
            Data::Runs { start, count } => &self.runs[start as usize..(start + count) as usize],
            _ => &[],
        }
    }

    /// Clusters with data on disk (not sparse, not missing).
    pub fn real_clusters(&self, i: u32) -> u64 {
        self.runs_of(i).iter().filter(|r| r.is_real()).map(|r| r.len).sum()
    }

    /// MFT record holding entry `i` (a stream lives in its file's record).
    pub fn record_of(&self, i: u32) -> u64 {
        let e = &self.entries[i as usize];
        if e.flags & ef::STREAM == 0 {
            return e.id;
        }
        // The file is the entry with the same parent whose name is the part before ':'.
        let name = self.name(i);
        let base = name.rsplit_once(':').map_or(name, |x| x.0);
        self.entries
            .iter()
            .enumerate()
            .find(|(k, f)| f.parent == e.parent && f.seq == e.seq && f.flags & ef::STREAM == 0 && self.name(*k as u32) == base)
            .map_or(e.id, |(_, f)| f.id)
    }

    pub fn find(&self, id: u64) -> Option<u32> {
        self.entries.binary_search_by_key(&id, |e| e.id).ok().map(|i| i as u32)
    }

    /// Folders from the root down to `i` (inclusive). The first element is [`NONE`] when
    /// the chain ends at an unknown parent.
    pub fn chain(&self, i: u32) -> Vec<u32> {
        let mut v = vec![i];
        let mut cur = i;
        // A reused record can link deleted folders into a loop; the depth bound breaks it.
        for _ in 0..1024 {
            let e = &self.entries[cur as usize];
            if e.parent == NONE {
                if e.flags & ef::ORPHAN != 0 {
                    v.push(NONE);
                }
                break;
            }
            cur = e.parent;
            v.push(cur);
        }
        v.reverse();
        v
    }

    /// `\folder\file`, or `<Lost files>\file` for an unknown parent. Entries from the
    /// Recycle Bin show their original location.
    pub fn path(&self, i: u32) -> String {
        self.path_with(i, LOST_DIR, |k| std::borrow::Cow::Borrowed(self.name(k)))
    }

    /// Format virtual folder names for an interface while preserving original aliases.
    pub fn path_with<'a>(&'a self, i: u32, lost: &str, name: impl Fn(u32) -> std::borrow::Cow<'a, str>) -> String {
        let chain = self.chain(i);
        let from = chain.iter().rposition(|k| self.alias.contains_key(k));
        let mut s = String::new();
        let rest = match from {
            Some(p) => {
                s.push_str(&self.alias[&chain[p]]);
                &chain[p + 1..]
            }
            None => &chain[..],
        };
        for &k in rest {
            if k == NONE {
                s.push_str(lost);
                continue;
            }
            if self.entries[k as usize].id == self.fs.root && k != i {
                continue;
            }
            s.push('\\');
            s.push_str(&name(k));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cat(entries: Vec<(u64, u32, u16, &str)>) -> Catalog {
        let mut names = Vec::new();
        let entries = entries
            .into_iter()
            .map(|(id, parent, flags, n)| {
                let off = names.len() as u32;
                names.extend_from_slice(n.as_bytes());
                Entry { id, parent, flags, name_off: off, name_len: n.len() as u16, ..Default::default() }
            })
            .collect();
        let fs = FsInfo {
            kind: FsKind::Ntfs,
            offset: 0,
            size: 0,
            cluster_size: 4096,
            record_size: 1024,
            mft: vec![],
            serial: 0,
            root: 5,
            data_offset: 0,
        };
        Catalog::new(fs, entries, names, vec![])
    }

    #[test]
    fn paths_and_orphans() {
        let c = cat(vec![
            (5, NONE, ef::DIR, "."),
            (40, 0, ef::DIR, "Photos"),
            (41, 1, ef::DELETED, "cat.jpg"),
            (42, NONE, ef::DELETED | ef::ORPHAN, "lost.doc"),
        ]);
        assert_eq!(c.path(2), r"\Photos\cat.jpg");
        assert_eq!(c.path(3), r"<Lost files>\lost.doc");
        assert_eq!(c.path_with(3, "<Missing parent>", |k| c.name(k).into()), r"<Missing parent>\lost.doc");
        assert_eq!(c.find(41), Some(2));
        assert_eq!(c.find(43), None);
    }

    #[test]
    fn deleted_user_files_are_exposed_without_exposing_other_ntfs_metadata() {
        let mut c = cat(vec![
            (5, NONE, ef::DIR, "."),
            (11, 0, ef::DIR, "$Extend"),
            (40, 1, ef::DIR | ef::SYSTEM, "$Deleted"),
            (41, 2, ef::DIR | ef::SYSTEM, "old"),
            (42, 3, ef::DELETED | ef::SYSTEM, "photo.jpg"),
            (43, 1, 0, "$UsnJrnl"),
            (44, 0, ef::SYSTEM, "$I000001"),
            (50_000_000_000, 0, ef::FOUND | ef::SYSTEM, "historical metadata"),
        ]);
        c.classify_ntfs_system();
        assert_eq!(c.entries[1].flags & ef::SYSTEM, ef::SYSTEM);
        assert_eq!(c.entries[4].flags, ef::DELETED);
        assert_eq!(c.entries[2].flags & ef::SYSTEM, 0);
        assert_eq!(c.entries[5].flags & ef::SYSTEM, ef::SYSTEM);
        assert_eq!(c.entries[6].flags & ef::SYSTEM, ef::SYSTEM);
        assert_eq!(c.entries[7].flags & ef::SYSTEM, ef::SYSTEM);
        c.classify_ntfs_system();
        assert_eq!(c.entries[4].flags, ef::DELETED);
    }

    #[test]
    fn recycle_bin_aliases() {
        let mut c = cat(vec![
            (5, NONE, ef::DIR, "."),
            (30, 0, ef::DIR, "$Recycle.Bin"),
            (31, 1, ef::DIR | ef::DELETED, "$RAB12CD"),
            (32, 2, ef::DELETED, "a.txt"),
        ]);
        c.alias.insert(2, r"\Users\Me\Old".into());
        assert_eq!(c.path(3), r"\Users\Me\Old\a.txt");
        assert_eq!(c.path_with(3, "<Missing parent>", |k| c.name(k).into()), r"\Users\Me\Old\a.txt");
        assert_eq!(c.path(1), r"\$Recycle.Bin");
    }

    #[test]
    fn builder_sorts_and_relinks() {
        let mut b = Builder::default();
        let root = b.add(Entry { id: 9, parent: NONE, flags: ef::DIR, ..Default::default() }, "", &[]);
        let d = b.add(Entry { id: 3, parent: root, flags: ef::DIR, ..Default::default() }, "Docs", &[]);
        b.add(Entry { id: 5, parent: d, size: 10, ..Default::default() }, "a.txt", &[Run { lcn: 7, len: 1 }]);
        let fs = FsInfo {
            kind: FsKind::Fat32,
            offset: 0,
            size: 0,
            cluster_size: 512,
            record_size: 0,
            mft: vec![],
            serial: 0,
            root: 9,
            data_offset: 0,
        };
        let c = b.finish(fs);
        assert_eq!(c.entries.iter().map(|e| e.id).collect::<Vec<_>>(), [3, 5, 9]);
        let a = c.find(5).unwrap();
        assert_eq!(c.path(a), r"\Docs\a.txt");
        assert_eq!(c.runs_of(a), &[Run { lcn: 7, len: 1 }]);
    }

    #[test]
    fn loops_terminate() {
        let c = cat(vec![(60, 1, ef::DIR | ef::DELETED, "a"), (61, 0, ef::DIR | ef::DELETED, "b")]);
        assert!(c.path(0).len() < 10_000);
    }

    #[test]
    fn state_names_roundtrip() {
        for s in State::ALL {
            assert_eq!(State::parse(s.name()), Some(s));
        }
    }
}
