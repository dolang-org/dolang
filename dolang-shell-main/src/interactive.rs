use std::{borrow::Cow, ops::ControlFlow, path::Path, string::String};

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

use crate::{cli::PreludeImport, diagnostic, load};

pub(crate) const DYNAMIC_PRELUDE: &str = "$dynamic$";

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
        w: &mut dyn dolang::runtime::Format<'v>,
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
    prelude: &[PreludeImport],
    strict: bool,
) -> Result<'v, 's, ()> {
    let dynamic = dynamic_prelude(strand).await?;
    let bytecode = Bytecode::new(load::compile(
        strand,
        Path::new(path),
        source,
        Some(&dynamic),
        prelude,
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

struct DoHelper {
    dynamic_prelude: Vec<String>,
    prelude: Vec<PreludeImport>,
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
            &self.prelude,
            &mut |_: Diag| ControlFlow::<()>::Continue(()),
            &mut |token: Token,
                  span: Span,
                  origin: Option<Origin>,
                  context: Context|
             -> ControlFlow<()> {
                tokens.push((token, span, origin, context));
                ControlFlow::Continue(())
            },
        );

        if tokens.is_empty() {
            return Cow::Borrowed(line);
        }
        Cow::Owned(dolang_ext_shell::highlight_source_range(
            line,
            &tokens,
            0..line.len(),
            true,
        ))
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
    prelude: &[PreludeImport],
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
        prelude: prelude.to_vec(),
    }));
    Ok(editor)
}

async fn repl<'v, 's>(
    st: &mut Strand<'v, 's>,
    custom_prelude: &[PreludeImport],
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
            match compile_and_run(
                st,
                "<repl>",
                &line,
                Slot::reborrow(&mut result),
                custom_prelude,
                strict,
            )
            .await
            {
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

pub(crate) async fn main<'v, 's>(
    strand: &mut Strand<'v, 's>,
    custom_prelude: &[PreludeImport],
    strict: bool,
) -> Result<'v, 's, ()> {
    let dirs = load::dirs(strand)?;
    let history_path = dirs
        .state_dir()
        .unwrap_or_else(|| dirs.data_local_dir())
        .join("history");
    let mut editor = create_editor(Some(&history_path), custom_prelude).into_do(strand)?;

    let res = repl(strand, custom_prelude, strict, &mut editor).await;
    save_history(&mut editor, &history_path);
    res
}
