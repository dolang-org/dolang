//! Playground-local adaptation of the LSP's semantic classification and type
//! checking.
use crate::{
    hover::{self, Checked, Hover, Reference},
    typelibs::typelibs,
};
use dolang::compile::{
    Context, Diag, ErrorKind, Kind, NodeId, Severity, Span, Token, Unit, UnitId, typeck,
};
use dolang_util::utf16::Offsets;
use serde::Serialize;
use std::{
    cell::{OnceCell, RefCell},
    collections::{HashMap, HashSet},
    path::Path,
};

thread_local! {
    /// The bundled typelibs, decoded as checks first need them.
    static TYPELIBS: RefCell<HashMap<&'static str, typeck::Typelib<'static>>> =
        RefCell::new(HashMap::new());
}

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

/// A span's UTF-16 range; all positions exposed to the editor are UTF-16.
fn range(offsets: &Offsets, span: &Span) -> (usize, usize) {
    (
        offsets.utf16(span.start().byte_offset()),
        offsets.utf16(span.end().byte_offset()),
    )
}

/// Converts a diagnostic located in the source, appending it and its
/// annotations to `out`.
///
/// `foreign` locates an annotation in another unit, which has no source here,
/// returning `None` for one in the source itself.
fn convert(
    offsets: &Offsets,
    diag: &Diag,
    foreign: impl Fn(&dolang::compile::SourceSpan) -> Option<String>,
    out: &mut Vec<Diagnostic>,
) {
    let (from, to) = range(offsets, &diag.span().span());
    let mut message = diag.message().to_string();
    for note in diag.notes() {
        message.push_str(&format!("\n{}", note.message()));
    }
    out.push(Diagnostic {
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
        if message.is_empty() {
            continue;
        }
        let span = annotation.span();
        // An annotation elsewhere is named in the message, on the
        // diagnostic's own range
        let (range, message) = match foreign(&span) {
            Some(location) => ((from, to), format!("{location}: {message}")),
            None => (range(offsets, &span.span()), message),
        };
        out.push(Diagnostic {
            from: range.0,
            to: range.1,
            severity: "info",
            message,
        });
    }
}

fn diagnostics(unit: &Unit<'_>, offsets: &Offsets) -> Vec<Diagnostic> {
    let mut result = Vec::new();
    for diag in unit.diagnostics() {
        assert!(
            diag.span().unit().is_none(),
            "playground diagnostics must be local"
        );
        convert(offsets, &diag, |_| None, &mut result);
    }
    result
}

/// The typelibs of the bundled modules a unit imports, directly or through
/// other typelibs, each once. `std` is always among them, since the types of
/// literals are declared there.
fn imported_by<'t>(
    cache: &'t mut HashMap<&'static str, typeck::Typelib<'static>>,
    unit: &Unit<'_>,
) -> Vec<&'t typeck::Typelib<'static>> {
    let mut queue: Vec<String> = unit.imports().into_iter().map(str::to_owned).collect();
    queue.push("std".to_owned());
    let mut seen = HashSet::new();
    while let Some(name) = queue.pop() {
        if seen.contains(&name) {
            continue;
        }
        if let Some((name, bytes)) = typelibs().find(|(module, _)| *module == name) {
            let typelib = cache.entry(name).or_insert_with(|| {
                typeck::Typelib::read(bytes).expect("bundled typelibs are readable")
            });
            queue.extend(typelib.imports().into_iter().map(str::to_owned));
        }
        seen.insert(name);
    }
    let mut found: Vec<_> = seen
        .iter()
        .filter_map(|name| cache.get(name.as_str()))
        .collect();
    found.sort_by_key(|typelib| typelib.module());
    found
}

/// A playground source, compiled for highlighting and checking.
pub(crate) struct Analyzed {
    unit: Unit<'static>,
    offsets: Offsets,
    /// The tokens naming declarations, once hover needs them
    refs: OnceCell<Vec<Reference>>,
    /// What the check concluded, once checked
    checked: Option<Checked>,
}

impl Analyzed {
    pub(crate) fn new(source: String) -> Self {
        let mut config = super::config();
        config.recover(true).document(true).typecheck(true);
        let unit = config.unit(Path::new("playground.dol"), source);
        Self {
            offsets: Offsets::new(unit.source()),
            unit,
            refs: OnceCell::new(),
            checked: None,
        }
    }

    /// The source's tokens, classified for highlighting, in source order.
    pub(crate) fn tokens(&self) -> Vec<TokenRange> {
        let unit = &self.unit;
        let offsets = &self.offsets;
        let mut tokens = Vec::new();
        unit.tokens(&mut |token, span: Span, node: Option<NodeId>, context| {
            let (from, to) = range(offsets, &span);
            if from == to {
                return;
            }
            // Unlike the LSP, which leaves `Token::Delim` (`(`, `-`, etc.) and
            // `Token::Sigil` to the client's own syntax highlighting, the
            // playground has nothing else to fall back on, so they must be
            // classified here or they render unstyled.
            let kind = node.and_then(|id| unit.node(id)).map(|node| node.kind());
            tokens.push(TokenRange {
                from,
                to,
                kind: classify_token(token, kind.as_ref(), context),
            });
        });
        tokens.sort_by_key(|token| (token.from, token.to));
        tokens
    }

