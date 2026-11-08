//! Read-only discovery of shadow-copy device names. No snapshots are created.
use std::{ffi::c_void, io, ptr};
use windows_sys::Win32::Foundation::{HANDLE, UNICODE_STRING};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

#[repr(C)]
struct Attributes {
    length: u32,
    root: HANDLE,
    name: *mut UNICODE_STRING,
    flags: u32,
    security: *mut c_void,
    qos: *mut c_void,
}

#[repr(C)]
struct Entry {
    name: UNICODE_STRING,
    kind: UNICODE_STRING,
}

type Open = unsafe extern "system" fn(*mut HANDLE, u32, *const Attributes) -> i32;
type Query = unsafe extern "system" fn(HANDLE, *mut c_void, u32, u8, u8, *mut u32, *mut u32) -> i32;
type Close = unsafe extern "system" fn(HANDLE) -> i32;

/// Enumerate actual names in the NT device directory, without guessing snapshot numbers.
/// Microsoft documents these exports as optional, so they are resolved at runtime.
pub fn shadow_copies() -> io::Result<Vec<u32>> {
    let unavailable =
        || crate::error::Error::new("shadow copy discovery is unavailable on this version of Windows").into_io(io::ErrorKind::Unsupported);
    let status_error = |s: i32| {
        crate::error::Error::value("shadow copy discovery failed (NTSTATUS {})", format!("{:08X}", s as u32)).into_io(io::ErrorKind::Other)
    };
    let malformed = || crate::error::Error::new("invalid device directory response").into_io(io::ErrorKind::InvalidData);
    // SAFETY: ntdll is loaded for the process lifetime. Only documented exports are
    // called, with their NT ABI signatures; absence is a reported discovery error.
    unsafe {
        let dll = GetModuleHandleW("ntdll.dll\0".encode_utf16().collect::<Vec<_>>().as_ptr());
        if dll.is_null() {
            return Err(unavailable());
        }
        let open: Open = std::mem::transmute(GetProcAddress(dll, c"NtOpenDirectoryObject".as_ptr().cast()).ok_or_else(unavailable)?);
        let query: Query = std::mem::transmute(GetProcAddress(dll, c"NtQueryDirectoryObject".as_ptr().cast()).ok_or_else(unavailable)?);
        let close: Close = std::mem::transmute(GetProcAddress(dll, c"NtClose".as_ptr().cast()).ok_or_else(unavailable)?);
        let mut path: Vec<u16> = "\\Device".encode_utf16().collect();
        let mut name =
            UNICODE_STRING { Length: (path.len() * 2) as u16, MaximumLength: (path.len() * 2) as u16, Buffer: path.as_mut_ptr() };
        let attrs = Attributes {
            length: size_of::<Attributes>() as u32,
            root: ptr::null_mut(),
            name: &mut name,
            flags: 0x40,
            security: ptr::null_mut(),
            qos: ptr::null_mut(),
        };
        let mut handle = ptr::null_mut();
        let status = open(&mut handle, 1, &attrs); // DIRECTORY_QUERY, OBJ_CASE_INSENSITIVE
        if status < 0 {
            return Err(status_error(status));
        }
        let result = (|| {
            let mut buf = vec![0usize; (16 << 10) / size_of::<usize>()];
            let mut context = 0u32;
            let mut restart = true;
            let mut found = Vec::new();
            loop {
                let previous = context;
                let mut used = 0;
                let status = query(
                    handle,
                    buf.as_mut_ptr().cast(),
                    (buf.len() * size_of::<usize>()) as u32,
                    0,
                    restart as u8,
                    &mut context,
                    &mut used,
                );
                if status as u32 == 0x8000001A {
                    break;
                } // STATUS_NO_MORE_ENTRIES
                if matches!(status as u32, 0xC0000023 | 0x80000005) && buf.len() * size_of::<usize>() < 1 << 20 {
                    context = previous;
                    buf.resize(buf.len() * 2, 0);
                    continue;
                }
                if status < 0 {
                    return Err(status_error(status));
                }
                if used as usize > buf.len() * size_of::<usize>() {
                    return Err(malformed());
                }
                let base = buf.as_ptr() as usize;
                let mut at = 0;
                while at + size_of::<Entry>() <= used as usize {
                    let entry = &*((base + at) as *const Entry);
                    if entry.name.Buffer.is_null() {
                        break;
                    }
                    let start = entry.name.Buffer as usize;
                    let len = entry.name.Length as usize;
                    if !len.is_multiple_of(2)
                        || !start.is_multiple_of(2)
                        || start < base
                        || start.checked_add(len).is_none_or(|end| end > base + used as usize)
                    {
                        return Err(malformed());
                    }
                    let text = String::from_utf16_lossy(std::slice::from_raw_parts(entry.name.Buffer, len / 2));
                    if let Some(number) = text.strip_prefix("HarddiskVolumeShadowCopy").and_then(|s| s.parse::<u32>().ok()) {
                        found.push(number);
                    }
                    at += size_of::<Entry>();
                }
                if status == 0 {
                    break;
                }
                if status != 0x105 || context == previous {
                    return Err(malformed());
                } // STATUS_MORE_ENTRIES
                restart = false;
            }
            found.sort_unstable();
            found.dedup();
            Ok(found)
        })();
        close(handle);
        result
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn queries_device_names_without_opening_disks() {
        let copies = super::shadow_copies().expect("NT device directory query");
        assert!(copies.windows(2).all(|pair| pair[0] < pair[1]));
    }
}
