//! Saved scans: a catalog and where it came from, in a file, so it can be listed,
//! filtered and recovered later without scanning again (a deep scan takes as long as
//! reading the disk). The boot sector of the scanned volume is kept to make sure the
//! same volume is read when files are recovered from a saved scan.

use std::collections::HashMap;
use std::io::{self, BufWriter, Write};
use std::path::Path;

use crate::model::{Catalog, Data, Entry, FsInfo, Run, State};
use crate::part::{FsKind, Part};

const MAGIC: &[u8; 8] = b"FRSCAN02";

pub struct Session {
    /// How the source was given: `C:`, `disk1`, an image path.
    pub source: String,
    pub part: Part,
    /// First 512 bytes of the volume when it was scanned.
    pub boot: Vec<u8>,
    /// Header lines of the scan report.
    pub notes: Vec<String>,
    pub catalog: Catalog,
}

fn kind_code(k: FsKind) -> u8 {
    match k {
        FsKind::Ntfs => 1,
        FsKind::Fat32 => 2,
        FsKind::Fat16 => 3,
        FsKind::ExFat => 4,
        FsKind::Unknown => 0,
    }
}

fn kind_of(c: u8) -> FsKind {
    match c {
        1 => FsKind::Ntfs,
        2 => FsKind::Fat32,
        3 => FsKind::Fat16,
        4 => FsKind::ExFat,
        _ => FsKind::Unknown,
    }
}

struct W<T: Write>(T);

impl<T: Write> W<T> {
    fn u8(&mut self, v: u8) -> io::Result<()> {
        self.0.write_all(&[v])
    }
    fn u16(&mut self, v: u16) -> io::Result<()> {
        self.0.write_all(&v.to_le_bytes())
    }
    fn u32(&mut self, v: u32) -> io::Result<()> {
        self.0.write_all(&v.to_le_bytes())
    }
    fn u64(&mut self, v: u64) -> io::Result<()> {
        self.0.write_all(&v.to_le_bytes())
    }
    fn bytes(&mut self, b: &[u8]) -> io::Result<()> {
        self.u64(b.len() as u64)?;
        self.0.write_all(b)
    }
    fn str(&mut self, s: &str) -> io::Result<()> {
        self.bytes(s.as_bytes())
    }
}

struct R<'a> {
    b: &'a [u8],
    p: usize,
}

fn bad() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, crate::error::Error::new("saved scan is damaged or from another version"))
}

impl R<'_> {
    fn take(&mut self, n: usize) -> io::Result<&[u8]> {
        let end = self.p.checked_add(n).filter(|&e| e <= self.b.len()).ok_or_else(bad)?;
        let s = &self.b[self.p..end];
        self.p = end;
        Ok(s)
    }
    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn bytes(&mut self) -> io::Result<Vec<u8>> {
        let n = self.u64()? as usize;
        Ok(self.take(n)?.to_vec())
    }
    fn str(&mut self) -> io::Result<String> {
        String::from_utf8(self.bytes()?).map_err(|_| bad())
    }

    fn count(&mut self, minimum_bytes: usize) -> io::Result<usize> {
        let n = usize::try_from(self.u64()?).map_err(|_| bad())?;
        if n > self.b.len().saturating_sub(self.p) / minimum_bytes {
            return Err(bad());
        }
        Ok(n)
    }
}

pub fn save(path: &Path, s: &Session) -> io::Result<()> {
    save_catalog(path, &s.source, &s.part, &s.boot, &s.notes, &s.catalog)
}

