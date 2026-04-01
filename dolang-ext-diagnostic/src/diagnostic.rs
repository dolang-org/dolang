use std::{borrow::Cow, io::Write, path::Path};

use annotate_snippets::{
    AnnotationKind as SnippetAnnotationKind, Group, Level, Patch as SnippetPatch, Renderer,
    Snippet, renderer::DecorStyle,
};
use anstyle::{AnsiColor, Style};
use console::Term;
use dolang::{
    compile::{self, Diag},
    runtime::{Error, Frame, Output, Result, Slot, Strand, Value, unpack, vm::Builder},
};

#[derive(Clone, Copy)]
pub enum ColorMode {
    Auto,
    Never,
    Always,
}

#[derive(Clone)]
struct RenderedFrame {
    module: String,
    receiver: String,
    method: Option<String>,
    source: Option<(String, u32)>,
}

fn render_styled(style: Style, value: impl std::fmt::Display) -> String {
    format!("{style}{value}{}", style.render_reset())
}

fn color_mode<'v, 's>(
    strand: &mut Strand<'v, 's>,
    color: Option<&Value<'v>>,
) -> Result<'v, 's, ColorMode> {
    let Some(color) = color else {
        return Ok(ColorMode::Auto);
    };
    let Some(sym) = color.as_sym(strand) else {
        return Err(Error::type_error(
            strand,
            "color: expected :auto:, :never:, or :always:",
        ));
    };
    match sym.as_str(strand) {
        "auto" => Ok(ColorMode::Auto),
        "never" => Ok(ColorMode::Never),
        "always" => Ok(ColorMode::Always),
        _ => Err(Error::type_error(
            strand,
            "color: expected :auto:, :never:, or :always:",
        )),
    }
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

pub fn print_compile_diag_stderr(file: &str, source: &str, diag: &Diag, color: ColorMode) {
    Term::stderr()
        .write_line(&render_compile_diag(file, source, diag, color))
        .unwrap();
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
            source: entry.source().map(|(path, line)| (path.into_owned(), line)),
        })
        .collect()
}

fn render_message_backtrace_frames(
    message: &str,
    backtrace: &[RenderedFrame],
    color: ColorMode,
    cwd: Option<&Path>,
) -> String {
    let term = Term::stderr();
    let use_color = use_color(&term, color);
    let mut out = String::new();
    out.push_str(&if use_color {
        render_styled(
            Style::new().fg_color(Some(AnsiColor::Red.into())).bold(),
            message,
        )
    } else {
        message.to_owned()
    });
    out.push('\n');

    let width = backtrace.len().saturating_sub(1).to_string().len();
    for (i, entry) in backtrace.iter().enumerate() {
        out.push_str("  ");
        out.push_str(&if use_color {
            format!(
                "{}{}",
                render_styled(
                    Style::new().fg_color(Some(AnsiColor::Magenta.into())),
                    format!("{i:>width$}")
                ),
                render_styled(Style::new().fg_color(Some(AnsiColor::Yellow.into())), ":")
            )
        } else {
            format!("{i:>width$}:")
        });
        out.push(' ');
        out.push_str(&if use_color {
            render_styled(
                Style::new().fg_color(Some(AnsiColor::White.into())),
                &entry.module,
            )
        } else {
            entry.module.clone()
        });
        out.push_str(&if use_color {
            render_styled(Style::new().dimmed(), ".")
        } else {
            ".".to_owned()
        });
        if let Some(method) = &entry.method {
            out.push_str(&if use_color {
                render_styled(
                    Style::new().fg_color(Some(AnsiColor::White.into())),
                    &entry.receiver,
                )
            } else {
                entry.receiver.clone()
            });
            out.push_str(&if use_color {
                render_styled(Style::new().dimmed(), ".")
            } else {
                ".".to_owned()
            });
            out.push_str(&if use_color {
                render_styled(Style::new().fg_color(Some(AnsiColor::Blue.into())), method)
            } else {
                method.clone()
            });
        } else {
            out.push_str(&if use_color {
                render_styled(
                    Style::new().fg_color(Some(AnsiColor::Blue.into())),
                    &entry.receiver,
                )
            } else {
                entry.receiver.clone()
            });
        }
        if let Some((path, line)) = &entry.source {
            let path = Path::new(path.as_str());
            let path = cwd
                .and_then(|cwd| path.strip_prefix(cwd).ok())
                .unwrap_or(path);
            out.push(' ');
            out.push_str(&if use_color {
                render_styled(Style::new().dimmed(), "at")
            } else {
                "at".to_owned()
            });
            out.push(' ');
            out.push_str(&path.display().to_string());
            out.push_str(&if use_color {
                render_styled(Style::new().fg_color(Some(AnsiColor::Yellow.into())), ":")
            } else {
                ":".to_owned()
            });
            out.push_str(&if use_color {
                render_styled(
                    Style::new().fg_color(Some(AnsiColor::Magenta.into())),
                    format!("{}", line + 1),
                )
            } else {
                format!("{}", line + 1)
            });
        }
        out.push('\n');
    }

    out
}

pub fn render_message_backtrace<I, F>(message: &str, backtrace: I, color: ColorMode) -> String
where
    I: IntoIterator<Item = F>,
    F: Frame,
{
    let cwd = std::env::current_dir().ok();
    let frames = collect_frames(backtrace);
    render_message_backtrace_frames(message, &frames, color, cwd.as_deref())
}

