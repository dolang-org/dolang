//! Directives: settings a source file gives its own compilation, in comments
//! near its top.
//!
//! A directive is a line of the form `# dolang: setting, ...` among the first
//! [`LINES`] lines, with settings separated by commas or whitespace. Lines are
//! scanned as text, not lexed, so a line of a here string that looks like a
//! directive counts as one.

use std::fmt::{self, Write};

use crate::{
    Compiler,
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
pub(crate) fn scan(file: &File<'_>, diags: &Diags) -> Directives {
    let mut directives = Directives::default();
    for line in lines(file) {
        let text = file.slice(line);
        let Some(settings) = settings(text) else {
            continue;
        };
        // The settings are a subslice of the line
        let base = line.start + (settings.as_ptr() as usize - text.as_ptr() as usize) as Offset;
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

/// Whether a comment on its own line is a directive, which documents nothing
pub(crate) fn is_directive(file: &File<'_>, comment: Span) -> bool {
    (file.coord(comment.start).line as usize) < LINES && settings(file.slice(comment)).is_some()
}

/// The spans of the lines scanned for directives, without their terminators
fn lines<'a>(file: &'a File<'_>) -> impl Iterator<Item = Span> + 'a {
    let len = file.content().len() as Offset;
    let ends = file.newlines().iter().copied().chain([len]);
    let starts = [0]
        .into_iter()
        .chain(file.newlines().iter().map(|&nl| nl + 1));
    starts
        .zip(ends)
        .take(LINES)
        .filter(move |&(start, _)| start <= len)
        .map(|(start, end)| Span { start, end })
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

    fn message(&self, compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        let setting = String::from_utf8_lossy(compiler.file.slice(self.0));
        write!(w, "unknown directive setting `{setting}`")
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn scan_str(source: &str) -> (Directives, usize) {
        let file = File::new(Path::new("test.dol"), source.as_bytes());
        let diags = Diags::new();
        let directives = scan(&file, &diags);
        (directives, diags.iter().count())
    }

    #[test]
    fn strict() {
        assert_eq!(scan_str("# dolang: strict\n").0.strict, Some(true));
        assert_eq!(scan_str("#dolang:strict").0.strict, Some(true));
        assert_eq!(
            scan_str("#!/usr/bin/env dolang\n# dolang: strict\n")
                .0
                .strict,
            Some(true)
        );
        assert_eq!(
            scan_str("  # dolang: strict, nostrict\n").0.strict,
            Some(false)
        );
    }

    #[test]
    fn absent() {
        assert_eq!(scan_str("").0.strict, None);
        assert_eq!(scan_str("# strict\necho dolang: strict\n").0.strict, None);
        let late = format!("{}# dolang: strict\n", "\n".repeat(LINES));
        assert_eq!(scan_str(&late).0.strict, None);
        let last = format!("{}# dolang: strict\n", "\n".repeat(LINES - 1));
        assert_eq!(scan_str(&last).0.strict, Some(true));
    }

    #[test]
    fn unknown() {
        let (directives, diags) = scan_str("# dolang: strict, sloppy\n");
        assert_eq!(directives.strict, Some(true));
        assert_eq!(diags, 1);
    }
}
