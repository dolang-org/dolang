use std::{borrow::Cow, path::Path};

use anstyle::{AnsiColor, Style};
use console::Term;
use dolang::{
    compile::Diag,
    runtime::{Error, Frame, Result, Strand, Value},
};

#[derive(Clone, Copy)]
pub(crate) enum ColorMode {
    Auto,
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

fn use_color(term: &Term, color: ColorMode) -> bool {
    match color {
        ColorMode::Always => true,
        ColorMode::Auto => term.is_term() && term.features().colors_supported(),
    }
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
    out.pop();
    out
}

pub fn render_message_backtrace<I, F>(message: &str, backtrace: I) -> String
where
    I: IntoIterator<Item = F>,
    F: Frame,
{
    let cwd = std::env::current_dir().ok();
    let frames = collect_frames(backtrace);
    render_message_backtrace_frames(message, &frames, ColorMode::Auto, cwd.as_deref())
}

pub(crate) fn render_error_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    error: &Value<'v>,
    backtrace: Option<&Value<'v>>,
) -> Result<'v, 's, String> {
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
    Ok(render_message_backtrace_frames(
        &message,
        &frames,
        ColorMode::Always,
        cwd.as_deref(),
    ))
}

pub fn print_error_stderr<'v, 's>(strand: &mut Strand<'v, 's>, error: Error<'v, 's>) {
    let message = error.display(strand).to_string();
    let rendered = render_message_backtrace(&message, error.backtrace());
    Term::stderr().write_line(&rendered).unwrap();
}

pub fn print_compile_diag_stderr(file: &str, source: &str, diag: &Diag) {
    let rendered = dolang_ext_compile::render_compile_diag(
        file,
        source,
        diag,
        dolang_ext_compile::ColorMode::Auto,
    );
    Term::stderr().write_line(&rendered).unwrap();
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

    impl Frame for TestFrame<'_> {
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
        );
        let rendered = console::strip_ansi_codes(&rendered);
        assert!(rendered.contains("foo/bar.dol:5"));
        assert!(!rendered.contains(&cwd.display().to_string()));
    }
}
