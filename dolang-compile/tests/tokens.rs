#![deny(warnings)]

//! Token stream tests driven by annotations written into fixture source.
//!
//! A fixture is an ordinary `.dol` file that compiles.  Expectations are
//! comments that point at a span of the line above or below them and name the
//! token that must cover exactly that span:
//!
//! ```text
//! echo hello
//! #    ^~~~: literal
//! ```
//!
//! Only annotated spans are asserted.  Tokens nobody points at may appear,
//! disappear or change kind freely, so a fixture does not have to be rewritten
//! every time the token stream changes elsewhere.
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
//! An annotation names a span on one line, so a token spanning several lines
//! cannot be annotated.  Nothing inside a here string can be annotated at all,
//! one line or many: a `#` there is content, not a comment.
//! Annotated lines must be ASCII, because columns are byte offsets and a
//! multi-byte character would silently slide the carets off the span they
//! appear to mark.

use std::{fmt::Write as _, fs, path::Path};

use dolang_compile::{Config, Context, Kind, NodeId, Token, diag};

#[test]
fn barewords() {
    check("tests/tokens/barewords.dol");
}

#[test]
fn compact_expr() {
    check("tests/tokens/compact_expr.dol");
}

#[test]
fn params() {
    check("tests/tokens/params.dol");
}

#[test]
fn strings() {
    check("tests/tokens/strings.dol");
}

/// Names for [`Token`], as written after the `:` in an annotation.
///
/// The match is exhaustive on purpose: a new token kind should not be able to
/// slip in without a name a fixture can ask for.
fn token_name(token: Token) -> &'static str {
    match token {
        Token::Comment => "comment",
        Token::Constant => "constant",
        Token::Delim => "delim",
        Token::Escape => "escape",
        Token::Field => "field",
        Token::Method => "method",
        Token::Key => "key",
        Token::ModuleName => "module_name",
        Token::ModuleItem => "module_item",
        Token::Keyword => "keyword",
        Token::Literal => "literal",
        Token::Number => "number",
        Token::Operator => "operator",
        Token::StringDelim => "string_delim",
        Token::Variable => "variable",
        Token::Sigil => "sigil",
    }
}

/// Every token kind, so a fixture naming a kind that does not exist is a
/// fixture error rather than a mismatch against every token in the file.
const ALL_TOKENS: [Token; 16] = [
    Token::Comment,
    Token::Constant,
    Token::Delim,
    Token::Escape,
    Token::Field,
    Token::Method,
    Token::Key,
    Token::ModuleName,
    Token::ModuleItem,
    Token::Keyword,
    Token::Literal,
    Token::Number,
    Token::Operator,
    Token::StringDelim,
    Token::Variable,
    Token::Sigil,
];

fn context_name(context: Context) -> &'static str {
    match context {
        Context::None => "none",
        Context::Call => "call",
    }
}

const ALL_CONTEXTS: [Context; 2] = [Context::None, Context::Call];

