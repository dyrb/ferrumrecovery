//! A shared sector address space for current and historical file systems.
use crate::{Catalog, Data, Entry, Run, State, ef};

impl Catalog {
    pub fn sort_ids(&mut self) {
        let mut order: Vec<_> = (0..self.entries.len()).collect();
        order.sort_unstable_by_key(|&i| self.entries[i].id);
        let mut mapped = vec![0; order.len()];
        for (new, &old) in order.iter().enumerate() {
            mapped[old] = new as u32;
        }
        let mut entries: Vec<_> = order
            .into_iter()
            .map(|i| {
                let mut e = self.entries[i];
                if e.parent != crate::NONE {
                    e.parent = mapped.get(e.parent as usize).copied().unwrap_or(crate::NONE);
                }
                e
            })
            .collect();
        for i in 0..entries.len() {
            let parent = entries[i].parent;
            if parent != crate::NONE && !entries[parent as usize].is_dir() {
                entries[i].parent = crate::NONE;
                entries[i].flags |= ef::ORPHAN;
            }
        }
        let mut colors = vec![0u8; entries.len()];
        for i in 0..entries.len() {
            let mut at = i;
            while colors[at] == 0 {
                colors[at] = 1;
                let parent = entries[at].parent;
                if parent == crate::NONE {
                    break;
                }
                if colors[parent as usize] == 1 {
                    entries[at].parent = crate::NONE;
                    entries[at].flags |= ef::ORPHAN;
                    break;
                }
                at = parent as usize;
            }
            at = i;
            while colors[at] == 1 {
                colors[at] = 2;
                let parent = entries[at].parent;
                if parent == crate::NONE {
                    break;
                }
                at = parent as usize;
            }
        }
        self.entries = entries;
        self.alias = std::mem::take(&mut self.alias).into_iter().map(|(i, name)| (mapped[i as usize], name)).collect();
    }
    /// Normalize physical runs without changing the displayed file-system geometry.
    pub fn sector_units(&mut self) {
        if self.unit == 512 && self.fs.data_offset == 0 {
            return;
        }
        let factor = self.unit as u64 / 512;
        let base = self.fs.data_offset / 512;
        for r in &mut self.runs {
            if r.is_real() {
                r.lcn = base + r.lcn * factor;
            }
            r.len *= factor;
        }
        for r in &mut self.fs.mft {
            r.lcn *= factor;
            r.len *= factor;
        }
        for e in &mut self.entries {
            if e.cu != 0 {
                e.cu += factor.trailing_zeros() as u8;
            }
            e.lost *= factor;
        }
        self.unit = 512;
        self.fs.data_offset = 0;
    }

    /// Import one bounded historical volume, preserving its tree and data geometry.
    /// Old allocation bits describe that old volume, not the current disk: states are unknown.
    pub fn import_volume(&mut self, mut old: Catalog, label: &str, id_start: u64, historical: bool) -> Result<u64, crate::error::Error> {
        let delta = old
            .fs
            .offset
            .checked_sub(self.fs.offset)
            .filter(|d| d.is_multiple_of(512) && d.checked_add(old.fs.size).is_some_and(|end| end <= self.fs.size))
            .ok_or_else(|| crate::error::Error::new("scan range is outside the source"))?;
        for r in &old.runs {
            if r.is_real()
                && r.lcn
                    .checked_add(r.len)
                    .and_then(|end| end.checked_mul(old.unit as u64))
                    .and_then(|end| end.checked_add(old.fs.data_offset))
                    .is_none_or(|end| end > old.fs.size)
            {
                return Err(crate::error::Error::new("scan range is outside the source"));
            }
        }
        self.sector_units();
        old.sector_units();
        let run_base = self.runs.len() as u32;
        for r in &old.runs {
            self.runs.push(Run { lcn: if r.is_real() { r.lcn + delta / 512 } else { r.lcn }, len: r.len });
        }
        let first = self.entries.len() as u32;
        let root = old.find(old.fs.root);
        let parent = self.find(self.fs.root).unwrap_or(crate::NONE);
        let (no, nl) = self.add_name(label);
        let historical_flags = if historical { ef::FOUND | ef::HISTORICAL } else { 0 };
        self.entries.push(Entry {
            id: id_start,
            parent,
            flags: ef::DIR | historical_flags,
            name_off: no,
            name_len: nl,
            ..Default::default()
        });
        let mut next = first + 1;
        let mapped: Vec<_> = (0..old.entries.len())
            .map(|i| {
                if Some(i as u32) == root {
                    first
                } else {
                    let index = next;
                    next += 1;
                    index
                }
            })
            .collect();
        for (i, mut e) in old.entries.iter().copied().enumerate() {
            if Some(i as u32) == root {
                continue;
            }
            e.id = id_start + (mapped[i] - first) as u64;
            e.flags |= historical_flags;
            if historical {
                e.flags &= !(ef::RECYCLED | ef::DELETED);
            }
            e.parent = if e.parent == crate::NONE { first } else { mapped.get(e.parent as usize).copied().unwrap_or(first) };
            let (off, len) = self.add_name(old.name(i as u32));
            e.name_off = off;
            e.name_len = len;
            e.data = match e.data {
                Data::Runs { start, count } => Data::Runs { start: run_base + start, count },
                Data::Inline { at, off, len, record_size } => Data::Inline { at: delta + at, off, len, record_size },
                Data::Resident { off, len } => {
                    let pieces = crate::data::map_bytes(
                        &old.fs.mft,
                        512,
                        old.record_of(i as u32) * old.fs.record_size as u64,
                        old.fs.record_size as u64,
                    );
                    match pieces {
                        Some(p) if p.len() == 1 => Data::Inline { at: delta + p[0].0, off, len, record_size: old.fs.record_size },
                        Some(p) => {
                            let start = self.runs.len() as u32;
                            for (at, size) in &p {
                                self.runs.push(Run { lcn: (delta + at) / 512, len: size / 512 });
                            }
                            Data::RecordRuns { start, count: p.len() as u32, record_size: old.fs.record_size, off, len }
                        }
                        None => Data::None,
                    }
                }
                Data::None => Data::None,
                Data::RecordRuns { start, count, record_size, off, len } => {
                    Data::RecordRuns { start: run_base + start, count, record_size, off, len }
                }
            };
            if historical && !e.is_dir() {
                e.state = if e.data == Data::None && e.size != 0 {
                    State::NoData
                } else if e.flags & ef::ENCRYPTED != 0 {
                    State::Encrypted
                } else {
                    State::Unknown
                };
            }
            self.entries.push(e);
        }
        for (i, name) in old.alias {
            if let Some(&index) = mapped.get(i as usize) {
                self.alias.insert(index, format!("\\{label}\\{}", name.trim_start_matches('\\')));
            }
        }
        Ok((self.entries.len() as u32 - first - 1) as u64)
    }
}
