//! Window state and the views computed from a scan: which entries pass the filters, the
//! folder tree with the number of found files under each folder, the file list of the
//! selected folder, its sort order, and the checked files.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Instant;

use fr_core::filter::{Filter, Pattern};
use fr_core::model::{Catalog, NONE, State, ef};
use fr_core::part::Part;
use fr_core::source::Source;
use fr_core::types::Group;
use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::{HBRUSH, HFONT};
use windows_sys::Win32::UI::WindowsAndMessaging::HACCEL;

use crate::lang::{S, t};
use crate::settings::Settings;
use crate::theme::Palette;

/// Child windows.
#[derive(Clone, Copy)]
pub struct Wnd {
    pub main: HWND,
    pub source: HWND,
    pub scan: HWND,
    pub deep: HWND,
    pub check: HWND,
    pub recover: HWND,
    pub search: HWND,
    pub types: HWND,
    pub states: HWND,
    pub folders: HWND,
    pub files: HWND,
    pub preview: HWND,
}

/// A source the combo box offers.
#[derive(Clone, Debug)]
pub struct SourceItem {
    /// `C:`, `disk1`, `shadow3`, an image path, or empty for the "open..." items.
    pub spec: String,
    pub label: String,
    pub size: Option<u64>,
    pub kind: SourceKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Volume,
    Disk,
    Shadow,
    Image,
    Saved,
    OpenImage,
    OpenSaved,
}

impl SourceItem {
    pub fn display(&self) -> String {
        match self.kind {
            SourceKind::Volume => format!("{} {}", t(S::Volume), self.spec),
            SourceKind::Disk => {
                format!("{} {} ({})", t(S::Disk), self.spec.trim_start_matches("disk"), crate::lang::size(self.size.unwrap_or(0)))
            }
            SourceKind::Shadow => format!("{} {}", t(S::ShadowCopy), self.spec.trim_start_matches("shadow")),
            SourceKind::OpenImage => t(S::OpenImage).into(),
            SourceKind::OpenSaved => t(S::OpenSaved).into(),
            _ => self.label.clone(),
        }
    }
}

/// A scan the window shows; shared with the preview and recovery threads.
pub struct Loaded {
    pub spec: String,
    pub src: Source,
    pub part: Part,
    pub scanned: fr_scan::Scanned,
}

impl Loaded {
    pub fn cat(&self) -> &Catalog {
        &self.scanned.catalog
    }
}

pub enum Status {
    Message(S),
    Scanned(std::time::Duration),
    Error(fr_core::error::Error),
}

impl Status {
    pub fn text(&self) -> String {
        match self {
            Self::Message(id) => t(*id).into(),
            Self::Scanned(took) => format!("{} {}", t(S::Done), crate::lang::seconds(*took)),
            Self::Error(e) => crate::lang::error(e),
        }
    }
}

pub enum Busy {
    Save,
    Scan { progress: fr_scan::Progress, started: Instant, spec: String },
    Recover { done: Arc<AtomicU64>, total: u64, bytes: Arc<AtomicU64>, started: Instant },
    Image { progress: Arc<std::sync::Mutex<fr_core::image::Progress>> },
}

/// Pseudo folders of the tree.
pub const ALL: u32 = u32::MAX;
pub const LOST: u32 = u32::MAX - 1;

#[derive(Clone, Copy, Debug)]
pub struct FolderRow {
    pub node: u32,
    pub depth: u16,
    pub expanded: bool,
    pub kids: bool,
    pub count: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Col {
    Name,
    Path,
    Size,
    State,
    Modified,
    Deleted,
}

pub const COLS: [(Col, S, i32, bool); 6] = [
    (Col::Name, S::ColName, 240, false),
    (Col::Path, S::ColPath, 300, false),
    (Col::Size, S::ColSize, 90, true),
    (Col::State, S::ColState, 130, false),
    (Col::Modified, S::ColModified, 130, false),
    (Col::Deleted, S::ColDeleted, 130, false),
];

/// The state filter of the combo box.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateChoice {
    Any,
    Recoverable,
    One(State),
}

pub const STATE_CHOICES: [StateChoice; 10] = [
    StateChoice::Any,
    StateChoice::Recoverable,
    StateChoice::One(State::Good),
    StateChoice::One(State::Partial),
    StateChoice::One(State::Overwritten),
    StateChoice::One(State::Zeroed),
    StateChoice::One(State::Mismatch),
    StateChoice::One(State::Unknown),
    StateChoice::One(State::NoData),
    StateChoice::One(State::Encrypted),
];

