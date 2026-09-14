use super::{
    ExprMode, Parser, Result, Scope,
    diag::{
        InvalidConstType, NonConstExpr, OptionalRest, OptionalTypeArg, ParamsWithoutArrow,
        RequiredAfterOptional,
    },
    stream::ExpectKind,
};
use crate::{
    ast::{Annot, Const, Ident, RetType, TypeArg, TypeArgKind, TypeExpr, TypeKey, visit::Node},
    lex::{Keyword, Mode, Op, Token, TokenInfo},
    source::Span,
};

/// Delimiters enclosing type arguments
#[derive(Clone, Copy, PartialEq, Eq)]
enum Delim {
    Bracket,
    Paren,
    Brace,
}

impl Delim {
    fn close(self) -> ExpectKind {
        match self {
            Delim::Bracket => ExpectKind::RightBracket,
            Delim::Paren => ExpectKind::RightParen,
            Delim::Brace => ExpectKind::RightBrace,
        }
    }

    fn is_close(self, info: &TokenInfo) -> bool {
        matches!(
            (self, info),
            (Delim::Bracket, TokenInfo::RightBracket)
                | (Delim::Paren, TokenInfo::RightParen)
                | (Delim::Brace, TokenInfo::RightBrace)
        )
    }
}

/// A compact type, or a parenthesized list that is only a type if `->` follows it
enum Compact {
    Type(TypeExpr),
    Params {
        args: Vec<TypeArg>,
        paren_span: Span,
    },
}

