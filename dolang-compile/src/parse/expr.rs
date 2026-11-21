use std::{borrow::Cow, cmp::Ordering};

use super::{
    Error, ExprMode, Parser, Result, Scope,
    diag::{
        AmbigIndex, BadFloat, InvalidCompactOp, MisleadingArg, MisleadingCall, MisleadingDollar,
        NonConstExpr,
    },
    stream::ExpectKind,
    string::StrKind,
};
use crate::{
    ast::{
        Arg, ArrayElem, CondPattern, CondPatternKind, Const, DictElem, Expand, Expr, ExprBody, For,
        GetVariant, GroupDelim, Ident, If, IfBranch, Key, Pair, Pattern, Single, visit::Node,
    },
    lex::{self, Keyword, Mode, Op, Token, TokenInfo},
    source::Span,
};

/// What parenthesized items make outside a C-style call
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ParenKind {
    Group,
    Tuple,
    Pack,
}

#[derive(PartialEq, Eq, Debug)]
enum Assoc {
    Left,
    Right,
}

#[derive(PartialEq, Eq, Debug)]
struct Prec(
    /// Associativity
    Assoc,
    /// Precedence (higher binds more "tightly")
    u32,
);

impl Prec {
    const INDEX: Self = Self(Assoc::Left, 2100);
    const CALL: Self = Self(Assoc::Left, 2000);
    const SHIFT: Self = Self(Assoc::Left, 1175);
    const RANGE: Self = Self(Assoc::Left, 1150);
    const DOLLAR_CALL: Self = Self(Assoc::Right, 500);

    fn terminate(&self, min: &Option<Prec>) -> bool {
        match min {
            None => false,
            Some(min) => match self.1.cmp(&min.1) {
                Ordering::Less => true,
                Ordering::Greater => false,
                // For equal precedence, we always terminate for left-associativity so that
                // reduction happens before shifting
                Ordering::Equal => min.0 == Assoc::Left,
            },
        }
    }
}

impl Op {
    #[expect(dead_code)]
    fn is_unary(&self) -> bool {
        self.unary_prec().is_some()
    }

    fn is_binary(&self) -> bool {
        self.binary_prec().is_some()
    }

    fn is_compact_unary(&self) -> bool {
        matches!(self, Op::Bang)
    }

    fn is_compact_binary(&self) -> bool {
        matches!(self, Op::Dot | Op::DotHash)
    }

    fn binary_prec(&self) -> Option<Prec> {
        Some(match self {
            Op::BarBar => Prec(Assoc::Left, 600),
            Op::AmpAmp => Prec(Assoc::Left, 700),
            Op::Bar | Op::Caret => Prec(Assoc::Left, 800),
            Op::Amp => Prec(Assoc::Left, 900),
            Op::Lt | Op::LtEq | Op::Gt | Op::GtEq => Prec(Assoc::Left, 1000),
            Op::EqEq | Op::BangEq => Prec(Assoc::Left, 1100),
            Op::LtLt | Op::GtGt => Prec::SHIFT,
            Op::Plus | Op::Minus => Prec(Assoc::Left, 1200),
            Op::Percent | Op::Star | Op::Slash | Op::SlashSlash => Prec(Assoc::Left, 1300),
            Op::Dot | Op::DotHash => Prec(Assoc::Left, 2200),
            _ => return None,
        })
    }

    fn unary_prec(&self) -> Option<Prec> {
        Some(match self {
            Op::Minus | Op::Bang | Op::Tilde => Prec(Assoc::Right, 1900),
            _ => return None,
        })
    }
}

