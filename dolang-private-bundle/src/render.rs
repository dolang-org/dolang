use annotate_snippets::{
    AnnotationKind as SnippetAnnotationKind, Group, Level, Patch as SnippetPatch, Renderer, Snippet,
};
use dolang::compile::{self, Diag};

fn render_report<'a>(file: &'a str, source: &'a str, diag: &'a Diag) -> Vec<Group<'a>> {
    let level = match diag.severity() {
        compile::Severity::Error => Level::ERROR,
        compile::Severity::Warning => Level::WARNING,
        other => Level::INFO.with_name(other.to_string()),
    };
    // The whole source is handed over, so it starts at line 1. `line_start` is
    // for a fragment cut out of a larger file; setting it to the diagnostic's
    // own line makes every reported number come back as roughly double.
    let mut snippet = Snippet::source(source).path(file).line_start(1);
    let mut have_primary = false;
    for ann in diag.annotations() {
        snippet = snippet.annotation(
            match ann.kind() {
                compile::AnnotationKind::Primary => {
                    have_primary = true;
                    SnippetAnnotationKind::Primary
                }
                _ => SnippetAnnotationKind::Context,
            }
            .span(ann.span().span().start().byte_offset()..ann.span().span().end().byte_offset())
            .label(ann.message().to_string()),
        );
    }
    if !have_primary {
        snippet = snippet.annotation(SnippetAnnotationKind::Primary.span(
            diag.span().span().start().byte_offset()..diag.span().span().end().byte_offset(),
        ));
    }
    let mut primary = level
        .primary_title(diag.message().to_string())
        .element(snippet);
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
                    patch.span().span().start().byte_offset()
                        ..patch.span().span().end().byte_offset(),
                    patch.sub().to_owned(),
                )),
            ),
        );
    }
    report
}

pub(crate) fn render_diag(file: &str, source: &str, diag: &Diag) -> String {
    Renderer::plain().render(&render_report(file, source, diag))
}
