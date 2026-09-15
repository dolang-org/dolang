use super::{
    ExprMode, Parser, Result, Scope,
    diag::{RequiredAfterOptional, RestMustBeTrailing},
    stream::ExpectKind,
};
use crate::{
    ast::{Annot, Ident, Param, ParamDefault, PatIdent, Pattern},
    lex::{Keyword, Op, Token, TokenInfo},
    source::Span,
};

#[derive(Copy, Clone)]
pub(super) enum ParamMode {
    HorizFunc,
    /// Horizontal parameters of a declaration without a body, which end with the statement
    HorizSig,
    VertFunc,
    HorizPattern,
    VertPattern,
}

impl ParamMode {
    fn is_pattern(&self) -> bool {
        matches!(self, Self::HorizPattern | Self::VertPattern)
    }

    fn is_vertical(&self) -> bool {
        matches!(self, Self::VertFunc | Self::VertPattern)
    }
    fn supports_defaults(&self) -> bool {
        matches!(
            self,
            Self::HorizFunc | Self::HorizSig | Self::VertFunc | Self::VertPattern
        )
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
        let params = self.parse_params(
            scope,
            if vertical {
                ParamMode::VertPattern
            } else {
                ParamMode::HorizPattern
            },
        )?;
        Ok(match params.len() {
            0 => unreachable!(),
            1 => match &params[0] {
                Param::Pos { default: None, .. } => match params.into_iter().next().unwrap() {
                    Param::Pos { ident, ty, .. } => Pattern::Ident(PatIdent { ident, ty }),
                    _ => unreachable!(),
                },
                _ => Pattern::Unpack(params),
            },
            _ => Pattern::Unpack(params),
        })
    }

