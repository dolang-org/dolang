use super::{
    Error, ExprMode, Parser, Result, Scope,
    diag::{ImplicitDelimitedConcat, KeyInKeyItem},
    stream::ExpectKind,
    string::StrKind,
};
use crate::{
    ast::{Arg, Expr, Key, PrimStmt, Single, visit::Node},
    lex::{Keyword, Op, Token, TokenInfo},
    source::{Offset, Span},
};

/// Where a statement, or a command in place of one, sits: whether the indented
/// block after its line and the lines after that are spoken for by an
/// enclosing construct
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum StmtContext {
    /// A statement in a block. Neither is spoken for: it may end in an
    /// indented block, and lines such as `else` may continue it.
    Block,
    /// A same-line body, as after `=>`. It may end in an indented block, but
    /// the lines after it belong to the construct holding it.
    Inline,
    /// A header, such as a condition or a scrutinee, ending where its
    /// construct's indented block begins or at a `=>`. Both are spoken for.
    Head,
}

impl StmtContext {
    /// Whether the statement may end in an indented block
    pub(super) fn allows_trailing(self) -> bool {
        !matches!(self, Self::Head)
    }

    /// Whether lines after the statement, such as `else`, may continue it
    pub(super) fn continues(self) -> bool {
        matches!(self, Self::Block)
    }
}

/// What a command's arguments may be and what ends them
#[derive(Clone, Copy, PartialEq, Eq)]
enum CmdContext {
    /// A command in a statement's place, given as a `Block` or `Inline`
    /// context, whose arguments may be keys and may end in an indented block,
    /// here string, `$` data or `do` block
    Stmt(StmtContext),
    /// A command in a header
    Head,
    /// A bin-packed line of vertical arguments, where a key argument must head
    /// its own line
    VertLine,
}

impl From<StmtContext> for CmdContext {
    fn from(ctx: StmtContext) -> Self {
        match ctx {
            StmtContext::Head => Self::Head,
            other => Self::Stmt(other),
        }
    }
}

impl CmdContext {
    fn allows_keys(self) -> bool {
        !matches!(self, Self::VertLine)
    }

    fn allows_trailing(self) -> bool {
        matches!(self, Self::Stmt(_))
    }

    fn ends_at_arrow(self) -> bool {
        matches!(self, Self::Head)
    }

