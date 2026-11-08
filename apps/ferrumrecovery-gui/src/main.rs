#![windows_subsystem = "windows"]

mod app;
mod cmds;
mod diagnostics;
mod dialog;
mod engine;
mod filters;
mod lang;
mod list;
mod preview;
mod settings;
mod theme;
mod ui;
mod util;

use app::{App, Col, Wnd};
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use util::{send, wide};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const ID_SOURCE: usize = 100;
const ID_SCAN: usize = 101;
const ID_RECOVER: usize = 102;
const ID_DEEP: usize = 103;
const ID_CHECK: usize = 104;
const ID_SEARCH: usize = 105;
const ID_TYPES: usize = 106;
const ID_STATES: usize = 107;
const MK_LBUTTON: usize = 0x0001;

fn main() {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        windows_sys::Win32::System::Ole::OleInitialize(null());
        let init =
            INITCOMMONCONTROLSEX { dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_LISTVIEW_CLASSES | ICC_STANDARD_CLASSES };
        InitCommonControlsEx(&init);
        BufferedPaintInit();
        let settings = settings::Settings::load();
        lang::set_lang(settings.lang);
        let pal = ui::palette(settings.theme);
        theme::set_app_dark(pal.dark);
        let inst = GetModuleHandleW(null());
        let class = wide("FerrumRecovery");
        let pc = wide("FerrumRecoveryPreview");
        let icon = LoadIconW(inst, std::ptr::without_provenance(1));
        let mut wc: WNDCLASSEXW = zeroed();
        wc.cbSize = size_of::<WNDCLASSEXW>() as u32;
        wc.style = CS_HREDRAW | CS_VREDRAW;
        wc.lpfnWndProc = Some(window_proc);
        wc.hInstance = inst;
        wc.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        wc.hIcon = icon;
        wc.hIconSm = icon;
        wc.lpszClassName = class.as_ptr();
        if RegisterClassExW(&wc) == 0 {
            return;
        }
        wc.lpfnWndProc = Some(preview_proc);
        wc.lpszClassName = pc.as_ptr();
        RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            wide("FerrumRecovery").as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1360,
            900,
            null_mut(),
            null_mut(),
            inst,
            null(),
        );
        if hwnd.is_null() {
            return;
        }
        let dpi = GetDpiForWindow(hwnd).max(96);
        let [font, bold, small, mono] = ui::make_fonts(dpi);
        let images = {
            use windows_sys::Win32::UI::Shell::*;
            let mut info: SHFILEINFOW = zeroed();
            SHGetFileInfoW(
                wide("file.txt").as_ptr(),
                windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL,
                &mut info,
                size_of::<SHFILEINFOW>() as u32,
                SHGFI_SYSICONINDEX | SHGFI_SMALLICON | SHGFI_USEFILEATTRIBUTES,
            ) as isize
        };
        let child = |class: &str, text: &str, style: u32, ex: u32, id: usize| {
            CreateWindowExW(
                ex,
                wide(class).as_ptr(),
                wide(text).as_ptr(),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_CLIPSIBLINGS | style,
                0,
                0,
                1,
                1,
                hwnd,
                id as HMENU,
                inst,
                null(),
            )
        };
        let w = Wnd {
            main: hwnd,
            source: child("COMBOBOX", "", CBS_DROPDOWNLIST as u32 | WS_VSCROLL, 0, ID_SOURCE),
            scan: child("BUTTON", "", BS_OWNERDRAW as u32, 0, ID_SCAN),
            recover: child("BUTTON", "", BS_OWNERDRAW as u32, 0, ID_RECOVER),
            deep: child("BUTTON", "", BS_OWNERDRAW as u32, 0, ID_DEEP),
            check: child("BUTTON", "", BS_OWNERDRAW as u32, 0, ID_CHECK),
            search: child("EDIT", "", ES_AUTOHSCROLL as u32, 0, ID_SEARCH),
            types: child("COMBOBOX", "", CBS_DROPDOWNLIST as u32 | WS_VSCROLL, 0, ID_TYPES),
            states: child("COMBOBOX", "", CBS_DROPDOWNLIST as u32 | WS_VSCROLL, 0, ID_STATES),
            folders: child(
                "SysListView32",
                "",
                LVS_REPORT | LVS_OWNERDATA | LVS_SHOWSELALWAYS | LVS_SINGLESEL | LVS_SHAREIMAGELISTS,
                0,
                list::ID_FOLDERS,
            ),
            files: child("SysListView32", "", LVS_REPORT | LVS_OWNERDATA | LVS_SHOWSELALWAYS | LVS_SHAREIMAGELISTS, 0, list::ID_FILES),
            preview: child("FerrumRecoveryPreview", "", 0, 0, 112),
        };
        for h in [w.source, w.scan, w.recover, w.deep, w.check, w.search, w.types, w.states, w.folders, w.files] {
            send(h, WM_SETFONT, font as usize, 0);
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let a = App {
            w,
            dpi,
            font,
            bold,
            small,
            mono,
            pal,
            brush_input: CreateSolidBrush(pal.input),
            accel: null_mut(),
            settings,
            layout: Default::default(),
            menu_widths: [0; 3],
            drag: None,
            sources: vec![],
            selected_source: String::new(),
            busy: None,
            status: app::Status::Message(lang::S::Ready),
            data: None,
            accepted: vec![],
            counts: vec![],
            count_all: 0,
            count_lost: 0,
            kids: Default::default(),
            expanded: [app::ALL].into_iter().collect(),
            folder_rows: vec![],
            folder_width: 122,
            folder: app::ALL,
            rows: vec![],
            checked: vec![],
            checked_count: 0,
            checked_bytes: 0,
            sort: (Col::Name, false),
            search: String::new(),
            advanced: Default::default(),
            group: None,
            state_choice: app::StateChoice::Any,
            preview_entry: None,
            preview_bytes: vec![],
            preview_image: None,
            preview_mode: Default::default(),
            preview_error: None,
            preview_notice: None,
            preview_cancel: None,
            preview_running: false,
            preview_generation: 0,
            events: rx,
            sender: tx.clone(),
            control: None,
            closing: false,
            low_priority: false,
            images,
            icons: Default::default(),
        };
        app::install(a);
        ui::control_metrics();
        list::setup(w.folders, true);
        list::setup(w.files, false);
        cmds::language();
        SetWindowPos(hwnd, null_mut(), 0, 0, 1360 * dpi as i32 / 96, 900 * dpi as i32 / 96, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
        ui::layout();
        ui::theme();
        let bindings = [
            (FVIRTKEY | FCONTROL, b'O' as u16, cmds::OPEN),
            (FVIRTKEY | FCONTROL, b'S' as u16, cmds::SAVE),
            (FVIRTKEY | FCONTROL, b'R' as u16, cmds::RECOVER),
            (FVIRTKEY, VK_F5, cmds::SCAN),
            (FVIRTKEY, VK_F6, cmds::PAUSE),
            (FVIRTKEY, VK_ESCAPE, cmds::STOP),
        ];
        let entries: Vec<_> = bindings.into_iter().map(|(f_virt, key, cmd)| ACCEL { fVirt: f_virt, key, cmd }).collect();
        let accel = CreateAcceleratorTableW(entries.as_ptr(), entries.len() as i32);
        app::with(|a| a.accel = accel);
        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);
        engine::sources(hwnd, tx);
        // A source argument is useful for opening an image from Explorer or the command line.
        if let Some(spec) = std::env::args().nth(1) {
            cmds::start(spec, None);
        }
        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            if TranslateAcceleratorW(hwnd, accel, &msg) == 0 && IsDialogMessageW(hwnd, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if !accel.is_null() {
            DestroyAcceleratorTable(accel);
        }
        windows_sys::Win32::System::Ole::OleUninitialize();
        BufferedPaintUnInit();
    }
}

