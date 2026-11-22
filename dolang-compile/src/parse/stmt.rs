use super::{
    Error, ExprMode, Parser, Result, Scope,
    cmd::StmtContext,
    diag::{InvalidLValue, SyntaxDiag},
    stream::ExpectKind,
};
use crate::{
    ast::{
        AliasBody, Arm, Assign, Bind, Block, CatchHandler, CondPattern, CondPatternKind, Expr, For,
        Function, Guard, Ident, If, IfBranch, ImportElement, Let, Match, PatBind, PatItem, Pattern,
        PrimStmt, Return, Stmt, Throw, Try, TypeAlias, While, visit::Node,
    },
    lex::{Keyword, Op, Token, TokenInfo},
    source::Span,
};

/// How a construct with both a statement and a vertical-element form, such as
/// `if` or `for`, parses its bodies.  The opening `Indent` has already been
/// consumed, and the body is parsed through its closing `Dedent`.
pub(super) trait BodyMode {
    type Body;

    fn parse_body(&self, parser: &mut Parser<'_>, scope: &mut Scope) -> Result<Self::Body>;

    /// Reject a same-line body after `lead`, a `=>` or `else`, where the
    /// context doesn't allow one
    fn check_inline(
        &self,
        _parser: &mut Parser<'_>,
        _scope: &mut Scope,
        _lead: &Token,
    ) -> Result<()> {
        Ok(())
    }

    /// Parse a same-line body after `lead`, a `=>` or keyword, once the
    /// separator following it has been consumed. A statement body is in
    /// `ctx`: `Inline` where the construct holding it continues on the lines
    /// after it, as an `if` does after a branch, and otherwise the context of
    /// that construct. A `match` arm is in `Block`, since its `match`
    /// continues only after all of its arms.
    fn parse_inline(
        &self,
        parser: &mut Parser<'_>,
        scope: &mut Scope,
        lead: &Token,
        ctx: StmtContext,
    ) -> Result<Self::Body>;

    /// What a same-line body holds, for errors: as possessed by its lead, and
    /// as needed after it
    fn inline_noun(&self) -> (&'static str, &'static str);

    /// The error for a `match` arm with no body, after a `|` group if `group`
    fn arm_expected(&self, group: bool) -> &'static str;
}

/// How errors name `lead`, the `=>` or keyword before a same-line body
pub(super) fn lead_name(lead: &Token) -> &'static str {
    match lead.info {
        TokenInfo::FatArrow => "=>",
        TokenInfo::Keyword(Keyword::Try) => "try",
        TokenInfo::Keyword(Keyword::Finally) => "finally",
        _ => "else",
    }
}

/// [`BodyMode::arm_expected`] where an arm can take `=>`
pub(super) fn arm_expected_inline(group: bool) -> &'static str {
    if group {
        "expected `do`, `=>` or `if` after an arm's `|` group"
    } else {
        "expected `=>`, `if` or an indented block after an arm's pattern"
    }
}

/// Bodies are blocks of statements
pub(super) struct StmtBody;

impl BodyMode for StmtBody {
    type Body = Block;

    fn parse_body(&self, parser: &mut Parser<'_>, scope: &mut Scope) -> Result<Block> {
        parser.parse_block_through_dedent(scope)
    }

    fn parse_inline(
        &self,
        parser: &mut Parser<'_>,
        scope: &mut Scope,
        _lead: &Token,
        ctx: StmtContext,
    ) -> Result<Block> {
        Ok(Block {
            stmts: vec![parser.parse_stmt(scope, ctx)?],
            vars: Vec::new(),
            repl: None,
        })
    }

    fn inline_noun(&self) -> (&'static str, &'static str) {
        ("its body", "a body")
    }

    fn arm_expected(&self, group: bool) -> &'static str {
        arm_expected_inline(group)
    }
}

