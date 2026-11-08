//! Owner-data list views: only visible rows are formatted and painted.
use crate::app::{self, COLS, Col};
use crate::util::{send, wide};
use std::mem::zeroed;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::Controls::*;

pub const ID_FOLDERS: usize = 110;
pub const ID_FILES: usize = 111;

fn subitem_rect(h: HWND, row: usize, col: usize) -> RECT {
    let mut r = crate::util::rect(LVIR_BOUNDS as i32, col as i32, 0, 0);
    unsafe { send(h, LVM_GETSUBITEMRECT, row, (&mut r as *mut RECT) as isize) };
    // Subitem zero is the whole row in Win32, not the Name cell.
    if col == 0 {
        r.right = r.left + unsafe { send(h, LVM_GETCOLUMNWIDTH, 0, 0) } as i32;
    }
    r
}

unsafe extern "system" fn lv_subclass(h: HWND, msg: u32, w: usize, l: isize, _id: usize, _data: usize) -> isize {
    unsafe {
        if msg == windows_sys::Win32::UI::WindowsAndMessaging::WM_LBUTTONDOWN
            && app::with(|a| h == a.w.folders || h == a.w.files).unwrap_or(false)
        {
            let x = l as u16 as i16 as i32;
            let y = (l as u32 >> 16) as i16 as i32;
            let mut hit = LVHITTESTINFO { pt: POINT { x, y }, ..zeroed() };
            send(h, LVM_SUBITEMHITTEST, 0, (&mut hit as *mut LVHITTESTINFO) as isize);
            if hit.iItem >= 0 && hit.iSubItem == 0 {
                let row = hit.iItem as usize;
                let file_check = app::with(|a| {
                    let left = subitem_rect(h, row, 0).left;
                    h == a.w.files && x >= left + a.scale(5) && x < left + a.scale(22)
                })
                .unwrap_or(false);
                if file_check {
                    windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus(h);
                    app::with(|a| {
                        if let Some(&i) = a.rows.get(row) {
                            a.checked[i as usize] ^= true;
                            a.recount_checked();
                        }
                    });
                    InvalidateRect(h, &subitem_rect(h, row, 0), 0);
                    if let Some(h) = app::with(|a| a.w.main) {
                        InvalidateRect(h, std::ptr::null(), 0);
                    }
                    return 0;
                }
                let glyph = app::with(|a| {
                    if h != a.w.folders {
                        return None;
                    }
                    a.folder_rows.get(row).map(|r| {
                        let left = subitem_rect(h, row, 0).left + a.scale(4 + r.depth as i32 * 18);
                        r.kids && x >= left && x < left + a.scale(16)
                    })
                })
                .flatten()
                .unwrap_or(false);
                if glyph {
                    windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus(h);
                    select_folder(row);
                    toggle_folder(row);
                    return 0;
                }
            }
        }
        if msg == windows_sys::Win32::UI::WindowsAndMessaging::WM_KEYDOWN
            && [0x25, 0x27, 0x08, 0x0D].contains(&w)
            && app::with(|a| h == a.w.folders).unwrap_or(false)
        {
            folder_key(w);
            return 0;
        }
        if msg == windows_sys::Win32::UI::WindowsAndMessaging::WM_NOTIFY && (*(l as *const NMHDR)).code == NM_CUSTOMDRAW {
            let cd = &*(l as *const NMCUSTOMDRAW);
            if cd.dwDrawStage == CDDS_PREPAINT {
                return CDRF_NOTIFYITEMDRAW as isize;
            }
            if cd.dwDrawStage == CDDS_ITEMPREPAINT {
                if let Some(color) = app::with(|a| a.pal.text) {
                    SetTextColor(cd.hdc, color);
                }
                return CDRF_DODEFAULT as isize;
            }
        }
        windows_sys::Win32::UI::Shell::DefSubclassProc(h, msg, w, l)
    }
}

