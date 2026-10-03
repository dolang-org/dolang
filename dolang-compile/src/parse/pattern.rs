use super::{
    ExprMode, Parser, Result, Scope,
    diag::{DuplicateImplicit, ImplicitInPattern, RequiredAfterOptional, RestMustBeTrailing},
    stream::ExpectKind,
};
use crate::{
    RestKind,
    ast::visit::Node,
    ast::{
        Annot, ClassSuper, Ident, Implicit, Implicits, PatBind, PatDefault, PatIdent, PatItem,
        Pattern, TypePattern,
    },
    lex::{Keyword, Mode, Op, Token, TokenInfo},
    source::Span,
};

/// Why a rest of `kind` cannot follow a rest of `prev`, if it cannot.
fn rest_order_error(prev: RestKind, kind: RestKind) -> Option<&'static str> {
    match (prev, kind) {
        (RestKind::Pos, RestKind::Key) => None,
        (RestKind::Key, RestKind::Pos) => Some("`*` rest must come before `**`"),
        (RestKind::Mixed, _) | (_, RestKind::Mixed) if prev != kind => {
            Some("`...` rest cannot be combined with `*` or `**`")
        }
        _ => Some("duplicate rest"),
    }
}

/// Build a pattern from its top-level items.
///
/// A lone positional item without a default matches the whole value rather than
/// unpacking it, so it stands for what it binds: a name, or a sub-pattern.
fn collapse_pattern(mut items: Vec<PatItem>) -> Pattern {
    if let [PatItem::Pos { default: None, .. }] = items.as_slice()
        && let Some(PatItem::Pos { bind, ty, .. }) = items.pop()
    {
        return match bind {
            PatBind::Ident(ident) => Pattern::Ident(PatIdent { ident, ty }),
            PatBind::Nested { pattern, .. } => *pattern,
        };
    }
    Pattern::Unpack(items)
}

#[derive(Copy, Clone)]
pub(super) enum PatMode {
    HorizFunc,
    /// Horizontal parameters of a declaration without a body, which end with the statement
    HorizSig,
    VertFunc,
    /// Vertical parameters of a declaration without a body
    VertSig,
    // Horizontal binding form, like `let` or `for`
    HorizBind,
    // Vertical binding form, like `bind`
    VertBind,
    /// The items of a horizontal sub-pattern, within `()`
    Nested,
}

impl PatMode {
    fn is_pattern(&self) -> bool {
        matches!(self, Self::HorizBind | Self::VertBind | Self::Nested)
    }

    fn is_vertical(&self) -> bool {
        matches!(self, Self::VertFunc | Self::VertSig | Self::VertBind)
    }

    fn supports_defaults(&self) -> bool {
        !matches!(self, Self::HorizBind)
    }

    /// Whether the items unpack arguments, which constrains their order. A
    /// declaration without a body only describes calls, leaving any constraint to
    /// the type checker.
    fn unpacks(&self) -> bool {
        !matches!(self, Self::HorizSig | Self::VertSig)
    }
}

