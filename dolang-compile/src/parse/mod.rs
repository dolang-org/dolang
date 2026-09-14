use self::stream::Peek;
use crate::{
    ast::{Arg, Block, Expand, Expr, Function, GroupDelim, Ident, Key, Root, Single},
    lex::{self, Lexer},
    source::{Diags, File, Span},
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
            match $token {
                $(Some(token!($pat, span)) => Some(Token { info: $expr, span }),)+
                other => other,
            }
        }
    }
}

macro_rules! decay_ident {
    ($token: expr) => {
        decay!($token,
            TokenInfo::Keyword(self::Keyword::Do) => TokenInfo::Keyword(self::Keyword::Do),
            TokenInfo::Keyword(self::Keyword::Nil) => TokenInfo::Keyword(self::Keyword::Nil),
            TokenInfo::Keyword(_) => TokenInfo::Ident)
    }
}

macro_rules! decay_field {
    ($token: expr) => {
        decay!($token, TokenInfo::Bool(_) | TokenInfo::Keyword(_) => TokenInfo::Ident)
    }
}

macro_rules! decay_shell {
    ($token: expr) => {
        decay!($token,
            TokenInfo::Keyword(self::Keyword::Do) => TokenInfo::Keyword(self::Keyword::Do),
            TokenInfo::Keyword(self::Keyword::Nil) => TokenInfo::Keyword(self::Keyword::Nil),
            TokenInfo::Equal
            | TokenInfo::Ident
            | TokenInfo::LeftBracket
            | TokenInfo::RightBracket
            | TokenInfo::LeftBrace
            | TokenInfo::RightBrace
            | TokenInfo::Comma
            | TokenInfo::DotDot
            | TokenInfo::Ellipsis
            | TokenInfo::Keyword(_)
            | TokenInfo::Op(_)
            | TokenInfo::RBar
            | TokenInfo::TBar
            | TokenInfo::DecoratorOpen
            | TokenInfo::Arrow
            | TokenInfo::At
            | TokenInfo::Question => TokenInfo::Literal)
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
            | TokenInfo::TQuote
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
            | TokenInfo::Arrow
            | TokenInfo::At
            | TokenInfo::Question
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
            | TokenInfo::TQuote
            | TokenInfo::RBar
            | TokenInfo::TBar
            | TokenInfo::ArgSep
            | TokenInfo::Hash
            | TokenInfo::DecoratorOpen
            | TokenInfo::Arrow
            | TokenInfo::At
            | TokenInfo::Question => TokenInfo::Literal)
    }
}

mod class;
mod cmd;
mod diag;
mod expr;
mod format;
mod func;
mod import;
mod params;
mod stmt;
mod stream;
mod string;
mod ty;
mod vert;

#[derive(Debug, Clone)]
pub(crate) struct Error;

impl From<lex::Error> for Error {
    fn from(_value: lex::Error) -> Self {
        Error
    }
}

pub(crate) type Result<T> = std::result::Result<T, Error>;

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

    fn dollar_group(expr: Expr, span: Span) -> Expr {
        Expr::Group {
            expr: Box::new(expr),
            delim: Some(GroupDelim::Dollar(span)),
        }
    }

    fn positional_arg(expr: Expr) -> Arg {
        Arg::Pos(Single {
            expr,
            delim_span: None,
        })
    }

    fn ditto_key(span: Span, delim_span: Option<Span>) -> Key {
        Key {
            key_span: span,
            colon_span: span.before_left_char(),
            expr: Expr::Ident(Ident::new(span)),
            delim_span,
        }
    }

    fn expansion(expr: Expr, ellipsis_span: Span, delim_span: Option<Span>) -> Expand {
        Expand {
            expr,
            ellipsis_span,
            delim_span,
        }
    }

    fn finish_call(arg0: Expr, args: Vec<Arg>) -> Expr {
        if args.is_empty() {
            arg0
        } else {
            Expr::Call {
                arg0: Box::new(arg0),
                args,
                delim: None,
            }
        }
    }

    /// Parse the source into a root AST node.
    ///
    /// Always yields a tree: if parsing fails outright, or if errors were recorded and
    /// `recover` is not set, the tree is an empty block.  Consult [`Parser::failed`] to
    /// determine whether errors occurred.
    pub(crate) fn parse(&mut self, recover: bool) -> Root {
        let mut scope = Scope::new();

        let body = match self.parse_block(&mut scope) {
            Ok(body) if recover || !self.fail => body,
            res => {
                if res.is_err() {
                    self.fail = true;
                }
                Block {
                    stmts: vec![],
                    vars: vec![],
                    repl: None,
                }
            }
        };

        Root(Function {
            params: vec![],
            body,
        })
    }

    /// Whether any error was recorded during parsing
    pub(crate) fn failed(&self) -> bool {
        self.fail
    }
}
