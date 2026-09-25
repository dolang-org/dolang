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
//! See [`dolang_private_test::annotate`] for the annotation forms and their
//! limits.

use std::{fmt::Write as _, fs, path::Path};

use dolang::compile::{Config, Context, Kind, NodeId, Severity, Span, Token};
use dolang_private_test::annotate::{self, Annotation};

/// Names for [`Token`], as written after the `:` in an annotation.
///
/// The match is exhaustive on purpose: a new token kind should not be able to
/// slip in without a name a fixture can ask for.
fn token_name(token: Token) -> &'static str {
    match token {
        Token::Annotation => "annotation",
        Token::Binder => "binder_token",
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
        Token::Type => "type_token",
        Token::TypeKey => "type_key",
        Token::Variable => "variable",
        Token::Sigil => "sigil",
    }
}

/// Every token kind, so a fixture naming a kind that does not exist is a
/// fixture error rather than a mismatch against every token in the file.
const ALL_TOKENS: [Token; 20] = [
    Token::Annotation,
    Token::Binder,
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
    Token::Type,
    Token::TypeKey,
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
        Kind::Root => "root",
        Kind::Alias { .. } => "alias",
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
        Kind::Type { .. } => "type",
        Kind::Binder { .. } => "binder",
        _ => "unknown",
    }
}

const NODE_NAMES: &[&str] = &[
    "alias",
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
    "binder",
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

/// What a token annotation asserts.
struct Expect {
    kind: String,
    node: Option<String>,
    context: Option<String>,
}

fn run(path: &Path) {
    let content = fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let source = std::str::from_utf8(&content)
        .unwrap_or_else(|e| panic!("{}: fixture is not UTF-8: {e}", path.display()));
    let lines: Vec<&str> = source.split('\n').collect();

    let annotations = annotate::parse(path, &lines);
    assert!(
        !annotations.is_empty(),
        "{}: fixture has no annotations",
        path.display()
    );
    let tokens = tokenize(path, &content);

    let mut failures = String::new();
    for annotation in &annotations {
        let expect = expect(&annotation.payload).unwrap_or_else(|msg| {
            panic!(
                "{}:{}: {msg}\n    {}",
                path.display(),
                annotation.line + 1,
                lines[annotation.line]
            )
        });
        let target = annotate::target_line(&annotations, annotation, &lines);
        if let Err(report) = compare(annotation, &expect, target, &lines, &tokens) {
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
        .filter(|diag| diag.severity() == Severity::Error)
        .map(|diag| {
            format!(
                "  {}:{}: {}",
                path.display(),
                diag.span().span().start().line_number(),
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
    unit.tokens(&mut |token, span: Span, node: Option<NodeId>, context| {
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
    });
    tokens
}

/// Parse a token annotation's payload: a token kind, then `key=value` fields.
fn expect(payload: &str) -> Result<Expect, String> {
    let mut kind = None;
    let mut node = None;
    let mut context = None;
    for (position, field) in payload.split(',').enumerate() {
        let field = field.trim();
        match field.split_once('=') {
            None if position == 0 => {
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
    Ok(Expect {
        kind: kind.ok_or("expected a token kind after `:`")?,
        node,
        context,
    })
}

/// Check one annotation, returning a report if it does not hold.
fn compare(
    annotation: &Annotation,
    expect: &Expect,
    target: usize,
    lines: &[&str],
    tokens: &[Tok],
) -> Result<(), String> {
    let start = (target, annotation.start_col);
    let end = (target, annotation.end_col);
    let exact: Vec<&Tok> = tokens
        .iter()
        .filter(|token| token.start == start && token.end == end)
        .collect();

    match exact.as_slice() {
        [token] => {
            let mut wrong = Vec::new();
            if token.kind != expect.kind {
                wrong.push(format!("expected {}, found {}", expect.kind, token.kind));
            }
            if let Some(node) = &expect.node
                && token.node != node
            {
                wrong.push(format!("expected node={node}, found node={}", token.node));
            }
            if let Some(context) = &expect.context
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
                    annotate::excerpt(annotation, target, lines)
                ))
            }
        }
        [] => Err(format!(
            "no token spans these columns exactly\n{}{}",
            annotate::excerpt(annotation, target, lines),
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
            annotate::excerpt(annotation, target, lines)
        )),
    }
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

include!(concat!(env!("OUT_DIR"), "/generated_token_tests.rs"));
