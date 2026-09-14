use super::{
    Error, ExprMode, Parser, Result, Scope, diag::ImplicitDelimitedConcat, stream::ExpectKind,
    string::StrKind,
};
use crate::{
    ast::{Arg, Expr, Key, PrimStmt, Single, visit::Node},
    lex::{Keyword, Op, Token, TokenInfo},
};

pub(super) enum UnquotedMode {
    Shell,
    Data,
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
        loop {
            let next = match self.peek()? {
                None | Some(token!(StmtSep | Indent | Dedent)) => break,
                Some(token!(ArgSep)) if matches!(mode, UnquotedMode::Shell) => break,
                Some(token!(DQuote)) if matches!(mode, UnquotedMode::Shell) => {
                    let span = self.advance();
                    self.parse_quoted_string(scope, span, StrKind::Str)?
                }
                Some(token!(Dollar)) => self.parse_expr_primary(scope, ExprMode::Shell)?,
                Some(_) => match decay_string!(self.next()?) {
                    Some(token!(Literal, span)) => Expr::Literal(span),
                    Some(token!(Key, span)) => Expr::Literal(span | span.after_right_char()),
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

    fn parse_cmd_arg_expr(
        &mut self,
        scope: &mut Scope,
        allow_trailing: bool,
    ) -> Result<(Expr, bool)> {
        use self::{Keyword, Op};
        use TokenInfo::*;

        match self.peek()? {
            Some(token!(Dollar)) => {
                let span = self.advance();
                match self.peek()? {
                    Some(token!(ArgSep)) => {
                        self.advance();
                        let expr = self.parse_cmd_or_expr(scope, allow_trailing)?;
                        return Ok((
                            Self::dollar_group(expr, span),
                            // This consumed the rest of the statement
                            true,
                        ));
                    }
                    Some(token!(Indent)) if allow_trailing => {
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
                Ok((self.parse_do_block(scope, allow_trailing)?, true))
            }
            Some(token!(Op(Op::Bar) | RBar | TBar)) => {
                let kind = self.heredoc_kind()?;
                let open_span = self.advance();
                let (intro_span, strip) = self.parse_heredoc_intro(open_span)?;
                if let Some(token!(Indent)) = self.peek()? {
                    self.advance();
                    return Ok((self.parse_heredoc(scope, intro_span, strip, kind)?, true));
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
        allow_keys: bool,
        allow_trailing: bool,
        args: &mut Vec<Arg>,
    ) -> Result<bool> {
        use self::Key;
        use TokenInfo::*;

        match self.peek()? {
            None | Some(token!(StmtSep | Dedent)) => Ok(true),
            Some(token!(ArgSep)) => {
                self.advance();
                Ok(false)
            }
            Some(token!(Indent)) => {
                if allow_trailing {
                    self.advance();
                    self.parse_cmd_vert_args(scope, args, true)?;
                }
                Ok(true)
            }
            Some(token!(Key, span)) => {
                if !allow_keys {
                    let token = self.consume();
                    return Err(self.syntax_error(
                        scope,
                        Some(token),
                        "key arguments must head a line in vertical contexts",
                    ));
                }
                self.advance();
                let (expr, consumed) = match self.peek()? {
                    Some(token!(Indent)) if allow_trailing => {
                        self.advance();
                        (self.parse_data(scope, vec![], false)?, true)
                    }
                    Some(token!(ArgSep)) => {
                        self.advance();
                        self.parse_cmd_arg_expr(scope, allow_trailing)?
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
                let (expr, consumed) = self.parse_cmd_arg_expr(scope, allow_trailing)?;
                args.push(Self::positional_arg(expr));
                Ok(consumed)
            }
        }
    }

    pub(super) fn parse_cmd_args(
        &mut self,
        scope: &mut Scope,
        allow_keys: bool,
        allow_trailing: bool,
        args: &mut Vec<Arg>,
    ) -> Result<()> {
        while !self.parse_cmd_arg(scope, allow_keys, allow_trailing, args)? {}

        Ok(())
    }

    pub(super) fn parse_cmd(
        &mut self,
        scope: &mut Scope,
        allow_trailing: bool,
    ) -> Result<PrimStmt> {
        let arg0 = self.parse_cmd_arg0(scope)?;
        let mut args = vec![];
        self.parse_cmd_args(scope, true, allow_trailing, &mut args)?;
        Ok(PrimStmt::Expr(Self::finish_call(arg0, args)))
    }

    pub(super) fn parse_cmd_or_expr(
        &mut self,
        scope: &mut Scope,
        allow_trailing: bool,
    ) -> Result<Expr> {
        use self::Op;
        match self.peek()? {
            Some(token!(TokenInfo::Keyword(Keyword::Do))) => {
                return self.parse_do_block(scope, allow_trailing);
            }
            Some(token!(TokenInfo::Op(Op::Bar) | TokenInfo::RBar | TokenInfo::TBar))
                if allow_trailing =>
            {
                let kind = self.heredoc_kind()?;
                let open_span = self.advance();
                let (intro_span, strip) = self.parse_heredoc_intro(open_span)?;
                self.expect(scope, &[ExpectKind::Indent])?;
                return self.parse_heredoc(scope, intro_span, strip, kind);
            }
            Some(token!(TokenInfo::Dollar)) if allow_trailing => {
                let dollar_span = self.advance();
                self.expect(scope, &[ExpectKind::Indent])?;
                let expr = self.parse_data(scope, vec![], true)?;
                return Ok(Self::dollar_group(expr, dollar_span));
            }
            _ => {}
        }

        let arg0 = self.parse_cmd_arg0(scope)?;
        let mut args = vec![];
        self.parse_cmd_args(scope, true, allow_trailing, &mut args)?;

        Ok(Self::finish_call(arg0, args))
    }
}
