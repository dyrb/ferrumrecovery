//! Structural boundaries of CAB, PCX/DCX, ICO/CUR and TGA images.
use super::stream::Stream;

fn le16(b: &[u8], p: usize) -> u16 {
    u16::from_le_bytes([b[p], b[p + 1]])
}
fn le32(b: &[u8], p: usize) -> u32 {
    u32::from_le_bytes(b[p..p + 4].try_into().unwrap())
}

pub fn pcx_header(h: &[u8]) -> bool {
    h.len() >= 128
        && h[0] == 10
        && [0, 2, 3, 4, 5].contains(&h[1])
        && h[2] <= 1
        && [1, 2, 4, 8].contains(&h[3])
        && h[64] == 0
        && (1..=4).contains(&h[65])
        && le16(h, 8) >= le16(h, 4)
        && le16(h, 10) >= le16(h, 6)
        && le16(h, 66) != 0
        && le16(h, 66).is_multiple_of(2)
        && (le16(h, 66) as u64) * 8 >= (le16(h, 8) as u64 - le16(h, 4) as u64 + 1) * h[3] as u64
        && h[74..128].iter().all(|&b| b == 0)
}

pub fn pcx(s: &mut Stream, base: u64) -> Option<u64> {
    let h = s.bytes(base, 128)?;
    if !pcx_header(h) {
        return None;
    }
    let palette = h[1] == 5 && h[3] == 8 && h[65] == 1;
    let encoded = h[2] == 1;
    let line = le16(h, 66) as u64 * h[65] as u64;
    let mut left = line * (le16(h, 10) as u64 - le16(h, 6) as u64 + 1);
    let total = left;
    let mut p = base + 128;
    if !encoded {
        p = p.checked_add(left)?;
    } else {
        // Decode counts only, in windows; never allocate the uncompressed image.
        let mut pending = false;
        let mut count = 0u64;
        while left != 0 {
            let n = (s.cap().checked_sub(p)?).min(60 << 10) as usize;
            if n == 0 {
                return None;
            }
            let b = s.bytes(p, n)?;
            let mut consumed = 0;
            for &value in b {
                consumed += 1;
                if pending {
                    pending = false;
                    if count > left || count > line - (total - left) % line {
                        return None;
                    }
                    left -= count;
                } else if value & 0xc0 == 0xc0 {
                    count = (value & 63) as u64;
                    if count == 0 {
                        return None;
                    }
                    pending = true;
                } else {
                    left -= 1;
                }
                if left == 0 {
                    break;
                }
            }
            p += consumed;
        }
    }
    if palette {
        if s.u8(p)? != 12 {
            return None;
        }
        p += 769;
    }
    (p <= s.cap()).then_some(p)
}

pub fn dcx(s: &mut Stream) -> Option<u64> {
    if s.u32le(0)? != 0x3ade_68b1 {
        return None;
    }
    let mut offsets = Vec::new();
    for i in 0..1024 {
        let off = s.u32le(4 + i * 4)? as u64;
        if off == 0 {
            break;
        }
        if off < 4100 || offsets.last().is_some_and(|&v| off <= v) {
            return None;
        }
        offsets.push(off);
    }
    if offsets.is_empty() || offsets.len() == 1024 {
        return None;
    }
    let mut end = 4100;
    for (i, &off) in offsets.iter().enumerate() {
        end = pcx(s, off)?;
        if offsets.get(i + 1).is_some_and(|&next| end > next) {
            return None;
        }
    }
    Some(end)
}

fn c_string(s: &mut Stream, from: u64, end: u64) -> Option<u64> {
    let p = s.find(from, &[0], end)?;
    Some(p + 1)
}

pub fn cab(s: &mut Stream) -> Option<u64> {
    let h = s.bytes(0, 36)?;
    if &h[..4] != b"MSCF" || [4, 12, 20].iter().any(|&p| le32(h, p) != 0) || h[24..26] != [3, 1] {
        return None;
    }
    let size = le32(h, 8) as u64;
    let files_at = le32(h, 16) as u64;
    let folders = le16(h, 26) as u64;
    let files = le16(h, 28) as u64;
    let flags = le16(h, 30);
    if size < 36 || size > s.cap() || files_at >= size || folders == 0 || flags & !7 != 0 {
        return None;
    }
    let mut p = 36;
    let (folder_reserve, data_reserve) = if flags & 4 != 0 {
        let header = s.u16le(p)? as u64;
        let folder = s.u8(p + 2)? as u64;
        let data = s.u8(p + 3)? as u64;
        p += 4 + header;
        (folder, data)
    } else {
        (0, 0)
    };
    for bit in [1, 2] {
        if flags & bit != 0 {
            p = c_string(s, p, size)?;
            p = c_string(s, p, size)?;
        }
    }
    if p.checked_add(folders * (8 + folder_reserve))? > files_at {
        return None;
    }
    let folder_at = p;
    p = files_at;
    for _ in 0..files {
        let folder = s.u16le(p + 8)? as u64;
        if folder >= folders
            && match folder {
                0xfffd => flags & 1 == 0,
                0xfffe => flags & 2 == 0,
                0xffff => flags & 3 != 3,
                _ => true,
            }
        {
            return None;
        }
        p = c_string(s, p + 16, size)?;
    }
    let file_table_end = p;
    for i in 0..folders {
        let p = folder_at + i * (8 + folder_reserve);
        let mut data = s.u32le(p)? as u64;
        let blocks = s.u16le(p + 4)? as u64;
        let compression = s.u16le(p + 6)? & 15;
        if compression > 3 || data < file_table_end || data > size {
            return None;
        }
        for _ in 0..blocks {
            let packed = s.u16le(data + 4)? as u64;
            let unpacked = s.u16le(data + 6)?;
            if unpacked > 32768 {
                return None;
            }
            data = data.checked_add(8 + data_reserve + packed)?;
            if data > size {
                return None;
            }
        }
    }
    Some(size)
}

