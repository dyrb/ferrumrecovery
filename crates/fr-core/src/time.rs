//! FILETIME (100 ns since 1601-01-01 UTC) to and from local dates.

use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows_sys::Win32::Storage::FileSystem::{FileTimeToLocalFileTime, LocalFileTimeToFileTime};
use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToFileTime};

fn ft(v: u64) -> FILETIME {
    FILETIME { dwLowDateTime: v as u32, dwHighDateTime: (v >> 32) as u32 }
}

fn val(f: &FILETIME) -> u64 {
    (f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64
}

/// Local `YYYY-MM-DD HH:MM`, or an empty string for 0 and invalid values.
pub fn format(v: u64) -> String {
    if v == 0 {
        return String::new();
    }
    let utc = ft(v);
    let mut local = ft(0);
    // SAFETY: valid pointers to FILETIME/SYSTEMTIME values on the stack.
    let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe {
        if FileTimeToLocalFileTime(&utc, &mut local) == 0 || FileTimeToSystemTime(&local, &mut st) == 0 {
            return String::new();
        }
    }
    format!("{:04}-{:02}-{:02} {:02}:{:02}", st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute)
}

/// `YYYY-MM-DD` (local midnight) to FILETIME.
pub fn parse_date(s: &str) -> Option<u64> {
    let mut it = s.trim().split('-');
    let y: u16 = it.next()?.parse().ok()?;
    let m: u16 = it.next()?.parse().ok()?;
    let d: u16 = it.next()?.parse().ok()?;
    if it.next().is_some() || !(1601..=30827).contains(&y) || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let st = SYSTEMTIME { wYear: y, wMonth: m, wDayOfWeek: 0, wDay: d, wHour: 0, wMinute: 0, wSecond: 0, wMilliseconds: 0 };
    let mut local = ft(0);
    let mut utc = ft(0);
    // SAFETY: valid pointers to values on the stack.
    unsafe {
        if SystemTimeToFileTime(&st, &mut local) == 0 || LocalFileTimeToFileTime(&local, &mut utc) == 0 {
            return None;
        }
    }
    Some(val(&utc))
}

/// FAT date and time fields to FILETIME (UTC). `utc_offset_min` is the offset exFAT
/// stores with its timestamps; without it the fields are local time. `extra_10ms` adds
/// the finer part some fields carry (0 to 199). 0 when the fields are not a valid date.
pub fn from_dos(date: u16, time: u16, extra_10ms: u8, utc_offset_min: Option<i32>) -> u64 {
    let (y, m, d) = (1980 + (date >> 9), (date >> 5) & 15, date & 31);
    let (hh, mm, ss) = (time >> 11, (time >> 5) & 63, (time & 31) * 2);
    if date == 0 || !(1..=12).contains(&m) || d == 0 || hh > 23 || mm > 59 || ss > 59 {
        return 0;
    }
    let ms = extra_10ms.min(199) as u16 * 10;
    let st = SYSTEMTIME {
        wYear: y,
        wMonth: m,
        wDayOfWeek: 0,
        wDay: d,
        wHour: hh,
        wMinute: mm,
        wSecond: ss + ms / 1000,
        wMilliseconds: ms % 1000,
    };
    let mut f = ft(0);
    // SAFETY: valid pointers to values on the stack.
    if unsafe { SystemTimeToFileTime(&st, &mut f) } == 0 {
        return 0;
    }
    match utc_offset_min {
        Some(off) => (val(&f) as i64 - off as i64 * 600_000_000) as u64,
        None => {
            let mut utc = ft(0);
            // SAFETY: as above.
            if unsafe { LocalFileTimeToFileTime(&f, &mut utc) } == 0 {
                return 0;
            }
            val(&utc)
        }
    }
}

/// FILETIME to `SystemTime`, for setting times on recovered files.
pub fn to_system(v: u64) -> Option<std::time::SystemTime> {
    const UNIX_EPOCH_FT: u64 = 116_444_736_000_000_000;
    if v == 0 {
        return None;
    }
    let base = std::time::UNIX_EPOCH;
    if v >= UNIX_EPOCH_FT {
        base.checked_add(std::time::Duration::from_nanos((v - UNIX_EPOCH_FT).saturating_mul(100)))
    } else {
        base.checked_sub(std::time::Duration::from_nanos((UNIX_EPOCH_FT - v).saturating_mul(100)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_roundtrip() {
        let v = parse_date("2026-03-15").unwrap();
        assert!(format(v).starts_with("2026-03-15 00:00"));
        assert_eq!(parse_date("2026-13-01"), None);
        assert_eq!(parse_date("yesterday"), None);
        assert_eq!(format(0), "");
    }

    #[test]
    fn dos_dates() {
        // 2026-03-15 12:34:56 with a UTC offset of +03:00.
        let date = ((2026 - 1980) << 9) | (3 << 5) | 15;
        let time = (12 << 11) | (34 << 5) | 28;
        let utc = from_dos(date, time, 0, Some(180));
        let plain = from_dos(date, time, 0, Some(0));
        assert_eq!(plain - utc, 3 * 3600 * 10_000_000);
        assert_eq!(from_dos(0, 0, 0, None), 0);
        assert_eq!(from_dos(date, (25 << 11) as u16, 0, None), 0);
    }
}
