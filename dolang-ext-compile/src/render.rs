use annotate_snippets::{
    Annotation, AnnotationKind as SnippetAnnotationKind, Group, Level, Origin,
    Patch as SnippetPatch, Renderer, Snippet, renderer::DecorStyle,
};
use std::ops::Range;

use console::Term;
use dolang::compile::{self, Diag, SourceSpan, Unit, UnitId};

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
/// checked from its typelib has a path but no text. `ranges` gives where each
/// location of the diagnostic in a unit with text lies in it.
#[derive(Clone, Copy)]
struct Sources<'a, 'b> {
    paths: &'b [&'a str],
    texts: &'b [Option<&'a str>],
    ranges: &'b Ranges,
}

/// Where each location of a diagnostic in a unit with source lies in that source,
/// in bytes
pub(crate) type Ranges = Vec<(SourceSpan, Range<usize>)>;

/// Where each location of `diag` lies in the source of its unit, as `unit` gives
/// it by the location's unit
pub(crate) fn ranges<'u>(
    diag: &Diag,
    unit: impl Fn(Option<UnitId>) -> Option<&'u Unit<'u>>,
) -> Ranges {
    let locations = [diag.span()]
        .into_iter()
        .chain(diag.annotations().map(|annotation| annotation.span()))
        .chain(diag.patches().map(|patch| patch.span()));
    locations
        .filter_map(|location| {
            let range = unit(location.unit())?.byte_range(location.span());
            Some((location, range))
        })
        .collect()
}

impl Sources<'_, '_> {
    fn range(&self, location: &SourceSpan) -> Range<usize> {
        (self.ranges.iter())
            .find(|(located, _)| located == location)
            .map(|(_, range)| range.clone())
            .expect("each location in a unit with source has a range")
    }
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
    type Located = (SnippetAnnotationKind, SourceSpan, Option<String>);
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
        let annotation = (kind, location.clone(), Some(ann.message().to_string()));
        match files.iter_mut().find(|(unit, _)| *unit == location.unit()) {
            Some((_, annotations)) => annotations.push(annotation),
            None => files.push((location.unit(), vec![annotation])),
        }
    }
    if !have_primary {
        files[0]
            .1
            .push((SnippetAnnotationKind::Primary, primary_location, None));
    }
    let mut primary = Group::with_title(level.primary_title(diag.message().to_string()));
    for (unit, annotations) in files {
        match snippet(sources, unit)? {
            Shown::Snippet(snippet) => {
                let annotations = annotations.into_iter().map(|(kind, location, label)| {
                    let annotation: Annotation<'a> = kind.span(sources.range(&location));
                    match label {
                        Some(label) => annotation.label(label),
                        None => annotation,
                    }
                });
                primary = primary.element(snippet.annotations(annotations));
            }
            Shown::Path(path) => {
                for (_, location, label) in annotations {
                    primary = primary.element(origin(path, location.span()));
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
                sources.range(&location),
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
    ranges: &Ranges,
    diag: &Diag,
    color: ColorMode,
) -> Result<String, UnknownSource> {
    let sources = Sources {
        paths,
        texts,
        ranges,
    };
    Ok(renderer(color).render(&render_report(sources, diag)?))
}

/// Render a diagnostic from [`Check::diagnostics`](dolang::compile::typeck::Check::diagnostics).
///
/// `paths` and `units` are indexed by the checked units' [`UnitId`]s; a unit
/// checked from its typelib has no [`Unit`], and is located by line and column.
///
/// # Panics
///
/// If the diagnostic refers to a unit beyond `paths` or `units`.
pub fn render_check_diag(
    paths: &[&str],
    units: &[Option<&Unit<'_>>],
    diag: &Diag,
    color: ColorMode,
) -> String {
    let texts: Vec<_> = units.iter().map(|unit| unit.map(Unit::source)).collect();
    let ranges = ranges(diag, |id| *units.get(id.map_or(0, UnitId::index))?);
    render_diag(paths, &texts, &ranges, diag, color).expect("diagnostic of a checked unit")
}

/// Render a diagnostic from [`Unit::diagnostics`](dolang::compile::Unit::diagnostics).
pub fn render_compile_diag(file: &str, unit: &Unit<'_>, diag: &Diag, color: ColorMode) -> String {
    let ranges = ranges(diag, |_| Some(unit));
    render_diag(&[file], &[Some(unit.source())], &ranges, diag, color)
        .expect("local compiler diagnostic")
}