/// Write a catalog without cloning its arenas. Existing files are never truncated.
pub fn save_catalog(path: &Path, source: &str, part: &Part, boot: &[u8], notes: &[String], catalog: &Catalog) -> io::Result<()> {
    let mut w = W(BufWriter::new(std::fs::File::create_new(path)?));
    w.0.write_all(MAGIC)?;
    w.str(source)?;
    let p = part;
    w.u32(p.index)?;
    w.u64(p.offset)?;
    w.u64(p.size)?;
    w.u8(kind_code(p.fs))?;
    w.str(&p.label)?;
    w.bytes(boot)?;
    w.u64(notes.len() as u64)?;
    for n in notes {
        w.str(n)?;
    }
    let c = catalog;
    let fs = &c.fs;
    w.u8(kind_code(fs.kind))?;
    for v in [fs.offset, fs.size, fs.serial, fs.root, fs.data_offset] {
        w.u64(v)?;
    }
    w.u32(fs.cluster_size)?;
    w.u32(fs.record_size)?;
    w.u32(c.unit)?;
    w.u64(fs.mft.len() as u64)?;
    for r in &fs.mft {
        w.u64(r.lcn)?;
        w.u64(r.len)?;
    }
    w.u64(c.entries.len() as u64)?;
    for e in &c.entries {
        w.u64(e.id)?;
        w.u16(e.seq)?;
        w.u16(e.flags)?;
        w.u8(e.state as u8)?;
        w.u8(e.cu)?;
        w.u32(e.attrs)?;
        w.u32(e.parent)?;
        w.u32(e.name_off)?;
        w.u16(e.name_len)?;
        for v in [e.size, e.valid, e.created, e.modified, e.accessed, e.changed, e.deleted_at, e.lost] {
            w.u64(v)?;
        }
        match e.data {
            Data::None => w.u8(0)?,
            Data::Resident { off, len } => {
                w.u8(1)?;
                w.u16(off)?;
                w.u16(len)?;
            }
            Data::Inline { at, off, len, record_size } => {
                w.u8(2)?;
                w.u64(at)?;
                w.u16(off)?;
                w.u16(len)?;
                w.u32(record_size)?;
            }
            Data::Runs { start, count } => {
                w.u8(3)?;
                w.u32(start)?;
                w.u32(count)?;
            }
            Data::RecordRuns { start, count, record_size, off, len } => {
                w.u8(4)?;
                w.u32(start)?;
                w.u32(count)?;
                w.u32(record_size)?;
                w.u16(off)?;
                w.u16(len)?;
            }
        }
    }
    w.bytes(&c.names)?;
    w.u64(c.runs.len() as u64)?;
    for r in &c.runs {
        w.u64(r.lcn)?;
        w.u64(r.len)?;
    }
    let mut alias: Vec<_> = c.alias.iter().collect();
    alias.sort();
    w.u64(alias.len() as u64)?;
    for (k, v) in alias {
        w.u32(*k)?;
        w.str(v)?;
    }
    w.0.flush()
}