    pub(super) fn parse_params(
        &mut self,
        scope: &mut Scope,
        mode: ParamMode,
    ) -> Result<Vec<Param>> {
        use self::{Ident, Keyword, Op};
        let mut params = Vec::new();
        let mut variadic = false;
        let mut variadic_span = None;
        let mut variadic_trailing_reported = false;
        let mut seen_optional = false;
        if mode.is_vertical() {
            self.expect(scope, &[ExpectKind::Indent])?;
        }
        loop {
            match self.peek()? {
                None
                | Some(token!(TokenInfo::Indent | TokenInfo::Op(Op::Bar) | TokenInfo::Equal))
                    if !mode.is_vertical() =>
                {
                    if params.is_empty() && mode.is_pattern() {
                        let token = self.next().unwrap();
                        return Err(self.syntax_error(
                            scope,
                            token,
                            "expected at least one item in pattern",
                        ));
                    }
                    break Ok(params);
                }
                Some(token!(TokenInfo::Arrow))
                    if matches!(mode, ParamMode::HorizFunc | ParamMode::HorizSig) =>
                {
                    break Ok(params);
                }
                Some(token!(TokenInfo::StmtSep | TokenInfo::Dedent))
                    if matches!(mode, ParamMode::HorizSig) =>
                {
                    break Ok(params);
                }
                token @ Some(token!(TokenInfo::Dedent)) if mode.is_vertical() => {
                    if params.is_empty() {
                        return Err(self.syntax_error(
                            scope,
                            token,
                            "expected at least one item in pattern",
                        ));
                    }
                    self.advance();
                    if matches!(mode, ParamMode::VertFunc) {
                        self.expect(scope, &[ExpectKind::Keyword(Keyword::Do)])?;
                    }
                    break Ok(params);
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
                    self.expect(scope, &[ExpectKind::ArgSep])?;
                    let ident_span = match decay_ident!(self.next()?) {
                        Some(token!(TokenInfo::Ident, span)) => span,
                        token => {
                            return Err(self.syntax_error(
                                scope,
                                token,
                                "expected variable name to receive value",
                            ));
                        }
                    };
                    let ty = self.parse_param_annot(scope)?;
                    let default = self.parse_param_default(scope, mode)?;
                    params.push(Param::Key {
                        key_span: key,
                        colon_span: key.after_right_char(),
                        ident: Ident::new(ident_span),
                        ty,
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
                    let ty = self.parse_param_annot(scope)?;
                    let default = self.parse_param_default(scope, mode)?;
                    params.push(Param::Key {
                        key_span: key,
                        colon_span: key.before_left_char(),
                        ident: Ident::new(key),
                        ty,
                        default,
                    })
                }
                Some(token!(TokenInfo::Op(Op::Minus))) if mode.is_vertical() => {
                    let _minus = self.advance();
                    self.expect(scope, &[ExpectKind::ArgSep])?;
                    let span = self.expect(scope, &[ExpectKind::Ident])?;
                    self.report_non_trailing_variadic(
                        variadic,
                        variadic_span,
                        &mut variadic_trailing_reported,
                    );
                    let ty = self.parse_param_annot(scope)?;
                    let default = self.parse_param_default(scope, mode)?;
                    if default.is_some() {
                        seen_optional = true;
                    } else if seen_optional {
                        self.fail = true;
                        self.diags.push(RequiredAfterOptional(span));
                    }
                    params.push(Param::Pos {
                        ident: Ident::new(span),
                        ty,
                        default,
                    })
                }
                Some(token @ token!(TokenInfo::Ellipsis)) => {
                    if variadic {
                        return Err(self.syntax_error(
                            scope,
                            Some(token),
                            "duplicate rest parameter",
                        ));
                    }
                    let ellipsis_span = self.advance();

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
                                "expected identifier or whitespace after '...'",
                            ));
                        }
                    };
                    let ty = self.parse_param_annot(scope)?;

                    params.push(Param::Rest {
                        ellipsis_span,
                        ident,
                        ty,
                    });
                    variadic = true;
                    variadic_span = Some(ellipsis_span);
                }
                Some(token!(expr_start!())) if mode.is_pattern() => {
                    self.report_non_trailing_variadic(
                        variadic,
                        variadic_span,
                        &mut variadic_trailing_reported,
                    );
                    let (key_expr, key_const) = self.parse_expr_const(scope, ExprMode::Compact)?;

                    // Expect colon
                    let colon_span = self.expect(scope, &[ExpectKind::Colon])?;
                    self.expect(scope, &[ExpectKind::ArgSep])?;

                    // Parse variable name
                    let ident_span = match self.next()? {
                        Some(token!(TokenInfo::Ident, span)) => span,
                        token => {
                            return Err(self.syntax_error(
                                scope,
                                token,
                                "expected variable name after constant key",
                            ));
                        }
                    };
                    let ty = self.parse_param_annot(scope)?;

                    let default = if matches!(mode, ParamMode::VertPattern) {
                        self.parse_param_default(scope, mode)?
                    } else {
                        None
                    };

                    params.push(Param::ConstKey {
                        key_expr,
                        key_const,
                        ident: Ident::new(ident_span),
                        ty,
                        default,
                        colon_span,
                    });
                }
                other => match decay_ident!(other) {
                    Some(token!(TokenInfo::Ident)) => {
                        let span = self.advance();
                        self.report_non_trailing_variadic(
                            variadic,
                            variadic_span,
                            &mut variadic_trailing_reported,
                        );
                        let ty = self.parse_param_annot(scope)?;
                        let default = self.parse_param_default(scope, mode)?;
                        if default.is_some() {
                            seen_optional = true;
                        } else if seen_optional {
                            self.fail = true;
                            self.diags.push(RequiredAfterOptional(span));
                        }
                        params.push(Param::Pos {
                            ident: Ident::new(span),
                            ty,
                            default,
                        })
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
    fn parse_param_annot(&mut self, scope: &mut Scope<'_>) -> Result<Option<Box<Annot>>> {
        if let Some(token!(TokenInfo::ArgSep)) = self.peek()? {
            self.advance();
        }
        self.parse_annot(scope)
    }

    fn parse_param_default(
        &mut self,
        scope: &mut Scope<'_>,
        mode: ParamMode,
    ) -> Result<Option<ParamDefault>> {
        Ok(if mode.supports_defaults() {
            if let Some(token!(TokenInfo::ArgSep)) = self.peek()? {
                self.advance();
            }
            match self.peek()? {
                Some(token!(TokenInfo::Equal)) => {
                    let delim_span = self.advance();
                    self.expect(scope, &[ExpectKind::ArgSep])?;
                    let expr = self.parse_expr(scope, ExprMode::Compact)?;
                    let fold = expr.fold(self.file);
                    Some(ParamDefault {
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
