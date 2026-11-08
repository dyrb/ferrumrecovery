//! Structured diagnostics. Interfaces choose the language; scanners keep message ids,
//! values, context and operating-system errors separate from rendered text.
use std::{fmt, io};

#[derive(Clone, Debug)]
pub enum Error {
    Message(&'static str, Vec<String>),
    Io(std::sync::Arc<io::Error>),
    Context(String, Box<Error>),
    Cause(&'static str, Box<Error>),
}

impl Error {
    pub fn new(message: &'static str) -> Self {
        Self::Message(message, Vec::new())
    }
    pub fn value(message: &'static str, value: impl ToString) -> Self {
        Self::Message(message, vec![value.to_string()])
    }
    pub fn context(self, context: impl ToString) -> Self {
        Self::Context(context.to_string(), Box::new(self))
    }
    pub fn cause(message: &'static str, cause: impl Into<Error>) -> Self {
        Self::Cause(message, Box::new(cause.into()))
    }
    pub fn into_io(self, kind: io::ErrorKind) -> io::Error {
        io::Error::new(kind, self)
    }

    pub fn render(&self, text: &impl Fn(&str) -> String, os: &impl Fn(&io::Error) -> String) -> String {
        match self {
            Self::Message(id, args) => {
                let template = text(id);
                let mut parts = template.split("{}");
                let mut out = parts.next().unwrap_or_default().to_owned();
                for (i, part) in parts.enumerate() {
                    out.push_str(args.get(i).map(String::as_str).unwrap_or("{}"));
                    out.push_str(part);
                }
                out
            }
            Self::Io(e) => {
                if let Some(inner) = e.get_ref().and_then(|e| e.downcast_ref::<Error>()) {
                    inner.render(text, os)
                } else {
                    os(e)
                }
            }
            Self::Context(context, inner) => format!("{context}: {}", inner.render(text, os)),
            Self::Cause(id, inner) => format!("{}: {}", text(id), inner.render(text, os)),
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(std::sync::Arc::new(e))
    }
}
impl From<&'static str> for Error {
    fn from(s: &'static str) -> Self {
        Self::new(s)
    }
}
impl From<Error> for String {
    fn from(e: Error) -> Self {
        e.to_string()
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render(&str::to_owned, &ToString::to_string))
    }
}
impl std::error::Error for Error {}
impl PartialEq for Error {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Message(a, av), Self::Message(b, bv)) => a == b && av == bv,
            (Self::Context(a, av), Self::Context(b, bv)) => a == b && av == bv,
            (Self::Cause(a, av), Self::Cause(b, bv)) => a == b && av == bv,
            (Self::Io(a), Self::Io(b)) => a.kind() == b.kind() && a.raw_os_error() == b.raw_os_error() && a.to_string() == b.to_string(),
            _ => false,
        }
    }
}
impl Eq for Error {}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn translated_diagnostics_preserve_paths_and_nested_causes() {
        let path = "C:\\image is damaged\\scan.frs";
        let e = Error::new("image is damaged").context(path).into_io(io::ErrorKind::InvalidData);
        let result =
            Error::from(e).render(&|s| if s == "image is damaged" { "translated damage".into() } else { s.into() }, &ToString::to_string);
        assert_eq!(result, format!("{path}: translated damage"));
        assert_eq!(Error::value("no partition {}", 12).to_string(), "no partition 12");
    }
}
