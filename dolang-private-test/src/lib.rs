#![deny(warnings)]

use std::{cell::RefCell, fs::File, io::Read, mem, ops::ControlFlow, path::Path};

use annotate_snippets::{AnnotationKind, Group, Level, Patch, Renderer, Snippet};

use dolang::{
    compile::{self, Compiler, Diag, Mode},
    extension::{CompilerExt, VmExt},
    runtime::{
        Arg, Args, Bytecode, Error, Frame, Slot, Strand, Sym, unpack,
        vm::{Builder, State, Stateful},
    },
};

use bstr::ByteSlice;

/// Read a file into a byte vector.
pub fn read_file(path: &Path) -> Vec<u8> {
    let mut file = File::open(path).unwrap();
    let mut content = Vec::new();
    file.read_to_end(&mut content).unwrap();
    content
}

/// Compile a test file using all registered compiler extensions and the standard prelude.
///
/// Returns `(content, bytecode, diags, directives)` where `bytecode` is `None` if
/// compilation failed. The caller may augment the compile step (extra prelude, module
/// mode, etc.) by using `configure_compiler`/`apply_compiler_extensions` directly
/// instead of this convenience function.
pub fn compile_standard(
    path: &Path,
    module: Option<&str>,
) -> (Vec<u8>, Option<Bytecode>, Vec<Diag>, Vec<Directive>) {
    let content = read_file(path);
    let mut compiler = Compiler::new(path, &content);
    apply_compiler_extensions(&mut compiler);
    let directives = configure_compiler(&mut compiler, &content);
    if let Some(name) = module {
        compiler.mode(Mode::Module { name });
    }
    let mut out = Vec::new();
    let mut diags = Vec::new();
    let res = compiler.compile(&mut out, &mut |diag| {
        diags.push(diag);
        ControlFlow::<(), _>::Continue(())
    });
    let bytecode = res.ok().map(|_| Bytecode::new(out));
    (content, bytecode, diags, directives)
}

/// Run the standard test VM phase: match diagnostics, execute bytecode, check results.
///
/// Handles update mode (`DOLANG_TEST_UPDATE` env var), unexpected diagnostic reporting,
/// runtime error reporting, and panics on failure when not in update mode.
///
/// `bytecode` is `None` when compilation failed (compile errors are expected in that case).
#[allow(clippy::too_many_arguments)]
pub async fn vm_run<'v>(
    strand: &mut Strand<'v, '_>,
    path: &Path,
    content: &[u8],
    bytecode: Option<Bytecode>,
    diags: Vec<Diag>,
    mut directives: Vec<Directive>,
    test_state: &TestState,
    mut retval: Slot<'v, '_>,
) {
    let update_mode = std::env::var("DOLANG_TEST_UPDATE").is_ok();
    let file = path.file_name().unwrap().to_str().unwrap();
    let source = std::str::from_utf8(content).unwrap();

    let mut unexpected_diag = false;
    let mut pending_updates: Vec<(u32, String)> = Vec::new();
    for d in diags {
        if let Some(rendered) = match_diagnostic(&mut directives, &d, file, source) {
            if update_mode {
                pending_updates.push((d.span().end().line_number(), rendered));
            } else {
                let display = render_diag_display(file, source, &d);
                eprintln!("unexpected diagnostic:\n{display}");
                unexpected_diag = true;
            }
        }
    }
    if !pending_updates.is_empty() {
        apply_diagnostic_updates(path, content, pending_updates);
    }

    let compile_failed = bytecode.is_none();
    let mut unexpected_err = false;

    if let Some(bc) = bytecode {
        match bc
            .run(strand, &mut retval)
            .await
            .and_then(|_| retval.to_string(strand))
        {
            Ok(res) => eprintln!("Result: {}", res),
            Err(e) => {
                print_error_backtrace(strand, &e);
                unexpected_err = true;
            }
        }
    }

    let passed = check_results(
        &directives,
        compile_failed,
        unexpected_diag,
        unexpected_err,
        test_state,
    );

    if !passed && !update_mode {
        panic!("test failed")
    }
}

/// Tracks test state including assertion failures and captured output.
pub struct TestState {
    failed: std::cell::Cell<bool>,
    output: RefCell<Vec<u8>>,
}

impl TestState {
    /// Create a new test state.
    pub fn new() -> Self {
        Self {
            failed: std::cell::Cell::new(false),
            output: RefCell::new(Vec::new()),
        }
    }