pub fn setup(h: HWND, folders: bool) {
    unsafe {
        windows_sys::Win32::UI::Shell::SetWindowSubclass(h, Some(lv_subclass), 1, 0);
        send(h, LVM_SETEXTENDEDLISTVIEWSTYLE, 0, (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_INFOTIP) as isize);
        let images = app::with(|a| a.images).unwrap_or(0);
        send(h, LVM_SETIMAGELIST, LVSIL_SMALL as usize, images);
        if folders {
            folder_columns(h);
        } else {
            columns(h);
        }
    }
}

pub fn folder_columns(h: HWND) {
    unsafe {
        while send(h, LVM_DELETECOLUMN, 0, 0) != 0 {}
        for (i, title) in [crate::lang::S::ColName, crate::lang::S::ColFiles].into_iter().enumerate() {
            let text = wide(crate::lang::t(title));
            let c = LVCOLUMNW {
                mask: LVCF_WIDTH | LVCF_TEXT | LVCF_FMT,
                cx: if i == 0 { 180 } else { 70 },
                fmt: if i == 0 { LVCFMT_LEFT } else { LVCFMT_RIGHT },
                pszText: text.as_ptr() as *mut u16,
                ..zeroed()
            };
            send(h, LVM_INSERTCOLUMNW, i, (&c as *const LVCOLUMNW) as isize);
        }
    }
}

pub fn columns(h: HWND) {
    unsafe {
        while send(h, LVM_DELETECOLUMN, 0, 0) != 0 {}
        for (i, &(_, title, width, right)) in COLS.iter().enumerate() {
            let mut text = wide(crate::lang::t(title));
            let mut c: LVCOLUMNW = zeroed();
            c.mask = LVCF_TEXT | LVCF_WIDTH | LVCF_FMT;
            c.pszText = text.as_mut_ptr();
            c.cx = app::with(|a| a.scale(width)).unwrap_or(width);
            c.fmt = if right { LVCFMT_RIGHT } else { LVCFMT_LEFT };
            send(h, LVM_INSERTCOLUMNW, i, (&c as *const LVCOLUMNW) as isize);
        }
    }
}

pub fn refresh() {
    if let Some((w, folders, files, folder)) =
        app::with(|a| (a.w, a.folder_rows.len(), a.rows.len(), a.folder_rows.iter().position(|r| r.node == a.folder)))
    {
        unsafe {
            send(w.folders, LVM_SETITEMCOUNT, folders, LVSICF_NOINVALIDATEALL as isize);
            send(w.files, LVM_SETITEMCOUNT, files, LVSICF_NOINVALIDATEALL as isize);
            let mut state: LVITEMW = zeroed();
            state.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
            send(w.folders, LVM_SETITEMSTATE, usize::MAX, (&state as *const LVITEMW) as isize);
            if let Some(row) = folder {
                state.state = LVIS_SELECTED | LVIS_FOCUSED;
                send(w.folders, LVM_SETITEMSTATE, row, (&state as *const LVITEMW) as isize);
            }
            InvalidateRect(w.folders, std::ptr::null(), 0);
            InvalidateRect(w.files, std::ptr::null(), 0);
            InvalidateRect(w.main, std::ptr::null(), 0);
        }
    }
    size_folders();
}

pub fn selected() -> Vec<usize> {
    let Some(h) = app::with(|a| a.w.files) else { return Vec::new() };
    let mut out = Vec::new();
    let mut i = -1isize;
    unsafe {
        loop {
            i = send(h, LVM_GETNEXTITEM, i as usize, LVNI_SELECTED as isize);
            if i < 0 {
                break;
            }
            out.push(i as usize);
        }
    }
    out
}

