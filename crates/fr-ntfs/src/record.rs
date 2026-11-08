//! `FILE` record (FILE_RECORD_SEGMENT_HEADER) helpers: update sequence fixup, attribute
//! decoding, attribute lists, name conversion, and a builder of synthetic records for tests.

use crate::bytes::*;
use crate::error::{Error, Result};
use crate::runlist::{self, Extent};

/// Attribute type codes.
pub mod attr {
    pub const STANDARD_INFORMATION: u32 = 0x10;
    pub const ATTRIBUTE_LIST: u32 = 0x20;
    pub const FILE_NAME: u32 = 0x30;
    pub const DATA: u32 = 0x80;
    pub const INDEX_ROOT: u32 = 0x90;
    pub const INDEX_ALLOCATION: u32 = 0xA0;
    pub const BITMAP: u32 = 0xB0;
    pub const END: u32 = 0xFFFF_FFFF;
}

pub const REC_IN_USE: u16 = 0x0001;
pub const REC_DIRECTORY: u16 = 0x0002;

pub const AF_COMPRESSED: u16 = 0x0001;
pub const AF_ENCRYPTED: u16 = 0x4000;
pub const AF_SPARSE: u16 = 0x8000;

/// `$FILE_NAME` namespaces.
pub mod ns {
    pub const POSIX: u8 = 0;
    pub const WIN32: u8 = 1;
    pub const DOS: u8 = 2;
    pub const WIN32_AND_DOS: u8 = 3;
}

/// Apply the Update Sequence Array fixup in place. Returns `false` for a torn or
/// malformed record. The USA always protects 512-byte strides, independent of the
/// physical sector size.
#[inline]
pub fn apply_fixup(rec: &mut [u8]) -> bool {
    let usa_off = u16le(rec, 4) as usize;
    let usa_cnt = u16le(rec, 6) as usize;
    if usa_cnt < 2 || usa_off + usa_cnt * 2 > rec.len() || (usa_cnt - 1) * 512 > rec.len() {
        return false;
    }
    let usn = [rec[usa_off], rec[usa_off + 1]];
    for i in 1..usa_cnt {
        let end = i * 512 - 2;
        if rec[end] != usn[0] || rec[end + 1] != usn[1] {
            return false;
        }
        rec[end] = rec[usa_off + 2 * i];
        rec[end + 1] = rec[usa_off + 2 * i + 1];
    }
    true
}

