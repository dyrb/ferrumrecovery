//! Presentation of engine diagnostics; paths and arguments are never translated.
use crate::lang::{Lang, lang};
use std::io;

const MESSAGES: &[(&str, &str)] = &[
    ("$MFT data lies outside the volume", "Данные $MFT выходят за пределы тома"),
    (
        "historical layout candidate limit reached; some layouts were not processed",
        "достигнут предел кандидатов старых структур; часть структур не обработана",
    ),
    ("operation cancelled", "операция отменена"),
    ("invalid file record range", "неверный диапазон файловой записи"),
    ("file record exceeds read buffer", "файловая запись превышает буфер чтения"),
    ("file record unreadable", "файловая запись не читается"),
    ("file record damaged", "файловая запись повреждена"),
    ("invalid record size", "неверный размер записи"),
    ("invalid record number", "неверный номер записи"),
    ("record outside the MFT", "запись за пределами MFT"),
    ("file record was reused after the scan", "файловая запись использована повторно после поиска"),
    ("invalid compression unit", "неверный блок сжатия"),
    ("compression unit too large", "слишком большой блок сжатия"),
    ("invalid or incomplete imaging map", "карта копирования повреждена или неполна"),
    ("imaging range is outside the source", "диапазон копирования за пределами источника"),
    ("image and map paths must differ", "пути образа и карты должны различаться"),
    ("no output folder", "не указана папка назначения"),
    ("no map folder", "не указана папка для карты копирования"),
    ("destination refers to the source image", "назначение указывает на исходный образ"),
    ("resume requires the original output image with its original length", "для продолжения нужен прежний образ с исходным размером"),
    ("output already exists without a resume map", "образ уже существует, но карта для продолжения отсутствует"),
    ("resume map does not match this source, range and output image", "карта не соответствует источнику, диапазону или выходному образу"),
    ("short image write", "образ записан не полностью"),
    ("partition table outside the source", "таблица разделов за пределами источника"),
    ("parent path components are not allowed", "переходы к родительской папке в пути запрещены"),
    (
        "destination must contain ordinary directories only (no junctions or symbolic links)",
        "путь назначения должен содержать обычные папки без точек соединения и символических ссылок",
    ),
    ("no such drive", "диск не найден"),
    ("cannot find its volume", "не удалось определить том"),
    (
        "destination is on the source volume {}; choose a folder on another drive",
        "назначение находится на исходном томе {}; выберите папку на другом диске",
    ),
    (
        "destination lies inside the area being recovered from; choose a folder on another drive",
        "назначение находится в восстанавливаемой области; выберите папку на другом диске",
    ),
    (
        "Cannot verify source and destination disk extents; choose a local destination on another drive",
        "не удалось проверить размещение источника и назначения; выберите локальную папку на другом диске",
    ),
    ("saved scan is damaged or from another version", "результаты поиска повреждены или сохранены другой версией"),
    ("not a saved scan", "это не файл результатов поиска"),
    ("Saved scan lies outside the source", "сохранённый диапазон поиска за пределами источника"),
    ("not a drive letter, disk number or image file", "не указана буква тома, номер диска или файл образа"),
    ("shadow copy size is unavailable", "не удалось определить размер теневой копии"),
    ("invalid shadow copy boot sector", "повреждён загрузочный сектор теневой копии"),
    (
        "VHDX images are not supported; attach it in Disk Management and scan its drive letter",
        "образы VHDX не поддерживаются; подключите образ в управлении дисками и выберите его том",
    ),
    (
        "dynamic VHD images are not supported; attach it in Disk Management and scan its drive letter",
        "динамические VHD не поддерживаются; подключите образ в управлении дисками и выберите его том",
    ),
    ("read range is outside the source", "диапазон чтения за пределами источника"),
    ("short read: {} of {} bytes", "прочитано {} из {} байт"),
    ("not an exFAT boot sector", "это не загрузочный сектор exFAT"),
    ("not a FAT boot sector", "это не загрузочный сектор FAT"),
    ("{} is not a FAT file system", "{} не является файловой системой FAT"),
    ("short read", "неполное чтение"),
    ("bytes per sector", "размер сектора"),
    ("sectors per cluster", "число секторов в кластере"),
    ("record size", "размер записи"),
    ("cluster size", "размер кластера"),
    ("volume size", "размер тома"),
    ("record smaller than sector", "запись меньше сектора"),
    ("$MFT location", "расположение $MFT"),
    ("volume is not NTFS", "том не содержит NTFS"),
    ("invalid NTFS boot sector", "неверный загрузочный сектор NTFS"),
    ("invalid $MFT record", "неверная запись $MFT"),
    ("invalid data run list", "неверный список участков данных"),
    ("unsupported", "не поддерживается"),
    ("metadata attribute too large", "слишком большой атрибут метаданных"),
    ("$MFT shorter than its data size", "$MFT короче объявленного размера данных"),
    ("hole in $MFT runlist", "пропуск в списке участков $MFT"),
    ("sparse $MFT run", "разреженный участок $MFT"),
    ("$MFT record not in use", "запись $MFT не используется"),
    ("unresolvable $MFT attribute list", "не удалось разрешить список атрибутов $MFT"),
    ("no $DATA", "атрибут $DATA отсутствует"),
    ("signature", "сигнатура"),
    ("update sequence", "последовательность обновления"),
    ("attribute overrun", "атрибут за пределами записи"),
    ("attribute length", "длина атрибута"),
    ("attribute name", "имя атрибута"),
    ("resident value", "резидентное значение"),
    ("non-resident header", "нерезидентный заголовок"),
    ("mapping pairs offset", "смещение списка участков"),
    ("attribute list entry", "элемент списка атрибутов"),
    ("reading a drive directly needs administrator rights", "для прямого чтения диска нужны права администратора"),
    ("no file system at offset {}", "файловая система по смещению {} не найдена"),
    ("no file system or partition table found", "файловая система и таблица разделов не найдены"),
    ("no partition {}", "раздел {} не найден"),
    ("several partitions, choose one:\n{}", "найдено несколько разделов, выберите один:\n{}"),
    (
        "no NTFS, FAT or exFAT file system found; --raw looks for files by their signatures",
        "NTFS, FAT или exFAT не найдены; режим --raw ищет файлы по сигнатурам",
    ),
    ("scan range is outside the source", "диапазон поиска за пределами источника"),
    ("this is not the volume that was scanned (its boot sector differs)", "это другой том: загрузочный сектор отличается от сохранённого"),
    ("worker stopped unexpectedly", "рабочий поток неожиданно завершился"),
    ("No destination folder", "не указана папка назначения"),
    ("not enough free space on the target drive", "на диске назначения недостаточно свободного места"),
    ("file dialog failed (code {})", "не удалось открыть диалог выбора файла (код {})"),
    ("cannot locate program settings", "не удалось определить путь настроек программы"),
    ("preview unavailable", "предпросмотр недоступен"),
    ("not enough memory for preview", "недостаточно памяти для предпросмотра"),
    ("file data was saved but its metadata could not be restored", "данные сохранены, но восстановить метаданные файла не удалось"),
    ("invalid GPT partition table", "таблица разделов GPT повреждена"),
    ("{} bytes could not be read", "не удалось прочитать {} байт"),
    ("{} damaged file records were skipped", "пропущено повреждённых файловых записей: {}"),
    (
        "allocation metadata could not be read; file states may be unknown",
        "не удалось прочитать сведения о занятых кластерах; состояние файлов может быть не определено",
    ),
    ("deep search of old MFT records was skipped for this cluster size", "поиск старых записей MFT пропущен из-за размера кластеров"),
    ("shadow copy discovery is unavailable on this version of Windows", "перечисление теневых копий недоступно в этой версии Windows"),
    ("shadow copy discovery failed (NTSTATUS {})", "не удалось перечислить теневые копии (NTSTATUS {})"),
    ("invalid device directory response", "некорректный ответ каталога устройств"),
];