impl Parser<'_> {
    fn parse_range_tail(&mut self, scope: &mut Scope, mode: ExprMode) -> Result<Option<Expr>> {
        Ok(match self.peek()? {
            None | Some(token!(expr_tail_break!())) => None,
            _ => Some(self.parse_expr_prec(scope, mode, Some(Prec::RANGE))?),
        })
    }

    /// Parse the items between parentheses, up to but not including the `)`.
    ///
    /// These become the arguments of a C-style call, or a group, tuple, or
    /// `Pack` via [`Self::paren_expr`].
    fn parse_arg_pack(&mut self, scope: &mut Scope) -> Result<Vec<Arg>> {
        let mut args: Vec<Arg> = Vec::new();
        loop {
            let fail = self.fail;
            let separated = args.last().is_none_or(|arg| arg_delim(arg).is_some());
            match self.peek()? {
                Some(token!(TokenInfo::RightParen)) => break,
                // Items must be separated by commas; leave anything else for the
                // caller's check for `)`
                _ if !separated => break,
                None if fail => break,
                _ => args.push(self.parse_horiz_elem(scope)?),
            }
        }
        Ok(args)
    }

    /// Parse an element of a comma-delimited list: a `for` or `if` element, or
    /// else an item of the list's kind
    fn parse_horiz_elem<T: HorizElem>(&mut self, scope: &mut Scope) -> Result<T> {
        use self::Keyword;
        match self.peek()? {
            Some(token!(TokenInfo::Keyword(Keyword::For))) => {
                Ok(T::from_for(self.parse_horiz_for(scope)?))
            }
            Some(token!(TokenInfo::Keyword(Keyword::If))) => {
                Ok(T::from_if(self.parse_horiz_if(scope)?))
            }
            _ => T::parse_item(self, scope),
        }
    }

    /// The element after a `for` or `if` element's `=>`
    fn parse_horiz_body<T: HorizElem>(&mut self, scope: &mut Scope) -> Result<ExprBody<T>> {
        Ok(ExprBody {
            elems: vec![self.parse_horiz_elem(scope)?],
            vars: Vec::new(),
        })
    }

    /// Parse a horizontal pattern ending at one of `ends`, which is consumed
    /// and returned, switching to inline shell mode as lambda parameters do
    fn parse_horiz_pattern(
        &mut self,
        scope: &mut Scope,
        arrow: bool,
        ends: &[ExpectKind],
    ) -> Result<(Pattern, Token)> {
        self.with_inline_shell(|this| {
            if let Some(token!(TokenInfo::ArgSep)) = this.peek()? {
                this.advance();
            }
            let pattern = if arrow {
                this.parse_arrow_pattern(scope)?
            } else {
                this.parse_pattern(scope, false)?
            };
            let info = this.peek()?.map(|token| token.info);
            let span = this.expect(scope, ends)?;
            let end = Token {
                info: info.expect("an expected token"),
                span,
            };
            Ok((pattern, end))
        })
    }

    /// Parse a `for` element: `for PATTERN = EXPR => ELEM`, or `for PATTERN =>
    /// ELEM` over the strand's input
    fn parse_horiz_for<T: HorizElem>(&mut self, scope: &mut Scope) -> Result<For<ExprBody<T>>> {
        let for_span = self.advance();
        let (bind, end) =
            self.parse_horiz_pattern(scope, false, &[ExpectKind::Equal, ExpectKind::FatArrow])?;
        let (equal_span, expr, arrow_span) = if end.info == TokenInfo::Equal {
            let expr = self.parse_expr(scope, ExprMode::Full)?;
            let arrow_span = self.expect(scope, &[ExpectKind::FatArrow])?;
            (Some(end.span), Some(expr), arrow_span)
        } else {
            (None, None, end.span)
        };
        let body = self.parse_horiz_body(scope)?;
        Ok(For {
            bind,
            expr,
            body,
            iter: None,
            for_span,
            equal_span,
            arrow_span: Some(arrow_span),
        })
    }

    /// Parse an `if` element: `if COND => ELEM`, where `COND` may be a `let` or
    /// `bind` test. It takes no `else`, which a nested element would make
    /// ambiguous.
    fn parse_horiz_if<T: HorizElem>(&mut self, scope: &mut Scope) -> Result<If<ExprBody<T>>> {
        use self::Keyword;
        let if_span = self.advance();
        let (expr, bind, arrow_span) = match self.peek()? {
            Some(token!(TokenInfo::Keyword(Keyword::Let))) => {
                let keyword_span = self.advance();
                let (pattern, equal) =
                    self.parse_horiz_pattern(scope, false, &[ExpectKind::Equal])?;
                let equal_span = equal.span;
                let expr = self.parse_expr(scope, ExprMode::Full)?;
                let arrow_span = self.expect(scope, &[ExpectKind::FatArrow])?;
                let bind = CondPattern {
                    keyword_span,
                    kind: CondPatternKind::Let { equal_span },
                    pattern,
                };
                (expr, Some(bind), arrow_span)
            }
            Some(token!(TokenInfo::Keyword(Keyword::Bind))) => {
                let keyword_span = self.advance();
                let expr = self.parse_expr(scope, ExprMode::Full)?;
                let pattern_arrow = self.expect(scope, &[ExpectKind::FatArrow])?;
                let (pattern, arrow) =
                    self.parse_horiz_pattern(scope, true, &[ExpectKind::FatArrow])?;
                let arrow_span = arrow.span;
                let bind = CondPattern {
                    keyword_span,
                    kind: CondPatternKind::Bind {
                        arrow_span: Some(pattern_arrow),
                        do_span: None,
                    },
                    pattern,
                };
                (expr, Some(bind), arrow_span)
            }
            _ => {
                let expr = self.parse_expr(scope, ExprMode::Full)?;
                let arrow_span = self.expect(scope, &[ExpectKind::FatArrow])?;
                (expr, None, arrow_span)
            }
        };
        let body = self.parse_horiz_body(scope)?;
        if let Some(token @ token!(TokenInfo::Keyword(Keyword::Else))) = self.peek()? {
            return Err(self.syntax_error(
                scope,
                Some(token),
                "an `if` element takes no `else`; for a conditional value, use `&&` and `||`",
            ));
        }
        Ok(If {
            tbranch: IfBranch {
                span: if_span,
                expr,
                bind,
                arrow_span: Some(arrow_span),
                body,
            },
            elif_branches: Vec::new(),
            else_branch: None,
        })
    }

    /// Parse an item between parentheses
    fn parse_paren_item(&mut self, scope: &mut Scope) -> Result<Arg> {
        use self::Key;
        use TokenInfo::*;

        Ok(match self.peek()? {
            Some(token!(DittoKey, span)) => {
                self.advance();
                Arg::Key(Self::ditto_key(span, self.consume_comma()?))
            }
            Some(token!(Key, span)) => {
                self.advance();
                Arg::Key(Key {
                    key_span: span,
                    colon_span: span.after_right_char(),
                    expr: self.parse_expr(scope, ExprMode::Full)?,
                    delim_span: self.consume_comma()?,
                })
            }
            Some(token!(Ellipsis, span)) => {
                self.advance();
                let expr = self.parse_expr(scope, ExprMode::Full)?;
                let delim_span = self.consume_comma()?;
                Arg::Expand(Self::expansion(expr, span, delim_span))
            }
            _ => Arg::Pos(Single {
                expr: self.parse_expr(scope, ExprMode::Full)?,
                delim_span: self.consume_comma()?,
            }),
        })
    }

    /// Classify parenthesized items.
    ///
    /// A lone positional item without a trailing comma is a group. Otherwise,
    /// any static key, including one under a `for` or `if` element, makes a
    /// `Pack`, and anything else is a tuple.
    fn paren_kind(args: &[Arg]) -> ParenKind {
        if let [
            Arg::Pos(Single {
                delim_span: None, ..
            }),
        ] = args
        {
            ParenKind::Group
        } else if Self::args_have_key(args) {
            ParenKind::Pack
        } else {
            ParenKind::Tuple
        }
    }

    /// Whether a cast's `@` (checked) or `!@` (unchecked) is next
    fn peek_cast(&mut self) -> Result<Option<bool>> {
        Ok(match self.peek()? {
            Some(token!(TokenInfo::At)) => Some(true),
            Some(token!(TokenInfo::BangAt)) => Some(false),
            _ => None,
        })
    }

    /// Parse the rest of a cast, from its `@` or `!@` after a group's expression
    /// through the closing `)`.
    fn parse_cast(
        &mut self,
        scope: &mut Scope,
        mut args: Vec<Arg>,
        left: Span,
        checked: bool,
    ) -> Result<Expr> {
        let Some(Arg::Pos(Single { expr, .. })) = args.pop() else {
            unreachable!("a group's lone item")
        };
        let at_span = self.advance();
        let (annot, _) = self.parse_annot_type(scope, at_span, false)?;
        let right = self.expect_matching(scope, ExpectKind::RightParen, left);
        Ok(Expr::Cast {
            expr: Box::new(expr),
            annot,
            checked,
            paren_span: left | right,
        })
    }

    /// Interpret parenthesized items as an expression (see [`Self::paren_kind`]).
    fn paren_expr(mut args: Vec<Arg>, paren_span: Span) -> Expr {
        match Self::paren_kind(&args) {
            ParenKind::Group => {
                let Some(Arg::Pos(Single { expr, .. })) = args.pop() else {
                    unreachable!()
                };
                return Expr::Group {
                    expr: Box::new(expr),
                    delim: Some(GroupDelim::Paren(paren_span)),
                };
            }
            ParenKind::Pack => return Expr::Pack { paren_span, args },
            ParenKind::Tuple => (),
        }
        let elems = args.into_iter().map(Self::arg_to_array_elem).collect();
        Expr::Tuple { paren_span, elems }
    }

    pub(super) fn parse_expr_primary(&mut self, scope: &mut Scope, mode: ExprMode) -> Result<Expr> {
        use self::{Ident, Keyword};
        use TokenInfo::*;

        match self.next()? {
            Some(token!(Dollar, span)) if matches!(mode, ExprMode::Shell) => Ok(
                Self::dollar_group(self.parse_expr(scope, ExprMode::Compact)?, span),
            ),
            Some(token!(Sym, span)) => Ok(Expr::Sym(span)),
            Some(token!(DQuote, span)) => self.parse_quoted_string(scope, span, StrKind::Str),
            Some(token!(BQuote, open)) => self.parse_quoted_string(scope, open, StrKind::Bin),
            Some(token!(TQuote, open)) => self.parse_quoted_string(scope, open, StrKind::Fmt),
            Some(token!(RawQuote, start)) => {
                let content = self.expect(scope, &[ExpectKind::Literal])?;
                let end = self.expect_matching(scope, ExpectKind::RawQuote, start);
                Ok(Expr::Group {
                    expr: Box::new(Expr::Literal(content)),
                    delim: Some(GroupDelim::RawQuotes(start, end)),
                })
            }
            Some(token!(LeftParen, left)) => self.with_mode(lex::Mode::FullExpr, |this| {
                let args = this.parse_arg_pack(scope)?;
                if Self::paren_kind(&args) == ParenKind::Group
                    && let Some(checked) = this.peek_cast()?
                {
                    return this.parse_cast(scope, args, left, checked);
                }
                let right = this.expect_matching(scope, ExpectKind::RightParen, left);
                Ok(Self::paren_expr(args, left | right))
            }),
            Some(token!(LeftBracket, left)) => self.parse_array_literal(scope, left, None),
            Some(token!(LeftBrace, left)) => self.parse_dict_literal(scope, left),
            Some(token!(Keyword(Keyword::Do), span)) => Ok(self.parse_lambda(scope, span)?),
            Some(token!(DotDot, span)) if !matches!(mode, ExprMode::Shell) => Ok(Expr::Range {
                exprs: Box::new([None, self.parse_range_tail(scope, mode)?]),
                op_span: span,
            }),
            Some(token!(Key, span)) => {
                self.push_colon(span.after_right_char());
                if matches!(mode, ExprMode::Full | ExprMode::Compact) {
                    Ok(Expr::Ident(Ident::new(span)))
                } else {
                    Ok(Expr::Literal(span))
                }
            }
            token => match if matches!(mode, ExprMode::Shell) {
                decay_shell!(token)
            } else {
                decay_ident!(token)
            } {
                Some(token!(Literal, span)) if matches!(mode, ExprMode::Shell) => {
                    Ok(Expr::Literal(span))
                }
                Some(token!(Escape(c), span)) => Ok(Expr::Escape(c, span)),
                Some(token!(Ident, span)) => Ok(Expr::Ident(Ident::new(span))),
                Some(token!(Int(v), span)) => {
                    if matches!(mode, ExprMode::Shell) {
                        Ok(Expr::VerbatimInt(v, span))
                    } else {
                        Ok(Expr::Int(v, span))
                    }
                }
                Some(token!(F64, span)) => {
                    let str = self.file.str(span);
                    let str = if str.contains('_') {
                        Cow::Owned(str.replace('_', ""))
                    } else {
                        Cow::Borrowed(str)
                    };
                    let value = match str.parse::<f64>() {
                        Ok(value) => value,
                        Err(_) => {
                            self.fail = true;
                            self.diags.push(BadFloat(span));
                            0.0
                        }
                    };
                    if matches!(mode, ExprMode::Shell) {
                        Ok(Expr::VerbatimF64(value, span))
                    } else {
                        Ok(Expr::F64(value, span))
                    }
                }
                Some(token!(Bool(v), span)) => Ok(Expr::Bool(v, span)),
                Some(token!(Keyword(Keyword::Nil), span)) => Ok(Expr::Nil(span)),
                rest => {
                    let end = rest.is_none();
                    let message = match &rest {
                        None => "expected expression",
                        Some(token!(Dollar)) => "`$` invalid when already in expression context",
                        _ => "invalid expression",
                    };
                    let err = self.syntax_error(scope, rest, message);
                    // Opportunistically recover
                    if end { Ok(Expr::Error) } else { Err(err) }
                }
            },
        }
    }

    pub(super) fn parse_expr_const(
        &mut self,
        scope: &mut Scope,
        mode: ExprMode,
    ) -> Result<(Expr, Const)> {
        let expr = self.parse_expr_primary(scope, mode)?;
        if let Some(c) = expr.fold(self.file) {
            Ok((expr, c))
        } else {
            self.fail = true;
            self.diags.push(NonConstExpr(expr.span()));
            Ok((expr, Const::Error))
        }
    }

    fn parse_dict_literal(&mut self, scope: &mut Scope<'_>, left: Span) -> Result<Expr> {
        self.with_mode(lex::Mode::FullExpr, |this| {
            let mut elems = Vec::new();
            let right = loop {
                match this.peek()? {
                    Some(token!(TokenInfo::RightBrace)) => break this.advance(),
                    None => break this.expect_matching(scope, ExpectKind::RightBrace, left),
                    _ => elems.push(this.parse_horiz_elem(scope)?),
                }
            };
            Ok(Expr::Dict {
                elems,
                brace_span: Some(left | right),
            })
        })
    }

    /// Parse an item between braces
    fn parse_dict_item(&mut self, scope: &mut Scope) -> Result<DictElem> {
        use self::{Ident, Key, Keyword};
        use TokenInfo::*;

        let (key, colon_span) = match self.peek()? {
            Some(token!(Key, key_span)) => {
                self.advance();
                return Ok(DictElem::Key(Key {
                    key_span,
                    colon_span: key_span.after_right_char(),
                    expr: self.parse_expr(scope, ExprMode::Full)?,
                    delim_span: self.consume_comma()?,
                }));
            }
            Some(token!(Dollar)) => {
                let dollar_span = self.advance();
                if let Some(token!(Key)) = self.peek()? {
                    let span = self.advance();
                    (
                        Self::dollar_group(Expr::Ident(Ident::new(span)), dollar_span),
                        span.after_right_char(),
                    )
                } else {
                    let key = self.parse_expr(scope, ExprMode::Full)?;
                    let span = self.expect(scope, &[ExpectKind::Colon])?;
                    (key, span)
                }
            }
            Some(token!(Ellipsis)) => {
                let ellipsis_span = self.advance();
                let expr = self.parse_expr(scope, ExprMode::Full)?;
                let comma_span = self.consume_comma()?;
                return Ok(DictElem::Expand(Self::expansion(
                    expr,
                    ellipsis_span,
                    comma_span,
                )));
            }
            Some(token!(DittoKey)) => {
                let key_span = self.advance();
                let delim_span = self.consume_comma()?;
                return Ok(DictElem::Key(Self::ditto_key(key_span, delim_span)));
            }
            _ => {
                let key = self.parse_expr(scope, ExprMode::Full)?;
                match self.peek()? {
                    Some(token!(Comma)) => {
                        return Ok(DictElem::Single(Single {
                            expr: key,
                            delim_span: Some(self.advance()),
                        }));
                    }
                    // The caller checks what ends the item: the closing brace,
                    // or the `else` an `if` element can't take
                    None | Some(token!(RightBrace | Keyword(Keyword::Else))) => {
                        return Ok(DictElem::Single(Single {
                            expr: key,
                            delim_span: None,
                        }));
                    }
                    Some(token!(Colon)) => (key, self.advance()),
                    _ => {
                        let token = self.next().unwrap();
                        return Err(self.syntax_error(
                            scope,
                            token,
                            "invalid expression in dict literal",
                        ));
                    }
                }
            }
        };
        let value = self.parse_expr(scope, ExprMode::Full)?;
        let comma_span = self.consume_comma()?;
        Ok(DictElem::Pair(Pair {
            key,
            value,
            colon_span: Some(colon_span),
            delim_span: comma_span,
        }))
    }

    fn parse_array_literal(
        &mut self,
        scope: &mut Scope<'_>,
        left: Span,
        init: Option<(Expr, Span)>,
    ) -> Result<Expr> {
        let expr = self.with_mode(lex::Mode::FullExpr, |this| {
            let mut elems = Vec::new();
            if let Some((expr, span)) = init {
                elems.push(ArrayElem::Single(Single {
                    expr,
                    delim_span: Some(span),
                }))
            }
            let right = loop {
                match this.peek()? {
                    Some(token!(TokenInfo::RightBracket)) => break this.advance(),
                    None => break this.expect_matching(scope, ExpectKind::RightBracket, left),
                    _ => elems.push(this.parse_horiz_elem(scope)?),
                }
            };
            Ok(Expr::Array {
                elems,
                bracket_span: Some(left | right),
            })
        })?;
        Ok(expr)
    }

    /// Parse an item between brackets
    fn parse_array_item(&mut self, scope: &mut Scope) -> Result<ArrayElem> {
        if let Some(token!(TokenInfo::Ellipsis)) = self.peek()? {
            let ellipsis_span = self.advance();
            let expr = self.parse_expr(scope, ExprMode::Full)?;
            let comma = self.consume_comma()?;
            return Ok(ArrayElem::Expand(Self::expansion(
                expr,
                ellipsis_span,
                comma,
            )));
        }
        let expr = self.parse_expr(scope, ExprMode::Full)?;
        let comma = self.consume_comma()?;
        Ok(ArrayElem::Single(Single {
            expr,
            delim_span: comma,
        }))
    }

    /// Parse an expression using Pratt's precedence climbing algorithm.
    ///
    /// # Algorithm Overview
    ///
    /// Pratt parsing (top-down operator precedence) works by:
    /// 1. Parse a "nud" (null denotation) - the left-hand side ("primary" or "atomic" expression)
    /// 2. While the next token is a binary operator with precedence >= min_prec:
    ///    a. Consume the operator
    ///    b. Parse the right-hand side with higher precedence
    ///    c. Combine into a new left-hand side
    ///
    /// # Precedence Handling
    ///
    /// The `min_prec` parameter controls operator associativity:
    /// - For left-associative ops, the recursive call effectively uses prec + 1
    ///   (handled in `Prec::terminate` tie-breaking logic)
    /// - For right-associative ops, the recursive call uses prec
    /// - This ensures `a - b - c` parses as `(a - b) - c` not `a - (b - c)`
    ///
    /// # Ambiguity Resolution
    ///
    /// This parser handles several syntactic ambiguities:
    ///
    /// ## Indexing vs Array Literals
    /// `a[b]` could be indexing or a call with an array literal.
    /// We initially assume indexing, but if we see `[]`, `...`, or `,` inside,
    /// we reinterpret as a call with an array literal argument.
    ///
    /// ## Juxtaposition Calls
    /// `f x` is a function call without parentheses.
    /// We detect this when we see an identifier followed by an expression-starting
    /// token that isn't an operator or delimiter.
    ///
    /// ## Dollar Calls
    /// `f $ x` is a low-precedence call, useful for chaining.
    /// It has lower precedence than most operators but higher than comma.
    fn parse_expr_prec(
        &mut self,
        scope: &mut Scope,
        mode: ExprMode,
        min_prec: Option<Prec>,
    ) -> Result<Expr> {
        // Parse the null denotation (atomic expression or unary prefix operator)
        let mut lhs = {
            match self.peek()? {
                Some(token!(TokenInfo::Op(op))) => {
                    let token = self.consume();
                    let prec = match op.unary_prec() {
                        None => {
                            return Err(self.syntax_error(
                                scope,
                                Some(token),
                                "invalid unary operator",
                            ));
                        }
                        Some(prec) => prec,
                    };
                    if mode == ExprMode::Compact && !op.is_compact_unary() {
                        self.diags.push(InvalidCompactOp(op, token.span));
                        return Err(Error);
                    }
                    Expr::Unary {
                        op,
                        expr: Box::new(self.parse_expr_prec(scope, mode, Some(prec))?),
                        op_span: token.span,
                    }
                }
                Some(token!(TokenInfo::Key)) => {
                    let span = self.advance();
                    self.push_colon(span.after_right_char());
                    Expr::Ident(Ident::new(span))
                }
                _ => self.parse_expr_primary(scope, mode)?,
            }
        };

        // A C-style call separated from its callee by whitespace, which looks like
        // a call with a tuple or `Pack`. Reported once the call is final, since
        // juxtaposing another argument turns it into exactly that.
        let mut spaced_call: Option<MisleadingCall> = None;

        loop {
            // An `else` ends an `if` element's item, for the error that it takes
            // no `else`, rather than decaying to an identifier argument
            if let Some(token!(TokenInfo::Keyword(Keyword::Else))) = self.peek()? {
                break;
            }
            match decay_ident!(self.peek()?) {
                Some(token!(TokenInfo::DotDot, span)) if !matches!(mode, ExprMode::Shell) => {
                    let prec = Prec::RANGE;
                    if prec.terminate(&min_prec) {
                        break;
                    }
                    self.advance();
                    lhs = Expr::Range {
                        exprs: Box::new([Some(lhs), self.parse_range_tail(scope, mode)?]),
                        op_span: span,
                    };
                }
                Some(token!(TokenInfo::Op(op), span)) if op.is_binary() => {
                    if mode == ExprMode::Compact && !op.is_compact_binary() {
                        break;
                    }
                    let prec = op.binary_prec().unwrap();
                    if prec.terminate(&min_prec) {
                        break;
                    }
                    self.advance();
                    if matches!(op, Op::Dot | Op::DotHash) {
                        let field = match decay_field!(self.next()?) {
                            Some(token!(TokenInfo::Ident, field)) => {
                                if op == Op::Dot {
                                    GetVariant::Normal(field)
                                } else {
                                    GetVariant::Private {
                                        span: field,
                                        res: None,
                                    }
                                }
                            }
                            Some(token!(TokenInfo::LeftParen, left)) if op == Op::Dot => {
                                let span = self.expect(scope, &[ExpectKind::Ident])?;
                                let right = self.expect(scope, &[ExpectKind::RightParen])?;
                                let method = self.special_method(scope, span)?;
                                GetVariant::SpecialMethod {
                                    method,
                                    span,
                                    paren_span: left | right,
                                }
                            }
                            Some(token!(TokenInfo::Key, span)) => {
                                self.push_colon(span.after_right_char());
                                if op == Op::Dot {
                                    GetVariant::Normal(span)
                                } else {
                                    GetVariant::Private { span, res: None }
                                }
                            }
                            other => return Err(self.syntax_error(scope, other, "invalid field")),
                        };
                        lhs = Expr::Get {
                            object: Box::new(lhs),
                            field,
                            dot_span: span,
                        };
                    } else {
                        let rhs = self.parse_expr_prec(scope, mode, Some(prec))?;
                        lhs = match lhs {
                            // A chain of the same short circuit is one node
                            Expr::Logical {
                                op: lop,
                                ref mut exprs,
                                ref mut op_spans,
                            } if lop == op => {
                                exprs.push(rhs);
                                op_spans.push(span);
                                lhs
                            }
                            lhs if matches!(op, Op::AmpAmp | Op::BarBar) => Expr::Logical {
                                op,
                                exprs: vec![lhs, rhs],
                                op_spans: vec![span],
                            },
                            lhs => Expr::Binary {
                                op,
                                exprs: [lhs, rhs].into(),
                                op_span: span,
                            },
                        }
                    }
                }
                // Low-precedence call
                Some(token!(TokenInfo::Dollar)) if mode == ExprMode::Full => {
                    let prec = Prec::DOLLAR_CALL;
                    if prec.terminate(&min_prec) {
                        break;
                    }
                    let span = self.advance();
                    let rhs = self.parse_expr_prec(scope, mode, Some(prec))?;
                    if span.end == rhs.span().start {
                        self.diags.push(MisleadingDollar(span))
                    }
                    match lhs {
                        Expr::Call {
                            ref mut args,
                            delim: ref mut delim @ None,
                            ..
                        } => {
                            args.push(Self::positional_arg(rhs));
                            *delim = Some(GroupDelim::Dollar(span));
                        }
                        _ => {
                            lhs = Expr::Call {
                                arg0: Box::new(lhs),
                                args: vec![Self::positional_arg(rhs)],
                                delim: Some(GroupDelim::Dollar(span)),
                            }
                        }
                    }
                }
                // Indexing
                //
                // **Syntactic ambiguity**: `a[b]` could be either:
                // 1. Indexing: accessing element at index b of array a
                // 2. Function call: calling function a with array literal [b] as argument
                //
                // **Resolution strategy**: We initially assume indexing (the more common case). If
                // we encounter certain markers inside the brackets, we reinterpret as a function
                // call with an array literal argument:
                // - `[]` (empty): definitely an empty array literal, not valid indexing
                // - `...` (ellipsis): indicates array spread, not valid indexing syntax
                // - `,` (comma): multi-element array literal
                //
                // This can lead to confusing error messages if the user actually meant indexing but
                // made a syntax error. We emit AmbigIndex warning when there's whitespace between
                // the base expression and `[` to help catch cases like `f [x]` where the user
                // probably meant `f([x])` not `f[x]`.
                Some(token!(TokenInfo::LeftBracket)) => {
                    if Prec::INDEX.terminate(&min_prec) {
                        break;
                    }
                    let left = self.advance();
                    match self.peek()? {
                        Some(token!(TokenInfo::RightBracket)) => {
                            // Empty brackets `[]` - reinterpret as call with empty array literal
                            // Example: `f[]` should be `f([])` not indexing into f
                            let rhs = Expr::Array {
                                elems: vec![],
                                bracket_span: Some(left | self.advance()),
                            };
                            match lhs {
                                Expr::Call {
                                    ref mut args,
                                    delim: None,
                                    ..
                                } => args.push(Self::positional_arg(rhs)),
                                _ => {
                                    lhs = Expr::Call {
                                        arg0: Box::new(lhs),
                                        args: vec![Self::positional_arg(rhs)],
                                        delim: None,
                                    };
                                }
                            }
                            continue;
                        }
                        Some(
                            token!(
                                TokenInfo::Ellipsis
                                    | TokenInfo::Keyword(Keyword::For | Keyword::If)
                            ),
                        ) => {
                            // Ellipsis `...` inside brackets - array literal with spread
                            // Example: `f[...a]` should be `f([...a])` not indexing.
                            // Likewise a `for` or `if` element: `f[for x = xs => x]`
                            let rhs = self.parse_array_literal(scope, left, None)?;
                            match lhs {
                                Expr::Call {
                                    ref mut args,
                                    delim: None,
                                    ..
                                } => args.push(Self::positional_arg(rhs)),
                                _ => {
                                    lhs = Expr::Call {
                                        arg0: Box::new(lhs),
                                        args: vec![Self::positional_arg(rhs)],
                                        delim: None,
                                    }
                                }
                            }
                            continue;
                        }
                        _ => (),
                    }
                    let index = self.with_mode(Mode::FullExpr, |this| {
                        this.parse_expr(scope, ExprMode::Full)
                    })?;
                    match self.next()? {
                        Some(token!(TokenInfo::RightBracket, right)) => {
                            if lhs.span().end != left.start {
                                self.diags.push(AmbigIndex(lhs.span(), left | right));
                            }
                            lhs = Expr::Index {
                                bracket_span: left | right,
                                exprs: [lhs, index].into(),
                            }
                        }
                        Some(token!(TokenInfo::Comma, span)) => {
                            // Comma inside brackets - multi-element array literal
                            // Example: `f[a, b]` should be `f([a, b])`
                            let rhs = self.parse_array_literal(scope, left, Some((index, span)))?;
                            match lhs {
                                Expr::Call {
                                    ref mut args,
                                    delim: None,
                                    ..
                                } => args.push(Self::positional_arg(rhs)),
                                _ => {
                                    lhs = Expr::Call {
                                        arg0: Box::new(lhs),
                                        args: vec![Self::positional_arg(rhs)],
                                        delim: None,
                                    }
                                }
                            }
                        }
                        None => {
                            // Actually an index expression
                            lhs = Expr::Index {
                                bracket_span: left
                                    | self.expect_matching(scope, ExpectKind::RightBracket, left),
                                exprs: [lhs, index].into(),
                            };
                            break;
                        }
                        token => {
                            return Err(self.syntax_error(
                                scope,
                                token,
                                "invalid expression in index or Array literal",
                            ));
                        }
                    }
                }
                // Juxtaposition (call expression)
                Some(
                    token!(
                        info @ (expr_start_not_left_bracket!()
                        | TokenInfo::Keyword(Keyword::Do)
                        | TokenInfo::Ident
                        | TokenInfo::Op(Op::Bang)
                        | TokenInfo::DittoKey
                        | TokenInfo::Ellipsis
                        | TokenInfo::Key)
                    ),
                ) => {
                    // Only C-style calls are possible in compact mode
                    if mode == ExprMode::Compact && !matches!(info, TokenInfo::LeftParen) {
                        break;
                    }
                    let info = info.clone();
                    let mut juxta_warn = false;
                    // Immediately invoking the result of a C-style call is disallowed.
                    // Convert it into a regular call whose first argument is a group,
                    // tuple, or `Pack`, unless in compact mode, where we give up parsing
                    if let Expr::Call {
                        arg0,
                        args,
                        delim: Some(GroupDelim::Paren(paren_span)),
                    } = lhs
                    {
                        if mode != ExprMode::Compact {
                            let arg0_span = arg0.span();
                            if arg0_span.end == paren_span.start {
                                juxta_warn = true;
                            }
                            if spaced_call.is_some_and(|call| call.paren_span == paren_span) {
                                spaced_call = None;
                            }
                            lhs = Expr::Call {
                                arg0,
                                args: vec![Self::positional_arg(Self::paren_expr(
                                    args, paren_span,
                                ))],
                                delim: None,
                            };
                        } else {
                            lhs = Expr::Call {
                                arg0,
                                args,
                                delim: Some(GroupDelim::Paren(paren_span)),
                            };
                            break;
                        }
                    };
                    let prec = Prec::CALL;
                    if prec.terminate(&min_prec) {
                        break;
                    }
                    let arg = match info {
                        TokenInfo::Key => {
                            let token = self.consume();
                            let expr = self.parse_expr_prec(scope, mode, Some(prec))?;
                            Arg::Key(Key {
                                key_span: token.span,
                                colon_span: token.span.after_right_char(),
                                expr,
                                delim_span: None,
                            })
                        }
                        TokenInfo::DittoKey => {
                            let token = self.consume();
                            Arg::Key(Self::ditto_key(token.span, None))
                        }
                        TokenInfo::Ellipsis => {
                            let token = self.consume();
                            let expr = self.parse_expr_prec(scope, mode, Some(prec))?;
                            Arg::Expand(Self::expansion(expr, token.span, None))
                        }
                        TokenInfo::LeftParen if !matches!(lhs, Expr::Call { .. }) => {
                            let left = self.advance();
                            let callee_span = lhs.span();
                            let (args, paren_span) = self.with_mode(Mode::FullExpr, |this| {
                                let args = this.parse_arg_pack(scope)?;
                                if this.peek_cast()?.is_some() {
                                    let token = this.peek()?;
                                    return Err(this.syntax_error(
                                        scope,
                                        token,
                                        "a cast in a call's arguments needs its own \
                                         parentheses, as in `f((x @ T))`",
                                    ));
                                }
                                let right =
                                    this.expect_matching(scope, ExpectKind::RightParen, left);
                                Ok((args, left | right))
                            })?;
                            if callee_span.end != left.start {
                                let kind = Self::paren_kind(&args);
                                if kind != ParenKind::Group {
                                    spaced_call = Some(MisleadingCall {
                                        callee_span,
                                        paren_span,
                                        args: kind == ParenKind::Pack,
                                    });
                                }
                            }
                            lhs = Expr::Call {
                                arg0: Box::new(lhs),
                                args,
                                delim: Some(GroupDelim::Paren(paren_span)),
                            };
                            continue;
                        }
                        _ => Self::positional_arg(self.parse_expr_prec(scope, mode, Some(prec))?),
                    };
                    // Join argument to existing call create new one
                    match lhs {
                        Expr::Call {
                            ref mut args,
                            ref arg0,
                            ..
                        } => {
                            let prev_span = args.last().unwrap().span();
                            let arg_span = arg.span();
                            if juxta_warn {
                                self.diags.push(MisleadingArg {
                                    arg0_span: arg0.span(),
                                    arg_span,
                                    patch_span: prev_span,
                                })
                            } else if prev_span.end == arg_span.start {
                                self.diags.push(MisleadingArg {
                                    arg0_span: arg0.span(),
                                    arg_span,
                                    patch_span: arg_span,
                                })
                            }
                            args.push(arg);
                        }
                        _ => {
                            lhs = Expr::Call {
                                arg0: Box::new(lhs),
                                args: vec![arg],
                                delim: None,
                            }
                        }
                    }
                }
                None | Some(token!(expr_tail_break!())) => break,
                _ => {
                    let token = self.consume();
                    return Err(self.syntax_error(scope, Some(token), "invalid expression"));
                }
            }
        }

        if let Some(call) = spaced_call {
            self.diags.push(call);
        }
        Ok(lhs)
    }

    pub(super) fn parse_expr(&mut self, scope: &mut Scope, mode: ExprMode) -> Result<Expr> {
        match mode {
            ExprMode::Shell => self.parse_expr_primary(scope, mode),
            ExprMode::Compact | ExprMode::Full => self.parse_expr_prec(scope, mode, None),
        }
    }
}

