use super::{
    ExprMode, Parser, Result, Scope,
    diag::{
        DuplicateImplicit, ImplicitInSchema, InvalidConstType, NonConstExpr, OptionalQuant,
        OptionalTypeArg, ParamsWithoutArrow, QuantifiedImplicit,
    },
    stream::ExpectKind,
};
use crate::{
    RestKind,
    ast::{
        Annot, Binder, BinderDefault, BinderKind, Binders, Const, Ident, Implicit, Implicits,
        RetType, TypeArg, TypeArgKind, TypeExpr, TypeKey, TypeParam, TypeParamKind, TypeQuant,
        visit::Node,
    },
    lex::{Keyword, Mode, Op, Token, TokenInfo},
    source::Span,
};

/// What a list of type parameters belongs to
#[derive(Clone, Copy, PartialEq, Eq)]
enum Params {
    /// A function type's, in `()`
    Func,
    /// A schema's, in `{}`
    Schema,
}

impl Params {
    fn close(self) -> ExpectKind {
        match self {
            Params::Func => ExpectKind::RightParen,
            Params::Schema => ExpectKind::RightBrace,
        }
    }

    fn is_close(self, info: &TokenInfo) -> bool {
        matches!(
            (self, info),
            (Params::Func, TokenInfo::RightParen) | (Params::Schema, TokenInfo::RightBrace)
        )
    }
}

/// Returns the kind of rest a parameter's or binder's sigil introduces, if it is
/// one.
fn rest_sigil(info: &TokenInfo) -> Option<RestKind> {
    match info {
        TokenInfo::Ellipsis => Some(RestKind::Mixed),
        TokenInfo::Op(Op::Star) => Some(RestKind::Pos),
        TokenInfo::Op(Op::StarStar) => Some(RestKind::Key),
        _ => None,
    }
}

