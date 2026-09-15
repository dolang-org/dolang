use super::{
    Parser, Result, Scope,
    diag::{ProtocolFieldDefault, RedundantTypeOnly},
    stream::ExpectKind,
};
use crate::{
    ast::{
        Block, Class, ClassBody, ClassMember, ClassSuper, Decorator, FieldDecl, FieldInit,
        FieldName, Function, Ident, MemberScope, PrimStmt, SpecialMethod, Stmt,
    },
    lex::{Keyword, Op, Token, TokenInfo},
    source::Span,
};

impl Parser<'_> {
    pub(super) fn special_method(
        &mut self,
        scope: &mut Scope<'_>,
        span: Span,
    ) -> Result<SpecialMethod> {
        Ok(match self.file.str(span) {
            "init" => SpecialMethod::Init,
            "call" => SpecialMethod::Call,
            "unpack" => SpecialMethod::Unpack,
            "iter" => SpecialMethod::Iter,
            "sink" => SpecialMethod::Sink,
            "next" => SpecialMethod::Next,
            "put" => SpecialMethod::Put,
            "str" => SpecialMethod::Str,
            "dbg" => SpecialMethod::Dbg,
            "verbatim" => SpecialMethod::Verbatim,
            "fmt" => SpecialMethod::Fmt,
            "add" => SpecialMethod::Add,
            "sub" => SpecialMethod::Sub,
            "rsub" => SpecialMethod::Rsub,
            "mul" => SpecialMethod::Mul,
            "div" => SpecialMethod::Div,
            "rdiv" => SpecialMethod::Rdiv,
            "ediv" => SpecialMethod::Ediv,
            "rediv" => SpecialMethod::Rediv,
            "mod" => SpecialMethod::Mod,
            "rmod" => SpecialMethod::Rmod,
            "band" => SpecialMethod::Band,
            "bor" => SpecialMethod::Bor,
            "bxor" => SpecialMethod::Bxor,
            "shl" => SpecialMethod::Shl,
            "shr" => SpecialMethod::Shr,
            "neg" => SpecialMethod::Neg,
            "bnot" => SpecialMethod::Bnot,
            "eq" => SpecialMethod::Eq,
            "lt" => SpecialMethod::Lt,
            "bool" => SpecialMethod::Bool,
            "index" => SpecialMethod::Index,
            "assign" => SpecialMethod::Assign,
            "get" => SpecialMethod::Get,
            "set" => SpecialMethod::Set,
            "hash" => SpecialMethod::Hash,
            _ => {
                return Err(self.syntax_error(
                    scope,
                    Some(Token {
                        info: TokenInfo::Ident,
                        span,
                    }),
                    "invalid special method",
                ));
            }
        })
    }

    fn parse_field(
        &mut self,
        scope: &mut Scope,
        pub_span: Option<Span>,
        decorators: Vec<Decorator>,
        protocol: bool,
    ) -> Result<FieldDecl> {
        use self::Ident;
        use TokenInfo::*;

        let field_span = self.expect(scope, &[ExpectKind::Keyword(self::Keyword::Field)])?;
        self.expect(scope, &[ExpectKind::ArgSep])?;

        let mut fields = Vec::new();
        let mut ty = None;

        loop {
            match decay_ident!(self.next()?) {
                Some(token!(Ident, span)) => fields.push(span),
                other => return Err(self.syntax_error(scope, other, "expected field name")),
            }

            if let Some(token!(ArgSep)) = self.peek()? {
                self.advance();
            }
            // One annotation after the names covers all of them
            if let Some(token!(At)) = self.peek()? {
                ty = self.parse_annot(scope)?;
                if let Some(token!(ArgSep)) = self.peek()? {
                    self.advance();
                }
                match self.peek()? {
                    Some(token!(Equal)) | None | Some(token!(StmtSep | Dedent)) => break,
                    other => {
                        return Err(self.syntax_error(
                            scope,
                            other,
                            "expected `=` or end of field declaration after type",
                        ));
                    }
                }
            }

            match self.peek()? {
                Some(token!(Equal)) | None | Some(token!(StmtSep | Dedent)) => break,
                _ => continue,
            }
        }

        let rhs = if let Some(token!(Equal)) = self.peek()? {
            let equal_span = self.expect(scope, &[ExpectKind::Equal])?;
            if protocol {
                self.fail = true;
                self.diags.push(ProtocolFieldDefault(equal_span));
            }
            self.expect(scope, &[ExpectKind::ArgSep])?;
            let rhs = self.parse_cmd_or_expr(scope, true)?;
            let init = if let Some(fold) = rhs.fold(self.file) {
                FieldInit::Const(rhs, fold)
            } else {
                FieldInit::Thunk(Function {
                    params: vec![],
                    ret: None,
                    body: Block {
                        stmts: vec![Stmt::Prim(PrimStmt::Expr(rhs))],
                        vars: vec![],
                        repl: None,
                    },
                })
            };
            Some((equal_span, init))
        } else {
            None
        };

        if let Some((equal_span, init)) = rhs {
            return Ok(FieldDecl {
                decorators,
                fields: fields
                    .into_iter()
                    .map(|span| FieldName {
                        ident: Ident::new(span),
                        node: None,
                        private_sym: None,
                    })
                    .collect(),
                ty,
                init,
                field_span,
                equal_span: Some(equal_span),
                pub_span,
                scope: MemberScope::Instance,
            });
        }

        Ok(FieldDecl {
            decorators,
            fields: fields
                .into_iter()
                .map(|span| FieldName {
                    ident: Ident::new(span),
                    node: None,
                    private_sym: None,
                })
                .collect(),
            ty,
            init: FieldInit::None,
            field_span,
            equal_span: None,
            pub_span,
            scope: MemberScope::Instance,
        })
    }

    fn parse_class_member(&mut self, scope: &mut Scope, protocol: bool) -> Result<ClassMember> {
        use self::Keyword::*;
        use TokenInfo::*;

        let decorators = self.parse_decorators(scope)?;

        let pub_span = if let Some(token!(Keyword(Pub))) = self.peek()? {
            let span = self.advance();
            self.expect(scope, &[ExpectKind::ArgSep])?;
            Some(span)
        } else {
            None
        };

        match self.peek()? {
            Some(token!(Keyword(Field))) => Ok(ClassMember::Field(
                self.parse_field(scope, pub_span, decorators, protocol)?,
            )),
            Some(token!(Keyword(Def))) => Ok(ClassMember::Method(
                self.parse_method(scope, pub_span, decorators, protocol)?,
            )),
            Some(token!(Dedent)) | None => {
                Err(self.syntax_error(scope, None, "expected statement"))
            }
            other => Err(self.syntax_error(
                scope,
                other,
                "class body only supports `field` and `def` declarations",
            )),
        }
    }

    fn parse_class_block(&mut self, scope: &mut Scope, protocol: bool) -> Result<ClassBody> {
        use TokenInfo::*;

        let mut members = Vec::new();

        loop {
            let done = (|| -> Result<bool> {
                match self.peek()? {
                    None | Some(token!(Dedent)) => return Ok(true),
                    Some(token!(StmtSep)) => {
                        self.advance();
                    }
                    _ => members.push(self.parse_class_member(scope, protocol)?),
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
                    self.lex.set_error();
                    while !matches!(self.peek()?, None | Some(token!(StmtSep | Dedent))) {
                        self.advance();
                    }
                }
            }
        }

        Ok(ClassBody { members })
    }

    pub(super) fn parse_class(
        &mut self,
        scope: &mut Scope,
        pub_span: Option<Span>,
        decorators: Vec<Decorator>,
    ) -> Result<Class> {
        let class_span = self.expect(scope, &[ExpectKind::Keyword(Keyword::Class)])?;
        self.expect(scope, &[ExpectKind::ArgSep])?;
        // `@` makes the class a protocol, which exists only in types
        let at_span = match self.peek()? {
            Some(token!(TokenInfo::At)) => Some(self.advance()),
            _ => None,
        };
        let protocol = at_span.is_some();

        // Class name can be either `Name` (Ident) or `Name:` (Key) if there's a superclass
        // The span of a Key token excludes the `:`, so we can use it directly for the identifier
        let (ident, binders, colon_span) = match self.next()? {
            Some(token!(TokenInfo::Ident, span)) => {
                // Binders end the name, so a `:` after them is a token of its own
                let binders = self.parse_binders(scope)?;
                let colon_span = match self.peek()? {
                    Some(token!(TokenInfo::Colon)) if binders.is_some() => Some(self.advance()),
                    _ => None,
                };
                (Ident::new(span), binders, colon_span)
            }
            Some(token!(TokenInfo::Key, span)) => {
                (Ident::new(span), None, Some(span.after_right_char()))
            }
            other => {
                return Err(self.syntax_error(scope, other, "expected class name after `class`"));
            }
        };

        // Superclasses are space-separated dotted names
        let mut super_refs = vec![];
        if colon_span.is_some() {
            loop {
                match self.peek()? {
                    None
                    | Some(token!(TokenInfo::StmtSep | TokenInfo::Indent | TokenInfo::Dedent)) => {
                        break;
                    }
                    Some(token!(TokenInfo::ArgSep)) => {
                        self.advance();
                    }
                    _ => super_refs.push(self.parse_class_super(scope, protocol)?),
                }
            }
        }

        let body = match self.peek()? {
            // A class without a body may end the block it is in
            Some(token!(TokenInfo::Dedent)) => ClassBody { members: vec![] },
            _ => match self.next()? {
                Some(token!(TokenInfo::Indent)) => {
                    let block = self.parse_class_block(scope, protocol)?;
                    self.expect(scope, &[ExpectKind::Dedent])?;
                    block
                }
                None | Some(token!(TokenInfo::StmtSep)) => ClassBody { members: vec![] },
                other => {
                    return Err(self.syntax_error(
                        scope,
                        other,
                        "expected indent or newline after class declaration",
                    ));
                }
            },
        };

        Ok(Class {
            class_span,
            decorators,
            at_span,
            ident,
            binders,
            colon_span,
            super_refs,
            body,
            pub_span,
            node: None,
        })
    }

    fn parse_class_super(&mut self, scope: &mut Scope, protocol: bool) -> Result<ClassSuper> {
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
        let ident = match decay_ident!(self.next()?) {
            Some(token!(TokenInfo::Ident, span)) => Ident::new(span),
            other => return Err(self.syntax_error(scope, other, "expected superclass name")),
        };
        let mut fields = Vec::new();
        while let Some(token!(TokenInfo::Op(Op::Dot))) = self.peek()? {
            self.advance();
            let field = match decay_field!(self.next()?) {
                Some(token!(TokenInfo::Ident, span)) => span,
                other => {
                    return Err(self.syntax_error(
                        scope,
                        other,
                        "expected field name after `.` in superclass reference",
                    ));
                }
            };
            fields.push(field);
        }
        let (args, bracket_span) = match self.peek()? {
            Some(token!(TokenInfo::LeftBracket)) => {
                let open = self.advance();
                let (args, bracket_span) = self.parse_type_bracket_args(scope, open)?;
                (args, Some(bracket_span))
            }
            _ => (vec![], None),
        };
        Ok(ClassSuper {
            at_span,
            type_only: protocol || at_span.is_some(),
            ident,
            fields,
            args,
            bracket_span,
            decl: None,
        })
    }
}
