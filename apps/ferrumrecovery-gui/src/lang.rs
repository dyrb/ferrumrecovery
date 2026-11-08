//! UI strings (English and Russian) and number formatting.
//!
//! The language follows Windows unless chosen in the View menu; it can change while the
//! program runs, so every text is looked up when it is drawn.

pub use crate::diagnostics::error;

use std::sync::atomic::{AtomicU8, Ordering};

use fr_core::model::State;
use fr_core::types::Group;
use windows_sys::Win32::Globalization::GetUserDefaultUILanguage;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    En,
    Ru,
}

/// 0: as in Windows, 1: English, 2: Russian.
static CHOICE: AtomicU8 = AtomicU8::new(0);

fn system_lang() -> Lang {
    // SAFETY: plain query. Primary language 0x19 is Russian.
    if unsafe { GetUserDefaultUILanguage() } & 0x3FF == 0x19 { Lang::Ru } else { Lang::En }
}

/// The name of a language in that language, as language menus show it.
pub fn native_name(l: Lang) -> &'static str {
    match l {
        Lang::En => "English",
        Lang::Ru => "Русский",
    }
}

/// `None`: as in Windows.
pub fn set_lang(l: Option<Lang>) {
    CHOICE.store(
        match l {
            None => 0,
            Some(Lang::En) => 1,
            Some(Lang::Ru) => 2,
        },
        Ordering::Relaxed,
    );
}

pub fn lang() -> Lang {
    match CHOICE.load(Ordering::Relaxed) {
        1 => Lang::En,
        2 => Lang::Ru,
        _ => system_lang(),
    }
}

macro_rules! strings {
    ($($id:ident = $en:literal, $ru:literal;)*) => {
        #[derive(Clone, Copy, Debug)]
        pub enum S { $($id),* }
        pub fn t(s: S) -> &'static str {
            match (lang(), s) {
                $((Lang::En, S::$id) => $en, (Lang::Ru, S::$id) => $ru,)*
            }
        }
    };
}