pub fn load(path: &Path) -> io::Result<Session> {
    let data = std::fs::read(path)?;
    if data.len() < 8 || (&data[..8] != MAGIC && &data[..8] != b"FRSCAN01") {
        return Err(io::Error::new(io::ErrorKind::InvalidData, crate::error::Error::new("not a saved scan")));
    }
    let legacy = &data[..8] == b"FRSCAN01";
    let mut r = R { b: &data, p: 8 };
    let source = r.str()?;
    let part = Part { index: r.u32()?, offset: r.u64()?, size: r.u64()?, fs: kind_of(r.u8()?), label: r.str()? };
    let boot = r.bytes()?;
    let n = r.count(8)?;
    if n > 1 << 16 {
        return Err(bad());
    }
    let notes = (0..n).map(|_| r.str()).collect::<io::Result<Vec<_>>>()?;
    let kind = kind_of(r.u8()?);
    let (offset, size, serial, root, data_offset) = (r.u64()?, r.u64()?, r.u64()?, r.u64()?, r.u64()?);
    let (cluster_size, record_size) = (r.u32()?, r.u32()?);
    let unit = if legacy { cluster_size } else { r.u32()? };
    if !unit.is_power_of_two() || !(512..=32 << 20).contains(&unit) {
        return Err(bad());
    }
    let max_cluster = match kind {
        FsKind::ExFat => 32 << 20,
        FsKind::Ntfs => 2 << 20,
        FsKind::Fat16 | FsKind::Fat32 => 512 << 10,
        FsKind::Unknown => 2 << 20,
    };
    if !cluster_size.is_power_of_two()
        || cluster_size < 512
        || cluster_size > max_cluster
        || (record_size != 0 && (!(512..=65536).contains(&record_size) || !record_size.is_power_of_two()))
        || kind == FsKind::Ntfs && record_size == 0
    {
        return Err(bad());
    }
    let n = r.count(16)?;
    let mft = (0..n).map(|_| Ok(Run { lcn: r.u64()?, len: r.u64()? })).collect::<io::Result<Vec<_>>>()?;
    let fs = FsInfo { kind, offset, size, cluster_size, record_size, mft, serial, root, data_offset };
    let n = r.count(93)?;
    if n >= u32::MAX as usize {
        return Err(bad());
    }
    let mut entries = Vec::new();
    entries.try_reserve_exact(n).map_err(|_| bad())?;
    for _ in 0..n {
        let mut e = Entry { id: r.u64()?, seq: r.u16()?, flags: r.u16()?, ..Default::default() };
        e.state = *State::ALL.get(r.u8()? as usize).ok_or_else(bad)?;
        e.cu = r.u8()?;
        e.attrs = r.u32()?;
        e.parent = r.u32()?;
        e.name_off = r.u32()?;
        e.name_len = r.u16()?;
        [e.size, e.valid, e.created, e.modified, e.accessed, e.changed, e.deleted_at, e.lost] =
            [r.u64()?, r.u64()?, r.u64()?, r.u64()?, r.u64()?, r.u64()?, r.u64()?, r.u64()?];
        e.data = match r.u8()? {
            0 => Data::None,
            1 => Data::Resident { off: r.u16()?, len: r.u16()? },
            2 => Data::Inline { at: r.u64()?, off: r.u16()?, len: r.u16()?, record_size: if legacy { record_size } else { r.u32()? } },
            3 => Data::Runs { start: r.u32()?, count: r.u32()? },
            4 if !legacy => Data::RecordRuns { start: r.u32()?, count: r.u32()?, record_size: r.u32()?, off: r.u16()?, len: r.u16()? },
            _ => return Err(bad()),
        };
        entries.push(e);
    }
    let names = r.bytes()?;
    let n = r.count(16)?;
    let runs = (0..n).map(|_| Ok(Run { lcn: r.u64()?, len: r.u64()? })).collect::<io::Result<Vec<_>>>()?;
    // Links must stay inside the tables.
    for e in &entries {
        let bad_link = e.parent != crate::NONE && e.parent as usize >= entries.len();
        let bad_name = e.name_off as usize + e.name_len as usize > names.len();
        let bad_runs = matches!(e.data, Data::Runs { start, count } | Data::RecordRuns { start, count, .. } if start as usize + count as usize > runs.len());
        if bad_link || bad_name || bad_runs || e.cu > 16 || e.valid > e.size {
            return Err(bad());
        }
        std::str::from_utf8(&names[e.name_off as usize..e.name_off as usize + e.name_len as usize]).map_err(|_| bad())?;
    }
    // Reject cycles in linear time before tree traversal or aggregation.
    let mut colors = vec![0u8; entries.len()];
    for i in 0..entries.len() {
        let mut cur = i;
        while colors[cur] == 0 {
            colors[cur] = 1;
            let p = entries[cur].parent;
            if p == crate::NONE {
                break;
            }
            cur = p as usize;
            if colors[cur] == 1 {
                return Err(bad());
            }
        }
        cur = i;
        while colors[cur] == 1 {
            colors[cur] = 2;
            let p = entries[cur].parent;
            if p == crate::NONE {
                break;
            }
            cur = p as usize;
        }
    }
    if entries.windows(2).any(|w| w[0].id >= w[1].id) {
        return Err(bad());
    }
    for run in fs.mft.iter().chain(&runs) {
        if run.len == 0 || (run.is_real() && run.lcn.checked_add(run.len).and_then(|n| n.checked_mul(unit as u64)).is_none()) {
            return Err(bad());
        }
    }
    let n = r.count(12)?;
    let mut alias = HashMap::new();
    for _ in 0..n {
        let k = r.u32()?;
        if k as usize >= entries.len() {
            return Err(bad());
        }
        alias.insert(k, r.str()?);
    }
    let mut catalog = Catalog::new(fs, entries, names, runs);
    catalog.unit = unit;
    catalog.alias = alias;
    if r.p != data.len() || boot.len() != 512 || part.offset.checked_add(part.size).is_none() {
        return Err(bad());
    }
    if !geometry_valid(&catalog, &part) {
        return Err(bad());
    }
    catalog.classify_ntfs_system();
    Ok(Session { source, part, boot, notes, catalog })
}