fn system_error(e: &io::Error) -> String {
    use windows_sys::Win32::System::Diagnostics::Debug::{FORMAT_MESSAGE_FROM_SYSTEM, FORMAT_MESSAGE_IGNORE_INSERTS, FormatMessageW};
    if let Some(code) = e.raw_os_error() {
        let mut buf = [0u16; 2048];
        // SAFETY: fixed writable buffer; no inserts or module pointer.
        let n = unsafe {
            FormatMessageW(
                FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
                std::ptr::null(),
                code as u32,
                if lang() == Lang::Ru { 0x419 } else { 0x409 },
                buf.as_mut_ptr(),
                buf.len() as u32,
                std::ptr::null(),
            )
        };
        if n > 0 {
            return String::from_utf16_lossy(&buf[..n as usize]).trim().to_owned();
        }
        return format!("{} ({code})", if lang() == Lang::Ru { "Ошибка Windows" } else { "Windows error" });
    }
    if lang() == Lang::En {
        return e.to_string();
    }
    match e.kind() {
        io::ErrorKind::NotFound => "Файл или устройство не найдены",
        io::ErrorKind::PermissionDenied => "Нет доступа",
        io::ErrorKind::AlreadyExists => "Файл уже существует; выберите другое имя",
        io::ErrorKind::OutOfMemory => "Недостаточно памяти",
        io::ErrorKind::InvalidData => "Данные повреждены или имеют неподдерживаемый формат",
        io::ErrorKind::InvalidInput => "Неверные параметры",
        io::ErrorKind::UnexpectedEof => "Данные прочитаны не полностью",
        io::ErrorKind::WriteZero => "Данные записаны не полностью",
        io::ErrorKind::Interrupted => "Операция прервана",
        _ => "Ошибка ввода-вывода",
    }
    .to_owned()
}