    /// Mark the test as having failed.
    pub fn mark_failed(&self) {
        self.failed.set(true);
    }

    /// Check if any assertions have failed.
    pub fn failed(&self) -> bool {
        self.failed.get()
    }
}

impl Default for TestState {
    fn default() -> Self {
        Self::new()
    }
}

/// Tag type for TestState registration with the VM.
pub struct TestStateTag;

impl<'v> Stateful<'v> for TestState {
    type Tag = TestStateTag;
}

/// Directives parsed from test file comments.
#[derive(Debug)]
pub enum Directive {
    /// Expected output block
    Output(Vec<u8>),
    /// Expected diagnostic rendered block (verbatim annotate-snippets output with anonymized line numbers)
    DiagBlock(String),
}

const OUTPUT: &[u8] = b"# output:";

/// Returns true if stripped content starts a top-level diagnostic block.
/// error: and warning: are always top-level diagnostics.
/// note: and help: can also trigger blocks (e.g., standalone notes).
fn is_diag_start(stripped: &[u8]) -> bool {
    stripped.starts_with(b"error:")
        || stripped.starts_with(b"warning:")
        || stripped.starts_with(b"note:")
        || stripped.starts_with(b"help:")
}

/// Returns true if stripped content starts a NEW top-level diagnostic,
/// meaning we should close the current block and start a fresh one.
/// Only error: and warning: can interrupt an existing block; note: and help:
/// are sub-elements of the current diagnostic's rendered output.
fn is_top_level_diag(stripped: &[u8]) -> bool {
    stripped.starts_with(b"error:") || stripped.starts_with(b"warning:")
}

fn parse_directives(content: &[u8]) -> Vec<Directive> {
    let lines: Vec<&[u8]> = content.lines().collect();
    let mut res = Vec::new();
    let mut marker: &[u8] = b"";
    let mut output = Vec::new();
    let mut in_diag = false;
    let mut diag_block = String::new();

    for line in &lines {
        // Strip leading whitespace to handle indented diagnostic blocks
        let trimmed = line.trim_ascii_start();

        if !marker.is_empty() {
            // Inside an output block (always at column 0)
            let stripped = line.strip_prefix(b"# ").expect("invalid output syntax");
            if stripped == marker {
                res.push(Directive::Output(mem::take(&mut output)));
                marker = b"";
            } else {
                output.extend_from_slice(stripped);
                output.push(b'\n');
            }
        } else if in_diag {
            // Inside a diagnostic block
            if let Some(rest) = trimmed.strip_prefix(OUTPUT) {
                // Output marker terminates the diagnostic block
                res.push(Directive::DiagBlock(mem::take(&mut diag_block)));
                in_diag = false;
                marker = rest.trim_ascii();
            } else if let Some(stripped) = trimmed.strip_prefix(b"# ") {
                if is_top_level_diag(stripped) {
                    // A new top-level diagnostic starts: close current block, begin new one
                    res.push(Directive::DiagBlock(mem::take(&mut diag_block)));
                    diag_block = str::from_utf8(stripped).unwrap().to_string();
                    diag_block.push('\n');
                } else {
                    diag_block.push_str(str::from_utf8(stripped).unwrap());
                    diag_block.push('\n');
                }
            } else {
                // Non-comment line terminates the block
                res.push(Directive::DiagBlock(mem::take(&mut diag_block)));
                in_diag = false;
            }
        } else if let Some(rest) = line.strip_prefix(OUTPUT) {
            marker = rest.trim_ascii();
        } else if let Some(stripped) = trimmed.strip_prefix(b"# ")
            && is_diag_start(stripped)
        {
            in_diag = true;
            diag_block = str::from_utf8(stripped).unwrap().to_owned();
            diag_block.push('\n');
        }
    }

    // Finalize any open diagnostic block at end of file
    if in_diag && !diag_block.is_empty() {
        res.push(Directive::DiagBlock(diag_block));
    }

    res
}