    /// The compiler's diagnostics.
    pub(crate) fn diagnostics(&self) -> Vec<Diagnostic> {
        diagnostics(&self.unit, &self.offsets)
    }

    /// Type checks the source against the bundled typelibs it imports,
    /// returning the diagnostics located in it, and keeps what the check
    /// concluded for hover.
    ///
    /// A source that failed to compile can't be checked and has none.  An
    /// import that isn't bundled is reported once, at the import.
    pub(crate) fn check(&mut self) -> Vec<Diagnostic> {
        let unit = &self.unit;
        let offsets = &self.offsets;
        let (result, checked) = TYPELIBS.with_borrow_mut(|cache| {
            let mut builder = typeck::Builder::new();
            builder.analysis(true);
            let id = match builder.unit(unit) {
                Ok(id) => id,
                Err(err) if matches!(err.kind(), ErrorKind::Fail) => return (Vec::new(), None),
                Err(err) => panic!("cannot check the playground's unit: {err}"),
            };
            for typelib in imported_by(cache, unit) {
                builder
                    .typelib(typelib)
                    .expect("bundled typelibs check together");
            }
            let check = builder.check();
            let mut result = Vec::new();
            for diag in check.diagnostics() {
                if diag.span().unit() == Some(id) {
                    let foreign = |span: &dolang::compile::SourceSpan| foreign(&check, id, span);
                    convert(offsets, diag, foreign, &mut result);
                }
            }
            let name = check.name(id);
            let checked = check.into_analysis().map(|analysis| Checked {
                unit: id,
                name,
                analysis,
            });
            (result, checked)
        });
        self.checked = checked;
        result
    }

    /// What hover shows at the UTF-16 offsets `from` to `to`: a position when
    /// they're equal, else a range. Before a check, only what the source
    /// declares.
    pub(crate) fn hover(&self, from: usize, to: usize) -> Option<Hover> {
        let refs = self.refs.get_or_init(|| hover::references(&self.unit));
        hover::hover(
            &self.unit,
            &self.offsets,
            refs,
            self.checked.as_ref(),
            from,
            to,
        )
    }
}

/// Where a span in a unit other than `id` lies, as `path:line:column`.
fn foreign(
    check: &typeck::Check<'_>,
    id: UnitId,
    span: &dolang::compile::SourceSpan,
) -> Option<String> {
    let unit = span.unit().filter(|&unit| unit != id)?;
    let start = span.span().start();
    Some(format!(
        "{}:{}:{}",
        check.path(unit).display(),
        start.line_number(),
        start.column_number()
    ))
}

fn classify_token(token: Token, kind: Option<&Kind<'_>>, context: Context) -> &'static str {
    match token {
        Token::Annotation => "annotation",
        Token::Binder => "type-parameter",
        Token::Comment => "comment",
        Token::Constant => "constant",
        Token::Delim => "punctuation",
        Token::Escape => "string",
        Token::Field => match context {
            Context::Call => "function",
            Context::None => "property",
        },
        Token::Method => "function",
        Token::Key => "property",
        Token::ModuleName => "namespace",
        Token::ModuleItem => "property",
        Token::Keyword => "keyword",
        Token::Literal => "string",
        Token::Number => "number",
        Token::Operator => "operator",
        Token::StringDelim => "string",
        Token::Type => match kind {
            Some(Kind::Binder { .. }) => "type-parameter",
            _ => "type",
        },
        Token::TypeKey => "property",
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
            ) => "class",
            (Context::Call, Some(Kind::PreludeItem { .. } | Kind::PreludeModule { .. })) => {
                "function"
            }
            (Context::Call, _) => "function",
            (
                Context::None,
                Some(
                    Kind::PositionalParam { .. }
                    | Kind::KeyParam { .. }
                    | Kind::RestParam { .. }
                    | Kind::SelfParam { .. },
                ),
            ) => "parameter",
            (
                Context::None,
                Some(Kind::Function { .. } | Kind::Method { .. } | Kind::SpecialMethod { .. }),
            ) => "function",
            (Context::None, Some(Kind::PreludeItem { .. })) => "variable",
            (Context::None, Some(Kind::PreludeModule { .. } | Kind::ImportModule { .. })) => {
                "namespace"
            }
            (Context::None, _) => "variable",
        },
        Token::Sigil => "sigil",
    }
}

#[cfg(all(test, target_family = "wasm"))]
pub mod tests {
    use super::*;
    use wasm_bindgen_test::wasm_bindgen_test;

    #[wasm_bindgen_test]
    fn unicode_and_recovery() {
        let source = "echo \"é e\u{301} 😀\"\nlet x = 42\nlet =";
        let result = Analyzed::new(source.to_owned());
        assert!(!result.diagnostics().is_empty());
        let tokens = result.tokens();
        let number = tokens.iter().find(|token| token.kind == "number").unwrap();
        let byte = source.find("42").unwrap();
        assert_eq!(number.from, source[..byte].encode_utf16().count());
        assert_eq!(number.to, number.from + 2);
        for token in &tokens {
            assert!(token.from < token.to);
            assert!(token.to <= source.encode_utf16().count());
        }
    }

