use super::{
    ExprMode, Parser, Result, Scope, diag::SpecialMethodOutsideClass, params::ParamMode,
    stream::ExpectKind,
};
use crate::{
    ast::{
        Block, Decorator, Def, Expr, Function, Ident, Method, Param, PrimStmt, SpecialMethod, Stmt,
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
        let expr = self.parse_expr(scope, ExprMode::Full)?;
        Ok(Expr::Lambda {
            func: Function {
                params,
                body: Block {
                    stmts: vec![Stmt::Prim(PrimStmt::Expr(expr))],
                    vars: Default::default(),
                    repl: None,
                },
            },
            do_span: Some(do_span),
        })
    }

    fn parse_do_params(&mut self, scope: &mut Scope) -> Result<Vec<Param>> {
        match self.peek()? {
            Some(token!(TokenInfo::Indent)) => return Ok(vec![]),
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
        match self.peek()? {
            Some(token!(TokenInfo::Op(Op::Bar))) => {
                self.advance();
                let params = self.parse_params(scope, ParamMode::HorizFunc)?;
                self.expect(scope, &[ExpectKind::Op(Op::Bar)])?;
                if let Some(token!(TokenInfo::ArgSep)) = self.peek()? {
                    self.advance();
                }
                Ok(params)
            }
            _ => Ok(vec![]),
        }
    }

    pub(super) fn parse_do_block(
        &mut self,
        scope: &mut Scope,
        allow_trailing: bool,
    ) -> Result<Expr> {
        let do_span = self.expect(scope, &[ExpectKind::Keyword(Keyword::Do)])?;
        let params = self.parse_do_params(scope)?;
        match self.peek()? {
            Some(token!(TokenInfo::Indent)) if allow_trailing => {
                self.advance();
                let function = Function {
                    params,
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

    fn parse_def_common(
        &mut self,
        scope: &mut Scope,
    ) -> Result<(Span, Span, Option<SpecialMethod>, Function)> {
        let def_span = self.expect(scope, &[ExpectKind::Keyword(Keyword::Def)])?;
        self.expect(scope, &[ExpectKind::ArgSep])?;
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
        let params = match self.peek()? {
            Some(token!(TokenInfo::Indent)) => self.parse_params(scope, ParamMode::VertFunc)?,
            Some(token!(TokenInfo::LeftParen)) => {
                let left = self.advance();
                let _right = self.expect_matching(scope, ExpectKind::RightParen, left);
                self.expect(scope, &[ExpectKind::Indent])?;
                // FIXME: include paren spans somewhere
                vec![]
            }
            _ => {
                let params = self.parse_params(scope, ParamMode::HorizFunc)?;
                self.expect(scope, &[ExpectKind::Indent])?;
                params
            }
        };
        let body = self.parse_block_through_dedent(scope)?;
        Ok((def_span, name_span, special, Function { params, body }))
    }

    pub(super) fn parse_def(
        &mut self,
        scope: &mut Scope,
        pub_span: Option<Span>,
        decorators: Vec<Decorator>,
    ) -> Result<Def> {
        let (def_span, name_span, special, func) = self.parse_def_common(scope)?;

        if special.is_some() {
            self.fail = true;
            self.diags.push(SpecialMethodOutsideClass(name_span));
        }

        Ok(Def {
            def_span,
            decorators,
            ident: Ident::new(name_span),
            func,
            pub_span,
        })
    }

    pub(super) fn parse_method(
        &mut self,
        scope: &mut Scope,
        pub_span: Option<Span>,
        decorators: Vec<Decorator>,
    ) -> Result<Method> {
        let (def_span, name_span, special, func) = self.parse_def_common(scope)?;
        Ok(Method {
            def_span,
            decorators,
            name_span,
            special,
            node: None,
            private_sym: None,
            func,
            pub_span,
        })
    }
}
