//! Which found entries to show and recover.

use crate::model::{Catalog, State, ef};
use crate::types::Group;

/// Case-insensitive name pattern: `*` and `?` wildcards, or a plain substring.
#[derive(Clone, Debug)]
pub struct Pattern {
    chars: Vec<char>,
    wild: bool,
    plain: String,
}

impl Pattern {
    pub fn new(p: &str) -> Pattern {
        let lower = p.to_lowercase();
        Pattern { chars: lower.chars().collect(), wild: lower.contains(['*', '?']), plain: lower }
    }

    pub fn matches(&self, s: &str) -> bool {
        let lower = s.to_lowercase();
        if self.wild { glob_match(&self.chars, &lower) } else { lower.contains(&self.plain) }
    }
}

/// Iterative glob with single-star backtracking: linear in practice.
pub fn glob_match(pat: &[char], name: &str) -> bool {
    let s: Vec<char> = name.chars().collect();
    let (mut p, mut i) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while i < s.len() {
        if p < pat.len() && (pat[p] == '?' || pat[p] == s[i]) {
            p += 1;
            i += 1;
        } else if p < pat.len() && pat[p] == '*' {
            star = p;
            mark = i;
            p += 1;
        } else if star != usize::MAX {
            p = star + 1;
            mark += 1;
            i = mark;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == '*' {
        p += 1;
    }
    p == pat.len()
}

#[derive(Clone, Debug, Default)]
pub struct Filter {
    pub name: Vec<Pattern>,
    pub path: Vec<Pattern>,
    pub groups: Vec<Group>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    /// Modified on or after / before (FILETIME).
    pub after: Option<u64>,
    pub before: Option<u64>,
    /// Deleted on or after / before (FILETIME); entries with no known deletion time fail.
    pub deleted_after: Option<u64>,
    pub deleted_before: Option<u64>,
    pub states: Vec<State>,
    /// Also files that are not deleted.
    pub existing: bool,
    pub system: bool,
    pub empty: bool,
    pub dirs: bool,
    /// Also names without data (change journal, folder index slack).
    pub traces: bool,
    /// Also named data streams (`file:stream`).
    pub streams: bool,
    /// Only these ids.
    pub ids: Vec<u64>,
}

impl Filter {
    pub fn accepts(&self, cat: &Catalog, i: u32) -> bool {
        let e = &cat.entries[i as usize];
        if !self.ids.is_empty() {
            return self.ids.contains(&e.id);
        }
        if e.is_dir() && !self.dirs {
            return false;
        }
        if !e.is_gone() && !self.existing {
            return false;
        }
        if e.flags & ef::TRACE != 0 && !self.traces {
            return false;
        }
        if e.flags & ef::STREAM != 0 && !self.streams {
            return false;
        }
        if e.flags & ef::SYSTEM != 0 && !self.system {
            return false;
        }
        if e.size == 0 && !e.is_dir() && !self.empty {
            return false;
        }
        if self.min_size.is_some_and(|m| e.size < m) || self.max_size.is_some_and(|m| e.size > m) {
            return false;
        }
        if self.after.is_some_and(|t| e.modified < t) || self.before.is_some_and(|t| e.modified >= t) {
            return false;
        }
        if self.deleted_after.is_some_and(|t| e.deleted_at < t)
            || self.deleted_before.is_some_and(|t| e.deleted_at == 0 || e.deleted_at >= t)
        {
            return false;
        }
        if !self.states.is_empty() && !self.states.contains(&e.state) {
            return false;
        }
        let name = cat.name(i);
        if !self.name.is_empty() && !self.name.iter().any(|p| p.matches(name)) {
            return false;
        }
        if !self.groups.is_empty() && !self.groups.contains(&Group::of(name)) {
            return false;
        }
        if !self.path.is_empty() {
            let path = cat.path(i);
            if !self.path.iter().any(|p| p.matches(&path)) {
                return false;
            }
        }
        true
    }

    /// Indices of all accepted entries.
    pub fn select(&self, cat: &Catalog) -> Vec<u32> {
        (0..cat.entries.len() as u32).filter(|&i| self.accepts(cat, i)).collect()
    }
}

/// `10M`, `1.5G`, `512K`, `100` (bytes), binary units.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_uppercase();
    let s = s.strip_suffix('B').unwrap_or(&s);
    let (num, mul) = match s.chars().last()? {
        'K' => (&s[..s.len() - 1], 1u64 << 10),
        'M' => (&s[..s.len() - 1], 1 << 20),
        'G' => (&s[..s.len() - 1], 1 << 30),
        'T' => (&s[..s.len() - 1], 1 << 40),
        _ => (s, 1),
    };
    let v: f64 = num.trim().parse().ok()?;
    if !v.is_finite() || v < 0.0 {
        return None;
    }
    Some((v * mul as f64) as u64)
}

/// `1.5 MB` style.
pub fn format_size(v: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if v < 1024 {
        return format!("{v} B");
    }
    let mut x = v as f64;
    let mut u = 0;
    while x >= 1024.0 && u < U.len() - 1 {
        x /= 1024.0;
        u += 1;
    }
    if x < 10.0 { format!("{x:.1} {}", U[u]) } else { format!("{x:.0} {}", U[u]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns() {
        assert!(Pattern::new("*.JPG").matches("IMG_1.jpg"));
        assert!(!Pattern::new("*.jpg").matches("a.jpeg"));
        assert!(Pattern::new("img_??.*").matches("IMG_12.png"));
        assert!(Pattern::new("report").matches("Q3 Report final.docx"));
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("10M"), Some(10 << 20));
        assert_eq!(parse_size("1.5g"), Some(3 << 29));
        assert_eq!(parse_size("512KB"), Some(512 << 10));
        assert_eq!(parse_size("100"), Some(100));
        assert_eq!(parse_size("x"), None);
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(300 << 20), "300 MB");
    }
}
