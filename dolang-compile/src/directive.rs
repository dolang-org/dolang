//! Directives: settings a source file gives its own compilation, in comments
//! near its top.
//!
//! A directive is a comment on its own line of the form
//! `# dolang: setting, ...` among the first [`LINES`] lines, with settings
//! separated by commas or whitespace.

use std::fmt::{self, Write};

use crate::{
    diag::Severity,
    source::{Diagnose, Diags, File, Offset, Span},
};

/// How many lines from the top of a file are scanned for directives
pub(crate) const LINES: usize = 10;

const PREFIX: &[u8] = b"dolang:";

/// The settings a file's directives give
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Directives {
    /// Whether the unit is checked strictly, when a directive says
    pub(crate) strict: Option<bool>,
}

/// Scan a file's directives, diagnosing settings that aren't known. A later
/// setting overrides an earlier one.
pub(crate) fn scan(file: &File<'_>, comments: &[Span], diags: &Diags) -> Directives {
    let mut directives = Directives::default();
    for &comment in comments {
        if !is_directive(file, comment) {
            continue;
        }
        let text = file.slice(comment);
        let settings = settings(text).expect("directive comment has settings");
        // The settings are a subslice of the comment.
        let base = comment.start + (settings.as_ptr() as usize - text.as_ptr() as usize) as Offset;
        let mut start = 0;
        for word in settings.split(|&b| b == b',' || b.is_ascii_whitespace()) {
            let span = Span {
                start: base + start as Offset,
                end: base + (start + word.len()) as Offset,
            };
            start += word.len() + 1;
            match word {
                b"" => {}
                b"strict" => directives.strict = Some(true),
                b"nostrict" => directives.strict = Some(false),
                _ => diags.push(UnknownSetting(span)),
            }
        }
    }
    directives
}

/// Whether a comment is a directive, which documents nothing
pub(crate) fn is_directive(file: &File<'_>, comment: Span) -> bool {
    (file.coord(comment.start).line as usize) < LINES
        && own_line(file, comment.start)
        && settings(file.slice(comment)).is_some()
}

/// Whether only whitespace precedes `offset` on its line.
pub(crate) fn own_line(file: &File<'_>, offset: Offset) -> bool {
    file.content()[..offset as usize]
        .iter()
        .rev()
        .take_while(|byte| **byte != b'\n')
        .all(u8::is_ascii_whitespace)
}

/// The settings of a directive line, or `None` if it isn't one
fn settings(line: &[u8]) -> Option<&[u8]> {
    let line = line.trim_ascii();
    let rest = line.strip_prefix(b"#")?.trim_ascii_start();
    rest.strip_prefix(PREFIX)
}

struct UnknownSetting(Span);

impl Diagnose for UnknownSetting {
    fn span(&self) -> Span {
        self.0
    }

    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn message(&self, file: &File<'_>, w: &mut dyn Write) -> fmt::Result {
        let setting = String::from_utf8_lossy(file.slice(self.0));
        write!(w, "unknown directive setting `{setting}`")
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn scan_str(source: &str) -> (bool, Vec<String>) {
        let unit = crate::Config::new().unit(Path::new("test.dol"), source.as_bytes());
        (
            unit.strict(),
            unit.diagnostics()
                .map(|diag| diag.message().to_string())
                .collect(),
        )
    }

    #[test]
    fn strict() {
        assert!(scan_str("# dolang: strict\n").0);
        assert!(scan_str("#dolang:strict").0);
        assert!(scan_str("#!/usr/bin/env dolang\n# dolang: strict\n").0);
        assert!(!scan_str("  # dolang: strict, nostrict\n").0);
        assert!(scan_str("# dolang: nostrict\n# dolang: strict\n").0);
        assert!(!scan_str("# dolang: strict\n# dolang: nostrict\n").0);
    }

    #[test]
    fn absent() {
        assert!(!scan_str("").0);
        assert!(!scan_str("# strict\necho dolang: strict\n").0);
        let late = format!("{}# dolang: strict\n", "\n".repeat(LINES));
        assert!(!scan_str(&late).0);
        let last = format!("{}# dolang: strict\n", "\n".repeat(LINES - 1));
        assert!(scan_str(&last).0);
    }

    #[test]
    fn unknown() {
        let (strict, diags) = scan_str("# dolang: strict, sloppy\n");
        assert!(strict);
        assert_eq!(diags, ["unknown directive setting `sloppy`"]);
    }

    #[test]
    fn here_strings_are_not_directives() {
        for marker in ["|", "r|"] {
            let source = format!("let doc = {marker}\n  # dolang: strict, sloppy\ndoc\n");
            let (strict, diags) = scan_str(&source);
            assert!(!strict, "{marker}");
            assert!(diags.is_empty(), "{marker}: {diags:?}");
        }
    }

    #[test]
    fn trailing_comment_is_not_a_directive() {
        let (strict, diags) = scan_str("let x = 1  # dolang: strict, sloppy\nx\n");
        assert!(!strict);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn config_strict_overrides_directive() {
        let mut config = crate::Config::new();
        config.strict(false);
        let unit = config.unit(Path::new("test.dol"), b"# dolang: strict\n");
        assert!(!unit.strict());
    }
}