/// An element of a comma-delimited list, which a `for` or `if` element can
/// stand in for
trait HorizElem: Sized {
    /// Parse an item other than a `for` or `if` element, along with its comma
    fn parse_item(parser: &mut Parser<'_>, scope: &mut Scope) -> Result<Self>;

    fn from_for(node: For<ExprBody<Self>>) -> Self;

    fn from_if(node: If<ExprBody<Self>>) -> Self;
}

impl HorizElem for ArrayElem {
    fn parse_item(parser: &mut Parser<'_>, scope: &mut Scope) -> Result<Self> {
        parser.parse_array_item(scope)
    }

    fn from_for(node: For<ExprBody<Self>>) -> Self {
        Self::For(node)
    }

    fn from_if(node: If<ExprBody<Self>>) -> Self {
        Self::If(node)
    }
}

impl HorizElem for DictElem {
    fn parse_item(parser: &mut Parser<'_>, scope: &mut Scope) -> Result<Self> {
        parser.parse_dict_item(scope)
    }

    fn from_for(node: For<ExprBody<Self>>) -> Self {
        Self::For(node)
    }

    fn from_if(node: If<ExprBody<Self>>) -> Self {
        Self::If(node)
    }
}

impl HorizElem for Arg {
    fn parse_item(parser: &mut Parser<'_>, scope: &mut Scope) -> Result<Self> {
        parser.parse_paren_item(scope)
    }

    fn from_for(node: For<ExprBody<Self>>) -> Self {
        Self::For(node)
    }

    fn from_if(node: If<ExprBody<Self>>) -> Self {
        Self::If(node)
    }
}

/// The comma after a parenthesized item, which for a `for` or `if` element is
/// the one after the item it ends in
fn arg_delim(arg: &Arg) -> Option<Span> {
    match arg {
        Arg::Pos(Single { delim_span, .. })
        | Arg::Key(Key { delim_span, .. })
        | Arg::Expand(Expand { delim_span, .. }) => *delim_span,
        Arg::For(node) => node.body.elems.last().and_then(arg_delim),
        Arg::If(node) => node.tbranch.body.elems.last().and_then(arg_delim),
        Arg::DynamicKey(_) | Arg::Match(_) => unreachable!("not parsed in parentheses"),
    }
}