/// Row numbers are native control state. Invalidate them whenever the row mapping changes.
pub fn clear_selection() {
    if let Some(w) = app::with(|a| {
        a.preview_entry = None;
        a.preview_generation = a.preview_generation.wrapping_add(1);
        a.preview_bytes.clear();
        a.preview_image = None;
        a.preview_error = None;
        a.preview_notice = None;
        if let Some(c) = &a.preview_cancel {
            c.cancel();
        }
        a.w
    }) {
        unsafe {
            let mut item: LVITEMW = zeroed();
            item.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
            send(w.files, LVM_SETITEMSTATE, usize::MAX, (&item as *const LVITEMW) as isize);
            send(w.files, LVM_SETSELECTIONMARK, 0, -1);
            InvalidateRect(w.preview, std::ptr::null(), 0);
        }
    }
}

pub fn theme() {
    if let Some((w, p)) = app::with(|a| (a.w, a.pal)) {
        for h in [w.folders, w.files] {
            crate::theme::style_control(h, p.dark, crate::theme::Kind::Explorer);
            unsafe {
                send(h, LVM_SETBKCOLOR, 0, p.list as isize);
                send(h, LVM_SETTEXTBKCOLOR, 0, p.list as isize);
                send(h, LVM_SETTEXTCOLOR, 0, p.text as isize);
                let header = send(h, LVM_GETHEADER, 0, 0) as HWND;
                crate::theme::style_control(header, p.dark, crate::theme::Kind::ItemsView);
                let tip = send(h, LVM_GETTOOLTIPS, 0, 0) as HWND;
                if !tip.is_null() {
                    // A plain tooltip obeys explicit colours in both themes.
                    SetWindowTheme(tip, crate::util::wide("").as_ptr(), crate::util::wide("").as_ptr());
                    send(tip, TTM_SETTIPBKCOLOR, p.panel as usize, 0);
                    send(tip, TTM_SETTIPTEXTCOLOR, p.text as usize, 0);
                    send(tip, TTM_SETMAXTIPWIDTH, 0, 700);
                }
            }
        }
    }
}

fn draw_cell(a: &mut app::App, dc: HDC, mut r: RECT, row: usize, col: usize, folders: bool, selected: bool) -> isize {
    let p = a.pal;
    crate::util::fill(dc, &r, if selected { p.sel } else { p.list });
    unsafe { SelectObject(dc, a.font) };
    let color = if selected { p.sel_text } else { p.text };
    if col == 0 {
        let icon = a.icon(row, folders);
        let mut x = r.left + a.scale(5);
        let label;
        if folders {
            let Some(fr) = a.folder_rows.get(row) else { return CDRF_SKIPDEFAULT as isize };
            x += a.scale(fr.depth as i32 * 18);
            if fr.kids {
                let d = a.scale(9);
                let y = (r.top + r.bottom - d) / 2;
                let boxr = crate::util::rect(x + a.scale(3), y, x + a.scale(3) + d, y + d);
                crate::util::frame(dc, &boxr, if selected { p.sel_text } else { p.dim });
                let cx = (boxr.left + boxr.right) / 2;
                let cy = (boxr.top + boxr.bottom) / 2;
                crate::util::fill(dc, &crate::util::rect(boxr.left + 2, cy, boxr.right - 2, cy + 1), color);
                if !fr.expanded {
                    crate::util::fill(dc, &crate::util::rect(cx, boxr.top + 2, cx + 1, boxr.bottom - 2), color);
                }
            }
            x += a.scale(18);
            label = a.folder_label(fr);
        } else {
            let Some(&i) = a.rows.get(row) else { return CDRF_SKIPDEFAULT as isize };
            let d = a.scale(14);
            let top = (r.top + r.bottom - d) / 2;
            let mut cb = crate::util::rect(x, top, x + d, top + d);
            unsafe {
                DrawFrameControl(
                    dc,
                    &mut cb,
                    DFC_BUTTON,
                    DFCS_BUTTONCHECK | if a.checked.get(i as usize) == Some(&true) { DFCS_CHECKED } else { 0 },
                )
            };
            x = cb.right + a.scale(6);
            label = a.cell(row, Col::Name);
        }
        let (mut iw, mut ih) = (0, 0);
        unsafe {
            if a.images != 0 {
                ImageList_GetIconSize(a.images, &mut iw, &mut ih);
                if icon >= 0 {
                    ImageList_Draw(a.images, icon, dc, x, (r.top + r.bottom - ih) / 2, ILD_TRANSPARENT);
                }
            }
        }
        r.left = x + iw + a.scale(4);
        r.right -= a.scale(6);
        crate::util::text(dc, &label, &r, color, 0);
    } else {
        r.left += a.scale(6);
        r.right -= a.scale(6);
        let (label, right, color) = if folders {
            (a.folder_rows.get(row).map(|r| crate::lang::count(r.count as u64)).unwrap_or_default(), true, color)
        } else if let Some(&(column, _, _, right)) = COLS.get(col) {
            let color = if column == Col::State && !selected {
                a.rows.get(row).and_then(|&i| a.cat().map(|cat| a.state_color(cat.entries[i as usize].state))).unwrap_or(color)
            } else {
                color
            };
            (a.cell(row, column), right, color)
        } else {
            return CDRF_SKIPDEFAULT as isize;
        };
        crate::util::text(dc, &label, &r, color, if right { DT_RIGHT } else { 0 });
    }
    CDRF_SKIPDEFAULT as isize
}