unsafe extern "system" fn preview_proc(h: HWND, m: u32, w: usize, l: isize) -> isize {
    unsafe {
        match m {
            WM_ERASEBKGND => 1,
            WM_SIZE => {
                InvalidateRect(h, null(), 0);
                0
            }
            WM_PAINT => {
                let mut ps: PAINTSTRUCT = zeroed();
                let dc = BeginPaint(h, &mut ps);
                preview::paint(dc, &util::client(h));
                EndPaint(h, &ps);
                0
            }
            WM_PRINTCLIENT => {
                preview::paint(w as HDC, &util::client(h));
                0
            }
            _ => DefWindowProcW(h, m, w, l),
        }
    }
}

unsafe extern "system" fn window_proc(h: HWND, m: u32, w: usize, l: isize) -> isize {
    unsafe {
        match m {
            WM_ERASEBKGND => 1,
            WM_PAINT => {
                let mut ps: PAINTSTRUCT = zeroed();
                let dc = BeginPaint(h, &mut ps);
                ui::paint(dc);
                EndPaint(h, &ps);
                0
            }
            WM_SIZE => {
                if w != SIZE_MINIMIZED as usize {
                    ui::layout();
                    InvalidateRect(h, null(), 0);
                }
                0
            }
            WM_GETMINMAXINFO => {
                let mm = &mut *(l as *mut MINMAXINFO);
                let dpi = GetDpiForWindow(h).max(96);
                mm.ptMinTrackSize = POINT { x: 900 * dpi as i32 / 96, y: 560 * dpi as i32 / 96 };
                0
            }
            WM_DRAWITEM => {
                if ui::button(l) {
                    1
                } else {
                    DefWindowProcW(h, m, w, l)
                }
            }
            WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX | WM_CTLCOLORBTN => app::with(|a| {
                SetTextColor(w as HDC, a.pal.text);
                SetBkColor(w as HDC, a.pal.input);
                a.brush_input as isize
            })
            .unwrap_or(0),
            WM_NOTIFY => list::notify(l).unwrap_or_else(|| DefWindowProcW(h, m, w, l)),
            engine::WM_WORK => {
                cmds::on_events();
                0
            }
            WM_COMMAND => {
                let id = w & 0xffff;
                let code = (w >> 16) & 0xffff;
                match id {
                    ID_SCAN => cmds::scan_choice(),
                    ID_RECOVER => cmds::run(cmds::RECOVER),
                    ID_DEEP | ID_CHECK => {
                        app::with(|a| {
                            if a.busy.is_none() {
                                let v = if id == ID_DEEP { &mut a.settings.deep } else { &mut a.settings.check };
                                *v ^= true;
                                if id == ID_DEEP && !a.settings.deep {
                                    a.settings.full = false;
                                }
                            }
                        });
                        cmds::save_settings();
                        ui::controls();
                        InvalidateRect(l as HWND, null(), 0);
                    }
                    ID_SEARCH if code == EN_CHANGE as usize => {
                        SetTimer(h, cmds::TIMER_FILTER, 180, None);
                    }
                    ID_TYPES | ID_STATES if code == CBN_SELCHANGE as usize => cmds::filters(),
                    ID_SOURCE if code == CBN_SELENDOK as usize => {
                        let choice = app::with(|a| a.sources.get(send(a.w.source, CB_GETCURSEL, 0, 0) as usize).cloned()).flatten();
                        if let Some(choice) = choice {
                            if matches!(choice.kind, app::SourceKind::OpenImage | app::SourceKind::OpenSaved) {
                                cmds::scan_choice();
                            } else {
                                app::with(|a| a.selected_source = choice.spec);
                            }
                        }
                    }
                    n if n >= 300 => cmds::run(n as u16),
                    _ => {}
                }
                0
            }
            WM_TIMER => {
                match w {
                    cmds::TIMER_PROGRESS => {
                        if let Some(r) = app::with(|a| a.layout.top) {
                            InvalidateRect(h, &r, 0);
                        }
                    }
                    cmds::TIMER_FILTER => {
                        KillTimer(h, w);
                        cmds::filters();
                    }
                    cmds::TIMER_PREVIEW => {
                        KillTimer(h, w);
                        cmds::load_preview();
                    }
                    _ => {}
                }
                0
            }
            WM_CONTEXTMENU => {
                let mut p = POINT { x: (l as u32 & 0xffff) as i16 as i32, y: ((l as u32 >> 16) & 0xffff) as i16 as i32 };
                if p.x == -1 && p.y == -1 {
                    p = POINT { x: 50, y: 250 };
                    ClientToScreen(h, &mut p);
                }
                cmds::menu(3, p.x, p.y);
                0
            }
            WM_LBUTTONDOWN => {
                let x = (l as u32 & 0xffff) as i16 as i32;
                let y = ((l as u32 >> 16) & 0xffff) as i16 as i32;
                let action = app::with(|a| {
                    if util::contains(&a.layout.split1, x, y) {
                        a.drag = Some(app::Drag::Tree { offset: x - a.layout.folders.right });
                        return None;
                    }
                    if util::contains(&a.layout.split2, x, y) {
                        a.drag = Some(app::Drag::Preview { offset: x - a.layout.preview.left });
                        return None;
                    }
                    a.layout.menus.iter().find(|(r, _)| util::contains(r, x, y)).map(|(r, i)| (*i, r.left, r.bottom))
                })
                .flatten();
                if let Some((i, x, y)) = action {
                    let mut p = POINT { x, y };
                    ClientToScreen(h, &mut p);
                    cmds::menu(i, p.x, p.y);
                } else if app::with(|a| a.drag.is_some()).unwrap_or(false) {
                    SetCapture(h);
                }
                0
            }
            WM_MOUSEMOVE if w & MK_LBUTTON != 0 => {
                let x = (l as u32 & 0xffff) as i16 as i32;
                if app::with(|a| {
                    let Some(drag) = a.drag else { return false };
                    let width = util::client(h).right.max(1);
                    let shares = ui::drag_splitter(a.dpi, width, &a.layout, drag, x);
                    if let Some((tree, preview)) = shares {
                        a.settings.split_tree = tree;
                        a.settings.split_preview = preview;
                        true
                    } else {
                        false
                    }
                })
                .unwrap_or(false)
                {
                    ui::layout();
                    InvalidateRect(h, null(), 0);
                }
                0
            }
            WM_SETCURSOR if l as u32 & 0xffff == HTCLIENT => {
                let mut p = POINT::default();
                GetCursorPos(&mut p);
                ScreenToClient(h, &mut p);
                if app::with(|a| {
                    a.drag.is_some() || util::contains(&a.layout.split1, p.x, p.y) || util::contains(&a.layout.split2, p.x, p.y)
                })
                .unwrap_or(false)
                {
                    SetCursor(LoadCursorW(null_mut(), IDC_SIZEWE));
                    1
                } else {
                    DefWindowProcW(h, m, w, l)
                }
            }
            WM_LBUTTONUP => {
                app::with(|a| a.drag = None);
                ReleaseCapture();
                0
            }
            WM_CAPTURECHANGED => {
                app::with(|a| a.drag = None);
                0
            }
            WM_SETTINGCHANGE => {
                if app::with(|a| a.settings.theme == settings::ThemePref::System).unwrap_or(false) {
                    ui::theme();
                }
                0
            }
            WM_DPICHANGED => {
                let dpi = (w & 0xffff) as u32;
                let fonts = ui::make_fonts(dpi);
                if let Some((old, children)) = app::with(|a| {
                    let old = [a.font, a.bold, a.small, a.mono];
                    [a.font, a.bold, a.small, a.mono] = fonts;
                    a.dpi = dpi;
                    (
                        old,
                        [a.w.source, a.w.scan, a.w.recover, a.w.deep, a.w.check, a.w.search, a.w.types, a.w.states, a.w.folders, a.w.files],
                    )
                }) {
                    for child in children {
                        send(child, WM_SETFONT, fonts[0] as usize, 1);
                    }
                    for f in old {
                        DeleteObject(f);
                    }
                }
                let r = &*(l as *const RECT);
                SetWindowPos(h, null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                if let Some(w) = app::with(|a| a.w) {
                    list::columns(w.files);
                    list::folder_columns(w.folders);
                }
                ui::control_metrics();
                ui::measure();
                ui::layout();
                0
            }
            WM_CLOSE => {
                let busy = app::with(|a| {
                    if a.busy.is_some() {
                        a.closing = true;
                        if let Some(c) = &a.control {
                            c.cancel();
                        }
                        true
                    } else {
                        if let Some(c) = &a.preview_cancel {
                            c.cancel();
                        }
                        false
                    }
                })
                .unwrap_or(false);
                if !busy {
                    DestroyWindow(h);
                }
                0
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            WM_NCDESTROY => {
                if let Some(a) = app::take() {
                    for f in [a.font, a.bold, a.small, a.mono] {
                        DeleteObject(f);
                    }
                    DeleteObject(a.brush_input);
                }
                DefWindowProcW(h, m, w, l)
            }
            _ => DefWindowProcW(h, m, w, l),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_resize_invalidates_the_whole_content() {
        unsafe {
            let class = wide("FerrumRecoveryResizeTest");
            let wc = WNDCLASSW { lpfnWndProc: Some(preview_proc), lpszClassName: class.as_ptr(), ..zeroed() };
            RegisterClassW(&wc);
            let parent = CreateWindowExW(
                WS_EX_NOACTIVATE,
                wide("STATIC").as_ptr(),
                wide("").as_ptr(),
                WS_POPUP | WS_VISIBLE,
                -32000,
                -32000,
                500,
                400,
                null_mut(),
                null_mut(),
                null_mut(),
                null(),
            );
            let h = CreateWindowExW(
                0,
                class.as_ptr(),
                wide("").as_ptr(),
                WS_CHILD | WS_VISIBLE,
                0,
                0,
                400,
                300,
                parent,
                null_mut(),
                null_mut(),
                null(),
            );
            assert!(!h.is_null());
            ValidateRect(h, null());
            send(h, WM_SIZE, 0, 280 << 16 | 360);
            let mut dirty: RECT = zeroed();
            let invalidated = GetUpdateRect(h, &mut dirty, 0);
            let bounds = util::client(h);
            DestroyWindow(parent);
            UnregisterClassW(class.as_ptr(), null_mut());
            assert_ne!(invalidated, 0, "resizing must repaint old text positions, not only the exposed strip");
            assert_eq!((dirty.left, dirty.top, dirty.right, dirty.bottom), (bounds.left, bounds.top, bounds.right, bounds.bottom));
        }
    }
}
