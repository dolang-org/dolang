use annotate_snippets::{
    AnnotationKind as SnippetAnnotationKind, Group, Level, Patch as SnippetPatch, Renderer,
    Snippet, renderer::DecorStyle,
};
use console::Term;
use dolang::compile::{self, Diag};

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

fn render_report<'a>(file: &'a str, source: &'a str, diag: &'a Diag) -> Vec<Group<'a>> {
    let level = match diag.severity() {
        compile::Severity::Error => Level::ERROR,
        compile::Severity::Warning => Level::WARNING,
        other => Level::INFO.with_name(other.to_string()),
    };
    let mut snippet = Snippet::source(source)
        .path(file)
        .line_start(diag.span().start().line_number() as usize);
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
            .span(ann.span().start().byte_offset()..ann.span().end().byte_offset())
            .label(ann.message().to_string()),
        );
    }
    if !have_primary {
        snippet = snippet.annotation(
            SnippetAnnotationKind::Primary
                .span(diag.span().start().byte_offset()..diag.span().end().byte_offset()),
        );
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
                    patch.span().start().byte_offset()..patch.span().end().byte_offset(),
                    patch.sub().to_owned(),
                )),
            ),
        );
    }
    report
}

pub fn render_compile_diag(file: &str, source: &str, diag: &Diag, color: ColorMode) -> String {
    let term = Term::stderr();
    let mut renderer = if use_color(&term, color) {
        Renderer::styled()
    } else {
        Renderer::plain()
    };
    renderer = renderer
        .decor_style(DecorStyle::Unicode)
        .term_width(term.size().1 as usize);
    renderer.render(&render_report(file, source, diag))
}
