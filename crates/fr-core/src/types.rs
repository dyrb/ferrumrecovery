//! File type groups by extension, for filters and summaries.

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Group {
    Photo,
    Video,
    Audio,
    Document,
    Archive,
    Mail,
    Other,
}

const PHOTO: &[&str] = &[
    "jpg", "jpeg", "jpe", "jfif", "png", "gif", "bmp", "tif", "tiff", "heic", "heif", "webp", "avif", "raw", "cr2", "cr3", "crw", "nef",
    "nrw", "arw", "srf", "sr2", "dng", "orf", "rw2", "raf", "pef", "srw", "x3f", "psd", "psb", "svg", "ico", "cur", "pcx", "dcx", "tga",
];
const VIDEO: &[&str] = &[
    "mp4", "m4v", "mov", "avi", "mkv", "webm", "wmv", "flv", "vp6", "swf", "mpg", "mpeg", "3gp", "3g2", "ts", "mts", "m2ts", "vob", "ogv",
    "asf",
];
const AUDIO: &[&str] = &["mp3", "wav", "flac", "ogg", "oga", "opus", "m4a", "aac", "wma", "aif", "aiff", "ape", "mid", "midi", "amr"];
const DOCUMENT: &[&str] = &[
    "pdf", "doc", "docx", "docm", "dot", "dotx", "xls", "xlsx", "xlsm", "xlsb", "ppt", "pptx", "pps", "ppsx", "odt", "ods", "odp", "odg",
    "rtf", "txt", "csv", "md", "epub", "djvu", "fb2", "mobi", "xps", "pub", "vsd", "vsdx", "one", "pages", "numbers", "key", "tex",
];
const ARCHIVE: &[&str] =
    &["zip", "rar", "7z", "gz", "tgz", "bz2", "tbz2", "xz", "txz", "zst", "lz", "lzma", "tar", "cab", "iso", "img", "arj"];
const MAIL: &[&str] = &["eml", "msg", "pst", "ost", "mbox", "emlx"];

impl Group {
    pub const ALL: [Group; 7] = [Group::Photo, Group::Video, Group::Audio, Group::Document, Group::Archive, Group::Mail, Group::Other];

    pub fn name(self) -> &'static str {
        match self {
            Group::Photo => "photo",
            Group::Video => "video",
            Group::Audio => "audio",
            Group::Document => "document",
            Group::Archive => "archive",
            Group::Mail => "mail",
            Group::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Option<Group> {
        let s = s.to_ascii_lowercase();
        let s = match s.as_str() {
            "photos" | "image" | "images" | "picture" | "pictures" => "photo",
            "videos" => "video",
            "music" => "audio",
            "doc" | "docs" | "documents" => "document",
            "archives" => "archive",
            "email" => "mail",
            other => other,
        };
        Group::ALL.into_iter().find(|g| g.name() == s)
    }

    /// Group of a file name by its extension.
    pub fn of(name: &str) -> Group {
        let Some(dot) = name.rfind('.') else { return Group::Other };
        let ext = name[dot + 1..].to_ascii_lowercase();
        let ext = ext.as_str();
        for (list, g) in [
            (PHOTO, Group::Photo),
            (VIDEO, Group::Video),
            (AUDIO, Group::Audio),
            (DOCUMENT, Group::Document),
            (ARCHIVE, Group::Archive),
            (MAIL, Group::Mail),
        ] {
            if list.contains(&ext) {
                return g;
            }
        }
        Group::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_by_extension() {
        assert_eq!(Group::of("IMG_0001.JPG"), Group::Photo);
        assert_eq!(Group::of("report.final.docx"), Group::Document);
        assert_eq!(Group::of("movie.mkv"), Group::Video);
        assert_eq!(Group::of("Makefile"), Group::Other);
        assert_eq!(Group::parse("Photos"), Some(Group::Photo));
        assert_eq!(Group::parse("docs"), Some(Group::Document));
    }
}