strings! {
    DiskImages = "Disk images", "Образы дисков";
    AllFiles = "All files", "Все файлы";
    Checking = "Checking data", "Проверка данных";
    Unknown = "Undetermined", "Не определено";
    DescUnknown = "Allocation metadata could not be read reliably. The data location is known; recovery can be attempted.", "Не удалось надёжно прочитать сведения о занятых кластерах. Расположение данных известно; можно попробовать восстановление.";
    DescSignature = "Found by its contents. Deletion status and file integrity are not verified.", "Найден по содержимому. Факт удаления и целостность файла не подтверждены.";
    DescHistorical = "Found in historical file-system metadata. Current allocation and file integrity are not verified.", "Найден в метаданных прежней файловой системы. Текущее размещение и целостность файла не подтверждены.";
    FullRange = "Entire source", "Весь источник";
    CreateImage = "Create source image...", "Создать образ источника...";
    Unreadable = "Unreadable", "Не прочитано";
    Remaining = "Not copied", "Не скопировано";
    ImageDone = "Image operation finished", "Создание образа завершено";
    ImagePickSource = "Choose a drive, disk or image in the source list first.", "Сначала выберите том, диск или образ в списке источников.";
    Filters = "Filters...", "Фильтры...";
    PathMask = "Path mask (; separates alternatives)", "Маска пути (; разделяет варианты)";
    MinSize = "Minimum size (B, KB, MB, GB)", "Минимальный размер (B, KB, MB, GB)";
    MaxSize = "Maximum size (B, KB, MB, GB)", "Максимальный размер (B, KB, MB, GB)";
    After = "Modified from (YYYY-MM-DD, local)", "Изменён с (ГГГГ-ММ-ДД, местное)";
    Before = "Modified before (exclusive, local)", "Изменён до (не включая, местное)";
    DeletedAfter = "Deleted from (YYYY-MM-DD, local)", "Удалён с (ГГГГ-ММ-ДД, местное)";
    DeletedBefore = "Deleted before (exclusive, local)", "Удалён до (не включая, местное)";
    Apply = "Apply", "Применить";
    Cancel = "Cancel", "Отмена";
    InvalidFilter = "Check sizes and dates. The upper date is exclusive and must be later than the lower date.", "Проверьте размеры и даты. Верхняя дата не включается и должна быть позже нижней.";
    Image = "Image", "Изображение";
    MenuFile = "File", "Файл";
    MenuView = "View", "Вид";
    MenuHelp = "Help", "Справка";
    Source = "Source:", "Источник:";
    Scan = "Scan", "Поиск";
    Stop = "Stop", "Остановить";
    Recover = "Recover", "Восстановить";
    Deep = "Deep search", "Глубокий поиск";
    Quick = "Quick search", "Быстрый поиск";
    Full = "Full search", "Полный поиск";
    ScanMode = "Search mode", "Режим поиска";
    ReadingFull = "Reading full range", "Чтение всей области";
    ReadingLayouts = "Analyzing file systems", "Анализ файловых систем";
    ReadingFormats = "Analyzing file formats", "Анализ форматов файлов";
    BuildingResults = "Assembling results", "Подготовка результатов";
    Check = "Check data", "Проверять данные";
    OpenImage = "Open disk image...", "Открыть образ диска...";
    OpenSaved = "Open saved scan...", "Открыть сохранённый поиск...";
    SaveScan = "Save scan...", "Сохранить результаты поиска...";
    ExportCsv = "Export list to CSV...", "Экспортировать список в CSV...";
    RecoverChecked = "Recover checked files...", "Восстановить отмеченные файлы...";
    Exit = "Exit", "Выход";
    ThemeMenu = "Theme", "Тема";
    ThemeSystem = "As in Windows", "Как в Windows";
    ThemeLight = "Light", "Светлая";
    ThemeDark = "Dark", "Тёмная";
    LangMenu = "Language", "Язык";
    LangSystem = "As in Windows", "Как в Windows";
    ShowExisting = "Include files that are not deleted (next scan)", "Включать неудалённые файлы (при следующем поиске)";
    ShowSystem = "Show system files", "Показывать системные файлы";
    ShowEmpty = "Show empty files", "Показывать пустые файлы";
    FindTraces = "Find names without data (next scan)", "Искать имена без данных (при следующем поиске)";
    FindStreams = "Find data streams (next scan)", "Искать потоки данных (при следующем поиске)";
    About = "About FerrumRecovery", "О программе";
    AboutText = "Recovers deleted files from NTFS, FAT and exFAT drives, disks and disk images.", "Восстановление удалённых файлов с дисков NTFS, FAT и exFAT, физических дисков и образов.";
    License = "License: MIT or Apache-2.0", "Лицензия: MIT или Apache-2.0";
    SearchCue = "Name or mask, for example *.jpg", "Имя или маска, например *.jpg";
    TypeAll = "All types", "Все типы";
    Photos = "Photos", "Фото";
    Video = "Video", "Видео";
    Audio = "Audio", "Аудио";
    Documents = "Documents", "Документы";
    Archives = "Archives", "Архивы";
    Mail = "Mail", "Почта";
    Other = "Other", "Другое";
    StateAny = "Any state", "Любое состояние";
    StateRecoverable = "Good or partial", "Хорошее или частичное";
    Good = "Good", "Хорошее";
    Partial = "Partial", "Частичное";
    Overwritten = "Overwritten", "Перезаписан";
    Zeroed = "Zero-filled samples", "Нулевые пробы";
    NoData = "No data", "Нет данных";
    Encrypted = "Encrypted", "Зашифрован";
    Mismatch = "Unexpected signature", "Сигнатура не совпадает";
    ColName = "Name", "Имя";
    Folders = "Folders", "Папки";
    Files = "Files", "Файлы";
    Preview = "Preview", "Предпросмотр";
    ColFiles = "Files", "Файлов";
    ColPath = "Path", "Путь";
    ColSize = "Size", "Размер";
    ColState = "State", "Состояние";
    ColModified = "Modified", "Изменён";
    ColDeleted = "Deleted", "Удалён";
    AllFound = "All found", "Все найденные";
    LostFiles = "Lost files", "Потерянные файлы";
    FoundMft = "Found MFT records", "Найденные записи MFT";
    FoundSignature = "Found by signature", "Найдено по сигнатуре";
    Folder = "Folder", "Папка";
    Reading = "Reading", "Чтение";
    ReadingFree = "Reading free space", "Чтение свободного места";
    Done = "Scan completed in", "Поиск выполнен за";
    Ready = "Choose a drive and press Scan.", "Выберите диск и нажмите «Поиск».";
    Recovering = "Recovering", "Восстановление";
    Stopped = "Stopped.", "Остановлено.";
    NoPreview = "No preview for this file.", "Для этого файла нет предпросмотра.";
    Loading = "Reading...", "Чтение...";
    Size = "Size", "Размер";
    StateLabel = "State", "Состояние";
    Modified = "Modified", "Изменён";
    InRecycleBin = "In the Recycle Bin", "В Корзине";
    DescGood = "No allocation conflict detected. File integrity is not verified.", "Конфликт размещения не обнаружен. Целостность файла не проверена.";
    DescPartial = "Some clusters are allocated elsewhere or their locations are missing.", "Часть кластеров занята другими файлами или их расположение потеряно.";
    DescFragment = "A content fragment. Original file boundaries and completeness are unknown.", "Фрагмент данных. Границы исходного файла и его полнота неизвестны.";
    TextFragments = "Text fragments", "Текстовые фрагменты";
    DescOverwritten = "Other files have taken all of its place.", "Всё его место заняли другие файлы.";
    DescZeroed = "The sampled ranges contain zeros. This does not prove TRIM or data loss.", "В проверенных участках нули. Это не доказывает TRIM или потерю данных.";
    DescNoData = "Only the name is known; where the data was is lost.", "Известно только имя, расположение данных потеряно.";
    DescEncrypted = "Encrypted with EFS: the data cannot be decrypted.", "Зашифрован EFS: данные не расшифровать.";
    DescMismatch = "The sampled header does not match the expected type. The file may be damaged or have a different format.", "Проверенный заголовок не соответствует ожидаемому типу. Файл может быть повреждён или иметь другой формат.";
    ChooseTarget = "Choose a folder on another drive for the recovered files", "Выберите папку на другом диске для восстановленных файлов";
    NothingChosen = "Check the files to recover, or select them in the list.", "Отметьте файлы для восстановления или выделите их в списке.";
    RecoverDone = "Recovery finished", "Восстановление завершено";
    RecoveredOk = "recovered", "восстановлено";
    RecoveredPartly = "recovered in part", "восстановлено частично";
    RecoveredErased = "saved with zeros", "сохранено с нулями";
    RecoveredSkipped = "skipped", "пропущено";
    RecoveredFailed = "failed", "с ошибкой";
    MetadataWarnings = "metadata warnings", "предупреждения о метаданных";
    CheckSel = "Check selected", "Отметить выделенные";
    UncheckSel = "Uncheck selected", "Снять отметку с выделенных";
    CheckAll = "Check all in the list", "Отметить все в списке";
    UncheckAll = "Uncheck all", "Снять все отметки";
    RecoverSel = "Recover selected...", "Восстановить выделенные...";
    Checked = "checked", "отмечено";
    InList = "in the list", "в списке";
    Volume = "Volume", "Том";
    Disk = "Physical disk", "Физический диск";
    ShadowCopy = "Shadow copy", "Теневая копия";
    Partition = "Partition", "Раздел";
    Saving = "Saving...", "Сохранение...";
    Saved = "Saved", "Сохранено";
    Pause = "Pause", "Пауза";
    Resume = "Resume", "Продолжить";
    Paused = "Paused", "Приостановлено";
    Stopping = "Stopping after pending I/O...", "Остановка после завершения текущего чтения...";
    LowPriority = "Low worker priority (next operation)", "Низкий приоритет (для следующей операции)";
    Eta = "Remaining", "Осталось";
    Hex = "Hex", "HEX";
    Text = "Text", "Текст";
    PreviewLimit = "Text and bytes: first 1 MB of the file.", "Текст и байты: первый 1 МБ файла.";
    PreviewImageLimit = "Image exceeds the 64 MB preview limit. Showing file bytes.", "Изображение превышает лимит предпросмотра 64 МБ. Показаны байты файла.";
    PreviewIncomplete = "Some data could not be read. Showing available bytes.", "Часть данных прочитать не удалось. Показаны доступные байты.";
    PreviewDecodeFailed = "Cannot decode this image. Showing file bytes.", "Не удалось декодировать изображение. Показаны байты файла.";
    LostPath = "The original folder path is unknown. Data condition is assessed separately.", "Исходный путь папки неизвестен. Состояние данных оценивается отдельно.";
    RawWarning = "No supported file system found. Search the whole source by signatures?", "Поддерживаемая файловая система не найдена. Искать по сигнатурам во всём источнике?";
    SameDisk = "The destination is on the same physical disk. Use another disk if the source may be failing. Continue?", "Папка назначения находится на том же физическом диске. При неисправности источника используйте другой диск. Продолжить?";
}