pub fn print_error_stderr<'v, 's>(
    strand: &mut Strand<'v, 's>,
    error: Error<'v, 's>,
    color: ColorMode,
) {
    let message = error.display(strand).to_string();
    let rendered = render_message_backtrace(&message, error.backtrace(), color);
    Term::stderr().write_all(rendered.as_bytes()).unwrap();
}

pub(crate) fn configure<'v>(builder: &mut Builder<'v>) {
    let backtrace_key = builder.sym("backtrace");
    let color_key = builder.sym("color");
    builder
        .module("diagnostic")
        .function("print_compile_diag", async move |strand, args, _out| {
            let ([diag], [color]) = unpack!(strand, args, 1, 0, color_key = None)?;
            let color = color_mode(strand, color.as_deref())?;
            strand.with_slots_sync(|strand, [mut source]| {
                let (diag, path) = dolang_ext_compile::extract_diagnostic(
                    strand,
                    &diag,
                    Slot::reborrow(&mut source),
                )?;
                let source = source
                    .as_u8_slice(strand)
                    .ok_or_else(|| Error::type_error(strand, "source: expected str or bin"))?;
                let source = std::str::from_utf8(source)
                    .map_err(|_| Error::type_error(strand, "source: expected valid utf-8"))?;
                print_compile_diag_stderr(path, source, diag, color);
                Ok(())
            })
        })
        .function("render_compile_diag", async move |strand, args, out| {
            let ([diag], [color]) = unpack!(strand, args, 1, 0, color_key = None)?;
            let color = color_mode(strand, color.as_deref())?;
            strand.with_slots_sync(|strand, [mut source]| {
                let (diag, path) = dolang_ext_compile::extract_diagnostic(
                    strand,
                    &diag,
                    Slot::reborrow(&mut source),
                )?;
                let source = source
                    .as_u8_slice(strand)
                    .ok_or_else(|| Error::type_error(strand, "source: expected str or bin"))?;
                let source = std::str::from_utf8(source)
                    .map_err(|_| Error::type_error(strand, "source: expected valid utf-8"))?;
                let rendered = render_compile_diag(path, source, diag, color);
                Output::set(strand, out, rendered.as_str());
                Ok(())
            })
        })
        .function("print_error", async move |strand, args, _out| {
            let ([error], [backtrace, color]) =
                unpack!(strand, args, 1, 0, backtrace_key = None, color_key = None)?;
            let color = color_mode(strand, color.as_deref())?;
            let message = error.to_string(strand)?;
            let frames = if let Some(backtrace) = backtrace {
                let Some(backtrace) = backtrace.as_backtrace(strand) else {
                    return Err(Error::type_error(strand, "expected strand.Backtrace"));
                };
                collect_frames(backtrace)
            } else {
                let Some(backtrace) = strand.error_backtrace() else {
                    return Err(Error::state_error(strand, "no active handled exception"));
                };
                collect_frames(backtrace)
            };
            let cwd = std::env::current_dir().ok();
            let rendered =
                render_message_backtrace_frames(&message, &frames, color, cwd.as_deref());
            Term::stderr().write_all(rendered.as_bytes()).unwrap();
            Ok(())
        })
        .function("render_error", async move |strand, args, out| {
            let ([error], [backtrace, color]) =
                unpack!(strand, args, 1, 0, backtrace_key = None, color_key = None)?;
            let color = color_mode(strand, color.as_deref())?;
            let message = error.to_string(strand)?;
            let frames = if let Some(backtrace) = backtrace {
                let Some(backtrace) = backtrace.as_backtrace(strand) else {
                    return Err(Error::type_error(strand, "expected strand.Backtrace"));
                };
                collect_frames(backtrace)
            } else {
                let Some(backtrace) = strand.error_backtrace() else {
                    return Err(Error::state_error(strand, "no active handled exception"));
                };
                collect_frames(backtrace)
            };
            let cwd = std::env::current_dir().ok();
            let rendered =
                render_message_backtrace_frames(&message, &frames, color, cwd.as_deref());
            Output::set(strand, out, rendered.as_str());
            Ok(())
        })
        .commit();
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestFrame<'a> {
        module: &'a str,
        receiver: &'a str,
        method: Option<&'a str>,
        source: Option<(&'a str, u32)>,
    }

    impl<'a> Frame for TestFrame<'a> {
        fn source(&self) -> Option<(Cow<'_, str>, u32)> {
            self.source.map(|(path, line)| (Cow::Borrowed(path), line))
        }

        fn receiver(&self) -> Cow<'_, str> {
            Cow::Borrowed(self.receiver)
        }

        fn method(&self) -> Option<Cow<'_, str>> {
            self.method.map(Cow::Borrowed)
        }

        fn module(&self) -> Cow<'_, str> {
            Cow::Borrowed(self.module)
        }
    }

    #[test]
    fn render_message_backtrace_relativizes_against_process_cwd() {
        let cwd = std::env::current_dir().unwrap();
        let path = cwd.join("foo/bar.dol");
        let path_str = path.to_string_lossy();
        let rendered = render_message_backtrace(
            "boom",
            [TestFrame {
                module: "m",
                receiver: "f",
                method: None,
                source: Some((&path_str, 4)),
            }],
            ColorMode::Never,
        );
        assert!(rendered.contains("foo/bar.dol:5"));
        assert!(!rendered.contains(&cwd.display().to_string()));
    }
}