fn select_folder(row: usize) {
    let change = app::with(|a| a.folder_rows.get(row).filter(|r| r.node != a.folder).map(|r| r.node)).flatten();
    if let Some(node) = change {
        clear_selection();
        app::with(|a| {
            a.folder = node;
            a.rebuild_rows();
        });
        refresh();
    }
}

fn toggle_folder(row: usize) {
    app::with(|a| {
        if let Some(r) = a.folder_rows.get(row).copied().filter(|r| r.kids) {
            if !a.expanded.remove(&r.node) {
                a.expanded.insert(r.node);
            }
            a.rebuild_tree();
        }
    });
    refresh();
}

fn folder_key(key: usize) {
    let action = app::with(|a| {
        let row = a.folder_rows.iter().position(|r| r.node == a.folder)?;
        folder_action(&a.folder_rows, row, key)
    })
    .flatten();
    if let Some((row, toggle)) = action {
        if toggle {
            toggle_folder(row);
        } else {
            select_folder(row);
        }
        if let Some(h) = app::with(|a| a.w.folders) {
            unsafe {
                send(h, LVM_ENSUREVISIBLE, row, 0);
            }
        }
    }
}

fn folder_action(rows: &[app::FolderRow], row: usize, key: usize) -> Option<(usize, bool)> {
    let r = *rows.get(row)?;
    match key {
        0x27 if r.kids && !r.expanded => Some((row, true)),
        0x27 if r.expanded => rows.get(row + 1).filter(|r| r.depth > rows[row].depth).map(|_| (row + 1, false)),
        0x25 | 0x08 if r.kids && r.expanded => Some((row, true)),
        0x25 | 0x08 => (0..row).rev().find(|&i| rows[i].depth < r.depth).map(|i| (i, false)),
        0x0D if r.kids => Some((row, true)),
        _ => None,
    }
}

pub fn folder_widths(dpi: u32, client_width: i32, name_width: i32) -> (i32, i32) {
    let count = (70 * dpi as i32 / 96).min(client_width / 3);
    ((client_width - count - 4).max(name_width * dpi as i32 / 96), count)
}

pub fn size_folders() {
    if let Some((h, dpi, name)) = app::with(|a| (a.w.folders, a.dpi, a.folder_width)) {
        let (name, count) = folder_widths(dpi, crate::util::client(h).right, name);
        unsafe {
            send(h, LVM_SETCOLUMNWIDTH, 0, name as isize);
            send(h, LVM_SETCOLUMNWIDTH, 1, count as isize);
        }
    }
}

