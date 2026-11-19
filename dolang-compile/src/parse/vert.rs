use super::{
    ExprMode, Parser, Result, Scope,
    cmd::CmdContext,
    cmd::UnquotedMode,
    stmt::{BodyMode, arm_expected_inline},
    stream::ExpectKind,
};
use crate::{
    ast::{Arg, ArrayElem, DictElem, Expr, ExprBody, For, If, Key, Pair, Single},
    lex::{Keyword, Op, Token, TokenInfo},
    source::Span,
};

/// Bodies are runs of vertical arguments, bin-packed or not
struct VertBody {
    bin_pack: bool,
}

impl BodyMode for VertBody {
    type Body = ExprBody<Arg>;

    fn parse_body(&self, parser: &mut Parser<'_>, scope: &mut Scope) -> Result<ExprBody<Arg>> {
        parser.parse_cmd_vert_body(scope, self.bin_pack)
    }

    fn parse_inline(
        &self,
        parser: &mut Parser<'_>,
        scope: &mut Scope,
        arrow_span: Span,
    ) -> Result<ExprBody<Arg>> {
        parser.parse_vert_arm_inline(scope, arrow_span, self.bin_pack)
    }

    fn arm_expected(&self, group: bool) -> &'static str {
        match (self.bin_pack, group) {
            (true, _) => arm_expected_inline(group),
            (false, true) => "expected `do` or `if` after an arm's `|` group",
            (false, false) => "expected `if` or an indented block after an arm's pattern",
        }
    }

    fn wrap_if(&self, node: If<ExprBody<Arg>>) -> ExprBody<Arg> {
        ExprBody {
            elems: vec![Arg::If(node)],
            vars: Vec::new(),
        }
    }
}

