//! Structural carving of legacy containers and images, including malformed boundaries.
use fr_core::{
    Source,
    carve::{self, Fmt},
};

fn put16(b: &mut [u8], at: usize, n: u16) {
    b[at..at + 2].copy_from_slice(&n.to_le_bytes());
}
fn put32(b: &mut [u8], at: usize, n: u32) {
    b[at..at + 4].copy_from_slice(&n.to_le_bytes());
}

fn pcx(planes: u8) -> Vec<u8> {
    let mut b = vec![0; 128];
    b[..4].copy_from_slice(&[10, 5, 1, 8]);
    put16(&mut b, 8, 1);
    put16(&mut b, 10, 1);
    b[65] = planes;
    put16(&mut b, 66, 2);
    put16(&mut b, 68, 1);
    for _ in 0..2 {
        b.extend_from_slice(&[0xc0 | (2 * planes), 127]);
    }
    if planes == 1 {
        b.push(12);
        b.extend_from_slice(&[42; 768]);
    }
    b
}

fn cab() -> Vec<u8> {
    let payload = b"contents of a cabinet member";
    let name = b"member.txt\0";
    let data = 44 + 16 + name.len();
    let mut b = vec![0; data + 8 + payload.len()];
    let size = b.len() as u32;
    b[..4].copy_from_slice(b"MSCF");
    put32(&mut b, 8, size);
    put32(&mut b, 16, 44);
    b[24..26].copy_from_slice(&[3, 1]);
    put16(&mut b, 26, 1);
    put16(&mut b, 28, 1);
    put32(&mut b, 36, data as u32);
    put16(&mut b, 40, 1);
    put32(&mut b, 44, payload.len() as u32);
    b[60..data].copy_from_slice(name);
    put16(&mut b, data + 4, payload.len() as u16);
    put16(&mut b, data + 6, payload.len() as u16);
    b[data + 8..].copy_from_slice(payload);
    b
}

fn tga(rle: bool) -> Vec<u8> {
    let mut b = vec![0; 18];
    b[2] = if rle { 10 } else { 2 };
    put16(&mut b, 12, 2);
    put16(&mut b, 14, 2);
    b[16] = 24;
    if rle {
        b.extend_from_slice(&[0x83, 10, 20, 30]);
    } else {
        b.extend_from_slice(&[10, 20, 30].repeat(4));
    }
    b.extend_from_slice(&[0; 8]);
    b.extend_from_slice(b"TRUEVISION-XFILE.\0");
    b
}

fn dcx() -> Vec<u8> {
    let a = pcx(1);
    let c = pcx(3);
    let mut b = vec![0; 4100];
    put32(&mut b, 0, 0x3ade68b1);
    put32(&mut b, 4, 4100);
    put32(&mut b, 8, (4100 + a.len()) as u32);
    b.extend_from_slice(&a);
    b.extend_from_slice(&c);
    b
}

fn icon(cursor: bool) -> Vec<u8> {
    let mut b = vec![0; 22 + 48];
    put16(&mut b, 2, if cursor { 2 } else { 1 });
    put16(&mut b, 4, 1);
    b[6..8].copy_from_slice(&[1, 1]);
    b[9] = 0x87;
    put32(&mut b, 14, 48);
    put32(&mut b, 18, 22);
    put32(&mut b, 22, 40);
    put32(&mut b, 26, 1);
    put32(&mut b, 30, 2);
    put16(&mut b, 34, 1);
    put16(&mut b, 36, 32);
    b
}

#[test]
fn complete_legacy_files_have_exact_ends_and_identical_recovered_bytes() {
    let dir = std::env::temp_dir().join(format!("fr-legacy-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let files = [
        (Fmt::Cab, cab()),
        (Fmt::Pcx, pcx(1)),
        (Fmt::Pcx, pcx(3)),
        (Fmt::Dcx, dcx()),
        (Fmt::Ico, icon(false)),
        (Fmt::Ico, icon(true)),
        (Fmt::Tga, tga(false)),
        (Fmt::Tga, tga(true)),
    ];
    let mut bytes = vec![0; 512];
    let mut expected = Vec::new();
    for (fmt, data) in files {
        expected.push((bytes.len() as u64 / 512, fmt, data.clone()));
        bytes.extend_from_slice(&data);
        bytes.resize(bytes.len().div_ceil(512) * 512 + 512, 0);
    }
    let path = dir.join("disk.img");
    std::fs::write(&path, &bytes).unwrap();
    let src = Source::open(path.to_str().unwrap()).unwrap();
    let cat = carve::scan_raw(&src, 0, bytes.len() as u64, None);
    let mut reader = fr_core::data::Reader::new(&src, &cat).unwrap();
    let files: Vec<_> = cat.entries.iter().enumerate().filter(|(_, e)| !e.is_dir()).collect();
    assert_eq!(files.len(), expected.len());
    for ((i, e), (at, fmt, expected)) in files.into_iter().zip(expected) {
        assert_eq!(e.size, expected.len() as u64, "{fmt:?} at {at}");
        let mut got = Vec::new();
        reader
            .read(i as u32, &mut |b| {
                got.extend_from_slice(b);
                Ok(())
            })
            .unwrap();
        assert_eq!(got, expected);
    }
    drop(reader);
    drop(src);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn corrupt_legacy_lengths_and_truncated_palettes_are_not_complete_files() {
    use carve::{formats::length, stream::Stream};
    let dir = std::env::temp_dir().join(format!("fr-legacy-bad-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("disk.img");
    let mut cases = Vec::new();
    let mut b = cab();
    put32(&mut b, 8, 0xffff_ffff);
    cases.push((Fmt::Cab, b));
    let mut b = cab();
    put16(&mut b, 52, 0xfffd);
    cases.push((Fmt::Cab, b));
    let mut b = pcx(1);
    b.truncate(b.len() - 1);
    cases.push((Fmt::Pcx, b));
    let mut b = pcx(3);
    b[128] = 0xc7;
    cases.push((Fmt::Pcx, b));
    let mut b = dcx();
    put32(&mut b, 8, 4100);
    cases.push((Fmt::Dcx, b));
    let mut b = icon(false);
    put32(&mut b, 18, 0xffff_ffff);
    cases.push((Fmt::Ico, b));
    let mut b = tga(true);
    b[18] = 0x84;
    cases.push((Fmt::Tga, b));
    let mut b = tga(false);
    b.truncate(b.len() - 1);
    cases.push((Fmt::Tga, b));
    for (fmt, mut bytes) in cases {
        let cap = bytes.len() as u64;
        bytes.resize(bytes.len().div_ceil(512) * 512, 0);
        std::fs::write(&path, &bytes).unwrap();
        let src = Source::open(path.to_str().unwrap()).unwrap();
        assert_eq!(length(fmt, &mut Stream::new(&src, 0, cap).unwrap()), None, "{fmt:?}");
    }
    std::fs::remove_dir_all(dir).unwrap();
}
