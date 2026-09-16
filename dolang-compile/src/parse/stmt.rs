use super::{
    Error, ExprMode, Parser, Result, Scope,
    diag::{InvalidLValue, SyntaxDiag},
    stream::ExpectKind,
};
use crate::{
    ast::{
        Assign, Bind, Block, CatchHandler, Expr, For, Function, Ident, If, IfBranch, ImportElement,
        Let, Param, PatternBind, PatternBindKind, PrimStmt, Return, Stmt, Throw, Try, TypeAlias,
        While, visit::Node,
    },
    lex::{Keyword, Token, TokenInfo},
    source::Span,
};

impl Parser<'_> {
    fn parse_rhs(&mut self, scope: &mut Scope) -> Result<PrimStmt> {
        self.expect(scope, &[ExpectKind::ArgSep])?;
        match self.peek()? {
            Some(token!(TokenInfo::Keyword(Keyword::If))) => {
                Ok(PrimStmt::If(self.parse_if(scope)?))
            }
            Some(token!(TokenInfo::Keyword(Keyword::Try))) => {
                Ok(PrimStmt::Try(self.parse_try(scope)?))
            }
            _ => Ok(PrimStmt::Expr(self.parse_cmd_or_expr(scope, true)?)),
        }
    }

    fn parse_let(&mut self, scope: &mut Scope, pub_span: Option<Span>) -> Result<Stmt> {
        let let_span = self.expect(scope, &[ExpectKind::Keyword(Keyword::Let)])?;
        self.expect(scope, &[ExpectKind::ArgSep])?;
        if let Some(token!(TokenInfo::At)) = self.peek()? {
            let at_span = self.advance();
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
            let ty = self.with_inline_shell(|this| this.parse_type_compact(scope))?;
            return Ok(Stmt::TypeAlias(TypeAlias {
                ident,
                binders,
                ty,
                let_span,
                at_span,
                equal_span,
                pub_span,
                node: None,
            }));
        }
        let bind = self.parse_pattern(scope, false)?;
        let equal_span = self.expect(scope, &[ExpectKind::Equal])?;
        let rhs = self.parse_rhs(scope)?;
        Ok(Stmt::Let(Let {
            bind,
            rhs,
            let_span,
            equal_span,
            pub_span,
        }))
    }

    fn parse_bind(&mut self, scope: &mut Scope) -> Result<Bind> {
        let bind_span = self.expect(scope, &[ExpectKind::Keyword(Keyword::Bind)])?;
        self.expect(scope, &[ExpectKind::ArgSep])?;
        let expr = self.parse_cmd_or_expr(scope, false)?;
        let bind = self.parse_pattern(scope, true)?;
        Ok(Bind {
            bind,
            expr,
            bind_span,
        })
    }

    /// Parse the condition of an `if` or `while`, along with the conditional
    /// pattern bind introduced by `let` or `bind` if there is one, consuming the
    /// `Indent` that opens the branch body.
    ///
    /// The three forms differ only in layout: `let` puts the pattern before the
    /// scrutinee and separates them with `=`, `bind` puts it after in an indented
    /// block terminated by `do`, and the plain form has no pattern at all.
    pub(super) fn parse_cond(&mut self, scope: &mut Scope) -> Result<(Expr, Option<PatternBind>)> {
        use self::Keyword;
        use TokenInfo::*;

        let (keyword_span, is_let) = match self.peek()? {
            Some(token!(Keyword(Keyword::Let))) => (self.advance(), true),
            Some(token!(Keyword(Keyword::Bind))) => (self.advance(), false),
            _ => {
                let expr = self.parse_cmd_or_expr(scope, false)?;
                self.expect(scope, &[ExpectKind::Indent])?;
                return Ok((expr, None));
            }
        };
        self.expect(scope, &[ExpectKind::ArgSep])?;

        let (expr, pattern, kind) = if is_let {
            let pattern = self.parse_pattern(scope, false)?;
            let equal_span = self.expect(scope, &[ExpectKind::Equal])?;
            self.expect(scope, &[ExpectKind::ArgSep])?;
            let expr = self.parse_cmd_or_expr(scope, false)?;
            self.expect(scope, &[ExpectKind::Indent])?;
            (expr, pattern, PatternBindKind::Let { equal_span })
        } else {
            let expr = self.parse_cmd_or_expr(scope, false)?;
            // The vertical pattern consumes its own indented block through the
            // closing `Dedent`, leaving `do` as the next token
            let pattern = self.parse_pattern(scope, true)?;
            let do_span = self.expect(scope, &[ExpectKind::Keyword(Keyword::Do)])?;
            self.expect(scope, &[ExpectKind::Indent])?;
            (expr, pattern, PatternBindKind::Bind { do_span })
        };

        Ok((
            expr,
            Some(PatternBind {
                keyword_span,
                kind,
                pattern,
            }),
        ))
    }

    fn parse_if(&mut self, scope: &mut Scope) -> Result<If<Block>> {
        use self::{If, IfBranch, Keyword};
        use Keyword::*;
        use TokenInfo::*;

        let if_span = self.expect(scope, &[ExpectKind::Keyword(If)])?;
        self.expect(scope, &[ExpectKind::ArgSep])?;
        let (cond, bind) = self.parse_cond(scope)?;
        let tbranch = self.parse_block_through_dedent(scope)?;

        let mut elif_branches = Vec::new();
        let mut else_branch = None;

        while let Some(token!(Keyword(Else))) = self.peek()? {
            let else_span = self.advance();

            if let Some(token!(ArgSep)) = self.peek()? {
                // This is "else if"
                self.advance();
                let elif_if_span = self.expect(scope, &[ExpectKind::Keyword(If)])?;
                self.expect(scope, &[ExpectKind::ArgSep])?;
                let (elif_cond, elif_bind) = self.parse_cond(scope)?;
                let elif_body = self.parse_block_through_dedent(scope)?;

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
                let else_body = self.parse_block_through_dedent(scope)?;

                else_branch = Some((else_body, else_span));
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

    fn parse_try(&mut self, scope: &mut Scope) -> Result<Try> {
        use self::{Ident, Keyword, Try};
        use Keyword::*;
        use TokenInfo::*;

        let try_span = self.expect(scope, &[ExpectKind::Keyword(Keyword::Try)])?;
        self.expect(scope, &[ExpectKind::Indent])?;
        let body_block = self.parse_block_through_dedent(scope)?;

        let body = Function {
            params: vec![],
            ret: None,
            stub_span: None,
            body: body_block,
        };

        let mut handlers = Vec::new();
        let mut has_catch_all = false;

        while let Some(token!(Keyword(Catch))) = self.peek()? {
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
                self.expect(scope, &[ExpectKind::Indent])?;
                let catch_block = self.parse_block_through_dedent(scope)?;
                handlers.push(CatchHandler {
                    class_expr: Some(expr),
                    func: Function {
                        params: vec![Param::Pos {
                            ident: Ident::new(var_span),
                            ty: None,
                            default: None,
                        }],
                        ret: None,
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
                self.expect(scope, &[ExpectKind::Indent])?;
                let catch_block = self.parse_block_through_dedent(scope)?;
                handlers.push(CatchHandler {
                    class_expr: None,
                    func: Function {
                        params: vec![Param::Pos {
                            ident: Ident::new(var_span),
                            ty: None,
                            default: None,
                        }],
                        ret: None,
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
            self.expect(scope, &[ExpectKind::Indent])?;
            let finally_block = self.parse_block_through_dedent(scope)?;
            Some((
                Function {
                    params: vec![],
                    ret: None,
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

    fn parse_while(&mut self, scope: &mut Scope) -> Result<Stmt> {
        let while_span = self.advance();
        self.expect(scope, &[ExpectKind::ArgSep])?;
        let (cond, bind) = self.parse_cond(scope)?;
        let body = self.parse_block_through_dedent(scope)?;
        Ok(Stmt::While(While {
            expr: cond,
            bind,
            body,
            while_span,
        }))
    }

    fn parse_for(&mut self, scope: &mut Scope) -> Result<Stmt> {
        let for_span = self.expect(scope, &[ExpectKind::Keyword(Keyword::For)])?;
        self.expect(scope, &[ExpectKind::ArgSep])?;
        let bind = self.parse_pattern(scope, false)?;
        let (equal_span, expr) = match self.next()? {
            Some(token!(TokenInfo::Equal, equal_span)) => {
                self.expect(scope, &[ExpectKind::ArgSep])?;
                let expr = self.parse_cmd_or_expr(scope, false)?;
                self.expect(scope, &[ExpectKind::Indent])?;
                (Some(equal_span), Some(expr))
            }
            Some(token!(TokenInfo::Indent)) => (None, None),
            other => {
                return Err(self.syntax_error(
                    scope,
                    other,
                    "expected `=` or indent after `for` pattern",
                ));
            }
        };
        let body = self.parse_block_through_dedent(scope)?;
        Ok(Stmt::For(For {
            bind,
            expr,
            body,
            iter: None,
            for_span,
            equal_span,
        }))
    }

    pub(super) fn parse_stmt(&mut self, scope: &mut Scope) -> Result<Stmt> {
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
                Some(token!(Keyword(Let | Def | Class | Import))) => (),
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

        if !decorators.is_empty() && !matches!(self.peek()?, Some(token!(Keyword(Def | Class)))) {
            let token = self.peek()?;
            return Err(self.syntax_error(
                scope,
                token,
                "decorators are only valid before `def` or `class`",
            ));
        }

        match self.peek()? {
            Some(token!(Keyword(Let))) => self.parse_let(scope, pub_span),
            Some(token!(Keyword(Def))) => {
                Ok(Stmt::Def(self.parse_def(scope, pub_span, decorators)?))
            }
            Some(token!(Keyword(Class))) => {
                Ok(Stmt::Class(self.parse_class(scope, pub_span, decorators)?))
            }
            Some(token!(Keyword(If))) => Ok(Stmt::Prim(PrimStmt::If(self.parse_if(scope)?))),
            Some(token!(Keyword(Try))) => Ok(Stmt::Prim(PrimStmt::Try(self.parse_try(scope)?))),
            Some(token!(Keyword(While))) => self.parse_while(scope),
            Some(token!(Keyword(For))) => self.parse_for(scope),
            Some(token!(Keyword(Bind))) => Ok(Stmt::Bind(self.parse_bind(scope)?)),
            Some(token!(Keyword(Import))) => {
                let import = self.parse_import(scope, pub_span)?;
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
                        Some(self.parse_cmd_or_expr(scope, true)?)
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
                let expr = self.parse_cmd_or_expr(scope, true)?;
                Ok(Stmt::Throw(Throw { expr, span }))
            }
            Some(token!(Keyword(Continue))) => Ok(Stmt::Continue(self.advance(), None)),
            Some(token!(Keyword(Break))) => Ok(Stmt::Break(self.advance(), None)),
            Some(token!(Keyword(Do))) => Ok(Stmt::Prim(PrimStmt::Expr(
                self.parse_do_block(scope, true)?,
            ))),
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
                        let rhs = self.parse_rhs(scope)?;
                        Ok(Stmt::Assign(Assign {
                            lhs,
                            rhs,
                            equal_span,
                        }))
                    }
                    _ => {
                        // This is just an expression
                        let mut args = vec![];
                        self.parse_cmd_args(scope, true, true, &mut args)?;
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
                    _ => stmts.push(self.parse_stmt(scope)?),
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
