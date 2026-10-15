use std::{
    borrow::Cow,
    path::{Path, PathBuf},
};

use dolang::{
    compile::{Config, Context, Mode, NodeId, Span, Token},
    runtime::{
        Error, Object, Output, Result, Strand, Type,
        error::ResultExt,
        object::{Mut, TypeBuilder},
        strand::InterruptMask,
        unpack,
        value::Nil,
    },
};
use dolang_ext_compile::apply_prelude_value;
use dolang_ext_shell::classify_node;
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

use crate::load;

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
    config: Config<'static>,
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
        let unit = self.config.unit(Path::new("<repl>"), line);
        unit.tokens(
            &mut |token: Token, span: Span, node: Option<NodeId>, context: Context| {
                let kind = node.and_then(|id| unit.node(id)).map(|node| node.kind());
                tokens.push((token, span, classify_node(kind.as_ref()), context));
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
    config: Config<'static>,
) -> rustyline::Result<Editor<DoHelper, DefaultHistory>> {
    let builder = Builder::new();
    let editor_config = builder.build();
    let mut editor = Editor::<DoHelper, DefaultHistory>::with_config(editor_config)?;
    if let Some(path) = history_path
        && path.exists()
        && let Err(e) = editor.load_history(path)
    {
        eprintln!("Warning: could not load history: {e}");
    }
    editor.set_helper(Some(DoHelper { config }));
    Ok(editor)
}

/// Only owned native data crosses the blocking-task boundary.
pub(crate) struct LineEditor {
    editor: Option<Editor<DoHelper, DefaultHistory>>,
    history_path: Option<PathBuf>,
}

impl<'v> Object<'v> for LineEditor {
    const NAME: &'v str = "Editor";
    const MODULE: &'v str = "_shell";
    // Slot 0: prelude used to rebuild the highlighting configuration each turn.
    const SLOTS: usize = 1;
    type Annex = ();
    type Type = ();
    type TypeAnnex = dolang::runtime::Sym<'v, 'v>;

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: dolang::runtime::Args<'v, 'a>,
        mut out: dolang::runtime::Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let prelude_sym = *this.annex(strand.vm());
        let ([path], [prelude]) = unpack!(strand, args, 1, 0, prelude_sym = None)?;
        let history_path = if path.is_nil() {
            None
        } else {
            Some(dolang_ext_shell::as_path(strand, &path).ok_or_else(|| {
                Error::type_error(strand, "history path must be Str, fs.Path, or nil")
            })?)
        };
        let mut config = load::compile_setup(&[], Mode::Repl);
        config.recover(true).document(true);
        if let Some(prelude) = prelude.as_ref().filter(|value| !value.is_nil()) {
            apply_prelude_value(strand, &mut config, prelude)?;
        }
        let editor = create_editor(history_path.as_deref(), config).into_do(strand)?;
        this.create(
            strand,
            Self {
                editor: Some(editor),
                history_path,
            },
            &mut out,
        );
        if let Some(prelude) = prelude {
            this.cast(&out)
                .unwrap()
                .enter_sync(strand, |strand, editor| {
                    Output::set(
                        strand,
                        Mut::slot_mut::<0>(&mut editor.borrow_mut_unwrap()),
                        prelude,
                    );
                });
        }
        Ok(())
    }

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let eof = builder.sym("EOF");
        let interrupted = builder.sym("INTERRUPTED");
        builder
            .method("readline", async move |this, strand, args, out| {
                let ([prompt, names], []) = unpack!(strand, args, 2, 0)?;
                let prompt = prompt
                    .as_str(strand)
                    .ok_or_else(|| Error::type_error(strand, "prompt must be Str"))?
                    .to_string();
                let names = names
                    .as_array(strand)
                    .ok_or_else(|| Error::type_error(strand, "names must be Array"))?;
                let names = strand.with_slots_sync(|strand, [mut name]| {
                    let mut result = Vec::new();
                    for i in 0..names.len(strand)? {
                        names.get(strand, i, &mut name)?;
                        let text = if let Some(sym) = name.as_sym(strand) {
                            sym.as_str(strand).to_owned()
                        } else if let Some(text) = name.as_str(strand) {
                            text.to_string()
                        } else {
                            return Err(Error::type_error(
                                strand,
                                "binding name must be Str or Sym",
                            ));
                        };
                        result.push(text);
                    }
                    Ok(result)
                })?;
                let mut borrow = this.borrow_mut(strand)?;
                let mut config = load::compile_setup(&[], Mode::Repl);
                config.recover(true).document(true);
                let prelude = Mut::slot::<0>(&borrow);
                if !prelude.is_nil() {
                    apply_prelude_value(strand, &mut config, prelude)?;
                }
                config
                    .prelude()
                    .import_items("$dynamic$")
                    .items(&names)
                    .commit();
                let mut editor = borrow
                    .editor
                    .take()
                    .ok_or_else(|| Error::concurrency_msg(strand, "editor is already reading"))?;
                editor.helper_mut().unwrap().config = config;
                drop(borrow);

                // Rustyline cannot be aborted from another thread. Keep the editor
                // owned by this call until it returns, even after cancellation.
                let result = strand
                    .with_interrupt_mask(InterruptMask::all(), async |strand| {
                        tokio::task::spawn_blocking(move || {
                            let result = editor.readline(&prompt).and_then(|line| {
                                editor.add_history_entry(&line)?;
                                Ok(line)
                            });
                            (editor, result)
                        })
                        .await
                        .into_do(strand)
                    })
                    .await?;
                let (editor, result) = result;
                this.borrow_mut(strand)?.editor = Some(editor);
                match result {
                    Ok(line) => Output::set(strand, out, line.as_str()),
                    Err(ReadlineError::Eof) => Output::set(strand, out, eof),
                    Err(ReadlineError::Interrupted) => Output::set(strand, out, interrupted),
                    Err(error) => return Err(Error::runtime(strand, error)),
                }
                Ok(())
            })
            .method("save_history", async |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let mut borrow = this.borrow_mut(strand)?;
                let path = borrow.history_path.clone();
                let editor = borrow
                    .editor
                    .as_mut()
                    .ok_or_else(|| Error::concurrency_msg(strand, "editor is already reading"))?;
                if let Some(path) = path {
                    save_history(editor, &path);
                }
                Output::set(strand, out, Nil);
                Ok(())
            })
    }
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
