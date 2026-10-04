use std::mem;

use super::{
    ExprMode, Parser, Result, Scope,
    diag::{
        DuplicateImplicit, ImplicitInPattern, OptionalName, OptionalNamedRest,
        OptionalNeedsDefault, RequiredAfterOptional, RestMustBeTrailing, SyntaxDiag,
    },
    stream::ExpectKind,
};
use crate::{
    RestKind,
    ast::visit::Node,
    ast::{
        Alternation, Annot, ClassSuper, Ident, Implicit, Implicits, PatBind, PatDefault, PatIdent,
        PatItem, Pattern, TypePattern,
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
/// A lone positional item that isn't optional matches the whole value rather
/// than unpacking it, so it stands for what it binds: a name, keeping any default,
/// or a sub-pattern.
fn collapse_pattern(mut items: Vec<PatItem>) -> Pattern {
    if let [PatItem::Pos { bind, .. }] = items.as_slice()
        && bind.optional().is_none()
        && let Some(PatItem::Pos { bind, ty, default }) = items.pop()
    {
        return match bind {
            PatBind::Ident(ident) => Pattern::Ident(PatIdent { ident, ty, default }),
            PatBind::Nested { pattern, .. } => *pattern,
        };
    }
    Pattern::Unpack(items)
}

/// Horizontal items, or alternatives separated by `|`
enum HorizAlts {
    Items(Vec<PatItem>),
    Alt(Alternation),
}

impl HorizAlts {
    /// The pattern the items stand for, where a lone item may stand for the whole
    /// value if `collapse`; alternatives always collapse
    fn into_pattern(self, collapse: bool) -> Pattern {
        match self {
            Self::Items(items) if collapse => collapse_pattern(items),
            Self::Items(items) => Pattern::Unpack(items),
            Self::Alt(alt) => Pattern::Alt(Box::new(alt)),
        }
    }
}

const MIXED_BLOCK: &str = "a pattern block is either all `|` alternatives or has none";

#[derive(Copy, Clone)]
pub(super) enum PatMode {
    /// Horizontal parameters of a `def`
    HorizFunc,
    /// Horizontal parameters of a declaration without a body, which end with the statement
    HorizSig,
    /// Vertical parameters of a `def`
    VertFunc,
    /// Vertical parameters of a declaration without a body
    VertSig,
    // Horizontal binding form, like `let` or `for`
    HorizBind,
    // Vertical binding form, like `bind`
    VertBind,
    /// The horizontal pattern of a `match` arm, which ends at `do` or `if`
    Arm,
    /// The items of a horizontal sub-pattern, within `()`
    Nested,
    /// Parameters of a lambda, within `||`
    Lambda,
}

impl PatMode {
    /// The error for a `def` parameter that isn't a name
    fn def_param_error(&self) -> &'static str {
        if self.unpacks() {
            "a `def` parameter must be a name; unpack it in the body"
        } else {
            "a `def` parameter must be a name"
        }
    }

    fn is_pattern(&self) -> bool {
        matches!(
            self,
            Self::HorizBind | Self::VertBind | Self::Nested | Self::Arm
        )
    }

    fn is_vertical(&self) -> bool {
        matches!(self, Self::VertFunc | Self::VertSig | Self::VertBind)
    }

    /// Whether the items are a `def`'s parameters, which must be names its
    /// signature can annotate rather than sub-patterns, type tests or constants.
    fn is_def(&self) -> bool {
        matches!(
            self,
            Self::HorizFunc | Self::VertFunc | Self::HorizSig | Self::VertSig
        )
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

    /// Mark an item's sub-pattern optional for the `?` before it.
    fn apply_optional(&mut self, item: &mut PatItem, question: Span) {
        let (PatItem::Pos { bind, .. }
        | PatItem::Key { bind, .. }
        | PatItem::ConstKey { bind, .. }) = item
        else {
            unreachable!("`?` before a rest")
        };
        match bind {
            PatBind::Nested {
                pattern, optional, ..
            } => {
                *optional = Some(question);
                self.check_optional(pattern);
            }
            PatBind::Ident(ident) => {
                self.fail = true;
                self.diags.push(OptionalName(ident.span));
            }
        }
    }

    /// Check that every binding in an optional sub-pattern can do without its
    /// item: an absent item gives each one its default.
    fn check_optional(&mut self, pattern: &Pattern) {
        match pattern {
            Pattern::Constant { .. } => {}
            Pattern::Ident(PatIdent {
                ident,
                default: None,
                ..
            }) => {
                self.fail = true;
                self.diags.push(OptionalNeedsDefault(ident.span));
            }
            Pattern::Ident(_) => {}
            Pattern::TypeTest(test) => self.check_optional(&test.pattern),
            // An absent item takes the first alternative
            Pattern::Alt(alt) => self.check_optional(&alt.alts[0]),
            Pattern::Unpack(items) => {
                for item in items {
                    match item {
                        PatItem::Pos { bind, default, .. }
                        | PatItem::Key { bind, default, .. }
                        | PatItem::ConstKey { bind, default, .. } => match bind {
                            PatBind::Ident(ident) if default.is_none() => {
                                self.fail = true;
                                self.diags.push(OptionalNeedsDefault(ident.span));
                            }
                            PatBind::Ident(_) => {}
                            // Its own `?` has checked it
                            PatBind::Nested {
                                optional: Some(_), ..
                            } => {}
                            PatBind::Nested { pattern, .. } => self.check_optional(pattern),
                        },
                        PatItem::Rest {
                            ident: Some(ident), ..
                        } => {
                            self.fail = true;
                            self.diags.push(OptionalNamedRest(ident.span));
                        }
                        PatItem::Rest { ident: None, .. } => {}
                    }
                }
            }
        }
    }

    /// Parse what a vertical positional item binds, after its `- ` or `? `,
    /// whose separator is `sep`.
    ///
    /// A `-` or key there starts a sub-pattern continuing on lines at its column,
    /// as in vertical data.
    fn parse_dash_bind(&mut self, scope: &mut Scope, mode: PatMode, sep: Span) -> Result<PatBind> {
        let token = self.peek()?;
        if mode.is_def()
            && let Some(
                token!(
                    TokenInfo::LeftParen
                        | TokenInfo::Op(Op::Minus)
                        | TokenInfo::Key
                        | TokenInfo::DittoKey
                        | const_pattern_start!()
                ),
            ) = token
        {
            return Err(self.syntax_error(scope, token, mode.def_param_error()));
        }
        Ok(match token {
            Some(token!(TokenInfo::LeftParen)) => self.parse_sub_pattern(scope)?,
            Some(token!(TokenInfo::Op(Op::Minus) | TokenInfo::Key | TokenInfo::DittoKey)) => {
                self.add_indent(sep.end);
                let (items, _) = self.parse_pat_list(scope, PatMode::VertBind)?;
                PatBind::Nested {
                    pattern: Box::new(Pattern::Unpack(items)),
                    parens: None,
                    optional: None,
                }
            }
            Some(token!(const_pattern_start!())) => {
                let (expr, value) = self.parse_expr_const(scope, ExprMode::Compact)?;
                PatBind::Nested {
                    pattern: Box::new(Pattern::Constant { expr, value }),
                    parens: None,
                    optional: None,
                }
            }
            _ => self.parse_named_bind(scope, mode)?,
        })
    }

    /// Finish a vertical positional item with its annotation and default.
    fn push_dash_item(
        &mut self,
        scope: &mut Scope,
        mode: PatMode,
        bind: PatBind,
        optional: bool,
        seen_optional: &mut bool,
        items: &mut Vec<PatItem>,
    ) -> Result<()> {
        let (ty, default) = self.parse_item_tail(scope, mode, &bind)?;
        if default.is_some() || optional {
            *seen_optional = true;
        } else if *seen_optional && mode.unpacks() {
            self.fail = true;
            self.diags.push(RequiredAfterOptional(bind.span()));
        }
        items.push(PatItem::Pos { bind, ty, default });
        Ok(())
    }

    pub(super) fn parse_pattern(&mut self, scope: &mut Scope, vertical: bool) -> Result<Pattern> {
        if vertical {
            self.parse_vert_pattern(scope)
        } else {
            Ok(self
                .parse_horiz_alts(scope, PatMode::HorizBind)?
                .into_pattern(true))
        }
    }

    /// Parse horizontal items in `mode`, and any further alternatives after `|`.
    fn parse_horiz_alts(&mut self, scope: &mut Scope, mode: PatMode) -> Result<HorizAlts> {
        // A pattern binds values, so any implicit in it has been diagnosed
        let (items, _) = self.parse_pat_items(scope, mode)?;
        if !matches!(self.peek()?, Some(token!(TokenInfo::Op(Op::Bar)))) {
            return Ok(HorizAlts::Items(items));
        }
        let mut alts = vec![collapse_pattern(items)];
        let mut bars = Vec::new();
        while let Some(token!(TokenInfo::Op(Op::Bar))) = self.peek()? {
            if let [.., Pattern::Unpack(items)] = alts.as_slice()
                && items.is_empty()
            {
                let token = self.peek()?;
                return Err(self.syntax_error(scope, token, "expected an alternative"));
            }
            bars.push(self.advance());
            if let Some(token!(TokenInfo::ArgSep)) = self.peek()? {
                self.advance();
            }
            let (items, _) = self.parse_pat_items(scope, mode)?;
            if items.is_empty() {
                let token = self.peek()?;
                return Err(self.syntax_error(scope, token, "expected an alternative"));
            }
            alts.push(collapse_pattern(items));
        }
        Ok(HorizAlts::Alt(Alternation {
            alts,
            bars,
            indicator: None,
        }))
    }

    /// Parse an indented pattern block: either items, or `|` alternatives, each
    /// continuing on lines two columns in.
    fn parse_vert_pattern(&mut self, scope: &mut Scope) -> Result<Pattern> {
        self.expect(scope, &[ExpectKind::Indent])?;
        if !matches!(self.peek()?, Some(token!(TokenInfo::Op(Op::Bar)))) {
            return self.parse_vert_items(scope);
        }
        let pattern = self.parse_vert_alts(scope)?;
        match self.peek()? {
            Some(token!(TokenInfo::Dedent)) => {
                self.advance();
                Ok(pattern)
            }
            token => Err(self.syntax_error(scope, token, MIXED_BLOCK)),
        }
    }

    /// Parse lines of `|` alternatives, each continuing on lines two columns in,
    /// through the line separator after the last.
    fn parse_vert_alts(&mut self, scope: &mut Scope) -> Result<Pattern> {
        let mut alts = Vec::new();
        let mut bars = Vec::new();
        loop {
            match self.peek()? {
                Some(token!(TokenInfo::Op(Op::Bar))) => {
                    bars.push(self.advance());
                    let sep = self.expect(scope, &[ExpectKind::ArgSep])?;
                    self.add_indent(sep.end);
                    alts.push(self.parse_vert_items(scope)?);
                }
                Some(token!(TokenInfo::StmtSep)) => {
                    self.advance();
                }
                _ => break,
            }
        }
        if alts.len() == 1 {
            return Ok(alts.pop().unwrap());
        }
        Ok(Pattern::Alt(Box::new(Alternation {
            alts,
            bars,
            indicator: None,
        })))
    }

    /// Parse the pattern of a `match` arm: lines of `|` alternatives, or a
    /// horizontal pattern ending at the arm's `do` or `if`.
    pub(super) fn parse_arm_pattern(&mut self, scope: &mut Scope) -> Result<Pattern> {
        if matches!(self.peek()?, Some(token!(TokenInfo::Op(Op::Bar)))) {
            return self.parse_vert_alts(scope);
        }
        Ok(self
            .parse_horiz_alts(scope, PatMode::Arm)?
            .into_pattern(true))
    }

    /// Parse a horizontal sub-pattern, starting at its `(`.
    ///
    /// Parentheses around `|` alternatives only group them.
    fn parse_sub_pattern(&mut self, scope: &mut Scope) -> Result<PatBind> {
        let open = self.advance();
        // Within `()`, newlines are only whitespace, so a long pattern can wrap
        self.with_mode(Mode::InlineShell, |this| {
            let alts = this.parse_horiz_alts(scope, PatMode::Nested)?;
            let close = this.expect_matching(scope, ExpectKind::RightParen, open);
            Ok(PatBind::Nested {
                pattern: Box::new(alts.into_pattern(false)),
                parens: Some((open, close)),
                optional: None,
            })
        })
    }

    /// Parse what a key item binds after its key: a name, a horizontal
    /// sub-pattern or, in vertical layout, an indented sub-pattern.
    ///
    /// Returns the annotation and default of a vertical sub-pattern that collapses
    /// to a name.
    fn parse_key_bind(
        &mut self,
        scope: &mut Scope,
        mode: PatMode,
    ) -> Result<(PatBind, Option<Box<Annot>>, Option<PatDefault>)> {
        if mode.unpacks()
            && mode.is_vertical()
            && let Some(token!(TokenInfo::Indent)) = self.peek()?
        {
            return Ok(match self.parse_vert_pattern(scope)? {
                Pattern::Ident(PatIdent { ident, ty, default }) => {
                    (PatBind::Ident(ident), ty, default)
                }
                pattern => {
                    // An indented block may only annotate a `def`'s parameter
                    if mode.is_def() {
                        self.fail = true;
                        self.diags
                            .push(SyntaxDiag::new(pattern.span(), mode.def_param_error()));
                    }
                    (
                        PatBind::Nested {
                            pattern: Box::new(pattern),
                            parens: None,
                            optional: None,
                        },
                        None,
                        None,
                    )
                }
            });
        }
        self.expect(scope, &[ExpectKind::ArgSep])?;
        if let token @ Some(token!(TokenInfo::LeftParen | const_pattern_start!())) = self.peek()?
            && mode.is_def()
        {
            return Err(self.syntax_error(scope, token, mode.def_param_error()));
        }
        if let Some(token!(TokenInfo::LeftParen)) = self.peek()? {
            return Ok((self.parse_sub_pattern(scope)?, None, None));
        }
        if matches!(self.peek()?, Some(token!(const_pattern_start!()))) {
            let (expr, value) = self.parse_expr_const(scope, ExprMode::Compact)?;
            return Ok((
                PatBind::Nested {
                    pattern: Box::new(Pattern::Constant { expr, value }),
                    parens: None,
                    optional: None,
                },
                None,
                None,
            ));
        }
        self.parse_named_bind(scope, mode)
            .map(|bind| (bind, None, None))
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
        if mode.is_def() {
            let mut next = self.peek()?;
            // Whitespace before an annotation or default may be consumed
            if mode.is_vertical() && matches!(next, Some(token!(TokenInfo::ArgSep))) {
                self.advance();
                next = self.peek()?;
            }
            let vertical = mode.is_vertical() && matches!(next, Some(token!(TokenInfo::Dollar)));
            if vertical
                || matches!(
                    next,
                    Some(token!(TokenInfo::Op(Op::Dot) | TokenInfo::LeftParen))
                )
            {
                return Err(self.syntax_error(scope, next, mode.def_param_error()));
            }
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
                let alts = this.parse_horiz_alts(scope, PatMode::Nested)?;
                let close = this.expect_matching(scope, ExpectKind::RightParen, open);
                Ok((alts.into_pattern(true), Some(close)))
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
            (self.parse_vert_pattern(scope)?, None)
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
            optional: None,
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
        let constant = matches!(bind, PatBind::Nested { pattern, .. } if matches!(&**pattern, Pattern::Constant { .. }));
        match self.peek()? {
            token @ Some(token!(TokenInfo::At)) => Err(self.syntax_error(
                scope,
                token,
                if constant {
                    "a constant pattern cannot be annotated"
                } else {
                    "a sub-pattern cannot be annotated; annotate its bindings instead"
                },
            )),
            token @ Some(token!(TokenInfo::Equal)) if mode.supports_defaults() => Err(self
                .syntax_error(
                    scope,
                    token,
                    if constant {
                        "a constant pattern cannot have a default"
                    } else {
                        "a sub-pattern cannot have a default"
                    },
                )),
            _ => Ok((None, None)),
        }
    }

    pub(super) fn parse_pat_items(
        &mut self,
        scope: &mut Scope,
        mode: PatMode,
    ) -> Result<(Vec<PatItem>, Implicits)> {
        if mode.is_vertical() {
            self.expect(scope, &[ExpectKind::Indent])?;
        }
        self.parse_pat_list(scope, mode)
    }

    /// Parse items through the end of the list, which for a vertical list is its
    /// `Dedent`.
    fn parse_pat_list(
        &mut self,
        scope: &mut Scope,
        mode: PatMode,
    ) -> Result<(Vec<PatItem>, Implicits)> {
        let (items, implicits, _) = self.parse_pat_list_dashed(scope, mode)?;
        Ok((items, implicits))
    }

    /// Parse a vertical pattern's items through its `Dedent`, as a pattern. A lone
    /// item stands for the whole value unless a `-` marks it as one positional item.
    fn parse_vert_items(&mut self, scope: &mut Scope) -> Result<Pattern> {
        let (items, _, dashed) = self.parse_pat_list_dashed(scope, PatMode::VertBind)?;
        Ok(if dashed {
            Pattern::Unpack(items)
        } else {
            collapse_pattern(items)
        })
    }

    /// Parse items as [`Self::parse_pat_list`] does, and whether any is a `-` item
    fn parse_pat_list_dashed(
        &mut self,
        scope: &mut Scope,
        mode: PatMode,
    ) -> Result<(Vec<PatItem>, Implicits, bool)> {
        use self::{Ident, Keyword, Op};
        let mut dashed = false;
        let mut items = Vec::new();
        let mut implicits = Implicits::default();
        let mut variadic = false;
        let mut variadic_span = None;
        let mut last_rest = None;
        let mut variadic_trailing_reported = false;
        let mut seen_optional = false;
        let mut line_start = true;
        // A `?` and the number of items before it, until the item after it is
        // parsed
        let mut question: Option<(Span, usize)> = None;
        loop {
            if let Some((span, before)) = question
                && items.len() > before
            {
                question = None;
                self.apply_optional(items.last_mut().unwrap(), span);
            }
            let at_line_start = mem::replace(&mut line_start, false);
            match self.peek()? {
                // `()` is valid, matching an empty value; `|` ends an alternative
                Some(token!(TokenInfo::RightParen | TokenInfo::Op(Op::Bar)))
                    if matches!(mode, PatMode::Nested) =>
                {
                    break Ok((items, implicits, dashed));
                }
                token @ Some(token!(TokenInfo::Op(Op::Bar))) if mode.is_vertical() => {
                    let msg = if mode.is_def() {
                        mode.def_param_error()
                    } else if at_line_start {
                        MIXED_BLOCK
                    } else {
                        "alternatives within a line need parentheses"
                    };
                    return Err(self.syntax_error(scope, token, msg));
                }
                // An arm's pattern ends where its guard or body begins, which
                // must be on the same line
                Some(
                    token!(
                        TokenInfo::Keyword(Keyword::Do | Keyword::If)
                            | TokenInfo::StmtSep
                            | TokenInfo::Dedent
                    ),
                ) if matches!(mode, PatMode::Arm) => {
                    break Ok((items, implicits, dashed));
                }
                // In shell mode, `r|` and `t|` start here strings, but in a
                // pattern they are a name followed by `|`
                Some(token @ token!(TokenInfo::RBar | TokenInfo::TBar))
                    if matches!(mode, PatMode::HorizBind | PatMode::Arm) =>
                {
                    self.advance();
                    let name = token.span.left_char();
                    self.push_bar(token.span.right_char());
                    self.report_non_trailing_variadic(
                        variadic,
                        variadic_span,
                        &mut variadic_trailing_reported,
                    );
                    if seen_optional && mode.unpacks() {
                        self.fail = true;
                        self.diags.push(RequiredAfterOptional(name));
                    }
                    items.push(PatItem::Pos {
                        bind: PatBind::Ident(Ident::new(name)),
                        ty: None,
                        default: None,
                    });
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
                    break Ok((items, implicits, dashed));
                }
                Some(token!(TokenInfo::Arrow))
                    if matches!(
                        mode,
                        PatMode::HorizFunc | PatMode::HorizSig | PatMode::Lambda
                    ) =>
                {
                    break Ok((items, implicits, dashed));
                }
                Some(token!(TokenInfo::StmtSep | TokenInfo::Dedent))
                    if matches!(mode, PatMode::HorizSig) =>
                {
                    break Ok((items, implicits, dashed));
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
                    break Ok((items, implicits, dashed));
                }
                Some(token!(TokenInfo::ArgSep)) => {
                    self.advance();
                    line_start = at_line_start;
                }
                Some(token!(TokenInfo::StmtSep)) if mode.is_vertical() => {
                    self.advance();
                    line_start = true;
                }
                token @ Some(token!(TokenInfo::Question)) if mode.is_def() => {
                    return Err(self.syntax_error(scope, token, mode.def_param_error()));
                }
                Some(token!(TokenInfo::Question)) => {
                    let span = self.advance();
                    // `? ` starts an optional positional item, as `- ` starts one
                    if mode.is_vertical()
                        && let Some(token!(TokenInfo::ArgSep)) = self.peek()?
                    {
                        let sep = self.advance();
                        question = Some((span, items.len()));
                        let bind = self.parse_dash_bind(scope, mode, sep)?;
                        self.report_non_trailing_variadic(
                            variadic,
                            variadic_span,
                            &mut variadic_trailing_reported,
                        );
                        self.push_dash_item(
                            scope,
                            mode,
                            bind,
                            true,
                            &mut seen_optional,
                            &mut items,
                        )?;
                        continue;
                    }
                    match decay_ident!(self.peek()?) {
                        Some(
                            token!(
                                TokenInfo::LeftParen
                                    | TokenInfo::Key
                                    | TokenInfo::Ident
                                    | TokenInfo::LeftBracket
                                    | TokenInfo::LeftBrace
                                    | TokenInfo::TQuote
                                    | const_pattern_start!()
                                    // Diagnosed once parsed, as `?name` is
                                    | TokenInfo::DittoKey
                            ),
                        ) => {}
                        token => {
                            return Err(self.syntax_error(
                                scope,
                                token,
                                "expected a sub-pattern after `?`",
                            ));
                        }
                    }
                    question = Some((span, items.len()));
                }
                Some(token!(TokenInfo::Key)) => {
                    self.report_non_trailing_variadic(
                        variadic,
                        variadic_span,
                        &mut variadic_trailing_reported,
                    );
                    let key = self.advance();
                    let (bind, block_ty, block_default) = self.parse_key_bind(scope, mode)?;
                    let (ty, default) = self.parse_item_tail(scope, mode, &bind)?;
                    items.push(PatItem::Key {
                        key_span: key,
                        colon_span: key.after_right_char(),
                        bind,
                        ty: block_ty.or(ty),
                        default: block_default.or(default),
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
                    let sep = self.expect(scope, &[ExpectKind::ArgSep])?;
                    // `- ?(…)` marks the item's sub-pattern optional, as `? (…)` does
                    if let token @ Some(token!(TokenInfo::Question)) = self.peek()? {
                        if mode.is_def() {
                            return Err(self.syntax_error(scope, token, mode.def_param_error()));
                        }
                        question = Some((self.advance(), items.len()));
                    }
                    let bind = self.parse_dash_bind(scope, mode, sep)?;
                    // A vertical sub-pattern has already consumed its closing dedent.
                    let block = match &bind {
                        PatBind::Nested {
                            parens: None,
                            pattern,
                            ..
                        } => match &**pattern {
                            Pattern::Unpack(_) => true,
                            Pattern::TypeTest(test) => test.close.is_none(),
                            _ => false,
                        },
                        _ => false,
                    };
                    self.report_non_trailing_variadic(
                        variadic,
                        variadic_span,
                        &mut variadic_trailing_reported,
                    );
                    let optional = question.is_some();
                    dashed = true;
                    self.push_dash_item(
                        scope,
                        mode,
                        bind,
                        optional,
                        &mut seen_optional,
                        &mut items,
                    )?;
                    if !block {
                        self.expect_item_end(scope)?;
                    }
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
                        // Closing delimiter of a group, a type test, or lambda parameters, or an
                        // alternative separator - discard case
                        Some(token!(TokenInfo::RightParen | TokenInfo::Op(Op::Bar))) => None,
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
                token @ Some(token!(TokenInfo::LeftParen | const_pattern_start!()))
                    if mode.is_def() =>
                {
                    return Err(self.syntax_error(scope, token, mode.def_param_error()));
                }
                Some(token!(TokenInfo::LeftParen)) => {
                    self.report_non_trailing_variadic(
                        variadic,
                        variadic_span,
                        &mut variadic_trailing_reported,
                    );
                    let bind = self.parse_sub_pattern(scope)?;
                    // A sub-pattern has no default, so only `?` makes it optional
                    let (ty, default) = self.parse_item_tail(scope, mode, &bind)?;
                    if question.is_some() {
                        seen_optional = true;
                    } else if seen_optional {
                        self.fail = true;
                        self.diags.push(RequiredAfterOptional(bind.span()));
                    }
                    items.push(PatItem::Pos { bind, ty, default });
                }
                // `(` starts a sub-pattern rather than a key
                Some(
                    token @ token!(
                        TokenInfo::LeftBracket
                            | TokenInfo::LeftBrace
                            | TokenInfo::TQuote
                            | const_pattern_start!()
                    ),
                ) if mode.is_pattern()
                    || (!mode.is_def() && matches!(token.info, const_pattern_start!())) =>
                {
                    self.report_non_trailing_variadic(
                        variadic,
                        variadic_span,
                        &mut variadic_trailing_reported,
                    );
                    let scalar = matches!(token.info, const_pattern_start!());
                    let (key_expr, key_const) = self.parse_expr_const(scope, ExprMode::Compact)?;

                    // Only a scalar or string can be a positional constant, and only
                    // a pattern has constant keys.
                    if scalar
                        && !(mode.is_pattern()
                            && matches!(self.peek()?, Some(token!(TokenInfo::Colon))))
                    {
                        let bind = PatBind::Nested {
                            pattern: Box::new(Pattern::Constant {
                                expr: key_expr,
                                value: key_const,
                            }),
                            parens: None,
                            optional: None,
                        };
                        let (ty, default) = self.parse_item_tail(scope, mode, &bind)?;
                        if question.is_some() {
                            seen_optional = true;
                        } else if seen_optional {
                            self.fail = true;
                            self.diags.push(RequiredAfterOptional(bind.span()));
                        }
                        items.push(PatItem::Pos { bind, ty, default });
                        continue;
                    }
                    let colon_span = self.expect(scope, &[ExpectKind::Colon])?;
                    let (bind, block_ty, block_default) = self.parse_key_bind(scope, mode)?;
                    let (ty, default) = self.parse_item_tail(scope, mode, &bind)?;

                    items.push(PatItem::ConstKey {
                        key_expr,
                        key_const,
                        bind,
                        ty: block_ty.or(ty),
                        default: block_default.or(default),
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
                        if default.is_some() || question.is_some() {
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
