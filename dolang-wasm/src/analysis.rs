//! The playground's highlighting, diagnostics and type checking.
use crate::{
    hover::{self, Checked, Hover, Occurrence},
    typelibs::typelibs,
};
use dolang::compile::{
    AnnotationKind, Diag, ErrorKind, NodeId, NoteKind, Severity, SourceSpan, Span, Unit, UnitId,
    typeck,
};
use dolang_ide::{Highlight, Reference};
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
/// A problem the compiler or checker found, as `annotate-snippets` would show
/// it: where it is, what is labelled around it, notes, and fixes.
#[derive(Debug, Serialize)]
pub(crate) struct Diagnostic {
    pub from: usize,
    pub to: usize,
    /// `error`, `warning` or `lint`
    pub severity: &'static str,
    pub message: String,
    pub labels: Vec<Label>,
    pub notes: Vec<Note>,
    pub fixes: Vec<Fix>,
}

/// A labelled span: the cause of a diagnostic or context for it.
#[derive(Debug, Serialize)]
pub(crate) struct Label {
    pub primary: bool,
    /// May be empty, for a label that only marks its span
    pub message: String,
    pub at: Place,
}

/// Where a label is.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum Place {
    /// In the source, as a UTF-16 range
    Here { from: usize, to: usize },
    /// In a bundled module, which has no text here
    Elsewhere {
        module: String,
        line: u32,
        column: u32,
    },
}

#[derive(Debug, Serialize)]
pub(crate) struct Note {
    /// `info` or `help`
    pub kind: &'static str,
    pub message: String,
}

/// A suggested change: replacing a UTF-16 range of the source with `insert`.
#[derive(Debug, Serialize)]
pub(crate) struct Fix {
    pub from: usize,
    pub to: usize,
    pub insert: String,
    pub title: String,
}

/// A span's UTF-16 range; all positions exposed to the editor are UTF-16.
fn range(offsets: &Offsets, unit: &Unit<'_>, span: Span) -> (usize, usize) {
    let range = unit.byte_range(span);
    (offsets.utf16(range.start), offsets.utf16(range.end))
}

/// Converts a diagnostic located in the source.
///
/// `foreign` places a span in another unit, which has no source here,
/// returning `None` for one in the source itself.
fn convert(
    offsets: &Offsets,
    unit: &Unit<'_>,
    diag: &Diag,
    foreign: impl Fn(&SourceSpan) -> Option<Place>,
) -> Diagnostic {
    let (from, to) = range(offsets, unit, diag.span().span());
    let here = |span: &SourceSpan| {
        let (from, to) = range(offsets, unit, span.span());
        Place::Here { from, to }
    };
    Diagnostic {
        from,
        to,
        severity: match diag.severity() {
            Severity::Error => "error",
            Severity::Warning => "warning",
            _ => "lint",
        },
        message: diag.message().to_string(),
        labels: (diag.annotations())
            .map(|annotation| {
                let span = annotation.span();
                Label {
                    primary: matches!(annotation.kind(), AnnotationKind::Primary),
                    message: annotation.message().to_string(),
                    at: foreign(&span).unwrap_or_else(|| here(&span)),
                }
            })
            .collect(),
        notes: (diag.notes())
            .map(|note| Note {
                kind: match note.kind() {
                    NoteKind::Help => "help",
                    _ => "info",
                },
                message: note.message().to_string(),
            })
            .collect(),
        // A change elsewhere can't be made here
        fixes: (diag.patches())
            .filter(|patch| foreign(&patch.span()).is_none())
            .map(|patch| {
                let (from, to) = range(offsets, unit, patch.span().span());
                Fix {
                    from,
                    to,
                    insert: patch.sub().to_owned(),
                    title: patch.message().to_owned(),
                }
            })
            .collect(),
    }
}

fn diagnostics(unit: &Unit<'_>, offsets: &Offsets) -> Vec<Diagnostic> {
    let mut result = Vec::new();
    for diag in unit.diagnostics() {
        assert!(
            diag.span().unit().is_none(),
            "playground diagnostics must be local"
        );
        result.push(convert(offsets, unit, &diag, |_| None));
    }
    result
}