pub fn error(e: &fr_core::error::Error) -> String {
    e.render(
        &|id| {
            if lang() == Lang::En {
                id.to_owned()
            } else {
                MESSAGES.iter().find(|&&(en, _)| en == id).map_or_else(|| id.to_owned(), |&(_, ru)| ru.to_owned())
            }
        },
        &system_error,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diagnostic_catalog_covers_engine_messages() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();
        fn visit(dir: &std::path::Path, messages: &mut Vec<String>) {
            if dir.is_file() {
                let src = std::fs::read_to_string(dir).unwrap();
                let src = src.split("#[cfg(test)]").next().unwrap();
                for prefix in [
                    "Error::new(",
                    "Error::value(",
                    "Error::cause(",
                    "Error::Message(",
                    "D::new(",
                    "D::cause(",
                    "BadBootSector(",
                    "BadMftRecord(",
                    "Unsupported(",
                    "unsupported(",
                    "invalid(",
                    "ok_or(",
                    "Err(",
                ] {
                    for part in src.split(prefix).skip(1) {
                        if let Some(part) = part.trim_start().strip_prefix('"')
                            && let Some((message, _)) = part.split_once('"')
                        {
                            messages.push(message.replace("\\n", "\n"));
                        }
                    }
                }
                return;
            }
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() || path.extension().is_some_and(|s| s == "rs") {
                    visit(&path, messages);
                }
            }
        }
        let mut messages = Vec::new();
        visit(&root.join("crates"), &mut messages);
        for name in ["engine.rs", "preview.rs", "settings.rs", "cmds.rs"] {
            visit(&root.join("apps/ferrumrecovery-gui/src").join(name), &mut messages);
        }
        for msg in messages {
            assert!(MESSAGES.iter().any(|&(en, _)| en == msg), "untranslated diagnostic: {msg}");
        }
        for &(en, ru) in MESSAGES {
            assert!(!ru.is_empty());
            assert_eq!(en.matches("{}").count(), ru.matches("{}").count(), "arguments: {en}");
        }
    }
}
