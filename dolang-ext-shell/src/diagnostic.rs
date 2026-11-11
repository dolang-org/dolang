use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    fs,
    ops::Range,
    path::Path,
};

use annotate_snippets::{AnnotationKind, Group, Level, Renderer, Snippet, renderer::DecorStyle};
use anstyle::{AnsiColor, Style};
use dolang::{
    compile::{Diag, Unit},
    runtime::{Error, Frame, Result, Strand, Value, vm::Span},
};

#[derive(Clone)]
struct RenderedFrame {
    module: String,
    receiver: String,
    method: Option<String>,
    source: Option<(String, Span)>,
}

/// A source file a backtrace shows code from
struct SourceFile {
    text: String,
    /// Where each line starts, in bytes
    line_starts: Vec<usize>,
}

impl SourceFile {
    fn open(path: &Path) -> Option<Self> {
        let text = fs::read_to_string(path).ok()?;
        let line_starts = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(offset, _)| offset + 1))
            .collect();
        Some(Self { text, line_starts })
    }

    /// Where `span` lies in the text, in bytes. A span past the end of the
    /// file, which may have changed since it ran, stops short.
    fn byte_range(&self, span: Span) -> Range<usize> {
        let start = span.start();
        let line = (self.line_starts.get(start.line_offset() as usize))
            .map_or(self.text.len(), |&line| line);
        let advance = |from: usize, count: u32| {
            (self.text[from..].char_indices())
                .nth(count as usize)
                .map_or(self.text.len(), |(offset, _)| from + offset)
        };
        let begin = advance(line, start.column_offset());
        begin..advance(begin, span.len())
    }
}

#[derive(Default)]
struct SourceCache {
    files: HashMap<String, Option<SourceFile>>,
}

impl SourceCache {
    /// The code `span` covers in the file at `path`, marked, in lines of at most
    /// `width` columns
    fn render_snippet(&mut self, path: &str, span: Span, width: Option<usize>) -> Option<String> {
        let file = self
            .files
            .entry(path.to_owned())
            .or_insert_with(|| SourceFile::open(Path::new(path)))
            .as_ref()?;
        let snippet = Snippet::source(file.text.as_str())
            .line_start(1)
            .annotation(AnnotationKind::Primary.span(file.byte_range(span)));
        let mut renderer = Renderer::styled().decor_style(DecorStyle::Unicode);
        if let Some(width) = width {
            renderer = renderer.term_width(width);
        }
        Some(renderer.render(&[Group::with_level(Level::NOTE).element(snippet)]))
    }
}

fn render_styled(style: Style, value: impl std::fmt::Display) -> String {
    format!("{style}{value}{}", style.render_reset())
}

fn collect_frames<I, F>(backtrace: I) -> Vec<RenderedFrame>
where
    I: IntoIterator<Item = F>,
    F: Frame,
{
    backtrace
        .into_iter()
        .map(|entry| RenderedFrame {
            module: entry.module().into_owned(),
            receiver: entry.receiver().into_owned(),
            method: entry.method().map(Cow::into_owned),
            source: entry.source().map(|(path, span)| (path.into_owned(), span)),
        })
        .collect()
}