    /// The context of a command nested in an argument, after `$` or `do`. It
    /// ends where this one does, but it heads its own line of arguments.
    fn nested(self) -> StmtContext {
        match self {
            Self::Stmt(ctx) => ctx,
            Self::Head | Self::VertLine => StmtContext::Head,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum UnquotedMode {
    /// Command arguments, which whitespace separates
    Shell,
    /// A line item's value, which keeps its whitespace
    Data,
    /// A dash item's value, which keeps its whitespace and continues onto
    /// lines at its content column, the offset given
    DashData(Offset),
    /// A key item's value, which keeps its whitespace and continues onto
    /// lines indented past its key. `bin_pack` is whether the item sits in a
    /// block that bin-packs, where a key later on its line reads like another
    /// key argument.
    KeyData { bin_pack: bool },
}

impl UnquotedMode {
    /// Whether a value in this mode continues at `token`, which ends its line
    pub(super) fn folds_at(self, token: &Option<Token>) -> bool {
        matches!(
            (self, token),
            (Self::DashData(_), Some(token!(TokenInfo::StmtSep)))
                | (Self::KeyData { .. }, Some(token!(TokenInfo::Indent)))
        )
    }
}

impl Parser<'_> {
    pub(super) fn parse_implicit_concat(
        &mut self,
        scope: &mut Scope,
        expr: Option<Expr>,
        mode: UnquotedMode,
    ) -> Result<Expr> {
        use TokenInfo::*;

        let mut exprs = match expr {
            Some(expr) => vec![expr],
            None => {
                // FIXME: should refactor call sites to handle this
                if matches!(mode, UnquotedMode::Shell) {
                    vec![self.parse_expr_primary(scope, ExprMode::Shell)?]
                } else {
                    vec![]
                }
            }
        };
        // Indentation levels a key item's continuation lines have entered
        let mut depth = 0;
        loop {
            let next = match self.peek()? {
                Some(token!(StmtSep, span))
                    if matches!(mode, UnquotedMode::DashData(_)) || depth > 0 =>
                {
                    self.advance();
                    self.push_fold(&mut exprs, span);
                    continue;
                }
                Some(token!(Indent, span)) if matches!(mode, UnquotedMode::KeyData { .. }) => {
                    self.advance();
                    self.push_fold(&mut exprs, span);
                    depth += 1;
                    continue;
                }
                // A line less indented than the one before it, but still past
                // the key, continues the value
                Some(token!(Dedent, span)) if depth > 1 => {
                    self.advance();
                    depth -= 1;
                    if !matches!(self.peek()?, Some(token!(Dedent))) {
                        self.push_fold(&mut exprs, span);
                    }
                    continue;
                }
                Some(token!(Dedent)) if depth == 1 => {
                    self.advance();
                    break;
                }
                None | Some(token!(StmtSep | Indent | Dedent)) => break,
                Some(token!(ArgSep)) if matches!(mode, UnquotedMode::Shell) => break,
                Some(token!(DQuote)) if matches!(mode, UnquotedMode::Shell) => {
                    let span = self.advance();
                    self.parse_quoted_string(scope, span, StrKind::Str)?
                }
                Some(token!(Dollar)) => self.parse_expr_primary(scope, ExprMode::Shell)?,
                Some(_) => match decay_string!(self.next()?) {
                    Some(token!(Literal, span)) => Expr::Literal(span),
                    Some(token!(Key, span)) => {
                        let key = span | span.after_right_char();
                        // A key after whitespace on the value's first line,
                        // and not at its start, reads like another key
                        // argument
                        if mode == (UnquotedMode::KeyData { bin_pack: true })
                            && depth == 0
                            && !exprs.is_empty()
                            && self.file.slice(Span {
                                start: key.start - 1,
                                end: key.start,
                            }) == b" "
                            && matches!(
                                self.peek()?,
                                None | Some(token!(ArgSep | StmtSep | Indent | Dedent))
                            )
                        {
                            self.diags.push(KeyInKeyItem(key));
                        }
                        Expr::Literal(key)
                    }
                    Some(token!(DittoKey, span)) => Expr::Literal(span.before_left_char() | span),
                    Some(token!(Sym, span)) => {
                        Expr::Literal(span.before_left_char() | span.after_right_char())
                    }
                    Some(token!(Escape(c), span)) => Expr::Escape(c, span),
                    _ => self.parse_expr_primary(scope, ExprMode::Shell)?,
                },
            };
            exprs.push(next)
        }
        if exprs.len() == 1 {
            Ok(exprs.pop().unwrap())
        } else {
            Ok(Expr::Concat {
                exprs,
                delim_span: None,
                verbatim: true,
            }
            .optimize())
        }
    }

    /// Push the characters a folded line break stands for: a space, or a
    /// newline per blank line in the gap. Comment lines don't count.
    fn push_fold(&self, exprs: &mut Vec<Expr>, gap: Span) {
        // The lines between the gap's first and last newline
        let mut lines: Vec<_> = self.file.slice(gap).split(|&b| b == b'\n').collect();
        lines.pop();
        let blank = lines
            .iter()
            .skip(1)
            .filter(|line| line.iter().all(|b| matches!(b, b' ' | b'\r')))
            .count();
        if blank == 0 {
            exprs.push(Expr::Fold(' ', gap));
        } else {
            exprs.extend((0..blank).map(|_| Expr::Fold('\n', gap)));
        }
    }

    pub(super) fn parse_cmd_arg0(&mut self, scope: &mut Scope) -> Result<Expr> {
        use TokenInfo::*;
        let res = self.parse_expr(scope, ExprMode::Compact)?;
        match self.peek()? {
            Some(token!(ArgSep)) => {
                self.advance();
            }
            None | Some(token!(StmtSep | Indent | Dedent)) => (),
            _ => {
                let token = self.consume();
                return Err(self.syntax_error(
                    scope,
                    Some(token),
                    "expected whitespace or end of statement",
                ));
            }
        }
        Ok(res)
    }

    fn parse_cmd_arg_expr(&mut self, scope: &mut Scope, ctx: CmdContext) -> Result<(Expr, bool)> {
        use self::{Keyword, Op};
        use TokenInfo::*;

        match self.peek()? {
            Some(token!(Dollar)) => {
                let span = self.advance();
                match self.peek()? {
                    Some(token!(ArgSep)) => {
                        self.advance();
                        let expr = self.parse_cmd_or_expr(scope, ctx.nested())?;
                        return Ok((
                            Self::dollar_group(expr, span),
                            // This consumed the rest of the statement
                            true,
                        ));
                    }
                    Some(token!(Indent)) if ctx.allows_trailing() => {
                        self.advance();
                        let expr = self.parse_data(scope, vec![], true)?;
                        return Ok((
                            Self::dollar_group(expr, span),
                            // This consumed the rest of the statement
                            true,
                        ));
                    }
                    _ => (),
                };
                let expr = self.parse_expr(scope, ExprMode::Compact)?;
                let expr = self.parse_implicit_concat(scope, Some(expr), UnquotedMode::Shell)?;
                Ok((Self::dollar_group(expr, span), false))
            }
            Some(token!(Keyword(Keyword::Do))) => {
                Ok((self.parse_do_block(scope, ctx.nested())?, true))
            }
            Some(token!(Op(Op::Bar | Op::Gt) | RBar | RGt | TBar | TGt)) => {
                let (kind, fold) = self.heredoc_kind()?;
                let open_span = self.advance();
                let (intro_span, strip) = self.parse_heredoc_intro(open_span)?;
                if let Some(token!(Indent)) = self.peek()? {
                    self.advance();
                    return Ok((
                        self.parse_heredoc(scope, intro_span, strip, kind, fold)?,
                        true,
                    ));
                }
                Ok((
                    self.parse_implicit_concat(
                        scope,
                        Some(Expr::Literal(intro_span)),
                        UnquotedMode::Shell,
                    )?,
                    false,
                ))
            }
            Some(
                token!(LeftParen | LeftBracket | LeftBrace | DQuote | RawQuote | BQuote | TQuote),
            ) => {
                let expr = self.parse_expr(scope, ExprMode::Shell)?;
                if !matches!(
                    self.peek()?,
                    None | Some(token!(Indent | Dedent | ArgSep | StmtSep))
                ) {
                    let token = self.consume();
                    self.fail = true;
                    self.diags.push(ImplicitDelimitedConcat {
                        span: token.span,
                        insert: expr.span(),
                    });
                    return Err(Error);
                }
                Ok((expr, false))
            }
            _ => Ok((
                self.parse_implicit_concat(scope, None, UnquotedMode::Shell)?,
                false,
            )),
        }
    }

    fn parse_cmd_arg(
        &mut self,
        scope: &mut Scope,
        ctx: CmdContext,
        args: &mut Vec<Arg>,
    ) -> Result<bool> {
        use self::Key;
        use TokenInfo::*;

        match self.peek()? {
            None | Some(token!(StmtSep | Dedent)) => Ok(true),
            Some(token!(FatArrow)) if ctx.ends_at_arrow() => Ok(true),
            Some(token!(ArgSep)) => {
                self.advance();
                Ok(false)
            }
            Some(token!(Indent)) => {
                if ctx.allows_trailing() {
                    self.advance();
                    self.parse_cmd_vert_args(scope, args, true)?;
                }
                Ok(true)
            }
            Some(token!(Key, span)) => {
                if !ctx.allows_keys() {
                    let token = self.consume();
                    return Err(self.syntax_error(
                        scope,
                        Some(token),
                        "key arguments must head a line in vertical contexts",
                    ));
                }
                self.advance();
                let (expr, consumed) = match self.peek()? {
                    Some(token!(Indent)) if ctx.allows_trailing() => {
                        self.advance();
                        (self.parse_data(scope, vec![], false)?, true)
                    }
                    Some(token!(ArgSep)) => {
                        self.advance();
                        self.parse_cmd_arg_expr(scope, ctx)?
                    }
                    _ => {
                        args.push(Arg::Pos(Single {
                            expr: self.parse_implicit_concat(
                                scope,
                                Some(Expr::Concat {
                                    exprs: vec![Expr::Literal(span | span.after_right_char())],
                                    delim_span: None,
                                    verbatim: true,
                                }),
                                UnquotedMode::Shell,
                            )?,
                            delim_span: None,
                        }));
                        return Ok(false);
                    }
                };
                args.push(Arg::Key(Key {
                    key_span: span,
                    colon_span: span.after_right_char(),
                    expr,
                    delim_span: None,
                }));
                Ok(consumed)
            }
            Some(token!(DittoKey)) => {
                let span = self.advance();
                args.push(Arg::Key(Self::ditto_key(span, None)));
                Ok(false)
            }
            Some(token!(Ellipsis)) => {
                let ellipsis_span = self.advance();
                let expr = self.parse_expr(scope, ExprMode::Compact)?;
                args.push(Arg::Expand(Self::expansion(expr, ellipsis_span, None)));
                Ok(false)
            }
            _ => {
                let (expr, consumed) = self.parse_cmd_arg_expr(scope, ctx)?;
                args.push(Self::positional_arg(expr));
                Ok(consumed)
            }
        }
    }

    fn parse_args(
        &mut self,
        scope: &mut Scope,
        ctx: CmdContext,
        args: &mut Vec<Arg>,
    ) -> Result<()> {
        while !self.parse_cmd_arg(scope, ctx, args)? {}

        Ok(())
    }

    /// Parse a command's arguments after its receiver, in `ctx`
    pub(super) fn parse_cmd_args(
        &mut self,
        scope: &mut Scope,
        ctx: StmtContext,
        args: &mut Vec<Arg>,
    ) -> Result<()> {
        self.parse_args(scope, ctx.into(), args)
    }

    /// Parse a bin-packed line of vertical arguments
    pub(super) fn parse_vert_line_args(
        &mut self,
        scope: &mut Scope,
        args: &mut Vec<Arg>,
    ) -> Result<()> {
        self.parse_args(scope, CmdContext::VertLine, args)
    }

    pub(super) fn parse_cmd(&mut self, scope: &mut Scope, ctx: StmtContext) -> Result<PrimStmt> {
        let arg0 = self.parse_cmd_arg0(scope)?;
        let mut args = vec![];
        self.parse_cmd_args(scope, ctx, &mut args)?;
        Ok(PrimStmt::Expr(Self::finish_call(arg0, args)))
    }

    pub(super) fn parse_cmd_or_expr(
        &mut self,
        scope: &mut Scope,
        ctx: StmtContext,
    ) -> Result<Expr> {
        use self::Op;
        match self.peek()? {
            Some(token!(TokenInfo::Keyword(Keyword::Do))) => {
                return self.parse_do_block(scope, ctx);
            }
            Some(
                token!(
                    TokenInfo::Op(Op::Bar | Op::Gt)
                        | TokenInfo::RBar
                        | TokenInfo::RGt
                        | TokenInfo::TBar
                        | TokenInfo::TGt
                ),
            ) if ctx.allows_trailing() => {
                let (kind, fold) = self.heredoc_kind()?;
                let open_span = self.advance();
                let (intro_span, strip) = self.parse_heredoc_intro(open_span)?;
                self.expect(scope, &[ExpectKind::Indent])?;
                return self.parse_heredoc(scope, intro_span, strip, kind, fold);
            }
            Some(token!(TokenInfo::Dollar)) if ctx.allows_trailing() => {
                let dollar_span = self.advance();
                self.expect(scope, &[ExpectKind::Indent])?;
                let expr = self.parse_data(scope, vec![], true)?;
                return Ok(Self::dollar_group(expr, dollar_span));
            }
            _ => {}
        }

        let arg0 = self.parse_cmd_arg0(scope)?;
        let mut args = vec![];
        self.parse_cmd_args(scope, ctx, &mut args)?;

        Ok(Self::finish_call(arg0, args))
    }
}
