use std::{
    borrow::Cow,
    fmt::{Display, Write},
    mem,
};

use super::{Error, Parser, Result, Scope, diag::SyntaxDiag};
use crate::{
    lex::{Keyword, Lexer, Mode, Op, Token, TokenInfo},
    source::{Offset, Span},
};

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ExpectKind {
    ArgSep,
    Const,
    DecoratorOpen,
    Dedent,
    Dollar,
    DQuote,
    End,
    Escape,
    EscapeByte,
    Equal,
    Ident,
    Indent,
    Key,
    Keyword(Keyword),
    LeftParen,
    Literal,
    Op(Op),
    RightParen,
    StmtSep,
    LeftBracket,
    RightBracket,
    LeftBrace,
    RightBrace,
    Comma,
    Colon,
    DotDot,
    Ellipsis,
    DittoKey,
    Sym,
    RawQuote,
    BQuote,
    RBar,
    TQuote,
    TBar,
    Hash,
}

impl ExpectKind {
    fn compatible(&self, other: &Self) -> bool {
        self == other
    }
}

impl Display for ExpectKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use ExpectKind::*;

        match self {
            ArgSep => &"<whitespace>",
            Const => &"constant",
            DecoratorOpen => &"#[",
            Dedent => &"<unindent>",
            Dollar => &"$",
            DQuote => &"\"",
            End => &"<eof>",
            Escape => &"<char escape>",
            EscapeByte => &"<byte escape>",
            Equal => &"=",
            Ident => &"identifier",
            Indent => &"<indent>",
            Key => &"key",
            LeftParen => &"(",
            Keyword(k) => k as &dyn Display,
            Op(op) => op as &dyn Display,
            RightParen => &")",
            Literal => &"literal",
            StmtSep => &"<new statement>",
            LeftBracket => &"[",
            RightBracket => &"]",
            LeftBrace => &"{",
            RightBrace => &"}",
            Comma => &",",
            Colon => &":",
            DotDot => &"..",
            Ellipsis => &"...",
            DittoKey => &"<ditto key>",
            Sym => &"<symbol>",
            RawQuote => &"<raw quote>",
            BQuote => &"<binary quote>",
            RBar => &"r|",
            TQuote => &"<formatted quote>",
            TBar => &"t|",
            Hash => &"#",
        }
        .fmt(f)
    }
}

impl From<&TokenInfo> for ExpectKind {
    fn from(value: &TokenInfo) -> Self {
        use self::Keyword;
        use TokenInfo::*;

        match value {
            ArgSep => ExpectKind::ArgSep,
            Bool(_) | Int(_) | F64 | Keyword(Keyword::Nil) => ExpectKind::Const,
            DecoratorOpen => ExpectKind::DecoratorOpen,
            Dedent => ExpectKind::Dedent,
            Dollar => ExpectKind::Dollar,
            DQuote => ExpectKind::DQuote,
            Equal => ExpectKind::Equal,
            Escape(_) => ExpectKind::Escape,
            EscapeByte(_) => ExpectKind::EscapeByte,
            Ident => ExpectKind::Ident,
            Indent => ExpectKind::Indent,
            Key => ExpectKind::Key,
            Keyword(k) => ExpectKind::Keyword(*k),
            LeftParen => ExpectKind::LeftParen,
            Literal => ExpectKind::Literal,
            RightParen => ExpectKind::RightParen,
            StmtSep => ExpectKind::StmtSep,
            Op(o) => ExpectKind::Op(*o),
            LeftBracket => ExpectKind::LeftBracket,
            RightBracket => ExpectKind::RightBracket,
            LeftBrace => ExpectKind::LeftBrace,
            RightBrace => ExpectKind::RightBrace,
            Comma => ExpectKind::Comma,
            Colon => ExpectKind::Colon,
            DotDot => ExpectKind::DotDot,
            Ellipsis => ExpectKind::Ellipsis,
            DittoKey => ExpectKind::DittoKey,
            Sym => ExpectKind::Sym,
            RawQuote => ExpectKind::RawQuote,
            BQuote => ExpectKind::BQuote,
            RBar => ExpectKind::RBar,
            TQuote => ExpectKind::TQuote,
            TBar => ExpectKind::TBar,
            Hash => ExpectKind::Hash,
        }
    }
}

#[derive(Default)]
enum Inner<T> {
    #[default]
    Empty,
    Full(T),
    End,
}

pub(super) struct Peek<'a> {
    lexer: Lexer<'a>,
    count: usize,
    peek: Inner<<Self as Iterator>::Item>,
}

const PEEK_INTERNAL_ERROR_COUNT: usize = 10;

impl<'a> Peek<'a> {
    fn set_mode(&mut self, mode: Mode) -> Mode {
        self.lexer.set_mode(mode)
    }

    pub(super) fn set_error(&mut self) {
        self.lexer.set_error()
    }

    fn peek(&mut self) -> Result<Option<Token>> {
        Ok(loop {
            break match self.peek {
                Inner::Empty => match self.lexer.next() {
                    None => {
                        self.peek = Inner::End;
                        None
                    }
                    Some(next) => {
                        self.peek = Inner::Full(next);
                        continue;
                    }
                },
                Inner::Full(Ok(ref t)) => {
                    if self.count >= PEEK_INTERNAL_ERROR_COUNT {
                        panic!("internal parser error: failed to make progress")
                    }
                    self.count += 1;
                    Some(t.clone())
                }
                Inner::Full(Err(e)) => {
                    self.peek = Inner::Empty;
                    return Err(e.into());
                }
                Inner::End => None,
            };
        })
    }

