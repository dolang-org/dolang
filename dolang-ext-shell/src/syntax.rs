use std::{cmp::Reverse, ops::Range};

use anstyle::{AnsiColor, Style};
use dolang::compile::{Context, Origin, Span, Token};

pub type SemanticToken = (Token, Span, Option<Origin>, Context);

#[derive(Clone, Copy)]
enum OriginClass {
    Normal,
    Param,
    Function,
    Module,
    Prelude,
    PreludeModule,
}

fn classify_origin(origin: Option<&Origin>) -> OriginClass {
    match origin {
        None | Some(Origin::Bind { .. }) | Some(Origin::Field { .. }) => OriginClass::Normal,
        Some(Origin::Param { .. }) | Some(Origin::SelfParam { .. }) => OriginClass::Param,
        Some(Origin::Class { .. }) | Some(Origin::Def { .. }) | Some(Origin::Method { .. }) => {
            OriginClass::Function
        }
        Some(Origin::ImportModule { .. }) => OriginClass::Module,
        Some(Origin::ImportItem { .. }) => OriginClass::Normal,
        Some(Origin::PreludeItem { .. }) => OriginClass::Prelude,
        Some(Origin::PreludeModule { .. }) => OriginClass::PreludeModule,
    }
}

fn token_style(token: Token, origin: Option<&Origin>, context: Context) -> Option<Style> {
    let color = match token {
        Token::Comment => {
            return Some(
                Style::new()
                    .fg_color(Some(AnsiColor::White.into()))
                    .dimmed(),
            );
        }
        Token::Keyword => AnsiColor::Red,
        Token::Literal | Token::Key => AnsiColor::Green,
        Token::Operator | Token::Delim | Token::Escape => AnsiColor::Yellow,
        Token::StringDelim | Token::ModuleItem => AnsiColor::Cyan,
        Token::Number | Token::Constant | Token::ModuleName => AnsiColor::Magenta,
        Token::Field => match context {
            Context::Call => AnsiColor::Blue,
            Context::None => AnsiColor::Cyan,
        },
        Token::Method => AnsiColor::Blue,
        Token::Sigil => AnsiColor::White,
        Token::Variable => match context {
            Context::Call => AnsiColor::Blue,
            Context::None => match classify_origin(origin) {
                OriginClass::Function => AnsiColor::Blue,
                OriginClass::Module | OriginClass::Param | OriginClass::PreludeModule => {
                    AnsiColor::Magenta
                }
                OriginClass::Prelude => AnsiColor::Cyan,
                OriginClass::Normal => return None,
            },
        },
    };
    Some(Style::new().fg_color(Some(color.into())))
}

fn push_sanitized(out: &mut String, value: &str) {
    for ch in value.chars() {
        if ch == '\t' || !ch.is_control() {
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
    for (token, span, origin, context) in sorted {
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
            token_style(*token, origin.as_ref(), *context),
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
    use std::{ops::ControlFlow, path::Path};

    use dolang::compile::{Compiler, Diag};

    use super::*;

    #[test]
    fn highlights_a_source_range_without_changing_plain_text() {
        let source = "let answer = 42";
        let mut tokens = Vec::new();
        let _ = Compiler::new(Path::new("example.dol"), source.as_bytes()).analyze(
            &mut |_: Diag| ControlFlow::<()>::Continue(()),
            &mut |token, span, origin, context| {
                tokens.push((token, span, origin, context));
                ControlFlow::<()>::Continue(())
            },
        );
        assert_eq!(
            highlight_range(source, &tokens, 0..source.len(), false),
            source
        );
        assert!(highlight_range(source, &tokens, 0..source.len(), true).contains("\u{1b}["));
    }
}