    #[wasm_bindgen_test]
    fn names_follow_compiler_context() {
        let source = "class Widget\n  pub field x\ndef identity value\n  value\nlet w = Widget()\nidentity $w\n# comment\necho \"value: $w\"";
        let result = Analyzed::new(source.to_owned());
        for kind in ["class", "function", "parameter", "comment", "string"] {
            assert!(
                result.tokens().iter().any(|token| token.kind == kind),
                "{kind}"
            );
        }
    }

    #[wasm_bindgen_test]
    fn type_syntax_has_distinct_highlights() {
        let source = "def first[T] value@T -> Array[T]\n  value";
        let result = Analyzed::new(source.to_owned());
        for kind in ["annotation", "type", "type-parameter"] {
            assert!(
                result.tokens().iter().any(|token| token.kind == kind),
                "{kind}"
            );
        }
    }

    #[wasm_bindgen_test]
    fn delimiters_are_classified_as_punctuation() {
        // `(`/`)` and the vertical-layout `-` are both `Token::Delim`; the
        // playground has no other highlighting to fall back on for them, so
        // they must show up here.
        let source = "let x = (1 + 2)\nfoo\n  - 1\n";
        let result = Analyzed::new(source.to_owned());
        assert!(
            result
                .tokens()
                .iter()
                .any(|token| token.kind == "punctuation")
        );
    }

    #[wasm_bindgen_test]
    fn sigil_is_highlighted_distinctly_from_the_variable_it_introduces() {
        let source = "let x = 1\necho $x";
        let byte = source.rfind('$').unwrap();
        let from = source[..byte].encode_utf16().count();
        let tokens = Analyzed::new(source.to_owned()).tokens();
        let sigil = tokens.iter().find(|token| token.from == from).unwrap();
        assert_eq!(sigil.kind, "sigil");
        assert_eq!(sigil.to, sigil.from + 1);
        assert!(tokens.iter().any(|token| token.kind == "variable"));
    }

    #[wasm_bindgen_test]
    fn sigil_is_highlighted_inside_string_interpolation() {
        // Both the bare `$name` form and the `${...}` formatted form.
        let source = "let x = 1\necho \"$x and ${x:x}\"";
        let dollars: Vec<usize> = source.match_indices('$').map(|(byte, _)| byte).collect();
        assert_eq!(dollars.len(), 2);
        let tokens = Analyzed::new(source.to_owned()).tokens();
        for byte in dollars {
            let from = source[..byte].encode_utf16().count();
            let sigil = tokens
                .iter()
                .find(|token| token.from == from)
                .unwrap_or_else(|| panic!("no token at byte {byte}"));
            assert_eq!(sigil.kind, "sigil");
            assert_eq!(sigil.to, sigil.from + 1);
        }
    }

    #[wasm_bindgen_test]
    fn comments_at_eof_include_the_last_character() {
        for source in ["#", "# latest", "# é", "# 😀", "# e\u{301}"] {
            let tokens = Analyzed::new(source.to_owned()).tokens();
            assert_eq!(tokens.len(), 1);
            assert_eq!(tokens[0].from, 0);
            assert_eq!(tokens[0].to, source.encode_utf16().count());
        }
    }

    #[wasm_bindgen_test]
    fn type_error_is_reported() {
        let source = "# é 😀\nlet x @ Int = \"a\"\necho $x";
        let mut analyzed = Analyzed::new(source.to_owned());
        assert!(analyzed.diagnostics().is_empty());
        let checked = analyzed.check();
        let error = checked
            .iter()
            .find(|diag| diag.severity == "error")
            .expect("a type error");
        let byte = source.find("\"a\"").unwrap();
        let from = source[..byte].encode_utf16().count();
        assert!(
            error.from <= from && from + 3 <= error.to,
            "{}..{}: {}",
            error.from,
            error.to,
            error.message
        );
    }

    #[wasm_bindgen_test]
    fn only_unbundled_imports_are_reported() {
        // `json` is bundled and `fs` is not
        let source = "import fs\nimport json\nfs.read_text (json.encode 1)";
        let reported: Vec<_> = Analyzed::new(source.to_owned())
            .check()
            .into_iter()
            .map(|diag| (diag.from, diag.to, diag.message))
            .collect();
        assert_eq!(reported, [(7, 9, "module `fs` not found".to_owned())]);
    }

    #[wasm_bindgen_test]
    fn empty_source_and_eof_diagnostic() {
        assert!(Analyzed::new(String::new()).tokens().is_empty());
        let result = Analyzed::new("let x =".to_owned());
        assert!(!result.diagnostics().is_empty());
        assert!(
            result
                .diagnostics()
                .iter()
                .all(|d| d.from <= d.to && d.to <= 7)
        );
    }
}