/// `p` is the NMHDR pointer passed with WM_NOTIFY by our controls.
pub unsafe fn notify(p: isize) -> Option<isize> {
    unsafe {
        let hdr = &*(p as *const NMHDR);
        let folders = hdr.idFrom == ID_FOLDERS;
        if !folders && hdr.idFrom != ID_FILES {
            return None;
        }
        match hdr.code {
            LVN_GETDISPINFOW => {
                let n = &mut *(p as *mut NMLVDISPINFOW);
                if n.item.mask & LVIF_TEXT != 0 && !n.item.pszText.is_null() && n.item.cchTextMax > 0 {
                    let s = app::with(|a| {
                        if folders {
                            a.folder_rows
                                .get(n.item.iItem as usize)
                                .map(|r| if n.item.iSubItem == 0 { a.folder_label(r) } else { crate::lang::count(r.count as u64) })
                                .unwrap_or_default()
                        } else {
                            COLS.get(n.item.iSubItem as usize).map(|c| a.cell(n.item.iItem as usize, c.0)).unwrap_or_default()
                        }
                    })
                    .unwrap_or_default();
                    let w: Vec<_> = s.encode_utf16().take(n.item.cchTextMax.saturating_sub(1) as usize).collect();
                    std::ptr::copy_nonoverlapping(w.as_ptr(), n.item.pszText, w.len());
                    *n.item.pszText.add(w.len()) = 0;
                }
                Some(0)
            }
            NM_CUSTOMDRAW => {
                let c = &mut *(p as *mut NMLVCUSTOMDRAW);
                match c.nmcd.dwDrawStage {
                    CDDS_PREPAINT => Some(CDRF_NOTIFYITEMDRAW as isize),
                    CDDS_ITEMPREPAINT => Some(CDRF_NOTIFYSUBITEMDRAW as isize),
                    st if st == CDDS_ITEMPREPAINT | CDDS_SUBITEM => {
                        let row = c.nmcd.dwItemSpec;
                        let col = c.iSubItem as usize;
                        let h = hdr.hwndFrom;
                        let r = subitem_rect(h, row, col);
                        let selected = send(h, LVM_GETITEMSTATE, row, LVIS_SELECTED as isize) != 0;
                        let saved = SaveDC(c.nmcd.hdc);
                        IntersectClipRect(c.nmcd.hdc, r.left, r.top, r.right, r.bottom);
                        let result = app::with(|a| draw_cell(a, c.nmcd.hdc, r, row, col, folders, selected));
                        RestoreDC(c.nmcd.hdc, saved);
                        result.or(Some(CDRF_DODEFAULT as isize))
                    }
                    _ => Some(CDRF_DODEFAULT as isize),
                }
            }
            LVN_COLUMNCLICK if !folders => {
                let n = &*(p as *const NMLISTVIEW);
                if let Some(&(col, _, _, _)) = COLS.get(n.iSubItem as usize) {
                    clear_selection();
                    app::with(|a| {
                        a.sort = (col, a.sort.0 == col && !a.sort.1);
                        a.sort_rows();
                    });
                    refresh();
                }
                Some(0)
            }
            NM_CLICK | NM_DBLCLK => {
                let n = &*(p as *const NMITEMACTIVATE);
                if n.iItem >= 0 {
                    let row = n.iItem as usize;
                    if folders {
                        let toggle =
                            app::with(|a| a.folder_rows.get(row).map(|r| r.kids && hdr.code == NM_DBLCLK)).flatten().unwrap_or(false);
                        if toggle {
                            toggle_folder(row);
                        }
                    }
                }
                Some(0)
            }
            LVN_ITEMCHANGED if folders => {
                let n = &*(p as *const NMLISTVIEW);
                if n.iItem >= 0 && n.uNewState & LVIS_SELECTED != 0 && n.uOldState & LVIS_SELECTED == 0 {
                    select_folder(n.iItem as usize);
                }
                Some(0)
            }
            LVN_GETINFOTIPW => {
                let tip = &mut *(p as *mut NMLVGETINFOTIPW);
                let text = app::with(|a| {
                    if folders {
                        a.folder_rows
                            .get(tip.iItem as usize)
                            .map(|r| if r.node == app::LOST { crate::lang::t(crate::lang::S::LostPath).into() } else { a.folder_label(r) })
                            .unwrap_or_default()
                    } else {
                        a.rows.get(tip.iItem as usize).and_then(|&i| a.cat().map(|cat| cat.path(i))).unwrap_or_default()
                    }
                })
                .unwrap_or_default();
                if !tip.pszText.is_null() && tip.cchTextMax > 0 {
                    let w: Vec<_> = text.encode_utf16().take(tip.cchTextMax as usize - 1).collect();
                    std::ptr::copy_nonoverlapping(w.as_ptr(), tip.pszText, w.len());
                    *tip.pszText.add(w.len()) = 0;
                }
                Some(0)
            }
            LVN_ITEMCHANGED if !folders => {
                let n = &*(p as *const NMLISTVIEW);
                if n.iItem >= 0 && n.uNewState & LVIS_SELECTED != 0 && n.uOldState & LVIS_SELECTED == 0 {
                    crate::cmds::select_preview(n.iItem as usize);
                }
                Some(0)
            }
            LVN_KEYDOWN if !folders => {
                let n = &*(p as *const NMLVKEYDOWN);
                if n.wVKey == 0x20 {
                    crate::cmds::check_selected(None);
                }
                Some(0)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    #[test]
    fn filtered_leaf_can_return_to_parent() {
        let rows = [
            app::FolderRow { node: app::ALL, depth: 0, expanded: true, kids: true, count: 1 },
            app::FolderRow { node: 3, depth: 1, expanded: true, kids: false, count: 1 },
        ];
        for key in [0x25, 0x08] {
            assert_eq!(folder_action(&rows, 1, key), Some((0, false)));
        }
    }

    #[test]
    fn deep_folders_can_scroll_to_their_names() {
        let required = 4 + 7 * 18 + 18 + 16 + 4 + 80;
        let (name, _) = folder_widths(96, 178, required);
        assert!(name >= required);
    }

    #[test]
    fn first_cell_does_not_include_other_columns() {
        unsafe {
            let init = INITCOMMONCONTROLSEX { dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_LISTVIEW_CLASSES };
            InitCommonControlsEx(&init);
            let parent = CreateWindowExW(
                0,
                wide("STATIC").as_ptr(),
                wide("").as_ptr(),
                WS_OVERLAPPED,
                0,
                0,
                640,
                200,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            let list = CreateWindowExW(
                0,
                wide("SysListView32").as_ptr(),
                wide("").as_ptr(),
                WS_CHILD | WS_VISIBLE | LVS_REPORT | LVS_OWNERDATA,
                0,
                0,
                300,
                160,
                parent,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            assert!(!list.is_null());
            for (i, width) in [120, 240, 90].into_iter().enumerate() {
                let c = LVCOLUMNW { mask: LVCF_WIDTH, cx: width, ..zeroed() };
                send(list, LVM_INSERTCOLUMNW, i, (&c as *const LVCOLUMNW) as isize);
            }
            send(list, LVM_SETITEMCOUNT, 2, 0);
            let name = subitem_rect(list, 0, 0);
            let path = subitem_rect(list, 0, 1);
            assert_eq!(name.right, path.left, "painting Name must not erase Path");
            assert_eq!(name.right - name.left, 120);
            send(list, LVM_SCROLL, 64, 0);
            let name = subitem_rect(list, 0, 0);
            let path = subitem_rect(list, 0, 1);
            assert!(name.left < 0, "fixture must scroll horizontally");
            assert_eq!(name.right, path.left);
            assert_eq!(name.right - name.left, 120);
            send(list, LVM_SETCOLUMNWIDTH, 0, 12);
            let name = subitem_rect(list, 0, 0);
            assert_eq!(name.right - name.left, 12, "narrow Name columns must clip icons too");
            DestroyWindow(parent);
        }
    }
}
