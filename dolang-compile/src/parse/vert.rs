use super::{ExprMode, Parser, Result, Scope, cmd::UnquotedMode, stream::ExpectKind};
use crate::{
    ast::{Arg, ArrayElem, DictElem, Expr, ExprBody, For, If, IfBranch, Key, Pair, Single},
    lex::{Keyword, Op, Token, TokenInfo},
    source::Span,
};

impl Parser<'_> {
    fn parse_cmd_vert_line_expr(&mut self, scope: &mut Scope, allow_object: bool) -> Result<Expr> {
        use self::Keyword;
        use self::Op;
        use TokenInfo::*;
        match self.peek()? {
            Some(token!(expr_start!())) => {
                let expr = self.parse_expr(scope, ExprMode::Shell)?;
                if allow_object && let Some(token!(Colon)) = self.peek()? {
                    let colon_span = self.advance();
                    let sep = self.expect(scope, &[ExpectKind::ArgSep])?;
                    self.add_indent(sep.end);
                    let arg = Arg::DynamicKey(Pair {
                        key: expr,
                        value: self.parse_cmd_vert_line_expr(scope, false)?,
                        colon_span: Some(colon_span),
                        delim_span: None,
                    });
                    // Parse any trailer
                    self.parse_data(scope, vec![arg], false)
                } else {
                    match self.peek()? {
                        Some(token!(ArgSep)) => {
                            // Consume trailing whitespace
                            while let Some(token!(ArgSep)) = self.peek()? {
                                self.advance();
                            }
                            Ok(expr)
                        }
                        None | Some(token!(StmtSep | Indent | Dedent)) => Ok(expr),
                        _ => self.parse_implicit_concat(scope, Some(expr), UnquotedMode::Data),
                    }
                }
            }
            Some(token!(Keyword(Keyword::Do))) => self.parse_cmd_or_expr(scope, true),
            Some(token!(Dollar)) => {
                let span = self.advance();
                match self.peek()? {
                    Some(token!(ArgSep)) => {
                        self.advance();
                        let expr = self.parse_cmd_or_expr(scope, true)?;
                        Ok(Self::dollar_group(expr, span))
                    }
                    Some(token!(Indent)) => {
                        self.advance();
                        let expr = self.parse_data(scope, vec![], true)?;
                        Ok(Self::dollar_group(expr, span))
                    }
                    _ => {
                        let expr = self.parse_expr(scope, ExprMode::Compact)?;
                        let expr = Self::dollar_group(expr, span);
                        if matches!(self.peek()?, Some(token!(Dedent | StmtSep)) | None) {
                            Ok(expr)
                        } else {
                            self.parse_implicit_concat(scope, Some(expr), UnquotedMode::Data)
                        }
                    }
                }
            }
            Some(token!(TokenInfo::Op(Op::Bar) | TokenInfo::RBar | TokenInfo::TBar)) => {
                let kind = self.heredoc_kind()?;
                let open_span = self.advance();
                let (intro_span, strip) = self.parse_heredoc_intro(open_span)?;
                if let Some(token!(Indent)) = self.peek()? {
                    self.advance();
                    self.parse_heredoc(scope, intro_span, strip, kind)
                } else {
                    self.parse_implicit_concat(
                        scope,
                        Some(Expr::Literal(intro_span)),
                        UnquotedMode::Data,
                    )
                }
            }
            _ => self.parse_implicit_concat(scope, None, UnquotedMode::Data),
        }
    }

    /// Parse an indented run of vertical arguments through its closing `Dedent`.
    ///
    /// The opening `Indent` has already been consumed by the caller.
    fn parse_cmd_vert_body(&mut self, scope: &mut Scope, bin_pack: bool) -> Result<ExprBody<Arg>> {
        use TokenInfo::*;
        let mut dedents = 1;

        let mut elems = Vec::new();
        loop {
            let res = (|| -> Result<bool> {
                match self.peek()? {
                    None | Some(token!(Dedent)) => return Ok(true),
                    token @ Some(token!(Indent)) => {
                        dedents += 1;
                        return Err(self.syntax_error(
                            scope,
                            token,
                            "unexpected indent in vertical data",
                        ));
                    }
                    Some(token!(StmtSep)) => {
                        self.advance();
                    }
                    _ => self.parse_cmd_vert_arg(scope, &mut elems, bin_pack)?,
                }
                Ok(false)
            })();
            match res {
                Ok(true) => break,
                Ok(false) => continue,
                Err(_) => {
                    self.resync_eol()?;
                }
            }
        }
        for _ in 0..dedents {
            self.expect(scope, &[ExpectKind::Dedent])?;
        }
        Ok(ExprBody {
            elems,
            vars: Vec::new(),
        })
    }

    fn parse_cmd_vert_if(
        &mut self,
        scope: &mut Scope,
        bin_pack: bool,
    ) -> Result<If<ExprBody<Arg>>> {
        use self::If;
        use self::IfBranch;
        use self::Keyword;
        use TokenInfo::*;

        let if_span = self.expect(scope, &[ExpectKind::Keyword(Keyword::If)])?;
        self.expect(scope, &[ExpectKind::ArgSep])?;
        let (cond, bind) = self.parse_cond(scope)?;
        let tbranch = self.parse_cmd_vert_body(scope, bin_pack)?;

        let mut elif_branches = Vec::new();
        let mut else_branch = None;

        while let Some(token!(Keyword(Keyword::Else))) = self.peek()? {
            let else_span = self.advance();

            if let Some(token!(ArgSep)) = self.peek()? {
                // This is "else if"
                self.advance();
                let elif_if_span = self.expect(scope, &[ExpectKind::Keyword(Keyword::If)])?;
                self.expect(scope, &[ExpectKind::ArgSep])?;
                let (elif_cond, elif_bind) = self.parse_cond(scope)?;
                let elif_body = self.parse_cmd_vert_body(scope, bin_pack)?;

                elif_branches.push((
                    IfBranch {
                        span: elif_if_span,
                        expr: elif_cond,
                        bind: elif_bind,
                        body: elif_body,
                    },
                    else_span,
                ));
            } else {
                // This is final "else"
                self.expect(scope, &[ExpectKind::Indent])?;
                else_branch = Some((self.parse_cmd_vert_body(scope, bin_pack)?, else_span));
                break;
            }
        }

        Ok(If {
            tbranch: IfBranch {
                span: if_span,
                expr: cond,
                bind,
                body: tbranch,
            },
            elif_branches,
            else_branch,
        })
    }

    fn parse_cmd_vert_dynamic_or_pos(
        &mut self,
        scope: &mut Scope,
        args: &mut Vec<Arg>,
        key: Expr,
    ) -> Result<()> {
        if let Some(token!(TokenInfo::Colon)) = self.peek()? {
            let colon_span = self.advance();
            let value = if let Some(token!(TokenInfo::Indent)) = self.peek()? {
                self.advance();
                self.parse_data(scope, vec![], false)?
            } else {
                self.expect(scope, &[ExpectKind::ArgSep])?;
                self.parse_cmd_vert_line_expr(scope, false)?
            };
            args.push(Arg::DynamicKey(Pair {
                key,
                value,
                colon_span: Some(colon_span),
                delim_span: None,
            }));
        } else {
            let expr = self.parse_implicit_concat(scope, Some(key), UnquotedMode::Shell)?;
            args.push(Self::positional_arg(expr));
            self.parse_cmd_args(scope, false, false, args)?;
        }
        Ok(())
    }

    fn parse_cmd_vert_arg(
        &mut self,
        scope: &mut Scope,
        args: &mut Vec<Arg>,
        bin_pack: bool,
    ) -> Result<()> {
        use self::{Key, Keyword, Op};
        use TokenInfo::*;

        match self.peek()? {
            Some(token!(Op(Op::Minus))) => {
                let minus_span = self.advance();
                if let Some(token!(ArgSep)) = self.peek()? {
                    args.push(self.parse_cmd_vert_dash_arg(scope, minus_span)?);
                } else {
                    args.push(Arg::Pos(Single {
                        expr: self.parse_implicit_concat(
                            scope,
                            Some(Expr::Literal(minus_span)),
                            UnquotedMode::Shell,
                        )?,
                        delim_span: None,
                    }));
                    self.parse_cmd_args(scope, false, false, args)?;
                }
                Ok(())
            }
            Some(token!(Key)) => {
                let span = self.advance();
                match self.peek()? {
                    Some(token!(Indent)) => {
                        self.advance();
                        args.push(Arg::Key(Key {
                            key_span: span,
                            colon_span: span.after_right_char(),
                            expr: self.parse_data(scope, vec![], false)?,
                            delim_span: None,
                        }));
                    }
                    Some(token!(ArgSep)) => {
                        self.advance();
                        let expr = if let Some(token!(Keyword(Keyword::Do))) = self.peek()? {
                            self.parse_do_block(scope, true)?
                        } else {
                            self.parse_cmd_vert_line_expr(scope, false)?
                        };
                        args.push(Arg::Key(Key {
                            key_span: span,
                            colon_span: span.after_right_char(),
                            expr,
                            delim_span: None,
                        }));
                    }
                    // Not a key item after all: nothing separates the colon
                    // from what follows it, so this is one token that happens
                    // to contain a colon, as in a `https://...` URL. The same
                    // fallback the non-vertical argument parser makes, and the
                    // same one the `-` case above makes for a bare `-`.
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
                        self.parse_cmd_args(scope, false, false, args)?;
                    }
                }
                Ok(())
            }
            Some(token!(DittoKey)) => {
                let span = self.advance();
                args.push(Arg::Key(Self::ditto_key(span, None)));
                Ok(())
            }
            Some(token!(Ellipsis)) => {
                let ellipsis_span = self.advance();
                let expr = self.parse_expr(scope, ExprMode::Compact)?;
                args.push(Arg::Expand(Self::expansion(expr, ellipsis_span, None)));
                Ok(())
            }
            Some(token!(Keyword(Keyword::If))) => {
                args.push(Arg::If(self.parse_cmd_vert_if(scope, bin_pack)?));
                Ok(())
            }
            Some(token!(Keyword(Keyword::For))) => {
                let for_span = self.advance();
                self.expect(scope, &[ExpectKind::ArgSep])?;
                // Parse bind pattern
                let bind = self.parse_pattern(scope, false)?;
                // Parse optional "= expr"
                let (expr, equal_span) = if let Some(token!(Equal)) = self.peek()? {
                    let equal_span = self.advance();
                    self.expect(scope, &[ExpectKind::ArgSep])?;
                    (
                        Some(self.parse_cmd_or_expr(scope, false)?),
                        Some(equal_span),
                    )
                } else {
                    (None, None)
                };
                // Expect indentation for body
                self.expect(scope, &[ExpectKind::Indent])?;
                let body = self.parse_cmd_vert_body(scope, bin_pack)?;
                args.push(Arg::For(For {
                    bind,
                    expr,
                    body,
                    iter: None, // Will be filled by resolver
                    for_span,
                    equal_span,
                }));
                Ok(())
            }
            Some(token!(expr_start!())) => {
                let key = self.parse_expr(scope, ExprMode::Shell)?;
                self.parse_cmd_vert_dynamic_or_pos(scope, args, key)
            }
            Some(token!(Dollar)) => {
                let span = self.advance();
                match self.peek()? {
                    Some(token!(ArgSep)) => {
                        self.advance();
                        let expr = self.parse_cmd_or_expr(scope, true)?;
                        args.push(Self::positional_arg(Self::dollar_group(expr, span)));
                        return Ok(());
                    }
                    Some(token!(Indent)) => {
                        self.advance();
                        let expr = self.parse_data(scope, vec![], true)?;
                        args.push(Self::positional_arg(Self::dollar_group(expr, span)));
                        return Ok(());
                    }
                    _ => (),
                }
                let expr = self.parse_expr(scope, ExprMode::Compact)?;
                let key = Self::dollar_group(expr, span);
                self.parse_cmd_vert_dynamic_or_pos(scope, args, key)
            }
            Some(token!(Keyword(Keyword::Do))) => {
                let expr = self.parse_do_block(scope, true)?;
                args.push(Self::positional_arg(expr));
                Ok(())
            }
            other => {
                if bin_pack {
                    self.parse_cmd_args(scope, false, false, args)
                } else {
                    Err(self.syntax_error(
                        scope,
                        other,
                        "expected `-` item, key item, or `do` block",
                    ))
                }
            }
        }
    }

    fn parse_cmd_vert_dash_arg(&mut self, scope: &mut Scope<'_>, minus_span: Span) -> Result<Arg> {
        use self::{Key, Op};
        use TokenInfo::*;
        let sep = self.expect(scope, &[ExpectKind::ArgSep])?;
        self.add_indent(sep.end);
        let expr = match self.peek()? {
            Some(token!(Op(Op::Minus))) => {
                let minus_span = self.advance();
                // Decide if this is actually a sub-list
                match self.peek()? {
                    Some(token!(ArgSep)) => {
                        // Yep
                        let args = vec![self.parse_cmd_vert_dash_arg(scope, minus_span)?];
                        let expr = self.parse_data(scope, args, false)?;
                        // This consumed remainder of this indentation scope, so exit early
                        return Ok(Arg::Pos(Single {
                            expr,
                            delim_span: Some(minus_span),
                        }));
                    }
                    _ => {
                        // Nevermind
                        self.parse_implicit_concat(
                            scope,
                            Some(Expr::Literal(minus_span)),
                            UnquotedMode::Data,
                        )?
                    }
                }
            }
            Some(token!(Key)) => {
                let span = self.advance();
                match self.peek()? {
                    Some(token!(Indent)) => {
                        // Starting a dict and then immediately defining a vertical data value for the
                        // key
                        self.advance();
                        let arg = Arg::Key(Key {
                            key_span: span,
                            colon_span: span.after_right_char(),
                            expr: self.parse_data(scope, vec![], false)?,
                            delim_span: None,
                        });
                        // Parse any trailer
                        let expr = self.parse_data(scope, vec![arg], false)?;
                        // This consumed remainder of this indentation scope, so exit early
                        return Ok(Arg::Pos(Single {
                            expr,
                            delim_span: Some(minus_span),
                        }));
                    }
                    Some(token!(ArgSep)) => {
                        self.advance();
                        // Parse first item on this line
                        let arg = Arg::Key(Key {
                            expr: self.parse_cmd_vert_line_expr(scope, false)?,
                            key_span: span,
                            colon_span: span.after_right_char(),
                            delim_span: None,
                        });
                        // Parse any trailer
                        let expr = self.parse_data(scope, vec![arg], false)?;
                        // This consumed remainder of this indentation scope, so exit early
                        return Ok(Arg::Pos(Single {
                            expr,
                            delim_span: Some(minus_span),
                        }));
                    }
                    _ => self.parse_implicit_concat(
                        scope,
                        Some(Expr::Literal(span | span.after_right_char())),
                        UnquotedMode::Data,
                    )?,
                }
            }
            _ => self.parse_cmd_vert_line_expr(scope, true)?,
        };
        self.expect(scope, &[ExpectKind::Dedent])?;
        Ok(Arg::Pos(Single {
            expr,
            delim_span: Some(minus_span),
        }))
    }

    pub(super) fn parse_cmd_vert_args(
        &mut self,
        scope: &mut Scope,
        args: &mut Vec<Arg>,
        bin_pack: bool,
    ) -> Result<()> {
        use TokenInfo::*;
        let mut dedents = 1;

        loop {
            let res = (|| -> Result<bool> {
                match self.peek()? {
                    None | Some(token!(Dedent)) => return Ok(true),
                    token @ Some(token!(Indent)) => {
                        dedents += 1;
                        return Err(self.syntax_error(
                            scope,
                            token,
                            "unexpected indent in vertical data",
                        ));
                    }
                    Some(token!(StmtSep)) => {
                        self.advance();
                    }
                    _ => self.parse_cmd_vert_arg(scope, args, bin_pack)?,
                }
                Ok(false)
            })();
            match res {
                Ok(true) => break,
                Ok(false) => continue,
                Err(_) => {
                    self.resync_eol()?;
                }
            }
        }

        for _ in 0..dedents {
            self.expect(scope, &[ExpectKind::Dedent, ExpectKind::End])?;
        }

        Ok(())
    }

    pub(super) fn parse_data(
        &mut self,
        scope: &mut Scope,
        mut args: Vec<Arg>,
        bin_pack: bool,
    ) -> Result<Expr> {
        self.parse_cmd_vert_args(scope, &mut args, bin_pack)?;

        // Decide what to emit by recursively analyzing the args
        if self.args_have_key(&args) {
            // Has keys somewhere, build a dict
            Ok(Expr::Dict {
                elems: args.into_iter().map(Self::arg_to_dict_elem).collect(),
                brace_span: None,
            })
        } else {
            // All positional (or for-loops that only contain positional), build an array
            Ok(Expr::Array {
                elems: args.into_iter().map(Self::arg_to_array_elem).collect(),
                bracket_span: None,
            })
        }
    }

    fn args_have_key(&self, args: &[Arg]) -> bool {
        for arg in args {
            match arg {
                Arg::Pos(..) | Arg::Expand { .. } => continue, // array elements
                Arg::Key(Key { .. }) | Arg::DynamicKey { .. } => return true, // dict elements
                Arg::For(For { body, .. }) => {
                    // Recursively analyze the for-body
                    if self.args_have_key(&body.elems) {
                        return true;
                    }
                }
                Arg::If(node) => {
                    // Check first if branch
                    if self.args_have_key(&node.tbranch.body.elems) {
                        return true;
                    }

                    // Check elif branches
                    for (elif_branch, _) in &node.elif_branches {
                        if self.args_have_key(&elif_branch.body.elems) {
                            return true;
                        }
                    }

                    // Check final else branch
                    if let Some((else_body, _)) = &node.else_branch
                        && self.args_have_key(&else_body.elems)
                    {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn arg_to_array_elem(arg: Arg) -> ArrayElem {
        match arg {
            Arg::Pos(single) => ArrayElem::Single(single),
            Arg::Expand(expand) => ArrayElem::Expand(expand),
            Arg::If(node) => {
                ArrayElem::If(node.map(&mut |body| body.map(&mut Self::arg_to_array_elem)))
            }
            Arg::For(node) => {
                ArrayElem::For(node.map(&mut |body| body.map(&mut Self::arg_to_array_elem)))
            }
            _ => unreachable!("Unexpected arg type in array conversion"),
        }
    }

    fn arg_to_dict_elem(arg: Arg) -> DictElem {
        match arg {
            Arg::Pos(single) => DictElem::Single(single),
            Arg::Key(node) => DictElem::Key(node),
            Arg::DynamicKey(node) => DictElem::Pair(node),
            Arg::Expand(node) => DictElem::Expand(node),
            Arg::If(node) => {
                DictElem::If(node.map(&mut |body| body.map(&mut Self::arg_to_dict_elem)))
            }
            Arg::For(node) => {
                DictElem::For(node.map(&mut |body| body.map(&mut Self::arg_to_dict_elem)))
            }
        }
    }
}