pub fn state_name(s: State) -> &'static str {
    t(match s {
        State::Good => S::Good,
        State::Partial => S::Partial,
        State::Overwritten => S::Overwritten,
        State::Zeroed => S::Zeroed,
        State::NoData => S::NoData,
        State::Encrypted => S::Encrypted,
        State::Mismatch => S::Mismatch,
        State::Unknown => S::Unknown,
    })
}

pub fn state_desc(s: State) -> &'static str {
    t(match s {
        State::Good => S::DescGood,
        State::Partial => S::DescPartial,
        State::Overwritten => S::DescOverwritten,
        State::Zeroed => S::DescZeroed,
        State::NoData => S::DescNoData,
        State::Encrypted => S::DescEncrypted,
        State::Mismatch => S::DescMismatch,
        State::Unknown => S::DescUnknown,
    })
}

pub fn entry_desc(e: &fr_core::Entry) -> &'static str {
    if e.flags & (fr_core::ef::SIGNATURE | fr_core::ef::INCOMPLETE) == (fr_core::ef::SIGNATURE | fr_core::ef::INCOMPLETE) {
        return t(S::DescFragment);
    }
    if e.state == State::Unknown {
        if e.flags & fr_core::ef::HISTORICAL != 0 {
            return t(S::DescHistorical);
        }
        if e.flags & fr_core::ef::SIGNATURE != 0 {
            return t(S::DescSignature);
        }
    }
    state_desc(e.state)
}

