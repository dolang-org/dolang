use std::{
    borrow::Cow,
    cmp::Ordering,
    fmt::{self, Display, Write},
    mem,
};

use crate::{
    Compiler,
    ast::{
        Arg, ArrayElem, Assign, Bind, Block, CatchHandler, Class, Const, Decorator, Def,
        DefVariant, DictElem, Expand, Expr, ExprBody, For, Function, GetVariant, GroupDelim, Ident,
        If, IfBranch, Import, ImportElement, ImportItem, Key, Let, Pair, Param, ParamDefault,
        Pattern, PrimStmt, Return, Single, SpecialMethod, Stmt, Throw, Try, Unit, While,
        visit::Node,
    },
    diag::{AnnotationKind, NoteKind, Severity},
    lex::{self, Keyword, Lexer, Mode, Op, Token, TokenInfo},
    source::{Annotate, Diagnose, Diags, File, Note, Offset, Patch, Span},
};

// Macro to make pattern matching tokens more compact
macro_rules! token {
    ($pat: pat) => {
        Token { info: $pat, .. }
    };
    ($pat: pat, $span: pat) => {
        Token {
            info: $pat,
            span: $span,
        }
    };
}

macro_rules! decay {
    ($token: expr, $($pat: pat => $expr: expr),+) => {
        {
            let mut token = $token;
            match &mut token {
                $(Some(token@token!($pat)) => {
                    token.info = $expr;
                })+
                _ => (),
            }
            token
        }
    }
}

macro_rules! decay_ident {
    ($token: expr) => {
        decay!($token, TokenInfo::Keyword(
                self::Keyword::Catch
                | self::Keyword::Def
                | self::Keyword::Bind
                | self::Keyword::Do
                | self::Keyword::Else
                | self::Keyword::Field
                | self::Keyword::Finally
                | self::Keyword::For
                | self::Keyword::If
                | self::Keyword::Let
                | self::Keyword::Throw
                | self::Keyword::Try
                | self::Keyword::While) => TokenInfo::Ident)
    }
}

macro_rules! decay_field {
    ($token: expr) => {
        decay!($token, TokenInfo::Bool(_) | TokenInfo::Keyword(
                self::Keyword::Catch
                | self::Keyword::Def
                | self::Keyword::Bind
                | self::Keyword::Do
                | self::Keyword::Else
                | self::Keyword::Field
                | self::Keyword::Finally
                | self::Keyword::For
                | self::Keyword::If
                | self::Keyword::Let
                | self::Keyword::Nil
                | self::Keyword::Throw
                | self::Keyword::Try
                | self::Keyword::While) => TokenInfo::Ident)
    }
}

macro_rules! decay_literal {
    ($token: expr) => {
        decay!($token,
            TokenInfo::Equal
            | TokenInfo::Ident
            | TokenInfo::LeftBracket
            | TokenInfo::RightBracket
            | TokenInfo::LeftBrace
            | TokenInfo::RightBrace
            | TokenInfo::Comma
            | TokenInfo::DotDot
            | TokenInfo::Ellipsis
            | TokenInfo::Keyword(
                self::Keyword::Catch
                | self::Keyword::Def
                | self::Keyword::Bind
                | self::Keyword::Do
                | self::Keyword::Else
                | self::Keyword::Field
                | self::Keyword::Finally
                | self::Keyword::For
                | self::Keyword::If
                | self::Keyword::Let
                | self::Keyword::Throw
                | self::Keyword::Try
                | self::Keyword::While)
            | TokenInfo::Op(_)
            | TokenInfo::RBar
                => TokenInfo::Literal)
    }
}

/// Match a token that can start an expression (string literals, numeric literals, etc.)
macro_rules! expr_start_not_left_bracket {
    () => {
        TokenInfo::LeftParen
            | TokenInfo::LeftBrace
            | TokenInfo::DQuote
            | TokenInfo::RawQuote
            | TokenInfo::BQuote
            | TokenInfo::Sym
            | TokenInfo::Int(_)
            | TokenInfo::F64
            | TokenInfo::Bool(_)
            | TokenInfo::Keyword(Keyword::Nil)
    };
}

macro_rules! expr_start {
    () => {
        expr_start_not_left_bracket!() | TokenInfo::LeftBracket
    };
}

macro_rules! expr_tail_break {
    () => {
        TokenInfo::RightParen
            | TokenInfo::RightBracket
            | TokenInfo::RightBrace
            | TokenInfo::Dollar
            | TokenInfo::Comma
            | TokenInfo::Colon
            | TokenInfo::ArgSep
            | TokenInfo::StmtSep
            | TokenInfo::Indent
            | TokenInfo::Dedent
            | TokenInfo::Literal
            | TokenInfo::Equal
            | TokenInfo::Escape(_)
    };
}

macro_rules! decay_string {
    ($token: expr) => {
        decay!($token,
            TokenInfo::Equal
            | TokenInfo::Int(_)
            | TokenInfo::Bool(_)
            | TokenInfo::F64
            | TokenInfo::Ident
            | TokenInfo::Keyword(_)
            | TokenInfo::Op(_)
            | TokenInfo::LeftBracket
            | TokenInfo::RightBracket
            | TokenInfo::LeftBrace
            | TokenInfo::RightBrace
            | TokenInfo::LeftParen
            | TokenInfo::RightParen
            | TokenInfo::Comma
            | TokenInfo::Colon
            | TokenInfo::DotDot
            | TokenInfo::Ellipsis
            | TokenInfo::DQuote
            | TokenInfo::RawQuote
            | TokenInfo::BQuote
            | TokenInfo::RBar
            | TokenInfo::ArgSep => TokenInfo::Literal)
    }
}

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
        }
    }
}

struct SyntaxDiag {
    span: Span,
    message: Cow<'static, str>,
}

impl SyntaxDiag {
    fn new(span: Span, message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            span,
            message: message.into(),
        }
    }
}

impl Diagnose for SyntaxDiag {
    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "{}", self.message)
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn span(&self) -> Span {
        self.span
    }
}

struct InvalidLValue(Span);

impl Diagnose for InvalidLValue {
    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "expression isn't a valid assignment target")
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn span(&self) -> Span {
        self.0
    }
}

struct InvalidCompactOp(Op, Span);

impl Diagnose for InvalidCompactOp {
    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "operator not allowed in compact expressions: {}", self.0)
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn span(&self) -> Span {
        self.1
    }
}

#[derive(Copy, Clone)]
struct ImplicitDelimitedConcat {
    span: Span,
    insert: Span,
}

impl Patch for ImplicitDelimitedConcat {
    fn span(&self) -> Span {
        Span {
            start: self.insert.start,
            end: self.insert.start,
        }
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(
            w,
            "insert `$` if you intended for the entire argument to be an expression"
        )
    }

    fn sub(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "$")
    }
}

impl Diagnose for ImplicitDelimitedConcat {
    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(
            w,
            "implicit concatenation not permitted after delimited expression"
        )
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn span(&self) -> Span {
        self.span
    }

    fn patches(&self) -> Box<dyn Iterator<Item = Box<dyn Patch>>> {
        Box::new([Box::new(*self) as Box<dyn Patch>].into_iter())
    }
}

struct AmbigIndex(Span, Span);

enum AmbigIndexPatchKind {
    NoSpace,
    Parens,
}

struct AmbigIndexPatch(AmbigIndexPatchKind, Span, Span);

impl Patch for AmbigIndexPatch {
    fn span(&self) -> Span {
        match self.0 {
            AmbigIndexPatchKind::NoSpace => self.1 | self.2,
            AmbigIndexPatchKind::Parens => self.2,
        }
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        match self.0 {
            AmbigIndexPatchKind::NoSpace => write!(w, "remove space"),
            AmbigIndexPatchKind::Parens => {
                write!(w, "wrap with parentheses to call with a singleton list")
            }
        }
    }

    fn sub(&self, compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        let index = compiler.file.str(self.2);
        match self.0 {
            AmbigIndexPatchKind::NoSpace => {
                let lhs = compiler.file.str(self.1);
                write!(w, "{lhs}{index}")
            }
            AmbigIndexPatchKind::Parens => write!(w, "({index})"),
        }
    }
}

impl Diagnose for AmbigIndex {
    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "index separated by whitespace is misleading")
    }

    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn span(&self) -> Span {
        self.0 | self.1
    }

    fn patches(&self) -> Box<dyn Iterator<Item = Box<dyn Patch>>> {
        Box::new(
            [
                Box::new(AmbigIndexPatch(
                    AmbigIndexPatchKind::NoSpace,
                    self.0,
                    self.1,
                )) as Box<dyn Patch>,
                Box::new(AmbigIndexPatch(AmbigIndexPatchKind::Parens, self.0, self.1)),
            ]
            .into_iter(),
        )
    }
}

struct BadFloat(Span);

impl Diagnose for BadFloat {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(
            w,
            "invalid floating point constant: {}",
            compiler.file.str(self.0)
        )
    }

    fn span(&self) -> Span {
        self.0
    }
}

#[derive(Copy, Clone)]
struct MisleadingArg {
    arg0_span: Span,
    arg_span: Span,
    patch_span: Span,
}

impl Patch for MisleadingArg {
    fn span(&self) -> Span {
        Span {
            start: self.patch_span.start,
            end: self.patch_span.start,
        }
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "insert a space")
    }

    fn sub(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, " ")
    }
}

impl Annotate for MisleadingArg {
    fn kind(&self) -> AnnotationKind {
        AnnotationKind::Context
    }

    fn span(&self) -> Span {
        self.arg0_span
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "expression is actually an argument to this function")
    }
}

impl Diagnose for MisleadingArg {
    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "juxtaposed argument is misleading")
    }

    fn span(&self) -> Span {
        self.arg_span
    }

    fn patches(&self) -> Box<dyn Iterator<Item = Box<dyn Patch>>> {
        Box::new([Box::new(*self) as Box<dyn Patch>].into_iter())
    }

    fn annotations(&self) -> Box<dyn Iterator<Item = Box<dyn Annotate>>> {
        Box::new([Box::new(*self) as Box<dyn Annotate>].into_iter())
    }
}

struct NonConstExpr(Span);

impl Diagnose for NonConstExpr {
    fn span(&self) -> Span {
        self.0
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "expression is not a constant")
    }
}

struct RequiredAfterOptional(Span);

impl Diagnose for RequiredAfterOptional {
    fn span(&self) -> Span {
        self.0
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(
            w,
            "required positional items must precede any optional positional items"
        )
    }
}

struct RestMustBeTrailing(Span);

impl Diagnose for RestMustBeTrailing {
    fn span(&self) -> Span {
        self.0
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "rest parameter must be trailing")
    }
}

#[derive(Clone)]
struct MisleadingDollar(Span);

impl Diagnose for MisleadingDollar {
    fn span(&self) -> Span {
        self.0
    }

    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "misleading `$`")
    }

    fn notes(&self) -> Box<dyn Iterator<Item = Box<dyn Note>>> {
        Box::new([Box::new(self.clone()) as Box<dyn Note>].into_iter())
    }

    fn patches(&self) -> Box<dyn Iterator<Item = Box<dyn Patch>>> {
        Box::new(
            [
                Box::new(MisleadingDollarPatch::Remove(self.0)) as Box<dyn Patch>,
                Box::new(MisleadingDollarPatch::Insert(Span {
                    start: self.0.end,
                    end: self.0.end,
                })),
            ]
            .into_iter(),
        )
    }
}

impl Note for MisleadingDollar {
    fn kind(&self) -> NoteKind {
        NoteKind::Info
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(
            w,
            "`$` is always a low-precedence call in full expression contexts"
        )
    }
}

enum MisleadingDollarPatch {
    Remove(Span),
    Insert(Span),
}

impl Patch for MisleadingDollarPatch {
    fn span(&self) -> Span {
        match self {
            MisleadingDollarPatch::Remove(span) | MisleadingDollarPatch::Insert(span) => *span,
        }
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        match self {
            MisleadingDollarPatch::Remove(_) => write!(w, "remove the `$`"),
            MisleadingDollarPatch::Insert(_) => write!(w, "insert a space"),
        }
    }

