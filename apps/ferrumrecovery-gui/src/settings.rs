//! Settings: `FerrumRecovery.ini` next to the program (portable: nothing elsewhere), one
//! `key=value` per line. Written only when a setting is changed from a menu, never during
//! a scan or a recovery: the program may run from the drive being scanned.

use std::path::PathBuf;

use crate::lang::Lang;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThemePref {
    System,
    Light,
    Dark,
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub theme: ThemePref,
    /// `None`: as in Windows.
    pub lang: Option<Lang>,
    pub deep: bool,
    pub full: bool,
    pub check: bool,
    pub existing: bool,
    pub system: bool,
    pub empty: bool,
    pub traces: bool,
    pub streams: bool,
    /// Width shares of the folder tree and the preview.
    pub split_tree: f32,
    pub split_preview: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            theme: ThemePref::System,
            lang: None,
            deep: false,
            full: false,
            check: true,
            existing: false,
            system: false,
            empty: false,
            traces: false,
            streams: false,
            split_tree: 0.20,
            split_preview: 0.26,
        }
    }
}

fn path() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.join("FerrumRecovery.ini"))
}

impl Settings {
    pub fn load() -> Settings {
        let mut s = Settings::default();
        let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) else { return s };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            let flag = v == "1";
            let frac = v.parse::<f32>().ok().filter(|f| (0.01..=0.95).contains(f));
            match k {
                "theme" => {
                    s.theme = match v {
                        "light" => ThemePref::Light,
                        "dark" => ThemePref::Dark,
                        _ => ThemePref::System,
                    }
                }
                "language" => {
                    s.lang = match v {
                        "en" => Some(Lang::En),
                        "ru" => Some(Lang::Ru),
                        _ => None,
                    }
                }
                "deep" => s.deep = flag,
                "full" => s.full = flag,
                "check" => s.check = flag,
                "existing" => s.existing = flag,
                "system" => s.system = flag,
                "empty" => s.empty = flag,
                "traces" => s.traces = flag,
                "streams" => s.streams = flag,
                "split_tree" => s.split_tree = frac.unwrap_or(s.split_tree),
                "split_preview" => s.split_preview = frac.unwrap_or(s.split_preview),
                _ => {}
            }
        }
        if s.full {
            s.deep = true;
        }
        s
    }

    pub fn save(&self) -> Result<(), fr_core::error::Error> {
        let p = path().ok_or_else(|| fr_core::error::Error::new("cannot locate program settings"))?;
        let b = |v: bool| if v { 1 } else { 0 };
        let out = format!(
            "language={}\ntheme={}\ndeep={}\nfull={}\ncheck={}\nexisting={}\nsystem={}\nempty={}\ntraces={}\nstreams={}\nsplit_tree={:.3}\nsplit_preview={:.3}\n",
            match self.lang {
                None => "system",
                Some(Lang::En) => "en",
                Some(Lang::Ru) => "ru",
            },
            match self.theme {
                ThemePref::System => "system",
                ThemePref::Light => "light",
                ThemePref::Dark => "dark",
            },
            b(self.deep),
            b(self.full),
            b(self.check),
            b(self.existing),
            b(self.system),
            b(self.empty),
            b(self.traces),
            b(self.streams),
            self.split_tree,
            self.split_preview,
        );
        if std::fs::read_to_string(&p).is_ok_and(|old| old == out) {
            return Ok(());
        }
        std::fs::write(&p, out).map_err(|e| fr_core::error::Error::from(e).context(p.display()))
    }
}