/// Names for what a token refers to, as written after `node=`.
///
/// `Kind` is `#[non_exhaustive]`, so this needs a fallback arm; [`NODE_NAMES`]
/// lists what a fixture may ask for.
fn node_name(kind: &Kind<'_>) -> &'static str {
    match kind {
        Kind::ImportItem { .. } => "import_item",
        Kind::ImportModule { .. } => "import_module",
        Kind::PreludeModule { .. } => "prelude_module",
        Kind::PreludeItem { .. } => "prelude_item",
        Kind::Class { .. } => "class",
        Kind::Function { .. } => "function",
        Kind::Bind { .. } => "bind",
        Kind::Method { .. } => "method",
        Kind::SpecialMethod { .. } => "special_method",
        Kind::Field { .. } => "field",
        Kind::PositionalParam { .. } => "positional_param",
        Kind::KeyParam { .. } => "key_param",
        Kind::RestParam { .. } => "rest_param",
        Kind::SelfParam { .. } => "self_param",
        Kind::Lambda => "lambda",
        Kind::If => "if",
        Kind::Else => "else",
        Kind::While => "while",
        Kind::For => "for",
        Kind::Try => "try",
        Kind::Catch => "catch",
        Kind::Finally => "finally",
        Kind::ForElem => "for_elem",
        Kind::IfElem => "if_elem",
        Kind::Decorator { .. } => "decorator",
        Kind::Break { .. } => "break",
        Kind::Continue { .. } => "continue",
        Kind::Return { .. } => "return",
        _ => "unknown",
    }
}

const NODE_NAMES: &[&str] = &[
    "import_item",
    "import_module",
    "prelude_module",
    "prelude_item",
    "class",
    "function",
    "bind",
    "method",
    "special_method",
    "field",
    "positional_param",
    "key_param",
    "rest_param",
    "self_param",
    "lambda",
    "if",
    "else",
    "while",
    "for",
    "try",
    "catch",
    "finally",
    "for_elem",
    "if_elem",
    "decorator",
    "break",
    "continue",
    "return",
    // A token that refers to no declaration at all.
    "none",
];

/// A token, flattened to the names a fixture writes.
struct Tok {
    kind: &'static str,
    node: &'static str,
    context: &'static str,
    start: (usize, usize),
    end: (usize, usize),
}

/// Which line an annotation binds to.
#[derive(Clone, Copy, PartialEq)]
enum Bind {
    /// A run containing `^`, binding to the line above.
    Up,
    /// A run starting immediately after the `#`, binding to the line below.
    Down,
}

struct Annotation {
    /// Line the annotation is written on, for error messages.
    line: usize,
    bind: Bind,
    /// Column the `#` sits at, which the down form must anchor.
    hash_col: usize,
    /// Marked span, as columns on the target line.
    start_col: usize,
    end_col: usize,
    kind: String,
    node: Option<String>,
    context: Option<String>,
}

fn check(path: &str) {
    let path = Path::new(path);
    let content = fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let source = std::str::from_utf8(&content)
        .unwrap_or_else(|e| panic!("{}: fixture is not UTF-8: {e}", path.display()));
    let lines: Vec<&str> = source.split('\n').collect();

    let annotations = parse(path, &lines);
    assert!(
        !annotations.is_empty(),
        "{}: fixture has no annotations",
        path.display()
    );
    let tokens = tokenize(path, &content);

    let mut failures = String::new();
    for annotation in &annotations {
        let target = target_line(&annotations, annotation, &lines);
        if let Err(report) = compare(annotation, target, &lines, &tokens) {
            let _ = write!(
                failures,
                "\n{}:{}: {report}",
                path.display(),
                annotation.line + 1
            );
        }
    }
    if !failures.is_empty() {
        panic!("{failures}");
    }
}

/// Compile the fixture and flatten its token stream.
fn tokenize(path: &Path, content: &[u8]) -> Vec<Tok> {
    let mut config = Config::new();
    config.document(true);
    // The default prelude pulls in more than a compile-only test needs; naming
    // the imports keeps `prelude_item` and `prelude_module` annotations
    // deterministic.
    config
        .prelude()
        .clear()
        .import_module("std")
        .import_items("std")
        .items(["str", "type", "dbg", "getter", "class", "static", "Record"])
        .commit();
    config.recover(true);

    let unit = config.unit(path, content);

    let errors: Vec<String> = unit
        .diagnostics()
        .filter(|diag| diag.severity() == diag::Severity::Error)
        .map(|diag| {
            format!(
                "  {}:{}: {}",
                path.display(),
                diag.span().start().line_number(),
                diag.message()
            )
        })
        .collect();
    assert!(
        errors.is_empty(),
        "{}: fixture does not compile:\n{}",
        path.display(),
        errors.join("\n")
    );

    let mut tokens = Vec::new();
    unit.tokens(
        &mut |token, span: diag::Span, node: Option<NodeId>, context| {
            tokens.push(Tok {
                kind: token_name(token),
                node: node
                    .and_then(|id| unit.node(id))
                    .map_or("none", |node| node_name(&node.kind())),
                context: context_name(context),
                start: (
                    span.start().line_offset() as usize,
                    span.start().column_offset() as usize,
                ),
                end: (
                    span.end().line_offset() as usize,
                    span.end().column_offset() as usize,
                ),
            });
        },
    );
    tokens
}

/// Parse every annotation in the fixture, panicking on a malformed one.
fn parse(path: &Path, lines: &[&str]) -> Vec<Annotation> {
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
        Some(payload) => payload,
        None if run.as_bytes().get(tildes) == Some(&b'^') => {
            return Err(
                "`^` may not appear in a run that binds to the line below; a run \
                        starting immediately after the `#` binds downward"
                    .to_owned(),
            );
        }
        None => return Err("expected `:` after the marker run".to_owned()),
    };

    let mut kind = None;
    let mut node = None;
    let mut context = None;
    for (position, field) in payload.split(',').enumerate() {
        let field = field.trim();
        match field.split_once('=') {
            None if position == 0 => {
                if field.is_empty() {
                    return Err("expected a token kind after `:`".to_owned());
                }
                if !ALL_TOKENS.iter().any(|token| token_name(*token) == field) {
                    return Err(format!("unknown token kind `{field}`"));
                }
                kind = Some(field.to_owned());
            }
            None => return Err(format!("expected `key=value`, found `{field}`")),
            Some(("node", value)) => {
                if !NODE_NAMES.contains(&value) {
                    return Err(format!("unknown node kind `{value}`"));
                }
                node = Some(value.to_owned());
            }
            Some(("context", value)) => {
                if !ALL_CONTEXTS
                    .iter()
                    .any(|context| context_name(*context) == value)
                {
                    return Err(format!("unknown context `{value}`"));
                }
                context = Some(value.to_owned());
            }
            Some((key, _)) => return Err(format!("unknown field `{key}`")),
        }
    }

    Ok(Some(Annotation {
        line: index,
        bind,
        hash_col,
        start_col,
        end_col,
        kind: kind.ok_or("expected a token kind after `:`")?,
        node,
        context,
    }))
}

