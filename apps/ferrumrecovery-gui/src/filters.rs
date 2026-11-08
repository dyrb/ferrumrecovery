//! Optional filters are transient; paths and search terms are not stored in settings.
use crate::app;
use crate::lang::{S, t};
use crate::util::{send, wide};
use fr_core::filter::{Filter, Pattern, parse_size};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

#[derive(Clone, Default)]
pub struct Advanced {
    pub values: [String; 7],
}

impl Advanced {
    pub fn apply(&self, f: &mut Filter) -> Result<(), ()> {
        f.path = self.values[0].split(';').map(str::trim).filter(|s| !s.is_empty()).map(Pattern::new).collect();
        let size =
            |i: usize| if self.values[i].trim().is_empty() { Ok(None) } else { parse_size(self.values[i].trim()).map(Some).ok_or(()) };
        let date = |i: usize| {
            if self.values[i].trim().is_empty() { Ok(None) } else { fr_core::time::parse_date(self.values[i].trim()).map(Some).ok_or(()) }
        };
        f.min_size = size(1)?;
        f.max_size = size(2)?;
        f.after = date(3)?;
        f.before = date(4)?;
        f.deleted_after = date(5)?;
        f.deleted_before = date(6)?;
        if f.min_size.zip(f.max_size).is_some_and(|(a, b)| a > b)
            || f.after.zip(f.before).is_some_and(|(a, b)| a >= b)
            || f.deleted_after.zip(f.deleted_before).is_some_and(|(a, b)| a >= b)
        {
            return Err(());
        }
        Ok(())
    }
}

struct Dialog {
    initial: Advanced,
    edits: [HWND; 7],
    accepted: bool,
    font: HFONT,
    dpi: u32,
}

pub fn show(initial: Advanced) -> Option<Advanced> {
    let (parent, font, pal, dpi) = app::with(|a| (a.w.main, a.font, a.pal, a.dpi))?;
    let s = |v| v * dpi as i32 / 96;
    let class = wide("FerrumRecoveryFilters");
    let mut state = Box::new(Dialog { initial, edits: [std::ptr::null_mut(); 7], accepted: false, font, dpi });
    unsafe {
        let inst = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null());
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(proc);
        wc.hInstance = inst;
        wc.lpszClassName = class.as_ptr();
        wc.hCursor = LoadCursorW(std::ptr::null_mut(), IDC_ARROW);
        RegisterClassW(&wc);
        let h = CreateWindowExW(
            WS_EX_DLGMODALFRAME,
            class.as_ptr(),
            wide(t(S::Filters)).as_ptr(),
            WS_CAPTION | WS_SYSMENU,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            s(650),
            s(430),
            parent,
            std::ptr::null_mut(),
            inst,
            (&mut *state as *mut Dialog).cast(),
        );
        if h.is_null() {
            return None;
        }
        crate::theme::set_title_dark(h, pal.dark);
        EnableWindow(parent, 0);
        ShowWindow(h, SW_SHOW);
        let mut msg: MSG = std::mem::zeroed();
        while IsWindow(h) != 0 {
            let r = GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0);
            if r <= 0 {
                if r == 0 {
                    PostQuitMessage(msg.wParam as i32);
                }
                break;
            }
            if IsDialogMessageW(h, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if IsWindow(h) != 0 {
            DestroyWindow(h);
        }
        EnableWindow(parent, 1);
        SetActiveWindow(parent);
    }
    state.accepted.then_some(state.initial)
}

