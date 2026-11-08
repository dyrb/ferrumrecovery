//! LZNT1, the compression of NTFS compressed files.
//!
//! A compression unit (usually 16 clusters, 64 KiB) is a series of chunks. Each chunk
//! starts with a 16-bit header: bits 0-11 hold the chunk length minus 3 (header included),
//! bit 15 tells whether it is compressed. An uncompressed chunk is 4096 raw bytes. A
//! compressed chunk is groups of a flag byte and eight tokens: a 0 bit is a literal byte,
//! a 1 bit a 16-bit back reference whose split between offset and length bits depends on
//! the position inside the 4096-byte chunk. A chunk that decodes to less than 4096 bytes
//! is followed by zeros; a zero header ends the unit.

const CHUNK: usize = 4096;

/// Decompress one unit from `src` into `out` (its full uncompressed length). Bytes the
/// stream does not cover stay zero. Returns `false` for a malformed stream; what was
/// decoded up to that point is kept.
pub fn decompress(src: &[u8], out: &mut [u8]) -> bool {
    out.fill(0);
    let mut s = 0usize;
    let mut o = 0usize;
    while o < out.len() && s + 2 <= src.len() {
        let h = u16::from_le_bytes([src[s], src[s + 1]]);
        if h == 0 {
            break;
        }
        let len = (h & 0x0FFF) as usize + 1;
        s += 2;
        if s + len > src.len() {
            return false;
        }
        let chunk = &src[s..s + len];
        s += len;
        let end = (o + CHUNK).min(out.len());
        if h & 0x8000 == 0 {
            let n = len.min(end - o);
            out[o..o + n].copy_from_slice(&chunk[..n]);
        } else if !chunk_decode(chunk, &mut out[o..end]) {
            return false;
        }
        o = end;
    }
    true
}

fn chunk_decode(c: &[u8], out: &mut [u8]) -> bool {
    let mut i = 0usize;
    let mut o = 0usize;
    while i < c.len() {
        let flags = c[i];
        i += 1;
        for bit in 0..8 {
            if i >= c.len() {
                return true;
            }
            if flags >> bit & 1 == 0 {
                if o >= out.len() {
                    return false;
                }
                out[o] = c[i];
                o += 1;
                i += 1;
            } else {
                if i + 2 > c.len() || o == 0 {
                    return false;
                }
                let t = u16::from_le_bytes([c[i], c[i + 1]]) as usize;
                i += 2;
                let mut lg = 0;
                let mut p = o - 1;
                while p >= 0x10 {
                    lg += 1;
                    p >>= 1;
                }
                let back = (t >> (12 - lg)) + 1;
                let n = (t & (0xFFF >> lg)) + 3;
                if back > o || o + n > out.len() {
                    return false;
                }
                // Byte by byte: the source may overlap the bytes being written.
                for k in 0..n {
                    out[o + k] = out[o + k - back];
                }
                o += n;
            }
        }
    }
    true
}

/// Greedy LZNT1 encoder, for tests: real data comes from NTFS.
#[cfg(test)]
pub fn compress(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for chunk in data.chunks(CHUNK) {
        let mut body = Vec::new();
        let mut o = 0usize;
        while o < chunk.len() {
            let flag_at = body.len();
            body.push(0u8);
            for bit in 0..8 {
                if o >= chunk.len() {
                    break;
                }
                let lg = if o == 0 { 0 } else { (usize::BITS - (o - 1).leading_zeros()).saturating_sub(4) as usize };
                let max_back = 1usize << (4 + lg);
                let max_len = (0xFFF >> lg) + 3;
                let (mut best, mut best_back) = (0usize, 0usize);
                for back in 1..=max_back.min(o) {
                    let mut n = 0;
                    while n < max_len && o + n < chunk.len() && chunk[o + n] == chunk[o + n - back] {
                        n += 1;
                    }
                    if n > best {
                        best = n;
                        best_back = back;
                    }
                }
                if best >= 3 {
                    body[flag_at] |= 1 << bit;
                    let t = ((best_back - 1) << (12 - lg)) | (best - 3);
                    body.extend_from_slice(&(t as u16).to_le_bytes());
                    o += best;
                } else {
                    body.push(chunk[o]);
                    o += 1;
                }
            }
        }
        if body.len() >= chunk.len() && chunk.len() == CHUNK {
            out.extend_from_slice(&(0x3000u16 | (CHUNK as u16 - 1)).to_le_bytes());
            out.extend_from_slice(chunk);
        } else {
            out.extend_from_slice(&(0xB000u16 | (body.len() as u16 - 1)).to_le_bytes());
            out.extend_from_slice(&body);
        }
    }
    out.extend_from_slice(&[0, 0]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hand_made_back_reference() {
        // literals a b c, then (back 3, length 6): "abcabcabc"
        let src = [0x05, 0xB0, 0x08, b'a', b'b', b'c', 0x03, 0x20, 0, 0];
        let mut out = [0xEEu8; 16];
        assert!(decompress(&src, &mut out));
        assert_eq!(&out[..9], b"abcabcabc");
        assert!(out[9..].iter().all(|&b| b == 0));
    }

    #[test]
    fn roundtrip_mixed_data() {
        let mut data = Vec::new();
        let mut x = 12345u32;
        for i in 0..65536u32 {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            // Text-like stretches, runs and noise: every split of offset/length bits gets used.
            let b = match (i / 3000) % 3 {
                0 => b"the quick brown fox jumps over the lazy dog "[(i % 44) as usize],
                1 => (i / 700) as u8,
                _ => (x >> 24) as u8,
            };
            data.push(b);
        }
        let packed = compress(&data);
        assert!(packed.len() < data.len());
        let mut out = vec![0u8; data.len()];
        assert!(decompress(&packed, &mut out));
        assert_eq!(out, data);
    }

    #[test]
    fn short_chunk_is_padded_with_zeros() {
        let mut packed = compress(b"hello hello hello");
        packed.truncate(packed.len() - 2);
        packed.extend_from_slice(&compress(b"next")[..]);
        let mut out = vec![1u8; 8192];
        assert!(decompress(&packed, &mut out));
        assert_eq!(&out[..17], b"hello hello hello");
        assert!(out[17..4096].iter().all(|&b| b == 0));
        assert_eq!(&out[4096..4100], b"next");
    }

    #[test]
    fn malformed_streams_do_not_panic() {
        let mut out = vec![0u8; 4096];
        assert!(!decompress(&[0xFF, 0xBF, 1, 2], &mut out));
        assert!(!decompress(&[0x02, 0xB0, 0x01, 0x05, 0x00], &mut out), "reference before any output");
    }
}
