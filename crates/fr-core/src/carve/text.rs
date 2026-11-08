//! Headerless text regions collected from sweep buffers, never by rereading the source.
//! These are fragments: readable text does not establish an original file boundary.
const MAX: u64 = 64 << 20;

#[derive(Clone, Copy, Debug)]
pub(super) struct Span {
    pub start: u64,
    pub len: u64,
    pub ext: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Encoding {
    Utf8,
    Le,
    Be,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Run {
    start: u64,
    len: u64,
    good: u64,
    letters: u64,
    spaces: u64,
    lines: u64,
    encoding: Encoding,
    pending: u8,
    value: u32,
    minimum: u32,
    half: Option<u8>,
    high: u16,
    ext: &'static str,
}

impl Run {
    fn new(start: u64, bytes: &[u8]) -> Self {
        let (encoding, bom) = if bytes.starts_with(&[255, 254]) {
            (Encoding::Le, 2)
        } else if bytes.starts_with(&[254, 255]) {
            (Encoding::Be, 2)
        } else if bytes.starts_with(&[239, 187, 191]) {
            (Encoding::Utf8, 3)
        } else {
            (Encoding::Utf8, 0)
        };
        let prefix = &bytes[bom..bytes.len().min(128)];
        let code = [b"#include".as_slice(), b"#define", b"#pragma"].iter().any(|p| prefix.starts_with(p));
        Self {
            start,
            len: bom as u64,
            good: bom as u64,
            letters: 0,
            spaces: 0,
            lines: 0,
            encoding,
            pending: 0,
            value: 0,
            minimum: 0,
            half: None,
            high: 0,
            ext: if code { "c" } else { "txt" },
        }
    }

    fn character(&mut self, value: u32) -> bool {
        let Some(ch) = char::from_u32(value) else { return false };
        if ch.is_control() && !matches!(ch, '\t' | '\r' | '\n' | '\x0c') {
            return false;
        }
        if ch.is_alphabetic() {
            self.letters += 1;
        }
        if ch.is_whitespace() {
            self.spaces += 1;
        }
        if ch == '\n' {
            self.lines += 1;
        }
        self.good = self.len;
        true
    }

    fn feed(&mut self, mut bytes: &[u8]) -> bool {
        if !matches!(self.encoding, Encoding::Utf8) {
            return self.feed_scalar(bytes);
        }
        while let Some(&first) = bytes.first() {
            if self.pending == 0 && bytes.len() >= 32 && (first.is_ascii_graphic() || first.is_ascii_whitespace()) {
                let ascii = ascii_prefix(bytes);
                self.len += ascii.bytes as u64;
                self.letters += ascii.letters as u64;
                self.spaces += ascii.spaces as u64;
                self.lines += ascii.lines as u64;
                if ascii.bytes != 0 {
                    self.good = self.len;
                    bytes = &bytes[ascii.bytes..];
                    if bytes.is_empty() {
                        return true;
                    }
                }
            }
            if !self.feed_scalar(&bytes[..1]) {
                return false;
            }
            bytes = &bytes[1..];
        }
        true
    }

    fn feed_scalar(&mut self, bytes: &[u8]) -> bool {
        for &b in bytes {
            self.len += 1;
            match self.encoding {
                Encoding::Utf8 => {
                    if self.pending == 0 {
                        if b < 128 {
                            if b.is_ascii_alphabetic() {
                                self.letters += 1;
                            } else if b.is_ascii_whitespace() {
                                self.spaces += 1;
                                if b == b'\n' {
                                    self.lines += 1;
                                }
                            } else if b < 32 || b == 127 {
                                return false;
                            }
                            self.good = self.len;
                        } else {
                            let (pending, value, minimum) = match b {
                                0xc2..=0xdf => (1, (b & 31) as u32, 0x80),
                                0xe0..=0xef => (2, (b & 15) as u32, 0x800),
                                0xf0..=0xf4 => (3, (b & 7) as u32, 0x10000),
                                _ => return false,
                            };
                            self.pending = pending;
                            self.value = value;
                            self.minimum = minimum;
                        }
                    } else {
                        if b & 0xc0 != 0x80 {
                            return false;
                        }
                        self.value = (self.value << 6) | (b & 63) as u32;
                        self.pending -= 1;
                        if self.pending == 0 && (self.value < self.minimum || !self.character(self.value)) {
                            return false;
                        }
                    }
                }
                Encoding::Le | Encoding::Be => {
                    let Some(first) = self.half.take() else {
                        self.half = Some(b);
                        continue;
                    };
                    let unit =
                        if matches!(self.encoding, Encoding::Le) { u16::from_le_bytes([first, b]) } else { u16::from_be_bytes([first, b]) };
                    if self.high != 0 {
                        if !(0xdc00..=0xdfff).contains(&unit) {
                            return false;
                        }
                        let value = 0x10000 + ((self.high as u32 - 0xd800) << 10) + unit as u32 - 0xdc00;
                        self.high = 0;
                        if !self.character(value) {
                            return false;
                        }
                    } else if (0xd800..=0xdbff).contains(&unit) {
                        self.high = unit;
                    } else if !self.character(unit as u32) {
                        return false;
                    }
                }
            }
        }
        true
    }

    fn span(&self) -> Option<Span> {
        (self.good >= 128 && self.letters >= 16 && (self.spaces >= 4 || self.lines > 0)).then_some(Span {
            start: self.start,
            len: self.good,
            ext: self.ext,
        })
    }
}

#[derive(Default)]
struct Ascii {
    bytes: usize,
    letters: u32,
    spaces: u32,
    lines: u32,
}

// Only complete ASCII characters take this path. Unicode keeps its scalar decoder state.
fn ascii_prefix(bytes: &[u8]) -> Ascii {
    #[cfg(target_arch = "x86_64")]
    {
        type Scan = unsafe fn(&[u8]) -> Ascii;
        static SCAN: std::sync::OnceLock<Scan> = std::sync::OnceLock::new();
        let scan = SCAN.get_or_init(|| if std::arch::is_x86_feature_detected!("avx2") { ascii_avx2 } else { ascii_sse2 });
        // SAFETY: SSE2 is part of x86-64; AVX2 requires OS/CPU feature detection.
        unsafe { scan(bytes) }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let mut out = Ascii::default();
        for &b in bytes {
            if !(b.is_ascii_graphic() || b.is_ascii_whitespace()) {
                break;
            }
            out.bytes += 1;
            out.letters += b.is_ascii_alphabetic() as u32;
            out.spaces += b.is_ascii_whitespace() as u32;
            out.lines += (b == b'\n') as u32;
        }
        out
    }
}

// Both instruction sets use the same masks and first-invalid-byte rule.
macro_rules! ascii_scanner {
    ($name:ident, $feature:literal, $width:literal, $load:ident, $set:ident, $or:ident, $and:ident, $eq:ident, $gt:ident, $mask:ident) => {
        #[cfg(target_arch = "x86_64")]
        #[target_feature(enable = $feature)]
        unsafe fn $name(bytes: &[u8]) -> Ascii {
            use std::arch::x86_64::*;
            let mut out = Ascii::default();
            while bytes.len() - out.bytes >= $width {
                // SAFETY: the full vector is in bounds; unaligned loads are permitted.
                let b = unsafe { $load(bytes.as_ptr().add(out.bytes).cast()) };
                let spaces = $or($eq(b, $set(32)), $or($or($eq(b, $set(9)), $eq(b, $set(10))), $or($eq(b, $set(12)), $eq(b, $set(13)))));
                let printable = $and($gt(b, $set(31)), $gt($set(127), b));
                let valid = $mask($or(printable, spaces)) as u32;
                let n = (!valid).trailing_zeros().min($width);
                let prefix = ((1u64 << n) - 1) as u32;
                let lower = $or(b, $set(32));
                let letters = $and($gt(lower, $set(96)), $gt($set(123), lower));
                out.letters += ($mask(letters) as u32 & prefix).count_ones();
                out.spaces += ($mask(spaces) as u32 & prefix).count_ones();
                out.lines += ($mask($eq(b, $set(10))) as u32 & prefix).count_ones();
                out.bytes += n as usize;
                if n < $width {
                    break;
                }
            }
            out
        }
    };
}

ascii_scanner!(
    ascii_sse2,
    "sse2",
    16,
    _mm_loadu_si128,
    _mm_set1_epi8,
    _mm_or_si128,
    _mm_and_si128,
    _mm_cmpeq_epi8,
    _mm_cmpgt_epi8,
    _mm_movemask_epi8
);
ascii_scanner!(
    ascii_avx2,
    "avx2",
    32,
    _mm256_loadu_si256,
    _mm256_set1_epi8,
    _mm256_or_si256,
    _mm256_and_si256,
    _mm256_cmpeq_epi8,
    _mm256_cmpgt_epi8,
    _mm256_movemask_epi8
);

#[derive(Default)]
pub(super) struct Index {
    spans: Vec<Span>,
    run: Option<Run>,
    next: u64,
}

impl Index {
    fn finish(&mut self) {
        if let Some(span) = self.run.take().and_then(|r| r.span()) {
            self.spans.push(span);
        }
    }

    pub fn observe(&mut self, at: u64, bytes: &[u8], unit: usize) {
        if at != self.next {
            self.finish();
        }
        for (i, part) in bytes.chunks(unit).enumerate() {
            let start = at + (i * unit) as u64;
            if self.run.is_none() {
                let mut run = Run::new(start, part);
                let skip = run.len as usize;
                if !run.feed(&part[skip..]) {
                    if let Some(span) = run.span() {
                        self.spans.push(span);
                    }
                    continue;
                }
                self.run = Some(run);
            } else if !self.run.as_mut().unwrap().feed(part) {
                self.finish();
            }
            if self.run.as_ref().is_some_and(|r| r.len >= MAX) {
                self.finish();
            }
        }
        self.next = at + bytes.len() as u64;
    }

    pub fn spans(&self) -> impl Iterator<Item = Span> + '_ {
        self.spans.iter().copied().chain(self.run.as_ref().and_then(Run::span))
    }

    pub fn at(&self, start: u64) -> Option<Span> {
        self.spans
            .binary_search_by_key(&start, |s| s.start)
            .ok()
            .map(|i| self.spans[i])
            .or_else(|| self.run.as_ref().filter(|r| r.start == start).and_then(Run::span))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn same_decoder(bytes: &[u8], chunk: usize) {
        let mut fast = Run::new(0, bytes);
        let mut scalar = fast.clone();
        let skip = fast.len as usize;
        for b in bytes[skip..].chunks(chunk) {
            let expected = scalar.feed_scalar(b);
            assert_eq!(fast.feed(b), expected);
            assert_eq!(fast, scalar);
            if !expected {
                break;
            }
        }
    }

    #[test]
    fn vector_text_matches_scalar_for_every_byte_and_vector_position() {
        let original = b"Useful ASCII text with numbers 1234 and punctuation.\t\r\n".repeat(12);
        for byte in 0..=255 {
            for at in 0..64 {
                let mut bytes = original.clone();
                bytes[at] = byte;
                same_decoder(&bytes, 512);
                #[cfg(target_arch = "x86_64")]
                {
                    let prefix = bytes.iter().take_while(|b| b.is_ascii_graphic() || b.is_ascii_whitespace()).count();
                    // SAFETY: SSE2 is guaranteed on this target; AVX2 is feature-checked.
                    let sse = unsafe { ascii_sse2(&bytes) };
                    assert_eq!(sse.bytes, prefix.min(bytes.len() / 16 * 16));
                    assert_eq!(sse.letters, bytes[..sse.bytes].iter().filter(|b| b.is_ascii_alphabetic()).count() as u32);
                    assert_eq!(sse.spaces, bytes[..sse.bytes].iter().filter(|b| b.is_ascii_whitespace()).count() as u32);
                    assert_eq!(sse.lines, bytes[..sse.bytes].iter().filter(|&&b| b == b'\n').count() as u32);
                    if std::arch::is_x86_feature_detected!("avx2") {
                        let avx = unsafe { ascii_avx2(&bytes) };
                        assert_eq!(avx.bytes, prefix.min(bytes.len() / 32 * 32));
                        assert_eq!(avx.letters, bytes[..avx.bytes].iter().filter(|b| b.is_ascii_alphabetic()).count() as u32);
                        assert_eq!(avx.spaces, bytes[..avx.bytes].iter().filter(|b| b.is_ascii_whitespace()).count() as u32);
                        assert_eq!(avx.lines, bytes[..avx.bytes].iter().filter(|&&b| b == b'\n').count() as u32);
                    }
                }
            }
        }
    }

    #[test]
    fn optimized_text_preserves_split_unicode_boms_and_invalid_sequences() {
        let text = "ASCII document with Unicode текст 日本語 and emoji 😀.\n".repeat(128);
        let mut cases = vec![text.as_bytes().to_vec()];
        for big_endian in [false, true] {
            let mut bytes = if big_endian { vec![254, 255] } else { vec![255, 254] };
            for unit in text.encode_utf16() {
                bytes.extend_from_slice(&if big_endian { unit.to_be_bytes() } else { unit.to_le_bytes() });
            }
            cases.push(bytes);
        }
        cases.push([b"\xef\xbb\xbf".as_slice(), text.as_bytes(), b"\xf0\x9f"].concat());
        for invalid in [b"\xc0\x80".as_slice(), b"\xed\xa0\x80", b"\xf4\x90\x80\x80", b"\xe2text", b"\x00"] {
            cases.push([text.as_bytes(), invalid].concat());
        }
        for bytes in cases {
            for chunk in [1, 7, 31, 32, 63, 512, 8192] {
                same_decoder(&bytes, chunk);
            }
        }
    }

    #[test]
    fn windows_coalesce_without_sector_duplicates_and_gaps_do_not_join() {
        let bytes = b"A useful line of source or document text.\n".repeat(16000);
        let mut ix = Index::default();
        for (i, b) in bytes.chunks(8192).enumerate() {
            ix.observe(i as u64 * 8192, b, 512);
        }
        let spans: Vec<_> = ix.spans().collect();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].len, bytes.len() as u64);
        ix.observe(bytes.len() as u64 + 512, &bytes[..512], 512);
        assert_eq!(ix.spans().count(), 2);
    }

