use std::{
    fmt::{self, Debug, Display, Formatter, Write},
    mem,
};

use phf::phf_map;

use super::{
    Compiler,
    diag::Severity,
    source::{Diagnose, Diags, File, Offset, Span},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Keyword {
    Bind,
    Break,
    Catch,
    Class,
    Continue,
    Def,
    Do,
    Else,
    Finally,
    For,
    If,
    Import,
    Let,
    Nil,
    Pub,
    Return,
    Throw,
    Try,
    While,
}

impl Display for Keyword {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        use Keyword::*;

        write!(
            f,
            "{}",
            match self {
                Bind => "bind",
                Break => "break",
                Catch => "catch",
                Class => "class",
                Continue => "continue",
                Def => "def",
                Do => "do",
                Else => "else",
                Finally => "finally",
                For => "for",
                If => "if",
                Import => "import",
                Let => "let",
                Nil => "nil",
                Pub => "pub",
                Return => "return",
                Throw => "throw",
                Try => "try",
                While => "while",
            }
        )
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, PartialOrd, Ord)]
pub(crate) enum Op {
    Amp,
    AmpAmp,
    Bang,
    BangEq,
    Bar,
    BarBar,
    Caret,
    EqEq,
    Gt,
    GtEq,
    Lt,
    LtEq,
    Minus,
    Percent,
    Dot,
    DotHash,
    Plus,
    Slash,
    SlashSlash,
    Star,
    Tilde,
}

impl Display for Op {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        use Op::*;