/// UTF-16LE -> UTF-8 with an ASCII fast path. Unpaired surrogates (legal in NTFS names)
/// become U+FFFD.
#[inline]
pub fn push_utf16_as_utf8(out: &mut Vec<u8>, raw: &[u8]) {
    let (pairs, _) = raw.as_chunks::<2>();
    let ascii = pairs.iter().all(|c| c[1] == 0 && c[0] < 0x80);
    if ascii {
        out.reserve(pairs.len());
        out.extend(pairs.iter().map(|c| c[0]));
        return;
    }
    let units = pairs.iter().map(|&c| u16::from_le_bytes(c));
    let mut buf = [0u8; 4];
    for ch in char::decode_utf16(units) {
        let ch = ch.unwrap_or(char::REPLACEMENT_CHARACTER);
        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RecordHeader {
    pub seq: u16,
    pub link_count: u16,
    pub flags: u16,
    pub base_ref: u64,
    pub bytes_in_use: u32,
}

impl RecordHeader {
    pub fn in_use(&self) -> bool {
        self.flags & REC_IN_USE != 0
    }
}

#[derive(Debug, Clone)]
pub struct NonResident {
    pub lowest_vcn: u64,
    pub highest_vcn: u64,
    pub allocated: u64,
    pub data_size: u64,
    pub initialized: u64,
    pub extents: Vec<Extent>,
}

#[derive(Debug, Clone)]
pub enum AttrValue {
    Resident(Vec<u8>),
    NonResident(NonResident),
}

#[derive(Debug, Clone)]
pub struct Attribute {
    pub ty: u32,
    pub name: String,
    pub flags: u16,
    pub value: AttrValue,
}

/// Validate signature, apply fixup and decode the header.
pub fn fixup_record(rec: &mut [u8]) -> Result<RecordHeader> {
    if rec.len() < 0x30 || &rec[0..4] != b"FILE" {
        return Err(Error::BadMftRecord("signature"));
    }
    if !apply_fixup(rec) {
        return Err(Error::BadMftRecord("update sequence"));
    }
    Ok(RecordHeader {
        seq: u16le(rec, 0x10),
        link_count: u16le(rec, 0x12),
        flags: u16le(rec, 0x16),
        base_ref: u64le(rec, 0x20),
        bytes_in_use: u32le(rec, 0x18),
    })
}

/// Decode all attributes of a record that has already been fixed up.
pub fn attributes(rec: &[u8]) -> Result<Vec<Attribute>> {
    let used = (u32le(rec, 0x18) as usize).min(rec.len());
    let mut off = u16le(rec, 0x14) as usize;
    let mut out = Vec::new();
    loop {
        if off + 4 > used {
            return Err(Error::BadMftRecord("attribute overrun"));
        }
        let ty = u32le(rec, off);
        if ty == attr::END {
            return Ok(out);
        }
        if off + 8 > used {
            return Err(Error::BadMftRecord("attribute overrun"));
        }
        let len = u32le(rec, off + 4) as usize;
        if len < 0x18 || off + len > used {
            return Err(Error::BadMftRecord("attribute length"));
        }
        let a = &rec[off..off + len];
        off += len;
        let nlen = a[9] as usize;
        let noff = u16le(a, 0x0A) as usize;
        if noff + 2 * nlen > len {
            return Err(Error::BadMftRecord("attribute name"));
        }
        let mut nb = Vec::new();
        push_utf16_as_utf8(&mut nb, &a[noff..noff + 2 * nlen]);
        let name = String::from_utf8(nb).unwrap_or_default();
        let flags = u16le(a, 0x0C);
        let value = if a[8] == 0 {
            let vlen = u32le(a, 0x10) as usize;
            let voff = u16le(a, 0x14) as usize;
            if voff + vlen > len {
                return Err(Error::BadMftRecord("resident value"));
            }
            AttrValue::Resident(a[voff..voff + vlen].to_vec())
        } else {
            if len < 0x40 {
                return Err(Error::BadMftRecord("non-resident header"));
            }
            let lowest_vcn = u64le(a, 0x10);
            let mp = u16le(a, 0x20) as usize;
            if mp > len {
                return Err(Error::BadMftRecord("mapping pairs offset"));
            }
            let mut extents = Vec::new();
            runlist::decode(&a[mp..], lowest_vcn, &mut extents)?;
            AttrValue::NonResident(NonResident {
                lowest_vcn,
                highest_vcn: u64le(a, 0x18),
                allocated: u64le(a, 0x28),
                data_size: u64le(a, 0x30),
                initialized: u64le(a, 0x38),
                extents,
            })
        };
        out.push(Attribute { ty, name, flags, value });
    }
}

/// One `$ATTRIBUTE_LIST` entry.
#[derive(Debug, Clone, Copy)]
pub struct AttrListEntry {
    pub ty: u32,
    pub lowest_vcn: u64,
    pub segment: u64,
    pub segment_seq: u16,
    pub name_len: u8,
}

pub fn parse_attribute_list(v: &[u8]) -> Result<Vec<AttrListEntry>> {
    let mut out = Vec::new();
    let mut off = 0;
    while off + 0x1A <= v.len() {
        let ty = u32le(v, off);
        let rlen = u16le(v, off + 4) as usize;
        if rlen < 0x1A || off + rlen > v.len() {
            return Err(Error::BadMftRecord("attribute list entry"));
        }
        let sref = u64le(v, off + 0x10);
        out.push(AttrListEntry {
            ty,
            name_len: v[off + 6],
            lowest_vcn: u64le(v, off + 8),
            segment: sref & 0xFFFF_FFFF_FFFF,
            segment_seq: (sref >> 48) as u16,
        });
        off += rlen;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// Synthetic record builder (tests, fuzzing, benchmarks).
// ---------------------------------------------------------------------------------------

/// Builds valid, fixup-protected FILE records in memory.
pub mod synth {
    use super::*;

    pub struct RecordBuilder {
        size: usize,
        seq: u16,
        flags: u16,
        base: u64,
        attrs: Vec<Vec<u8>>,
    }

    fn pad8(v: &mut Vec<u8>) {
        while !v.len().is_multiple_of(8) {
            v.push(0);
        }
    }

    fn utf16(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
    }

    impl RecordBuilder {
        pub fn new(size: usize) -> Self {
            RecordBuilder { size, seq: 1, flags: REC_IN_USE, base: 0, attrs: Vec::new() }
        }
        pub fn seq(mut self, s: u16) -> Self {
            self.seq = s;
            self
        }
        pub fn dir(mut self) -> Self {
            self.flags |= REC_DIRECTORY;
            self
        }
        pub fn unused(mut self) -> Self {
            self.flags &= !REC_IN_USE;
            self
        }
        pub fn extension_of(mut self, base: u32, base_seq: u16) -> Self {
            self.base = base as u64 | (base_seq as u64) << 48;
            self
        }
        pub fn std_info(self, mtime: u64, attrs: u32) -> Self {
            let mut v = vec![0u8; 0x48];
            v[0x08..0x10].copy_from_slice(&mtime.to_le_bytes());
            v[0x20..0x24].copy_from_slice(&attrs.to_le_bytes());
            self.resident(attr::STANDARD_INFORMATION, "", &v)
        }
        pub fn file_name(self, parent: u32, parent_seq: u16, name: &str, namespace: u8) -> Self {
            let n = utf16(name);
            let mut v = vec![0u8; 0x42];
            let pref = parent as u64 | (parent_seq as u64) << 48;
            v[0..8].copy_from_slice(&pref.to_le_bytes());
            v[0x40] = (n.len() / 2) as u8;
            v[0x41] = namespace;
            v.extend_from_slice(&n);
            self.resident(attr::FILE_NAME, "", &v)
        }
        pub fn resident(mut self, ty: u32, name: &str, value: &[u8]) -> Self {
            let n = utf16(name);
            let mut a = vec![0u8; 0x18];
            a.extend_from_slice(&n);
            pad8(&mut a);
            let voff = a.len();
            a.extend_from_slice(value);
            pad8(&mut a);
            let len = a.len() as u32;
            a[0..4].copy_from_slice(&ty.to_le_bytes());
            a[4..8].copy_from_slice(&len.to_le_bytes());
            a[8] = 0;
            a[9] = (n.len() / 2) as u8;
            a[0x0A..0x0C].copy_from_slice(&0x18u16.to_le_bytes());
            a[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
            a[0x14..0x16].copy_from_slice(&(voff as u16).to_le_bytes());
            self.attrs.push(a);
            self
        }
        #[allow(clippy::too_many_arguments)]
        pub fn non_resident(
            mut self,
            ty: u32,
            name: &str,
            aflags: u16,
            lowest_vcn: u64,
            allocated: u64,
            data_size: u64,
            compressed: Option<u64>,
            runs: &[Extent],
        ) -> Self {
            let n = utf16(name);
            let hdr = if compressed.is_some() { 0x48 } else { 0x40 };
            let mut a = vec![0u8; hdr];
            a.extend_from_slice(&n);
            pad8(&mut a);
            let mp = a.len();
            a.extend_from_slice(&runlist::encode(runs));
            pad8(&mut a);
            let len = a.len() as u32;
            let highest = runs.last().map(|e| e.vcn + e.len - 1).unwrap_or(0);
            a[0..4].copy_from_slice(&ty.to_le_bytes());
            a[4..8].copy_from_slice(&len.to_le_bytes());
            a[8] = 1;
            a[9] = (n.len() / 2) as u8;
            a[0x0A..0x0C].copy_from_slice(&(hdr as u16).to_le_bytes());
            a[0x0C..0x0E].copy_from_slice(&aflags.to_le_bytes());
            a[0x10..0x18].copy_from_slice(&lowest_vcn.to_le_bytes());
            a[0x18..0x20].copy_from_slice(&highest.to_le_bytes());
            a[0x20..0x22].copy_from_slice(&(mp as u16).to_le_bytes());
            a[0x28..0x30].copy_from_slice(&allocated.to_le_bytes());
            a[0x30..0x38].copy_from_slice(&data_size.to_le_bytes());
            a[0x38..0x40].copy_from_slice(&data_size.to_le_bytes());
            if let Some(c) = compressed {
                a[0x40..0x48].copy_from_slice(&c.to_le_bytes());
            }
            self.attrs.push(a);
            self
        }

        /// Serialize and protect with the update sequence `usn`.
        pub fn build(&self, usn: u16) -> Vec<u8> {
            let mut r = vec![0u8; self.size];
            let usa_cnt = self.size / 512 + 1;
            let usa_off = 0x30usize;
            let first = (usa_off + 2 * usa_cnt).div_ceil(8) * 8;
            r[0..4].copy_from_slice(b"FILE");
            r[4..6].copy_from_slice(&(usa_off as u16).to_le_bytes());
            r[6..8].copy_from_slice(&(usa_cnt as u16).to_le_bytes());
            r[0x10..0x12].copy_from_slice(&self.seq.to_le_bytes());
            r[0x12..0x14].copy_from_slice(&1u16.to_le_bytes());
            r[0x14..0x16].copy_from_slice(&(first as u16).to_le_bytes());
            r[0x16..0x18].copy_from_slice(&self.flags.to_le_bytes());
            r[0x1C..0x20].copy_from_slice(&(self.size as u32).to_le_bytes());
            r[0x20..0x28].copy_from_slice(&self.base.to_le_bytes());
            let mut off = first;
            for a in &self.attrs {
                r[off..off + a.len()].copy_from_slice(a);
                off += a.len();
            }
            r[off..off + 4].copy_from_slice(&attr::END.to_le_bytes());
            off += 8;
            assert!(off <= self.size - 2, "attributes overflow the record");
            r[0x18..0x1C].copy_from_slice(&(off as u32).to_le_bytes());
            // protect
            r[usa_off..usa_off + 2].copy_from_slice(&usn.to_le_bytes());
            for i in 1..usa_cnt {
                let end = i * 512 - 2;
                let (s0, s1) = (r[end], r[end + 1]);
                r[usa_off + 2 * i] = s0;
                r[usa_off + 2 * i + 1] = s1;
                r[end..end + 2].copy_from_slice(&usn.to_le_bytes());
            }
            r
        }
    }
}

#[cfg(test)]
mod tests {
    use super::synth::RecordBuilder;
    use super::*;

    #[test]
    fn cold_path_decodes_attributes() {
        let runs = [Extent { vcn: 0, len: 16, lcn: Some(786432) }];
        let mut raw = RecordBuilder::new(1024)
            .std_info(0, 6)
            .file_name(5, 5, "$MFT", ns::WIN32_AND_DOS)
            .non_resident(attr::DATA, "", 0, 0, 65536, 65536, None, &runs)
            .build(1);
        let h = fixup_record(&mut raw).unwrap();
        assert!(h.in_use());
        let attrs = attributes(&raw).unwrap();
        let data = attrs.iter().find(|a| a.ty == attr::DATA).unwrap();
        match &data.value {
            AttrValue::NonResident(nr) => {
                assert_eq!(nr.extents, runs);
                assert_eq!(nr.data_size, 65536);
            }
            _ => panic!("expected non-resident"),
        }
    }

    #[test]
    fn torn_record_is_rejected() {
        let mut raw = RecordBuilder::new(1024).std_info(0, 0).build(0xABCD);
        raw[1022] ^= 0xFF;
        assert!(!apply_fixup(&mut raw));
    }

    #[test]
    fn non_ascii_names() {
        let mut out = Vec::new();
        let raw: Vec<u8> = "Документы 📁".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        push_utf16_as_utf8(&mut out, &raw);
        assert_eq!(String::from_utf8(out).unwrap(), "Документы 📁");
    }
}