    pub(super) fn push(&mut self, token: Token) {
        assert!(matches!(self.peek, Inner::End | Inner::Empty));
        self.peek = Inner::Full(Ok(token))
    }

    fn peek_with_mode(&mut self, mode: Mode) -> Result<Option<&mut Token>> {
        Ok(loop {
            break match self.peek {
                Inner::Empty => {
                    let prev = self.lexer.set_mode(mode);
                    let next = self.lexer.next();
                    self.lexer.set_mode(prev);
                    match next {
                        None => {
                            self.peek = Inner::End;
                            None
                        }
                        Some(next) => {
                            self.peek = Inner::Full(next);
                            continue;
                        }
                    }
                }
                Inner::Full(Ok(ref mut t)) => Some(t),
                Inner::Full(Err(e)) => {
                    self.peek = Inner::Empty;
                    return Err(e.into());
                }
                Inner::End => None,
            };
        })
    }

    pub(crate) fn span(&self) -> Span {
        self.lexer.span()
    }
}

impl<'a> From<Lexer<'a>> for Peek<'a> {
    fn from(value: Lexer<'a>) -> Self {
        Self {
            lexer: value,
            peek: Default::default(),
            count: 0,
        }
    }
}

impl<'a> Iterator for Peek<'a> {
    type Item = <Lexer<'a> as Iterator>::Item;

    fn next(&mut self) -> Option<Self::Item> {
        self.count = 0;
        match mem::take(&mut self.peek) {
            Inner::End => {
                self.peek = Inner::End;
                None
            }
            Inner::Empty => self.lexer.next(),
            Inner::Full(t) => Some(t),
        }
    }
}

impl Parser<'_> {
    pub(super) fn push_colon(&mut self, span: Span) {
        self.lex.push(Token {
            info: TokenInfo::Colon,
            span,
        })
    }

    pub(super) fn add_indent(&mut self, offset: Offset) {
        self.lex.lexer.add_indent(offset)
    }

    pub(super) fn consume_comma(&mut self) -> Result<Option<Span>> {
        Ok(if let Some(token!(TokenInfo::Comma)) = self.peek()? {
            Some(self.advance())
        } else {
            None
        })
    }

    pub(super) fn with_mode<R>(
        &mut self,
        mode: Mode,
        f: impl for<'b> FnOnce(&'b mut Self) -> Result<R>,
    ) -> Result<R> {
        let prev = self.lex.set_mode(mode);
        let res = f(self);
        self.lex.set_mode(prev);
        res
    }

    pub(super) fn peek(&mut self) -> Result<Option<Token>> {
        let res = self.lex.peek();
        if res.is_err() {
            self.fail = true;
        }
        res
    }

    #[expect(dead_code)]
    fn peek_with_mode(&mut self, mode: Mode) -> Result<Option<&mut Token>> {
        let res = self.lex.peek_with_mode(mode);
        if res.is_err() {
            self.fail = true;
        }
        res
    }

    pub(super) fn next(&mut self) -> Result<Option<Token>> {
        Ok(match self.lex.next() {
            None => None,
            Some(Ok(token)) => Some(token),
            Some(Err(e)) => {
                self.fail = true;
                return Err(e.into());
            }
        })
    }

    pub(super) fn consume(&mut self) -> Token {
        self.next()
            .expect("consume: unexpected error")
            .expect("consume: unexpected end")
    }

    pub(super) fn expect(&mut self, _scope: &mut Scope, expect: &[ExpectKind]) -> Result<Span> {
        let token = match self.lex.next() {
            None => {
                for e in expect.iter() {
                    if e.compatible(&ExpectKind::End) {
                        return Ok(self.lex.span());
                    }
                }
                let mut msg = "got <eof>, expected: ".to_owned();
                for (i, e) in expect.iter().enumerate() {
                    if i != 0 {
                        msg.push_str(", ");
                    }
                    write!(&mut msg, "{}", e).unwrap();
                }
                self.fail = true;
                self.diags.push(SyntaxDiag::new(self.lex.span(), msg));
                return Err(Error);
            }
            Some(res) => res?,
        };
        let got = ExpectKind::from(&token.info);
        for e in expect.iter() {
            if e.compatible(&got) {
                return Ok(token.span);
            }
        }
        self.fail = true;
        let mut msg = format!("got {}, expected: ", got);
        for (i, e) in expect.iter().enumerate() {
            if i != 0 {
                msg.push_str(", ");
            }
            write!(&mut msg, "{}", e).unwrap();
        }
        self.diags.push(SyntaxDiag::new(token.span, msg));
        Err(Error)
    }

    pub(super) fn expect_matching(
        &mut self,
        scope: &mut Scope,
        expect: ExpectKind,
        open: Span,
    ) -> Span {
        self.expect(scope, &[expect]).ok().unwrap_or(open)
    }

    pub(super) fn advance(&mut self) -> Span {
        self.lex
            .next()
            .expect("advance: end of tokens")
            .expect("advance: unhandled error")
            .span
    }

    pub(super) fn resync_eol(&mut self) -> Result<()> {
        use TokenInfo::*;
        // Destined to fail
        self.lex.set_error();
        // Try to resynchronize with token stream at EOL
        while !matches!(self.peek()?, None | Some(token!(StmtSep | Dedent))) {
            self.advance();
        }
        Ok(())
    }

    pub(super) fn syntax_error(
        &mut self,
        _scope: &mut Scope,
        token: Option<Token>,
        message: impl Into<Cow<'static, str>>,
    ) -> Error {
        self.fail = true;
        self.diags.push(SyntaxDiag::new(
            token.map(|t| t.span).unwrap_or_else(|| self.lex.span()),
            message,
        ));
        Error
    }
}