/// Replace `:DIGITS` in `-->` header lines with `:LL` for stability across file edits.
fn anonymize_origin_lines(rendered: String) -> String {
    let mut result = String::with_capacity(rendered.len());
    for line in rendered.split('\n') {
        // Match "  --> " with any leading whitespace (the --> origin header line)
        let trimmed = line.trim_start_matches(' ');
        if let Some(path_part) = trimmed.strip_prefix("--> ") {
            // Replace colon-prefixed digit sequences with :LL in the path part
            let indent_len = line.len() - trimmed.len();
            result.push_str(&line[..indent_len]);
            result.push_str("--> ");
            let mut i = 0;
            let bytes = path_part.as_bytes();
            while i < bytes.len() {
                if bytes[i] == b':' && bytes.get(i + 1).is_some_and(|b| b.is_ascii_digit()) {
                    result.push(':');
                    i += 1;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                    result.push_str("LL");
                } else {
                    result.push(bytes[i] as char);
                    i += 1;
                }
            }
        } else {
            result.push_str(line);
        }
        result.push('\n');
    }
    result
}

fn build_diag_report<'s>(file: &'s str, source: &'s str, diag: &Diag) -> Vec<Group<'s>> {
    let level = match diag.severity() {
        compile::Severity::Error => Level::ERROR,
        compile::Severity::Warning => Level::WARNING,
        other => Level::INFO.with_name(other.to_string()),
    };
    let mut snippet = Snippet::source(source).path(file).line_start(1);
    let mut have_primary = false;
    for ann in diag.annotations() {
        snippet = snippet.annotation(
            match ann.kind() {
                compile::AnnotationKind::Primary => {
                    have_primary = true;
                    AnnotationKind::Primary
                }
                _ => AnnotationKind::Context,
            }
            .span(ann.span().start().byte_offset()..ann.span().end().byte_offset())
            .label(ann.message().to_string()),
        );
    }
    if !have_primary {
        snippet = snippet.annotation(
            AnnotationKind::Primary
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
                Snippet::source(source).path(file).patch(Patch::new(
                    patch.span().start().byte_offset()..patch.span().end().byte_offset(),
                    patch.sub().to_owned(),
                )),
            ),
        );
    }
    report
}

/// Render a diagnostic to a plain-text string with anonymized line numbers.
///
/// The output matches what would appear in the test file comment blocks.
pub fn render_diag(file: &str, source: &str, diag: &Diag) -> String {
    let report = build_diag_report(file, source, diag);
    let renderer = Renderer::plain().anonymized_line_numbers(true);
    anonymize_origin_lines(renderer.render(&report))
}

/// Render a diagnostic with real line numbers, for human-readable display.
pub fn render_diag_display(file: &str, source: &str, diag: &Diag) -> String {
    let report = build_diag_report(file, source, diag);
    let renderer = Renderer::plain();
    renderer.render(&report)
}

pub fn expect_compile_error(directives: &[Directive]) -> bool {
    directives
        .iter()
        .any(|d| matches!(d, Directive::DiagBlock(s) if s.starts_with("error:")))
}

/// Try to match a diagnostic against pending directives.
///
/// Returns `None` if matched (and removes the matching directive).
/// Returns `Some(rendered)` if unmatched, where `rendered` is the diagnostic's
/// rendered string — useful for update mode.
pub fn match_diagnostic(
    directives: &mut Vec<Directive>,
    diag: &Diag,
    file: &str,
    source: &str,
) -> Option<String> {
    let rendered = render_diag(file, source, diag);
    if let Some(i) = directives
        .iter()
        .position(|d| matches!(d, Directive::DiagBlock(s) if s.trim() == rendered.trim()))
    {
        directives.remove(i);
        return None; // matched
    }
    Some(rendered)
}

/// Insert rendered diagnostic blocks into a test file, after the lines they reference.
///
/// `updates` is a list of `(line_number, rendered_block)` pairs where `line_number`
/// is 1-indexed (from `diag.span().end().line_number()`), so blocks are inserted
/// after the last source line covered by the span.
pub fn apply_diagnostic_updates(path: &Path, source: &[u8], mut updates: Vec<(u32, String)>) {
    if updates.is_empty() {
        return;
    }
    // Sort ascending so we process from top to bottom; same-line items keep order
    updates.sort_by_key(|(line, _)| *line);

    // Pre-split into lines for look-ahead
    let lines: Vec<&[u8]> = {
        let mut v = Vec::new();
        let mut p = 0;
        while p < source.len() {
            let nl = source[p..].iter().position(|&b| b == b'\n');
            let end = nl.map(|i| p + i + 1).unwrap_or(source.len());
            v.push(&source[p..end]);
            p = end;
        }
        v
    };

    let mut result = Vec::with_capacity(source.len() * 2);
    let mut update_idx = 0;

    for (idx, &line) in lines.iter().enumerate() {
        result.extend_from_slice(line);

        // Ensure line ends with newline before inserting blocks
        if !line.ends_with(b"\n") {
            result.push(b'\n');
        }

        let line_num = (idx + 1) as u32;

        // Insert all blocks scheduled for this line
        if update_idx < updates.len() && updates[update_idx].0 == line_num {
            // Determine indentation from the next non-empty line
            let indent: &[u8] = lines[idx + 1..]
                .iter()
                .find(|l| {
                    !l.iter()
                        .all(|&b| b == b' ' || b == b'\t' || b == b'\n' || b == b'\r')
                })
                .map(|l| {
                    let n = l.iter().take_while(|&&b| b == b' ' || b == b'\t').count();
                    &l[..n]
                })
                .unwrap_or(b"");

            while update_idx < updates.len() && updates[update_idx].0 == line_num {
                let rendered = &updates[update_idx].1;
                for bl in rendered.trim_end_matches('\n').lines() {
                    result.extend_from_slice(indent);
                    result.extend_from_slice(b"# ");
                    result.extend_from_slice(bl.as_bytes());
                    result.push(b'\n');
                }
                update_idx += 1;
            }
        }
    }

    std::fs::write(path, &result).unwrap();
}

