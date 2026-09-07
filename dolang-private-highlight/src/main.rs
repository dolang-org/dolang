use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

use clap::Parser;
use dolang::compile::{Config, Context, Kind, NodeId, Pos, Severity, Span, Token};

use serde_json::{Value, json};

#[derive(Parser)]
struct Cli {
    /// Source path
    path: Option<PathBuf>,
}

fn pos_repr(pos: &Pos) -> Value {
    json!({
        "offset": pos.byte_offset(),
        "line": pos.line_offset(),
        "col": pos.column_offset(),
    })
}

fn span_repr(span: &Span) -> Value {
    json!({
        "start": pos_repr(&span.start()),
        "end": pos_repr(&span.end()),
    })
}

fn token_kind(token: &Token) -> &'static str {
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
        Token::Operator => "operand",
        Token::StringDelim => "string_delim",
        Token::Variable => "variable",
        Token::Sigil => "sigil",
    }
}

fn severity_kind(severity: &Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        _ => "error",
    }
}

/// Name the kind of declaration a token refers to.
///
/// This vocabulary is its own, not the token vocabulary: it answers what a name
/// *is*, where `kind` answers what a span of text looks like.
fn node_kind(kind: &Kind<'_>) -> &'static str {
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
        Kind::PositionalParam { .. } | Kind::KeyParam { .. } | Kind::RestParam { .. } => "param",
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

fn context_kind(context: &Context) -> Option<&'static str> {
    match context {
        Context::None => None,
        Context::Call => Some("call"),
    }
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();
    let (path, content) = if let Some(path) = &cli.path {
        (path.as_ref(), fs::read(path)?)
    } else {
        let mut content = vec![];
        io::stdin().read_to_end(&mut content)?;
        (Path::new("<stdin>"), content)
    };
    let unit = Config::new()
        .document(true)
        .recover(true)
        .unit(path, &content);
    let mut tokens = vec![];
    unit.tokens(&mut |token, span, node: Option<NodeId>, context| {
        let mut obj = json!({
            "kind": token_kind(&token),
            "span": span_repr(&span),
        });
        if let Some(kind) = node.and_then(|id| unit.node(id)).map(|node| node.kind()) {
            obj.as_object_mut()
                .unwrap()
                .insert("node".into(), node_kind(&kind).into());
        }
        if let Some(context) = context_kind(&context) {
            obj.as_object_mut()
                .unwrap()
                .insert("context".into(), context.into());
        }
        tokens.push(obj);
    });
    let diagnostics: Vec<_> = unit
        .diagnostics()
        .map(|diag| {
            json!({
                "kind": severity_kind(&diag.severity()),
                "span": span_repr(&diag.span()),
            })
        })
        .collect();
    let result: Vec<_> = tokens.into_iter().chain(diagnostics).collect();
    serde_json::to_writer_pretty(io::stdout(), &result)?;
    Ok(())
}
