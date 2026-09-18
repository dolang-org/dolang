use super::{
    ExprMode, Parser, Result, Scope,
    diag::{
        InvalidConstType, NonConstExpr, OptionalRest, OptionalTypeArg, ParamsWithoutArrow,
        RequiredAfterOptional, RestMustBeTrailing,
    },
    params::rest_order_error,
    stream::ExpectKind,
};
use crate::{
    RestKind,
    ast::{
        Annot, Binder, BinderDefault, BinderKind, Binders, Const, Ident, RetType, TypeArg,
        TypeArgKind, TypeExpr, TypeKey, visit::Node,
    },
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
        self.parse_annot_with_ellipsis(scope, false)
            .map(|(annot, _)| annot)
    }

    pub(super) fn parse_annot_with_ellipsis(
        &mut self,
        scope: &mut Scope,
        allow_ellipsis: bool,
    ) -> Result<(Option<Box<Annot>>, Option<Span>)> {
        let mut ellipsis = None;
        let annot = match self.peek()? {
            Some(token!(TokenInfo::At)) => {
                let at_span = self.advance();
                if let Some(token!(TokenInfo::ArgSep)) = self.peek()? {
                    self.advance();
                }
                let ty = self.with_inline_shell(|this| {
                    if allow_ellipsis && matches!(this.peek()?, Some(token!(TokenInfo::Ellipsis))) {
                        ellipsis = Some(this.advance());
                    }
                    this.parse_type_compact(scope)
                })?;
                Some(Box::new(Annot { at_span, ty }))
            }
            _ => None,
        };
        Ok((annot, ellipsis))
    }

    /// Parse a `->` return type if one is next.
    pub(super) fn parse_ret_type(&mut self, scope: &mut Scope) -> Result<Option<Box<RetType>>> {
        let Some(token!(TokenInfo::Arrow)) = self.peek()? else {
            return Ok(None);
        };
        let arrow_span = self.advance();
        let ty = self.with_inline_shell(|this| {
            this.expect(scope, &[ExpectKind::ArgSep])?;
            this.parse_type_compact(scope)
        })?;
        Ok(Some(Box::new(RetType { arrow_span, ty })))
    }

    /// Parse the binders in `[]` after a declared name if they are next.
    pub(super) fn parse_binders(&mut self, scope: &mut Scope) -> Result<Option<Box<Binders>>> {
        let Some(token!(TokenInfo::LeftBracket)) = self.peek()? else {
            return Ok(None);
        };
        let open = self.advance();
        self.with_mode(Mode::FullExpr, |this| {
            let mut binders = Vec::new();
            // The last rest, which only another rest may follow
            let mut last_rest = None;
            let mut seen_default = false;
            let close = loop {
                if let Some(token!(TokenInfo::RightBracket)) = this.peek()?
                    && !binders.is_empty()
                {
                    break this.advance();
                }
                let (kind, ident) = match this.next()? {
                    Some(token!(TokenInfo::DittoKey, span)) => (
                        BinderKind::Key {
                            colon_span: span.before_left_char(),
                        },
                        span,
                    ),
                    Some(
                        token @ token!(
                            TokenInfo::Ellipsis
                                | TokenInfo::Op(Op::Star)
                                | TokenInfo::Op(Op::StarStar)
                        ),
                    ) => {
                        let kind = match token.info {
                            TokenInfo::Ellipsis => RestKind::Mixed,
                            TokenInfo::Op(Op::Star) => RestKind::Pos,
                            _ => RestKind::Key,
                        };
                        if let Some(msg) =
                            last_rest.and_then(|(prev, _)| rest_order_error(prev, kind))
                        {
                            return Err(this.syntax_error(scope, Some(token), msg));
                        }
                        last_rest = Some((kind, token.span));
                        let ident = this.expect(scope, &[ExpectKind::Ident])?;
                        (
                            BinderKind::Rest {
                                kind,
                                sigil_span: token.span,
                            },
                            ident,
                        )
                    }
                    token => match decay_ident!(token) {
                        Some(token!(TokenInfo::Ident, span)) => (BinderKind::Pos, span),
                        token => return Err(this.syntax_error(scope, token, "expected binder")),
                    },
                };
                if !matches!(kind, BinderKind::Rest { .. })
                    && let Some((_, span)) = last_rest.take()
                {
                    this.fail = true;
                    this.diags.push(RestMustBeTrailing(span));
                }
                let bound = this.parse_annot(scope)?;
                let default = if let Some(token!(TokenInfo::Equal)) = this.peek()? {
                    let equal_span = this.advance();
                    if matches!(kind, BinderKind::Rest { .. }) {
                        return Err(this.syntax_error(
                            scope,
                            Some(Token {
                                info: TokenInfo::Equal,
                                span: equal_span,
                            }),
                            "a rest binder cannot have a default",
                        ));
                    }
                    Some(Box::new(BinderDefault {
                        equal_span,
                        ty: this.parse_type_full(scope)?,
                    }))
                } else {
                    None
                };
                if matches!(kind, BinderKind::Pos) {
                    if default.is_some() {
                        seen_default = true;
                    } else if seen_default {
                        this.fail = true;
                        this.diags.push(RequiredAfterOptional(ident));
                    }
                }
                let delim_span = this.consume_comma()?;
                binders.push(Binder {
                    kind,
                    ident: Ident::new(ident),
                    bound,
                    default,
                    delim_span,
                    node: None,
                });
                if delim_span.is_none() {
                    break this.expect(scope, &[ExpectKind::RightBracket])?;
                }
            };
            Ok(Some(Box::new(Binders {
                binders,
                bracket_span: open | close,
            })))
        })
    }

    /// Parse type arguments in `[]` after the opening bracket.
    pub(super) fn parse_type_bracket_args(
        &mut self,
        scope: &mut Scope,
        open: Span,
    ) -> Result<(Vec<TypeArg>, Span)> {
        self.parse_type_args(scope, Delim::Bracket, open)
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
                TypeExpr::Name {
                    head,
                    fields,
                    decl: None,
                }
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
                    Some(token @ token!(TokenInfo::Op(Op::Star) | TokenInfo::Op(Op::StarStar))) => {
                        let sigil_span = this.advance();
                        if let Some(span) = optional
                            && delim != Delim::Bracket
                        {
                            this.fail = true;
                            this.diags.push(OptionalRest(span));
                        }
                        TypeArgKind::Rest {
                            kind: match token.info {
                                TokenInfo::Op(Op::Star) => RestKind::Pos,
                                _ => RestKind::Key,
                            },
                            sigil_span,
                            ty: this.parse_type_full(scope)?,
                        }
                    }
                    Some(token!(TokenInfo::Ellipsis)) => {
                        let ellipsis_span = this.advance();
                        if let Some(span) = optional
                            && delim != Delim::Bracket
                        {
                            this.fail = true;
                            this.diags.push(OptionalRest(span));
                        }
                        // `K:` is one lexer token, while a compound key type such as
                        // `Tuple[Int, Int]:` leaves the `:` as its own token.
                        if let Some(token!(TokenInfo::Key, key_span)) = this.peek()? {
                            if delim == Delim::Paren {
                                let token = this.next()?;
                                return Err(this.syntax_error(
                                    scope,
                                    token,
                                    "a keyed rest item is not valid in function parameters",
                                ));
                            }
                            this.advance();
                            TypeArgKind::KeyRest {
                                ellipsis_span,
                                key_ty: TypeExpr::Name {
                                    head: Ident::new(key_span),
                                    fields: Vec::new(),
                                    decl: None,
                                },
                                colon_span: key_span.after_right_char(),
                                ty: this.parse_type_full(scope)?,
                            }
                        } else if matches!(this.peek()?, Some(token!(TokenInfo::Comma)) | None)
                            || this
                                .peek()?
                                .is_some_and(|token| delim.is_close(&token.info))
                        {
                            if delim == Delim::Paren {
                                let token = this.peek()?;
                                return Err(this.syntax_error(
                                    scope,
                                    token,
                                    "an open rest item is not valid in function parameters",
                                ));
                            }
                            TypeArgKind::OpenRest { ellipsis_span }
                        } else {
                            let ty = this.parse_type_full(scope)?;
                            if let Some(token!(TokenInfo::Colon)) = this.peek()? {
                                if delim == Delim::Paren {
                                    let token = this.next()?;
                                    return Err(this.syntax_error(
                                        scope,
                                        token,
                                        "a keyed rest item is not valid in function parameters",
                                    ));
                                }
                                let colon_span = this.advance();
                                TypeArgKind::KeyRest {
                                    ellipsis_span,
                                    key_ty: ty,
                                    colon_span,
                                    ty: this.parse_type_full(scope)?,
                                }
                            } else {
                                TypeArgKind::Rest {
                                    kind: RestKind::Mixed,
                                    sigil_span: ellipsis_span,
                                    ty,
                                }
                            }
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
                            // A schema key may be any type, while a named type argument or
                            // parameter has a name
                            Some(token @ token!(TokenInfo::Colon)) => {
                                if delim != Delim::Brace {
                                    return Err(this.syntax_error(
                                        scope,
                                        Some(token),
                                        "a key outside a schema must be a name",
                                    ));
                                }
                                let colon_span = this.advance();
                                TypeArgKind::Key {
                                    key: TypeKey::Type(Box::new(ty)),
                                    colon_span,
                                    ty: this.parse_type_full(scope)?,
                                }
                            }
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
