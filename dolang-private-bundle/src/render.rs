use annotate_snippets::{
    AnnotationKind as SnippetAnnotationKind, Group, Level, Patch as SnippetPatch, Renderer, Snippet,
};
use dolang_compile::{
    Unit, UnitId,
    diag::{self as compile, Diag},
};

/// A unit's file, as the compiler sees its path, and the unit, by its ID
pub(crate) type Other<'a> = &'a dyn Fn(UnitId) -> Option<(&'a str, &'a Unit<'a>)>;

fn render_report<'a>(
    file: &'a str,
    unit: &'a Unit<'_>,
    diag: &'a Diag,
    other: Other<'a>,
) -> Vec<Group<'a>> {
    let source = unit.source();
    let own = diag.span().unit();
    let level = match diag.severity() {
        compile::Severity::Error => Level::ERROR,
        compile::Severity::Warning => Level::WARNING,
        other => Level::INFO.with_name(other.to_string()),
    };
    // The whole source is handed over, so it starts at line 1. `line_start` is
    // for a fragment cut out of a larger file; setting it to the diagnostic's
    // own line makes every reported number come back as roughly double.
    let mut snippet = Snippet::source(source).path(file).line_start(1);
    // Another unit's annotations, each unit's in a snippet of its own
    let mut others: Vec<(UnitId, &Unit<'_>, Snippet<'a, _>)> = Vec::new();
    let mut have_primary = false;
    for ann in diag.annotations() {
        let location = ann.span();
        if let Some(at) = location.unit()
            && location.unit() != own
        {
            let at_index = match others.iter().position(|(unit, ..)| *unit == at) {
                Some(index) => index,
                None => {
                    let Some((file, found)) = other(at) else {
                        continue;
                    };
                    let snippet = Snippet::source(found.source()).path(file).line_start(1);
                    others.push((at, found, snippet));
                    others.len() - 1
                }
            };
            let (_, found, snippet) = &mut others[at_index];
            let range = found.byte_range(location.span());
            let taken = std::mem::replace(snippet, Snippet::source(""));
            *snippet = taken.annotation(
                SnippetAnnotationKind::Context
                    .span(range)
                    .label(ann.message().to_string()),
            );
            continue;
        }
        snippet = snippet.annotation(
            match ann.kind() {
                compile::AnnotationKind::Primary => {
                    have_primary = true;
                    SnippetAnnotationKind::Primary
                }
                _ => SnippetAnnotationKind::Context,
            }
            .span(unit.byte_range(ann.span().span()))
            .label(ann.message().to_string()),
        );
    }
    if !have_primary {
        snippet = snippet
            .annotation(SnippetAnnotationKind::Primary.span(unit.byte_range(diag.span().span())));
    }
    let mut primary = level
        .primary_title(diag.message().to_string())
        .element(snippet);
    for (_, _, snippet) in others {
        primary = primary.element(snippet);
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
        report.push(
            Group::with_title(Level::HELP.secondary_title(patch.message().to_string())).element(
                Snippet::source(source).path(file).patch(SnippetPatch::new(
                    unit.byte_range(patch.span().span()),
                    patch.sub().to_owned(),
                )),
            ),
        );
    }
    report
}

/// Render `diag`, whose own span is in `unit`, finding each other unit it points
/// into with `other`
pub(crate) fn render_diag<'a>(
    file: &'a str,
    unit: &'a Unit<'_>,
    diag: &'a Diag,
    other: Other<'a>,
) -> String {
    Renderer::plain().render(&render_report(file, unit, diag, other))
}
