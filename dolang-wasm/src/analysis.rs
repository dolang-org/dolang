//! Playground-local adaptation of the LSP's semantic classification.
use dolang::compile::{Context, Kind, NodeId, Severity, Span, Token, Unit};
use serde::Serialize;
use std::path::Path;

#[derive(Serialize)]
pub(crate) struct TokenRange {
    pub from: usize,
    pub to: usize,
    pub kind: &'static str,
}
#[derive(Serialize)]
pub(crate) struct Diagnostic {
    pub from: usize,
    pub to: usize,
    pub severity: &'static str,
    pub message: String,
}
#[derive(Serialize)]
pub(crate) struct Analysis {
    pub tokens: Vec<TokenRange>,
    pub diagnostics: Vec<Diagnostic>,
}

// Index by UTF-8 byte offset; all positions exposed to the editor are UTF-16.
struct Offsets(Vec<usize>);
impl Offsets {
    fn new(source: &str) -> Self {
        let mut map = vec![0; source.len() + 1];
        let mut utf16 = 0;
        for (byte, ch) in source.char_indices() {
            map[byte..byte + ch.len_utf8()].fill(utf16);
            utf16 += ch.len_utf16();
        }
        map[source.len()] = utf16;
        Self(map)
    }
    fn range(&self, span: &Span) -> (usize, usize) {
        let last = self.0.len() - 1;
        (
            self.0[span.start().byte_offset().min(last)],
            self.0[span.end().byte_offset().min(last)],
        )
    }
}

pub(crate) fn diagnostics(unit: &Unit<'_>, source: &str) -> Vec<Diagnostic> {
    let offsets = Offsets::new(source);
    let mut result = Vec::new();
    for diag in unit.diagnostics() {
        let (from, to) = offsets.range(&diag.span());
        let mut message = diag.message().to_string();
        for note in diag.notes() {
            message.push_str(&format!("\n{}", note.message()));
        }
        result.push(Diagnostic {
            from,
            to,
            severity: match diag.severity() {
                Severity::Error => "error",
                Severity::Warning => "warning",
                _ => "info",
            },
            message,
        });
        for annotation in diag.annotations() {
            let message = annotation.message().to_string();
            if !message.is_empty() {
                let (from, to) = offsets.range(&annotation.span());
                result.push(Diagnostic {
                    from,
                    to,
                    severity: "info",
                    message,
                });
            }
        }
    }
    result
}

pub(crate) fn analyze(source: &str) -> Analysis {
    let mut config = super::config();
    config.recover(true).document(true);
    let unit = config.unit(Path::new("playground.dol"), source.as_bytes());
    let offsets = Offsets::new(source);
    let names = [
        "constant",
        "operator",
        "string",
        "property",
        "function",
        "keyword",
        "number",
        "parameter",
        "variable",
        "namespace",
        "comment",
        "class",
    ];
    let mut tokens = Vec::new();
    unit.tokens(&mut |token, span: Span, node: Option<NodeId>, context| {
        let (from, to) = offsets.range(&span);
        if from == to || matches!(token, Token::Delim) {
            return;
        }
        let kind = node.and_then(|id| unit.node(id)).map(|node| node.kind());
        let (class, _) = classify_token(token, kind.as_ref(), context);
        tokens.push(TokenRange {
            from,
            to,
            kind: names[class as usize],
        });
    });
    tokens.sort_by_key(|token| (token.from, token.to));
    Analysis {
        tokens,
        diagnostics: diagnostics(&unit, source),
    }
}

const TT_CONSTANT: u32 = 0;
const TT_OPERATOR: u32 = 1;
const TT_STRING: u32 = 2;
const TT_PROPERTY: u32 = 3;
const TT_FUNCTION: u32 = 4;
const TT_KEYWORD: u32 = 5;
const TT_NUMBER: u32 = 6;
const TT_PARAMETER: u32 = 7;
const TT_VARIABLE: u32 = 8;
const TT_NAMESPACE: u32 = 9;
const TT_COMMENT: u32 = 10;
const TT_CLASS: u32 = 11;

const MOD_PRELUDE: u32 = 1 << 0;