        write!(
            f,
            "{}",
            match self {
                Amp => "&",
                AmpAmp => "&&",
                Bang => "!",
                BangEq => "!=",
                Bar => "|",
                BarBar => "||",
                Caret => "^",
                EqEq => "==",
                Gt => ">",
                GtEq => ">=",
                Lt => "<",
                LtEq => "<=",
                Minus => "-",
                Percent => "%",
                Dot => ".",
                DotHash => ".#",
                Plus => "+",
                Slash => "/",
                SlashSlash => "//",
                Star => "*",
                Tilde => "~",
            }
        )
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub(crate) enum ErrorDiagKind {
    // Character other than \n after \r
    BadCr,
    // Tab
    Tab,
    // Dedent to previously unused level
    BadIndent,
    // Expected valid identifier characters in an identifier
    BadIdent,
    // Unexpected escape
    BadEscape,
    // Integer overflow
    Overflow,
}

pub(crate) struct ErrorDiag(ErrorDiagKind, Span);

impl Diagnose for ErrorDiag {
    fn span(&self) -> Span {
        self.1
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        use ErrorDiagKind::*;

        write!(
            w,
            "{}",
            match self.0 {
                BadCr => "unexpected character after carriage return",
                Tab => "tabs are not accepted as indentation",
                BadIndent => "inconsistent indentation",
                BadIdent => "unexpected character in identifier",
                Overflow => "overflow in constant",
                BadEscape => "unexpected escape sequence",
            }
        )
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum WarnDiagKind {
    // Overflow into literal
    OverflowLit,
}

pub(crate) struct WarnDiag(WarnDiagKind, Span);

impl Diagnose for WarnDiag {
    fn span(&self) -> Span {
        self.1
    }

    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        use WarnDiagKind::*;

        write!(
            w,
            "{}",
            match self.0 {
                OverflowLit => "overflow in constant; treated as literal",
            }
        )
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Mode {
    Shell,
    FullExpr,
    String,
    Heredoc,
    RawHeredoc,
}

#[derive(Debug)]
enum RawToken {
    DecoratorOpen,
    Dollar,
    DQuote,
    Escape(char),
    EscapeByte(u8),
    Equal,
    I64(i64),
    F64,
    Ident,
    Indent,
    Key,
    LeftParen,
    Literal,
    Op(Op),
    NewlineIndent,
    RightParen,
    Space,
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
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum RawState {
    Amp,
    AmpAmp,
    Bang,
    BangEqual,
    Bar,
    BarBar,
    Caret,
    Colon,
    Comma,
    Comment,
    Cr,
    DittoKey,
    Dollar,
    DQuote,
    Empty,
    End,
    Equal,
    EqualEqual,
    Error,
    Escape,
    Exponent,
    ExponentStart,
    Float,
    Gt,
    GtEq,
    Ident,
    Indent,
    Init,
    Key,
    LeftBrace,
    LeftBracket,
    LeftParen,
    Literal,
    Lt,
    LtEq,
    Minus,
    Percent,
    Dot,
    DotHash,
    DotDot,
    DotDotDot,
    Plus,
    RightBrace,
    RightBracket,
    RightParen,
    Signed,
    Slash,
    SlashSlash,
    Space,
    Star,
    EmitDotDot,
    Tilde,
    SignedDot,
    Unsigned,
    UnsignedDot,
    Zero,
    SignedZero,
    Hex,
    Hash,
    SignedHex,
    Octal,
    SignedOctal,
    Binary,
    SignedBinary,
    R,
    B,
    // Raw string states
    RawHash,           // Counting #s in opening delimiter (count stored in current_hashes)
    RawBody,           // Inside raw string content
    RawCloseHash,      // In closing delimiter (count stored in current_hashes)
    EmitRawQuoteClose, // Emit closing RawQuote, then transition to Empty
    // Escape hex states for binary strings
    EscapeHex,  // Seen \x, waiting for first hex digit
    EscapeHex1, // Seen first hex digit (stored in acc low nibble)
}

enum Defer {
    Error(ErrorDiagKind),
    Warn(WarnDiagKind),
}

pub(crate) trait Comment {
    fn comment(&mut self, span: Span);
}

impl<F: FnMut(Span)> Comment for F {
    fn comment(&mut self, span: Span) {
        self(span)
    }
}

struct RawLexer<'a, I: Iterator<Item = u8>> {
    mode: Mode,
    state: RawState,
    start: Offset,
    offset: Offset,
    defer: Option<Defer>,
    iter: I,
    acc: u64,
    diags: &'a Diags,
    comment: Option<&'a mut dyn Comment>,
    // Raw string tracking fields
    target_hashes: u8,
    current_hashes: u8,
    quote_offset: Offset,
}

impl<'a, I: Iterator<Item = u8>> RawLexer<'a, I> {
    fn new<T: IntoIterator<IntoIter = I>>(
        content: T,
        diags: &'a Diags,
        comment: Option<&'a mut dyn Comment>,
    ) -> Self {
        Self {
            mode: Mode::Shell,
            state: RawState::Init,
            defer: None,
            start: 0,
            offset: 0,
            iter: content.into_iter(),
            acc: 0,
            diags,
            comment,
            target_hashes: 0,
            current_hashes: 0,
            quote_offset: 0,
        }
    }

    fn advance(&mut self) -> Option<u8> {
        let res = self.iter.next();
        self.offset += res.is_some() as u32;
        res
    }

    fn defer(&mut self, defer: Defer) {
        self.defer = Some(defer);
    }

    fn error(&mut self, e: ErrorDiagKind) -> Option<Result<(RawToken, Span), ErrorDiag>> {
        self.state = RawState::Error;
        let span = (self.start..self.offset).into();
        Some(Err(ErrorDiag(e, span)))
    }

    fn warn(&self, w: WarnDiagKind) {
        self.diags
            .push(WarnDiag(w, (self.start..self.offset).into()));
    }

    fn error_adj(
        &mut self,
        e: ErrorDiagKind,
        adjust_start: i32,
        adjust_end: i32,
    ) -> Option<Result<(RawToken, Span), ErrorDiag>> {
        self.state = RawState::Error;
        Some(Err(ErrorDiag(
            e,
            (self.start.strict_add_signed(adjust_start)..self.offset.strict_add_signed(adjust_end))
                .into(),
        )))
    }

    fn token_adj(
        &mut self,
        token: RawToken,
        state: RawState,
        adjust_start: i32,
        adjust_end: i32,
    ) -> Option<Result<(RawToken, Span), ErrorDiag>> {
        if let Some(defer) = self.defer.take() {
            match defer {
                Defer::Error(e) => return self.error_adj(e, 0, -1),
                Defer::Warn(w) => self
                    .diags
                    .push(WarnDiag(w, (self.start..self.offset - 1).into())),
            }
        }
        let start = self.start.checked_add_signed(adjust_start).unwrap();
        let end = self.offset.checked_add_signed(adjust_end).unwrap();
        self.start = end;
        self.state = state;
        Some(Ok((token, (start..end).into())))
    }

    fn token(
        &mut self,
        token: RawToken,
        state: RawState,
    ) -> Option<Result<(RawToken, Span), ErrorDiag>> {
        let (start_adj, end_adj) = match (&token, &state) {
            // FIXME: this hack has to go, needs to be handled in parser
            (RawToken::Key, _) => (0, -2),
            (RawToken::Escape(_), _) => (0, 0),
            (RawToken::Sym, _) => (1, -1),
            (RawToken::DittoKey, _) => (1, -1),
            (_, RawState::Empty) => (0, 0),
            _ => (0, -1),
        };
        let res = self.token_adj(token, state, start_adj, end_adj);
        if end_adj == -2 {
            self.start += 1;
        }
        res
    }

    fn trans(&mut self, state: RawState) {
        // If there's a pending detour, exchange with it
        self.state = state;
    }

    fn skip(&mut self, state: RawState) {
        self.start = self.offset - 1;
        self.state = state;
        self.defer = None;
    }

    fn comment(&mut self, state: RawState) {
        if let Some(comment) = &mut self.comment {
            comment.comment(Span {
                start: self.start,
                end: self.offset - 1,
            })
        }
        self.start = self.offset - 1;
        self.state = state;
        self.defer = None;
    }

    fn emit(
        &mut self,
        token: Option<RawToken>,
        next: RawState,
    ) -> Option<Result<(RawToken, Span), ErrorDiag>> {
        if let Some(token) = token {
            self.token(token, next)
        } else {
            self.skip(next);
            None
        }
    }

    fn set_mode(&mut self, mode: Mode) -> Mode {
        use RawState::*;

        if mode == self.mode {
            // Nothing to do
            return mode;
        }

        self.state = match (self.mode, mode, self.state) {
            // Shell/String/Heredoc to expr transitions
            (Mode::Shell | Mode::String | Mode::Heredoc | Mode::RawHeredoc, Mode::FullExpr, _) => {
                self.state
            }
            // Expr to shell/string/heredoc transitions
            (Mode::FullExpr, Mode::Shell | Mode::String | Mode::Heredoc | Mode::RawHeredoc, _) => {
                self.state
            }
            // Shell-shell transitions
            (Mode::Shell, Mode::String, _) => self.state,
            (Mode::String, Mode::Shell, _) => self.state,
            (Mode::Shell, Mode::Heredoc | Mode::RawHeredoc, _)
            | (Mode::Heredoc | Mode::RawHeredoc, Mode::Shell, _) => self.state,
            // End/error transitions always succeed
            (_, _, End | Error) => self.state,
            _ => panic!(
                "Illegal mode transition {:?} => {mode:?}: {:?}",
                self.mode, self.state
            ),
        };
        mem::replace(&mut self.mode, mode)
    }

    fn set_error(&mut self) {
        self.state = RawState::Error;
    }
}

// Helper macro to call a method and possibly return its result
macro_rules! emit {
    ($self: ident . $method: ident, $token: expr, $state: expr) => {
        if let Some(res) = $self.$method($token, $state) {
            return Some(res);
        }
    };
}

macro_rules! lex {
    // Terminal rule: expands to full match expression
    {
        $self: ident : $token: expr => match { , $($m: tt)* };
        {}
    } => {
        match $self.advance() { $($m)* }
    };
    // Recursive rules: adds one explicit pattern match
    {
        $self: ident : $token: expr => match { $($m:tt)* };
        { match $pat: pat => $rhs: expr, $($rest: tt)* }
    } => {
        lex!{$self: $token => match { $($m)*, $pat => $rhs }; { $($rest)* }}
    };
    {
        $self: ident : $token: expr => match { $($m:tt)* };
        { match $pat: pat if $cond: expr => $rhs: expr, $($rest: tt)* }
    } => {
        lex!{$self: $token => match { $($m)*, $pat if $cond => $rhs }; { $($rest)* }}
    };
    // Recursive rule: common (end of stream, comments, whitespace, escapes)
    {
        $self: ident : $token: expr => match { $($m:tt)* };
        { enum common, $($rest: tt)* }
    } => {
        lex!{
            $self: $token => match {
                $($m)*,
                None => return $self.token($token, End),
                Some(b' ' | b'\t') => return $self.token($token, Space),
                Some(b'\r') => return $self.token($token, Cr),
                Some(b'\n') => return $self.token($token, Indent),
                Some(b'\\') if matches!($self.mode, Mode::String | Mode::Heredoc) => return $self.token($token, Escape)
            };
            { $($rest)* }
        }
    };
    // Recursive rule: alphanumerics
    {
        $self: ident : $token: expr => match { $($m:tt)* };
        { enum alphanum($method: ident), $($rest: tt)* }
    } => {
        lex!{
            $self: $token => match {
                $($m)*,
                Some(b'A'..=b'Z' | b'a'..=b'z' | b'_') => emit!($self.$method, $token, Ident),
                #[allow(unreachable_patterns)]
                Some(b'0') => {
                    $self.acc = 0;
                    emit!($self.$method, $token, Zero);
                }
                #[allow(unreachable_patterns)]
                Some(c @ b'1'..=b'9') => {
                    $self.acc = (c - b'0') as u64;
                    emit!($self.$method, $token, Unsigned);
                }
            };
            { $($rest)* }
        }
    };
    // Recursive rule: free symbols (may appear in literals)
    {
        $self: ident : $token: expr => match { $($m:tt)* };
        { enum symbol_free($method: ident), $($rest: tt)* }
    } => {
        lex!{
            $self: $token => match {
                $($m)*,
                // Pattern may be superseded by symbol, key, ditto key parsing
                #[allow(unreachable_patterns)]
                Some(b':') => emit!($self.$method, $token, Colon),
                Some(b'!') => emit!($self.$method, $token, Bang),
                // Pattern may be superseded by ==
                #[allow(unreachable_patterns)]
                Some(b'=') => emit!($self.$method, $token, Equal),
                // Pattern may be superseded by floating point exponent
                #[allow(unreachable_patterns)]
                Some(b'+') => emit!($self.$method, $token, Plus),
                // Pattern may be superseded by floating point exponent
                #[allow(unreachable_patterns)]
                Some(b'-') => emit!($self.$method, $token, Minus),
                // Pattern may be superseded by //
                #[allow(unreachable_patterns)]
                Some(b'/') => emit!($self.$method, $token, Slash),
                Some(b'%') => emit!($self.$method, $token, Percent),
                // Pattern may be superseded by floating point token
                #[allow(unreachable_patterns)]
                Some(b'.') => emit!($self.$method, $token, Dot),
                Some(b'[') => emit!($self.$method, $token, LeftBracket),
                Some(b']') => emit!($self.$method, $token, RightBracket),
                Some(b',') => emit!($self.$method, $token, Comma),
                Some(b'~') => emit!($self.$method, $token, Tilde),
                Some(b'^') => emit!($self.$method, $token, Caret),
                Some(b'<') => emit!($self.$method, $token, Lt),
                Some(b'>') => emit!($self.$method, $token, Gt),
                // Pattern may be superseded by ||
                #[allow(unreachable_patterns)]
                Some(b'|') => emit!($self.$method, $token, Bar),
                // Pattern may be superseded by &&
                #[allow(unreachable_patterns)]
                Some(b'&') => emit!($self.$method, $token, Amp),
                Some(b'{') => emit!($self.$method, $token, LeftBrace),
                Some(b'}') => emit!($self.$method, $token, RightBrace)
            };
            { $($rest)* }
        }
    };
    // Recursive rule: reserved symbols (may not appear in literals)
    {
        $self: ident : $token: expr => match { $($m:tt)* };
        { enum symbol_reserved($method: ident), $($rest: tt)* }
    } => {
        lex! {
            $self: $token => match {
                $($m)*,
                Some(b'$') => emit!($self.$method, $token, Dollar),
                Some(b'*') => emit!($self.$method, $token, Star),
                Some(b'(') => emit!($self.$method, $token, LeftParen),
                Some(b')') => emit!($self.$method, $token, RightParen),
                #[allow(unreachable_patterns)]
                Some(b'"') => emit!($self.$method, $token, DQuote)
            };
            { $($rest)* }
        }
    };
    // Initial rule: kicks off recursive expansion
    {$self: ident : $token: expr => { $($m: tt)* }} => {
        lex! {
            $self: $token => match {};
            { $($m)* }
        }
    };
}

// Continues matching a literal (or similar lexeme: keyword, identifier)
macro_rules! literal {
    ($self: ident, $token : expr, { $($low:tt)* } ) => {
        literal!($self, $token, {}, { $($low)* })
    };
    ($self: ident, $token : expr, { $($mid:tt)* }, { $($low:tt)* } ) => {
        lex! {
            $self: $token => {
                enum common,
                $($mid)*
                enum symbol_free(token),
                enum symbol_reserved(token),
                $($low)*
            }
        }
    };
}

// Continues matching a number-like lexeme
macro_rules! number {
    ($self: ident, $token : expr, { $($low:tt)* } ) => {
        number!($self, $token, {}, { $($low)* })
    };
    ($self: ident, $token : expr, { $($mid:tt)* }, { $($low:tt)* } ) => {
        lex! {
            $self: $token => {
                enum common,
                $($mid)*
                match Some(b':') => emit!($self.token, $token, Colon),
                enum symbol_free(token),
                enum symbol_reserved(token),
                $($low)*
            }
        }
    };
}

// Continues matching a symbol
macro_rules! symbol {
    ($self: ident, $token : expr, { $($arms:tt)* }) => {
        lex! {
            $self: $token => {
                enum common,
                $($arms)*
                match Some(b'r') if matches!($self.mode, Mode::Shell | Mode::FullExpr) => {
                    return $self.token($token, R)
                },
                enum alphanum(token),
                enum symbol_free(token),
                enum symbol_reserved(token),
                match Some(_) => match $self.mode {
                    Mode::FullExpr => return $self.error(ErrorDiagKind::BadIdent),
                    Mode::Shell | Mode::String | Mode::Heredoc | Mode::RawHeredoc => $self.trans(Literal),
                },
            }
        }
    };
}

impl<'a, I: Iterator<Item = u8>> Iterator for RawLexer<'a, I> {
    type Item = Result<(RawToken, Span), ErrorDiag>;

    /// Main state machine for lexical analysis.
    ///
    /// # State Machine Overview
    ///
    /// The lexer is essentially a hand-constructed DFA.
    ///
    /// ## State Categories
    ///
    /// Most states are named after what character was just seen or what they are trying to
    /// parse.
    ///
    /// - **Whitespace/neutral states**: `Init`, `Indent`, `Space`, `Empty` - handle whitespace
    ///   prior to next "real" token
    /// - **Identifier states**: `Ident`, `Key`, etc. - mostly alphanumeric tokens
    /// - **Number states**: `Signed`, `Unsigned`, `Float`, `Hex`, etc. - parse numeric literals
    /// - **Operator states**: `Bang`, `Minus`, `Slash`, etc. - recognize operators
    /// - **String states**: `DQuote`, `Escape`, etc. - handle string literals
    /// - **Raw string states**: `RawHash`, `RawBody`, `RawCloseHash` - raw string handling
    ///
    /// ## Keyword Recognition
    ///
    /// Keywords are recognized via a perfect hash map in `Lexer`
    ///
    /// ## Number Parsing
    ///
    /// Integer are accumulated in `self.acc` as they're read:
    /// - Leading `-` or `+` sets signed state
    /// - `0x`, `0o`, `0b` prefixes switch to hex/octal/binary
    ///
    /// Floats are handed to the parser as a span to interpret with Rust std functions since
    /// incremental floating point parsing is not trivial.
    ///
    /// ## Macros
    ///
    /// The lexer uses several macros to reduce boilerplate:
    /// - `lex!`: Main dispatch macro for state transitions
    /// - `symbol!`: Handle non-alphanumeric symbols
    /// - `literal!`: Handle mostly alphanumeric tokens (idents)
    /// - `number!`: Handle numbers
    ///
    /// # Indentation Handling
    ///
    /// This lexer folds contiguous spans of whitespace, newlines, and comments into "indent",
    /// "newline with indent", or "space" tokens.  The `Lexer` wrapper converts these into
    /// `Indent`/`Dedent` tokens by tracking indentation depth.
    #[rust_analyzer::skip]
    fn next(&mut self) -> Option<Self::Item> {
        use self::Op;
        use RawState::*;

        loop {
            match self.state {
                Init | Indent | Space | Empty => {
                    let token = match (self.state, self.mode) {
                        (Empty, _) => None,
                        (Init, _) => Some(RawToken::Indent),
                        (Indent, Mode::Shell | Mode::String | Mode::Heredoc | Mode::RawHeredoc) => {
                            Some(RawToken::NewlineIndent)
                        }
                        (Space, Mode::Shell | Mode::String | Mode::Heredoc | Mode::RawHeredoc) => {
                            Some(RawToken::Space)
                        }
                        (Space | Indent, Mode::FullExpr) => None,
                        _ => unreachable!(),
                    };
                    lex! {
                        self: token => {
                            match None => self.skip(End),
                            match Some(b'\n') => if matches!(self.mode, Mode::Heredoc | Mode::RawHeredoc) {
                                self.trans(Indent)
                            } else {
                                self.skip(Indent)
                            },
                            match Some(b'\r') => self.skip(Cr),
                            match Some(b'\t') if self.state != Space => {
                                self.start = self.offset - 1;
                                return self.error(ErrorDiagKind::Tab)
                            },
                            match Some(b' ' | b'\t') => if self.state == Empty {
                                emit!(self.emit, token, Space)
                            },
                            match Some(b'#') if self.mode != Mode::String => {
                                emit!(self.emit, token, Hash)
                            },
                            match Some(b':') => emit!(self.emit, token, Colon),
                            match Some(b'\\') if matches!(self.mode, Mode::String | Mode::Heredoc) => {
                                emit!(self.emit, token, Escape)
                            },
                            match Some(b'r') if matches!(self.mode, Mode::Shell | Mode::FullExpr) => {
                                emit!(self.emit, token, R)
                            },
                            match Some(b'b') if matches!(self.mode, Mode::Shell | Mode::FullExpr) => {
                                emit!(self.emit, token, B)
                            },
                            enum alphanum(emit),
                            enum symbol_free(emit),
                            enum symbol_reserved(emit),
                            match Some(_) => match self.mode {
                                Mode::Shell | Mode::String | Mode::Heredoc | Mode::RawHeredoc => {
                                    emit!(self.emit, token, Literal)
                                }
                                Mode::FullExpr => return self.error(ErrorDiagKind::BadIdent),
                            },
                        }
                    }
                }
                Cr => match self.advance() {
                    Some(b'\n') => self.skip(Indent),
                    _ => return self.error_adj(ErrorDiagKind::BadCr, 1, 0),
                },
                Ident => literal!(self, RawToken::Ident, {
                    match Some(b':') => self.trans(Key),
                }, {
                    match Some(b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_') => (),
                    match Some(_) => match self.mode {
                        Mode::Shell | Mode::String | Mode::Heredoc | Mode::RawHeredoc => self.trans(Literal),
                        _ => return self.error(ErrorDiagKind::BadIdent),
                    },
                }),
                Key => symbol!(self, RawToken::Key, {}),
                Bang => symbol!(self, RawToken::Op(Op::Bang), {
                    match Some(b'=') => self.trans(BangEqual),
                }),
                BangEqual => symbol!(self, RawToken::Op(Op::BangEq), {}),
                Minus => symbol!(self, RawToken::Op(Op::Minus), {
                    match Some(b'0') => {
                        self.acc = 0;
                        self.trans(SignedZero)
                    },
                    match Some(c@b'1'..=b'9') => {
                        self.acc = (c - b'0') as u64;
                        self.trans(Signed)
                    },
                }),
                Plus => symbol!(self, RawToken::Op(Op::Plus), {}),
                Star => symbol!(self, RawToken::Op(Op::Star), {}),
                Slash => symbol!(self, RawToken::Op(Op::Slash), {
                    match Some(b'/') => self.trans(SlashSlash),
                }),
                SlashSlash => symbol!(self, RawToken::Op(Op::SlashSlash), {}),
                Percent => symbol!(self, RawToken::Op(Op::Percent), {}),
                Equal => symbol!(self, RawToken::Equal, {
                    match Some(b'=') => self.trans(EqualEqual),
                }),
                EqualEqual => symbol!(self, RawToken::Op(Op::EqEq), {}),
                Dollar => return self.token(RawToken::Dollar, Empty),
                DQuote => return self.token(RawToken::DQuote, Empty),
                LeftParen => return self.token(RawToken::LeftParen, Empty),
                RightParen => return self.token(RawToken::RightParen, Empty),
                LeftBracket => return self.token(RawToken::LeftBracket, Empty),
                RightBracket => return self.token(RawToken::RightBracket, Empty),
                LeftBrace => return self.token(RawToken::LeftBrace, Empty),
                RightBrace => return self.token(RawToken::RightBrace, Empty),
                Comma => return self.token(RawToken::Comma, Empty),
                Colon => literal!(self, RawToken::Colon, {
                    match Some(b'A'..=b'Z' | b'a'..=b'z' | b'_') => self.trans(DittoKey),
                    match Some(_) => self.trans(Literal),
                }),
                DittoKey => literal!(self, RawToken::DittoKey, {
                    match Some(b':') => return self.token(RawToken::Sym, Empty),
                }, {
                    match Some(b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_') => (),
                    match Some(_) => self.trans(Literal),
                }),
                Lt => symbol!(self, RawToken::Op(Op::Lt), {
                    match Some(b'=') => self.trans(LtEq),
                }),
                Gt => symbol!(self, RawToken::Op(Op::Gt), {
                    match Some(b'=') => self.trans(GtEq),
                }),
                LtEq => symbol!(self, RawToken::Op(Op::LtEq), {}),
                GtEq => symbol!(self, RawToken::Op(Op::GtEq), {}),
                Bar => symbol!(self, RawToken::Op(Op::Bar), {
                    match Some(b'|') => self.trans(BarBar),
                }),
                BarBar => symbol!(self, RawToken::Op(Op::BarBar), {}),
                Amp => symbol!(self, RawToken::Op(Op::Amp), {
                    match Some(b'&') => self.trans(AmpAmp),
                }),
                AmpAmp => symbol!(self, RawToken::Op(Op::AmpAmp), {}),
                Dot => symbol!(self, RawToken::Op(Op::Dot), {
                    match Some(b'0'..=b'9') => self.trans(Float),
                    match Some(b'.') => self.trans(DotDot),
                    match Some(b'#') => self.trans(DotHash),
                }),
                DotHash => symbol!(self, RawToken::Op(Op::DotHash), {}),
                DotDot => symbol!(self, RawToken::DotDot, {
                    match Some(b'.') => self.trans(DotDotDot),
                }),
                DotDotDot => return self.token(RawToken::Ellipsis, Empty),
                EmitDotDot => return self.token(RawToken::DotDot, Empty),
                Tilde => symbol!(self, RawToken::Op(Op::Tilde), {}),
                Caret => symbol!(self, RawToken::Op(Op::Caret), {}),
                Literal => literal!(self, RawToken::Literal, {
                    match Some(_) => (),
                }),
                Unsigned | Signed => number!(self,
                    if self.state == Unsigned {
                        match i64::try_from(self.acc) {
                            Ok(v) => RawToken::I64(v),
                            Err(_) => {
                                if self.mode == Mode::FullExpr {
                                    return self.error(ErrorDiagKind::Overflow)
                                }
                                if self.mode != Mode::String {
                                    self.warn(WarnDiagKind::OverflowLit);
                                }
                                RawToken::Literal
                            }
                        }
                    } else {
                        match 0i64.checked_sub_unsigned(self.acc) {
                            Some(v) => RawToken::I64(v),
                            None => {
                                if self.mode == Mode::FullExpr {
                                    return self.error(ErrorDiagKind::Overflow)
                                }
                                if self.mode != Mode::String {
                                    self.warn(WarnDiagKind::OverflowLit);
                                }
                                RawToken::Literal
                            }
                        }
                    }, {
                        match Some(c@b'0'..=b'9') => {
                            match self.acc.checked_mul(10).and_then(|v| v.checked_add((c - b'0') as u64)) {
                                Some(v) => self.acc = v,
                                None => {
                                    if self.mode == Mode::FullExpr {
                                        self.acc = 0;
                                        self.defer(Defer::Error(ErrorDiagKind::Overflow));
                                    } else {
                                        if self.mode != Mode::String {
                                            self.defer(Defer::Warn(WarnDiagKind::OverflowLit));
                                        }
                                        self.trans(Literal)
                                    }
                                }
                            }
                        },
                        match Some(b'.') => {
                            self.defer = None;
                            self.trans(if self.state == Signed { SignedDot } else { UnsignedDot } )
                        },
                        match Some(b'e') => {
                            self.defer = None;
                            self.trans(ExponentStart)
                        },
                    }, {
                        match Some(..) => {
                            self.defer = None;
                            self.trans(Literal)
                        },
                    }
                ),
                Zero | SignedZero => {
                    let signed = self.state == SignedZero;
                    number!(self, RawToken::I64(0), {
                        match Some(b'x' | b'X') => {
                            self.acc = 0;
                            self.trans(if signed { SignedHex } else { Hex })
                        },
                        match Some(b'o' | b'O') => {
                            self.acc = 0;
                            self.trans(if signed { SignedOctal } else { Octal })
                        },
                        match Some(b'b' | b'B') => {
                            self.acc = 0;
                            self.trans(if signed { SignedBinary } else { Binary })
                        },
                        match Some(c @ b'0'..=b'9') => {
                            self.acc = (c - b'0') as u64;
                            self.trans(if signed { Signed } else { Unsigned })
                        },
                        match Some(b'.') => {
                            self.defer = None;
                            self.trans(if signed { SignedDot } else { UnsignedDot })
                        },
                        match Some(b'e') => {
                            self.defer = None;
                            self.trans(ExponentStart)
                        },
                    }, {
                        match Some(..) => {
                            self.trans(Literal)
                        },
                    })
                }
                UnsignedDot | SignedDot => {
                    let signed = self.state == SignedDot;
                    number!(self, RawToken::F64, {
                        match Some(b'.') => {
                            return self.token_adj(
                                match if signed {
                                    0i64.checked_sub_unsigned(self.acc)
                                } else {
                                    i64::try_from(self.acc).ok()
                                } {
                                    Some(v) => RawToken::I64(v),
                                    None => {
                                        if self.mode == Mode::FullExpr {
                                            return self.error(ErrorDiagKind::Overflow)
                                        }
                                        if self.mode != Mode::String {
                                            self.warn(WarnDiagKind::OverflowLit);
                                        }
                                        RawToken::Literal
                                    }
                                },
                                EmitDotDot,
                                0,
                                -2,
                            );
                        },
                    }, {
                        match Some(b'0'..=b'9') => self.trans(Float),
                        match Some(b'e') => self.trans(ExponentStart),
                        match Some(..) => self.trans(Literal),
                    })
                }
                Hex | SignedHex => {
                    let signed = self.state == SignedHex;
                    number!(self,
                        if signed {
                            match 0i64.checked_sub_unsigned(self.acc) {
                                Some(v) => RawToken::I64(v),
                                None => {
                                    if self.mode == Mode::FullExpr {
                                        return self.error(ErrorDiagKind::Overflow)
                                    }
                                    if self.mode != Mode::String {
                                        self.warn(WarnDiagKind::OverflowLit);
                                    }
                                    RawToken::Literal
                                }
                            }
                        } else {
                            match i64::try_from(self.acc) {
                                Ok(v) => RawToken::I64(v),
                                Err(_) => {
                                    if self.mode == Mode::FullExpr {
                                        return self.error(ErrorDiagKind::Overflow)
                                    }
                                    if self.mode != Mode::String {
                                        self.warn(WarnDiagKind::OverflowLit);
                                    }
                                    RawToken::Literal
                                }
                            }
                        }, {
                            match Some(c) if c.is_ascii_hexdigit() => {
                                let digit = match c {
                                    b'0'..=b'9' => c - b'0',
                                    b'a'..=b'f' => c - b'a' + 10,
                                    b'A'..=b'F' => c - b'A' + 10,
                                    _ => unreachable!(),
                                };
                                match self.acc.checked_mul(16).and_then(|v| v.checked_add(digit as u64)) {
                                    Some(v) => self.acc = v,
                                    None => {
                                        if self.mode == Mode::FullExpr {
                                            self.acc = 0;
                                            self.defer(Defer::Error(ErrorDiagKind::Overflow));
                                        } else {
                                            if self.mode != Mode::String {
                                                self.defer(Defer::Warn(WarnDiagKind::OverflowLit));
                                            }
                                            self.trans(Literal)
                                        }
                                    }
                                }
                            },
                        }, {
                            match Some(..) => {
                                self.defer = None;
                                self.trans(Literal)
                            },
                        }
                    )
                }
                Octal | SignedOctal => {
                    let signed = self.state == SignedOctal;
                    number!(self,
                        if signed {
                            match 0i64.checked_sub_unsigned(self.acc) {
                                Some(v) => RawToken::I64(v),
                                None => {
                                    if self.mode == Mode::FullExpr {
                                        return self.error(ErrorDiagKind::Overflow)
                                    }
                                    self.warn(WarnDiagKind::OverflowLit);
                                    RawToken::Literal
                                }
                            }
                        } else {
                            match i64::try_from(self.acc) {
                                Ok(v) => RawToken::I64(v),
                                Err(_) => {
                                    if self.mode == Mode::FullExpr {
                                        return self.error(ErrorDiagKind::Overflow)
                                    }
                                    self.warn(WarnDiagKind::OverflowLit);
                                    RawToken::Literal
                                }
                            }
                        }, {
                            match Some(c @ b'0'..=b'7') => {
                                let digit = (c - b'0') as u64;
                                match self.acc.checked_mul(8).and_then(|v| v.checked_add(digit)) {
                                    Some(v) => self.acc = v,
                                    None => {
                                        if self.mode == Mode::FullExpr {
                                            self.acc = 0;
                                            self.defer(Defer::Error(ErrorDiagKind::Overflow));
                                        } else {
                                            if self.mode != Mode::String {
                                                self.defer(Defer::Warn(WarnDiagKind::OverflowLit));
                                            }
                                            self.trans(Literal)
                                        }
                                    }
                                }
                            },
                        }, {
                            match Some(..) => {
                                self.defer = None;
                                self.trans(Literal)
                            },
                        }
                    )
                }
                Binary | SignedBinary => {
                    let signed = self.state == SignedBinary;
                    number!(self,
                        if signed {
                            match 0i64.checked_sub_unsigned(self.acc) {
                                Some(v) => RawToken::I64(v),
                                None => {
                                    if self.mode == Mode::FullExpr {
                                        return self.error(ErrorDiagKind::Overflow)
                                    }
                                    if self.mode != Mode::String {
                                        self.warn(WarnDiagKind::OverflowLit);
                                    }
                                    RawToken::Literal
                                }
                            }
                        } else {
                            match i64::try_from(self.acc) {
                                Ok(v) => RawToken::I64(v),
                                Err(_) => {
                                    if self.mode == Mode::FullExpr {
                                        return self.error(ErrorDiagKind::Overflow)
                                    }
                                    if self.mode != Mode::String {
                                        self.warn(WarnDiagKind::OverflowLit);
                                    }
                                    RawToken::Literal
                                }
                            }
                        }, {
                            match Some(c @ b'0'..=b'1') => {
                                let digit = (c - b'0') as u64;
                                match self.acc.checked_mul(2).and_then(|v| v.checked_add(digit)) {
                                    Some(v) => self.acc = v,
                                    None => {
                                        if self.mode == Mode::FullExpr {
                                            self.acc = 0;
                                            self.defer(Defer::Error(ErrorDiagKind::Overflow));
                                        } else {
                                            if self.mode != Mode::String {
                                                self.defer(Defer::Warn(WarnDiagKind::OverflowLit));
                                            }
                                            self.trans(Literal)
                                        }
                                    }
                                }
                            },
                        }, {
                            match Some(..) => {
                                self.defer = None;
                                self.trans(Literal)
                            },
                        }
                    )
                }
                Float => number!(self, RawToken::F64, {
                        match Some(b'0'..=b'9') => (),
                        match Some(b'e') => self.trans(ExponentStart),
                        match Some(..) => self.trans(Literal),
                }),
                ExponentStart => number!(self, RawToken::F64, {
                        match Some(b'+' | b'-' | b'0'..=b'9') => self.trans(Exponent),
                }, {
                        match Some(..) => self.trans(Literal),
                }),
                Exponent => number!(self, RawToken::F64, {
                        match Some(b'0'..=b'9') => (),
                        match Some(..) => self.trans(Literal),
                }),
                Escape => match self.advance() {
                    Some(b't') => return self.token(RawToken::Escape('\t'), Empty),
                    Some(b'r') => return self.token(RawToken::Escape('\r'), Empty),
                    Some(b'n') => return self.token(RawToken::Escape('\n'), Empty),
                    Some(b'"') => return self.token(RawToken::Escape('"'), Empty),
                    Some(b'0') => return self.token(RawToken::Escape('\0'), Empty),
                    Some(b'$') => return self.token(RawToken::Escape('$'), Empty),
                    Some(b'\\') => return self.token(RawToken::Escape('\\'), Empty),
                    Some(b'x') => {
                        self.acc = 0;
                        self.trans(EscapeHex);
                    }
                    _ => return self.error(ErrorDiagKind::BadEscape),
                },
                Hash => match self.advance() {
                    Some(b'[') => return self.token(RawToken::DecoratorOpen, Empty),
                    None => self.comment(End),
                    Some(b'\r') => self.comment(Cr),
                    Some(b'\n') => self.comment(Indent),
                    _ => self.trans(Comment),
                },
                Comment if matches!(self.mode, Mode::Heredoc | Mode::RawHeredoc) => {
                    match self.advance() {
                        None => return self.token(RawToken::Literal, End),
                        Some(b'\n') => return self.token(RawToken::Literal, Indent),
                        _ => (),
                    }
                }
                Comment => match self.advance() {
                    None => self.comment(End),
                    Some(b'\r') => self.comment(Cr),
                    Some(b'\n') => self.comment(Indent),
                    _ => (),
                },
                R => {
                    if self.mode != Mode::String {
                        literal!(self, RawToken::Ident, {
                            // Middle patterns - handle raw string starts and key colon
                            match Some(b'"') => {
                                // r" - raw string with 0 hashes
                                self.target_hashes = 0;
                                return self.token_adj(RawToken::RawQuote, RawBody, 0, 0);
                            },
                            match Some(b'#') => {
                                // r# - start counting hashes
                                self.current_hashes = 1;
                                self.trans(RawHash);
                            },
                            match Some(b':') => self.trans(Key),
                            match Some(b'|') if matches!(self.mode, Mode::Shell) => {
                                return self.token(RawToken::RBar, RawState::Empty);
                            },
                        }, {
                            // Lower patterns - keyword continuation or identifier/literal fallback
                            match Some(b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_') => self.trans(Ident),
                            match Some(_) => match self.mode {
                                Mode::FullExpr => return self.error(ErrorDiagKind::BadIdent),
                                _ => self.trans(Literal),
                            },
                        })
                    } else {
                        self.trans(Ident)
                    }
                }
                B => {
                    if self.mode != Mode::String {
                        literal!(self, RawToken::Ident, {
                            // Handle b" binary string start
                            match Some(b'"') => {
                                return self.token_adj(RawToken::BQuote, Empty, 0, 0);
                            },
                            match Some(b':') => self.trans(Key),
                        }, {
                            match Some(b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_') => self.trans(Ident),
                            match Some(_) => match self.mode {
                                Mode::FullExpr => return self.error(ErrorDiagKind::BadIdent),
                                _ => self.trans(Literal),
                            },
                        })
                    } else {
                        self.trans(Ident)
                    }
                }
                // Raw string states
                RawHash => match self.advance() {
                    Some(b'#') => {
                        self.current_hashes += 1;
                        // Continue counting
                    }
                    Some(b'"') => {
                        // Opening delimiter complete, emit RawQuote and go to body
                        self.target_hashes = self.current_hashes;
                        self.current_hashes = 0;
                        return self.token_adj(RawToken::RawQuote, RawBody, 0, 0);
                    }
                    None => {
                        return self.token(
                            if self.current_hashes == 0 {
                                RawToken::Ident
                            } else {
                                RawToken::Literal
                            },
                            End,
                        );
                    }
                    Some(b'\r') => return self.token(RawToken::Literal, Cr),
                    Some(b'\n') => return self.token(RawToken::Literal, Indent),
                    _ => {
                        // Invalid raw string delimiter, treat as literal
                        self.trans(Literal);
                    }
                },
                RawBody => {
                    match self.advance() {
                        Some(b'"') => {
                            if self.target_hashes == 0 {
                                // Definite closing quote
                                return self.token(RawToken::Literal, EmitRawQuoteClose);
                            } else {
                                // Potential closing quote, record position and start counting hashes
                                self.quote_offset = self.offset - 1;
                                self.current_hashes = 0;
                                self.trans(RawCloseHash);
                            }
                        }
                        None => return self.token(RawToken::Literal, End),
                        _ => (),
                    }
                }
                RawCloseHash => match self.advance() {
                    Some(b'#') => {
                        self.current_hashes += 1;
                        if self.current_hashes == self.target_hashes {
                            // Emit literal body (ending before the closing delimiter), then emit RawQuote
                            return self.token_adj(
                                RawToken::Literal,
                                EmitRawQuoteClose,
                                0,
                                -((self.offset - self.quote_offset) as i32),
                            );
                        }
                        // Continue counting
                    }
                    Some(b'"') => {
                        // Another quote - reset and treat this as the new potential closing quote
                        self.quote_offset = self.offset - 1;
                        self.current_hashes = 0;
                        // Stay in RawCloseHash
                    }
                    Some(_) => {
                        // Not a closing delimiter, go back to raw body
                        self.current_hashes = 0;
                        self.trans(RawBody);
                    }
                    None => return self.token(RawToken::Literal, End),
                },
                EmitRawQuoteClose => return self.token(RawToken::RawQuote, Empty),
                EscapeHex => match self.advance() {
                    Some(c @ (b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F')) => {
                        let nibble = match c {
                            b'0'..=b'9' => c - b'0',
                            b'a'..=b'f' => c - b'a' + 10,
                            b'A'..=b'F' => c - b'A' + 10,
                            _ => unreachable!(),
                        };
                        self.acc = nibble as u64;
                        self.trans(EscapeHex1);
                    }
                    _ => return self.error(ErrorDiagKind::BadEscape),
                },
                EscapeHex1 => match self.advance() {
                    Some(c @ (b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F')) => {
                        let nibble = match c {
                            b'0'..=b'9' => c - b'0',
                            b'a'..=b'f' => c - b'a' + 10,
                            b'A'..=b'F' => c - b'A' + 10,
                            _ => unreachable!(),
                        };
                        let byte = ((self.acc as u8) << 4) | nibble;
                        return self.token(RawToken::EscapeByte(byte), Empty);
                    }
                    _ => return self.error(ErrorDiagKind::BadEscape),
                },
                End => return None,
                // Try to resynchronize at newline
                Error => match self.advance() {
                    None => self.skip(End),
                    Some(b'\r') => self.skip(Cr),
                    Some(b'\n') => self.skip(Indent),
                    _ => (),
                },
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Error;

#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) enum TokenInfo {
    ArgSep,
    Bool(bool),
    DecoratorOpen,
    Dedent,
    Dollar,
    DQuote,
    Escape(char),
    EscapeByte(u8),
    Equal,
    I64(i64),
    F64,
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
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) struct Token {
    pub(crate) info: TokenInfo,
    pub(crate) span: Span,
}

impl Token {
    fn new(info: TokenInfo, span: Span) -> Token {
        Self { info, span }
    }
}

struct CopyIter<'s>(std::slice::Iter<'s, u8>);

impl<'s> Iterator for CopyIter<'s> {
    type Item = u8;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().copied()
    }
}

/// High-level lexer with implicit indentation handling.
///
/// ## Architecture
///
/// The lexer is split into two layers:
/// - `RawLexer`: Low-level state machine that emits raw tokens
/// - `Lexer`: High-level wrapper that adds Python-style indentation tokens
///
/// ## Indentation Handling
///
/// Do uses Python-style significant whitespace. The lexer maintains an
/// indentation stack and emits `Indent`/`Dedent` tokens implicitly:
///
/// - When indentation increases: emit `Indent` token
/// - When indentation decreases: emit `Dedent` tokens (one per level)
/// - When indentation stays same: emit `StmtSep` (statement separator)
///
/// The `current` field tracks the current indentation level, while `target`
/// is the indentation of the next line being processed.
pub(crate) struct Lexer<'a> {
    file: &'a File<'a>,
    // Raw lexer
    raw: RawLexer<'a, CopyIter<'a>>,
    // Current indentation level
    current: Offset,
    // Indentation level we are transitioning to, emitting `Indent`/`Dedent` tokens as needed
    target: Offset,
    // Baseline indentation level of the entire document (i.e. the indentation of the first line)
    baseline: Offset,
    // Stack of indentation depths
    stack: Vec<Offset>,
    // Last seen token span, sometimes needed by parser
    span: Span,
    // Byte offset of last newline
    nl: Offset,
    // Heredoc content baseline indentation level (meaningful when raw.mode() is *Heredoc)
    heredoc_baseline: Offset,
    // Remaining span to drain when heredoc_pending is true (start advances as tokens are emitted)
    heredoc_ws: Span,
}

impl<'a> Lexer<'a> {
    pub(crate) fn new(
        file: &'a File<'a>,
        diags: &'a Diags,
        comment: Option<&'a mut dyn Comment>,
    ) -> Lexer<'a> {
        let raw = RawLexer::new(CopyIter(file.content().iter()), diags, comment);
        Lexer {
            file,
            raw,
            current: 0,
            target: 0,
            baseline: Offset::MAX,
            stack: Default::default(),
            span: Default::default(),
            nl: 0,
            heredoc_baseline: 0,
            heredoc_ws: Default::default(),
        }
    }

    pub(crate) fn set_mode(&mut self, mode: Mode) -> Mode {
        let prev = self.raw.set_mode(mode);
        if matches!(mode, Mode::Heredoc | Mode::RawHeredoc) {
            self.heredoc_baseline = self.current;
        }
        prev
    }

    // Inject an error; used by the parser to force resynchronization
    pub(crate) fn set_error(&mut self) {
        self.raw.set_error()
    }

    pub(crate) fn span(&self) -> Span {
        self.span
    }

    // Inject a "synthetic" indentation level; used by the parser for certain dash lists:
    //
    // - foo
    // ^ ^
    // | |
    // | synthetic indentation level
    // |
    // "natural" indentation level
    pub(crate) fn add_indent(&mut self, offset: Offset) {
        self.stack.push(self.current);
        self.target = offset - self.nl;
        self.current = self.target;
    }

    // Set new target indentation level.
    // Indentation tokens will be emitted as needed by `check_indent`
    fn set_indent(&mut self, span: Span) {
        self.span = span;
        self.target = self.span.end - self.span.start;
        if self.baseline == Offset::MAX {
            self.baseline = self.target;
            self.current = self.target;
        }
    }

    // Called when we get a newline + indent token
    fn newline(&mut self, span: Span) -> Option<Result<Token, Error>> {
        if matches!(self.raw.mode, Mode::Heredoc | Mode::RawHeredoc) {
            return self.heredoc_newline(span);
        }
        // Record offset of most recent newline
        self.nl = span.start + 1;
        // Set indentation level based on span
        self.set_indent((span.start + 1..span.end).into());
        if self.target != self.current {
            self.check_indent()
        } else {
            Some(Ok(Token::new(
                TokenInfo::StmtSep,
                (span.start..span.start + 1).into(),
            )))
        }
    }

    fn heredoc_newline(&mut self, span: Span) -> Option<Result<Token, Error>> {
        let slice = self.file.slice(span);
        let first_nl = slice.iter().position(|&b| b == b'\n').unwrap() as Offset + span.start;
        let last_nl = slice.iter().rposition(|&b| b == b'\n').unwrap() as Offset + span.start;
        if span.end - last_nl > self.heredoc_baseline {
            self.heredoc_ws.start = first_nl;
            self.heredoc_ws.end = span.end;
            if span.start != first_nl {
                Some(Ok(Token::new(
                    TokenInfo::Literal,
                    (span.start..first_nl).into(),
                )))
            } else {
                self.check_indent()
            }
        } else {
            self.nl = last_nl;
            self.raw.set_mode(Mode::Shell);
            self.set_indent((last_nl + 1..span.end).into());
            Some(Ok(Token::new(
                TokenInfo::Literal,
                (span.start..first_nl + 1).into(),
            )))
        }
    }

    // Called to emit an ordinary token; good place for debugging
    fn token(&mut self, info: TokenInfo, span: Span) -> Token {
        // Note that we don't need to update self.span here because the parser
        // only needs to call it when hitting the end of the iterator
        Token::new(info, span)
    }

    fn end(&mut self, offset: Offset) -> Option<Result<Token, Error>> {
        self.span = (offset..offset).into();
        self.target = if self.baseline == Offset::MAX {
            0
        } else {
            self.baseline
        };
        // Clear heredoc state so check_indent can unwind the indent stack normally
        self.heredoc_ws.start = self.heredoc_ws.end;
        self.raw.set_mode(Mode::Shell);
        self.check_indent()
    }

    // Check if we need to emit indentation token, or drain pending heredoc content
    fn check_indent(&mut self) -> Option<Result<Token, Error>> {
        // Drain pending heredoc content tokens first (one per call)
        if self.heredoc_ws.start != self.heredoc_ws.end {
            let remaining = self.file.slice(self.heredoc_ws);
            let (start, end) = if let Some(i) = remaining.iter().position(|&b| b == b'\n') {
                let end = self.heredoc_ws.start + i as Offset + 1;
                let start = (self.heredoc_ws.start + self.heredoc_baseline).min(end - 1);
                (start, end)
            } else {
                let end = self.heredoc_ws.end;
                let start = (self.heredoc_ws.start + self.heredoc_baseline).min(end);
                (start, end)
            };
            // Emit everything up to and including the \n; leave the rest for next iteration
            self.heredoc_ws.start = end;
            return Some(Ok(Token::new(TokenInfo::Literal, (start..end).into())));
        }

        // Suppress normal indent logic while inside an active heredoc
        if matches!(self.raw.mode, Mode::Heredoc | Mode::RawHeredoc) {
            return None;
        }

        if self.baseline == Offset::MAX {
            return None;
        }

        if self.target > self.current {
            // Indentation increased
            self.stack.push(self.current);
            self.current = self.target;
            Some(Ok(Token::new(TokenInfo::Indent, self.span)))
        } else if self.target < self.baseline {
            // Indentation decreased beyond baseline (initial indentation of file)
            self.raw
                .diags
                .push(ErrorDiag(ErrorDiagKind::BadIndent, self.span));
            // Override baseline and current to attempt to resync
            self.baseline = self.target;
            self.current = self.target;
            Some(Err(Error))
        } else if self.target < self.current {
            // Indentation decreased, pop previous level from indentation stack
            let prev = self.stack.pop().expect("indentation logic bug");
            if self.target > prev {
                // Indentation level was not seen previously (invalid)
                self.raw
                    .diags
                    .push(ErrorDiag(ErrorDiagKind::BadIndent, self.span));
                // Try to resynchronize on next token
                if self.stack.is_empty() {
                    self.baseline = Offset::MAX;
                }
                self.stack.push(self.target);
                Some(Err(Error))
            } else {
                // Dedent one level
                self.current = prev;
                Some(Ok(Token::new(TokenInfo::Dedent, self.span)))
            }
        } else {
            None
        }
    }
}

static KEYWORDS: phf::Map<&'static [u8], TokenInfo> = phf_map! {
    b"bind" => TokenInfo::Keyword(Keyword::Bind),
    b"break" => TokenInfo::Keyword(Keyword::Break),
    b"catch" => TokenInfo::Keyword(Keyword::Catch),
    b"class" => TokenInfo::Keyword(Keyword::Class),
    b"continue" => TokenInfo::Keyword(Keyword::Continue),
    b"def" => TokenInfo::Keyword(Keyword::Def),
    b"do" => TokenInfo::Keyword(Keyword::Do),
    b"else" => TokenInfo::Keyword(Keyword::Else),
    b"false" => TokenInfo::Bool(false),
    b"finally" => TokenInfo::Keyword(Keyword::Finally),
    b"for" => TokenInfo::Keyword(Keyword::For),
    b"if" => TokenInfo::Keyword(Keyword::If),
    b"import" => TokenInfo::Keyword(Keyword::Import),
    b"let" => TokenInfo::Keyword(Keyword::Let),
    b"nil" => TokenInfo::Keyword(Keyword::Nil),
    b"pub" => TokenInfo::Keyword(Keyword::Pub),
    b"return" => TokenInfo::Keyword(Keyword::Return),
    b"throw" => TokenInfo::Keyword(Keyword::Throw),
    b"try" => TokenInfo::Keyword(Keyword::Try),
    b"true" => TokenInfo::Bool(true),
    b"while" => TokenInfo::Keyword(Keyword::While),
};

impl<'a> Iterator for Lexer<'a> {
    type Item = Result<Token, Error>;

    /// Get the next token with indentation handling.
    ///
    /// ## Token Processing Pipeline
    ///
    /// 1. **Check indentation**: First, check if we need to emit Indent/Dedent tokens
    ///    based on the current vs target indentation levels
    ///
    /// 2. **Get raw token**: Ask the RawLexer for the next raw token
    ///
    /// 3. **Handle special tokens**:
    ///    - `NewlineIndent`: Process potential statement separation and indentation change
    ///    - `Indent`: Establishes document-initial indentation level (usually consumed, not emitted)
    ///    - `Space`: Convert to `ArgSep` (argument separator in shell mode)
    ///
    /// 4. **Convert to Token**: Map RawToken to Token
    ///
    /// ## Indentation Logic
    ///
    /// The `check()` method handles the indentation stack:
    /// - If `target > current`: Emit Indent, push current to stack
    /// - If `target < current`: Emit Dedent, pop from stack
    /// - If `target == current`: Continue with next token
    ///
    /// This continues until the indentation level matches the target.
    fn next(&mut self) -> Option<Self::Item> {
        use RawToken::*;

        if let indent @ Some(..) = self.check_indent() {
            return indent;
        }

        loop {
            let t = match self.raw.next() {
                Some(item) => item,
                None => return self.end(self.raw.offset - 1),
            };
            break Some(Ok(match t {
                Err(e) => {
                    self.raw.diags.push(e);
                    return Some(Err(Error));
                }
                Ok((NewlineIndent, span)) => {
                    if let res @ Some(..) = self.newline(span) {
                        return res;
                    } else {
                        continue;
                    }
                }
                Ok((Indent, span)) => {
                    self.set_indent(span);
                    continue;
                }
                Ok((DecoratorOpen, span)) => self.token(TokenInfo::DecoratorOpen, span),
                Ok((Dollar, span)) => self.token(TokenInfo::Dollar, span),
                Ok((DQuote, span)) => self.token(TokenInfo::DQuote, span),
                Ok((Escape(c), span)) => self.token(TokenInfo::Escape(c), span),
                Ok((EscapeByte(b), span)) => self.token(TokenInfo::EscapeByte(b), span),
                Ok((Equal, span)) => self.token(TokenInfo::Equal, span),
                Ok((I64(v), span)) => self.token(TokenInfo::I64(v), span),
                Ok((F64, span)) => self.token(TokenInfo::F64, span),
                Ok((Ident, span)) => self.token(
                    KEYWORDS
                        .get(self.file.slice(span))
                        .cloned()
                        .unwrap_or(TokenInfo::Ident),
                    span,
                ),
                Ok((Key, span)) => self.token(TokenInfo::Key, span),
                Ok((LeftParen, span)) => self.token(TokenInfo::LeftParen, span),
                Ok((Literal, span)) => self.token(TokenInfo::Literal, span),
                Ok((RightParen, span)) => self.token(TokenInfo::RightParen, span),
                Ok((Space, span)) => self.token(TokenInfo::ArgSep, span),
                Ok((Op(o), span)) => self.token(TokenInfo::Op(o), span),
                Ok((LeftBracket, span)) => self.token(TokenInfo::LeftBracket, span),
                Ok((RightBracket, span)) => self.token(TokenInfo::RightBracket, span),
                Ok((LeftBrace, span)) => self.token(TokenInfo::LeftBrace, span),
                Ok((RightBrace, span)) => self.token(TokenInfo::RightBrace, span),
                Ok((Comma, span)) => self.token(TokenInfo::Comma, span),
                Ok((Colon, span)) => self.token(TokenInfo::Colon, span),
                Ok((DotDot, span)) => self.token(TokenInfo::DotDot, span),
                Ok((Ellipsis, span)) => self.token(TokenInfo::Ellipsis, span),
                Ok((DittoKey, span)) => self.token(TokenInfo::DittoKey, span),
                Ok((Sym, span)) => self.token(TokenInfo::Sym, span),
                Ok((RawQuote, span)) => self.token(TokenInfo::RawQuote, span),
                Ok((BQuote, span)) => self.token(TokenInfo::BQuote, span),
                Ok((RBar, span)) => self.token(TokenInfo::RBar, span),
            }));
        }
    }
}