/// Append the header line of `entry`, and, when `code` gives an indent and
/// width, its code in lines of at most that width
fn render_frame_into(
    out: &mut String,
    entry: &RenderedFrame,
    cwd: Option<&Path>,
    source_cache: &mut SourceCache,
    code: Option<(&str, Option<usize>)>,
) {
    out.push_str(&render_styled(
        Style::new().fg_color(Some(AnsiColor::White.into())),
        &entry.module,
    ));
    out.push_str(&render_styled(Style::new().dimmed(), "."));
    if let Some(method) = &entry.method {
        out.push_str(&render_styled(
            Style::new().fg_color(Some(AnsiColor::White.into())),
            &entry.receiver,
        ));
        out.push_str(&render_styled(Style::new().dimmed(), "."));
        out.push_str(&render_styled(
            Style::new().fg_color(Some(AnsiColor::Blue.into())),
            method,
        ));
    } else {
        out.push_str(&render_styled(
            Style::new().fg_color(Some(AnsiColor::Blue.into())),
            &entry.receiver,
        ));
    }
    let Some((source_path, span)) = &entry.source else {
        return;
    };
    let path = Path::new(source_path.as_str());
    let path = cwd
        .and_then(|cwd| path.strip_prefix(cwd).ok())
        .unwrap_or(path);
    let start = span.start();
    let colon = render_styled(Style::new().fg_color(Some(AnsiColor::Yellow.into())), ":");
    let number = |n: u32| render_styled(Style::new().fg_color(Some(AnsiColor::Magenta.into())), n);
    out.push(' ');
    out.push_str(&render_styled(Style::new().dimmed(), "at"));
    out.push(' ');
    out.push_str(&path.display().to_string());
    out.push_str(&colon);
    out.push_str(&number(start.line_number()));
    out.push_str(&colon);
    out.push_str(&number(start.column_number()));
    if let Some((indent, width)) = code
        && let Some(snippet) = source_cache.render_snippet(source_path, *span, width)
    {
        for line in snippet.lines() {
            out.push('\n');
            if !line.is_empty() {
                out.push_str(indent);
                out.push_str(line);
            }
        }
    }
}

/// Append the frames of a backtrace, numbered, each frame's code shown once
/// in lines of at most `cols` columns
fn render_frames(
    out: &mut String,
    backtrace: &[RenderedFrame],
    cwd: Option<&Path>,
    cols: Option<u16>,
) {
    let width = backtrace.len().saturating_sub(1).to_string().len();
    let source_indent = " ".repeat(width + 4);
    let snippet_width = cols.map(|cols| usize::from(cols).saturating_sub(source_indent.len()));
    let mut source_cache = SourceCache::default();
    let mut rendered_sources = HashSet::new();
    for (i, entry) in backtrace.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str("  ");
        out.push_str(&format!(
            "{}{}",
            render_styled(
                Style::new().fg_color(Some(AnsiColor::Magenta.into())),
                format!("{i:>width$}")
            ),
            render_styled(Style::new().fg_color(Some(AnsiColor::Yellow.into())), ":")
        ));
        out.push(' ');
        let code = entry
            .source
            .as_ref()
            .filter(|source| rendered_sources.insert(*source))
            .map(|_| (source_indent.as_str(), snippet_width));
        render_frame_into(out, entry, cwd, &mut source_cache, code);
    }
}

/// Render `message` and the frames of its backtrace, each frame's code shown
/// once in lines of at most `cols` columns
fn render_message_backtrace_frames(
    message: &str,
    backtrace: &[RenderedFrame],
    cwd: Option<&Path>,
    cols: Option<u16>,
) -> String {
    let mut out = String::new();
    out.push_str(&render_styled(
        Style::new().fg_color(Some(AnsiColor::Red.into())).bold(),
        message,
    ));
    if !backtrace.is_empty() {
        out.push('\n');
        render_frames(&mut out, backtrace, cwd, cols);
    }
    out
}

/// Render `message` and its backtrace as an uncaught error is reported, in
/// lines of at most `cols` columns
pub fn render_message_backtrace<I, F>(message: &str, backtrace: I, cols: Option<u16>) -> String
where
    I: IntoIterator<Item = F>,
    F: Frame,
{
    let cwd = std::env::current_dir().ok();
    let frames = collect_frames(backtrace);
    render_message_backtrace_frames(message, &frames, cwd.as_deref(), cols)
}

