use super::{ExprMode, Parser, Result, Scope};
use crate::{
    ast::{Expr, Ident},
    lex::{self, Op, Token, TokenInfo},
    source::Span,
};

/// Which flavor of string literal is being parsed.
///
/// The flavors differ in more than their delimiters: `b"..."` builds bytes,
/// `r|` takes its content literally, and `t"..."` keeps its interpolations
/// apart instead of concatenating them, so the parse has to know which one it
/// is from the opening token onward.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum StrKind {
    /// `"..."` and `|`
    Str,
    /// `b"..."`
    Bin,
    /// `r|`
    Raw,
    /// `t"..."` and `t|`
    Fmt,
}

impl Parser<'_> {
    pub(super) fn parse_quoted_string(
        &mut self,
        scope: &mut Scope,
        open: Span,
        kind: StrKind,
    ) -> Result<Expr> {
        use TokenInfo::*;

        let bin = kind == StrKind::Bin;

        let expr = self.with_mode(lex::Mode::String, |this| {
            let mut exprs = Vec::new();
            let close = loop {
                let expr = match this.peek()? {
                    token @ (None | Some(token!(StmtSep | Indent | Dedent))) => {
                        this.syntax_error(scope, token, "expected closing `\"`");
                        // Try to recover by considering string ended here
                        break open;
                    }
                    Some(token!(Dollar, dollar_span)) => {
                        this.advance();
                        if let Some(token!(LeftBrace)) = this.peek()? {
                            if bin {
                                let token = this.peek()?;
                                return Err(this.syntax_error(
                                    scope,
                                    token,
                                    "formatted interpolation is not valid in binary strings",
                                ));
                            }
                            this.parse_fmt_interp(scope, dollar_span, kind)?
                        } else if let Some(token!(Hash, hash_span)) = this.peek()? {
                            this.advance();
                            this.parse_fmt_param_short(scope, dollar_span, hash_span, kind)?
                        } else {
                            let expr = this.parse_expr_primary(scope, ExprMode::Compact)?;
                            // The sigil is part of how the interpolation was
                            // written, so it belongs to the interpolation's
                            // span: a sequence needs it to delimit the segment,
                            // and every string needs it for the sigil to be a
                            // token rather than a gap between the literal text
                            // and the name.
                            Self::dollar_group(expr, dollar_span)
                        }
                    }
                    Some(token!(DQuote)) => break this.advance(),
                    Some(_) => match decay_string!(this.next()?) {
                        Some(token!(Literal, span)) => Expr::Literal(span),
                        Some(token!(Key, span)) => Expr::Literal(span | span.after_right_char()),
                        Some(token!(DittoKey, span)) => {
                            Expr::Literal(span.before_left_char() | span)
                        }
                        Some(token!(Sym, span)) => {
                            Expr::Literal(span.before_left_char() | span.after_right_char())
                        }
                        Some(token!(Escape(c), span)) => Expr::Escape(c, span),
                        Some(token @ token!(EscapeByte(..), _)) => {
                            if bin {
                                let TokenInfo::EscapeByte(b) = token.info else {
                                    unreachable!()
                                };
                                Expr::EscapeByte(b, token.span)
                            } else {
                                return Err(this.syntax_error(
                                    scope,
                                    Some(token),
                                    "\\x escapes are only valid in binary strings",
                                ));
                            }
                        }
                        _ => unreachable!(),
                    },
                };
                exprs.push(expr);
            };
            match kind {
                StrKind::Bin => Ok(Expr::BinConcat { exprs, open, close }),
                StrKind::Fmt => Ok(Expr::FmtSeq {
                    exprs,
                    open,
                    close: Some(close),
                }),
                StrKind::Str | StrKind::Raw => Ok(Expr::Concat {
                    exprs,
                    delim_span: Some(open | close),
                    verbatim: false,
                }),
            }
        })?;
        Ok(expr.optimize())
    }

    /// After consuming `|`, optionally consume `-` for strip mode.
    /// Returns `(intro_span, strip)` where `intro_span` covers `|` or `|-`.
    /// Classifies the here-string about to be parsed from its opening bar.
    pub(super) fn heredoc_kind(&mut self) -> Result<StrKind> {
        Ok(match self.peek()? {
            Some(token!(TokenInfo::RBar)) => StrKind::Raw,
            Some(token!(TokenInfo::TBar)) => StrKind::Fmt,
            _ => StrKind::Str,
        })
    }

    pub(super) fn parse_heredoc_intro(&mut self, pipe_span: Span) -> Result<(Span, bool)> {
        use self::Op;
        use TokenInfo::*;
        if let Some(token!(Op(Op::Minus))) = self.peek()? {
            let minus_span = self.advance();
            Ok((pipe_span | minus_span, true))
        } else {
            Ok((pipe_span, false))
        }
    }

    pub(super) fn parse_heredoc(
        &mut self,
        scope: &mut Scope,
        pipe_span: Span,
        strip: bool,
        kind: StrKind,
    ) -> Result<Expr> {
        use self::Ident;
        use TokenInfo::*;

        let raw = kind == StrKind::Raw;

        let mut exprs = Vec::new();
        self.with_mode(
            if raw {
                lex::Mode::RawHeredoc
            } else {
                lex::Mode::Heredoc
            },
            |this| {
                loop {
                    match this.peek()? {
                        None => unreachable!("heredoc always closed by Dedent before EOF"),
                        Some(token!(Dedent)) => {
                            this.advance();
                            break;
                        }
                        Some(token!(Dollar, dollar_span)) if !raw => {
                            this.advance();
                            if let Some(token!(LeftBrace)) = this.peek()? {
                                exprs.push(this.parse_fmt_interp(scope, dollar_span, kind)?);
                                continue;
                            }
                            if let Some(token!(Hash, hash_span)) = this.peek()? {
                                this.advance();
                                exprs.push(this.parse_fmt_param_short(
                                    scope,
                                    dollar_span,
                                    hash_span,
                                    kind,
                                )?);
                                continue;
                            }
                            let expr = match this.peek()? {
                                Some(token!(Key)) => {
                                    let span = this.advance();
                                    let expr = Expr::Ident(Ident::new(span));
                                    exprs.push(Self::dollar_group(expr, dollar_span));
                                    Expr::Literal(span.after_right_char())
                                }
                                _ => {
                                    let expr = this.parse_expr_primary(scope, ExprMode::Compact)?;
                                    Self::dollar_group(expr, dollar_span)
                                }
                            };
                            exprs.push(expr);
                        }
                        Some(token!(Literal)) => {
                            let span = this.advance();
                            exprs.push(Expr::Literal(span));
                        }
                        Some(_) => match decay_string!(this.next()?) {
                            Some(token!(Literal, span)) => exprs.push(Expr::Literal(span)),
                            Some(token!(Key, span)) => {
                                exprs.push(Expr::Literal(span | span.after_right_char()))
                            }
                            Some(token!(DittoKey, span)) => {
                                exprs.push(Expr::Literal(span.before_left_char() | span))
                            }
                            Some(token!(Sym, span)) => exprs.push(Expr::Literal(
                                span.before_left_char() | span.after_right_char(),
                            )),
                            Some(token!(Escape(c), span)) if !raw => {
                                exprs.push(Expr::Escape(c, span))
                            }
                            Some(token @ token!(Escape(_), _)) => {
                                return Err(this.syntax_error(
                                    scope,
                                    Some(token),
                                    "escape sequences are not valid in raw here-docs",
                                ));
                            }
                            Some(token @ token!(EscapeByte(..), _)) => {
                                return Err(this.syntax_error(
                                    scope,
                                    Some(token),
                                    "\\x escapes are only valid in binary strings",
                                ));
                            }
                            Some(token!(Dollar, span)) => exprs.push(Expr::Literal(span)),
                            _ => unreachable!(),
                        },
                    }
                }
                Ok(())
            },
        )?;
        if strip && let Some(Expr::Literal(span)) = exprs.last_mut() {
            let slice = self.file.slice(*span);
            if slice.ends_with(b"\n") {
                span.end -= 1;
            } else if slice.ends_with(b"\r\n") {
                span.end -= 2;
            }
        }
        Ok(match kind {
            StrKind::Fmt => Expr::FmtSeq {
                exprs,
                open: pipe_span,
                close: None,
            },
            _ => Expr::Concat {
                exprs,
                delim_span: Some(pipe_span),
                verbatim: false,
            },
        }
        .optimize())
    }
}