pub fn ico_header(h: &[u8]) -> bool {
    h.len() >= 22
        && le16(h, 0) == 0
        && [1, 2].contains(&le16(h, 2))
        && (1..=1024).contains(&le16(h, 4))
        && le32(h, 14) != 0
        && le32(h, 18) >= 6 + 16 * le16(h, 4) as u32
}

pub fn ico(s: &mut Stream) -> Option<u64> {
    if !ico_header(s.bytes(0, 22)?) {
        return None;
    }
    let count = s.u16le(4)? as u64;
    let mut end = 6 + 16 * count;
    for i in 0..count {
        let p = 6 + 16 * i;
        // Some Windows icon writers leave the entry's reserved byte nonzero.
        // Validate its bounds and embedded image rather than rejecting that padding.
        let size = s.u32le(p + 8)? as u64;
        let at = s.u32le(p + 12)? as u64;
        if at < 6 + 16 * count || size < 12 || at.checked_add(size)? > s.cap() {
            return None;
        }
        let h = s.bytes(at, 12)?;
        if !h.starts_with(b"\x89PNG\r\n\x1a\n") && ![12, 40, 108, 124].contains(&le32(h, 0)) {
            return None;
        }
        end = end.max(at + size);
    }
    Some(end)
}

pub fn tga_header(h: &[u8]) -> bool {
    h.len() >= 18
        && h[1] <= 1
        && [1, 2, 3, 9, 10, 11].contains(&h[2])
        && [8, 15, 16, 24, 32].contains(&h[16])
        && h[17] & 0xc0 == 0
        && le16(h, 12) != 0
        && le16(h, 14) != 0
        && if h[2] % 8 == 1 {
            h[1] == 1 && le16(h, 5) != 0 && [15, 16, 24, 32].contains(&h[7])
        } else {
            h[1] == 0 && h[3..8].iter().all(|&v| v == 0) && (h[2] % 8 != 3 || h[16] <= 16)
        }
}

pub fn tga(s: &mut Stream) -> Option<u64> {
    let h = s.bytes(0, 18)?;
    if !tga_header(h) {
        return None;
    }
    let mut p = 18 + h[0] as u64 + le16(h, 5) as u64 * (h[7] as u64).div_ceil(8);
    let mut pixels = le16(h, 12) as u64 * le16(h, 14) as u64;
    let bpp = (h[16] as u64).div_ceil(8);
    if h[2] < 8 {
        p += pixels * bpp;
    } else {
        while pixels != 0 {
            let packet = s.u8(p)?;
            let n = (packet as u64 & 127) + 1;
            if n > pixels {
                return None;
            }
            p += 1 + if packet & 128 != 0 { bpp } else { n * bpp };
            pixels -= n;
        }
    }
    // The footer disambiguates the weak TGA header from arbitrary disk data.
    let signature = s.find(p + 8, b"TRUEVISION-XFILE.\0", p.saturating_add(16 << 20))?;
    let footer = signature - 8;
    for (field, min_len) in [(footer, 495u64), (footer + 4, 2)] {
        let at = s.u32le(field)? as u64;
        if at != 0 && (at < p || at.checked_add(min_len)? > footer) {
            return None;
        }
        if at != 0 && min_len == 495 && s.u16le(at)? != 495 {
            return None;
        }
        if at != 0 && min_len == 2 {
            let count = s.u16le(at)? as u64;
            if at.checked_add(2 + count * 10)? > footer {
                return None;
            }
            for i in 0..count {
                let offset = s.u32le(at + 4 + i * 10)? as u64;
                let size = s.u32le(at + 8 + i * 10)? as u64;
                if offset.checked_add(size)? > footer {
                    return None;
                }
            }
        }
    }
    Some(signature + 18)
}