pub fn report_unmatched(directives: &[Directive]) -> bool {
    let mut failed = false;
    for directive in directives {
        match directive {
            Directive::DiagBlock(s) => {
                failed = true;
                eprintln!("missing diagnostic block:\n{s}");
            }
            Directive::Output(_) => {
                // Output mismatches are reported separately in check_results
            }
        }
    }
    failed
}

pub fn check_results(
    directives: &[Directive],
    compile_failed: bool,
    unexpected_diag: bool,
    unexpected_err: bool,
    test_state: &TestState,
) -> bool {
    // Check output if not a compile error
    let mut incorrect_output = false;
    if !compile_failed {
        let output = &*test_state.output.borrow();
        for directive in directives.iter() {
            if let Directive::Output(expect) = directive {
                if expect.as_slice() != output {
                    eprintln!("unexpected output.  Expected:");
                    eprintln!("{}", str::from_utf8(expect).unwrap());
                    eprintln!("Got:");
                    eprintln!("{}", str::from_utf8(output).unwrap());
                    incorrect_output = true;
                }
                break; // Only check first output directive
            }
        }
    }

    // Report any unmatched directives (excluding output which we already checked)
    let missing_directives = report_unmatched(directives);

    // Check assertion failures
    let assertions_failed = test_state.failed();

    !unexpected_diag
        && !missing_directives
        && !unexpected_err
        && !incorrect_output
        && !assertions_failed
}

pub fn print_backtrace(strand: &Strand) {
    for (i, frame) in strand.backtrace().enumerate() {
        let module = frame.module();
        let receiver = frame.receiver();
        let method = frame.method();
        let name = if let Some(method) = method {
            format!("{}.{}::{}", module, receiver, method)
        } else {
            format!("{}.{}", module, receiver)
        };
        if let Some((path, line)) = frame.source() {
            let path: &str = path.as_ref();
            eprintln!(
                "  #{}: {} at {}:{}",
                i,
                name,
                Path::new(path).file_name().unwrap().display(),
                line + 1
            );
        } else {
            eprintln!("  #{}: {}", i, name)
        }
    }
}

pub fn print_error_backtrace<'v, 's>(strand: &mut Strand<'v, 's>, error: &Error<'v, 's>) {
    eprintln!("error: {}", error.display(strand));

    for (i, entry) in error.backtrace().enumerate() {
        let module = entry.module();
        let receiver = entry.receiver();
        let method = entry.method();
        let name = if let Some(method) = method {
            format!("{}.{}.{}", module, receiver, method)
        } else {
            format!("{}.{}", module, receiver)
        };
        if let Some((path, line)) = entry.source() {
            let path: &str = path.as_ref();
            eprintln!(
                "  #{}: {} at {}:{}",
                i,
                name,
                Path::new(path).file_name().unwrap().display(),
                line + 1
            );
        } else {
            eprintln!("  #{}: {}", i, name)
        }
    }
}

pub fn apply_compiler_extensions(compiler: &mut Compiler) {
    for ext in compiler.extensions() {
        ext.apply(compiler).unwrap();
    }
}