    #[test]
    fn utf8_and_both_utf16_byte_orders_stop_before_binary_padding() {
        let text = "A document with Cyrillic текст and emoji 😀.\n".repeat(100);
        for encoding in [Encoding::Utf8, Encoding::Le, Encoding::Be] {
            let mut b = match encoding {
                Encoding::Utf8 => text.as_bytes().to_vec(),
                Encoding::Le | Encoding::Be => {
                    let mut b = if matches!(encoding, Encoding::Le) { vec![255, 254] } else { vec![254, 255] };
                    for u in text.encode_utf16() {
                        b.extend_from_slice(&if matches!(encoding, Encoding::Le) { u.to_le_bytes() } else { u.to_be_bytes() });
                    }
                    b
                }
            };
            let len = b.len();
            b.resize(b.len().div_ceil(512) * 512 + 512, 0);
            let mut ix = Index::default();
            ix.observe(0, &b, 512);
            let spans: Vec<_> = ix.spans().collect();
            assert_eq!(spans.len(), 1);
            assert_eq!(spans[0].len, len as u64);
        }
    }

    #[test]
    fn binary_and_zero_filled_regions_are_not_text() {
        let mut ix = Index::default();
        ix.observe(0, &[0; 8192], 512);
        let bytes: Vec<_> = (0..8192).map(|i| (i % 256) as u8).collect();
        ix.observe(8192, &bytes, 512);
        assert_eq!(ix.spans().count(), 0);
    }
}
