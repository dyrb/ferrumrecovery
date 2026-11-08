#![allow(dead_code)]
use fr_core::{Catalog, Source};

pub fn put16(b: &mut [u8], at: usize, n: u16) {
    b[at..at + 2].copy_from_slice(&n.to_le_bytes());
}
pub fn put32(b: &mut [u8], at: usize, n: u32) {
    b[at..at + 4].copy_from_slice(&n.to_le_bytes());
}

pub fn fat12(spc: u8) -> Vec<u8> {
    let mut b = vec![0u8; 2880 * 512];
    b[..3].copy_from_slice(&[0xeb, 0x3c, 0x90]);
    b[3..11].copy_from_slice(b"MSDOS5.0");
    put16(&mut b, 11, 512);
    b[13] = spc;
    put16(&mut b, 14, 1);
    b[16] = 2;
    put16(&mut b, 17, 224);
    put16(&mut b, 19, 2880);
    b[21] = 0xf0;
    put16(&mut b, 22, 9);
    put16(&mut b, 24, 18);
    put16(&mut b, 26, 2);
    b[38] = 0x29;
    b[54..62].copy_from_slice(b"FAT12   ");
    b[510..512].copy_from_slice(&[0x55, 0xaa]);
    for off in [512, 5120] {
        b[off..off + 3].copy_from_slice(&[0xf0, 0xff, 0xff]);
    }
    let root = 19 * 512;
    b[root..root + 11].copy_from_slice(b"\xe5OTE    TXT");
    b[root + 11] = 0x20;
    put16(&mut b, root + 26, 2);
    put32(&mut b, root + 28, 1024);
    b[33 * 512..35 * 512].fill(b'R');
    b
}

pub struct Image {
    pub dir: std::path::PathBuf,
    pub path: std::path::PathBuf,
    pub src: Source,
}
impl Image {
    pub fn new(name: &str, bytes: &[u8]) -> Self {
        let dir = std::env::temp_dir().join(format!("fr-full-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("disk.img");
        std::fs::write(&path, bytes).unwrap();
        let src = Source::open(path.to_str().unwrap()).unwrap();
        Self { dir, path, src }
    }
    pub fn roundtrip(&self, cat: &Catalog, part: &fr_core::Part) -> Catalog {
        let path = self.dir.join("scan.frs");
        fr_core::session::save_catalog(&path, self.path.to_str().unwrap(), part, &self.src.read_bytes(part.offset, 512).unwrap(), &[], cat)
            .unwrap();
        fr_core::session::load(&path).unwrap().catalog
    }
    pub fn close(self) {
        drop(self.src);
        std::fs::remove_dir_all(self.dir).unwrap();
    }
}

pub fn contents(src: &Source, cat: &Catalog, i: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    let stats = fr_core::data::Reader::new(src, cat)
        .unwrap()
        .read(i, &mut |b| {
            bytes.extend_from_slice(b);
            Ok(())
        })
        .unwrap();
    assert_eq!(stats.bad + stats.missing + stats.corrupt_units as u64, 0);
    bytes
}

pub fn wave() -> Vec<u8> {
    let mut b = b"RIFF\x44\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x40\x1f\0\0\x40\x1f\0\0\x01\0\x08\0data\x20\0\0\0".to_vec();
    b.extend_from_slice(&[128; 32]);
    b
}

/// A 2 MiB NTFS layout with 1 KiB clusters and 4 KiB FILE records. Record 24
/// crosses an MFT extent boundary; record 25 has non-resident data.
pub fn ntfs() -> Vec<u8> {
    use fr_ntfs::{
        record::{attr, ns, synth::RecordBuilder},
        runlist::Extent,
    };
    let mut b = vec![0; 2 << 20];
    b[..3].copy_from_slice(&[0xeb, 0x52, 0x90]);
    b[3..11].copy_from_slice(b"NTFS    ");
    put16(&mut b, 11, 512);
    b[13] = 2;
    b[40..48].copy_from_slice(&4096u64.to_le_bytes());
    b[48..56].copy_from_slice(&32u64.to_le_bytes());
    b[56..64].copy_from_slice(&4u64.to_le_bytes());
    b[64] = (-12i8) as u8;
    b[68] = (-12i8) as u8;
    b[72..80].copy_from_slice(&0x183590ac56u64.to_le_bytes());
    b[510..512].copy_from_slice(&[0x55, 0xaa]);
    let runs = [Extent { vcn: 0, len: 97, lcn: Some(32) }, Extent { vcn: 97, len: 31, lcn: Some(512) }];
    let mft: Vec<_> = runs.iter().map(|r| fr_core::Run { lcn: r.lcn.unwrap(), len: r.len }).collect();
    let records = [
        (
            0,
            RecordBuilder::new(4096).file_name(5, 1, "$MFT", ns::WIN32).non_resident(
                attr::DATA,
                "",
                0,
                0,
                128 << 10,
                128 << 10,
                None,
                &runs,
            ),
        ),
        (5, RecordBuilder::new(4096).dir().file_name(5, 1, ".", ns::WIN32)),
        (6, RecordBuilder::new(4096).file_name(5, 1, "$Bitmap", ns::WIN32).resident(attr::DATA, "", &[0; 256])),
        (
            24,
            RecordBuilder::new(4096).unused().file_name(5, 1, "small.txt", ns::WIN32).resident(
                attr::DATA,
                "",
                b"fragmented MFT resident bytes",
            ),
        ),
        (
            25,
            RecordBuilder::new(4096).unused().file_name(5, 1, "large.bin", ns::WIN32).non_resident(
                attr::DATA,
                "",
                0,
                0,
                2048,
                2000,
                None,
                &[Extent { vcn: 0, len: 2, lcn: Some(800) }],
            ),
        ),
    ];
    for (id, builder) in records {
        let mut rec = builder.build(0x21);
        put32(&mut rec, 44, id);
        let mut pos = 0;
        for (at, len) in fr_core::data::map_bytes(&mft, 1024, id as u64 * 4096, 4096).unwrap() {
            b[at as usize..(at + len) as usize].copy_from_slice(&rec[pos..pos + len as usize]);
            pos += len as usize;
        }
    }
    b[800 * 1024..800 * 1024 + 2000].fill(0x5a);
    let end = b.len() - 512;
    let boot = b[..512].to_vec();
    b[end..].copy_from_slice(&boot);
    b
}
