//! Annotations written into fixture source, pointing at a span of a neighbouring
//! line.
//!
//! An annotation is a comment holding a marker run and a payload after a `:`:
//!
//! ```text
//! echo hello
//! #    ^~~~: literal
//! ```
//!
//! What the payload means is up to the harness reading it.
//!
//! # Why two forms
//!
//! Comments participate in indentation: a comment at column 0 inside an
//! indented block ends the block, and a comment indented less than its block is
//! an indentation error.  An annotation comment therefore sits at the enclosing
//! block's indent column or deeper, which puts the *first* token of every line
//! out of reach of a `^`.  A run that starts immediately after the `#` binds
//! downward instead, with the `#` standing in for the span's first column:
//!
//! ```text
//! #~~: keyword
//! def foo bar
//! ```
//!
//! The `#` must sit at exactly the column where the next line's first token
//! starts, which is also the column the comment would naturally be written at.
//!
//! # Limits
//!
//! Annotations stack, but nothing else may come between one and what it marks:
//! an ordinary comment written among them becomes the line they bind to.  Prose
//! goes above the source line, not in the middle of its annotations.
//!
//! An annotation names a span on one line, so a span covering several lines
//! cannot be annotated.  Nothing inside a here string can be annotated at all,
//! one line or many: a `#` there is content, not a comment.
//! Annotated lines must be ASCII, because columns are byte offsets and a
//! multi-byte character would silently slide the carets off the span they
//! appear to mark.

use std::path::Path;

/// Which line an annotation binds to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bind {
    /// A run containing `^`, binding to the line above.
    Up,
    /// A run starting immediately after the `#`, binding to the line below.
    Down,
}

#[derive(Debug)]
pub struct Annotation {
    /// Line the annotation is written on, 0-based.
    pub line: usize,
    pub bind: Bind,
    /// Column the `#` sits at, which the down form must anchor.
    pub hash_col: usize,
    /// Marked span, as 0-based columns on the target line, end exclusive.
    pub start_col: usize,
    pub end_col: usize,
    /// Everything after the `:`, trimmed.
    pub payload: String,
}

/// Parse every annotation in a fixture, panicking on a malformed one.
pub fn parse(path: &Path, lines: &[&str]) -> Vec<Annotation> {
    let mut annotations = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        match parse_line(index, line) {
            Ok(Some(annotation)) => annotations.push(annotation),
            Ok(None) => {}
            Err(msg) => panic!("{}:{}: {msg}\n    {line}", path.display(), index + 1),
        }
    }
    annotations
}

/// Whether a line is an annotation that binds to the line above.
pub fn is_up_annotation(line: &str) -> bool {
    line.trim_start()
        .strip_prefix('#')
        .is_some_and(|rest| rest.starts_with(' ') && rest.trim_start().starts_with('^'))
}

/// Recognize an annotation, or return `None` for an ordinary line.
///
/// A comment that looks like an annotation but is not one — a marker run with
/// no `:` after it — is an error rather than an ordinary comment, so that a
/// miscounted or mistyped run cannot quietly assert nothing.
fn parse_line(index: usize, line: &str) -> Result<Option<Annotation>, String> {
    let Some(hash_col) = line.find(|c: char| !c.is_whitespace()) else {
        return Ok(None);
    };
    if line.as_bytes()[hash_col] != b'#' {
        return Ok(None);
    }
    let rest = &line[hash_col + 1..];

    let (bind, start_col, run) = match rest.as_bytes().first() {
        Some(b'~' | b':') => (Bind::Down, hash_col, rest),
        _ => {
            let caret = rest.len() - rest.trim_start().len();
            if rest.as_bytes().get(caret) != Some(&b'^') {
                // An ordinary comment.
                return Ok(None);
            }
            (Bind::Up, hash_col + 1 + caret, &rest[caret + 1..])
        }
    };

    // The `#` (down) or the `^` (up) is the first marked column; `~` extends it.
    let tildes = run.len() - run.trim_start_matches('~').len();
    let end_col = start_col + 1 + tildes;
    let payload = match run[tildes..].strip_prefix(':') {
        Some(payload) => payload.trim(),
        None if run.as_bytes().get(tildes) == Some(&b'^') => {
            return Err(
                "`^` may not appear in a run that binds to the line below; a run \
                        starting immediately after the `#` binds downward"
                    .to_owned(),
            );
        }
        None => return Err("expected `:` after the marker run".to_owned()),
    };
    if payload.is_empty() {
        return Err("expected something after `:`".to_owned());
    }

    Ok(Some(Annotation {
        line: index,
        bind,
        hash_col,
        start_col,
        end_col,
        payload: payload.to_owned(),
    }))
}

/// The line an annotation binds to: the nearest neighbour that is not itself an
/// annotation, checked to be one the annotation's columns can mark.
///
/// Annotations stack, so several may share one target, but nothing else may
/// come between an annotation and what it marks — a blank line in between is a
/// mistake rather than something to scan past.
pub fn target_line(annotations: &[Annotation], annotation: &Annotation, lines: &[&str]) -> usize {
    let no_target = || -> ! {
        panic!(
            "the annotation on line {} has no line to mark {} it",
            annotation.line + 1,
            match annotation.bind {
                Bind::Up => "above",
                Bind::Down => "below",
            }
        )
    };
    let mut line = annotation.line;
    let target = loop {
        line = match annotation.bind {
            Bind::Up => match line.checked_sub(1) {
                Some(line) => line,
                None => no_target(),
            },
            Bind::Down => line + 1,
        };
        if annotations.iter().any(|other| other.line == line) {
            continue;
        }
        match lines.get(line) {
            Some(text) if !text.trim().is_empty() => break line,
            _ => no_target(),
        }
    };
    check_target(annotation, target, lines);
    target
}

fn check_target(annotation: &Annotation, target: usize, lines: &[&str]) {
    let line = lines[target];
    assert!(
        line.is_ascii(),
        "line {} is annotated but is not ASCII; columns are byte offsets, so the \
         markers would not line up with what they appear to mark",
        target + 1
    );
    assert!(
        annotation.end_col <= line.len(),
        "the marker run on line {} is {} columns wide, but line {} — the line it \
         marks — is only {} columns long:\n    {line}",
        annotation.line + 1,
        annotation.end_col - annotation.start_col,
        target + 1,
        line.len(),
    );
    if annotation.bind == Bind::Down {
        let first = line.find(|c: char| !c.is_whitespace()).expect("not blank");
        assert!(
            annotation.hash_col == first,
            "the `#` on line {} sits at column {}, but a run that binds downward \
             marks the span starting at the `#` — so it must sit at column {}, \
             where the first token of the line below starts",
            annotation.line + 1,
            annotation.hash_col + 1,
            first + 1,
        );
    }
}

/// The marked line with the run redrawn beneath it.
///
/// The run is drawn as `^~~` whichever form the annotation used, so a down-form
/// annotation reads the same way as an up-form one in a failure.
pub fn excerpt(annotation: &Annotation, target: usize, lines: &[&str]) -> String {
    let mut out = format!("    {}\n    ", lines[target]);
    for _ in 0..annotation.start_col {
        out.push(' ');
    }
    out.push('^');
    for _ in annotation.start_col + 1..annotation.end_col {
        out.push('~');
    }
    out
}