fn geometry_valid(c: &Catalog, p: &Part) -> bool {
    let fs = &c.fs;
    if fs.kind != p.fs || fs.offset != p.offset || fs.size == 0 || fs.size > p.size || fs.data_offset >= fs.size {
        return false;
    }
    if fs.kind == FsKind::Ntfs && fs.data_offset != 0 {
        return false;
    }
    let cs = c.unit as u64;
    // Raw ranges may end in a partial sector. Only uncompressed logical bytes
    // within that sector are readable; allocation padding is not file content.
    let end_limit = if cs == 512 && fs.data_offset == 0 { fs.size.div_ceil(cs).checked_mul(cs).unwrap_or(fs.size) } else { fs.size };
    let inside = |r: &Run, base: u64| {
        r.len > 0
            && (!r.is_real()
                || r.lcn
                    .checked_add(r.len)
                    .and_then(|n| n.checked_mul(cs))
                    .and_then(|n| n.checked_add(base))
                    .is_some_and(|end| end <= end_limit))
    };
    if fs.mft.iter().any(|r| !r.is_real() || !inside(r, 0)) || c.runs.iter().any(|r| !inside(r, fs.data_offset)) {
        return false;
    }
    for (i, e) in c.entries.iter().enumerate() {
        if e.parent != crate::NONE && !c.entries[e.parent as usize].is_dir() {
            return false;
        }
        let resident = |off: u16, len: u16| fs.kind == FsKind::Ntfs && off as u32 + len as u32 <= fs.record_size;
        match e.data {
            Data::Resident { off, len } => {
                if !resident(off, len) {
                    return false;
                }
            }
            Data::Inline { at, off, len, record_size } => {
                if !record_size.is_power_of_two()
                    || !(512..=65536).contains(&record_size)
                    || off as u32 + len as u32 > record_size
                    || at.checked_add(record_size as u64).is_none_or(|end| end > fs.size)
                {
                    return false;
                }
            }
            Data::Runs { .. } => {
                let runs = c.runs_of(i as u32);
                if runs.iter().try_fold(0u64, |s, r| s.checked_add(r.len)).and_then(|n| n.checked_mul(cs)).is_none() {
                    return false;
                }
                let mut remaining = e.valid;
                for r in runs {
                    let bytes = r.len * cs;
                    let take = if e.cu == 0 { remaining.min(bytes) } else { bytes };
                    if r.is_real()
                        && r.lcn
                            .checked_mul(cs)
                            .and_then(|at| at.checked_add(take))
                            .and_then(|end| end.checked_add(fs.data_offset))
                            .is_none_or(|end| end > fs.size)
                    {
                        return false;
                    }
                    remaining = remaining.saturating_sub(bytes);
                }
            }
            Data::RecordRuns { start, count, record_size, off, len } => {
                let records = &c.runs[start as usize..start as usize + count as usize];
                if !record_size.is_power_of_two()
                    || !(512..=65536).contains(&record_size)
                    || off as u32 + len as u32 > record_size
                    || records.iter().any(|r| !r.is_real())
                    || records.iter().try_fold(0u64, |n, r| n.checked_add(r.len)).and_then(|n| n.checked_mul(cs))
                        != Some(record_size as u64)
                {
                    return false;
                }
            }
            Data::None => {}
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Builder, ef};

    #[test]
    fn saved_scan_rejects_data_outside_its_declared_partition() {
        let path = std::env::temp_dir().join(format!("fr-geometry-{}.frs", std::process::id()));
        let fixture = || {
            let fs = FsInfo {
                kind: FsKind::Ntfs,
                offset: 0,
                size: 8192,
                cluster_size: 512,
                record_size: 1024,
                mft: vec![Run { lcn: 2, len: 2 }],
                serial: 0,
                root: 5,
                data_offset: 0,
            };
            let entry = Entry {
                id: 42,
                parent: crate::NONE,
                size: 512,
                valid: 512,
                name_len: 8,
                data: Data::Runs { start: 0, count: 1 },
                ..Default::default()
            };
            Session {
                source: "fixture.img".into(),
                part: Part { index: 0, offset: 0, size: 8192, fs: FsKind::Ntfs, label: String::new() },
                boot: vec![0; 512],
                notes: vec![],
                catalog: Catalog::new(fs, vec![entry], b"file.bin".to_vec(), vec![Run { lcn: 4, len: 1 }]),
            }
        };
        for case in 0..6 {
            let mut s = fixture();
            match case {
                0 => s.catalog.fs.offset = 512,
                1 => s.catalog.runs[0].lcn = 16,
                2 => s.catalog.entries[0].data = Data::Inline { at: 8192, off: 0, len: 16, record_size: 1024 },
                3 => {
                    s.catalog.fs.kind = FsKind::Fat32;
                    s.part.fs = FsKind::Fat32;
                    s.catalog.fs.record_size = 4 * 1024 * 1024 + 512;
                    s.catalog.entries[0].data = Data::Inline { at: 0, off: 0, len: 16, record_size: 1024 };
                }
                4 => s.catalog.entries[0].parent = 0,
                _ => {
                    s.catalog.fs.data_offset = 4096;
                    s.catalog.fs.mft[0] = Run { lcn: 14, len: 2 };
                    s.catalog.entries[0].id = 0;
                    s.catalog.entries[0].data = Data::Resident { off: 64, len: 16 };
                }
            }
            save(&path, &s).unwrap();
            assert!(load(&path).is_err(), "case {case}");
            std::fs::remove_file(&path).unwrap();
        }
    }

    #[test]
    fn loading_older_catalog_exposes_deleted_user_data_without_reading_its_source() {
        let mut b = Builder::default();
        let root = b.add(Entry { id: 5, parent: crate::NONE, flags: ef::DIR, ..Default::default() }, "", &[]);
        let ext = b.add(Entry { id: 11, parent: root, flags: ef::DIR | ef::SYSTEM, ..Default::default() }, "$Extend", &[]);
        let deleted = b.add(Entry { id: 40, parent: ext, flags: ef::DIR | ef::SYSTEM, ..Default::default() }, "$Deleted", &[]);
        b.add(
            Entry { id: 41, parent: deleted, flags: ef::DELETED | ef::SYSTEM, size: 128, valid: 128, ..Default::default() },
            "file.txt",
            &[Run { lcn: 100, len: 1 }],
        );
        let fs = FsInfo {
            kind: FsKind::Ntfs,
            offset: 0,
            size: 1 << 20,
            cluster_size: 512,
            record_size: 1024,
            mft: vec![Run { lcn: 4, len: 16 }],
            serial: 0,
            root: 5,
            data_offset: 0,
        };
        let c = b.finish(fs);
        let p = std::env::temp_dir().join(format!("fr-system-migration-{}.frs", std::process::id()));
        let part = Part { index: 0, offset: 0, size: 1 << 20, fs: FsKind::Ntfs, label: String::new() };
        save_catalog(&p, "nonexistent-offline-source.img", &part, &[0; 512], &[], &c).unwrap();
        let session = load(&p).unwrap();
        let i = session.catalog.find(41).unwrap();
        assert!(crate::filter::Filter::default().accepts(&session.catalog, i));
        assert_eq!(session.catalog.entries[i as usize].flags & ef::SYSTEM, 0);
        std::fs::remove_file(p).unwrap();
    }

    #[test]
    fn save_and_load_roundtrip() {
        let mut b = Builder::default();
        let root = b.add(Entry { id: 5, parent: crate::NONE, flags: ef::DIR, ..Default::default() }, "", &[]);
        let d = b.add(Entry { id: 40, parent: root, flags: ef::DIR | ef::DELETED, deleted_at: 7, ..Default::default() }, "Фото", &[]);
        b.add(
            Entry { id: 41, parent: d, flags: ef::DELETED, size: 5000, state: State::Partial, ..Default::default() },
            "a.jpg",
            &[Run { lcn: 100, len: 2 }],
        );
        let fs = FsInfo {
            kind: FsKind::Ntfs,
            offset: 1 << 20,
            size: 1 << 30,
            cluster_size: 4096,
            record_size: 1024,
            mft: vec![Run { lcn: 4, len: 16 }],
            serial: 99,
            root: 5,
            data_offset: 0,
        };
        let mut cat = b.finish(fs);
        cat.alias.insert(1, r"\Users\Me\Old".into());
        let s = Session {
            source: "disk1".into(),
            part: Part { index: 2, offset: 1 << 20, size: 1 << 30, fs: FsKind::Ntfs, label: "Data".into() },
            boot: vec![0xEB; 512],
            notes: vec!["MFT: 1 record".into()],
            catalog: cat,
        };
        let p = std::env::temp_dir().join(format!("fr-session-{}.frs", std::process::id()));
        save(&p, &s).unwrap();
        let l = load(&p).unwrap();
        // A cut file fails cleanly.
        let full = std::fs::read(&p).unwrap();
        std::fs::write(&p, &full[..full.len() / 2]).unwrap();
        assert!(load(&p).is_err());
        std::fs::remove_file(&p).unwrap();
        assert_eq!(l.source, "disk1");
        assert_eq!(l.part, s.part);
        assert_eq!(l.boot, s.boot);
        let a = l.catalog.find(41).unwrap();
        assert_eq!(l.catalog.path(a), r"\Users\Me\Old\a.jpg");
        assert_eq!(l.catalog.entries[a as usize].state, State::Partial);
        assert_eq!(l.catalog.runs_of(a), &[Run { lcn: 100, len: 2 }]);
        assert_eq!(l.catalog.fs.mft, vec![Run { lcn: 4, len: 16 }]);
        assert!(load(Path::new("Z:/does/not/exist.frs")).is_err());
    }
}
