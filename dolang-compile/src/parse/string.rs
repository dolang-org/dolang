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

    /// Classifies the here string about to be parsed from its introducer:
    /// its kind, and whether it folds line breaks (`>`) rather than keeping
    /// them (`|`).
    pub(super) fn heredoc_kind(&mut self) -> Result<(StrKind, bool)> {
        Ok(match self.peek()? {
            Some(token!(TokenInfo::RBar)) => (StrKind::Raw, false),
            Some(token!(TokenInfo::RGt)) => (StrKind::Raw, true),
            Some(token!(TokenInfo::TBar)) => (StrKind::Fmt, false),
            Some(token!(TokenInfo::TGt)) => (StrKind::Fmt, true),
            Some(token!(TokenInfo::Op(Op::Gt))) => (StrKind::Str, true),
            _ => (StrKind::Str, false),
        })
    }

    /// After consuming the introducer, optionally consume `-` for strip mode.
    /// Returns `(intro_span, strip)` where `intro_span` covers the introducer
    /// and any `-`.
    pub(super) fn parse_heredoc_intro(&mut self, pipe_span: Span) -> Result<(Span, bool)> {
        use self::Op;
        use TokenInfo::*;
        let res = if let Some(token!(Op(Op::Minus))) = self.peek_heredoc_intro()? {
            let minus_span = self.advance();
            (pipe_span | minus_span, true)
        } else {
            (pipe_span, false)
        };
        // Buffer the token after the introducer, which is an `Indent` before a
        // here string body
        self.peek_heredoc_intro()?;
        Ok(res)
    }

    pub(super) fn parse_heredoc(
        &mut self,
        scope: &mut Scope,
        pipe_span: Span,
        strip: bool,
        kind: StrKind,
        fold: bool,
    ) -> Result<Expr> {
        use self::Ident;
        use TokenInfo::*;

        let raw = kind == StrKind::Raw;

        let mut exprs = Vec::new();
        let mut folder = Folder::default();
        self.with_mode(
            if raw {
                lex::Mode::RawHeredoc
            } else {
                lex::Mode::Heredoc
            },
            |this| {
                loop {
                    let token = this.peek()?;
                    if fold {
                        match token {
                            Some(token!(HeredocBreak, span)) => {
                                this.advance();
                                folder.line_break(span, &mut exprs);
                                continue;
                            }
                            Some(token!(HeredocIndent, span)) => {
                                this.advance();
                                folder.indent = span;
                                continue;
                            }
                            Some(token!(Dedent)) => folder.finish(&mut exprs),
                            Some(_) => folder.content(&mut exprs),
                            None => (),
                        }
                    }
                    match token {
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

/// Folds the source line breaks of a `>` here string, following YAML: a break
/// between two lines at the baseline becomes a space, and of the breaks before
/// a run of blank lines between them only the first is dropped. Breaks next to
/// a line indented beyond the baseline are all kept, as are those before the
/// first line and after the last.
#[derive(Default)]
struct Folder {
    /// Breaks since the last content line
    breaks: Vec<Span>,
    /// Whether the last content line was indented beyond the baseline, if
    /// there was one
    prev: Option<bool>,
    /// Indentation beyond the baseline of the current line
    indent: Span,
    /// Whether the current line's content has begun
    in_line: bool,
}

impl Folder {
    /// A source line break
    fn line_break(&mut self, span: Span, exprs: &mut Vec<Expr>) {
        // A line of only whitespace beyond the baseline is content
        if !self.in_line && self.indent.start != self.indent.end {
            self.content(exprs);
        }
        self.breaks.push(span);
        self.in_line = false;
        self.indent = Span::default();
    }

    /// Content of the current line follows
    fn content(&mut self, exprs: &mut Vec<Expr>) {
        if self.in_line {
            return;
        }
        let more = self.indent.start != self.indent.end;
        let mut breaks = self.breaks.drain(..);
        if self.prev == Some(false) && !more {
            let first = breaks.next().expect("a content line follows a break");
            if breaks.len() == 0 {
                exprs.push(Expr::Fold(' ', first));
            }
        }
        exprs.extend(breaks.map(Expr::Literal));
        if more {
            exprs.push(Expr::Literal(self.indent));
        }
        self.prev = Some(more);
        self.in_line = true;
    }

    /// The here string ends
    fn finish(&mut self, exprs: &mut Vec<Expr>) {
        exprs.extend(self.breaks.drain(..).map(Expr::Literal));
    }
}