pub fn configure_compiler(compiler: &mut Compiler, content: &[u8]) -> Vec<Directive> {
    // Parse directives from source
    let directives = parse_directives(content);

    // Set up standard regression prelude
    compiler
        .prelude()
        .import_module("regression")
        .import_items("regression")
        .items([
            "log",
            "echo",
            "assert",
            "assert_eq",
            "assert_ne",
            "assert_not",
        ])
        .commit();

    directives
}

pub fn configure_vm<'v>(vm: &mut Builder<'v>) -> State<'v, TestState> {
    let msg: Sym<'v, 'v> = vm.sym("msg");
    let state = vm.register_state(TestState::new());

    vm.module("regression")
        .function(
            "log",
            async move |strand: &mut Strand<'v, '_>, args: Args<'v, '_>, _out: Slot<'v, '_>| {
                let mut space = false;
                for arg in args {
                    if space {
                        eprint!(" ")
                    }
                    space = true;
                    match arg {
                        Arg::Pos(value) => {
                            eprint!("{}", value.to_string(strand).unwrap())
                        }
                        Arg::Key(sym, value) => {
                            let s = value.to_string(strand).unwrap();
                            eprint!("{}: {}", sym.as_str(strand), s)
                        }
                    }
                }
                eprintln!();
                Ok(())
            },
        )
        .function("echo", async move |strand, args, _| {
            use std::io::Write;
            let test_state = strand.vm().state::<TestState>();
            let mut buf = test_state.output.borrow_mut();
            let mut space = false;
            for arg in args {
                if space {
                    write!(buf, " ").unwrap();
                }
                space = true;
                match arg {
                    Arg::Pos(value) => write!(buf, "{}", value.to_arg(strand).unwrap()).unwrap(),
                    Arg::Key(sym, value) => {
                        let str = value.to_arg(strand).unwrap();
                        write!(buf, "{}: {}", sym.as_str(strand), str).unwrap()
                    }
                }
            }
            writeln!(buf).unwrap();
            Ok(())
        })
        .function("assert", {
            async move |strand, args, _| {
                let ([cond], [msg]) = unpack!(strand, args, 1, 0, msg = None)?;
                if !cond.to_bool(strand) {
                    if let Some(msg) = msg {
                        eprintln!("assertion failed: {}", msg.to_string(strand)?);
                    } else {
                        eprintln!("assertion failed");
                    }
                    print_backtrace(strand);
                    strand.vm().state::<TestState>().mark_failed();
                }
                Ok(())
            }
        })
        .function("assert_not", {
            async move |strand, args, _| {
                let ([cond], [msg]) = unpack!(strand, args, 1, 0, msg = None)?;
                if cond.to_bool(strand) {
                    if let Some(msg) = msg {
                        eprintln!("assertion failed: {}", msg.to_string(strand)?);
                    } else {
                        eprintln!("assertion failed");
                    }
                    print_backtrace(strand);
                    strand.vm().state::<TestState>().mark_failed();
                }
                Ok(())
            }
        })
        .function("assert_eq", {
            async move |strand, args, _| {
                let ([left, right], [msg]) = unpack!(strand, args, 2, 0, msg = None)?;
                if !left.eq(strand, &right) {
                    if let Some(msg) = msg {
                        eprintln!(
                            "assertion failed: {} ({} != {})",
                            msg.to_string(strand)?,
                            left.to_debug(strand)?,
                            right.to_debug(strand)?
                        );
                    } else {
                        eprintln!(
                            "assertion failed: {} != {}",
                            left.to_debug(strand)?,
                            right.to_debug(strand)?
                        );
                    }
                    print_backtrace(strand);
                    strand.vm().state::<TestState>().mark_failed();
                }
                Ok(())
            }
        })
        .function("assert_ne", {
            async move |strand, args, _| {
                let ([left, right], [msg]) = unpack!(strand, args, 2, 0, msg = None)?;
                if !left.ne(strand, &right) {
                    if let Some(msg) = msg {
                        eprintln!(
                            "assertion failed: {} ({} == {})",
                            msg.to_string(strand)?,
                            left.to_debug(strand)?,
                            right.to_debug(strand)?
                        );
                    } else {
                        eprintln!(
                            "assertion failed: {} == {}",
                            left.to_debug(strand)?,
                            right.to_debug(strand)?
                        );
                    }
                    print_backtrace(strand);
                    strand.vm().state::<TestState>().mark_failed();
                }
                Ok(())
            }
        })
        .commit();

    state
}

pub fn apply_vm_extensions(vm: &mut Builder) {
    for ext in vm.extensions() {
        ext.apply(vm).unwrap();
    }
}
