use super::{
    ExprMode, Parser, Result, Scope,
    diag::{RedundantTypeOnly, SpecialMethodOutsideClass},
    params::ParamMode,
    stream::ExpectKind,
};
use crate::{
    ast::{
        Binders, Block, Decorator, Def, Expr, Function, Ident, Method, Param, PrimStmt, RetType,
        SpecialMethod, Stmt,
    },
    lex::{self, Keyword, Op, Token, TokenInfo},
    source::Span,
};

impl Parser<'_> {
    fn parse_lambda_params(&mut self, scope: &mut Scope) -> Result<Vec<Param>> {
        match self.peek()? {
            Some(token!(TokenInfo::Op(Op::Bar))) => {
                self.advance();
                let params = self.parse_params(scope, ParamMode::HorizFunc)?;
                self.expect(scope, &[ExpectKind::Op(Op::Bar)])?;
                Ok(params)
            }
            _ => Ok(vec![]),
        }
    }

    pub(super) fn parse_lambda(&mut self, scope: &mut Scope, do_span: Span) -> Result<Expr> {
        let params = self.parse_lambda_params(scope)?;
        let ret = self.parse_ret_type(scope)?;
        let expr = self.parse_expr(scope, ExprMode::Full)?;
        Ok(Expr::Lambda {
            func: Function {
                params,
                ret,
                body: Block {
                    stmts: vec![Stmt::Prim(PrimStmt::Expr(expr))],
                    vars: Default::default(),
                    repl: None,
                },
            },
            do_span: Some(do_span),
        })
    }

    fn parse_do_params(&mut self, scope: &mut Scope) -> Result<(Vec<Param>, Option<Box<RetType>>)> {
        match self.peek()? {
            Some(token!(TokenInfo::Indent)) => return Ok((vec![], None)),
            Some(token!(TokenInfo::ArgSep)) => self.advance(),
            _ => {
                let token = self.next()?;
                return Err(self.syntax_error(
                    scope,
                    token,
                    "expected parameters, statement or indent after `do`",
                ));
            }
        };
        let params = match self.peek()? {
            Some(token!(TokenInfo::Op(Op::Bar))) => {
                self.advance();
                let params = self.parse_params(scope, ParamMode::HorizFunc)?;
                self.expect(scope, &[ExpectKind::Op(Op::Bar)])?;
                if let Some(token!(TokenInfo::ArgSep)) = self.peek()? {
                    self.advance();
                }
                params
            }
            _ => vec![],
        };
        let ret = self.parse_ret_type(scope)?;
        if ret.is_some()
            && let Some(token!(TokenInfo::ArgSep)) = self.peek()?
        {
            self.advance();
        }
        Ok((params, ret))
    }

    pub(super) fn parse_do_block(
        &mut self,
        scope: &mut Scope,
        allow_trailing: bool,
    ) -> Result<Expr> {
        let do_span = self.expect(scope, &[ExpectKind::Keyword(Keyword::Do)])?;
        let (params, ret) = self.parse_do_params(scope)?;
        match self.peek()? {
            Some(token!(TokenInfo::Indent)) if allow_trailing => {
                self.advance();
                let function = Function {
                    params,
                    ret,
                    body: self.parse_block_through_dedent(scope)?,
                };
                Ok(Expr::Lambda {
                    func: function,
                    do_span: Some(do_span),
                })
            }
            _ => Ok(Expr::Lambda {
                func: Function {
                    params,
                    ret,
                    body: Block {
                        stmts: vec![if allow_trailing {
                            self.parse_stmt(scope)?
                        } else {
                            Stmt::Prim(self.parse_cmd(scope, allow_trailing)?)
                        }],
                        vars: Default::default(),
                        repl: None,
                    },
                },
                do_span: Some(do_span),
            }),
        }
    }

    pub(super) fn parse_decorators(&mut self, scope: &mut Scope) -> Result<Vec<Decorator>> {
        let mut decorators = Vec::new();
        while let Some(token!(TokenInfo::DecoratorOpen)) = self.peek()? {
            let open_span = self.expect(scope, &[ExpectKind::DecoratorOpen])?;
            let expr = self.with_mode(lex::Mode::FullExpr, |this| {
                let expr = this.parse_expr(scope, ExprMode::Full)?;
                let close_span = this.expect_matching(scope, ExpectKind::RightBracket, open_span);
                Ok(Decorator {
                    open_span,
                    expr,
                    close_span,
                })
            })?;
            decorators.push(expr);
            self.expect(scope, &[ExpectKind::StmtSep])?;
        }
        Ok(decorators)
    }

    /// Parse a `def`, which has no body when it is type-only: marked with `@`, or a
    /// protocol member.
    fn parse_def_common(&mut self, scope: &mut Scope, protocol: bool) -> Result<DefCommon> {
        let def_span = self.expect(scope, &[ExpectKind::Keyword(Keyword::Def)])?;
        self.expect(scope, &[ExpectKind::ArgSep])?;
        let at_span = match self.peek()? {
            Some(token!(TokenInfo::At)) => {
                let span = self.advance();
                if protocol {
                    self.fail = true;
                    self.diags.push(RedundantTypeOnly(span));
                }
                Some(span)
            }
            _ => None,
        };
        let type_only = protocol || at_span.is_some();
        // A declaration names what it defines; nothing after `def` is read as
        // the keyword it spells, so a function may take the name of one.
        let (name_span, special) = match decay_ident!(self.next()?) {
            Some(token!(TokenInfo::LeftParen)) => {
                let span = self.expect(scope, &[ExpectKind::Ident])?;
                self.expect(scope, &[ExpectKind::RightParen])?;
                (span, Some(self.special_method(scope, span)?))
            }
            Some(token!(TokenInfo::Ident, span)) => (span, None),
            token => {
                return Err(self.syntax_error(scope, token, "expected function or special method"));
            }
        };
        let binders = self.parse_binders(scope)?;
        let params = match self.peek()? {
            Some(token!(TokenInfo::Indent)) => self.parse_params(scope, ParamMode::VertFunc)?,
            Some(token!(TokenInfo::LeftParen)) => {
                let left = self.advance();
                let _right = self.expect_matching(scope, ExpectKind::RightParen, left);
                // FIXME: include paren spans somewhere
                vec![]
            }
            _ if type_only => self.parse_params(scope, ParamMode::HorizSig)?,
            _ => self.parse_params(scope, ParamMode::HorizFunc)?,
        };
        // The return type follows the parameters, or the `do` ending vertical ones
        if let Some(token!(TokenInfo::ArgSep)) = self.peek()? {
            self.advance();
        }
        let ret = self.parse_ret_type(scope)?;
        if ret.is_some()
            && let Some(token!(TokenInfo::ArgSep)) = self.peek()?
        {
            self.advance();
        }
        let body = if type_only {
            match self.peek()? {
                None | Some(token!(TokenInfo::StmtSep | TokenInfo::Dedent)) => Block {
                    stmts: vec![],
                    vars: Default::default(),
                    repl: None,
                },
                other => {
                    return Err(self.syntax_error(
                        scope,
                        other,
                        "a type-only declaration has no body",
                    ));
                }
            }
        } else {
            self.expect(scope, &[ExpectKind::Indent])?;
            self.parse_block_through_dedent(scope)?
        };
        Ok(DefCommon {
            def_span,
            at_span,
            name_span,
            special,
            binders,
            func: Function { params, ret, body },
        })
    }

    pub(super) fn parse_def(
        &mut self,
        scope: &mut Scope,
        pub_span: Option<Span>,
        decorators: Vec<Decorator>,
    ) -> Result<Def> {
        let DefCommon {
            def_span,
            at_span,
            name_span,
            special,
            binders,
            func,
        } = self.parse_def_common(scope, false)?;

        if special.is_some() {
            self.fail = true;
            self.diags.push(SpecialMethodOutsideClass(name_span));
        }

        Ok(Def {
            def_span,
            decorators,
            at_span,
            ident: Ident::new(name_span),
            binders,
            func,
            pub_span,
            node: None,
        })
    }

    pub(super) fn parse_method(
        &mut self,
        scope: &mut Scope,
        pub_span: Option<Span>,
        decorators: Vec<Decorator>,
        protocol: bool,
    ) -> Result<Method> {
        let DefCommon {
            def_span,
            at_span,
            name_span,
            special,
            binders,
            func,
        } = self.parse_def_common(scope, protocol)?;
        Ok(Method {
            def_span,
            decorators,
            at_span,
            type_only: protocol || at_span.is_some(),
            name_span,
            special,
            node: None,
            private_sym: None,
            binders,
            func,
            pub_span,
        })
    }
}

/// What a function and a method declaration share
struct DefCommon {
    def_span: Span,
    at_span: Option<Span>,
    name_span: Span,
    special: Option<SpecialMethod>,
    binders: Option<Box<Binders>>,
    func: Function,
}