pub fn group_name(g: Group) -> &'static str {
    t(match g {
        Group::Photo => S::Photos,
        Group::Video => S::Video,
        Group::Audio => S::Audio,
        Group::Document => S::Documents,
        Group::Archive => S::Archives,
        Group::Mail => S::Mail,
        Group::Other => S::Other,
    })
}

pub fn entry_name(cat: &fr_core::Catalog, i: u32) -> std::borrow::Cow<'_, str> {
    use fr_core::carve::CARVE_ID_BASE;
    use fr_core::model::{FOUND_ID_BASE, FOUND_MFT_ROOT, ef};
    let e = &cat.entries[i as usize];
    let name = cat.name(i);
    if e.is_dir() && e.flags & ef::FOUND != 0 && lang() == Lang::Ru {
        if name == fr_core::carve::TEXT_ROOT {
            return format!("<{}>", t(S::TextFragments)).into();
        }
        if e.id == CARVE_ID_BASE {
            return format!("<{}>", t(S::FoundSignature)).into();
        }
        if let Some(g) = e.id.checked_sub(CARVE_ID_BASE + 1).and_then(|n| Group::ALL.get(n as usize)) {
            return group_name(*g).into();
        }
        if name == FOUND_MFT_ROOT {
            return format!("<{}>", t(S::FoundMft)).into();
        }
        if matches!(cat.fs.kind, fr_core::FsKind::Fat16 | fr_core::FsKind::Fat32 | fr_core::FsKind::ExFat)
            && let Some(n) = name.strip_prefix("Folder ").and_then(|s| s.parse::<u64>().ok())
            && e.id.checked_sub(FOUND_ID_BASE).and_then(|id| id.checked_add(if cat.fs.kind == fr_core::FsKind::ExFat { 2 } else { 0 }))
                == Some(n)
        {
            return format!("{} {n}", t(S::Folder)).into();
        }
    }
    name.into()
}