impl Parser<'_> {
    fn report_non_trailing_variadic(
        &mut self,
        variadic: bool,
        variadic_span: Option<Span>,
        variadic_trailing_reported: &mut bool,
    ) {
        if variadic && !*variadic_trailing_reported {
            self.fail = true;
            self.diags.push(RestMustBeTrailing(
                variadic_span.expect("variadic span missing"),
            ));
            *variadic_trailing_reported = true;
        }
    }

    pub(super) fn parse_pattern(&mut self, scope: &mut Scope, vertical: bool) -> Result<Pattern> {
        // A pattern binds values, so any implicit in it has been diagnosed
        let (items, _) = self.parse_pat_items(
            scope,
            if vertical {
                PatMode::VertBind
            } else {
                PatMode::HorizBind
            },
        )?;
        Ok(collapse_pattern(items))
    }

    /// Parse a horizontal sub-pattern, starting at its `(`.
    fn parse_sub_pattern(&mut self, scope: &mut Scope) -> Result<PatBind> {
        let open = self.advance();
        // Within `()`, newlines are only whitespace, so a long pattern can wrap
        self.with_mode(Mode::InlineShell, |this| {
            let (items, _) = this.parse_pat_items(scope, PatMode::Nested)?;
            let close = this.expect_matching(scope, ExpectKind::RightParen, open);
            Ok(PatBind::Nested {
                pattern: Box::new(Pattern::Unpack(items)),
                parens: Some((open, close)),
            })
        })
    }

    /// Parse what a key item binds after its key: a name, a horizontal
    /// sub-pattern or, in vertical layout, an indented sub-pattern.
    ///
    /// Returns the annotation of a vertical sub-pattern that collapses to a name.
    fn parse_key_bind(
        &mut self,
        scope: &mut Scope,
        mode: PatMode,
    ) -> Result<(PatBind, Option<Box<Annot>>)> {
        if mode.unpacks()
            && mode.is_vertical()
            && let Some(token!(TokenInfo::Indent)) = self.peek()?
        {
            let (items, _) = self.parse_pat_items(scope, PatMode::VertBind)?;
            return Ok(match collapse_pattern(items) {
                Pattern::Ident(PatIdent { ident, ty }) => (PatBind::Ident(ident), ty),
                pattern => (
                    PatBind::Nested {
                        pattern: Box::new(pattern),
                        parens: None,
                    },
                    None,
                ),
            });
        }
        self.expect(scope, &[ExpectKind::ArgSep])?;
        if mode.unpacks()
            && let Some(token!(TokenInfo::LeftParen)) = self.peek()?
        {
            return Ok((self.parse_sub_pattern(scope)?, None));
        }
        self.parse_named_bind(scope, mode).map(|bind| (bind, None))
    }

    /// A name binding or a runtime class test, including a dotted class name.
    fn parse_named_bind(&mut self, scope: &mut Scope, mode: PatMode) -> Result<PatBind> {
        let span = match decay_ident!(self.next()?) {
            Some(token!(TokenInfo::Ident, span)) => span,
            token => {
                return Err(self.syntax_error(
                    scope,
                    token,
                    "expected variable name to receive value",
                ));
            }
        };
        if !mode.unpacks() {
            return Ok(PatBind::Ident(Ident::new(span)));
        }
        let mut fields = Vec::new();
        let mut next = self.peek()?;
        while matches!(next, Some(token!(TokenInfo::Op(Op::Dot)))) {
            self.advance();
            let field = match decay_field!(self.next()?) {
                Some(token!(TokenInfo::Ident, span)) => span,
                token => {
                    return Err(self.syntax_error(
                        scope,
                        token,
                        "expected field name after `.` in type-test pattern",
                    ));
                }
            };
            fields.push(field);
            next = self.peek()?;
        }
        let horizontal = matches!(next, Some(token!(TokenInfo::LeftParen)));
        if mode.is_vertical() && matches!(next, Some(token!(TokenInfo::ArgSep))) {
            self.advance();
            next = self.peek()?;
        }
        let vertical = matches!(next, Some(token!(TokenInfo::Dollar))) && mode.is_vertical();
        if !horizontal && !vertical && fields.is_empty() {
            return Ok(PatBind::Ident(Ident::new(span)));
        }
        if !horizontal && !vertical {
            return Err(self.syntax_error(scope, next, "expected a runtime type-test pattern"));
        }
        let open = self.advance();
        let (pattern, close) = if horizontal {
            self.with_mode(Mode::InlineShell, |this| {
                let (items, _) = this.parse_pat_items(scope, PatMode::Nested)?;
                let close = this.expect_matching(scope, ExpectKind::RightParen, open);
                Ok((collapse_pattern(items), Some(close)))
            })?
        } else {
            if !matches!(self.peek()?, Some(token!(TokenInfo::Indent))) {
                let token = self.peek()?;
                return Err(self.syntax_error(
                    scope,
                    token,
                    "expected an indented type-test pattern",
                ));
            }
            let (items, _) = self.parse_pat_items(scope, PatMode::VertBind)?;
            (collapse_pattern(items), None)
        };
        Ok(PatBind::Nested {
            pattern: Box::new(Pattern::TypeTest(Box::new(TypePattern {
                class: ClassSuper {
                    at_span: None,
                    type_only: false,
                    ident: Ident::new(span),
                    fields,
                    args: Vec::new(),
                    bracket_span: None,
                    res: None,
                },
                pattern: Box::new(pattern),
                open,
                close,
            }))),
            parens: None,
        })
    }

    /// Parse the annotation and default after what an item binds.
    ///
    /// A sub-pattern takes neither: its own bindings carry any annotations.
    fn parse_item_tail(
        &mut self,
        scope: &mut Scope,
        mode: PatMode,
        bind: &PatBind,
    ) -> Result<(Option<Box<Annot>>, Option<PatDefault>)> {
        if let PatBind::Ident(_) = bind {
            let ty = self.parse_pat_annot(scope)?;
            let default = self.parse_pat_default(scope, mode)?;
            return Ok((ty, default));
        }
        if let Some(token!(TokenInfo::ArgSep)) = self.peek()? {
            self.advance();
        }
        match self.peek()? {
            token @ Some(token!(TokenInfo::At)) => Err(self.syntax_error(
                scope,
                token,
                "a sub-pattern cannot be annotated; annotate its bindings instead",
            )),
            token @ Some(token!(TokenInfo::Equal)) if mode.supports_defaults() => {
                Err(self.syntax_error(scope, token, "a sub-pattern cannot have a default"))
            }
            _ => Ok((None, None)),
        }
    }

    pub(super) fn parse_pat_items(
        &mut self,
        scope: &mut Scope,
        mode: PatMode,
    ) -> Result<(Vec<PatItem>, Implicits)> {
        use self::{Ident, Keyword, Op};
        let mut items = Vec::new();
        let mut implicits = Implicits::default();
        let mut variadic = false;
        let mut variadic_span = None;
        let mut last_rest = None;
        let mut variadic_trailing_reported = false;
        let mut seen_optional = false;
        if mode.is_vertical() {
            self.expect(scope, &[ExpectKind::Indent])?;
        }
        loop {
            match self.peek()? {
                // `()` is valid, matching an empty value
                Some(token!(TokenInfo::RightParen)) if matches!(mode, PatMode::Nested) => {
                    break Ok((items, implicits));
                }
                None
                | Some(token!(TokenInfo::Indent | TokenInfo::Op(Op::Bar) | TokenInfo::Equal))
                    if !mode.is_vertical() && !matches!(mode, PatMode::Nested) =>
                {
                    if items.is_empty() && mode.is_pattern() {
                        let token = self.next().unwrap();
                        return Err(self.syntax_error(
                            scope,
                            token,
                            "expected at least one item in pattern",
                        ));
                    }
                    break Ok((items, implicits));
                }
                Some(token!(TokenInfo::Arrow))
                    if matches!(mode, PatMode::HorizFunc | PatMode::HorizSig) =>
                {
                    break Ok((items, implicits));
                }
                Some(token!(TokenInfo::StmtSep | TokenInfo::Dedent))
                    if matches!(mode, PatMode::HorizSig) =>
                {
                    break Ok((items, implicits));
                }
                token @ Some(token!(TokenInfo::Dedent)) if mode.is_vertical() => {
                    if items.is_empty() {
                        return Err(self.syntax_error(
                            scope,
                            token,
                            "expected at least one item in pattern",
                        ));
                    }
                    self.advance();
                    if matches!(mode, PatMode::VertFunc | PatMode::VertSig) {
                        self.expect(scope, &[ExpectKind::Keyword(Keyword::Do)])?;
                    }
                    break Ok((items, implicits));
                }
                Some(token!(TokenInfo::ArgSep)) => {
                    self.advance();
                }
                Some(token!(TokenInfo::StmtSep)) if mode.is_vertical() => {
                    self.advance();
                }
                Some(token!(TokenInfo::Key)) => {
                    self.report_non_trailing_variadic(
                        variadic,
                        variadic_span,
                        &mut variadic_trailing_reported,
                    );
                    let key = self.advance();
                    let (bind, block_ty) = self.parse_key_bind(scope, mode)?;
                    let (ty, default) = self.parse_item_tail(scope, mode, &bind)?;
                    items.push(PatItem::Key {
                        key_span: key,
                        colon_span: key.after_right_char(),
                        bind,
                        ty: block_ty.or(ty),
                        default,
                    });
                }
                Some(token!(TokenInfo::DittoKey)) => {
                    self.report_non_trailing_variadic(
                        variadic,
                        variadic_span,
                        &mut variadic_trailing_reported,
                    );
                    let key = self.advance();
                    let ty = self.parse_pat_annot(scope)?;
                    let default = self.parse_pat_default(scope, mode)?;
                    items.push(PatItem::Key {
                        key_span: key,
                        colon_span: key.before_left_char(),
                        bind: PatBind::Ident(Ident::new(key)),
                        ty,
                        default,
                    })
                }
                Some(token!(TokenInfo::Op(Op::Minus))) if mode.is_vertical() => {
                    let _minus = self.advance();
                    self.expect(scope, &[ExpectKind::ArgSep])?;
                    let bind = if mode.unpacks()
                        && let Some(token!(TokenInfo::LeftParen)) = self.peek()?
                    {
                        self.parse_sub_pattern(scope)?
                    } else {
                        self.parse_named_bind(scope, mode)?
                    };
                    self.report_non_trailing_variadic(
                        variadic,
                        variadic_span,
                        &mut variadic_trailing_reported,
                    );
                    let (ty, default) = self.parse_item_tail(scope, mode, &bind)?;
                    if default.is_some() {
                        seen_optional = true;
                    } else if seen_optional && mode.unpacks() {
                        self.fail = true;
                        self.diags.push(RequiredAfterOptional(bind.span()));
                    }
                    items.push(PatItem::Pos { bind, ty, default })
                }
                Some(token @ token!(TokenInfo::Op(Op::Lt) | TokenInfo::Op(Op::Gt))) => {
                    let input = matches!(token.info, TokenInfo::Op(Op::Lt));
                    let sigil_span = self.advance();
                    // An implicit describes an ambient channel rather than a
                    // value the list unpacks, so it binds nothing
                    if mode.is_pattern() {
                        self.fail = true;
                        self.diags.push(ImplicitInPattern(sigil_span));
                    }
                    // Whitespace may separate the sigil from the type, as `@`
                    // allows, so the type itself ends at the next separator
                    if let Some(token!(TokenInfo::ArgSep)) = self.peek()? {
                        self.advance();
                    }
                    let ty = self.with_inline_shell(|this| this.parse_type_compact(scope))?;
                    let slot = if input {
                        &mut implicits.input
                    } else {
                        &mut implicits.output
                    };
                    // A list admits at most one of each, so a second is dropped
                    if slot.is_some() {
                        self.fail = true;
                        self.diags.push(DuplicateImplicit(sigil_span));
                    } else {
                        *slot = Some(Box::new(Implicit { sigil_span, ty }));
                    }
                }
                Some(
                    token @ token!(
                        TokenInfo::Ellipsis | TokenInfo::Op(Op::Star) | TokenInfo::Op(Op::StarStar)
                    ),
                ) => {
                    let kind = match token.info {
                        TokenInfo::Ellipsis => RestKind::Mixed,
                        TokenInfo::Op(Op::Star) => RestKind::Pos,
                        _ => RestKind::Key,
                    };
                    if mode.unpacks()
                        && let Some(msg) = last_rest.and_then(|prev| rest_order_error(prev, kind))
                    {
                        return Err(self.syntax_error(scope, Some(token), msg));
                    }
                    let sigil_span = self.advance();

                    // Check if followed by identifier, whitespace, or newline
                    let next_token = self.peek()?;
                    let ident = match next_token {
                        // Followed by identifier - capture case
                        Some(token!(TokenInfo::Ident)) => {
                            let span = self.advance();
                            Some(Ident::new(span))
                        }
                        // Followed by explicit whitespace separator - discard case
                        Some(token!(TokenInfo::ArgSep)) => None,
                        // Newline-related tokens (implicitly separated) - discard case
                        Some(
                            token!(TokenInfo::Indent | TokenInfo::Dedent | TokenInfo::StmtSep),
                        ) => None,
                        // Closing delimiter for do blocks and other contexts - discard case
                        Some(token!(TokenInfo::Op(Op::Bar))) => None,
                        // End of input - discard case
                        None => None,
                        // Error case - require whitespace before other delimiters
                        _ => {
                            return Err(self.syntax_error(
                                scope,
                                next_token,
                                format!(
                                    "expected identifier or whitespace after '{}'",
                                    self.file.str(sigil_span)
                                ),
                            ));
                        }
                    };
                    if let Some(token!(TokenInfo::ArgSep)) = self.peek()? {
                        self.advance();
                    }
                    let (ty, type_ellipsis_span) = self.parse_annot_with_ellipsis(scope, true)?;

                    items.push(PatItem::Rest {
                        kind,
                        sigil_span,
                        ident,
                        ty,
                        type_ellipsis_span,
                    });
                    last_rest = Some(kind);
                    variadic = mode.unpacks();
                    variadic_span.get_or_insert(sigil_span);
                }
                Some(token!(TokenInfo::LeftParen)) if mode.unpacks() => {
                    self.report_non_trailing_variadic(
                        variadic,
                        variadic_span,
                        &mut variadic_trailing_reported,
                    );
                    let bind = self.parse_sub_pattern(scope)?;
                    // A sub-pattern has no default, so it is always required
                    let (ty, default) = self.parse_item_tail(scope, mode, &bind)?;
                    if seen_optional {
                        self.fail = true;
                        self.diags.push(RequiredAfterOptional(bind.span()));
                    }
                    items.push(PatItem::Pos { bind, ty, default });
                }
                Some(token!(expr_start!())) if mode.is_pattern() => {
                    self.report_non_trailing_variadic(
                        variadic,
                        variadic_span,
                        &mut variadic_trailing_reported,
                    );
                    let (key_expr, key_const) = self.parse_expr_const(scope, ExprMode::Compact)?;

                    let colon_span = self.expect(scope, &[ExpectKind::Colon])?;
                    let (bind, block_ty) = self.parse_key_bind(scope, mode)?;
                    let (ty, default) = self.parse_item_tail(scope, mode, &bind)?;

                    items.push(PatItem::ConstKey {
                        key_expr,
                        key_const,
                        bind,
                        ty: block_ty.or(ty),
                        default,
                        colon_span,
                    });
                }
                other => match decay_ident!(other) {
                    Some(token!(TokenInfo::Ident)) => {
                        let bind = self.parse_named_bind(scope, mode)?;
                        let span = bind.span();
                        self.report_non_trailing_variadic(
                            variadic,
                            variadic_span,
                            &mut variadic_trailing_reported,
                        );
                        let (ty, default) = self.parse_item_tail(scope, mode, &bind)?;
                        if default.is_some() {
                            seen_optional = true;
                        } else if seen_optional && mode.unpacks() {
                            self.fail = true;
                            self.diags.push(RequiredAfterOptional(span));
                        }
                        items.push(PatItem::Pos { bind, ty, default })
                    }

                    _ => {
                        let token = self.next()?;
                        return Err(self.syntax_error(
                            scope,
                            token,
                            if mode.is_pattern() {
                                "invalid pattern"
                            } else {
                                "invalid parameter"
                            },
                        ));
                    }
                },
            }
        }
    }

    /// Parse the annotation after a bound name, with optional whitespace before it.
    fn parse_pat_annot(&mut self, scope: &mut Scope<'_>) -> Result<Option<Box<Annot>>> {
        if let Some(token!(TokenInfo::ArgSep)) = self.peek()? {
            self.advance();
        }
        self.parse_annot(scope)
    }

    fn parse_pat_default(
        &mut self,
        scope: &mut Scope<'_>,
        mode: PatMode,
    ) -> Result<Option<PatDefault>> {
        Ok(if mode.supports_defaults() {
            let mut next = self.peek()?;
            if matches!(next, Some(token!(TokenInfo::ArgSep))) {
                self.advance();
                next = self.peek()?;
            }
            match next {
                Some(token!(TokenInfo::Equal)) => {
                    let delim_span = self.advance();
                    self.expect(scope, &[ExpectKind::ArgSep])?;
                    let expr = self.parse_expr(scope, ExprMode::Compact)?;
                    let fold = expr.fold(self.file);
                    Some(PatDefault {
                        delim_span,
                        expr,
                        fold,
                    })
                }
                _ => None,
            }
        } else {
            None
        })
    }
}