/// A parenthesized type expression, whose meaning is not known until a `->`
/// either follows it, making it a function type's parameters, or does not,
/// making it a grouped type
enum Group {
    Type(TypeExpr),
    Params {
        params: Vec<TypeParam>,
        implicits: Implicits,
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
                    Some(token) if let Some(kind) = rest_sigil(&token.info) => {
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

    /// Parse a compact type, which whitespace ends in shell-like contexts.
    pub(super) fn parse_type_compact(&mut self, scope: &mut Scope) -> Result<TypeExpr> {
        let group = self.parse_type_compact_or_params(scope)?;
        Ok(self.finish_params(group))
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
            let (params, implicits, paren_span) = match first {
                Group::Params {
                    params,
                    implicits,
                    paren_span,
                } => (params, implicits, Some(paren_span)),
                Group::Type(ty) => (
                    vec![TypeParam {
                        quant: None,
                        kind: Some(TypeParamKind::Pos(ty)),
                        delim_span: None,
                    }],
                    Implicits::default(),
                    None,
                ),
            };
            let ret = self.parse_type_full(scope)?;
            return Ok(TypeExpr::Func {
                params,
                input: implicits.input,
                output: implicits.output,
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

    fn parse_type_compact_or_params(&mut self, scope: &mut Scope) -> Result<Group> {
        let mut group = self.parse_type_primary(scope)?;
        // In shell-like contexts, whitespace before `[` lexes as a separator, which
        // ends the type
        while let Some(token!(TokenInfo::LeftBracket)) = self.peek()? {
            let left = self.advance();
            let base = self.finish_params(group);
            let (args, bracket_span) = self.parse_type_args(scope, left)?;
            group = Group::Type(TypeExpr::App {
                base: Box::new(base),
                args,
                bracket_span,
            });
        }
        Ok(group)
    }

    fn parse_type_primary(&mut self, scope: &mut Scope) -> Result<Group> {
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
                let (params, implicits, paren_span) =
                    self.parse_type_params(scope, Params::Func, left)?;
                return Ok(Group::Params {
                    params,
                    implicits,
                    paren_span,
                });
            }
            Some(token!(TokenInfo::LeftBrace)) => {
                let left = self.advance();
                let (params, _, brace_span) =
                    self.parse_type_params(scope, Params::Schema, left)?;
                TypeExpr::Schema { params, brace_span }
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
        Ok(Group::Type(ty))
    }

    /// Parse type arguments after the opening `[`.
    pub(super) fn parse_type_args(
        &mut self,
        scope: &mut Scope,
        open: Span,
    ) -> Result<(Vec<TypeArg>, Span)> {
        self.with_mode(Mode::FullExpr, |this| {
            let mut args = Vec::new();
            let close = loop {
                if let Some(token!(TokenInfo::RightBracket)) = this.peek()? {
                    break this.advance();
                }
                if let Some(token!(TokenInfo::Question)) = this.peek()? {
                    let span = this.advance();
                    this.fail = true;
                    this.diags.push(OptionalTypeArg(span));
                }
                let kind = match this.peek()? {
                    Some(token @ token!(TokenInfo::Op(Op::Star) | TokenInfo::Op(Op::StarStar))) => {
                        return Err(this.syntax_error(
                            scope,
                            Some(token),
                            "type arguments expand a pack only with `...`",
                        ));
                    }
                    Some(token!(TokenInfo::Ellipsis)) => TypeArgKind::Expand {
                        ellipsis_span: this.advance(),
                        ty: this.parse_type_full(scope)?,
                    },
                    Some(token!(TokenInfo::Key, name)) => {
                        this.advance();
                        TypeArgKind::Key {
                            name,
                            colon_span: name.after_right_char(),
                            ty: this.parse_type_full(scope)?,
                        }
                    }
                    _ => {
                        let ty = this.parse_type_full(scope)?;
                        if let Some(token @ token!(TokenInfo::Colon)) = this.peek()? {
                            return Err(this.syntax_error(
                                scope,
                                Some(token),
                                "a key outside a schema must be a name",
                            ));
                        }
                        TypeArgKind::Pos(ty)
                    }
                };
                let delim_span = this.consume_comma()?;
                args.push(TypeArg { kind, delim_span });
                if delim_span.is_none() {
                    break this.expect(scope, &[ExpectKind::RightBracket])?;
                }
            };
            Ok((args, open | close))
        })
    }

    /// Parse the parameters of a function type or a schema after the opening
    /// delimiter.
    fn parse_type_params(
        &mut self,
        scope: &mut Scope,
        list: Params,
        open: Span,
    ) -> Result<(Vec<TypeParam>, Implicits, Span)> {
        self.with_mode(Mode::FullExpr, |this| {
            let mut params = Vec::new();
            let mut implicits = Implicits::default();
            let close = loop {
                if let Some(token) = this.peek()?
                    && list.is_close(&token.info)
                {
                    break this.advance();
                }
                let quant = match this.peek()? {
                    Some(token!(TokenInfo::Question)) => Some(TypeQuant::Opt(this.advance())),
                    Some(token!(TokenInfo::Op(Op::Star))) => Some(TypeQuant::Star(this.advance())),
                    Some(token!(TokenInfo::Op(Op::StarStar))) => {
                        Some(TypeQuant::StarStar(this.advance()))
                    }
                    _ => None,
                };
                // An item takes one quantifier, so `?` cannot also repeat.
                if let Some(TypeQuant::Opt(span)) = quant
                    && matches!(
                        this.peek()?,
                        Some(token!(
                            TokenInfo::Op(Op::Star) | TokenInfo::Op(Op::StarStar)
                        ))
                    )
                {
                    this.fail = true;
                    this.diags.push(OptionalQuant(span));
                    this.advance();
                }
                // An implicit is an item of its own rather than an element a
                // quantifier applies to, and a list holds at most one of each.
                if let Some(token) = this.peek()?
                    && matches!(token.info, TokenInfo::Op(Op::Lt) | TokenInfo::Op(Op::Gt))
                {
                    let input = matches!(token.info, TokenInfo::Op(Op::Lt));
                    let sigil_span = this.advance();
                    if let Some(quant) = &quant {
                        this.fail = true;
                        this.diags.push(QuantifiedImplicit(quant.span()));
                    }
                    // A schema describes data, and an ambient channel is not data
                    if list != Params::Func {
                        this.fail = true;
                        this.diags.push(ImplicitInSchema(sigil_span));
                    }
                    let ty = this.parse_type_full(scope)?;
                    let slot = if input {
                        &mut implicits.input
                    } else {
                        &mut implicits.output
                    };
                    if slot.is_some() {
                        this.fail = true;
                        this.diags.push(DuplicateImplicit(sigil_span));
                    } else {
                        *slot = Some(Box::new(Implicit { sigil_span, ty }));
                    }
                    if this.consume_comma()?.is_none() {
                        break this.expect(scope, &[list.close()])?;
                    }
                    continue;
                }
                // `*` and `**` may stand alone, admitting any item of their kind.
                let bare = matches!(quant, Some(TypeQuant::Star(_) | TypeQuant::StarStar(_)))
                    && this.at_item_end(list)?;
                let kind = if bare {
                    None
                } else {
                    Some(this.parse_type_element(scope, list)?)
                };
                let delim_span = this.consume_comma()?;
                params.push(TypeParam {
                    quant,
                    kind,
                    delim_span,
                });
                if delim_span.is_none() {
                    break this.expect(scope, &[list.close()])?;
                }
            };
            Ok((params, implicits, open | close))
        })
    }

    /// Whether the next token ends an item, so that a quantifier stands alone.
    fn at_item_end(&mut self, list: Params) -> Result<bool> {
        Ok(
            matches!(self.peek()?, Some(token!(TokenInfo::Comma)) | None)
                || self.peek()?.is_some_and(|token| list.is_close(&token.info)),
        )
    }

    /// Parse the element a quantifier applies to: `T`, `k: V`, `(K): V`, `...S`,
    /// or an open `...`.
    fn parse_type_element(&mut self, scope: &mut Scope, list: Params) -> Result<TypeParamKind> {
        if let Some(token!(TokenInfo::Ellipsis)) = self.peek()? {
            let ellipsis_span = self.advance();
            if self.at_item_end(list)? {
                return Ok(TypeParamKind::Open { ellipsis_span });
            }
            return Ok(TypeParamKind::Include {
                ellipsis_span,
                ty: self.parse_type_full(scope)?,
            });
        }
        // `k:` is one lexer token, while a compound key type such as
        // `Tuple[Int, Int]:` leaves the `:` as its own token.
        if let Some(token!(TokenInfo::Key, span)) = self.peek()? {
            self.advance();
            return Ok(TypeParamKind::Key {
                key: TypeKey::Sym(span),
                colon_span: span.after_right_char(),
                ty: self.parse_type_full(scope)?,
            });
        }
        let ty = self.parse_type_full(scope)?;
        match self.peek()? {
            // A schema key may be any type, while a parameter's key is a name
            Some(token @ token!(TokenInfo::Colon)) => {
                if list != Params::Schema {
                    return Err(self.syntax_error(
                        scope,
                        Some(token),
                        "a parameter key must be a name",
                    ));
                }
                let colon_span = self.advance();
                Ok(TypeParamKind::Key {
                    key: TypeKey::Type(Box::new(ty)),
                    colon_span,
                    ty: self.parse_type_full(scope)?,
                })
            }
            _ => Ok(TypeParamKind::Pos(ty)),
        }
    }

    /// Interpret a parenthesized list that `->` does not follow.
    fn finish_params(&mut self, group: Group) -> TypeExpr {
        match group {
            Group::Type(ty) => ty,
            Group::Params {
                mut params,
                implicits,
                paren_span,
            } => {
                if let [
                    TypeParam {
                        quant: None,
                        kind: Some(TypeParamKind::Pos(_)),
                        delim_span: None,
                    },
                ] = params.as_slice()
                    // An implicit describes a function, so it leaves no grouped type
                    && implicits.input.is_none()
                    && implicits.output.is_none()
                    && let Some(TypeParam {
                        kind: Some(TypeParamKind::Pos(ty)),
                        ..
                    }) = params.pop()
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