impl Parser<'_> {
    fn parse_rhs(&mut self, scope: &mut Scope, ctx: StmtContext) -> Result<PrimStmt> {
        self.expect(scope, &[ExpectKind::ArgSep])?;
        self.parse_rhs_value(scope, ctx)
    }

    /// A right-hand side in a statement in `ctx`, after the separator
    /// following its `=`
    fn parse_rhs_value(&mut self, scope: &mut Scope, ctx: StmtContext) -> Result<PrimStmt> {
        self.check_continuing(ctx)?;
        match self.peek()? {
            Some(token!(TokenInfo::Keyword(Keyword::If))) => {
                Ok(PrimStmt::If(self.parse_if(scope, ctx)?))
            }
            Some(token!(TokenInfo::Keyword(Keyword::Match))) => {
                Ok(PrimStmt::Match(self.parse_match(scope)?))
            }
            Some(token!(TokenInfo::Keyword(Keyword::Try))) => {
                Ok(PrimStmt::Try(self.parse_try(scope, ctx)?))
            }
            _ => Ok(PrimStmt::Expr(self.parse_cmd_or_expr(scope, ctx)?)),
        }
    }

    /// Reject a construct that continues onto the lines after it, such as
    /// `if` with `else`, where those lines are spoken for
    fn check_continuing(&mut self, ctx: StmtContext) -> Result<()> {
        if ctx.continues() {
            return Ok(());
        }
        if let Some(token!(TokenInfo::Keyword(Keyword::If | Keyword::Try), span)) = self.peek()? {
            self.fail = true;
            self.diags.push(SyntaxDiag::new(
                span,
                "a same-line body can't hold `if` or `try`, since the lines after it \
                 would be ambiguous; put the body in an indented block",
            ));
        }
        Ok(())
    }

    fn parse_let(
        &mut self,
        scope: &mut Scope,
        pub_span: Option<Span>,
        at_span: Option<Span>,
        ctx: StmtContext,
    ) -> Result<Stmt> {
        let let_span = self.expect(scope, &[ExpectKind::Keyword(Keyword::Let)])?;
        self.expect(scope, &[ExpectKind::ArgSep])?;
        if let Some(at_span) = at_span {
            let ident = match decay_ident!(self.next()?) {
                Some(token!(TokenInfo::Ident, span)) => Ident::new(span),
                other => return Err(self.syntax_error(scope, other, "expected alias name")),
            };
            let binders = self.parse_binders(scope)?;
            if let Some(token!(TokenInfo::ArgSep)) = self.peek()? {
                self.advance();
            }
            let equal_span = self.expect(scope, &[ExpectKind::Equal])?;
            self.expect(scope, &[ExpectKind::ArgSep])?;
            let (body, block) = if let Some(token!(TokenInfo::Ellipsis)) = self.peek()? {
                (AliasBody::Opaque(self.advance()), false)
            } else {
                let (ty, block) = self.parse_type_alias_body(scope)?;
                (AliasBody::Type(ty), block)
            };
            if let Some(token!(TokenInfo::ArgSep)) = self.peek()? {
                self.advance();
            }
            match self.peek()? {
                _ if block => {}
                None | Some(token!(TokenInfo::StmtSep | TokenInfo::Dedent)) => {}
                _ => {
                    let token = self.consume();
                    return Err(self.syntax_error(
                        scope,
                        Some(token),
                        "expected end of statement after alias",
                    ));
                }
            }
            return Ok(Stmt::TypeAlias(TypeAlias {
                ident,
                binders,
                body,
                let_span,
                at_span,
                equal_span,
                pub_span,
                node: None,
            }));
        }
        let bind = self.parse_pattern(scope, false)?;
        let equal_span = self.expect(scope, &[ExpectKind::Equal])?;
        self.expect(scope, &[ExpectKind::ArgSep])?;
        let rhs_start = self.peek()?;
        let rhs_owns_else = matches!(rhs_start, Some(token!(TokenInfo::Keyword(Keyword::If))));
        let rhs = match rhs_start {
            Some(token!(TokenInfo::Ellipsis, span)) => {
                self.advance();
                if let Some(token!(TokenInfo::ArgSep)) = self.peek()? {
                    self.advance();
                }
                match self.peek()? {
                    None | Some(token!(TokenInfo::StmtSep | TokenInfo::Dedent)) => {}
                    other => {
                        return Err(self.syntax_error(
                            scope,
                            other,
                            "`...` must be the whole value of a `let`",
                        ));
                    }
                }
                PrimStmt::Expr(Expr::Stub(span))
            }
            _ => self.parse_rhs_value(scope, ctx)?,
        };
        let else_branch = if rhs_owns_else {
            if let token @ Some(token!(TokenInfo::Keyword(Keyword::Else))) = self.peek()? {
                return Err(self.syntax_error(
                    scope,
                    token,
                    "`else` belongs to the `if` RHS, not the `let`",
                ));
            }
            None
        } else {
            self.parse_binding_else(scope, ctx)?
        };
        Ok(Stmt::Let(Let {
            else_branch,
            bind,
            rhs,
            let_span,
            equal_span,
            pub_span,
        }))
    }

    fn parse_bind(&mut self, scope: &mut Scope, ctx: StmtContext) -> Result<Bind> {
        let bind_span = self.expect(scope, &[ExpectKind::Keyword(Keyword::Bind)])?;
        self.expect(scope, &[ExpectKind::ArgSep])?;
        let expr = self.parse_cmd_or_expr(scope, StmtContext::Head)?;
        let (bind, arrow_span) = if let Some(token!(TokenInfo::FatArrow)) = self.peek()? {
            let arrow = self.consume();
            let bind = self.parse_arrow_bind_pattern(scope, &arrow)?;
            // A `C $` type test's block ends the statement with its `Dedent`
            match self.peek()? {
                _ if bind.ends_in_block() => {}
                None | Some(token!(TokenInfo::StmtSep | TokenInfo::Dedent)) => {}
                Some(token!(TokenInfo::Indent)) => {
                    return Err(self.syntax_error(
                        scope,
                        Some(arrow.clone()),
                        "a pattern after `=>` ends with its line; put a vertical pattern under \
                         `bind`, or end the line with a `C $` type test",
                    ));
                }
                token => {
                    return Err(self.syntax_error(
                        scope,
                        token,
                        "expected end of statement after `bind` pattern",
                    ));
                }
            }
            (bind, Some(arrow.span))
        } else {
            (self.parse_pattern(scope, true)?, None)
        };
        let else_branch = self.parse_binding_else(scope, ctx)?;
        Ok(Bind {
            else_branch,
            bind,
            expr,
            bind_span,
            arrow_span,
        })
    }

    /// Parse the pattern after `bind SCRUTINEE` and its `=>`, `arrow`
    fn parse_arrow_bind_pattern(&mut self, scope: &mut Scope, arrow: &Token) -> Result<Pattern> {
        match self.peek()? {
            Some(token!(TokenInfo::ArgSep)) => {
                self.advance();
            }
            None | Some(token!(TokenInfo::StmtSep | TokenInfo::Dedent | TokenInfo::Indent)) => {
                return Err(self.syntax_error(
                    scope,
                    Some(arrow.clone()),
                    "`=>` needs a pattern on the same line; for a vertical pattern, omit `=>`, \
                     or end the line with a `C $` type test",
                ));
            }
            _ => {
                self.expect(scope, &[ExpectKind::ArgSep])?;
            }
        }
        self.parse_arrow_pattern(scope)
    }

    /// A binding's failure body, after its value or vertical pattern. A
    /// binding in `ctx` that doesn't continue leaves an `else` after it to the
    /// construct holding it.
    fn parse_binding_else(
        &mut self,
        scope: &mut Scope,
        ctx: StmtContext,
    ) -> Result<Option<(Box<Block>, Span)>> {
        if !ctx.continues() {
            return Ok(None);
        }
        loop {
            match self.peek()? {
                Some(token!(TokenInfo::StmtSep)) => {
                    self.advance();
                }
                Some(token!(TokenInfo::Keyword(Keyword::Else))) => break,
                _ => return Ok(None),
            }
        }
        let span = self.advance();
        let Some(body) = self.parse_else_body(scope, &StmtBody, span, ctx)? else {
            let token = self.peek()?;
            return Err(self.syntax_error(
                scope,
                token,
                "`else if` is not allowed after `let` or `bind`",
            ));
        };
        Ok(Some((Box::new(body), span)))
    }

    /// Parse the body after `else` at `else_span`: an indented block or a
    /// same-line body in `ctx`, that of the statement the `else` ends. Returns
    /// `None`, with the separator consumed, if `if` follows instead.
    fn parse_else_body<M: BodyMode>(
        &mut self,
        scope: &mut Scope,
        mode: &M,
        else_span: Span,
        ctx: StmtContext,
    ) -> Result<Option<M::Body>> {
        if let Some(token!(TokenInfo::Indent)) = self.peek()? {
            self.advance();
            return Ok(Some(mode.parse_body(self, scope)?));
        }
        let lead = Token {
            info: TokenInfo::Keyword(Keyword::Else),
            span: else_span,
        };
        self.expect_inline_sep(scope, mode, &lead)?;
        if let Some(token!(TokenInfo::Keyword(Keyword::If))) = self.peek()? {
            return Ok(None);
        }
        mode.check_inline(self, scope, &lead)?;
        Ok(Some(mode.parse_inline(self, scope, &lead, ctx)?))
    }

    /// Consume the separator between `lead`, a `=>` or a keyword such as
    /// `else`, and a body on the same line
    fn expect_inline_sep<M: BodyMode>(
        &mut self,
        scope: &mut Scope,
        mode: &M,
        lead: &Token,
    ) -> Result<()> {
        let (its, needs) = mode.inline_noun();
        let name = lead_name(lead);
        // A keyword can instead take an indented block directly
        let block = if lead.info != TokenInfo::FatArrow {
            " or in an indented block"
        } else {
            ""
        };
        match self.peek()? {
            Some(token!(TokenInfo::ArgSep)) => {
                self.advance();
                Ok(())
            }
            Some(token!(TokenInfo::Indent)) => Err(self.syntax_error(
                scope,
                Some(lead.clone()),
                format!("`{name}` takes {its} on the same line"),
            )),
            None | Some(token!(TokenInfo::StmtSep | TokenInfo::Dedent)) => Err(self.syntax_error(
                scope,
                Some(lead.clone()),
                format!("`{name}` needs {needs} on the same line{block}"),
            )),
            _ => {
                self.expect(scope, &[ExpectKind::ArgSep])?;
                Ok(())
            }
        }
    }

    /// Parse the body after `lead`, a keyword such as `try` that takes one
    /// directly: an indented block or a same-line body in `ctx`. Also returns
    /// whether the body is on the same line, leaving the separator that ends
    /// it.
    fn parse_keyword_body(
        &mut self,
        scope: &mut Scope,
        lead: Token,
        ctx: StmtContext,
    ) -> Result<(Block, bool)> {
        if let Some(token!(TokenInfo::Indent)) = self.peek()? {
            self.advance();
            return Ok((self.parse_block_through_dedent(scope)?, false));
        }
        self.expect_inline_sep(scope, &StmtBody, &lead)?;
        let body = StmtBody.parse_inline(self, scope, &lead, ctx)?;
        Ok((body, true))
    }

    /// Parse a `catch` handler's body, after its variable: an indented block
    /// or, after `=>`, whose span is also returned, a same-line body
    fn parse_catch_body(&mut self, scope: &mut Scope) -> Result<(Block, Option<Span>)> {
        if let Some(token!(TokenInfo::ArgSep)) = self.peek()? {
            self.advance();
        }
        let arrow_span = self.expect_cond_end(scope)?;
        let body = self.parse_branch_body(scope, &StmtBody, arrow_span, StmtContext::Inline)?;
        Ok((body, arrow_span))
    }

    /// Parse a same-line body after `lead`, a `=>`, in `ctx`
    pub(super) fn parse_inline_body<M: BodyMode>(
        &mut self,
        scope: &mut Scope,
        mode: &M,
        lead: &Token,
        ctx: StmtContext,
    ) -> Result<M::Body> {
        mode.check_inline(self, scope, lead)?;
        self.expect_inline_sep(scope, mode, lead)?;
        mode.parse_inline(self, scope, lead, ctx)
    }

    /// Parse a branch body: a same-line body in `ctx` after the `=>` at
    /// `arrow_span`, or else an indented block whose `Indent` has been consumed
    fn parse_branch_body<M: BodyMode>(
        &mut self,
        scope: &mut Scope,
        mode: &M,
        arrow_span: Option<Span>,
        ctx: StmtContext,
    ) -> Result<M::Body> {
        match arrow_span {
            Some(span) => {
                let lead = Token {
                    info: TokenInfo::FatArrow,
                    span,
                };
                self.parse_inline_body(scope, mode, &lead, ctx)
            }
            None => mode.parse_body(self, scope),
        }
    }

    /// Parse the condition of an `if`, `while` or guard, along with the
    /// conditional pattern bind introduced by `let` or `bind` if there is one.
    /// The condition ends at the `Indent` opening the body, which is consumed,
    /// or at a `=>` introducing a same-line body, whose span is returned.
    ///
    /// The forms differ only in layout: `let` puts the pattern before the
    /// scrutinee and separates them with `=`, and `bind` puts it after, either
    /// in an indented block terminated by `do` or on the header line after
    /// `=>`. The plain form has no pattern at all.
    pub(super) fn parse_cond(
        &mut self,
        scope: &mut Scope,
    ) -> Result<(Expr, Option<CondPattern>, Option<Span>)> {
        use self::Keyword;
        use TokenInfo::*;

        let (keyword_span, is_let) = match self.peek()? {
            Some(token!(Keyword(Keyword::Let))) => (self.advance(), true),
            Some(token!(Keyword(Keyword::Bind))) => (self.advance(), false),
            _ => {
                let expr = self.parse_cmd_or_expr(scope, StmtContext::Head)?;
                let arrow_span = self.expect_cond_end(scope)?;
                return Ok((expr, None, arrow_span));
            }
        };
        self.expect(scope, &[ExpectKind::ArgSep])?;

        let (expr, pattern, kind, arrow_span) = if is_let {
            let pattern = self.parse_pattern(scope, false)?;
            let equal_span = self.expect(scope, &[ExpectKind::Equal])?;
            self.expect(scope, &[ExpectKind::ArgSep])?;
            let expr = self.parse_cmd_or_expr(scope, StmtContext::Head)?;
            let arrow_span = self.expect_cond_end(scope)?;
            (
                expr,
                pattern,
                CondPatternKind::Let { equal_span },
                arrow_span,
            )
        } else {
            let expr = self.parse_cmd_or_expr(scope, StmtContext::Head)?;
            let (pattern, pattern_arrow) = if let Some(token!(FatArrow)) = self.peek()? {
                let arrow = self.consume();
                (
                    self.parse_arrow_bind_pattern(scope, &arrow)?,
                    Some(arrow.span),
                )
            } else {
                // The vertical pattern consumes its own indented block through
                // the closing `Dedent`, leaving `do` or `=>` as the next token
                (self.parse_pattern(scope, true)?, None)
            };
            // A horizontal pattern after `=>` is followed directly by the
            // body's `Indent`; a pattern block, by `do`
            let block = pattern_arrow.is_none() || pattern.ends_in_block();
            let (do_span, arrow_span) = match self.peek()? {
                Some(token!(FatArrow)) => (None, Some(self.advance())),
                Some(token!(Indent)) if !block => {
                    self.advance();
                    (None, None)
                }
                _ => {
                    let kinds: &[ExpectKind] = if block {
                        &[ExpectKind::Keyword(Keyword::Do), ExpectKind::FatArrow]
                    } else {
                        &[ExpectKind::Indent, ExpectKind::FatArrow]
                    };
                    let do_span = self.expect(scope, kinds)?;
                    self.expect(scope, &[ExpectKind::Indent])?;
                    (Some(do_span), None)
                }
            };
            (
                expr,
                pattern,
                CondPatternKind::Bind {
                    arrow_span: pattern_arrow,
                    do_span,
                },
                arrow_span,
            )
        };

        Ok((
            expr,
            Some(CondPattern {
                keyword_span,
                kind,
                pattern,
            }),
            arrow_span,
        ))
    }

    /// Consume the end of a horizontal condition: the `Indent` opening its
    /// body, or a `=>` introducing a same-line body, whose span is returned.
    fn expect_cond_end(&mut self, scope: &mut Scope) -> Result<Option<Span>> {
        if let Some(token!(TokenInfo::FatArrow)) = self.peek()? {
            return Ok(Some(self.advance()));
        }
        self.expect(scope, &[ExpectKind::Indent, ExpectKind::FatArrow])?;
        Ok(None)
    }

    fn parse_if(&mut self, scope: &mut Scope, ctx: StmtContext) -> Result<If<Block>> {
        self.parse_if_with(scope, &StmtBody, ctx)
    }

    /// Parse an `if` in `ctx` whose branch bodies `mode` parses. A same-line
    /// branch is followed by the lines continuing the `if`, but its `else`
    /// body ends it.
    pub(super) fn parse_if_with<M: BodyMode>(
        &mut self,
        scope: &mut Scope,
        mode: &M,
        ctx: StmtContext,
    ) -> Result<If<M::Body>> {
        use self::{If, IfBranch, Keyword};
        use Keyword::*;
        use TokenInfo::*;

        let if_span = self.expect(scope, &[ExpectKind::Keyword(If)])?;
        self.expect(scope, &[ExpectKind::ArgSep])?;
        let (cond, bind, arrow_span) = self.parse_cond(scope)?;
        let tbranch = self.parse_branch_body(scope, mode, arrow_span, StmtContext::Inline)?;
        let mut inline = arrow_span.is_some();

        let mut elif_branches = Vec::new();
        let mut else_branch = None;

        loop {
            // A same-line body leaves the separator ending its line, which a
            // block's `Dedent` takes the place of
            if inline {
                while let Some(token!(StmtSep)) = self.peek()? {
                    self.advance();
                }
            }
            let Some(token!(Keyword(Else))) = self.peek()? else {
                break;
            };
            let else_span = self.advance();
            if let Some(else_body) = self.parse_else_body(scope, mode, else_span, ctx)? {
                else_branch = Some((else_body, else_span));
                break;
            }
            // This is "else if"
            let elif_if_span = self.expect(scope, &[ExpectKind::Keyword(If)])?;
            self.expect(scope, &[ExpectKind::ArgSep])?;
            let (elif_cond, elif_bind, elif_arrow) = self.parse_cond(scope)?;
            let elif_body = self.parse_branch_body(scope, mode, elif_arrow, StmtContext::Inline)?;
            inline = elif_arrow.is_some();

            elif_branches.push((
                IfBranch {
                    span: elif_if_span,
                    expr: elif_cond,
                    bind: elif_bind,
                    arrow_span: elif_arrow,
                    body: elif_body,
                },
                else_span,
            ));
        }

        Ok(If {
            tbranch: IfBranch {
                span: if_span,
                expr: cond,
                bind,
                arrow_span,
                body: tbranch,
            },
            elif_branches,
            else_branch,
        })
    }

    fn parse_match(&mut self, scope: &mut Scope) -> Result<Match<Block>> {
        self.parse_match_with(scope, &StmtBody)
    }

    /// Parse a `match` whose arm bodies `mode` parses
    pub(super) fn parse_match_with<M: BodyMode>(
        &mut self,
        scope: &mut Scope,
        mode: &M,
    ) -> Result<Match<M::Body>> {
        use self::Keyword;
        use TokenInfo::*;

        let match_span = self.expect(scope, &[ExpectKind::Keyword(Keyword::Match)])?;
        self.expect(scope, &[ExpectKind::ArgSep])?;
        let scrutinee = self.parse_cmd_or_expr(scope, StmtContext::Head)?;
        if let token @ Some(token!(FatArrow)) = self.peek()? {
            return Err(self.syntax_error(
                scope,
                token,
                "`match` takes its arms in an indented block; `=>` follows an arm's pattern",
            ));
        }
        self.expect(scope, &[ExpectKind::Indent])?;

        let mut arms = Vec::new();
        loop {
            match self.peek()? {
                Some(token!(StmtSep)) => {
                    self.advance();
                }
                Some(token!(Dedent)) => {
                    self.advance();
                    break;
                }
                _ => arms.push(self.parse_arm(scope, mode)?),
            }
        }
        if arms.is_empty() {
            let token = self.peek()?;
            return Err(self.syntax_error(scope, token, "expected at least one arm"));
        }

        Ok(Match {
            match_span,
            scrutinee,
            arms,
            var: None,
        })
    }

    /// Parse an arm of a `match`: its pattern, an optional guard, and its body.
    ///
    /// The body is an indented block or, after `=>`, a statement on the same
    /// line. A block follows a horizontal pattern or a guard directly, and a `|`
    /// group after `do`. `mode` parses the body.
    fn parse_arm<M: BodyMode>(&mut self, scope: &mut Scope, mode: &M) -> Result<Arm<M::Body>> {
        use self::{Keyword, Op};
        use TokenInfo::*;

        if let token @ Some(token!(Keyword(Keyword::Do | Keyword::If) | FatArrow)) = self.peek()? {
            return Err(self.syntax_error(
                scope,
                token,
                "an arm needs a pattern before its guard or body; `_` matches anything",
            ));
        }
        let vertical = matches!(self.peek()?, Some(token!(TokenInfo::Op(Op::Bar))));
        let pattern = self.parse_arm_pattern(scope)?;
        if let Some(token!(ArgSep)) = self.peek()? {
            self.advance();
        }
        let mut guard = None;
        let mut do_span = None;
        let mut arrow_span = None;
        let body = match self.peek()? {
            Some(token!(Keyword(Keyword::If))) => {
                let if_span = self.advance();
                self.expect(scope, &[ExpectKind::ArgSep])?;
                let (expr, bind, arrow) = self.parse_cond(scope)?;
                guard = Some(Guard {
                    if_span,
                    expr,
                    bind,
                });
                arrow_span = arrow;
                self.parse_branch_body(scope, mode, arrow, StmtContext::Block)?
            }
            Some(token!(FatArrow)) => {
                let arrow = self.consume();
                arrow_span = Some(arrow.span);
                self.parse_inline_body(scope, mode, &arrow, StmtContext::Block)?
            }
            Some(token!(Indent)) if !vertical => {
                self.advance();
                mode.parse_body(self, scope)?
            }
            token @ Some(token!(Keyword(Keyword::Do), span)) => {
                self.advance();
                if let Some(token!(Indent)) = self.peek()? {
                    if vertical {
                        do_span = Some(span);
                    } else {
                        self.fail = true;
                        self.diags.push(SyntaxDiag::new(
                            span,
                            "an arm's block follows its pattern directly, without `do`",
                        ));
                    }
                    self.advance();
                    mode.parse_body(self, scope)?
                } else {
                    return Err(self.syntax_error(
                        scope,
                        token,
                        "an arm's body on the same line follows `=>`, not `do`",
                    ));
                }
            }
            token => {
                // At the end of the line, point at the end of the pattern
                let token = match token {
                    None | Some(token!(StmtSep | Dedent)) => Some(Token {
                        info: StmtSep,
                        span: pattern.span().right_char(),
                    }),
                    token => token,
                };
                return Err(self.syntax_error(scope, token, mode.arm_expected(vertical)));
            }
        };
        Ok(Arm {
            pattern,
            guard,
            do_span,
            arrow_span,
            body,
        })
    }

    /// Parse a `try` in `ctx`. Its body and handlers are followed by the lines
    /// continuing it, but its `finally` body ends it.
    fn parse_try(&mut self, scope: &mut Scope, ctx: StmtContext) -> Result<Try> {
        use self::{Ident, Keyword, Try};
        use Keyword::*;
        use TokenInfo::*;

        let try_span = self.expect(scope, &[ExpectKind::Keyword(Keyword::Try)])?;
        let lead = Token {
            info: Keyword(Keyword::Try),
            span: try_span,
        };
        let (body_block, mut inline) = self.parse_keyword_body(scope, lead, StmtContext::Inline)?;

        let body = Function {
            params: vec![],
            input: None,
            output: None,
            ret: None,
            bars: None,
            stub_span: None,
            body: body_block,
        };

        let mut handlers = Vec::new();
        let mut has_catch_all = false;

        loop {
            // A same-line body leaves the separator ending its line, which a
            // block's `Dedent` takes the place of
            if inline {
                while let Some(token!(StmtSep)) = self.peek()? {
                    self.advance();
                }
            }
            let Some(token!(Keyword(Catch))) = self.peek()? else {
                break;
            };
            let catch_span = self.advance();

            self.expect(scope, &[ExpectKind::ArgSep])?;

            if has_catch_all {
                self.fail = true;
                self.diags.push(SyntaxDiag::new(
                    catch_span,
                    "catch-all handler must be last",
                ));
                return Err(Error);
            }

            // Parse a compact expression, then decide based on whether a colon follows
            let expr = self.parse_expr(scope, ExprMode::Compact)?;

            if let Some(token!(Colon)) = self.peek()? {
                // Typed catch: <class_expr>: <var>
                self.advance();
                self.expect(scope, &[ExpectKind::ArgSep])?;
                let var_span = self.expect(scope, &[ExpectKind::Ident])?;
                let (catch_block, arrow_span) = self.parse_catch_body(scope)?;
                inline = arrow_span.is_some();
                handlers.push(CatchHandler {
                    arrow_span,
                    class_expr: Some(expr),
                    func: Function {
                        params: vec![PatItem::Pos {
                            bind: PatBind::Ident(Ident::new(var_span)),
                            ty: None,
                            default: None,
                        }],
                        input: None,
                        output: None,
                        ret: None,
                        bars: None,
                        stub_span: None,
                        body: catch_block,
                    },
                    catch_span,
                });
            } else {
                // Catch-all: expression must be a plain identifier
                let var_span = match expr {
                    Expr::Ident(ident) => ident.span,
                    other => {
                        self.fail = true;
                        self.diags.push(SyntaxDiag::new(
                            other.span(),
                            "catch-all expects a plain identifier",
                        ));
                        return Err(Error);
                    }
                };
                has_catch_all = true;
                let (catch_block, arrow_span) = self.parse_catch_body(scope)?;
                inline = arrow_span.is_some();
                handlers.push(CatchHandler {
                    arrow_span,
                    class_expr: None,
                    func: Function {
                        params: vec![PatItem::Pos {
                            bind: PatBind::Ident(Ident::new(var_span)),
                            ty: None,
                            default: None,
                        }],
                        input: None,
                        output: None,
                        ret: None,
                        bars: None,
                        stub_span: None,
                        body: catch_block,
                    },
                    catch_span,
                });
            }
        }

        // Parse optional finally
        let finally = if let Some(token!(TokenInfo::Keyword(Keyword::Finally))) = self.peek()? {
            let finally_span = self.advance();
            let lead = Token {
                info: Keyword(Keyword::Finally),
                span: finally_span,
            };
            let (finally_block, _) = self.parse_keyword_body(scope, lead, ctx)?;
            Some((
                Function {
                    params: vec![],
                    input: None,
                    output: None,
                    ret: None,
                    bars: None,
                    stub_span: None,
                    body: finally_block,
                },
                finally_span,
            ))
        } else {
            None
        };

        Ok(Try {
            body,
            handlers,
            finally,
            try_span,
        })
    }

    fn parse_while(&mut self, scope: &mut Scope, ctx: StmtContext) -> Result<Stmt> {
        let while_span = self.advance();
        self.expect(scope, &[ExpectKind::ArgSep])?;
        let (cond, bind, arrow_span) = self.parse_cond(scope)?;
        let body = self.parse_branch_body(scope, &StmtBody, arrow_span, ctx)?;
        Ok(Stmt::While(While {
            expr: cond,
            bind,
            arrow_span,
            body,
            while_span,
        }))
    }

    fn parse_for(&mut self, scope: &mut Scope, ctx: StmtContext) -> Result<Stmt> {
        Ok(Stmt::For(self.parse_for_with(scope, &StmtBody, ctx)?))
    }

    /// Parse a `for` in `ctx` whose body `mode` parses
    pub(super) fn parse_for_with<M: BodyMode>(
        &mut self,
        scope: &mut Scope,
        mode: &M,
        ctx: StmtContext,
    ) -> Result<For<M::Body>> {
        let for_span = self.expect(scope, &[ExpectKind::Keyword(Keyword::For)])?;
        self.expect(scope, &[ExpectKind::ArgSep])?;
        let bind = self.parse_pattern(scope, false)?;
        let (equal_span, expr, arrow_span) = match self.next()? {
            Some(token!(TokenInfo::Equal, equal_span)) => {
                self.expect(scope, &[ExpectKind::ArgSep])?;
                let expr = self.parse_cmd_or_expr(scope, StmtContext::Head)?;
                let arrow_span = self.expect_cond_end(scope)?;
                (Some(equal_span), Some(expr), arrow_span)
            }
            Some(token!(TokenInfo::Indent)) => (None, None, None),
            Some(token!(TokenInfo::FatArrow, arrow_span)) => (None, None, Some(arrow_span)),
            other => {
                return Err(self.syntax_error(
                    scope,
                    other,
                    "expected `=`, `=>` or indent after `for` pattern",
                ));
            }
        };
        let body = self.parse_branch_body(scope, mode, arrow_span, ctx)?;
        Ok(For {
            bind,
            expr,
            body,
            iter: None,
            for_span,
            equal_span,
            arrow_span,
        })
    }

    pub(super) fn parse_decl_marker(
        &mut self,
        scope: &mut Scope,
        method: bool,
    ) -> Result<Option<Span>> {
        if !matches!(self.peek()?, Some(token!(TokenInfo::At))) {
            return Ok(None);
        }
        let span = self.advance();
        match self.peek()? {
            Some(token!(TokenInfo::Keyword(Keyword::Def | Keyword::Class))) => {}
            Some(token!(TokenInfo::Keyword(Keyword::Let | Keyword::Import))) if !method => {}
            other => {
                return Err(self.syntax_error(
                    scope,
                    other,
                    "expected checker-only declaration keyword after `@`",
                ));
            }
        }
        Ok(Some(span))
    }

    /// Parse a statement in `ctx`, `Block` or `Inline`
    pub(super) fn parse_stmt(&mut self, scope: &mut Scope, ctx: StmtContext) -> Result<Stmt> {
        use self::{Keyword, Return, Throw};
        use Keyword::*;
        use TokenInfo::*;

        if let Some(token!(ArgSep)) = self.peek()? {
            self.advance();
        }

        let decorators = self.parse_decorators(scope)?;

        // Check for pub modifier
        let pub_span = if let Some(token!(Keyword(Pub))) = self.peek()? {
            let span = self.advance();
            self.expect(scope, &[ExpectKind::ArgSep])?;
            match self.peek()? {
                Some(token!(Keyword(Let | Def | Class | Import) | At)) => (),
                Some(token @ token!(DecoratorOpen)) => {
                    return Err(self.syntax_error(
                        scope,
                        Some(token),
                        "`pub` must follow decorators",
                    ));
                }
                other => {
                    let _ = self.syntax_error(
                        scope,
                        other,
                        "`pub` is only valid before `let`, `def`, `class`, or `import`",
                    );
                }
            }
            Some(span)
        } else {
            None
        };

        let at_span = self.parse_decl_marker(scope, false)?;

        if !decorators.is_empty() && !matches!(self.peek()?, Some(token!(Keyword(Def | Class)))) {
            let token = self.peek()?;
            return Err(self.syntax_error(
                scope,
                token,
                "decorators are only valid before `def` or `class`",
            ));
        }

        self.check_continuing(ctx)?;
        match self.peek()? {
            Some(token!(Keyword(Let))) => self.parse_let(scope, pub_span, at_span, ctx),
            Some(token!(Keyword(Def))) => Ok(Stmt::Def(
                self.parse_def(scope, pub_span, decorators, at_span, ctx)?,
            )),
            Some(token!(Keyword(Class))) => Ok(Stmt::Class(
                self.parse_class(scope, pub_span, decorators, at_span)?,
            )),
            Some(token!(Keyword(If))) => Ok(Stmt::Prim(PrimStmt::If(self.parse_if(scope, ctx)?))),
            Some(token!(Keyword(Match))) => {
                Ok(Stmt::Prim(PrimStmt::Match(self.parse_match(scope)?)))
            }
            Some(token!(Keyword(Try))) => {
                Ok(Stmt::Prim(PrimStmt::Try(self.parse_try(scope, ctx)?)))
            }
            Some(token!(Keyword(While))) => self.parse_while(scope, ctx),
            Some(token!(Keyword(For))) => self.parse_for(scope, ctx),
            Some(token!(Keyword(Bind))) => Ok(Stmt::Bind(self.parse_bind(scope, ctx)?)),
            Some(token!(Keyword(Import))) => {
                let import = self.parse_import(scope, pub_span, at_span)?;
                if pub_span.is_some() {
                    for element in &import.elements {
                        if let ImportElement::ModuleAsIs { module, .. } = element
                            && self.file.str(*module).contains('.')
                        {
                            return Err(self.syntax_error(
                                scope,
                                Some(Token {
                                    info: TokenInfo::Ident,
                                    span: *module,
                                }),
                                "public dotted module imports must be renamed",
                            ));
                        }
                    }
                }
                Ok(Stmt::Import(import))
            }
            Some(token!(Keyword(Return), span)) => {
                self.advance();
                let expr = match self.peek()? {
                    Some(token!(ArgSep)) => {
                        self.advance();
                        Some(self.parse_cmd_or_expr(scope, ctx)?)
                    }
                    None | Some(token!(Dedent | StmtSep)) => None,
                    _ => {
                        let token = self.next()?;
                        return Err(self.syntax_error(
                            scope,
                            token,
                            "expected space after `return`",
                        ));
                    }
                };
                Ok(Stmt::Return(Return {
                    expr,
                    span,
                    nl: None,
                }))
            }
            Some(token!(Keyword(Throw))) => {
                let span = self.advance();
                self.expect(scope, &[ExpectKind::ArgSep])?;
                let expr = self.parse_cmd_or_expr(scope, ctx)?;
                Ok(Stmt::Throw(Throw { expr, span }))
            }
            Some(token!(Keyword(Continue))) => Ok(Stmt::Continue(self.advance(), None)),
            Some(token!(Keyword(Break))) => Ok(Stmt::Break(self.advance(), None)),
            Some(token!(Keyword(Do))) => {
                Ok(Stmt::Prim(PrimStmt::Expr(self.parse_do_block(scope, ctx)?)))
            }
            Some(token!(TokenInfo::Dollar)) => {
                let dollar_span = self.advance();
                self.expect(scope, &[ExpectKind::Indent])?;
                let expr = self.parse_data(scope, vec![], true)?;
                Ok(Stmt::Prim(PrimStmt::Expr(Self::dollar_group(
                    expr,
                    dollar_span,
                ))))
            }
            Some(..) => {
                // Parse expression and check for assignment
                let arg0 = self.parse_cmd_arg0(scope)?;
                match self.peek()? {
                    Some(token!(Equal)) => {
                        // This is an assignment
                        let lhs = match arg0.into_lvalue() {
                            Ok(lvalue) => lvalue,
                            Err(expr) => {
                                self.fail = true;
                                self.diags.push(InvalidLValue(expr.span()));
                                return Err(Error);
                            }
                        };
                        let equal_span = self.expect(scope, &[ExpectKind::Equal])?;
                        let rhs = self.parse_rhs(scope, ctx)?;
                        Ok(Stmt::Assign(Assign {
                            lhs,
                            rhs,
                            equal_span,
                        }))
                    }
                    _ => {
                        // This is just an expression
                        let mut args = vec![];
                        self.parse_cmd_args(scope, ctx, &mut args)?;
                        let expr = Self::finish_call(arg0, args);
                        Ok(Stmt::Prim(PrimStmt::Expr(expr)))
                    }
                }
            }
            None => Err(self.syntax_error(scope, None, "expected statement")),
        }
    }

    pub(super) fn parse_block_through_dedent(&mut self, scope: &mut Scope) -> Result<Block> {
        let block = self.parse_block(scope)?;
        self.expect(scope, &[ExpectKind::Dedent])?;
        Ok(block)
    }

    pub(super) fn parse_block(&mut self, scope: &mut Scope) -> Result<Block> {
        use TokenInfo::*;

        let mut stmts = Vec::new();

        loop {
            let done = (|| -> Result<bool> {
                match self.peek()? {
                    None | Some(token!(Dedent)) => return Ok(true),
                    Some(token!(StmtSep)) => {
                        self.advance();
                    }
                    _ => stmts.push(self.parse_stmt(scope, StmtContext::Block)?),
                }
                if let Some(token!(ArgSep)) = self.peek()? {
                    self.advance();
                }
                Ok(false)
            })();
            match done {
                Ok(true) => break,
                Ok(false) => continue,
                Err(_) => {
                    self.resync_eol()?;
                }
            }
        }

        Ok(Block {
            stmts,
            vars: Vec::new(),
            repl: None,
        })
    }
}
