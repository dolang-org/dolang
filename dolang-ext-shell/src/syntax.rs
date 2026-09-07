use std::{cmp::Reverse, ops::Range};

use anstyle::{AnsiColor, Style};
use dolang::compile::{Context, Kind, Span, Token};

/// A token together with what it refers to, ready to be colored.
///
/// The classification is resolved when the token is collected rather than
/// carried as a node identity, because a [`Kind`] borrows the unit that
/// produced it and these outlive that borrow.
pub type SemanticToken = (Token, Span, NodeClass, Context);

/// What a token refers to, at the granularity coloring cares about.
#[derive(Clone, Copy)]
pub enum NodeClass {
    Normal,
    Param,
    Function,
    Module,
    Prelude,
    PreludeModule,
}

/// Classify what a token refers to.
///
/// Pass the [`Kind`] of the node the token names, if it names one.
pub fn classify_node(kind: Option<&Kind<'_>>) -> NodeClass {
    match kind {
        Some(
            Kind::PositionalParam { .. }
            | Kind::KeyParam { .. }
            | Kind::RestParam { .. }
            | Kind::SelfParam { .. },
        ) => NodeClass::Param,
        Some(
            Kind::Class { .. }
            | Kind::Function { .. }
            | Kind::Method { .. }
            | Kind::SpecialMethod { .. },
        ) => NodeClass::Function,
        Some(Kind::ImportModule { .. }) => NodeClass::Module,
        Some(Kind::PreludeItem { .. }) => NodeClass::Prelude,
        Some(Kind::PreludeModule { .. }) => NodeClass::PreludeModule,
        _ => NodeClass::Normal,
    }
}

fn token_style(token: Token, class: NodeClass, context: Context) -> Option<Style> {
    let color = match token {
        Token::Comment => {
            return Some(
                Style::new()
                    .fg_color(Some(AnsiColor::White.into()))
                    .dimmed(),
            );
        }
        Token::Keyword => AnsiColor::Red,
        Token::Literal => AnsiColor::Green,
        Token::Operator | Token::Delim | Token::Escape | Token::Key => AnsiColor::Yellow,
        Token::StringDelim | Token::ModuleItem | Token::Number => AnsiColor::Cyan,
        // `true`, `nil` and symbols name a value rather than compute one, which
        // is what a number does, so the two do not share a color.
        Token::Constant | Token::ModuleName => AnsiColor::Magenta,
        Token::Field => match context {
            Context::Call => AnsiColor::Blue,
            Context::None => AnsiColor::Cyan,
        },
        Token::Method => AnsiColor::Blue,
        Token::Sigil => AnsiColor::White,
        Token::Variable => match context {
            Context::Call => AnsiColor::Blue,
            Context::None => match class {
                NodeClass::Function => AnsiColor::Blue,
                NodeClass::Module | NodeClass::Param | NodeClass::PreludeModule => {
                    AnsiColor::Magenta
                }
                NodeClass::Prelude => AnsiColor::Cyan,
                NodeClass::Normal => return None,
            },
        },
    };
    Some(Style::new().fg_color(Some(color.into())))
}

fn push_sanitized(out: &mut String, value: &str) {
    for ch in value.chars() {
        if ch == '\t' || ch == '\n' || ch == '\r' || !ch.is_control() {
            out.push(ch);
        } else {
            out.push('\u{fffd}');
        }
    }
}

fn push_styled(out: &mut String, value: &str, style: Option<Style>, color: bool) {
    if color && let Some(style) = style {
        out.push_str(&style.to_string());
        push_sanitized(out, value);
        out.push_str(&style.render_reset().to_string());
    } else {
        push_sanitized(out, value);
    }
}

pub fn highlight_range(
    source: &str,
    tokens: &[SemanticToken],
    range: Range<usize>,
    color: bool,
) -> String {
    let mut sorted = tokens.iter().collect::<Vec<_>>();
    sorted.sort_by_key(|(_, span, _, _)| {
        (
            span.start().byte_offset(),
            Reverse(span.end().byte_offset()),
        )
    });

    let mut out = String::new();
    let mut last_end = range.start;
    for (token, span, class, context) in sorted {
        let start = span.start().byte_offset().max(range.start).max(last_end);
        let end = span.end().byte_offset().min(range.end);
        if end <= start {
            continue;
        }
        if start > last_end {
            push_sanitized(&mut out, &source[last_end..start]);
        }
        push_styled(
            &mut out,
            &source[start..end],
            token_style(*token, *class, *context),
            color,
        );
        last_end = end;
    }
    if last_end < range.end {
        push_sanitized(&mut out, &source[last_end..range.end]);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use dolang::compile::{Config, NodeId};

    use super::*;

    #[test]
    fn highlights_a_source_range_without_changing_plain_text() {
        let source = "let answer = 42";
        let mut tokens = Vec::new();
        let unit = Config::new()
            .document(true)
            .recover(true)
            .unit(Path::new("example.dol"), source.as_bytes());
        unit.tokens(&mut |token, span, node: Option<NodeId>, context| {
            let kind = node.and_then(|id| unit.node(id)).map(|node| node.kind());
            tokens.push((token, span, classify_node(kind.as_ref()), context));
        });
        assert_eq!(
            highlight_range(source, &tokens, 0..source.len(), false),
            source
        );
        assert_eq!(
            highlight_range(source, &tokens, 0..source.len(), true),
            "\u{1b}[31mlet\u{1b}[0m answer \u{1b}[33m=\u{1b}[0m \u{1b}[36m42\u{1b}[0m"
        );
    }
}