/// Add the bundled typelibs of the modules `check` is missing, and those they
/// need in turn, each once. A module that isn't bundled is left for the check to
/// report.
fn add_bundled(
    cache: &mut HashMap<&'static str, typeck::Typelib<'static>>,
    check: &mut typeck::Check<'_>,
) {
    let mut tried = HashSet::new();
    loop {
        let names: Vec<String> = (check.missing().into_iter())
            .filter(|name| !tried.contains(*name))
            .map(str::to_owned)
            .collect();
        if names.is_empty() {
            return;
        }
        for name in names {
            if let Some((name, bytes)) = typelibs().find(|(module, _)| *module == name) {
                let typelib = cache.entry(name).or_insert_with(|| {
                    typeck::Typelib::read(bytes).expect("bundled typelibs are readable")
                });
                check
                    .typelib(typelib)
                    .expect("bundled typelibs check together");
            }
            tried.insert(name);
        }
    }
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
            let (from, to) = range(offsets, unit, span);
            if from == to {
                return;
            }
            // Unlike the LSP, which leaves `Token::Delim` (`(`, `-`, etc.) and
            // `Token::Sigil` to the client's own syntax highlighting, the
            // playground has nothing else to fall back on, so they must be
            // classified here or they render unstyled.
            let kind = node.and_then(|id| unit.node(id)).map(|node| node.kind());
            let (highlight, _) = dolang_ide::classify(token, kind.as_ref(), context);
            tokens.push(TokenRange {
                from,
                to,
                kind: class_name(highlight),
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
            let mut builder = typeck::Check::new();
            builder.analysis(true);
            let id = match builder.unit(unit) {
                Ok(id) => id,
                Err(err) if matches!(err.kind(), ErrorKind::Fail) => return (Vec::new(), None),
                Err(err) => panic!("cannot check the playground's unit: {err}"),
            };
            add_bundled(cache, &mut builder);
            let check = builder.run();
            let mut result = Vec::new();
            for diag in check.diagnostics() {
                if diag.span().unit() == Some(id) {
                    let foreign = |span: &SourceSpan| foreign(&check, id, span);
                    result.push(convert(offsets, unit, diag, foreign));
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

    fn refs(&self) -> &[Reference] {
        self.refs.get_or_init(|| dolang_ide::references(&self.unit))
    }

    /// The names of the declaration that the name at the UTF-16 offset
    /// `pos` names, that one included, or none if nothing there names a
    /// declaration.
    pub(crate) fn occurrences(&self, pos: usize) -> Vec<Occurrence> {
        hover::occurrences(&self.unit, &self.offsets, self.refs(), pos)
    }

    /// What hover shows at the UTF-16 offsets `from` to `to`: a position when
    /// they're equal, else a range. Before a check, only what the source
    /// declares.
    pub(crate) fn hover(&self, from: usize, to: usize) -> Option<Hover> {
        let refs = self.refs();
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

/// Where a span in a unit other than `id` lies.
fn foreign(check: &typeck::Checked, id: UnitId, span: &SourceSpan) -> Option<Place> {
    let unit = span.unit().filter(|&unit| unit != id)?;
    let start = span.span().start();
    Some(Place::Elsewhere {
        module: check.name(unit),
        line: start.line_number(),
        column: start.column_number(),
    })
}

/// The page's class for a highlight
fn class_name(highlight: Highlight) -> &'static str {
    match highlight {
        Highlight::Annotation => "annotation",
        Highlight::TypeParameter => "type-parameter",
        Highlight::Comment => "comment",
        Highlight::Constant => "constant",
        Highlight::Punctuation => "punctuation",
        Highlight::Sigil => "sigil",
        Highlight::String => "string",
        Highlight::Function => "function",
        Highlight::Property => "property",
        Highlight::Namespace => "namespace",
        Highlight::Keyword => "keyword",
        Highlight::Number => "number",
        Highlight::Operator => "operator",
        Highlight::Type => "type",
        Highlight::Class => "class",
        Highlight::Parameter => "parameter",
        Highlight::Variable => "variable",
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
    fn lint_carries_its_label_and_fixes() {
        let source = "let name = \"é\"\necho name $name";
        let utf16 = |byte: usize| source[..byte].encode_utf16().count();
        let from = utf16(source.rfind("name $").unwrap());
        let lint = Analyzed::new(source.to_owned())
            .diagnostics()
            .into_iter()
            .find(|diag| diag.message.starts_with("bareword"))
            .expect("a bareword lint");
        assert_eq!(
            (lint.from, lint.to, lint.severity),
            (from, from + 4, "lint")
        );
        let labels: Vec<_> = (lint.labels.iter())
            .map(|label| match label.at {
                Place::Here { from, to } => (label.primary, &label.message[..], from, to),
                Place::Elsewhere { .. } => panic!("a label elsewhere: {label:?}"),
            })
            .collect();
        assert_eq!(labels, [(false, "variable bound here", 4, 8)]);
        let fixes: Vec<_> = (lint.fixes.iter())
            .map(|fix| (fix.from, fix.to, &fix.insert[..]))
            .collect();
        assert_eq!(
            fixes,
            [(from, from + 4, "$name"), (from, from + 4, "\"name\"")]
        );
    }

    #[wasm_bindgen_test]
    fn a_label_in_a_bundled_module_names_it() {
        let source =
            "class C: @std.BaseIter[Int]\n  pub def (next) _self -> Str => \"a\"\necho (C())";
        let error = (Analyzed::new(source.to_owned()).check().into_iter())
            .find(|diag| diag.severity == "error")
            .expect("a conformance error");
        let from = source.find("Str").unwrap();
        let labels: Vec<_> = (error.labels.iter())
            .map(|label| match &label.at {
                Place::Here { from, to } => (*from, *to, ""),
                Place::Elsewhere { module, .. } => (0, 0, &module[..]),
            })
            .collect();
        assert_eq!(labels, [(from, from + 3, ""), (0, 0, "std")]);
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