    fn sub(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        match self {
            MisleadingDollarPatch::Remove(_) => Ok(()),
            MisleadingDollarPatch::Insert(_) => write!(w, " "),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Error;

impl From<lex::Error> for Error {
    fn from(_value: lex::Error) -> Self {
        Error
    }
}

pub(crate) type Result<T> = std::result::Result<T, Error>;

#[derive(Default)]
enum Inner<T> {
    #[default]
    Empty,
    Full(T),
    End,
}

struct Peek<'a> {
    lexer: Lexer<'a>,
    peek: Inner<<Self as Iterator>::Item>,
}

impl<'a> Peek<'a> {
    fn set_mode(&mut self, mode: Mode) -> Mode {
        self.lexer.set_mode(mode)
    }

    fn set_error(&mut self) {
        self.lexer.set_error()
    }

    fn peek(&mut self) -> Result<Option<&mut Token>> {
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
                Inner::Full(Ok(ref mut t)) => Some(t),
                Inner::Full(Err(e)) => {
                    self.peek = Inner::Empty;
                    return Err(e.into());
                }
                Inner::End => None,
            };
        })
    }

    fn push(&mut self, token: Token) {
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
        }
    }
}

impl<'a> Iterator for Peek<'a> {
    type Item = <Lexer<'a> as Iterator>::Item;