/// Render `error` with `backtrace`: by default that of the active handled
/// exception, if any, and none for `nil`
pub(crate) fn render_error_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    error: &Value<'v>,
    backtrace: Option<&Value<'v>>,
) -> Result<'v, 's, String> {
    let message = error.to_string(strand)?;
    let frames = match backtrace {
        Some(backtrace) if backtrace.is_nil() => Vec::new(),
        Some(backtrace) => {
            let Some(backtrace) = backtrace.as_backtrace(strand) else {
                return Err(Error::type_error(strand, "expected inspect.Backtrace"));
            };
            collect_frames(backtrace)
        }
        None => strand
            .error_backtrace()
            .map_or_else(Vec::new, collect_frames),
    };
    let cwd = std::env::current_dir().ok();
    Ok(render_message_backtrace_frames(
        &message,
        &frames,
        cwd.as_deref(),
        crate::stderr_cols(strand),
    ))
}

/// Render `backtrace` as [`render_error_value`] renders the backtrace of an
/// error
pub(crate) fn render_backtrace_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    backtrace: &Value<'v>,
) -> Result<'v, 's, String> {
    let Some(backtrace) = backtrace.as_backtrace(strand) else {
        return Err(Error::type_error(strand, "expected inspect.Backtrace"));
    };
    let frames = collect_frames(backtrace);
    let cwd = std::env::current_dir().ok();
    let mut out = String::new();
    render_frames(
        &mut out,
        &frames,
        cwd.as_deref(),
        crate::stderr_cols(strand),
    );
    Ok(out)
}

/// Render the `inspect.Frame` `frame` on its own, and its code if `code`
pub(crate) fn render_frame_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    frame: &Value<'v>,
    code: bool,
) -> Result<'v, 's, String> {
    let Some(frame) = frame.as_backtrace_frame(strand) else {
        return Err(Error::type_error(strand, "expected inspect.Frame"));
    };
    let frames = collect_frames([frame]);
    let cwd = std::env::current_dir().ok();
    const INDENT: &str = "  ";
    let width =
        crate::stderr_cols(strand).map(|cols| usize::from(cols).saturating_sub(INDENT.len()));
    let mut out = String::new();
    render_frame_into(
        &mut out,
        &frames[0],
        cwd.as_deref(),
        &mut SourceCache::default(),
        code.then_some((INDENT, width)),
    );
    Ok(out)
}

async fn write_preformatted_stderr<'v, 's>(
    strand: &mut Strand<'v, 's>,
    rendered: &str,
) -> Result<'v, 's, ()> {
    let ansi = dolang_ext_term::ansi_enabled(strand);
    let rendered = dolang_ext_term::filter_preformatted(strand, rendered, ansi)?;
    dolang_ext_term::writeln(strand, rendered.as_bytes()).await?;
    dolang_ext_term::flush(strand).await
}

pub async fn print_error_stderr<'v, 's>(
    strand: &mut Strand<'v, 's>,
    error: Error<'v, 's>,
) -> Result<'v, 's, ()> {
    let message = error.display(strand).to_string();
    let rendered =
        render_message_backtrace(&message, error.backtrace(), crate::stderr_cols(strand));
    drop(error);
    write_preformatted_stderr(strand, &rendered).await
}

pub async fn print_compile_diag_stderr<'v, 's>(
    strand: &mut Strand<'v, 's>,
    file: &str,
    unit: &Unit<'_>,
    diag: &Diag,
) -> Result<'v, 's, ()> {
    let rendered = dolang_ext_compile::render_compile_diag(
        file,
        unit,
        diag,
        dolang_ext_compile::ColorMode::Always,
    );
    write_preformatted_stderr(strand, &rendered).await
}

/// Print a diagnostic from a check to stderr, as
/// [`render_check_diag`](dolang_ext_compile::render_check_diag) renders it.
pub async fn print_check_diag_stderr<'v, 's>(
    strand: &mut Strand<'v, 's>,
    paths: &[&str],
    units: &[Option<&Unit<'_>>],
    diag: &Diag,
) -> Result<'v, 's, ()> {
    let rendered = dolang_ext_compile::render_check_diag(
        paths,
        units,
        diag,
        dolang_ext_compile::ColorMode::Always,
    );
    write_preformatted_stderr(strand, &rendered).await
}