unsafe extern "system" fn proc(h: HWND, m: u32, w: usize, l: isize) -> isize {
    unsafe {
        if m == WM_NCCREATE {
            let c = &*(l as *const CREATESTRUCTW);
            SetWindowLongPtrW(h, GWLP_USERDATA, c.lpCreateParams as isize);
        }
        let p = GetWindowLongPtrW(h, GWLP_USERDATA) as *mut Dialog;
        if p.is_null() {
            return DefWindowProcW(h, m, w, l);
        }
        let d = &mut *p;
        let s = |v| v * d.dpi as i32 / 96;
        match m {
            WM_CREATE => {
                let labels = [S::PathMask, S::MinSize, S::MaxSize, S::After, S::Before, S::DeletedAfter, S::DeletedBefore];
                let inst = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null());
                for (i, label) in labels.into_iter().enumerate() {
                    let y = s(18 + i as i32 * 40);
                    let label = CreateWindowExW(
                        0,
                        wide("STATIC").as_ptr(),
                        wide(t(label)).as_ptr(),
                        WS_CHILD | WS_VISIBLE,
                        s(18),
                        y + s(3),
                        s(280),
                        s(26),
                        h,
                        std::ptr::null_mut(),
                        inst,
                        std::ptr::null(),
                    );
                    send(label, WM_SETFONT, d.font as usize, 0);
                    let edit = CreateWindowExW(
                        WS_EX_CLIENTEDGE,
                        wide("EDIT").as_ptr(),
                        wide(&d.initial.values[i]).as_ptr(),
                        WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL as u32,
                        s(310),
                        y,
                        s(300),
                        s(28),
                        h,
                        (100 + i) as HMENU,
                        inst,
                        std::ptr::null(),
                    );
                    send(edit, WM_SETFONT, d.font as usize, 0);
                    send(edit, EM_SETLIMITTEXT, 4096, 0);
                    d.edits[i] = edit;
                    let dark = app::with(|a| a.pal.dark).unwrap_or(false);
                    crate::theme::style_control(edit, dark, crate::theme::Kind::Cfd);
                }
                for (id, label, x) in [(IDOK, S::Apply, 390), (IDCANCEL, S::Cancel, 505)] {
                    let b = CreateWindowExW(
                        0,
                        wide("BUTTON").as_ptr(),
                        wide(t(label)).as_ptr(),
                        WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_OWNERDRAW as u32,
                        s(x),
                        s(320),
                        s(105),
                        s(30),
                        h,
                        id as HMENU,
                        inst,
                        std::ptr::null(),
                    );
                    send(b, WM_SETFONT, d.font as usize, 0);
                }
                0
            }
            WM_COMMAND => {
                match (w & 0xffff) as i32 {
                    IDOK => {
                        let mut next = Advanced::default();
                        for (i, &edit) in d.edits.iter().enumerate() {
                            let mut b = vec![0u16; 4097];
                            let n = GetWindowTextW(edit, b.as_mut_ptr(), b.len() as i32);
                            next.values[i] = String::from_utf16_lossy(&b[..n as usize]);
                        }
                        if next.apply(&mut Filter::default()).is_err() {
                            MessageBoxW(h, wide(t(S::InvalidFilter)).as_ptr(), wide(t(S::Filters)).as_ptr(), MB_OK | MB_ICONERROR);
                            return 0;
                        }
                        d.initial = next;
                        d.accepted = true;
                        DestroyWindow(h);
                    }
                    IDCANCEL => {
                        DestroyWindow(h);
                    }
                    _ => {}
                }
                0
            }
            WM_DRAWITEM => crate::ui::button(l) as isize,
            WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT => app::with(|a| {
                SetTextColor(w as HDC, a.pal.text);
                SetBkColor(w as HDC, a.pal.input);
                a.brush_input as isize
            })
            .unwrap_or(0),
            WM_ERASEBKGND => {
                app::with(|a| crate::util::fill(w as HDC, &crate::util::client(h), a.pal.bg));
                1
            }
            WM_CLOSE => {
                DestroyWindow(h);
                0
            }
            _ => DefWindowProcW(h, m, w, l),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_sizes_and_exclusive_date_ranges() {
        let mut a = Advanced::default();
        let mut f = Filter::default();
        a.values[1] = "2 MB".into();
        a.values[2] = "1 KB".into();
        assert!(a.apply(&mut f).is_err());
        a.values[2] = "5 MB".into();
        a.values[3] = "2026-01-01".into();
        a.values[4] = "2026-01-01".into();
        assert!(a.apply(&mut f).is_err());
        a.values[4] = "2026-02-01".into();
        a.values[0] = "*Photos*;*Docs*".into();
        assert!(a.apply(&mut f).is_ok());
        assert_eq!(f.path.len(), 2);
        assert_eq!(f.min_size, Some(2 << 20));
    }
}