pub fn path(cat: &fr_core::Catalog, i: u32) -> String {
    cat.path_with(i, &format!("<{}>", t(S::LostFiles)), |k| entry_name(cat, k))
}

/// "1 file", "2 файла", "5 файлов": `forms` are (English singular, English plural,
/// Russian for 1, Russian for 2..4, Russian for 5..20).
pub fn plural(n: u64, forms: [&str; 5]) -> String {
    let w = match lang() {
        Lang::En if n == 1 => forms[0],
        Lang::En => forms[1],
        Lang::Ru => match (n % 10, n % 100) {
            (_, 11..=14) => forms[4],
            (1, _) => forms[2],
            (2..=4, _) => forms[3],
            _ => forms[4],
        },
    };
    format!("{} {w}", count(n))
}

pub fn files(n: u64) -> String {
    plural(n, ["file", "files", "файл", "файла", "файлов"])
}

fn decimal(v: f64, digits: usize) -> String {
    let s = format!("{v:.digits$}");
    if lang() == Lang::Ru { s.replace('.', ",") } else { s }
}

/// Thousands grouped with a no-break space (Russian) or a comma (English).
pub fn count(n: u64) -> String {
    let sep = if lang() == Lang::Ru { '\u{00A0}' } else { ',' };
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3 * 2);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(sep);
        }
        out.push(c);
    }
    out
}

pub fn size(n: u64) -> String {
    let names = match lang() {
        Lang::En => ["KB", "MB", "GB", "TB", "PB"],
        Lang::Ru => ["КБ", "МБ", "ГБ", "ТБ", "ПБ"],
    };
    if n < 1024 {
        return format!("{n} {}", if lang() == Lang::Ru { "Б" } else { "B" });
    }
    let mut v = n as f64 / 1024.0;
    let mut i = 0;
    while v >= 1024.0 && i < names.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{} {}", decimal(v, 1), names[i])
}

pub fn percent(f: f64) -> String {
    format!("{} %", decimal(f * 100.0, 0))
}

pub fn seconds(d: std::time::Duration) -> String {
    let s = d.as_secs_f64();
    match lang() {
        Lang::En => format!("{} sec", decimal(s, 1)),
        Lang::Ru => format!("{} сек", decimal(s, 1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn russian_plurals_and_sizes() {
        set_lang(Some(Lang::Ru));
        assert_eq!([files(1), files(3), files(11), files(21)], ["1 файл", "3 файла", "11 файлов", "21 файл"]);
        assert_eq!(size(1536), "1,5 КБ");
        use fr_core::carve::{CARVE_ID_BASE, CARVE_ROOT};
        use fr_core::model::{Entry, FsInfo, NONE, ef};
        let fs = FsInfo {
            kind: fr_core::FsKind::Ntfs,
            offset: 0,
            size: 8192,
            cluster_size: 512,
            record_size: 1024,
            mft: vec![],
            serial: 0,
            root: 5,
            data_offset: 0,
        };
        let mut cat = fr_core::Catalog::new(fs, vec![], vec![], vec![]);
        for (id, parent, name, flags) in [
            (5, NONE, "", ef::DIR),
            (CARVE_ID_BASE, 0, CARVE_ROOT, ef::DIR | ef::FOUND),
            (CARVE_ID_BASE + 1, 1, "Photos", ef::DIR | ef::FOUND),
            (CARVE_ID_BASE + 100, 2, "cat.jpg", ef::DELETED),
        ] {
            let (name_off, name_len) = cat.add_name(name);
            cat.entries.push(Entry { id, parent, flags, name_off, name_len, ..Default::default() });
        }
        assert_eq!(path(&cat, 3), r"\<Найдено по сигнатуре>\Фото\cat.jpg");
        cat.alias.insert(2, r"\Photos\User files".into());
        assert_eq!(path(&cat, 3), r"\Photos\User files\cat.jpg");
        set_lang(Some(Lang::En));
        assert_eq!(size(1536), "1.5 KB");
        assert_eq!(count(1_234_567), "1,234,567");
        set_lang(None);
    }
}