    fn next(&mut self) -> Option<Self::Item> {
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

struct Scope<'a> {
    #[expect(dead_code)]
    parent: Option<&'a Scope<'a>>,
}

impl<'a> Scope<'a> {
    fn new() -> Self {
        Self { parent: None }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExprMode {
    Shell,
    Compact,
    Full,
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

#[derive(Copy, Clone)]
enum ParamMode {
    HorizFunc,
    VertFunc,
    HorizPattern,
    VertPattern,
}

enum UnquotedMode {
    Shell,
    Data,
}

impl ParamMode {
    fn is_pattern(&self) -> bool {
        matches!(self, Self::HorizPattern | Self::VertPattern)
    }

    fn is_vertical(&self) -> bool {
        matches!(self, Self::VertFunc | Self::VertPattern)
    }
    fn supports_defaults(&self) -> bool {
        matches!(self, Self::HorizFunc | Self::VertFunc | Self::VertPattern)
    }
}

pub(crate) struct Parser<'a> {
    lex: Peek<'a>,
    file: &'a File<'a>,
    diags: &'a Diags,
    fail: bool,
}

impl<'a> Parser<'a> {
    pub(crate) fn new(lexer: Lexer<'a>, file: &'a File<'a>, diags: &'a Diags) -> Self {
        Parser {
            diags,
            lex: lexer.into(),
            file,
            fail: false,
        }
    }

    fn push_colon(&mut self, span: Span) {
        self.lex.push(Token {
            info: TokenInfo::Colon,
            span,
        })
    }

    fn add_indent(&mut self, offset: Offset) {
        self.lex.lexer.add_indent(offset)
    }

    fn with_mode<R>(
        &mut self,
        mode: Mode,
        f: impl for<'b> FnOnce(&'b mut Self) -> Result<R>,
    ) -> Result<R> {
        let prev = self.lex.set_mode(mode);
        let res = f(self);
        self.lex.set_mode(prev);
        res
    }

    fn parse_range_tail(&mut self, scope: &mut Scope, mode: ExprMode) -> Result<Option<Expr>> {
        Ok(match self.peek()? {
            None | Some(token!(expr_tail_break!())) => None,
            _ => Some(self.parse_expr_prec(scope, mode, Some(Prec::RANGE))?),
        })
    }

    fn peek(&mut self) -> Result<Option<&mut Token>> {
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

    fn next(&mut self) -> Result<Option<Token>> {
        Ok(match self.lex.next() {
            None => None,
            Some(Ok(token)) => Some(token),
            Some(Err(e)) => {
                self.fail = true;
                return Err(e.into());
            }
        })
    }

    fn consume(&mut self) -> Token {
        self.next()
            .expect("consume: unexpected error")
            .expect("consume: unexpected end")
    }

    fn expect(&mut self, _scope: &mut Scope, expect: &[ExpectKind]) -> Result<Span> {
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

    fn expect_matching(&mut self, scope: &mut Scope, expect: ExpectKind, open: Span) -> Span {
        self.expect(scope, &[expect]).ok().unwrap_or(open)
    }

    fn advance(&mut self) -> Span {
        self.lex
            .next()
            .expect("advance: end of tokens")
            .expect("advance: unhandled error")
            .span
    }

    fn syntax_error(
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

    fn parse_quoted_string(&mut self, scope: &mut Scope, open: Span, bin: bool) -> Result<Expr> {
        use TokenInfo::*;

        let expr = self.with_mode(lex::Mode::String, |this| {
            let mut exprs = Vec::new();
            let close = loop {
                let expr = match this.peek()? {
                    token @ (None | Some(token!(StmtSep | Indent | Dedent))) => {
                        let token = token.cloned();
                        this.syntax_error(scope, token, "expected closing `\"`");
                        // Try to recover by considering string ended here
                        break open;
                    }
                    Some(token!(Dollar)) => {
                        this.advance();
                        this.parse_expr_primary(scope, ExprMode::Compact)?
                    }
                    Some(token!(DQuote)) => break this.advance(),
                    Some(_) => match decay_string!(this.next()?) {
                        Some(token!(Literal, span)) => Expr::Literal(span),
                        Some(token!(Key, span)) => Expr::Literal(span | span.after_right_char()),
                        Some(token!(DittoKey, span)) => {
                            Expr::Literal(span.before_left_char() | span)
                        }
                        Some(token!(Sym, span)) => {
                            Expr::Literal(span.before_left_char() | span.after_right_char())
                        }
                        Some(token!(Escape(c), span)) => {
                            if bin {
                                Expr::EscapeByte(c as u8, span)
                            } else {
                                Expr::Escape(c, span)
                            }
                        }
                        Some(token @ token!(EscapeByte(..), _)) => {
                            if bin {
                                let TokenInfo::EscapeByte(b) = token.info else {
                                    unreachable!()
                                };
                                Expr::EscapeByte(b, token.span)
                            } else {
                                return Err(this.syntax_error(
                                    scope,
                                    Some(token),
                                    "\\x escapes are only valid in binary strings",
                                ));
                            }
                        }
                        _ => unreachable!(),
                    },
                };
                exprs.push(expr);
            };
            if bin {
                Ok(Expr::BinConcat { exprs, open, close })
            } else {
                Ok(Expr::Concat {
                    exprs,
                    delim_span: Some(open | close),
                    arg: false,
                })
            }
        })?;
        Ok(expr.optimize())
    }

    /// After consuming `|`, optionally consume `-` for strip mode.
    /// Returns `(intro_span, strip)` where `intro_span` covers `|` or `|-`.
    fn parse_heredoc_intro(&mut self, pipe_span: Span) -> Result<(Span, bool)> {
        use self::Op;
        use TokenInfo::*;
        if let Some(token!(Op(Op::Minus))) = self.peek()? {
            let minus_span = self.advance();
            Ok((pipe_span | minus_span, true))
        } else {
            Ok((pipe_span, false))
        }
    }

    fn parse_heredoc(
        &mut self,
        scope: &mut Scope,
        pipe_span: Span,
        strip: bool,
        raw: bool,
    ) -> Result<Expr> {
        use self::Ident;
        use TokenInfo::*;

        let mut exprs = Vec::new();
        self.with_mode(
            if raw {
                lex::Mode::RawHeredoc
            } else {
                lex::Mode::Heredoc
            },
            |this| {
                loop {
                    match this.peek()? {
                        None => unreachable!("heredoc always closed by Dedent before EOF"),
                        Some(token!(Dedent)) => {
                            this.advance();
                            break;
                        }
                        Some(token!(Dollar)) if !raw => {
                            this.advance();
                            match this.peek()? {
                                Some(token!(Key)) => {
                                    let span = this.advance();
                                    exprs.push(Expr::Ident(Ident::new(span)));
                                    exprs.push(Expr::Literal(span.after_right_char()));
                                }
                                _ => exprs.push(this.parse_expr_primary(scope, ExprMode::Compact)?),
                            }
                        }
                        Some(token!(Literal)) => {
                            let span = this.advance();
                            exprs.push(Expr::Literal(span));
                        }
                        Some(_) => match decay_string!(this.next()?) {
                            Some(token!(Literal, span)) => exprs.push(Expr::Literal(span)),
                            Some(token!(Key, span)) => {
                                exprs.push(Expr::Literal(span | span.after_right_char()))
                            }
                            Some(token!(DittoKey, span)) => {
                                exprs.push(Expr::Literal(span.before_left_char() | span))
                            }
                            Some(token!(Sym, span)) => exprs.push(Expr::Literal(
                                span.before_left_char() | span.after_right_char(),
                            )),
                            Some(token!(Escape('$'), span)) if !raw => {
                                exprs.push(Expr::Escape('$', span))
                            }
                            Some(token!(Escape('\\'), span)) if !raw => {
                                exprs.push(Expr::Escape('\\', span))
                            }
                            Some(token!(Escape(_), span)) => exprs.push(Expr::Literal(span)),
                            Some(token!(Dollar, span)) => exprs.push(Expr::Literal(span)),
                            _ => unreachable!(),
                        },
                    }
                }
                Ok(())
            },
        )?;
        if strip && let Some(Expr::Literal(span)) = exprs.last_mut() {
            let slice = self.file.slice(*span);
            if slice.ends_with(b"\n") {
                span.end -= 1;
            } else if slice.ends_with(b"\r\n") {
                span.end -= 2;
            }
        }
        Ok(Expr::Concat {
            exprs,
            delim_span: Some(pipe_span),
            arg: false,
        }
        .optimize())
    }

    fn parse_lambda_params(&mut self, scope: &mut Scope) -> Result<Vec<Param>> {
        match self.peek()? {
            Some(token!(TokenInfo::Op(Op::Bar))) => {
                self.advance();
                let params = self.parse_params(scope, ParamMode::HorizFunc)?;
                self.expect(scope, &[ExpectKind::Op(Op::Bar)])?;
                Ok(params)
            }
            _ => Ok(vec![]),
        }
    }

    fn parse_lambda(&mut self, scope: &mut Scope, do_span: Span) -> Result<Expr> {
        let params = self.parse_lambda_params(scope)?;
        let expr = self.parse_expr(scope, ExprMode::Full)?;
        Ok(Expr::Lambda {
            func: Function {
                params,
                body: Block {
                    stmts: vec![Stmt::Prim(PrimStmt::Expr(expr))],
                    vars: Default::default(),
                    repl: None,
                },
            },
            do_span: Some(do_span),
        })
    }

    fn parse_arg_pack(
        &mut self,
        scope: &mut Scope,
        initial: Option<(Expr, Span)>,
    ) -> Result<Vec<Arg>> {
        use self::Key;
        use TokenInfo::*;

        let mut args = if let Some((arg, span)) = initial {
            vec![Arg::Pos(Single {
                expr: arg,
                delim_span: Some(span),
            })]
        } else {
            Vec::new()
        };
        loop {
            let fail = self.fail;
            let arg = match self.peek()? {
                Some(token!(RightParen)) => break,
                Some(token!(Key, span)) => {
                    let span = *span;
                    self.advance();
                    Arg::Key(Key {
                        key_span: span,
                        colon_span: span.after_right_char(),
                        expr: self.parse_expr(scope, ExprMode::Full)?,
                        delim_span: if let Some(token!(Comma)) = self.peek()? {
                            Some(self.advance())
                        } else {
                            None
                        },
                    })
                }
                Some(token!(Ellipsis, span)) => {
                    let span = *span;
                    self.advance();
                    Arg::Expand(Expand {
                        expr: self.parse_expr(scope, ExprMode::Full)?,
                        ellipsis_span: span,
                        delim_span: if let Some(token!(Comma)) = self.peek()? {
                            Some(self.advance())
                        } else {
                            None
                        },
                    })
                }
                None if fail => break,
                _ => Arg::Pos(Single {
                    expr: self.parse_expr(scope, ExprMode::Full)?,
                    delim_span: if let Some(token!(Comma)) = self.peek()? {
                        Some(self.advance())
                    } else {
                        None
                    },
                }),
            };
            args.push(arg)
        }
        Ok(args)
    }

    fn parse_expr_primary(&mut self, scope: &mut Scope, mode: ExprMode) -> Result<Expr> {
        use self::{Ident, Keyword};
        use TokenInfo::*;

        match self.next()? {
            Some(token!(Dollar, span)) if matches!(mode, ExprMode::Shell) => Ok(Expr::Group {
                expr: Box::new(self.parse_expr(scope, ExprMode::Compact)?),
                delim: Some(GroupDelim::Dollar(span)),
            }),
            Some(token!(Sym, span)) => Ok(Expr::Sym(span)),
            Some(token!(DQuote, span)) => self.parse_quoted_string(scope, span, false),
            Some(token!(BQuote, open)) => self.parse_quoted_string(scope, open, true),
            Some(token!(RawQuote, start)) => {
                let content = self.expect(scope, &[ExpectKind::Literal])?;
                let end = self.expect_matching(scope, ExpectKind::RawQuote, start);
                Ok(Expr::Group {
                    expr: Box::new(Expr::Literal(content)),
                    delim: Some(GroupDelim::RawQuotes(start, end)),
                })
            }
            Some(token!(LeftParen, left)) => {
                let expr = self.with_mode(lex::Mode::FullExpr, |this| {
                    let expr = this.parse_expr(scope, ExprMode::Full)?;
                    let right = this.expect_matching(scope, ExpectKind::RightParen, left);
                    Ok(Expr::Group {
                        expr: Box::new(expr),
                        delim: Some(GroupDelim::Paren(left | right)),
                    })
                })?;
                Ok(expr)
            }
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
                decay_literal!(token)
            } else {
                decay_ident!(token)
            } {
                Some(token!(Literal, span)) => Ok(Expr::Literal(span)),
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

    fn parse_expr_const(&mut self, scope: &mut Scope, mode: ExprMode) -> Result<(Expr, Const)> {
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
        use self::Ident;
        use TokenInfo::*;
        self.with_mode(lex::Mode::FullExpr, |this| {
            let mut elems = Vec::new();
            let right = loop {
                let (key, colon_span) = match this.peek()? {
                    Some(token!(RightBrace)) => break this.advance(),
                    Some(token!(Key, span)) => {
                        let span = *span;
                        this.advance();
                        (Expr::Sym(span), span.after_right_char())
                    }
                    Some(token!(Dollar)) => {
                        let dollar_span = this.advance();
                        if let Some(token!(Key)) = this.peek()? {
                            let span = this.advance();
                            (
                                Expr::Group {
                                    expr: Box::new(Expr::Ident(Ident::new(span))),
                                    delim: Some(GroupDelim::Dollar(dollar_span)),
                                },
                                span.after_right_char(),
                            )
                        } else {
                            let key = this.parse_expr(scope, ExprMode::Full)?;
                            let span = this.expect(scope, &[ExpectKind::Colon])?;
                            (key, span)
                        }
                    }
                    Some(token!(Ellipsis)) => {
                        let ellipsis_span = this.advance();
                        let expr = this.parse_expr(scope, ExprMode::Full)?;
                        let comma_span = if let Some(token!(Comma)) = this.peek()? {
                            Some(this.advance())
                        } else {
                            None
                        };
                        elems.push(DictElem::Expand(Expand {
                            expr,
                            delim_span: comma_span,
                            ellipsis_span,
                        }));
                        continue;
                    }
                    Some(token!(DittoKey)) => {
                        let span = this.advance();
                        let comma_span = if let Some(token!(Comma)) = this.peek()? {
                            Some(this.advance())
                        } else {
                            None
                        };
                        elems.push(DictElem::Pair(Pair {
                            key: Expr::Sym(span),
                            value: Expr::Ident(Ident::new(span)),
                            colon_span: Some(span.before_left_char()),
                            delim_span: comma_span,
                        }));
                        continue;
                    }
                    None => break this.expect_matching(scope, ExpectKind::RightBracket, left),
                    _ => {
                        let key = this.parse_expr(scope, ExprMode::Full)?;
                        let span = match this.peek()? {
                            Some(token!(Comma)) => {
                                let delim_span = Some(this.advance());
                                elems.push(DictElem::Single(Single {
                                    expr: key,
                                    delim_span,
                                }));
                                continue;
                            }
                            Some(token!(RightBrace)) => {
                                elems.push(DictElem::Single(Single {
                                    expr: key,
                                    delim_span: None,
                                }));
                                break this.advance();
                            }
                            Some(token!(Colon)) => this.advance(),
                            None => {
                                elems.push(DictElem::Single(Single {
                                    expr: key,
                                    delim_span: None,
                                }));
                                break this.expect_matching(scope, ExpectKind::RightBracket, left);
                            }
                            _ => {
                                let token = this.next().unwrap();
                                return Err(this.syntax_error(
                                    scope,
                                    token,
                                    "invalid expression in dict literal",
                                ));
                            }
                        };
                        (key, span)
                    }
                };
                let value = this.parse_expr(scope, ExprMode::Full)?;
                let comma_span = if let Some(token!(Comma)) = this.peek()? {
                    Some(this.advance())
                } else {
                    None
                };
                elems.push(DictElem::Pair(Pair {
                    key,
                    value,
                    colon_span: Some(colon_span),
                    delim_span: comma_span,
                }));
            };
            Ok(Expr::Dict {
                elems,
                brace_span: Some(left | right),
            })
        })
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
                    Some(token!(TokenInfo::Ellipsis)) => {
                        let ellipsis_span = this.advance();
                        let expr = this.parse_expr(scope, ExprMode::Full)?;
                        let comma = if let Some(token!(TokenInfo::Comma)) = this.peek()? {
                            Some(this.advance())
                        } else {
                            None
                        };
                        elems.push(crate::ast::ArrayElem::Expand(Expand {
                            expr,
                            delim_span: comma,
                            ellipsis_span,
                        }));
                    }
                    None => break this.expect_matching(scope, ExpectKind::RightBracket, left),
                    _ => {
                        let expr = this.parse_expr(scope, ExprMode::Full)?;
                        let comma = if let Some(token!(TokenInfo::Comma)) = this.peek()? {
                            Some(this.advance())
                        } else {
                            None
                        };
                        elems.push(crate::ast::ArrayElem::Single(Single {
                            expr,
                            delim_span: comma,
                        }));
                    }
                }
            };
            Ok(Expr::Array {
                elems,
                bracket_span: Some(left | right),
            })
        })?;
        Ok(expr)
    }

    fn parse_implicit_concat(
        &mut self,
        scope: &mut Scope,
        expr: Option<Expr>,
        mode: UnquotedMode,
    ) -> Result<Expr> {
        use TokenInfo::*;

        let mut exprs = match expr {
            Some(expr) => vec![expr],
            None => {
                // FIXME: should refactor call sites to handle this
                if matches!(mode, UnquotedMode::Shell) {
                    vec![self.parse_expr_primary(scope, ExprMode::Shell)?]
                } else {
                    vec![]
                }
            }
        };
        loop {
            let next = match self.peek()? {
                None | Some(token!(StmtSep | Indent | Dedent)) => break,
                Some(token!(ArgSep)) if matches!(mode, UnquotedMode::Shell) => break,
                Some(token!(DQuote)) if matches!(mode, UnquotedMode::Shell) => {
                    let span = self.advance();
                    self.parse_quoted_string(scope, span, false)?
                }
                Some(token!(Dollar)) => self.parse_expr_primary(scope, ExprMode::Shell)?,
                Some(_) => match decay_string!(self.next()?) {
                    Some(token!(Literal, span)) => Expr::Literal(span),
                    Some(token!(Key, span)) => Expr::Literal(span | span.after_right_char()),
                    Some(token!(DittoKey, span)) => Expr::Literal(span.before_left_char() | span),
                    Some(token!(Sym, span)) => {
                        Expr::Literal(span.before_left_char() | span.after_right_char())
                    }
                    Some(token!(Escape(c), span)) => Expr::Escape(c, span),
                    _ => self.parse_expr_primary(scope, ExprMode::Shell)?,
                },
            };
            exprs.push(next)
        }
        if exprs.len() == 1 {
            Ok(exprs.pop().unwrap())
        } else {
            Ok(Expr::Concat {
                exprs,
                delim_span: None,
                arg: true,
            }
            .optimize())
        }
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
                    let op = *op;
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

        loop {
            match self.peek()? {
                Some(token!(TokenInfo::DotDot, span)) if !matches!(mode, ExprMode::Shell) => {
                    let prec = Prec::RANGE;
                    if prec.terminate(&min_prec) {
                        break;
                    }
                    let span = *span;
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
                    let op = *op;
                    let span = *span;
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
                        lhs = Expr::Binary {
                            op,
                            exprs: [lhs, rhs].into(),
                            op_span: span,
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
                            args.push(Arg::Pos(Single {
                                expr: rhs,
                                delim_span: None,
                            }));
                            *delim = Some(GroupDelim::Dollar(span));
                        }
                        _ => {
                            lhs = Expr::Call {
                                arg0: Box::new(lhs),
                                args: vec![Arg::Pos(Single {
                                    expr: rhs,
                                    delim_span: None,
                                })],
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
                                } => args.push(Arg::Pos(Single {
                                    expr: rhs,
                                    delim_span: None,
                                })),
                                _ => {
                                    lhs = Expr::Call {
                                        arg0: Box::new(lhs),
                                        args: vec![Arg::Pos(Single {
                                            expr: rhs,
                                            delim_span: None,
                                        })],
                                        delim: None,
                                    };
                                }
                            }
                            continue;
                        }
                        Some(token!(TokenInfo::Ellipsis)) => {
                            // Ellipsis `...` inside brackets - array literal with spread
                            // Example: `f[...a]` should be `f([...a])` not indexing
                            let rhs = self.parse_array_literal(scope, left, None)?;
                            match lhs {
                                Expr::Call {
                                    ref mut args,
                                    delim: None,
                                    ..
                                } => args.push(Arg::Pos(Single {
                                    expr: rhs,
                                    delim_span: None,
                                })),
                                _ => {
                                    lhs = Expr::Call {
                                        arg0: Box::new(lhs),
                                        args: vec![Arg::Pos(Single {
                                            expr: rhs,
                                            delim_span: None,
                                        })],
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
                                } => args.push(Arg::Pos(Single {
                                    expr: rhs,
                                    delim_span: None,
                                })),
                                _ => {
                                    lhs = Expr::Call {
                                        arg0: Box::new(lhs),
                                        args: vec![Arg::Pos(Single {
                                            expr: rhs,
                                            delim_span: None,
                                        })],
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
                                "invalid expression in index or array literal",
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
                    // Convert it into a regular call if possible, otherwise give up parsing
                    if let Expr::Call {
                        arg0,
                        mut args,
                        delim: Some(GroupDelim::Paren(paren_span)),
                    } = lhs
                    {
                        if mode != ExprMode::Compact
                            && args.len() == 1
                            && matches!(&args[0], Arg::Pos(..))
                        {
                            let arg0_span = arg0.span();
                            if arg0_span.end == paren_span.start {
                                juxta_warn = true;
                            }
                            let expr = match args.remove(0) {
                                Arg::Pos(Single { expr, .. }) => expr,
                                _ => unreachable!(),
                            };
                            lhs = Expr::Call {
                                arg0,
                                args: vec![Arg::Pos(Single {
                                    expr: Expr::Group {
                                        expr: Box::new(expr),
                                        delim: Some(GroupDelim::Paren(paren_span)),
                                    },
                                    delim_span: None,
                                })],
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
                            Arg::Key(Key {
                                key_span: token.span,
                                colon_span: token.span.before_left_char(),
                                expr: Expr::Ident(Ident::new(token.span)),
                                delim_span: None,
                            })
                        }
                        TokenInfo::Ellipsis => {
                            let token = self.consume();
                            let expr = self.parse_expr_prec(scope, mode, Some(prec))?;
                            Arg::Expand(Expand {
                                expr,
                                ellipsis_span: token.span,
                                delim_span: None,
                            })
                        }
                        TokenInfo::LeftParen if !matches!(lhs, Expr::Call { .. }) => {
                            let left = self.advance();
                            self.with_mode(Mode::FullExpr, |this| {
                                lhs = Expr::Call {
                                    arg0: Box::new(mem::replace(
                                        &mut lhs,
                                        Expr::Nil(Span::INVALID),
                                    )),
                                    args: this.parse_arg_pack(scope, None)?,
                                    delim: Some(GroupDelim::Paren(
                                        left | this.expect_matching(
                                            scope,
                                            ExpectKind::RightParen,
                                            left,
                                        ),
                                    )),
                                };
                                Ok(())
                            })?;
                            continue;
                        }
                        _ => Arg::Pos(Single {
                            expr: self.parse_expr_prec(scope, mode, Some(prec))?,
                            delim_span: None,
                        }),
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

        Ok(lhs)
    }

    fn special_method(&mut self, scope: &mut Scope<'_>, span: Span) -> Result<SpecialMethod> {
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
            "arg" => SpecialMethod::Arg,
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

    fn parse_expr(&mut self, scope: &mut Scope, mode: ExprMode) -> Result<Expr> {
        match mode {
            ExprMode::Shell => self.parse_expr_primary(scope, mode),
            ExprMode::Compact | ExprMode::Full => self.parse_expr_prec(scope, mode, None),
        }
    }

    fn parse_cmd_vert_line_expr(&mut self, scope: &mut Scope) -> Result<Expr> {
        use self::Keyword;
        use self::Op;
        use TokenInfo::*;
        match self.peek()? {
            Some(token!(expr_start!())) => {
                let expr = self.parse_expr(scope, ExprMode::Compact)?;
                if let Some(token!(Colon)) = self.peek()? {
                    let colon_span = self.advance();
                    let sep = self.expect(scope, &[ExpectKind::ArgSep])?;
                    self.add_indent(sep.end);
                    let arg = Arg::DynamicKey(Pair {
                        key: expr,
                        value: self.parse_cmd_vert_line_expr(scope)?,
                        colon_span: Some(colon_span),
                        delim_span: None,
                    });
                    // Parse any trailer
                    self.parse_data(scope, vec![arg])
                } else {
                    // Consume trailing whitespace
                    while let Some(token!(ArgSep)) = self.peek()? {
                        self.advance();
                    }
                    Ok(expr)
                }
            }
            Some(token!(Keyword(Keyword::Do))) => self.parse_cmd_or_expr(scope, true),
            Some(token!(Dollar)) => {
                let span = self.advance();
                if let Some(token!(ArgSep)) = self.peek()? {
                    self.advance();
                    Ok(Expr::Group {
                        expr: Box::new(self.parse_cmd_or_expr(scope, true)?),
                        delim: Some(GroupDelim::Dollar(span)),
                    })
                } else {
                    let expr = Expr::Group {
                        expr: Box::new(self.parse_expr(scope, ExprMode::Compact)?),
                        delim: Some(GroupDelim::Dollar(span)),
                    };
                    if matches!(self.peek()?, Some(token!(Dedent | StmtSep)) | None) {
                        Ok(expr)
                    } else {
                        self.parse_implicit_concat(scope, Some(expr), UnquotedMode::Data)
                    }
                }
            }
            Some(token!(TokenInfo::Op(Op::Bar))) => {
                let pipe_span = self.advance();
                let (intro_span, strip) = self.parse_heredoc_intro(pipe_span)?;
                if let Some(token!(Indent)) = self.peek()? {
                    self.advance();
                    self.parse_heredoc(scope, intro_span, strip, false)
                } else {
                    self.parse_implicit_concat(
                        scope,
                        Some(Expr::Literal(intro_span)),
                        UnquotedMode::Data,
                    )
                }
            }
            Some(token!(TokenInfo::RBar)) => {
                let rbar_span = self.advance();
                let (intro_span, strip) = self.parse_heredoc_intro(rbar_span)?;
                if let Some(token!(Indent)) = self.peek()? {
                    self.advance();
                    self.parse_heredoc(scope, intro_span, strip, true)
                } else {
                    self.parse_implicit_concat(
                        scope,
                        Some(Expr::Literal(intro_span)),
                        UnquotedMode::Data,
                    )
                }
            }
            _ => self.parse_implicit_concat(scope, None, UnquotedMode::Data),
        }
    }

    fn parse_cmd_vert_if(&mut self, scope: &mut Scope) -> Result<If<Vec<Arg>>> {
        use self::If;
        use self::IfBranch;
        use self::Keyword;
        use TokenInfo::*;

        let if_span = self.expect(scope, &[ExpectKind::Keyword(Keyword::If)])?;
        self.expect(scope, &[ExpectKind::ArgSep])?;
        let cond = self.parse_cmd_or_expr(scope, false)?;
        self.expect(scope, &[ExpectKind::Indent])?;
        let mut tbranch = Vec::new();
        loop {
            match self.peek()? {
                None | Some(token!(Dedent)) => break,
                Some(token!(StmtSep)) => {
                    self.advance();
                }
                _ => self.parse_cmd_vert_arg(scope, &mut tbranch)?,
            }
        }
        self.expect(scope, &[ExpectKind::Dedent])?;

        let mut elif_branches = Vec::new();
        let mut else_branch = None;

        while let Some(token!(Keyword(Keyword::Else))) = self.peek()? {
            let else_span = self.advance();

            if let Some(token!(ArgSep)) = self.peek()? {
                // This is "else if"
                self.advance();
                let elif_if_span = self.expect(scope, &[ExpectKind::Keyword(Keyword::If)])?;
                self.expect(scope, &[ExpectKind::ArgSep])?;
                let elif_cond = self.parse_cmd_or_expr(scope, false)?;
                self.expect(scope, &[ExpectKind::Indent])?;
                let mut elif_body = Vec::new();
                loop {
                    match self.peek()? {
                        None | Some(token!(Dedent)) => break,
                        Some(token!(StmtSep)) => {
                            self.advance();
                        }
                        _ => self.parse_cmd_vert_arg(scope, &mut elif_body)?,
                    }
                }
                self.expect(scope, &[ExpectKind::Dedent])?;

                elif_branches.push((
                    IfBranch {
                        span: elif_if_span,
                        cond: elif_cond,
                        body: elif_body,
                    },
                    else_span,
                ));
            } else {
                // This is final "else"
                self.expect(scope, &[ExpectKind::Indent])?;
                let mut else_body = Vec::new();
                loop {
                    match self.peek()? {
                        None | Some(token!(Dedent)) => break,
                        Some(token!(StmtSep)) => {
                            self.advance();
                        }
                        _ => self.parse_cmd_vert_arg(scope, &mut else_body)?,
                    }
                }
                self.expect(scope, &[ExpectKind::Dedent])?;

                else_branch = Some((else_body, else_span));
                break;
            }
        }

        Ok(If {
            tbranch: IfBranch {
                span: if_span,
                cond,
                body: tbranch,
            },
            elif_branches,
            else_branch,
        })
    }

    fn parse_cmd_vert_arg(&mut self, scope: &mut Scope, args: &mut Vec<Arg>) -> Result<()> {
        use self::{Ident, Key, Keyword, Op};
        use TokenInfo::*;

        match self.peek()? {
            Some(token!(Op(Op::Minus))) => {
                let minus_span = self.advance();
                if let Some(token!(ArgSep)) = self.peek()? {
                    args.push(self.parse_cmd_vert_dash_arg(scope, minus_span)?);
                } else {
                    args.push(Arg::Pos(Single {
                        expr: self.parse_implicit_concat(
                            scope,
                            Some(Expr::Literal(minus_span)),
                            UnquotedMode::Shell,
                        )?,
                        delim_span: None,
                    }));
                    self.parse_cmd_args(scope, false, false, args)?;
                }
                Ok(())
            }
            Some(token!(Key)) => {
                let span = self.advance();
                if let Some(token!(Indent)) = self.peek()? {
                    self.advance();
                    args.push(Arg::Key(Key {
                        key_span: span,
                        colon_span: span.after_right_char(),
                        expr: self.parse_data(scope, vec![])?,
                        delim_span: None,
                    }));
                } else {
                    self.expect(scope, &[ExpectKind::ArgSep])?;
                    if let Some(token!(Keyword(Keyword::Do))) = self.peek()? {
                        args.push(Arg::Key(Key {
                            key_span: span,
                            colon_span: span.after_right_char(),
                            expr: self.parse_do_block(scope, true)?,
                            delim_span: None,
                        }));
                    } else {
                        args.push(Arg::Key(Key {
                            key_span: span,
                            colon_span: span.after_right_char(),
                            delim_span: None,
                            expr: self.parse_cmd_vert_line_expr(scope)?,
                        }))
                    }
                };
                Ok(())
            }
            Some(token!(DittoKey)) => {
                let span = self.advance();
                args.push(Arg::Key(Key {
                    key_span: span,
                    colon_span: span.before_left_char(),
                    expr: Expr::Ident(Ident::new(span)),
                    delim_span: None,
                }));
                Ok(())
            }
            Some(token!(Ellipsis)) => {
                let ellipsis_span = self.advance();
                let expr = self.parse_expr(scope, ExprMode::Compact)?;
                args.push(Arg::Expand(Expand {
                    expr,
                    ellipsis_span,
                    delim_span: None,
                }));
                Ok(())
            }
            Some(token!(Keyword(Keyword::If))) => {
                args.push(Arg::If(self.parse_cmd_vert_if(scope)?));
                Ok(())
            }
            Some(token!(Keyword(Keyword::For))) => {
                let for_span = self.advance();
                self.expect(scope, &[ExpectKind::ArgSep])?;
                // Parse bind pattern
                let bind = self.parse_pattern(scope, false)?;
                // Parse optional "= expr"
                let (expr, equal_span) = if let Some(token!(Equal)) = self.peek()? {
                    let equal_span = self.advance();
                    self.expect(scope, &[ExpectKind::ArgSep])?;
                    (
                        Some(self.parse_cmd_or_expr(scope, false)?),
                        Some(equal_span),
                    )
                } else {
                    (None, None)
                };
                // Expect indentation for body
                self.expect(scope, &[ExpectKind::Indent])?;
                let mut body = Vec::new();
                loop {
                    match self.peek()? {
                        None | Some(token!(Dedent)) => break,
                        Some(token!(StmtSep)) => {
                            self.advance();
                        }
                        _ => self.parse_cmd_vert_arg(scope, &mut body)?,
                    }
                }
                self.expect(scope, &[ExpectKind::Dedent])?;
                args.push(Arg::For(For {
                    bind,
                    expr,
                    body: ExprBody {
                        elems: body,
                        vars: Vec::new(),
                    },
                    iter: None, // Will be filled by resolver
                    for_span,
                    equal_span,
                }));
                Ok(())
            }
            Some(token!(expr_start!())) => {
                let key = self.parse_expr(scope, ExprMode::Shell)?;
                if let Some(token!(Colon)) = self.peek()? {
                    let colon_span = self.advance();
                    let value = if let Some(token!(Indent)) = self.peek()? {
                        self.advance();
                        self.parse_data(scope, vec![])?
                    } else {
                        self.expect(scope, &[ExpectKind::ArgSep])?;
                        self.parse_cmd_vert_line_expr(scope)?
                    };
                    args.push(Arg::DynamicKey(Pair {
                        key,
                        value,
                        colon_span: Some(colon_span),
                        delim_span: None,
                    }));
                } else {
                    args.push(Arg::Pos(Single {
                        expr: self.parse_implicit_concat(scope, Some(key), UnquotedMode::Shell)?,
                        delim_span: None,
                    }));
                    self.parse_cmd_args(scope, false, false, args)?;
                    return Ok(());
                }
                Ok(())
            }
            Some(token!(Dollar)) => {
                self.advance();
                let key = self.parse_expr(scope, ExprMode::Compact)?;
                if let Some(token!(Colon)) = self.peek()? {
                    let colon_span = self.advance();
                    let value = if let Some(token!(Indent)) = self.peek()? {
                        self.advance();
                        self.parse_data(scope, vec![])?
                    } else {
                        self.expect(scope, &[ExpectKind::ArgSep])?;
                        self.parse_cmd_vert_line_expr(scope)?
                    };
                    args.push(Arg::DynamicKey(Pair {
                        key,
                        value,
                        colon_span: Some(colon_span),
                        delim_span: None,
                    }));
                } else {
                    args.push(Arg::Pos(Single {
                        expr: self.parse_implicit_concat(scope, Some(key), UnquotedMode::Shell)?,
                        delim_span: None,
                    }));
                    self.parse_cmd_args(scope, false, false, args)?;
                };
                Ok(())
            }
            Some(token!(Keyword(Keyword::Do))) => {
                args.push(Arg::Pos(Single {
                    expr: self.parse_do_block(scope, true)?,
                    delim_span: None,
                }));
                Ok(())
            }
            _ => self.parse_cmd_args(scope, false, false, args),
        }
    }

    fn parse_cmd_vert_dash_arg(&mut self, scope: &mut Scope<'_>, minus_span: Span) -> Result<Arg> {
        use self::{Key, Op};
        use TokenInfo::*;
        let sep = self.expect(scope, &[ExpectKind::ArgSep])?;
        self.add_indent(sep.end);
        let expr = match self.peek()? {
            Some(token!(Op(Op::Minus))) => {
                let minus_span = self.advance();
                // Decide if this is actually a sub-list
                match self.peek()? {
                    Some(token!(ArgSep)) => {
                        // Yep
                        let args = vec![self.parse_cmd_vert_dash_arg(scope, minus_span)?];
                        let expr = self.parse_data(scope, args)?;
                        // This consumed remainder of this indentation scope, so exit early
                        return Ok(Arg::Pos(Single {
                            expr,
                            delim_span: Some(minus_span),
                        }));
                    }
                    _ => {
                        // Nevermind
                        self.parse_implicit_concat(
                            scope,
                            Some(Expr::Literal(minus_span)),
                            UnquotedMode::Data,
                        )?
                    }
                }
            }
            Some(token!(Key)) => {
                let span = self.advance();
                if let Some(token!(ArgSep)) = self.peek()? {
                    self.advance();
                    // Parse first item on this line
                    let arg = Arg::Key(Key {
                        expr: self.parse_cmd_vert_line_expr(scope)?,
                        key_span: span,
                        colon_span: span.after_right_char(),
                        delim_span: None,
                    });
                    // Parse any trailer
                    let expr = self.parse_data(scope, vec![arg])?;
                    // This consumed remainder of this indentation scope, so exit early
                    return Ok(Arg::Pos(Single {
                        expr,
                        delim_span: Some(minus_span),
                    }));
                } else {
                    self.parse_implicit_concat(
                        scope,
                        Some(Expr::Literal(span | span.after_right_char())),
                        UnquotedMode::Data,
                    )?
                }
            }
            _ => self.parse_cmd_vert_line_expr(scope)?,
        };
        self.expect(scope, &[ExpectKind::Dedent])?;
        Ok(Arg::Pos(Single {
            expr,
            delim_span: Some(minus_span),
        }))
    }

    fn parse_cmd_vert_args(
        &mut self,
        scope: &mut Scope,
        args: &mut Vec<Arg>,
        bin_pack: bool,
    ) -> Result<()> {
        use self::Keyword;
        use Keyword::*;
        use TokenInfo::*;

        loop {
            let res = (|| -> Result<bool> {
                match self.peek()? {
                    None | Some(token!(Dedent)) => return Ok(true),
                    token @ Some(token!(Indent)) => {
                        if bin_pack {
                            return Ok(true);
                        } else {
                            let token = token.cloned();
                            return Err(self.syntax_error(
                                scope,
                                token,
                                "unexpected indent in vertical data",
                            ));
                        }
                    }
                    Some(token!(StmtSep)) => {
                        self.advance();
                    }
                    _ => self.parse_cmd_vert_arg(scope, args)?,
                }
                Ok(false)
            })();
            match res {
                Ok(true) => break,
                Ok(false) => continue,
                Err(_) => {
                    // Destined to fail
                    self.lex.set_error();
                    // Try to resynchronize with token stream
                    while !matches!(
                        self.peek()?,
                        None | Some(token!(StmtSep | Keyword(Do) | Dedent))
                    ) {
                        self.advance();
                    }
                }
            }
        }

        self.expect(scope, &[ExpectKind::Dedent, ExpectKind::End])?;

        Ok(())
    }

    fn parse_data(&mut self, scope: &mut Scope, mut args: Vec<Arg>) -> Result<Expr> {
        self.parse_cmd_vert_args(scope, &mut args, false)?;

        // Decide what to emit by recursively analyzing the args
        if self.args_have_key(&args) {
            // Has keys somewhere, build a dict
            Ok(Expr::Dict {
                elems: args.into_iter().map(Self::arg_to_dict_elem).collect(),
                brace_span: None,
            })
        } else {
            // All positional (or for-loops that only contain positional), build an array
            Ok(Expr::Array {
                elems: args.into_iter().map(Self::arg_to_array_elem).collect(),
                bracket_span: None,
            })
        }
    }

    fn args_have_key(&self, args: &[Arg]) -> bool {
        for arg in args {
            match arg {
                Arg::Pos(..) | Arg::Expand { .. } => continue, // array elements
                Arg::Key(Key { .. }) | Arg::DynamicKey { .. } => return true, // dict elements
                Arg::For(For { body, .. }) => {
                    // Recursively analyze the for-body
                    if self.args_have_key(&body.elems) {
                        return true;
                    }
                }
                Arg::If(node) => {
                    // Check first if branch
                    if self.args_have_key(&node.tbranch.body) {
                        return true;
                    }

                    // Check elif branches
                    for (elif_branch, _) in &node.elif_branches {
                        if self.args_have_key(&elif_branch.body) {
                            return true;
                        }
                    }

                    // Check final else branch
                    if let Some((else_body, _)) = &node.else_branch
                        && self.args_have_key(else_body)
                    {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn arg_to_array_elem(arg: Arg) -> ArrayElem {
        match arg {
            Arg::Pos(single) => ArrayElem::Single(single),
            Arg::Expand(expand) => ArrayElem::Expand(expand),
            Arg::If(node) => ArrayElem::If(
                node.map(&mut |args| args.into_iter().map(Self::arg_to_array_elem).collect()),
            ),
            Arg::For(node) => ArrayElem::For(node.map(&mut |body| {
                ExprBody {
                    elems: body
                        .elems
                        .into_iter()
                        .map(Self::arg_to_array_elem)
                        .collect(),
                    vars: body.vars,
                }
            })),
            _ => unreachable!("Unexpected arg type in array conversion"),
        }
    }

    fn arg_to_dict_elem(arg: Arg) -> DictElem {
        match arg {
            Arg::Pos(single) => DictElem::Single(single),
            Arg::Key(node) => DictElem::Key(node),
            Arg::DynamicKey(node) => DictElem::Pair(node),
            Arg::Expand(node) => DictElem::Expand(node),
            Arg::If(node) => DictElem::If(
                node.map(&mut |args| args.into_iter().map(Self::arg_to_dict_elem).collect()),
            ),
            Arg::For(node) => DictElem::For(node.map(&mut |body| ExprBody {
                elems: body.elems.into_iter().map(Self::arg_to_dict_elem).collect(),
                vars: body.vars,
            })),
        }
    }

    fn parse_rhs(&mut self, scope: &mut Scope) -> Result<PrimStmt> {
        if let Some(token!(TokenInfo::Indent)) = self.peek()? {
            self.advance();
            let res = PrimStmt::Expr(self.parse_data(scope, vec![])?);
            Ok(res)
        } else {
            self.expect(scope, &[ExpectKind::ArgSep])?;
            match self.peek()? {
                Some(token!(TokenInfo::Keyword(Keyword::If))) => {
                    Ok(PrimStmt::If(self.parse_if(scope)?))
                }
                Some(token!(TokenInfo::Keyword(Keyword::Try))) => {
                    Ok(PrimStmt::Try(self.parse_try(scope)?))
                }
                _ => Ok(PrimStmt::Expr(self.parse_cmd_or_expr(scope, true)?)),
            }
        }
    }

    fn parse_pattern(&mut self, scope: &mut Scope, vertical: bool) -> Result<Pattern> {
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
                    Param::Pos { ident, .. } => Pattern::Ident(ident),
                    _ => unreachable!(),
                },
                _ => Pattern::Unpack(params),
            },
            _ => Pattern::Unpack(params),
        })
    }

    fn parse_let(&mut self, scope: &mut Scope, pub_span: Option<Span>) -> Result<Let> {
        let let_span = self.expect(scope, &[ExpectKind::Keyword(Keyword::Let)])?;
        self.expect(scope, &[ExpectKind::ArgSep])?;
        let bind = self.parse_pattern(scope, false)?;
        let equal_span = self.expect(scope, &[ExpectKind::Equal])?;
        let rhs = self.parse_rhs(scope)?;
        Ok(Let {
            bind,
            rhs,
            let_span,
            equal_span,
            pub_span,
        })
    }

    fn parse_bind(&mut self, scope: &mut Scope) -> Result<Bind> {
        let bind_span = self.expect(scope, &[ExpectKind::Keyword(Keyword::Bind)])?;
        self.expect(scope, &[ExpectKind::ArgSep])?;
        let expr = self.parse_cmd_or_expr(scope, false)?;
        let bind = self.parse_pattern(scope, true)?;
        Ok(Bind {
            bind,
            expr,
            bind_span,
        })
    }

    fn parse_if(&mut self, scope: &mut Scope) -> Result<If<Block>> {
        use self::{If, IfBranch, Keyword};
        use Keyword::*;
        use TokenInfo::*;

        let if_span = self.expect(scope, &[ExpectKind::Keyword(If)])?;
        self.expect(scope, &[ExpectKind::ArgSep])?;
        let cond = self.parse_cmd_or_expr(scope, false)?;
        self.expect(scope, &[ExpectKind::Indent])?;
        let tbranch = self.parse_block(scope)?;
        self.expect(scope, &[ExpectKind::Dedent])?;

        let mut elif_branches = Vec::new();
        let mut else_branch = None;

        while let Some(token!(Keyword(Else))) = self.peek()? {
            let else_span = self.advance();

            if let Some(token!(ArgSep)) = self.peek()? {
                // This is "else if"
                self.advance();
                let elif_if_span = self.expect(scope, &[ExpectKind::Keyword(If)])?;
                self.expect(scope, &[ExpectKind::ArgSep])?;
                let elif_cond = self.parse_cmd_or_expr(scope, false)?;
                self.expect(scope, &[ExpectKind::Indent])?;
                let elif_body = self.parse_block(scope)?;
                self.expect(scope, &[ExpectKind::Dedent])?;

                elif_branches.push((
                    IfBranch {
                        span: elif_if_span,
                        cond: elif_cond,
                        body: elif_body,
                    },
                    else_span,
                ));
            } else {
                // This is final "else"
                self.expect(scope, &[ExpectKind::Indent])?;
                let else_body = self.parse_block(scope)?;
                self.expect(scope, &[ExpectKind::Dedent])?;

                else_branch = Some((else_body, else_span));
                break;
            }
        }

        Ok(If {
            tbranch: IfBranch {
                span: if_span,
                cond,
                body: tbranch,
            },
            elif_branches,
            else_branch,
        })
    }

    fn parse_try(&mut self, scope: &mut Scope) -> Result<Try> {
        use self::{Ident, Keyword, Try};
        use Keyword::*;
        use TokenInfo::*;

        let try_span = self.expect(scope, &[ExpectKind::Keyword(Keyword::Try)])?;
        self.expect(scope, &[ExpectKind::Indent])?;
        let body_block = self.parse_block(scope)?;
        self.expect(scope, &[ExpectKind::Dedent])?;

        let body = Function {
            params: vec![],
            body: body_block,
        };

        let mut handlers = Vec::new();
        let mut has_catch_all = false;

        while let Some(token!(Keyword(Catch))) = self.peek()? {
            let catch_span = self.advance();

            self.expect(scope, &[ExpectKind::ArgSep])?;

            if has_catch_all {
                self.fail = true;
                self.diags.push(SyntaxDiag::new(
                    catch_span,
                    "catch-all handler must be last",
                ));
                return Err(Error);
            }

            // Parse a compact expression, then decide based on whether a colon follows
            let expr = self.parse_expr(scope, ExprMode::Compact)?;

            if let Some(token!(Colon)) = self.peek()? {
                // Typed catch: <class_expr>: <var>
                self.advance();
                self.expect(scope, &[ExpectKind::ArgSep])?;
                let var_span = self.expect(scope, &[ExpectKind::Ident])?;
                self.expect(scope, &[ExpectKind::Indent])?;
                let catch_block = self.parse_block(scope)?;
                self.expect(scope, &[ExpectKind::Dedent])?;
                handlers.push(CatchHandler {
                    class_expr: Some(expr),
                    func: Function {
                        params: vec![Param::Pos {
                            ident: Ident::new(var_span),
                            default: None,
                        }],
                        body: catch_block,
                    },
                    catch_span,
                });
            } else {
                // Catch-all: expression must be a plain identifier
                let var_span = match expr {
                    Expr::Ident(ident) => ident.span,
                    other => {
                        self.fail = true;
                        self.diags.push(SyntaxDiag::new(
                            other.span(),
                            "catch-all expects a plain identifier",
                        ));
                        return Err(Error);
                    }
                };
                has_catch_all = true;
                self.expect(scope, &[ExpectKind::Indent])?;
                let catch_block = self.parse_block(scope)?;
                self.expect(scope, &[ExpectKind::Dedent])?;
                handlers.push(CatchHandler {
                    class_expr: None,
                    func: Function {
                        params: vec![Param::Pos {
                            ident: Ident::new(var_span),
                            default: None,
                        }],
                        body: catch_block,
                    },
                    catch_span,
                });
            }
        }

        // Parse optional finally
        let finally = if let Some(token!(TokenInfo::Keyword(Keyword::Finally))) = self.peek()? {
            let finally_span = self.advance();
            self.expect(scope, &[ExpectKind::Indent])?;
            let finally_block = self.parse_block(scope)?;
            self.expect(scope, &[ExpectKind::Dedent])?;
            Some((
                Function {
                    params: vec![],
                    body: finally_block,
                },
                finally_span,
            ))
        } else {
            None
        };

        Ok(Try {
            body,
            handlers,
            finally,
            try_span,
        })
    }

    fn parse_while(&mut self, scope: &mut Scope) -> Result<Stmt> {
        let while_span = self.advance();
        self.expect(scope, &[ExpectKind::ArgSep])?;
        let cond = self.parse_cmd_or_expr(scope, false)?;
        self.expect(scope, &[ExpectKind::Indent])?;
        let body = self.parse_block(scope)?;
        self.expect(scope, &[ExpectKind::Dedent])?;
        Ok(Stmt::While(While {
            cond,
            body,
            while_span,
        }))
    }

    fn parse_for(&mut self, scope: &mut Scope) -> Result<Stmt> {
        let for_span = self.expect(scope, &[ExpectKind::Keyword(Keyword::For)])?;
        self.expect(scope, &[ExpectKind::ArgSep])?;
        let bind = self.parse_pattern(scope, false)?;
        let (equal_span, expr) = match self.next()? {
            Some(token!(TokenInfo::Equal, equal_span)) => {
                self.expect(scope, &[ExpectKind::ArgSep])?;
                let expr = self.parse_cmd_or_expr(scope, false)?;
                self.expect(scope, &[ExpectKind::Indent])?;
                (Some(equal_span), Some(expr))
            }
            Some(token!(TokenInfo::Indent)) => (None, None),
            other => {
                return Err(self.syntax_error(
                    scope,
                    other,
                    "expected `=` or indent after `for` pattern",
                ));
            }
        };
        let body = self.parse_block(scope)?;
        self.expect(scope, &[ExpectKind::Dedent])?;
        Ok(Stmt::For(For {
            bind,
            expr,
            body,
            iter: None,
            for_span,
            equal_span,
        }))
    }

    fn parse_do_params(&mut self, scope: &mut Scope) -> Result<Vec<Param>> {
        match self.peek()? {
            Some(token!(TokenInfo::Indent)) => return Ok(vec![]),
            Some(token!(TokenInfo::ArgSep)) => self.advance(),
            _ => {
                let token = self.next()?;
                return Err(self.syntax_error(
                    scope,
                    token,
                    "expected parameters, statement or indent after `do`",
                ));
            }
        };
        match self.peek()? {
            Some(token!(TokenInfo::Op(Op::Bar))) => {
                self.advance();
                let params = self.parse_params(scope, ParamMode::HorizFunc)?;
                self.expect(scope, &[ExpectKind::Op(Op::Bar)])?;
                if let Some(token!(TokenInfo::ArgSep)) = self.peek()? {
                    self.advance();
                }
                Ok(params)
            }
            _ => Ok(vec![]),
        }
    }

    fn parse_do_block(&mut self, scope: &mut Scope, allow_trailing: bool) -> Result<Expr> {
        let do_span = self.expect(scope, &[ExpectKind::Keyword(Keyword::Do)])?;
        let params = self.parse_do_params(scope)?;
        match self.peek()? {
            Some(token!(TokenInfo::Indent)) if allow_trailing => {
                self.advance();
                let function = Function {
                    params,
                    body: self.parse_block(scope)?,
                };
                self.expect(scope, &[ExpectKind::Dedent])?;
                Ok(Expr::Lambda {
                    func: function,
                    do_span: Some(do_span),
                })
            }
            _ => Ok(Expr::Lambda {
                func: Function {
                    params,
                    body: Block {
                        stmts: vec![if allow_trailing {
                            self.parse_stmt(scope)?
                        } else {
                            Stmt::Prim(self.parse_cmd(scope, allow_trailing)?)
                        }],
                        vars: Default::default(),
                        repl: None,
                    },
                },
                do_span: Some(do_span),
            }),
        }
    }

    fn parse_cmd_arg0(&mut self, scope: &mut Scope) -> Result<Expr> {
        self.parse_expr(scope, ExprMode::Compact)
    }

    fn parse_cmd_arg_expr(
        &mut self,
        scope: &mut Scope,
        allow_trailing: bool,
    ) -> Result<(Expr, bool)> {
        use self::{Keyword, Op};
        use TokenInfo::*;

        match self.peek()? {
            Some(token!(Dollar)) => {
                let span = self.advance();
                match self.peek()? {
                    Some(token!(ArgSep)) => {
                        self.advance();
                        return Ok((
                            Expr::Group {
                                expr: Box::new(self.parse_cmd_or_expr(scope, allow_trailing)?),
                                delim: Some(GroupDelim::Dollar(span)),
                            },
                            // This consumed the rest of the statement
                            true,
                        ));
                    }
                    Some(token!(Indent)) if allow_trailing => {
                        self.advance();
                        return Ok((
                            Expr::Group {
                                expr: Box::new(self.parse_data(scope, vec![])?),
                                delim: Some(GroupDelim::Dollar(span)),
                            },
                            // This consumed the rest of the statement
                            true,
                        ));
                    }
                    _ => (),
                };
                let expr = self.parse_expr(scope, ExprMode::Compact)?;
                Ok((
                    Expr::Group {
                        expr: Box::new(self.parse_implicit_concat(
                            scope,
                            Some(expr),
                            UnquotedMode::Shell,
                        )?),
                        delim: Some(GroupDelim::Dollar(span)),
                    },
                    false,
                ))
            }
            Some(token!(Keyword(Keyword::Do))) => {
                Ok((self.parse_do_block(scope, allow_trailing)?, true))
            }
            Some(token!(Op(Op::Bar))) => {
                let pipe_span = self.advance();
                let (intro_span, strip) = self.parse_heredoc_intro(pipe_span)?;
                if let Some(token!(Indent)) = self.peek()? {
                    self.advance();
                    return Ok((self.parse_heredoc(scope, intro_span, strip, false)?, true));
                }
                Ok((
                    self.parse_implicit_concat(
                        scope,
                        Some(Expr::Literal(intro_span)),
                        UnquotedMode::Shell,
                    )?,
                    false,
                ))
            }
            Some(token!(RBar)) => {
                let rbar_span = self.advance();
                let (intro_span, strip) = self.parse_heredoc_intro(rbar_span)?;
                if let Some(token!(Indent)) = self.peek()? {
                    self.advance();
                    return Ok((self.parse_heredoc(scope, intro_span, strip, true)?, true));
                }
                Ok((
                    self.parse_implicit_concat(
                        scope,
                        Some(Expr::Literal(intro_span)),
                        UnquotedMode::Shell,
                    )?,
                    false,
                ))
            }
            Some(token!(LeftParen | LeftBracket | LeftBrace | DQuote | RawQuote | BQuote)) => {
                let expr = self.parse_expr(scope, ExprMode::Shell)?;
                if !matches!(
                    self.peek()?,
                    None | Some(token!(Indent | Dedent | ArgSep | StmtSep))
                ) {
                    let token = self.consume();
                    self.fail = true;
                    self.diags.push(ImplicitDelimitedConcat {
                        span: token.span,
                        insert: expr.span(),
                    });
                    return Err(Error);
                }
                Ok((expr, false))
            }
            _ => Ok((
                self.parse_implicit_concat(scope, None, UnquotedMode::Shell)?,
                false,
            )),
        }
    }

    fn parse_cmd_arg(
        &mut self,
        scope: &mut Scope,
        allow_keys: bool,
        allow_trailing: bool,
        args: &mut Vec<Arg>,
    ) -> Result<bool> {
        use self::{Ident, Key};
        use TokenInfo::*;

        match self.peek()? {
            None | Some(token!(StmtSep | Dedent)) => Ok(true),
            Some(token!(ArgSep)) => {
                self.advance();
                Ok(false)
            }
            Some(token!(Indent)) => {
                if allow_trailing {
                    self.advance();
                    self.parse_cmd_vert_args(scope, args, true)?;
                }
                Ok(true)
            }
            Some(token!(Key, span)) if allow_keys => {
                let span = *span;
                self.advance();
                let (expr, consumed) = match self.peek()? {
                    Some(token!(Indent)) if allow_trailing => {
                        self.advance();
                        (self.parse_data(scope, vec![])?, true)
                    }
                    Some(token!(ArgSep)) => {
                        self.advance();
                        self.parse_cmd_arg_expr(scope, allow_trailing)?
                    }
                    _ => {
                        args.push(Arg::Pos(Single {
                            expr: self.parse_implicit_concat(
                                scope,
                                Some(Expr::Concat {
                                    exprs: vec![Expr::Literal(span | span.after_right_char())],
                                    delim_span: None,
                                    arg: true,
                                }),
                                UnquotedMode::Shell,
                            )?,
                            delim_span: None,
                        }));
                        return Ok(false);
                    }
                };
                args.push(Arg::Key(Key {
                    key_span: span,
                    colon_span: span.after_right_char(),
                    expr,
                    delim_span: None,
                }));
                Ok(consumed)
            }
            Some(token!(DittoKey)) => {
                let span = self.advance();
                args.push(Arg::Key(Key {
                    key_span: span,
                    colon_span: span.before_left_char(),
                    expr: Expr::Ident(Ident::new(span)),
                    delim_span: None,
                }));
                Ok(false)
            }
            Some(token!(Ellipsis)) => {
                let ellipsis_span = self.advance();
                let expr = self.parse_expr(scope, ExprMode::Compact)?;
                args.push(Arg::Expand(Expand {
                    expr,
                    ellipsis_span,
                    delim_span: None,
                }));
                Ok(false)
            }
            _ => {
                let (expr, consumed) = self.parse_cmd_arg_expr(scope, allow_trailing)?;
                args.push(Arg::Pos(Single {
                    expr,
                    delim_span: None,
                }));
                Ok(consumed)
            }
        }
    }

    fn parse_cmd_args(
        &mut self,
        scope: &mut Scope,
        allow_keys: bool,
        allow_trailing: bool,
        args: &mut Vec<Arg>,
    ) -> Result<()> {
        while !self.parse_cmd_arg(scope, allow_keys, allow_trailing, args)? {}

        Ok(())
    }

    fn parse_cmd(&mut self, scope: &mut Scope, allow_trailing: bool) -> Result<PrimStmt> {
        use TokenInfo::*;

        let arg0 = self.parse_cmd_arg0(scope)?;
        if let Some(token!(ArgSep)) = self.peek()? {
            self.advance();
        }
        let mut args = vec![];
        Ok(match self.peek()? {
            Some(token!(Equal)) => {
                // Assignment is now handled at the statement level
                // For RHS parsing, just parse the expression
                self.parse_cmd_args(scope, true, allow_trailing, &mut args)?;
                PrimStmt::Expr(if args.is_empty() {
                    arg0
                } else {
                    Expr::Call {
                        arg0: Box::new(arg0),
                        args,
                        delim: None,
                    }
                })
            }
            _ => {
                self.parse_cmd_args(scope, true, allow_trailing, &mut args)?;

                PrimStmt::Expr(if args.is_empty() {
                    arg0
                } else {
                    Expr::Call {
                        arg0: Box::new(arg0),
                        args,
                        delim: None,
                    }
                })
            }
        })
    }

    fn parse_cmd_or_expr(&mut self, scope: &mut Scope, allow_trailing: bool) -> Result<Expr> {
        use self::Op;
        match self.peek()? {
            Some(token!(TokenInfo::Keyword(Keyword::Do))) => {
                return self.parse_do_block(scope, allow_trailing);
            }
            Some(token!(TokenInfo::Op(Op::Bar))) if allow_trailing => {
                let pipe_span = self.advance();
                let (intro_span, strip) = self.parse_heredoc_intro(pipe_span)?;
                self.expect(scope, &[ExpectKind::Indent])?;
                return self.parse_heredoc(scope, intro_span, strip, false);
            }
            Some(token!(TokenInfo::RBar)) if allow_trailing => {
                let rbar_span = self.advance();
                let (intro_span, strip) = self.parse_heredoc_intro(rbar_span)?;
                self.expect(scope, &[ExpectKind::Indent])?;
                return self.parse_heredoc(scope, intro_span, strip, true);
            }
            _ => {}
        }

        let arg0 = self.parse_cmd_arg0(scope)?;
        let mut args = vec![];
        self.parse_cmd_args(scope, true, allow_trailing, &mut args)?;

        Ok(if args.is_empty() {
            arg0
        } else {
            Expr::Call {
                arg0: Box::new(arg0),
                args,
                delim: None,
            }
        })
    }

    fn parse_params(&mut self, scope: &mut Scope, mode: ParamMode) -> Result<Vec<Param>> {
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
                token @ Some(token!(TokenInfo::Dedent)) if mode.is_vertical() => {
                    if params.is_empty() {
                        let token = token.cloned();
                        return Err(self.syntax_error(
                            scope,
                            token,
                            "expected at least one item in pattern",
                        ));
                    }
                    self.advance();
                    if matches!(mode, ParamMode::VertFunc) {
                        self.expect(scope, &[ExpectKind::Keyword(Keyword::Do)])?;
                        self.expect(scope, &[ExpectKind::Indent])?;
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
                    let default = self.parse_param_default(scope, mode)?;
                    params.push(Param::Key {
                        key_span: key,
                        ident: Ident::new(ident_span),
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
                    let default = self.parse_param_default(scope, mode)?;
                    params.push(Param::Key {
                        key_span: key,
                        ident: Ident::new(key),
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
                    let default = self.parse_param_default(scope, mode)?;
                    if default.is_some() {
                        seen_optional = true;
                    } else if seen_optional {
                        self.fail = true;
                        self.diags.push(RequiredAfterOptional(span));
                    }
                    params.push(Param::Pos {
                        ident: Ident::new(span),
                        default,
                    })
                }
                Some(token!(TokenInfo::Ellipsis)) => {
                    if variadic {
                        let token = self.peek()?.cloned();
                        return Err(self.syntax_error(scope, token, "duplicate rest parameter"));
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
                            let token = next_token.cloned();
                            return Err(self.syntax_error(
                                scope,
                                token,
                                "expected identifier or whitespace after '...'",
                            ));
                        }
                    };

                    params.push(Param::Rest {
                        ellipsis_span,
                        ident,
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

                    let default = if matches!(mode, ParamMode::VertPattern) {
                        self.parse_param_default(scope, mode)?
                    } else {
                        None
                    };

                    params.push(Param::ConstKey {
                        key_expr,
                        key_const,
                        ident: Ident::new(ident_span),
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
                        let default = self.parse_param_default(scope, mode)?;
                        if default.is_some() {
                            seen_optional = true;
                        } else if seen_optional {
                            self.fail = true;
                            self.diags.push(RequiredAfterOptional(span));
                        }
                        params.push(Param::Pos {
                            ident: Ident::new(span),
                            default,
                        })
                    }
                    _ => {
                        if matches!(mode, ParamMode::VertFunc) {
                            break Ok(params);
                        }
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
                    let expr = self.parse_expr_primary(scope, ExprMode::Compact)?;
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

    fn parse_decorators(&mut self, scope: &mut Scope) -> Result<Vec<Decorator>> {
        let mut decorators = Vec::new();
        while let Some(token!(TokenInfo::DecoratorOpen)) = self.peek()? {
            let open_span = self.expect(scope, &[ExpectKind::DecoratorOpen])?;
            let expr = self.with_mode(lex::Mode::FullExpr, |this| {
                let expr = this.parse_expr(scope, ExprMode::Full)?;
                let close_span = this.expect_matching(scope, ExpectKind::RightBracket, open_span);
                Ok(Decorator {
                    open_span,
                    expr,
                    close_span,
                })
            })?;
            decorators.push(expr);
            self.expect(scope, &[ExpectKind::StmtSep])?;
        }
        Ok(decorators)
    }

    fn parse_def(
        &mut self,
        scope: &mut Scope,
        pub_span: Option<Span>,
        decorators: Vec<Decorator>,
    ) -> Result<Def> {
        let def_span = self.expect(scope, &[ExpectKind::Keyword(Keyword::Def)])?;
        self.expect(scope, &[ExpectKind::ArgSep])?;
        let variant = match self.next()? {
            Some(token!(TokenInfo::LeftParen)) => {
                let span = self.expect(scope, &[ExpectKind::Ident])?;
                self.expect(scope, &[ExpectKind::RightParen])?;
                DefVariant::Special(self.special_method(scope, span)?, span, None)
            }
            Some(token!(TokenInfo::Ident, span)) => DefVariant::Normal(Ident::new(span)),
            token => {
                return Err(self.syntax_error(scope, token, "expected function or special method"));
            }
        };
        let params = match self.peek()? {
            Some(token!(TokenInfo::Indent)) => self.parse_params(scope, ParamMode::VertFunc)?,
            Some(token!(TokenInfo::LeftParen)) => {
                let left = self.advance();
                let _right = self.expect_matching(scope, ExpectKind::RightParen, left);
                self.expect(scope, &[ExpectKind::Indent])?;
                // FIXME: include paren spans somewhere
                vec![]
            }
            _ => {
                let params = self.parse_params(scope, ParamMode::HorizFunc)?;
                self.expect(scope, &[ExpectKind::Indent])?;
                params
            }
        };
        let body = self.parse_block(scope)?;
        self.expect(scope, &[ExpectKind::Dedent])?;
        Ok(Def {
            def_span,
            decorators,
            variant,
            func: Function { params, body },
            pub_span,
        })
    }

    fn parse_field_into(
        &mut self,
        scope: &mut Scope,
        pub_span: Option<Span>,
        out: &mut Vec<Stmt>,
    ) -> Result<()> {
        use TokenInfo::*;

        let field_span = self.expect(scope, &[ExpectKind::Keyword(self::Keyword::Field)])?;
        self.expect(scope, &[ExpectKind::ArgSep])?;

        let mut fields = Vec::new();

        loop {
            match decay_ident!(self.next()?) {
                Some(token!(Ident, span)) => fields.push(span),
                other => return Err(self.syntax_error(scope, other, "expected field name")),
            }

            if let Some(token!(ArgSep)) = self.peek()? {
                self.advance();
            }

            match self.peek()? {
                Some(token!(Equal)) | None | Some(token!(StmtSep | Dedent)) => break,
                _ => continue,
            }
        }

        let rhs = if let Some(token!(Equal)) = self.peek()? {
            if fields.len() > 1 {
                let token = self.next()?;
                return Err(self.syntax_error(scope, token, "multiple field names cannot use `=`"));
            }
            self.expect(scope, &[ExpectKind::Equal])?;
            self.expect(scope, &[ExpectKind::ArgSep])?;
            let (expr, _) = self.parse_expr_const(scope, ExprMode::Compact)?;
            Some(PrimStmt::Expr(expr))
        } else {
            None
        };

        if let Some(rhs) = rhs {
            let field = fields[0];
            out.push(Stmt::Let(Let {
                bind: Pattern::Ident(crate::ast::Ident::new(field)),
                rhs,
                let_span: field_span,
                equal_span: Span {
                    start: field.end,
                    end: field.end,
                },
                pub_span,
            }));
            return Ok(());
        }

        for field in fields {
            out.push(Stmt::Let(Let {
                bind: Pattern::Ident(crate::ast::Ident::new(field)),
                rhs: PrimStmt::Expr(Expr::Nil(field_span)),
                let_span: field_span,
                equal_span: Span {
                    start: field.end,
                    end: field.end,
                },
                pub_span,
            }));
        }

        Ok(())
    }

    fn parse_class_stmt(&mut self, scope: &mut Scope, out: &mut Vec<Stmt>) -> Result<()> {
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
            Some(token @ token!(Keyword(Field))) => {
                if !decorators.is_empty() {
                    let token = token.clone();
                    return Err(self.syntax_error(
                        scope,
                        Some(token),
                        "decorators are only valid before `def` in a class body",
                    ));
                }
                self.parse_field_into(scope, pub_span, out)
            }
            Some(token!(Keyword(Def))) => {
                out.push(Stmt::Def(self.parse_def(scope, pub_span, decorators)?));
                Ok(())
            }
            Some(token!(Dedent)) | None => {
                Err(self.syntax_error(scope, None, "expected statement"))
            }
            other => {
                let token = other.cloned();
                Err(self.syntax_error(
                    scope,
                    token,
                    "class body only supports `field` and `def` declarations",
                ))
            }
        }
    }

    fn parse_class_block(&mut self, scope: &mut Scope) -> Result<Block> {
        use TokenInfo::*;

        let mut stmts = Vec::new();

        loop {
            let done = (|| -> Result<bool> {
                match self.peek()? {
                    None | Some(token!(Dedent)) => return Ok(true),
                    Some(token!(StmtSep)) => {
                        self.advance();
                    }
                    _ => self.parse_class_stmt(scope, &mut stmts)?,
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

        Ok(Block {
            stmts,
            vars: Vec::new(),
            repl: None,
        })
    }

    fn parse_class(
        &mut self,
        scope: &mut Scope,
        pub_span: Option<Span>,
        decorators: Vec<Decorator>,
    ) -> Result<Class> {
        let class_span = self.expect(scope, &[ExpectKind::Keyword(Keyword::Class)])?;
        self.expect(scope, &[ExpectKind::ArgSep])?;

        // Class name can be either `Name` (Ident) or `Name:` (Key) if there's a superclass
        // The span of a Key token excludes the `:`, so we can use it directly for the identifier
        let (ident, colon_span, super_exprs) = match self.next()? {
            Some(token!(TokenInfo::Ident, span)) => {
                // Plain identifier, no superclasses
                (Ident::new(span), None, vec![])
            }
            Some(token!(TokenInfo::Key, span)) => {
                // Identifier with colon suffix; parse space-separated compact expressions
                let colon_span = span.after_right_char();
                let mut super_exprs = vec![];
                loop {
                    match self.peek()? {
                        None
                        | Some(
                            token!(TokenInfo::StmtSep | TokenInfo::Indent | TokenInfo::Dedent),
                        ) => break,
                        Some(token!(TokenInfo::ArgSep)) => {
                            self.advance();
                        }
                        _ => super_exprs.push(self.parse_expr(scope, ExprMode::Compact)?),
                    }
                }
                (Ident::new(span), Some(colon_span), super_exprs)
            }
            other => {
                return Err(self.syntax_error(scope, other, "expected class name after `class`"));
            }
        };

        let body = match self.next()? {
            Some(token!(TokenInfo::Indent)) => {
                let block = self.parse_class_block(scope)?;
                self.expect(scope, &[ExpectKind::Dedent])?;
                block
            }
            Some(token!(TokenInfo::StmtSep)) => Block {
                stmts: vec![],
                vars: Default::default(),
                repl: None,
            },
            other => {
                return Err(self.syntax_error(
                    scope,
                    other,
                    "expected indent or newline after class declaration",
                ));
            }
        };

        Ok(Class {
            class_span,
            decorators,
            ident,
            colon_span,
            super_exprs,
            body,
            pub_span,
        })
    }

    fn reinterpret_module_name(&mut self, scope: &mut Scope, mut token: Token) -> Result<Token> {
        // Reinterpret a literal ending with `:` as a key as a special case
        if token.info == TokenInfo::Literal {
            let content = self.file.str(token.span).as_bytes();
            if !content[0].is_ascii_alphabetic()
                || content[1..content.len() - 1]
                    .iter()
                    .any(|c| !c.is_ascii_alphanumeric() && *c != b'_' && *c != b'.')
                || !content.last().unwrap() == b':'
            {
                return Err(self.syntax_error(scope, Some(token), "invalid module name"));
            }
            token.info = TokenInfo::Key;
            token.span.end -= 1;
        }
        Ok(token)
    }

    fn module_name_first(&self, span: Span) -> Span {
        if let Some((start, _)) = self.file.str(span).split_once(".") {
            (span.start..(span.start + start.len() as u32)).into()
        } else {
            span
        }
    }

    fn parse_module_name(&mut self, scope: &mut Scope, allow_key: bool) -> Result<(Span, bool)> {
        use self::Op;
        use TokenInfo::*;

        let mut result = match decay_ident!(self.next()?) {
            Some(token!(Ident, span)) => span,
            Some(token!(Key, span)) if allow_key => return Ok((span, true)),
            other => return Err(self.syntax_error(scope, other, "expected module name")),
        };

        loop {
            if let Some(token!(Op(Op::Dot))) = self.peek()? {
                self.advance();
                match self.next()? {
                    Some(token!(Ident, span)) => result = result | span,
                    Some(token!(Key, span)) if allow_key => return Ok((result | span, true)),
                    other => {
                        return Err(self.syntax_error(scope, other, "invalid module name"));
                    }
                }
            } else {
                break Ok((result, false));
            }
        }
    }

    fn parse_import_items(&mut self, scope: &mut Scope) -> Result<Vec<ImportItem>> {
        use self::{Ident, Op};
        use TokenInfo::*;

        let mut items = Vec::new();

        loop {
            match self.peek()? {
                Some(token!(Dedent)) => break Ok(items),
                Some(token!(StmtSep)) => {
                    self.advance();
                    continue;
                }
                _ => (),
            }
            items.push(match self.next()? {
                Some(token!(Op(Op::Minus), minus_span)) => {
                    self.expect(scope, &[ExpectKind::ArgSep])?;
                    let span = match decay_ident!(self.next()?) {
                        Some(token!(Ident, span)) => span,
                        other => {
                            return Err(self.syntax_error(scope, other, "invalid import item"));
                        }
                    };
                    ImportItem::AsIs {
                        bind: Ident::new(span),
                        delim_span: minus_span,
                    }
                }
                Some(mut token @ token!(Literal | Key)) => {
                    token = self.reinterpret_module_name(scope, token)?;
                    self.expect(scope, &[ExpectKind::ArgSep])?;
                    match decay_ident!(self.next()?) {
                        Some(token!(TokenInfo::Ident, span)) => ImportItem::Renamed {
                            item: token.span,
                            bind: Ident::new(span),
                            delim_span: token.span.after_right_char(),
                        },
                        other => {
                            return Err(self.syntax_error(
                                scope,
                                other,
                                "expected identifier for renamed import",
                            ));
                        }
                    }
                }
                other => {
                    return Err(self.syntax_error(scope, other, "invalid import item"));
                }
            })
        }
    }

    fn parse_import_elem_vert(&mut self, scope: &mut Scope) -> Result<ImportElement> {
        use self::{Ident, Op};
        use TokenInfo::*;

        match decay_ident!(self.peek()?) {
            Some(token!(Op(Op::Minus))) => {
                // FIXME: this needs to go back into AST
                let _minus_span = self.advance();
                self.expect(scope, &[ExpectKind::ArgSep])?;
                let (span, _) = self.parse_module_name(scope, false)?;
                Ok(ImportElement::ModuleAsIs {
                    module: span,
                    bind: Ident::new(self.module_name_first(span)),
                    insert: false,
                })
            }
            Some(token!(Ident | Key)) => {
                let (module_span, is_key) = self.parse_module_name(scope, true)?;
                if is_key {
                    if let Some(token!(TokenInfo::Indent)) = self.peek()? {
                        self.advance();
                        let items = self.parse_import_items(scope)?;
                        self.expect(scope, &[ExpectKind::Dedent])?;
                        return Ok(ImportElement::Items {
                            module: module_span,
                            items,
                        });
                    }
                    self.expect(scope, &[ExpectKind::ArgSep])?;
                    match decay_ident!(self.next()?) {
                        Some(token!(TokenInfo::Ident, span)) => Ok(ImportElement::ModuleRenamed {
                            module: module_span,
                            bind: Ident::new(span),
                            delim_span: span.after_right_char(),
                        }),
                        other => Err(self.syntax_error(
                            scope,
                            other,
                            "expected identifier for renamed module import",
                        )),
                    }
                } else {
                    Ok(ImportElement::ModuleAsIs {
                        module: module_span,
                        bind: Ident::new(self.module_name_first(module_span)),
                        insert: false,
                    })
                }
            }
            _ => {
                let token = self.next()?;
                Err(self.syntax_error(scope, token, "invalid import"))
            }
        }
    }

    fn parse_import_vert(
        &mut self,
        scope: &mut Scope,
        mut elems: Vec<ImportElement>,
        import_span: Span,
    ) -> Result<Import> {
        use TokenInfo::*;

        self.expect(scope, &[ExpectKind::Indent])?;

        loop {
            match self.peek()? {
                Some(token!(StmtSep)) => {
                    self.advance();
                }
                Some(token!(Dedent)) => {
                    self.advance();
                    break Ok(Import(elems, import_span));
                }
                _ => elems.push(self.parse_import_elem_vert(scope)?),
            }
        }
    }

    fn parse_import(&mut self, scope: &mut Scope) -> Result<Import> {
        use self::{Ident, Keyword};
        use TokenInfo::*;

        let import_span = self.expect(scope, &[ExpectKind::Keyword(Keyword::Import)])?;

        let mut elems = Vec::new();

        loop {
            match self.peek()? {
                None | Some(token!(StmtSep | Dedent)) => break Ok(Import(elems, import_span)),
                Some(token!(ArgSep)) => {
                    self.advance();
                    continue;
                }
                Some(token!(Indent)) => {
                    return self.parse_import_vert(scope, elems, import_span);
                }
                _ => (),
            }
            elems.push(match decay_ident!(self.peek()?) {
                Some(token!(Ident | Key)) => {
                    let (mod_span, is_key) = self.parse_module_name(scope, true)?;
                    if is_key {
                        if let Some(token!(Indent)) = self.peek()? {
                            self.advance();
                            elems.push(ImportElement::Items {
                                module: mod_span,
                                items: self.parse_import_items(scope)?,
                            });
                            self.expect(scope, &[ExpectKind::Dedent])?;
                            break Ok(Import(elems, import_span));
                        }
                        self.expect(scope, &[ExpectKind::ArgSep])?;
                        match decay_ident!(self.next()?) {
                            Some(token!(TokenInfo::Ident, span)) => ImportElement::ModuleRenamed {
                                module: mod_span,
                                bind: Ident::new(span),
                                delim_span: mod_span.after_right_char(),
                            },
                            other => {
                                return Err(self.syntax_error(
                                    scope,
                                    other,
                                    "expected identifier for renamed module import",
                                ));
                            }
                        }
                    } else {
                        ImportElement::ModuleAsIs {
                            module: mod_span,
                            bind: Ident::new(self.module_name_first(mod_span)),
                            insert: false,
                        }
                    }
                }
                _ => {
                    let token = self.next()?;
                    return Err(self.syntax_error(scope, token, "invalid import"));
                }
            })
        }
    }

    fn parse_stmt(&mut self, scope: &mut Scope) -> Result<Stmt> {
        use self::{Keyword, Return, Throw};
        use Keyword::*;
        use TokenInfo::*;

        if let Some(token!(ArgSep)) = self.peek()? {
            self.advance();
        }

        let decorators = self.parse_decorators(scope)?;

        // Check for pub modifier
        let pub_span = if let Some(token!(Keyword(Pub))) = self.peek()? {
            let span = self.advance();
            self.expect(scope, &[ExpectKind::ArgSep])?;
            match self.peek()? {
                Some(token!(Keyword(Let | Def | Class))) => (),
                Some(token!(DecoratorOpen)) => {
                    let token = self.peek()?.cloned();
                    return Err(self.syntax_error(scope, token, "`pub` must follow decorators"));
                }
                other => {
                    let other = other.cloned();
                    let _ = self.syntax_error(
                        scope,
                        other,
                        "`pub` is only valid before `let`, `def`, or `class`",
                    );
                }
            }
            Some(span)
        } else {
            None
        };

        if !decorators.is_empty() && !matches!(self.peek()?, Some(token!(Keyword(Def | Class)))) {
            let token = self.peek()?.cloned();
            return Err(self.syntax_error(
                scope,
                token,
                "decorators are only valid before `def` or `class`",
            ));
        }

        match self.peek()? {
            Some(token!(Keyword(Let))) => Ok(Stmt::Let(self.parse_let(scope, pub_span)?)),
            Some(token!(Keyword(Def))) => {
                Ok(Stmt::Def(self.parse_def(scope, pub_span, decorators)?))
            }
            Some(token!(Keyword(Class))) => {
                Ok(Stmt::Class(self.parse_class(scope, pub_span, decorators)?))
            }
            Some(token!(Keyword(If))) => Ok(Stmt::Prim(PrimStmt::If(self.parse_if(scope)?))),
            Some(token!(Keyword(Try))) => Ok(Stmt::Prim(PrimStmt::Try(self.parse_try(scope)?))),
            Some(token!(Keyword(While))) => self.parse_while(scope),
            Some(token!(Keyword(For))) => self.parse_for(scope),
            Some(token!(Keyword(Bind))) => Ok(Stmt::Bind(self.parse_bind(scope)?)),
            Some(token!(Keyword(Import))) => Ok(Stmt::Import(self.parse_import(scope)?)),
            Some(token!(Keyword(Return), span)) => {
                let span = *span;
                self.advance();
                let expr = match self.peek()? {
                    Some(token!(Indent)) => {
                        self.advance();
                        Some(self.parse_data(scope, vec![])?)
                    }
                    Some(token!(ArgSep)) => {
                        self.advance();
                        Some(self.parse_cmd_or_expr(scope, true)?)
                    }
                    None | Some(token!(Dedent | StmtSep)) => None,
                    _ => {
                        let token = self.next()?;
                        return Err(self.syntax_error(
                            scope,
                            token,
                            "expected space or indentation after `return`",
                        ));
                    }
                };
                Ok(Stmt::Return(Return {
                    expr,
                    span,
                    nl: None,
                }))
            }
            Some(token!(Keyword(Throw))) => {
                let span = self.advance();
                self.expect(scope, &[ExpectKind::ArgSep])?;
                let expr = self.parse_cmd_or_expr(scope, true)?;
                Ok(Stmt::Throw(Throw { expr, span }))
            }
            Some(token!(Keyword(Continue))) => Ok(Stmt::Continue(self.advance(), None)),
            Some(token!(Keyword(Break))) => Ok(Stmt::Break(self.advance(), None)),
            Some(token!(Keyword(Do))) => Ok(Stmt::Prim(PrimStmt::Expr(
                self.parse_do_block(scope, true)?,
            ))),
            Some(..) => {
                // Parse expression and check for assignment
                let arg0 = self.parse_cmd_arg0(scope)?;
                if let Some(token!(ArgSep)) = self.peek()? {
                    self.advance();
                }
                match self.peek()? {
                    Some(token!(Equal)) => {
                        // This is an assignment
                        let lhs = match arg0.into_lvalue() {
                            Ok(lvalue) => lvalue,
                            Err(expr) => {
                                self.fail = true;
                                self.diags.push(InvalidLValue(expr.span()));
                                return Err(Error);
                            }
                        };
                        let equal_span = self.expect(scope, &[ExpectKind::Equal])?;
                        let rhs = self.parse_rhs(scope)?;
                        Ok(Stmt::Assign(Assign {
                            lhs,
                            rhs,
                            equal_span,
                        }))
                    }
                    _ => {
                        // This is just an expression
                        let mut args = vec![];
                        self.parse_cmd_args(scope, true, true, &mut args)?;
                        let expr = if args.is_empty() {
                            arg0
                        } else {
                            Expr::Call {
                                arg0: Box::new(arg0),
                                args,
                                delim: None,
                            }
                        };
                        Ok(Stmt::Prim(PrimStmt::Expr(expr)))
                    }
                }
            }
            None => Err(self.syntax_error(scope, None, "expected statement")),
        }
    }

    fn parse_block(&mut self, scope: &mut Scope) -> Result<Block> {
        use self::Keyword;
        use Keyword::*;
        use TokenInfo::*;

        let mut stmts = Vec::new();

        loop {
            let done = (|| -> Result<bool> {
                match self.peek()? {
                    None | Some(token!(Dedent)) => return Ok(true),
                    Some(token!(StmtSep)) => {
                        self.advance();
                    }
                    _ => stmts.push(self.parse_stmt(scope)?),
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
                    // Destined to fail
                    self.lex.set_error();
                    // Try to resynchronize with token stream
                    while !matches!(
                        self.peek()?,
                        None | Some(token!(StmtSep | Keyword(Do) | Dedent))
                    ) {
                        self.advance();
                    }
                }
            }
        }

        Ok(Block {
            stmts,
            vars: Vec::new(),
            repl: None,
        })
    }

    pub(crate) fn parse(&mut self, ignore_error: bool) -> Result<Unit> {
        let mut scope = Scope::new();

        let body = match self.parse_block(&mut scope).and_then(|ast| {
            if self.fail && !ignore_error {
                Err(Error)
            } else {
                Ok(ast)
            }
        }) {
            Ok(body) => Ok(body),
            Err(_) if ignore_error => Ok(Block {
                stmts: vec![],
                vars: vec![],
                repl: None,
            }),
            Err(e) => Err(e),
        }?;

        Ok(Unit(Function {
            params: vec![],
            body,
        }))
    }
}