impl Parser<'_> {
    /// Parse a vertical `match` arm's same-line items, after its `=>` at
    /// `arrow_span`: one line of bin-packed positional items, where the
    /// context allows bin-packing
    fn parse_vert_arm_inline(
        &mut self,
        scope: &mut Scope,
        arrow_span: Span,
        bin_pack: bool,
    ) -> Result<ExprBody<Arg>> {
        use self::Op;
        use TokenInfo::*;

        let arrow = Token {
            info: FatArrow,
            span: arrow_span,
        };
        if !bin_pack {
            return Err(self.syntax_error(
                scope,
                Some(arrow),
                "a nested `match` arm's items go in an indented block, or in a block introduced \
                 with `$`",
            ));
        }
        match self.peek()? {
            Some(token!(ArgSep)) => {
                self.advance();
            }
            None | Some(token!(StmtSep | Dedent)) => {
                return Err(self.syntax_error(
                    scope,
                    Some(arrow),
                    "`=>` needs items on the same line",
                ));
            }
            Some(token!(Indent)) => {
                return Err(self.syntax_error(
                    scope,
                    Some(arrow),
                    "`=>` takes its items on the same line",
                ));
            }
            _ => {
                self.expect(scope, &[ExpectKind::ArgSep])?;
            }
        }
        let mut elems = Vec::new();
        if let Some(token!(Op(Op::Minus))) = self.peek()? {
            let minus_span = self.advance();
            if let Some(token!(ArgSep)) = self.peek()? {
                let token = Token {
                    info: Op(Op::Minus),
                    span: minus_span,
                };
                return Err(self.syntax_error(scope, Some(token), "a `-` item heads its own line"));
            }
            elems.push(Arg::Pos(Single {
                expr: self.parse_implicit_concat(
                    scope,
                    Some(Expr::Literal(minus_span)),
                    UnquotedMode::Shell,
                )?,
                delim_span: None,
            }));
        }
        self.parse_cmd_args(scope, CmdContext::VertLine, &mut elems)?;
        if let Some(token!(Indent)) = self.peek()? {
            return Err(self.syntax_error(
                scope,
                Some(arrow),
                "`=>` takes its items on the same line",
            ));
        }
        Ok(ExprBody {
            elems,
            vars: Vec::new(),
        })
    }

    /// Parse the value of a line item. `UnquotedMode::DashData` is a dash
    /// item's value, which may be a dynamic key, and `UnquotedMode::KeyData`
    /// a key item's; a text-like value in either may continue onto later
    /// lines.
    fn parse_cmd_vert_item_expr(&mut self, scope: &mut Scope, mode: UnquotedMode) -> Result<Expr> {
        use self::Keyword;
        use self::Op;
        use TokenInfo::*;
        match self.peek()? {
            Some(token!(expr_start!())) => {
                let expr = self.parse_expr(scope, ExprMode::Shell)?;
                if let UnquotedMode::DashData(column) = mode
                    && let Some(token!(Colon)) = self.peek()?
                {
                    let colon_span = self.advance();
                    self.expect(scope, &[ExpectKind::ArgSep])?;
                    // The dict's further items line up with this one, at the
                    // dash item's content column
                    self.add_indent(column);
                    let arg = Arg::DynamicKey(Pair {
                        key: expr,
                        value: self.parse_cmd_vert_item_expr(scope, UnquotedMode::KeyData)?,
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
                            if mode.folds_at(&self.peek()?) {
                                self.parse_implicit_concat(scope, Some(expr), mode)
                            } else {
                                Ok(expr)
                            }
                        }
                        token @ (None | Some(token!(StmtSep | Indent | Dedent))) => {
                            if mode.folds_at(&token) {
                                self.parse_implicit_concat(scope, Some(expr), mode)
                            } else {
                                Ok(expr)
                            }
                        }
                        _ => self.parse_implicit_concat(scope, Some(expr), mode),
                    }
                }
            }
            Some(token!(Keyword(Keyword::Do))) => self.parse_cmd_or_expr(scope, CmdContext::Stmt),
            Some(token!(Dollar)) => {
                let span = self.advance();
                match self.peek()? {
                    Some(token!(ArgSep)) => {
                        self.advance();
                        let expr = self.parse_cmd_or_expr(scope, CmdContext::Stmt)?;
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
                        match self.peek()? {
                            token @ (None | Some(token!(StmtSep | Indent | Dedent)))
                                if !mode.folds_at(&token) =>
                            {
                                Ok(expr)
                            }
                            _ => self.parse_implicit_concat(scope, Some(expr), mode),
                        }
                    }
                }
            }
            Some(
                token!(
                    TokenInfo::Op(Op::Bar | Op::Gt)
                        | TokenInfo::RBar
                        | TokenInfo::RGt
                        | TokenInfo::TBar
                        | TokenInfo::TGt
                ),
            ) => {
                let (kind, fold) = self.heredoc_kind()?;
                let open_span = self.advance();
                let (intro_span, strip) = self.parse_heredoc_intro(open_span)?;
                if let Some(token!(Indent)) = self.peek()? {
                    self.advance();
                    self.parse_heredoc(scope, intro_span, strip, kind, fold)
                } else {
                    self.parse_implicit_concat(
                        scope,
                        Some(Expr::Literal(intro_span)),
                        UnquotedMode::Data,
                    )
                }
            }
            _ => self.parse_implicit_concat(scope, None, mode),
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
                self.parse_cmd_vert_item_expr(scope, UnquotedMode::KeyData)?
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
            self.parse_cmd_args(scope, CmdContext::VertLine, args)?;
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
                    self.parse_cmd_args(scope, CmdContext::VertLine, args)?;
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
                            self.parse_do_block(scope, CmdContext::Stmt)?
                        } else {
                            self.parse_cmd_vert_item_expr(scope, UnquotedMode::KeyData)?
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
                        self.parse_cmd_args(scope, CmdContext::VertLine, args)?;
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
                args.push(Arg::If(self.parse_if_with(scope, &VertBody { bin_pack })?));
                Ok(())
            }
            Some(token!(Keyword(Keyword::For))) => {
                args.push(Arg::For(
                    self.parse_for_with(scope, &VertBody { bin_pack })?,
                ));
                Ok(())
            }
            Some(token!(Keyword(Keyword::Match))) => {
                args.push(Arg::Match(
                    self.parse_match_with(scope, &VertBody { bin_pack })?,
                ));
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
                        let expr = self.parse_cmd_or_expr(scope, CmdContext::Stmt)?;
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
                let expr = self.parse_do_block(scope, CmdContext::Stmt)?;
                args.push(Self::positional_arg(expr));
                Ok(())
            }
            other => {
                if bin_pack {
                    self.parse_cmd_args(scope, CmdContext::VertLine, args)
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
                            expr: self.parse_cmd_vert_item_expr(scope, UnquotedMode::KeyData)?,
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
            _ => self.parse_cmd_vert_item_expr(scope, UnquotedMode::DashData(sep.end))?,
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
                Arg::Match(node) => {
                    if node.branches().any(|body| self.args_have_key(&body.elems)) {
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
            Arg::Match(node) => {
                ArrayElem::Match(node.map(&mut |body| body.map(&mut Self::arg_to_array_elem)))
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
            Arg::Match(node) => {
                DictElem::Match(node.map(&mut |body| body.map(&mut Self::arg_to_dict_elem)))
            }
        }
    }
}
