use std::{
    borrow::Cow,
    fmt::{self, Write},
    ops::ControlFlow,
    path::Path,
    string::String,
};

use dolang::{
    compile::{Context, Diag, Mode, Origin, Span, Token},
    runtime::{
        Bytecode, Error, Instance, Object, Result, Slot, Strand, Sym, error::ResultExt, value::Root,
    },
};

use rustyline::{
    Editor, Helper,
    completion::{Candidate, Completer},
    config::Builder,
    error::ReadlineError,
    highlight::Highlighter,
    hint::{Hint, Hinter},
    history::DefaultHistory,
    validate::{ValidationContext, ValidationResult, Validator},
};

use anstyle::{AnsiColor, Style};

use crate::{diagnostic, load};

pub(crate) const DYNAMIC_PRELUDE: &str = "$dynamic$";

fn render_styled(style: Style, value: impl std::fmt::Display) -> String {
    format!("{style}{value}{}", style.render_reset())
}

pub(crate) struct DynamicPrelude<'v> {
    pub(crate) root: Root<'v>,
}

impl<'v> Object<'v> for DynamicPrelude<'v> {
    const NAME: &'v str = "Prelude";
    const MODULE: &'v str = "<repl>";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        Self::debug(this, strand, w)
    }

    fn get<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        this.borrow(strand)?.root.index(strand, field, out)
    }

    fn set<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        this.borrow(strand)?.root.assign(strand, field, value)
    }

    async fn input<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        this.borrow(strand)?.root.iter(strand, out).await
    }
}

async fn compile_and_run<'v, 's>(
    strand: &mut Strand<'v, 's>,
    path: &str,
    source: &str,
    out: Slot<'v, '_>,
    strict: bool,
) -> Result<'v, 's, ()> {
    let dynamic = dynamic_prelude(strand).await?;
    let bytecode = Bytecode::new(load::compile(
        strand,
        Path::new(path),
        source,
        Some(&dynamic),
        Mode::Repl,
        strict,
    )?);
    bytecode.run(strand, out).await
}

async fn dynamic_prelude<'v, 's>(strand: &mut Strand<'v, 's>) -> Result<'v, 's, Vec<String>> {
    let dynamic = strand
        .with_slots(
            async move |strand, [mut prelude, mut iter, mut pair, mut key]| {
                let mut dynamic = Vec::new();
                strand.import(DYNAMIC_PRELUDE, &mut prelude).await?;
                prelude.iter(strand, &mut iter).await?;
                while iter.next(strand, &mut pair).await? {
                    pair.index(strand, 0, &mut key)?;
                    dynamic.push(key.as_sym(strand).unwrap().as_str(strand).to_string())
                }
                Ok(dynamic)
            },
        )
        .await?;
    Ok(dynamic)
}

struct DoCandidate {}

impl Candidate for DoCandidate {
    fn display(&self) -> &str {
        ""
    }

    fn replacement(&self) -> &str {
        ""
    }
}

struct DoHint {}

impl Hint for DoHint {
    fn display(&self) -> &str {
        ""
    }

    fn completion(&self) -> Option<&str> {
        None
    }
}

#[derive(Clone, Copy)]
enum OriginClass {
    Normal,
    Param,
    Function,
    Module,
    Prelude,
    PreludeModule,
}

fn classify_origin(origin: Option<&Origin>) -> OriginClass {
    match origin {
        None | Some(Origin::Bind { .. }) => OriginClass::Normal,
        Some(Origin::Param { .. }) | Some(Origin::SelfParam { .. }) => OriginClass::Param,
        Some(Origin::Class { .. }) | Some(Origin::Def { .. }) => OriginClass::Function,
        Some(Origin::ImportModule { .. }) => OriginClass::Module,
        Some(Origin::ImportItem { .. }) => OriginClass::Normal,
        Some(Origin::PreludeItem { .. }) => OriginClass::Prelude,
        Some(Origin::PreludeModule { .. }) => OriginClass::PreludeModule,
    }
}

struct DoHelper {
    dynamic_prelude: Vec<String>,
}

impl Completer for DoHelper {
    type Candidate = DoCandidate;
}

impl Hinter for DoHelper {
    type Hint = DoHint;
}