impl Parser<'_> {
    /// Parse a `@` annotation if one is next.
    pub(super) fn parse_annot(&mut self, scope: &mut Scope) -> Result<Option<Box<Annot>>> {
        Ok(match self.peek()? {
            Some(token!(TokenInfo::At)) => {
                let at_span = self.advance();
                let ty = self.with_type_mode(|this| this.parse_type_compact(scope))?;
                Some(Box::new(Annot { at_span, ty }))
            }
            _ => None,
        })
    }

    /// Parse a `->` return type if one is next.
    pub(super) fn parse_ret_type(&mut self, scope: &mut Scope) -> Result<Option<Box<RetType>>> {
        let Some(token!(TokenInfo::Arrow)) = self.peek()? else {
            return Ok(None);
        };
        let arrow_span = self.advance();
        let ty = self.with_type_mode(|this| {
            this.expect(scope, &[ExpectKind::ArgSep])?;
            this.parse_type_compact(scope)
        })?;
        Ok(Some(Box::new(RetType { arrow_span, ty })))
    }

    /// Lex a compact type so that whitespace ends it, even within a full expression.
    ///
    /// The token before the type must already be consumed.
    fn with_type_mode<R>(
        &mut self,
        f: impl for<'b> FnOnce(&'b mut Self) -> Result<R>,
    ) -> Result<R> {
        if self.mode() != Mode::FullExpr {
            return f(self);
        }
        let res = self.with_mode(Mode::Type, f)?;
        // Finding the end of the type peeked the whitespace after it, which means
        // nothing in the enclosing full expression
        if let Some(token!(TokenInfo::ArgSep)) = self.peek()? {
            self.advance();
        }
        Ok(res)
    }

    /// Parse a compact type, which whitespace ends in shell-like contexts.
    pub(super) fn parse_type_compact(&mut self, scope: &mut Scope) -> Result<TypeExpr> {
        let compact = self.parse_type_compact_or_params(scope)?;
        Ok(self.finish_params(compact))
    }

    /// Parse a full type, as found within `[]`, `()` and `{}`.
    fn parse_type_full(&mut self, scope: &mut Scope) -> Result<TypeExpr> {
        let leading = match self.peek()? {
            Some(token!(TokenInfo::Op(Op::Bar))) => Some(self.advance()),
            _ => None,
        };
        let first = self.parse_type_compact_or_params(scope)?;
        if leading.is_none()
            && let Some(token!(TokenInfo::Arrow)) = self.peek()?
        {
            let arrow_span = self.advance();
            let (params, paren_span) = match first {
                Compact::Params { args, paren_span } => {
                    self.check_type_params(&args);
                    (args, Some(paren_span))
                }
                Compact::Type(ty) => (
                    vec![TypeArg {
                        optional: None,
                        kind: TypeArgKind::Pos(ty),
                        delim_span: None,
                    }],
                    None,
                ),
            };
            let ret = self.parse_type_full(scope)?;
            return Ok(TypeExpr::Func {
                params,
                paren_span,
                arrow_span,
                ret: Box::new(ret),
            });
        }
        let first = self.finish_params(first);
        if leading.is_none() && !matches!(self.peek()?, Some(token!(TokenInfo::Op(Op::Bar)))) {
            return Ok(first);
        }
        let mut bars: Vec<Span> = leading.into_iter().collect();
        let mut members = vec![first];
        while let Some(token!(TokenInfo::Op(Op::Bar))) = self.peek()? {
            bars.push(self.advance());
            members.push(self.parse_type_compact(scope)?);
        }
        if let Some(token @ token!(TokenInfo::Arrow)) = self.peek()? {
            return Err(self.syntax_error(
                scope,
                Some(token),
                "a union must be parenthesized to be a parameter type",
            ));
        }
        Ok(TypeExpr::Union { members, bars })
    }

    fn parse_type_compact_or_params(&mut self, scope: &mut Scope) -> Result<Compact> {
        let mut compact = self.parse_type_primary(scope)?;
        // In shell-like contexts, whitespace before `[` lexes as a separator, which
        // ends the type
        while let Some(token!(TokenInfo::LeftBracket)) = self.peek()? {
            let left = self.advance();
            let base = self.finish_params(compact);
            let (args, bracket_span) = self.parse_type_args(scope, Delim::Bracket, left)?;
            compact = Compact::Type(TypeExpr::App {
                base: Box::new(base),
                args,
                bracket_span,
            });
        }
        Ok(compact)
    }

    fn parse_type_primary(&mut self, scope: &mut Scope) -> Result<Compact> {
        let ty = match decay_ident!(self.peek()?) {
            Some(token!(TokenInfo::Ident)) => {
                let head = Ident::new(self.advance());
                let mut fields = Vec::new();
                while let Some(token!(TokenInfo::Op(Op::Dot))) = self.peek()? {
                    self.advance();
                    match decay_field!(self.next()?) {
                        Some(token!(TokenInfo::Ident, span)) => fields.push(span),
                        other => {
                            return Err(self.syntax_error(
                                scope,
                                other,
                                "expected name after `.` in type",
                            ));
                        }
                    }
                }
                TypeExpr::Name { head, fields }
            }
            Some(token!(TokenInfo::LeftParen)) => {
                let left = self.advance();
                let (args, paren_span) = self.parse_type_args(scope, Delim::Paren, left)?;
                return Ok(Compact::Params { args, paren_span });
            }
            Some(token!(TokenInfo::LeftBrace)) => {
                let left = self.advance();
                let (args, brace_span) = self.parse_type_args(scope, Delim::Brace, left)?;
                TypeExpr::Schema { args, brace_span }
            }
            Some(
                token!(
                    TokenInfo::Sym
                        | TokenInfo::DQuote
                        | TokenInfo::RawQuote
                        | TokenInfo::BQuote
                        | TokenInfo::TQuote
                        | TokenInfo::Int(_)
                        | TokenInfo::F64
                        | TokenInfo::Bool(_)
                        | TokenInfo::Keyword(Keyword::Nil)
                ),
            ) => {
                let expr = self.parse_expr_primary(scope, ExprMode::Full)?;
                match expr.fold(self.file) {
                    Some(
                        Const::Sym(_) | Const::Str(_) | Const::Int(_) | Const::Bool(_) | Const::Nil,
                    ) => {}
                    Some(_) => {
                        self.fail = true;
                        self.diags.push(InvalidConstType(expr.span()));
                    }
                    None => {
                        self.fail = true;
                        self.diags.push(NonConstExpr(expr.span()));
                    }
                }
                TypeExpr::Const {
                    expr: Box::new(expr),
                }
            }
            _ => {
                let token = self.next()?;
                return Err(self.syntax_error(scope, token, "expected type"));
            }
        };
        Ok(Compact::Type(ty))
    }

    /// Parse the items of `[]`, `()` or `{}` after the opening delimiter.
    fn parse_type_args(
        &mut self,
        scope: &mut Scope,
        delim: Delim,
        open: Span,
    ) -> Result<(Vec<TypeArg>, Span)> {
        self.with_mode(Mode::FullExpr, |this| {
            let mut args = Vec::new();
            let close = loop {
                if let Some(token) = this.peek()?
                    && delim.is_close(&token.info)
                {
                    break this.advance();
                }
                let optional = match this.peek()? {
                    Some(token!(TokenInfo::Question)) => Some(this.advance()),
                    _ => None,
                };
                if let Some(span) = optional
                    && delim == Delim::Bracket
                {
                    this.fail = true;
                    this.diags.push(OptionalTypeArg(span));
                }
                let kind = match this.peek()? {
                    Some(token!(TokenInfo::Ellipsis)) => {
                        let ellipsis_span = this.advance();
                        if let Some(span) = optional
                            && delim != Delim::Bracket
                        {
                            this.fail = true;
                            this.diags.push(OptionalRest(span));
                        }
                        TypeArgKind::Rest {
                            ellipsis_span,
                            ty: this.parse_type_full(scope)?,
                        }
                    }
                    Some(token!(TokenInfo::Key, span)) => {
                        this.advance();
                        TypeArgKind::Key {
                            key: TypeKey::Sym(span),
                            colon_span: span.after_right_char(),
                            ty: this.parse_type_full(scope)?,
                        }
                    }
                    _ => {
                        let ty = this.parse_type_full(scope)?;
                        match this.peek()? {
                            Some(token @ token!(TokenInfo::Colon)) => match ty {
                                TypeExpr::Const { expr }
                                    if matches!(expr.fold(this.file), Some(Const::Str(_))) =>
                                {
                                    let colon_span = this.advance();
                                    TypeArgKind::Key {
                                        key: TypeKey::Str(expr),
                                        colon_span,
                                        ty: this.parse_type_full(scope)?,
                                    }
                                }
                                _ => {
                                    return Err(this.syntax_error(
                                        scope,
                                        Some(token),
                                        "a key in a type must be a name or a string",
                                    ));
                                }
                            },
                            _ => TypeArgKind::Pos(ty),
                        }
                    }
                };
                let delim_span = this.consume_comma()?;
                args.push(TypeArg {
                    optional,
                    kind,
                    delim_span,
                });
                if delim_span.is_none() {
                    break this.expect(scope, &[delim.close()])?;
                }
            };
            Ok((args, open | close))
        })
    }

    /// Check the parameter list of a function type.
    fn check_type_params(&mut self, args: &[TypeArg]) {
        let mut seen_optional = false;
        for arg in args {
            if let TypeArgKind::Pos(ty) = &arg.kind {
                if arg.optional.is_some() {
                    seen_optional = true;
                } else if seen_optional {
                    self.fail = true;
                    self.diags.push(RequiredAfterOptional(ty.span()));
                }
            }
        }
    }

    /// Interpret a compact type that `->` does not follow.
    fn finish_params(&mut self, compact: Compact) -> TypeExpr {
        match compact {
            Compact::Type(ty) => ty,
            Compact::Params {
                mut args,
                paren_span,
            } => {
                if let [
                    TypeArg {
                        optional: None,
                        kind: TypeArgKind::Pos(_),
                        delim_span: None,
                    },
                ] = args.as_slice()
                    && let Some(TypeArg {
                        kind: TypeArgKind::Pos(ty),
                        ..
                    }) = args.pop()
                {
                    TypeExpr::Group {
                        ty: Box::new(ty),
                        paren_span,
                    }
                } else {
                    self.fail = true;
                    self.diags.push(ParamsWithoutArrow(paren_span));
                    TypeExpr::Error
                }
            }
        }
    }
}