#[derive(Clone, Copy, Default)]
pub struct Layout {
    pub menus: [(RECT, usize); 3],
    pub top: RECT,
    pub status_line: RECT,
    pub progress: RECT,
    pub brand: RECT,
    pub filter_row: RECT,
    pub folders: RECT,
    pub files: RECT,
    pub preview: RECT,
    pub split1: RECT,
    pub split2: RECT,
    pub statusbar: RECT,
    pub source: RECT,
    pub scan: RECT,
    pub recover: RECT,
    pub deep: RECT,
    pub check: RECT,
    pub search: RECT,
    pub types: RECT,
    pub states: RECT,
    pub headings: [RECT; 3],
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Drag {
    Tree { offset: i32 },
    Preview { offset: i32 },
}

pub struct App {
    pub w: Wnd,
    pub dpi: u32,
    pub font: HFONT,
    pub bold: HFONT,
    pub small: HFONT,
    pub mono: HFONT,
    pub pal: Palette,
    pub brush_input: HBRUSH,
    pub accel: HACCEL,
    pub settings: Settings,
    pub layout: Layout,
    pub menu_widths: [i32; 3],
    pub drag: Option<Drag>,
    pub sources: Vec<SourceItem>,
    pub selected_source: String,
    pub busy: Option<Busy>,
    pub status: Status,
    pub data: Option<Arc<Loaded>>,
    /// Per entry: passes the filters (files only).
    pub accepted: Vec<bool>,
    /// Per entry: accepted files below it (folders).
    pub counts: Vec<u32>,
    pub count_all: u32,
    pub count_lost: u32,
    /// Folders with accepted files below them, by parent (`ALL` for the top level, `LOST`
    /// for folders whose parent is unknown).
    pub kids: HashMap<u32, Vec<u32>>,
    pub expanded: HashSet<u32>,
    pub folder_rows: Vec<FolderRow>,
    pub folder_width: i32,
    pub folder: u32,
    pub rows: Vec<u32>,
    pub checked: Vec<bool>,
    pub checked_count: u64,
    pub checked_bytes: u64,
    pub sort: (Col, bool),
    pub search: String,
    pub advanced: crate::filters::Advanced,
    pub group: Option<Group>,
    pub state_choice: StateChoice,
    pub preview_entry: Option<u32>,
    pub preview_bytes: Vec<u8>,
    pub preview_image: Option<crate::preview::Image>,
    pub preview_mode: crate::preview::Mode,
    pub preview_error: Option<fr_core::error::Error>,
    pub preview_notice: Option<S>,
    pub preview_cancel: Option<Arc<fr_core::control::Control>>,
    pub preview_running: bool,
    pub preview_generation: u64,
    pub events: std::sync::mpsc::Receiver<crate::engine::Event>,
    pub sender: std::sync::mpsc::Sender<crate::engine::Event>,
    pub control: Option<Arc<fr_core::control::Control>>,
    pub closing: bool,
    pub low_priority: bool,
    pub images: isize,
    pub icons: HashMap<String, i32>,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

pub fn install(a: App) {
    APP.with(|c| *c.borrow_mut() = Some(a));
}

pub fn take() -> Option<App> {
    APP.with(|c| c.borrow_mut().take())
}

/// Run `f` on the app state. Never call back into `with` from inside `f` (Win32 calls
/// that send messages synchronously can re-enter: keep them outside).
pub fn with<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|c| c.try_borrow_mut().ok().and_then(|mut b| b.as_mut().map(f)))
}

impl App {
    pub fn scale(&self, v: i32) -> i32 {
        v * self.dpi as i32 / 96
    }

    pub fn cat(&self) -> Option<&Catalog> {
        self.data.as_deref().map(|d| d.cat())
    }

    /// The filter the controls describe.
    pub fn filter(&self) -> Filter {
        let mut f = Filter {
            existing: self.settings.existing,
            system: self.settings.system,
            empty: self.settings.empty,
            traces: true,
            streams: true,
            ..Default::default()
        };
        for part in self.search.split(';').map(str::trim).filter(|p| !p.is_empty()) {
            f.name.push(Pattern::new(part));
        }
        if let Some(g) = self.group {
            f.groups.push(g);
        }
        match self.state_choice {
            StateChoice::Any => {}
            StateChoice::Recoverable => f.states = vec![State::Good, State::Partial],
            StateChoice::One(s) => f.states = vec![s],
        }
        let _ = self.advanced.apply(&mut f);
        f
    }

