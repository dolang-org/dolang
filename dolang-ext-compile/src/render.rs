use annotate_snippets::{
    Annotation, AnnotationKind as SnippetAnnotationKind, Group, Level, Origin,
    Patch as SnippetPatch, Renderer, Snippet, renderer::DecorStyle,
};
use console::Term;
use dolang::compile::{self, Diag, UnitId};

#[derive(Clone, Copy)]
pub enum ColorMode {
    Auto,
    Never,
    Always,
}

fn use_color(term: &Term, color: ColorMode) -> bool {
    match color {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => term.is_term() && term.features().colors_supported(),
    }
}

/// A location with no known source.
#[derive(Debug)]
pub(crate) struct UnknownSource;

/// Sources indexed by unit; a location with no unit refers to the first. A unit
/// checked from its typelib has a path but no text.
#[derive(Clone, Copy)]
struct Sources<'a, 'b> {
    paths: &'b [&'a str],
    texts: &'b [Option<&'a str>],
}

/// Where a unit's locations are shown: its source, or for a unit without one, only
/// its path
enum Shown<'a, T: Clone> {
    Snippet(Snippet<'a, T>),
    Path(&'a str),
}

fn snippet<'a, T: Clone>(
    sources: Sources<'a, '_>,
    unit: Option<UnitId>,
) -> Result<Shown<'a, T>, UnknownSource> {
    let index = unit.map_or(0, UnitId::index);
    let (&file, &source) = sources
        .paths
        .get(index)
        .zip(sources.texts.get(index))
        .ok_or(UnknownSource)?;
    // The whole source is handed over, so it starts at line 1. `line_start` is
    // for a fragment cut out of a larger file; setting it to the diagnostic's
    // own line makes every reported number come back as roughly double.
    Ok(match source {
        Some(source) => Shown::Snippet(Snippet::source(source).path(file).line_start(1)),
        None => Shown::Path(file),
    })
}

/// A location of a unit without source, by line and column
fn origin<'a>(path: &'a str, span: compile::Span) -> Origin<'a> {
    let start = span.start();
    Origin::path(path)
        .line(start.line_number() as usize)
        .char_column(start.column_number() as usize)
}

fn render_report<'a>(
    sources: Sources<'a, '_>,
    diag: &Diag,
) -> Result<Vec<Group<'a>>, UnknownSource> {
    let level = match diag.severity() {
        compile::Severity::Error => Level::ERROR,
        compile::Severity::Warning => Level::WARNING,
        other => Level::INFO.with_name(other.to_string()),
    };
    // Annotations grouped by file, the primary location's first.
    let primary_location = diag.span();
    type Located = (SnippetAnnotationKind, compile::Span, Option<String>);
    let mut files: Vec<(Option<UnitId>, Vec<Located>)> =
        vec![(primary_location.unit(), Vec::new())];
    let mut have_primary = false;
    for ann in diag.annotations() {
        let location = ann.span();
        let kind = match ann.kind() {
            compile::AnnotationKind::Primary => {
                have_primary = true;
                SnippetAnnotationKind::Primary
            }
            _ => SnippetAnnotationKind::Context,
        };
        let annotation = (kind, location.span(), Some(ann.message().to_string()));
        match files.iter_mut().find(|(unit, _)| *unit == location.unit()) {
            Some((_, annotations)) => annotations.push(annotation),
            None => files.push((location.unit(), vec![annotation])),
        }
    }
    if !have_primary {
        files[0].1.push((
            SnippetAnnotationKind::Primary,
            primary_location.span(),
            None,
        ));
    }
    let mut primary = Group::with_title(level.primary_title(diag.message().to_string()));
    for (unit, annotations) in files {
        match snippet(sources, unit)? {
            Shown::Snippet(snippet) => {
                let annotations = annotations.into_iter().map(|(kind, span, label)| {
                    let annotation: Annotation<'a> =
                        kind.span(span.start().byte_offset()..span.end().byte_offset());
                    match label {
                        Some(label) => annotation.label(label),
                        None => annotation,
                    }
                });
                primary = primary.element(snippet.annotations(annotations));
            }
            Shown::Path(path) => {
                for (_, span, label) in annotations {
                    primary = primary.element(origin(path, span));
                    if let Some(label) = label {
                        primary = primary.element(Level::NOTE.message(label));
                    }
                }
            }
        }
    }
    for note in diag.notes() {
        match note.kind() {
            compile::NoteKind::Help => {
                primary = primary.element(Level::HELP.message(note.message().to_string()))
            }
            _ => primary = primary.element(Level::NOTE.message(note.message().to_string())),
        }
    }
    let mut report = vec![primary];
    for patch in diag.patches() {
        let location = patch.span();
        let span = location.span();
        let title = Group::with_title(Level::HELP.secondary_title(patch.message().to_string()));
        report.push(match snippet(sources, location.unit())? {
            Shown::Snippet(snippet) => title.element(snippet.patch(SnippetPatch::new(
                span.start().byte_offset()..span.end().byte_offset(),
                patch.sub().to_owned(),
            ))),
            Shown::Path(path) => title.element(origin(path, span)),
        });
    }
    Ok(report)
}

fn renderer(color: ColorMode) -> Renderer {
    let term = Term::stderr();
    let renderer = if use_color(&term, color) {
        Renderer::styled()
    } else {
        Renderer::plain()
    };
    renderer
        .decor_style(DecorStyle::Unicode)
        .term_width(term.size().1 as usize)
}

/// Render a diagnostic whose locations may refer to several files.
///
/// `paths` and `texts` are indexed by unit; a location with no unit refers to
/// the first. A unit without text is located by line and column.
pub(crate) fn render_diag(
    paths: &[&str],
    texts: &[Option<&str>],
    diag: &Diag,
    color: ColorMode,
) -> Result<String, UnknownSource> {
    let sources = Sources { paths, texts };
    Ok(renderer(color).render(&render_report(sources, diag)?))
}

/// Render a diagnostic from [`Unit::diagnostics`](dolang::compile::Unit::diagnostics).
pub fn render_compile_diag(file: &str, source: &str, diag: &Diag, color: ColorMode) -> String {
    render_diag(&[file], &[Some(source)], diag, color).expect("local compiler diagnostic")
}