impl Highlighter for DoHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        let mut tokens = Vec::new();
        let _ = load::analyze(
            Path::new("<repl>"),
            line,
            Some(&self.dynamic_prelude),
            &mut |_: Diag| ControlFlow::<()>::Continue(()),
            &mut |token: Token,
                  span: Span,
                  origin: Option<Origin>,
                  context: Context|
             -> ControlFlow<()> {
                tokens.push((token, span, classify_origin(origin.as_ref()), context));
                ControlFlow::Continue(())
            },
        );

        if tokens.is_empty() {
            return Cow::Borrowed(line);
        }

        // Sort tokens by span start, then longest
        tokens.sort_by_key(|(_, span, _, _)| {
            (
                span.start().byte_offset(),
                0isize.checked_sub_unsigned(span.end().byte_offset()),
            )
        });

        // Remove overlapping tokens, keeping the first one for each unique span start
        let mut unique_tokens = Vec::new();
        for entry in tokens.into_iter() {
            if unique_tokens.last().is_none_or(
                |(_, last_span, _, _): &(Token, Span, OriginClass, Context)| {
                    last_span.start().byte_offset() != entry.1.start().byte_offset()
                },
            ) {
                unique_tokens.push(entry);
            }
        }

        let mut result = String::new();
        let mut last_end = 0;
        for (token, span, origin, context) in unique_tokens {
            // Add unhighlighted part
            let start = span.start().byte_offset();
            let end = span.end().byte_offset();
            if start > last_end {
                result.push_str(&line[last_end..start]);
            }
            // Add highlighted token
            let token_str = &line[start..end];
            let styled = match token {
                Token::Comment => render_styled(
                    Style::new()
                        .fg_color(Some(AnsiColor::White.into()))
                        .dimmed(),
                    token_str,
                ),
                Token::Keyword => render_styled(
                    Style::new().fg_color(Some(AnsiColor::Red.into())),
                    token_str,
                ),
                Token::Literal => render_styled(
                    Style::new().fg_color(Some(AnsiColor::Green.into())),
                    token_str,
                ),
                Token::Operator => render_styled(
                    Style::new().fg_color(Some(AnsiColor::Yellow.into())),
                    token_str,
                ),
                Token::StringDelim => render_styled(
                    Style::new().fg_color(Some(AnsiColor::Cyan.into())),
                    token_str,
                ),
                Token::Number => render_styled(
                    Style::new().fg_color(Some(AnsiColor::Magenta.into())),
                    token_str,
                ),
                Token::Constant => render_styled(
                    Style::new().fg_color(Some(AnsiColor::Magenta.into())),
                    token_str,
                ),
                Token::Delim => render_styled(
                    Style::new().fg_color(Some(AnsiColor::Yellow.into())),
                    token_str,
                ),
                Token::Escape => render_styled(
                    Style::new().fg_color(Some(AnsiColor::Yellow.into())),
                    token_str,
                ),
                Token::ModuleName => render_styled(
                    Style::new().fg_color(Some(AnsiColor::Magenta.into())),
                    token_str,
                ),
                Token::ModuleItem => render_styled(
                    Style::new().fg_color(Some(AnsiColor::Cyan.into())),
                    token_str,
                ),
                Token::Field => match context {
                    Context::Call => render_styled(
                        Style::new().fg_color(Some(AnsiColor::Blue.into())),
                        token_str,
                    ),
                    Context::None => render_styled(
                        Style::new().fg_color(Some(AnsiColor::Cyan.into())),
                        token_str,
                    ),
                },
                Token::Key => render_styled(
                    Style::new().fg_color(Some(AnsiColor::Green.into())),
                    token_str,
                ),
                Token::Sigil => render_styled(
                    Style::new().fg_color(Some(AnsiColor::White.into())),
                    token_str,
                ),
                Token::Variable => match context {
                    Context::Call => render_styled(
                        Style::new().fg_color(Some(AnsiColor::Blue.into())),
                        token_str,
                    ),
                    Context::None => match origin {
                        OriginClass::Function => render_styled(
                            Style::new().fg_color(Some(AnsiColor::Blue.into())),
                            token_str,
                        ),
                        OriginClass::Module => render_styled(
                            Style::new().fg_color(Some(AnsiColor::Magenta.into())),
                            token_str,
                        ),
                        OriginClass::Param => render_styled(
                            Style::new().fg_color(Some(AnsiColor::Magenta.into())),
                            token_str,
                        ),
                        OriginClass::Prelude => render_styled(
                            Style::new().fg_color(Some(AnsiColor::Cyan.into())),
                            token_str,
                        ),
                        OriginClass::PreludeModule => render_styled(
                            Style::new().fg_color(Some(AnsiColor::Magenta.into())),
                            token_str,
                        ),
                        OriginClass::Normal => token_str.to_string(),
                    },
                },
            };
            write!(result, "{}", styled).unwrap();
            last_end = end;
        }
        // Add remaining part
        if last_end < line.len() {
            result.push_str(&line[last_end..]);
        }

        Cow::Owned(result)
    }

    fn highlight_char(&self, line: &str, pos: usize, kind: rustyline::highlight::CmdKind) -> bool {
        let _ = (line, pos, kind);
        true
    }
}