fn classify_token(token: Token, kind: Option<&Kind<'_>>, context: Context) -> (u32, u32) {
    match token {
        Token::Comment => (TT_COMMENT, 0),
        Token::Constant => (TT_CONSTANT, 0),
        Token::Delim => (TT_OPERATOR, 0),
        Token::Escape => (TT_STRING, 0),
        Token::Field => match context {
            Context::Call => (TT_FUNCTION, 0),
            Context::None => (TT_PROPERTY, 0),
        },
        Token::Method => (TT_FUNCTION, 0),
        Token::Key => (TT_PROPERTY, 0),
        Token::ModuleName => (TT_NAMESPACE, 0),
        Token::ModuleItem => (TT_PROPERTY, 0),
        Token::Keyword => (TT_KEYWORD, 0),
        Token::Literal => (TT_STRING, 0),
        Token::Number => (TT_NUMBER, 0),
        Token::Operator => (TT_OPERATOR, 0),
        Token::StringDelim => (TT_STRING, 0),
        Token::Variable => match (context, kind) {
            (_, Some(Kind::Class { .. })) => (TT_CLASS, 0),
            (Context::Call, Some(Kind::PreludeItem { .. })) => (TT_FUNCTION, MOD_PRELUDE),
            (Context::Call, Some(Kind::PreludeModule { .. })) => (TT_FUNCTION, MOD_PRELUDE),
            (Context::Call, _) => (TT_FUNCTION, 0),
            (
                Context::None,
                Some(
                    Kind::PositionalParam { .. }
                    | Kind::KeyParam { .. }
                    | Kind::RestParam { .. }
                    | Kind::SelfParam { .. },
                ),
            ) => (TT_PARAMETER, 0),
            (
                Context::None,
                Some(Kind::Function { .. } | Kind::Method { .. } | Kind::SpecialMethod { .. }),
            ) => (TT_FUNCTION, 0),
            (Context::None, Some(Kind::PreludeItem { .. })) => (TT_VARIABLE, MOD_PRELUDE),
            (Context::None, Some(Kind::PreludeModule { .. })) => (TT_NAMESPACE, MOD_PRELUDE),
            (Context::None, Some(Kind::ImportModule { .. })) => (TT_NAMESPACE, 0),
            (Context::None, _) => (TT_VARIABLE, 0),
        },
        Token::Sigil => (TT_OPERATOR, 0),
    }
}

#[cfg(all(test, target_family = "wasm"))]
pub mod tests {
    use super::*;
    use wasm_bindgen_test::wasm_bindgen_test;

    #[wasm_bindgen_test]
    fn unicode_and_recovery() {
        let source = "echo \"é e\u{301} 😀\"\nlet x = 42\nlet =";
        let result = analyze(source);
        assert!(!result.diagnostics.is_empty());
        let number = result
            .tokens
            .iter()
            .find(|token| token.kind == "number")
            .unwrap();
        let byte = source.find("42").unwrap();
        assert_eq!(number.from, source[..byte].encode_utf16().count());
        assert_eq!(number.to, number.from + 2);
        for token in result.tokens {
            assert!(token.from < token.to);
            assert!(token.to <= source.encode_utf16().count());
        }
    }

    #[wasm_bindgen_test]
    fn names_follow_compiler_context() {
        let source = "class Widget\n  pub field x\ndef identity value\n  value\nlet w = Widget()\nidentity $w\n# comment\necho \"value: $w\"";
        let result = analyze(source);
        for kind in ["class", "function", "parameter", "comment", "string"] {
            assert!(
                result.tokens.iter().any(|token| token.kind == kind),
                "{kind}"
            );
        }
    }

    #[wasm_bindgen_test]
    fn sigil_is_highlighted_distinctly_from_the_variable_it_introduces() {
        let source = "let x = 1\necho $x";
        let result = analyze(source);
        let byte = source.rfind('$').unwrap();
        let from = source[..byte].encode_utf16().count();
        let sigil = result
            .tokens
            .iter()
            .find(|token| token.from == from)
            .unwrap();
        assert_eq!(sigil.kind, "operator");
        assert_eq!(sigil.to, sigil.from + 1);
        assert!(result.tokens.iter().any(|token| token.kind == "variable"));
    }

    #[wasm_bindgen_test]
    fn sigil_is_highlighted_inside_string_interpolation() {
        // Both the bare `$name` form and the `${...}` formatted form.
        let source = "let x = 1\necho \"$x and ${x:x}\"";
        let dollars: Vec<usize> = source.match_indices('$').map(|(byte, _)| byte).collect();
        assert_eq!(dollars.len(), 2);
        let result = analyze(source);
        for byte in dollars {
            let from = source[..byte].encode_utf16().count();
            let sigil = result
                .tokens
                .iter()
                .find(|token| token.from == from)
                .unwrap_or_else(|| panic!("no token at byte {byte}"));
            assert_eq!(sigil.kind, "operator");
            assert_eq!(sigil.to, sigil.from + 1);
        }
    }

    #[wasm_bindgen_test]
    fn comments_at_eof_include_the_last_character() {
        for source in ["#", "# latest", "# é", "# 😀", "# e\u{301}"] {
            let result = analyze(source);
            assert_eq!(result.tokens.len(), 1);
            assert_eq!(result.tokens[0].from, 0);
            assert_eq!(result.tokens[0].to, source.encode_utf16().count());
        }
    }

    #[wasm_bindgen_test]
    fn empty_source_and_eof_diagnostic() {
        assert!(analyze("").tokens.is_empty());
        let result = analyze("let x =");
        assert!(!result.diagnostics.is_empty());
        assert!(
            result
                .diagnostics
                .iter()
                .all(|d| d.from <= d.to && d.to <= 7)
        );
    }
}