    /// Recompute what passes the filters and the folder counts.
    pub fn refilter(&mut self) {
        let Some(data) = self.data.clone() else {
            self.accepted.clear();
            self.counts.clear();
            self.kids.clear();
            self.folder_rows.clear();
            self.rows.clear();
            return;
        };
        let cat = data.cat();
        let n = cat.entries.len();
        let f = self.filter();
        self.accepted = (0..n as u32).map(|i| !cat.entries[i as usize].is_dir() && f.accepts(cat, i)).collect();
        self.counts = vec![0; n];
        self.count_all = 0;
        self.count_lost = 0;
        for i in 0..n {
            if !self.accepted[i] {
                continue;
            }
            self.count_all += 1;
            let mut cur = cat.entries[i].parent;
            let mut top_orphan = cat.entries[i].flags & ef::ORPHAN != 0 && cur == NONE;
            for _ in 0..1024 {
                if cur == NONE {
                    break;
                }
                self.counts[cur as usize] += 1;
                let e = &cat.entries[cur as usize];
                if e.parent == NONE {
                    top_orphan = e.flags & ef::ORPHAN != 0;
                }
                cur = e.parent;
            }
            if top_orphan {
                self.count_lost += 1;
            }
        }
        // Folder children, by name.
        let root = cat.find(cat.fs.root).unwrap_or(NONE);
        let mut kids: HashMap<u32, Vec<u32>> = HashMap::new();
        for i in 0..n as u32 {
            let e = &cat.entries[i as usize];
            if !e.is_dir() || self.counts[i as usize] == 0 || i == root {
                continue;
            }
            let key = if e.parent == NONE {
                if e.flags & ef::ORPHAN != 0 { LOST } else { ALL }
            } else if e.parent == root {
                ALL
            } else {
                e.parent
            };
            kids.entry(key).or_default().push(i);
        }
        for v in kids.values_mut() {
            v.sort_by_cached_key(|&i| cat.name(i).to_lowercase());
        }
        self.kids = kids;
        if (self.folder == LOST && self.count_lost == 0)
            || (self.folder < LOST && self.counts.get(self.folder as usize).copied().unwrap_or(0) == 0)
        {
            self.folder = ALL;
        }
        self.rebuild_tree();
        self.rebuild_rows();
    }

    /// Visible rows of the folder tree.
    pub fn rebuild_tree(&mut self) {
        let open = self.expanded.contains(&ALL);
        let mut rows = vec![FolderRow {
            node: ALL,
            depth: 0,
            expanded: open,
            kids: !self.kids.is_empty() || self.count_lost > 0,
            count: self.count_all,
        }];
        let mut stack: Vec<(u32, u16)> = Vec::new();
        let push_kids = |stack: &mut Vec<(u32, u16)>, of: u32, depth: u16, kids: &HashMap<u32, Vec<u32>>| {
            if let Some(v) = kids.get(&of) {
                for &k in v.iter().rev() {
                    stack.push((k, depth));
                }
            }
        };
        if open && self.count_lost > 0 {
            stack.push((LOST, 1));
        }
        if open {
            push_kids(&mut stack, ALL, 1, &self.kids);
        }
        while let Some((node, depth)) = stack.pop() {
            let expanded = self.expanded.contains(&node);
            let count = if node == LOST { self.count_lost } else { self.counts[node as usize] };
            rows.push(FolderRow { node, depth, expanded, kids: self.kids.contains_key(&node), count });
            if expanded {
                push_kids(&mut stack, node, depth + 1, &self.kids);
            }
        }
        let depth = rows.iter().map(|r| r.depth).max().unwrap_or(0) as i32;
        self.folder_width = 4 + depth * 18 + 18 + 16 + 4 + 80;
        self.folder_rows = rows;
    }

    /// Files of the selected folder (and below), sorted.
    pub fn rebuild_rows(&mut self) {
        let Some(data) = self.data.clone() else { return };
        let cat = data.cat();
        let folder = self.folder;
        let mut rows: Vec<u32> = Vec::new();
        for i in 0..cat.entries.len() as u32 {
            if !self.accepted[i as usize] {
                continue;
            }
            let inside = match folder {
                ALL => true,
                LOST => {
                    let mut cur = i;
                    for _ in 0..1024 {
                        let p = cat.entries[cur as usize].parent;
                        if p == NONE {
                            break;
                        }
                        cur = p;
                    }
                    cat.entries[cur as usize].flags & ef::ORPHAN != 0
                }
                d => {
                    let mut cur = cat.entries[i as usize].parent;
                    let mut hit = false;
                    for _ in 0..1024 {
                        if cur == NONE {
                            break;
                        }
                        if cur == d {
                            hit = true;
                            break;
                        }
                        cur = cat.entries[cur as usize].parent;
                    }
                    hit
                }
            };
            if inside {
                rows.push(i);
            }
        }
        self.rows = rows;
        self.sort_rows();
    }