impl Validator for DoHelper {
    fn validate(&self, strand: &mut ValidationContext) -> rustyline::Result<ValidationResult> {
        let input = strand.input();
        if input.starts_with("\n") {
            if strand.input().ends_with("\n") {
                Ok(ValidationResult::Valid(None))
            } else {
                Ok(ValidationResult::Incomplete)
            }
        } else if strand.input() == "" {
            Ok(ValidationResult::Incomplete)
        } else {
            Ok(ValidationResult::Valid(None))
        }
    }
}

impl Helper for DoHelper {}

fn create_editor(
    history_path: Option<&Path>,
) -> rustyline::Result<Editor<DoHelper, DefaultHistory>> {
    let builder = Builder::new();
    let config = builder.build();
    let mut editor = Editor::<DoHelper, DefaultHistory>::with_config(config).unwrap();
    if let Some(path) = history_path
        && path.exists()
        && let Err(e) = editor.load_history(path)
    {
        eprintln!("Warning: could not load history: {e}");
    }
    editor.set_helper(Some(DoHelper {
        dynamic_prelude: Default::default(),
    }));
    Ok(editor)
}

async fn repl<'v, 's>(
    st: &mut Strand<'v, 's>,
    strict: bool,
    editor: &mut Editor<DoHelper, DefaultHistory>,
) -> Result<'v, 's, ()> {
    st.with_slots(
        async |st,
               [
            mut prelude,
            mut iter,
            mut pair,
            mut key,
            mut value,
            mut result,
        ]| loop {
            let line = match editor.readline("❯ ") {
                Ok(line) => line,
                Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => break Ok(()),
                Err(e) => return Err(Error::runtime(st, e)),
            };
            editor.add_history_entry(&line).into_do(st)?;
            match compile_and_run(st, "<repl>", &line, Slot::reborrow(&mut result), strict).await {
                Err(e) if e.catchable() => {
                    diagnostic::print_backtrace(st, e);
                    continue;
                }
                Err(e) => return Err(e),
                Ok(()) => (),
            }
            st.import(DYNAMIC_PRELUDE, &mut prelude).await?;
            result.iter(st, &mut iter).await?;
            while iter.next(st, &mut pair).await? {
                pair.index(st, 0, &mut key)?;
                let key = key.as_sym(st).expect("module key was not a symbol?!");
                pair.index(st, 1, &mut value)?;
                if key.as_str(st) == "_" && !value.is_nil() {
                    eprintln!("↪ {}", value.to_debug(st)?);
                }
                prelude.set(st, key, &mut value)?;
            }
            editor.helper_mut().unwrap().dynamic_prelude = dynamic_prelude(st).await?;
        },
    )
    .await
}

fn save_history(editor: &mut Editor<DoHelper, DefaultHistory>, history_path: &Path) {
    if let Some(parent) = history_path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("Warning: could not create history directory: {e}");
        return;
    }
    if let Err(e) = editor.save_history(history_path) {
        eprintln!("Warning: could not save history: {e}");
    }
}

pub(crate) async fn main<'v, 's>(strand: &mut Strand<'v, 's>, strict: bool) -> Result<'v, 's, ()> {
    let history_path = load::dirs(strand)?.state_dir().map(|p| p.join("history"));
    let mut editor = create_editor(history_path.as_deref()).into_do(strand)?;

    let res = repl(strand, strict, &mut editor).await;
    if let Some(ref path) = history_path {
        save_history(&mut editor, path);
    }
    res
}
