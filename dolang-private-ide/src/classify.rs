//! How a token is highlighted, by what it names.
use dolang_compile::{Context, Kind, Token};

/// What a token is highlighted as
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Highlight {
    /// The `@` of an annotation
    Annotation,
    TypeParameter,
    Comment,
    Constant,
    /// A delimiter, such as `(` or a vertical layout's `-`
    Punctuation,
    /// The `$` introducing an expression
    Sigil,
    String,
    Function,
    Property,
    Namespace,
    Keyword,
    Number,
    Operator,
    Type,
    Class,
    Parameter,
    Variable,
}

/// How to highlight `token`, given the kind of the declaration it names and
/// whether it's called, and whether that's a prelude binding
pub fn classify(token: Token, kind: Option<&Kind<'_>>, context: Context) -> (Highlight, bool) {
    use Highlight::*;
    let plain = |highlight| (highlight, false);
    match token {
        Token::Annotation => plain(Annotation),
        Token::Binder => plain(TypeParameter),
        Token::Comment => plain(Comment),
        Token::Constant => plain(Constant),
        Token::Delim => plain(Punctuation),
        Token::Sigil => plain(Sigil),
        Token::Escape | Token::Literal | Token::StringDelim => plain(String),
        Token::Field => match context {
            Context::Call => plain(Function),
            Context::None => plain(Property),
        },
        Token::Method => plain(Function),
        Token::Key | Token::ModuleItem | Token::TypeKey => plain(Property),
        Token::ModuleName => plain(Namespace),
        Token::Keyword => plain(Keyword),
        Token::Number => plain(Number),
        Token::Operator => plain(Operator),
        Token::Type => match kind {
            Some(Kind::PreludeItem {
                type_only: true, ..
            }) => (Type, true),
            Some(Kind::Binder { .. }) => plain(TypeParameter),
            _ => plain(Type),
        },
        Token::Variable => match (context, kind) {
            (
                _,
                Some(
                    Kind::Class { .. }
                    | Kind::Binder { .. }
                    | Kind::ImportItem {
                        type_only: true, ..
                    },
                ),
            ) => plain(Class),
            (Context::Call, Some(Kind::PreludeItem { .. } | Kind::PreludeModule { .. })) => {
                (Function, true)
            }
            (Context::Call, _) => plain(Function),
            (
                Context::None,
                Some(
                    Kind::PositionalParam { .. }
                    | Kind::KeyParam { .. }
                    | Kind::RestParam { .. }
                    | Kind::SelfParam { .. },
                ),
            ) => plain(Parameter),
            (
                Context::None,
                Some(Kind::Function { .. } | Kind::Method { .. } | Kind::SpecialMethod { .. }),
            ) => plain(Function),
            (Context::None, Some(Kind::PreludeItem { .. })) => (Variable, true),
            (Context::None, Some(Kind::PreludeModule { .. })) => (Namespace, true),
            (Context::None, Some(Kind::ImportModule { .. })) => plain(Namespace),
            (Context::None, _) => plain(Variable),
        },
    }
}