    pub fn sort_rows(&mut self) {
        let Some(data) = self.data.clone() else { return };
        let cat = data.cat();
        let (col, desc) = self.sort;
        let rows = &mut self.rows;
        match col {
            Col::Name => rows.sort_by_cached_key(|&i| cat.name(i).to_lowercase()),
            Col::Path => rows.sort_by_cached_key(|&i| cat.path(i).to_lowercase()),
            Col::Size => rows.sort_by_key(|&i| cat.entries[i as usize].size),
            Col::State => rows.sort_by_key(|&i| cat.entries[i as usize].state),
            Col::Modified => rows.sort_by_key(|&i| cat.entries[i as usize].modified),
            Col::Deleted => rows.sort_by_key(|&i| cat.entries[i as usize].deleted_at),
        }
        if desc {
            rows.reverse();
        }
    }

    /// Text of a cell of the file list.
    pub fn cell(&self, row: usize, col: Col) -> String {
        let (Some(cat), Some(&i)) = (self.cat(), self.rows.get(row)) else { return String::new() };
        let e = &cat.entries[i as usize];
        match col {
            Col::Name => crate::lang::entry_name(cat, i).into_owned(),
            Col::Path => {
                let p = crate::lang::path(cat, i);
                match p.rfind('\\') {
                    Some(k) if k > 0 => p[..k].to_owned(),
                    _ => "\\".into(),
                }
            }
            Col::Size => crate::lang::size(e.size),
            Col::State => crate::lang::state_name(e.state).to_string(),
            Col::Modified => crate::util::filetime(e.modified),
            Col::Deleted => {
                if e.flags & ef::RECYCLED != 0 {
                    t(S::InRecycleBin).to_string()
                } else {
                    crate::util::filetime(e.deleted_at)
                }
            }
        }
    }

    pub fn state_color(&self, s: State) -> u32 {
        match s {
            State::Good => self.pal.good,
            State::Partial => self.pal.warn,
            State::Overwritten | State::Mismatch | State::Zeroed => self.pal.bad,
            State::NoData | State::Encrypted | State::Unknown => self.pal.dim,
        }
    }

    pub fn folder_label(&self, r: &FolderRow) -> String {
        match r.node {
            ALL => t(S::AllFound).to_string(),
            LOST => t(S::LostFiles).to_string(),
            d => self.cat().map(|c| crate::lang::entry_name(c, d).into_owned()).unwrap_or_default(),
        }
    }

    /// Same extension cache as FerrumTree; file attributes avoid opening recovered paths.
    pub fn icon(&mut self, row: usize, folders: bool) -> i32 {
        let key = if folders {
            "/".to_string()
        } else {
            let Some(&i) = self.rows.get(row) else { return -1 };
            let Some(cat) = self.cat() else { return -1 };
            let name = cat.name(i);
            name.rfind('.').filter(|&p| p > 0).map(|p| name[p..].to_ascii_lowercase()).unwrap_or_default()
        };
        if let Some(&i) = self.icons.get(&key) {
            return i;
        }
        use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
        use windows_sys::Win32::UI::Shell::*;
        let folder = key == "/";
        let probe = crate::util::wide(&if folder { "folder".into() } else { format!("file{key}") });
        let mut info: SHFILEINFOW = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            SHGetFileInfoW(
                probe.as_ptr(),
                if folder { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL },
                &mut info,
                size_of::<SHFILEINFOW>() as u32,
                SHGFI_SYSICONINDEX | SHGFI_SMALLICON | SHGFI_USEFILEATTRIBUTES,
            )
        };
        let icon = if ok == 0 { -1 } else { info.iIcon };
        self.icons.insert(key, icon);
        icon
    }

    /// Entries the recovery takes: the checked ones, or else the selected rows.
    pub fn chosen(&self, selected: &[usize]) -> Vec<u32> {
        let checked: Vec<u32> = (0..self.checked.len() as u32).filter(|&i| self.checked[i as usize]).collect();
        if !checked.is_empty() {
            return checked;
        }
        selected.iter().filter_map(|&r| self.rows.get(r).copied()).collect()
    }

    pub fn checked_summary(&self) -> (u64, u64) {
        (self.checked_count, self.checked_bytes)
    }

    pub fn recount_checked(&mut self) {
        let Some(cat) = self.cat() else {
            self.checked_count = 0;
            self.checked_bytes = 0;
            return;
        };
        let mut n = 0;
        let mut bytes = 0;
        for (i, &c) in self.checked.iter().enumerate() {
            if c {
                n += 1;
                bytes += cat.entries[i].size;
            }
        }
        self.checked_count = n;
        self.checked_bytes = bytes;
    }
}