/// The line an annotation binds to: the nearest neighbour that is not itself an
/// annotation.
///
/// Annotations stack, so several may share one target, but nothing else may
/// come between an annotation and what it marks — a blank line in between is a
/// mistake rather than something to scan past.
fn target_line(annotations: &[Annotation], annotation: &Annotation, lines: &[&str]) -> usize {
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
    loop {
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
            Some(text) if !text.trim().is_empty() => return line,
            _ => no_target(),
        }
    }
}

/// Check one annotation, returning a report if it does not hold.
fn compare(
    annotation: &Annotation,
    target: usize,
    lines: &[&str],
    tokens: &[Tok],
) -> Result<(), String> {
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

    let start = (target, annotation.start_col);
    let end = (target, annotation.end_col);
    let exact: Vec<&Tok> = tokens
        .iter()
        .filter(|token| token.start == start && token.end == end)
        .collect();

    match exact.as_slice() {
        [token] => {
            let mut wrong = Vec::new();
            if token.kind != annotation.kind {
                wrong.push(format!(
                    "expected {}, found {}",
                    annotation.kind, token.kind
                ));
            }
            if let Some(node) = &annotation.node
                && token.node != node
            {
                wrong.push(format!("expected node={node}, found node={}", token.node));
            }
            if let Some(context) = &annotation.context
                && token.context != context
            {
                wrong.push(format!(
                    "expected context={context}, found context={}",
                    token.context
                ));
            }
            if wrong.is_empty() {
                Ok(())
            } else {
                Err(format!(
                    "{}\n{}",
                    wrong.join("; "),
                    excerpt(annotation, target, lines)
                ))
            }
        }
        [] => Err(format!(
            "no token spans these columns exactly\n{}{}",
            excerpt(annotation, target, lines),
            overlapping(annotation, target, tokens)
        )),
        several => Err(format!(
            "{} tokens span these columns exactly: {}\n{}",
            several.len(),
            several
                .iter()
                .map(|token| token.kind)
                .collect::<Vec<_>>()
                .join(", "),
            excerpt(annotation, target, lines)
        )),
    }
}

/// The marked line with the run redrawn beneath it.
///
/// The run is drawn as `^~~` whichever form the annotation used, so a down-form
/// annotation reads the same way as an up-form one in a failure.
fn excerpt(annotation: &Annotation, target: usize, lines: &[&str]) -> String {
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

/// Every token touching the marked columns, so a miscounted run is visible.
fn overlapping(annotation: &Annotation, target: usize, tokens: &[Tok]) -> String {
    let mut out = String::new();
    for token in tokens {
        if token.start.0 != target
            || token.start.1 >= annotation.end_col
            || token.end.1 <= annotation.start_col
        {
            continue;
        }
        // Inclusive, 1-based: the columns an editor would show for the text the
        // token actually covers.
        let _ = write!(
            out,
            "\n      {} at columns {}-{}",
            token.kind,
            token.start.1 + 1,
            token.end.1
        );
    }
    if out.is_empty() {
        "\n    nothing overlaps those columns".to_owned()
    } else {
        format!("\n    overlapping tokens:{out}")
    }
}
