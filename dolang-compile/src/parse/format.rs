use std::collections::VecDeque;

use super::{
    Error, ExprMode, Parser, Result, Scope,
    diag::{BadFmtParamName, FmtParamOutsideSeq},
    stream::ExpectKind,
    string::StrKind,
};
use crate::{
    ast::{
        Expr, FmtParamName, FormatAlign, FormatKind, FormatSign, FormatSpec, FormatValue,
        visit::Node,
    },
    lex::{Keyword, Token, TokenInfo},
    source::Span,
};

enum FormatAtom {
    Static(FormatValue<char>),
    Dynamic(Expr),
}

/// An escape is data, never specification syntax, so the only place one may
/// appear is the fill.
const FORMAT_ESCAPE_MESSAGE: &str =
    "escapes are only valid as the fill character in format specifications";

impl Parser<'_> {
    fn next_format_atom(&mut self, scope: &mut Scope) -> Result<Option<FormatAtom>> {
        use TokenInfo::*;

        let token = match self.next()? {
            Some(token!(RightBrace)) => return Ok(None),
            Some(token @ token!(StmtSep | Indent | Dedent)) => {
                return Err(self.syntax_error(
                    scope,
                    Some(token),
                    "newlines are not valid in format specifications",
                ));
            }
            Some(token @ token!(DQuote)) => {
                return Err(self.syntax_error(
                    scope,
                    Some(token),
                    "expected closing `}` in formatted interpolation",
                ));
            }
            None => {
                return Err(self.syntax_error(
                    scope,
                    None,
                    "expected closing `}` in formatted interpolation",
                ));
            }
            Some(token!(Dollar, dollar_span)) => {
                let expr = match decay_ident!(self.peek()?) {
                    Some(token!(Ident | Key)) => {
                        let expr = self.parse_expr_primary(scope, ExprMode::Compact)?;
                        if !matches!(expr, Expr::Ident(_)) {
                            unreachable!()
                        }
                        expr
                    }
                    Some(token!(LeftParen)) => self.parse_expr_primary(scope, ExprMode::Compact)?,
                    other => {
                        return Err(self.syntax_error(
                            scope,
                            other,
                            "format width and precision substitutions must be `$name` or `$(expr)`",
                        ));
                    }
                };
                return Ok(Some(FormatAtom::Dynamic(Self::dollar_group(
                    expr,
                    dollar_span,
                ))));
            }
            Some(token @ token!(EscapeByte(_))) => {
                return Err(self.syntax_error(
                    scope,
                    Some(token),
                    "\\x escapes are only valid in binary strings",
                ));
            }
            Some(token @ token!(Escape(_))) => {
                return Err(self.syntax_error(scope, Some(token), FORMAT_ESCAPE_MESSAGE));
            }
            Some(token) => token,
        };

        let token = match token.info {
            Key => Token {
                info: Literal,
                span: token.span | token.span.after_right_char(),
            },
            DittoKey => Token {
                info: Literal,
                span: token.span.before_left_char() | token.span,
            },
            Sym => Token {
                info: Literal,
                span: token.span.before_left_char() | token.span.after_right_char(),
            },
            _ => decay_string!(Some(token)).expect("format token disappeared"),
        };
        let text = self.file.str(token.span);
        let ch = text.chars().next().expect("empty lexer token");
        if matches!(ch, '\n' | '\r') {
            return Err(self.syntax_error(
                scope,
                Some(token),
                "newlines are not valid in format specifications",
            ));
        }
        let split = token.span.start + u32::try_from(ch.len_utf8()).unwrap();
        let span = Span {
            start: token.span.start,
            end: split,
        };
        if split != token.span.end {
            self.lex.push(Token {
                info: Literal,
                span: Span {
                    start: split,
                    end: token.span.end,
                },
            });
        }
        Ok(Some(FormatAtom::Static(FormatValue { value: ch, span })))
    }

    /// Parses the name of a `${#...}` parameter.
    ///
    /// A name is an integer or an identifier, and either way it is a name: an
    /// explicit position is never renumbered, so `#0` means parameter `0`
    /// wherever the sequence it sits in ends up.
    fn parse_fmt_param_name(&mut self, scope: &mut Scope) -> Result<FmtParamName> {
        match self.next()? {
            Some(token!(TokenInfo::Int(value), span)) => match u32::try_from(value) {
                Ok(value) => Ok(FmtParamName::Pos(value, span)),
                Err(_) => {
                    self.fail = true;
                    self.diags.push(BadFmtParamName(span));
                    Ok(FmtParamName::Pos(0, span))
                }
            },
            Some(token!(TokenInfo::Ident, span)) => Ok(FmtParamName::Named(span)),
            // The lexer glues a name to a following `:`, so `${#foo:>8}` hands
            // back a `Key`. Its span excludes the colon; put the colon back and
            // the shared specification path runs unchanged.
            Some(token!(TokenInfo::Key, span)) => {
                self.push_colon(span.after_right_char());
                Ok(FmtParamName::Named(span))
            }
            other => Err(self.syntax_error(
                scope,
                other,
                "expected a parameter name or number after `#`",
            )),
        }
    }

    /// Parses a `$#0` or `$#foo`: the shorthand for a parameter that states no
    /// specification.
    ///
    /// A name ends at the first character that cannot be part of one, so the
    /// braces have nothing left to delimit. `${#...}` remains the form that
    /// takes a specification.
    pub(super) fn parse_fmt_param_short(
        &mut self,
        scope: &mut Scope,
        dollar_span: Span,
        hash_span: Span,
        kind: StrKind,
    ) -> Result<Expr> {
        if kind != StrKind::Fmt {
            self.fail = true;
            self.diags.push(FmtParamOutsideSeq(hash_span));
        }
        // A name glued to a `:` comes back as a `Key`, and the name parser puts
        // the colon back. Outside a specification it is ordinary text, which is
        // what the string loop makes of it.
        let name = self.parse_fmt_param_name(scope)?;
        Ok(Expr::FmtParam {
            name,
            spec: Box::new(FormatSpec {
                fill: None,
                zero: None,
                align: None,
                sign: None,
                alt: None,
                width: None,
                precision: None,
                kind: None,
            }),
            dollar_span,
            hash_span,
            brace_span: None,
            colon_span: None,
        })
    }

    pub(super) fn parse_fmt_interp(
        &mut self,
        scope: &mut Scope,
        dollar_span: Span,
        kind: StrKind,
    ) -> Result<Expr> {
        let left = self.expect(scope, &[ExpectKind::LeftBrace])?;
        // `${#0}` and `${#foo}` name an unbound position rather than
        // interpolating a value. A hole only means something in a sequence,
        // which is what keeps the segments apart to fill in later.
        let param = match self.peek()? {
            Some(token!(TokenInfo::Hash, hash_span)) => {
                self.advance();
                if kind != StrKind::Fmt {
                    self.fail = true;
                    self.diags.push(FmtParamOutsideSeq(hash_span));
                }
                Some((hash_span, self.parse_fmt_param_name(scope)?))
            }
            _ => None,
        };
        let value = match param {
            Some(_) => Expr::Error,
            None => self.parse_expr(scope, ExprMode::Compact)?,
        };
        let mut spec = FormatSpec {
            fill: None,
            zero: None,
            align: None,
            sign: None,
            alt: None,
            width: None,
            precision: None,
            kind: None,
        };
        // An interpolation need not state a specification at all: `${x}` binds
        // the value to nothing, leaving the kind to the surrounding conversion.
        if let Some(token!(TokenInfo::RightBrace, right)) = self.peek()? {
            self.advance();
            return Ok(Self::fmt_interp(
                param,
                value,
                spec,
                dollar_span,
                left | right,
                None,
            ));
        }
        let colon_span = match self.next()? {
            Some(token!(TokenInfo::Colon, span)) => span,
            Some(token!(TokenInfo::DittoKey, span)) => {
                self.lex.push(Token {
                    info: TokenInfo::Literal,
                    span,
                });
                span.before_left_char()
            }
            Some(token!(TokenInfo::Sym, span)) => {
                self.lex.push(Token {
                    info: TokenInfo::Literal,
                    span: span | span.after_right_char(),
                });
                span.before_left_char()
            }
            Some(token) => {
                let span = token.span;
                if self.file.str(span).starts_with(':') {
                    let colon = span.left_char();
                    if span.end != colon.end {
                        self.lex.push(Token {
                            info: TokenInfo::Literal,
                            span: Span {
                                start: colon.end,
                                end: span.end,
                            },
                        });
                    }
                    colon
                } else {
                    return Err(self.syntax_error(scope, Some(token), "expected `:`"));
                }
            }
            None => return Err(self.syntax_error(scope, None, "expected `:`")),
        };
        let align = |ch| match ch {
            '<' => Some(FormatAlign::Left),
            '>' => Some(FormatAlign::Right),
            '^' => Some(FormatAlign::Center),
            _ => None,
        };

        // A leading escape is the fill, so an alignment must follow it.
        // `next_format_atom` refuses an escape anywhere else.
        if let Some(escape) = self.peek()?
            && let TokenInfo::Escape(ch) = escape.info
        {
            self.advance();
            let aligned = match self.peek()? {
                Some(token!(TokenInfo::RightBrace)) => None,
                _ => match self.next_format_atom(scope)? {
                    Some(FormatAtom::Static(second)) => {
                        align(second.value).map(|value| FormatValue {
                            value,
                            span: second.span,
                        })
                    }
                    _ => None,
                },
            };
            let Some(aligned) = aligned else {
                return Err(self.syntax_error(scope, Some(escape), FORMAT_ESCAPE_MESSAGE));
            };
            spec.fill = Some(FormatValue {
                value: ch,
                span: escape.span,
            });
            spec.align = Some(aligned);
        }

        let mut atoms = VecDeque::new();
        let right = loop {
            match self.peek()? {
                Some(token!(TokenInfo::RightBrace, span)) => {
                    self.advance();
                    break span;
                }
                _ => atoms.push_back(self.next_format_atom(scope)?.ok_or(Error)?),
            }
        };

        if spec.align.is_none() && matches!(atoms.front(), Some(FormatAtom::Static(_))) {
            let first = match atoms.pop_front().unwrap() {
                FormatAtom::Static(first) => first,
                FormatAtom::Dynamic(_) => unreachable!(),
            };
            if let Some(FormatAtom::Static(second)) = atoms.front()
                && let Some(value) = align(second.value)
            {
                spec.fill = Some(first);
                let second = match atoms.pop_front().unwrap() {
                    FormatAtom::Static(second) => second,
                    FormatAtom::Dynamic(_) => unreachable!(),
                };
                spec.align = Some(FormatValue {
                    value,
                    span: second.span,
                });
            } else if let Some(value) = align(first.value) {
                spec.align = Some(FormatValue {
                    value,
                    span: first.span,
                });
            } else {
                atoms.push_front(FormatAtom::Static(first));
            }
        }

        if let Some(FormatAtom::Static(value)) = atoms.front()
            && matches!(value.value, '+' | ' ')
        {
            let value = match atoms.pop_front().unwrap() {
                FormatAtom::Static(value) => value,
                FormatAtom::Dynamic(_) => unreachable!(),
            };
            spec.sign = Some(FormatValue {
                value: if value.value == '+' {
                    FormatSign::Plus
                } else {
                    FormatSign::Space
                },
                span: value.span,
            });
        }
        if let Some(FormatAtom::Static(value)) = atoms.front()
            && value.value == '#'
        {
            spec.alt = Some(value.span);
            atoms.pop_front();
        }
        if let Some(FormatAtom::Static(value)) = atoms.front()
            && value.value == '0'
        {
            spec.zero = Some(value.span);
            atoms.pop_front();
        }

        spec.width = self.parse_format_count(scope, &mut atoms)?;
        if let Some(FormatAtom::Static(value)) = atoms.front()
            && value.value == '.'
        {
            let dot = value.span;
            atoms.pop_front();
            spec.precision = self.parse_format_count(scope, &mut atoms)?;
            if spec.precision.is_none() {
                return Err(self.syntax_error(
                    scope,
                    Some(Token {
                        info: TokenInfo::Literal,
                        span: dot,
                    }),
                    "expected precision after `.`",
                ));
            }
        }

        if let Some(FormatAtom::Static(value)) = atoms.front() {
            let kind = match value.value {
                's' => Some(FormatKind::Str),
                '?' => Some(FormatKind::Dbg),
                '!' => Some(FormatKind::Verbatim),
                'x' => Some(FormatKind::Hex),
                'o' => Some(FormatKind::Oct),
                'b' => Some(FormatKind::Bin),
                'd' => Some(FormatKind::Dec),
                'e' => Some(FormatKind::Exp),
                'f' => Some(FormatKind::Fixed),
                _ => None,
            };
            if let Some(kind) = kind {
                spec.kind = Some(FormatValue {
                    value: kind,
                    span: value.span,
                });
                atoms.pop_front();
            }
        }
        if !atoms.is_empty() {
            let span = match atoms.front().unwrap() {
                FormatAtom::Static(value) => value.span,
                FormatAtom::Dynamic(expr) => expr.span(),
            };
            return Err(self.syntax_error(
                scope,
                Some(Token {
                    info: TokenInfo::Literal,
                    span,
                }),
                "invalid format specification",
            ));
        }

        Ok(Self::fmt_interp(
            param,
            value,
            spec,
            dollar_span,
            left | right,
            Some(colon_span),
        ))
    }

    /// Builds the interpolation just parsed: a hole when one was named, an
    /// ordinary bound interpolation otherwise.
    fn fmt_interp(
        param: Option<(Span, FmtParamName)>,
        value: Expr,
        spec: FormatSpec,
        dollar_span: Span,
        brace_span: Span,
        colon_span: Option<Span>,
    ) -> Expr {
        match param {
            Some((hash_span, name)) => Expr::FmtParam {
                name,
                spec: Box::new(spec),
                dollar_span,
                hash_span,
                brace_span: Some(brace_span),
                colon_span,
            },
            None => Expr::Fmt {
                value: Box::new(value),
                spec: Box::new(spec),
                dollar_span,
                brace_span,
                colon_span,
            },
        }
    }

    fn parse_format_count(
        &mut self,
        scope: &mut Scope,
        atoms: &mut VecDeque<FormatAtom>,
    ) -> Result<Option<Expr>> {
        if matches!(atoms.front(), Some(FormatAtom::Dynamic(_))) {
            let FormatAtom::Dynamic(expr) = atoms.pop_front().unwrap() else {
                unreachable!()
            };
            return Ok(Some(expr));
        }
        let mut value = 0u32;
        let mut span: Option<Span> = None;
        while let Some(FormatAtom::Static(atom)) = atoms.front()
            && atom.value.is_ascii_digit()
        {
            let atom = match atoms.pop_front().unwrap() {
                FormatAtom::Static(atom) => atom,
                FormatAtom::Dynamic(_) => unreachable!(),
            };
            value = match value
                .checked_mul(10)
                .and_then(|v| v.checked_add(atom.value.to_digit(10).unwrap()))
            {
                Some(value) => value,
                None => {
                    return Err(self.syntax_error(
                        scope,
                        Some(Token {
                            info: TokenInfo::Literal,
                            span: atom.span,
                        }),
                        "format count is too large",
                    ));
                }
            };
            span = Some(span.map_or(atom.span, |span| span | atom.span));
        }
        Ok(span.map(|span| Expr::Int(i128::from(value), span)))
    }
}
