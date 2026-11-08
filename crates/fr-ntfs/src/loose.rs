//! FILE records outside recognized MFT extents, collected during the full-range pass.
use crate::scan::{FIRST_USER, K_DATA, ROOT, ScanOptions, Worker, append_streams, build_data, parse};
use fr_core::{Catalog, Data, Entry, FsInfo, NONE, State, ef};
use std::collections::BTreeMap;

pub struct Span {
    pub start: u64,
    pub end: u64,
    record_size: u32,
    phase: u32,
}

impl Span {
    pub fn contains(&self, at: u64, len: u64) -> bool {
        len == self.record_size as u64
            && self.start <= at
            && at + len <= self.end
            && (at - self.start + self.phase as u64).is_multiple_of(len)
    }
}

pub fn mft_spans(cat: &Catalog) -> Vec<Span> {
    let mut logical = 0;
    cat.fs
        .mft
        .iter()
        .map(|r| {
            let len = r.len * cat.unit as u64;
            let span = Span {
                start: r.lcn * cat.unit as u64,
                end: (r.lcn * cat.unit as u64) + len,
                record_size: cat.fs.record_size,
                phase: (logical % cat.fs.record_size as u64) as u32,
            };
            logical += len;
            span
        })
        .collect()
}

pub struct Records {
    groups: BTreeMap<(i128, u32), (Worker, Vec<u64>)>,
    pub rejected_groups: u64,
    options: ScanOptions,
}

impl Records {
    pub fn new(streams: bool) -> Self {
        Self {
            groups: BTreeMap::new(),
            rejected_groups: 0,
            options: ScanOptions {
                existing: true,
                streams,
                threads: 1,
                owners: false,
                traces: false,
                deep: false,
                sweep_progress: None,
                progress: None,
            },
        }
    }
    pub fn observe(&mut self, at: u64, bytes: &[u8]) {
        if bytes.len() < 512 || bytes.len() > 65536 || !bytes.len().is_power_of_two() {
            return;
        }
        let id = u32::from_le_bytes(bytes[44..48].try_into().unwrap()) as u64;
        let rs = bytes.len() as u32;
        let origin = at as i128 - id as i128 * rs as i128;
        let key = (origin, rs);
        if !self.groups.contains_key(&key) {
            let mut record = bytes.to_vec();
            if &record[..4] != b"FILE" || !crate::record::apply_fixup(&mut record) {
                return;
            }
        }
        // A corrupt image must not allocate a worker for every random FILE header.
        if self.groups.len() >= 1024 && !self.groups.contains_key(&key) {
            self.rejected_groups += 1;
            return;
        }
        let (worker, positions) = self.groups.entry(key).or_default();
        let mut record = bytes.to_vec();
        let before = worker.raws.len();
        parse(worker, id, &mut record, &self.options);
        if worker.raws.len() > before {
            positions.push(at);
        }
    }

    /// Recover resident bytes exactly; non-resident runs use the selected NTFS geometry
    /// as a hypothesis and are explicitly marked undetermined.
    pub fn finish(
        self,
        src: &fr_core::Source,
        cat: &mut Catalog,
        covered: &[Span],
        next: &mut u64,
        geometry_known: bool,
    ) -> Result<u64, fr_core::error::Error> {
        let mut found = 0;
        for ((origin, rs), (mut worker, positions)) in self.groups {
            src.checkpoint()?;
            worker.segs.sort_unstable_by_key(|s| (s.base, s.kind, s.lowest_vcn, !s.own));
            let data_segs: Vec<_> = worker.segs.iter().filter(|s| s.kind == K_DATA).copied().collect();
            let cs = cat.fs.cluster_size as u64;
            let fs = FsInfo {
                kind: fr_core::FsKind::Ntfs,
                offset: cat.fs.offset,
                size: cat.fs.size,
                cluster_size: cs as u32,
                record_size: rs,
                mft: vec![],
                serial: 0,
                root: 5,
                data_offset: 0,
            };
            let mut entries = Vec::new();
            let mut runs = Vec::new();
            let mut parents = Vec::new();
            let addresses: BTreeMap<_, _> = worker.raws.iter().zip(&positions).map(|(raw, &at)| (raw.id, at)).collect();
            for (k, (raw, at)) in worker.raws.iter().zip(positions).enumerate() {
                if k.is_multiple_of(1024) {
                    src.checkpoint()?;
                }
                if covered.iter().any(|s| s.contains(at, rs as u64)) {
                    continue;
                }
                let mut e = Entry {
                    id: raw.id,
                    seq: raw.seq,
                    flags: raw.flags | ef::DELETED | ef::HISTORICAL,
                    attrs: raw.attrs,
                    parent: NONE,
                    name_off: raw.name_off,
                    name_len: raw.name_len,
                    created: raw.times[0],
                    modified: raw.times[1],
                    accessed: raw.times[2],
                    changed: raw.times[3],
                    ..Default::default()
                };
                if raw.dir {
                    e.flags |= ef::DIR;
                } else {
                    build_data(&mut e, raw, &data_segs, &worker.runs, &mut runs, cs, cat.fs.size / cs);
                }
                if raw.id < FIRST_USER && raw.id != ROOT {
                    e.flags |= ef::SYSTEM;
                }
                if let Data::Resident { off, len } = e.data {
                    e.data = Data::Inline { at, off, len, record_size: rs };
                }
                if !geometry_known && matches!(e.data, Data::Runs { .. }) {
                    e.data = Data::None;
                }
                e.state = if e.data == Data::None && e.size != 0 { State::NoData } else { State::Unknown };
                parents.push(raw.parent);
                entries.push(e);
            }
            if entries.is_empty() {
                continue;
            }
            let ids: BTreeMap<_, _> = entries.iter().enumerate().map(|(i, e)| (e.id, i as u32)).collect();
            for (i, p) in parents.into_iter().enumerate() {
                let no = p & 0xffff_ffff_ffff;
                let seq = (p >> 48) as u16;
                if let Some(&parent) = ids.get(&no) {
                    let e = entries[parent as usize];
                    if parent != i as u32 && e.is_dir() && (e.seq == seq || e.seq == seq.wrapping_add(1)) {
                        entries[i].parent = parent;
                    }
                }
            }
            let mut old = Catalog::new(fs, entries, worker.names, runs);
            append_streams(&mut old, &worker.ads, &worker.runs, None, &|id| ids.get(&id).copied(), cs);
            for e in &mut old.entries {
                if let Data::Resident { off, len } = e.data {
                    let stream = (e.id - fr_core::model::STREAM_ID_BASE) as usize;
                    e.data = worker.ads.get(stream).and_then(|a| addresses.get(&a.base)).map_or(Data::None, |&at| Data::Inline {
                        at,
                        off,
                        len,
                        record_size: rs,
                    });
                }
                if !geometry_known && matches!(e.data, Data::Runs { .. }) {
                    e.data = Data::None;
                }
            }
            if !geometry_known {
                old.runs.clear();
            }
            if let Ok(n) = cat.import_volume(old, &format!("MFT {origin}"), *next, true) {
                *next += n + 1;
                found += n;
            }
        }
        Ok(found)
    }
}
