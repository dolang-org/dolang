use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    fs,
    ops::{ControlFlow, Range},
    path::Path,
};

use anstyle::{AnsiColor, Style};
use console::Term;
use dolang::{
    compile::{Compiler, Diag},
    runtime::{Error, Frame, Result, Strand, Value},
};

use crate::syntax::{SemanticToken, highlight_range};

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

struct SourceFile {
    source: String,
    tokens: Vec<SemanticToken>,
}

impl SourceFile {
    fn open(path: &Path) -> Option<Self> {
        let source = fs::read_to_string(path).ok()?;
        let mut tokens = Vec::new();
        let _ = Compiler::new(path, source.as_bytes()).analyze(
            &mut |_: Diag| ControlFlow::<()>::Continue(()),
            &mut |token, span, origin, context| {
                tokens.push((token, span, origin, context));
                ControlFlow::<()>::Continue(())
            },
        );
        Some(Self { source, tokens })
    }

    fn line_range(&self, line: u32) -> Option<Range<usize>> {
        let wanted = usize::try_from(line).ok()?;
        let mut start = 0;
        for (index, value) in self.source.split_inclusive('\n').enumerate() {
            let end = start + value.len();
            if index == wanted {
                let mut end = end - usize::from(value.ends_with('\n'));
                if end > start && self.source.as_bytes()[end - 1] == b'\r' {
                    end -= 1;
                }
                return Some(start..end);
            }
            start = end;
        }
        None
    }
}

#[derive(Default)]
struct SourceCache {
    files: HashMap<String, Option<SourceFile>>,
}

impl SourceCache {
    fn render_line(&mut self, path: &str, line: u32, color: bool) -> Option<String> {
        let file = self
            .files
            .entry(path.to_owned())
            .or_insert_with(|| SourceFile::open(Path::new(path)))
            .as_ref()?;
        let range = file.line_range(line)?;
        Some(highlight_range(&file.source, &file.tokens, range, color))
    }
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
    let source_indent = " ".repeat(width + 4);
    let mut source_cache = SourceCache::default();
    let mut rendered_sources = HashSet::new();
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
        if let Some((source_path, line)) = &entry.source {
            let source_key = (source_path.clone(), *line);
            let path = Path::new(source_path.as_str());
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
            if rendered_sources.insert(source_key)
                && let Some(source) = source_cache.render_line(source_path, *line, use_color)
            {
                out.push('\n');
                out.push_str(&source_indent);
                out.push_str(&source);
            }
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

    #[test]
    fn render_message_backtrace_includes_each_source_line_once() {
        let path = std::env::temp_dir().join(format!(
            "dolang-backtrace-source-{}.dol",
            std::process::id()
        ));
        fs::write(&path, "def fail()\n  throw \"boom\"\n").unwrap();
        let path_str = path.to_string_lossy();
        let frames = [
            RenderedFrame {
                module: "m".to_owned(),
                receiver: "fail".to_owned(),
                method: None,
                source: Some((path_str.to_string(), 1)),
            },
            RenderedFrame {
                module: "m".to_owned(),
                receiver: "fail".to_owned(),
                method: None,
                source: Some((path_str.to_string(), 1)),
            },
        ];
        let rendered = render_message_backtrace_frames(
            "boom",
            &frames,
            ColorMode::Always,
            Some(Path::new("/")),
        );
        fs::remove_file(path).unwrap();

        assert!(rendered.contains("\u{1b}["));
        let plain = console::strip_ansi_codes(&rendered);
        assert_eq!(plain.matches("  throw \"boom\"").count(), 1);
    }
}
