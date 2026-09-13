use dolang::runtime::{
    Instance, Object, Output, Result, Slot, State, Strand, Value,
    object::TypeBuilder,
    unpack,
    value::{Root, TypeObject},
    vm::Builder,
};
use tokio::io::AsyncWriteExt;

use crate::{
    error::ErrorExt as _,
    geometry::{HostGeometry, HostGeometryAnnex},
    global::Global,
    io_mode::encode_value,
};

/// The host console's line ending: LF on every platform, since this is a
/// terminal, not a file, and `echo` has always written LF on Windows too.
const LINE_ENDING: &str = "\n";

/// The host console, reachable as `term.console`.
///
/// Writes to the terminal writer, which extension terminal takeover
/// (`with_terminal`) swaps out, so output during a progress display goes through
/// the display's writer rather than fighting it.
pub(crate) struct HostConsole;

impl<'v> Object<'v> for HostConsole {
    const NAME: &'v str = "Console";
    const MODULE: &'v str = "shell";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .supertype(TypeObject::Sink)
            .method("write", async move |_this, strand, args, out| {
                let bytes = dolang_ext_term::write_data(strand, args)?;
                write_host(strand, &bytes).await?;
                Output::set(strand, out, bytes.len());
                Ok(())
            })
            .method("flush", async move |_this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let global = strand.state::<Global<'v>>();
                global
                    .terminal
                    .writer
                    .lock()
                    .await
                    .flush()
                    .await
                    .map_err(|error| error.into_sys(strand))
            })
            .get("line_ending", |_this, strand, out| {
                Output::set(strand, out, LINE_ENDING);
                Ok(())
            })
            .get("can_style", |_this, strand, out| {
                let global = strand.state::<Global<'v>>();
                Output::set(strand, out, global.terminal.ansi);
                Ok(())
            })
            .get("is_tty", |_this, strand, out| {
                // Whether the console is usable as a terminal: while an
                // extension has taken the terminal over, it owns the cursor.
                let global = strand.state::<Global<'v>>();
                let is_tty =
                    global.terminal.stderr_is_terminal && !global.terminal.redirected.get();
                Output::set(strand, out, is_tty);
                Ok(())
            })
            .method("geometry", async move |_this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let global = strand.state::<Global<'v>>();
                let ov = &global.terminal.console_override;
                // The real terminal is queried even under takeover (a
                // progress display owns the cursor, but the width is still
                // the width), but only for whichever of rows/cols
                // `DOLANG_CONSOLE` didn't already pin down. `size_checked`
                // itself answers `None` on a non-terminal fd or one that
                // declines to report its size, so there's nothing further to
                // gate on here.
                let real = if ov.rows.is_none() || ov.cols.is_none() {
                    ::console::Term::stderr().size_checked()
                } else {
                    None
                };
                let rows = ov.rows.map(u32::from).or(real.map(|(r, _)| r.into()));
                let cols = ov.cols.map(u32::from).or(real.map(|(_, c)| c.into()));
                // Always a `Geometry`, never nil: the host console is the
                // terminal-shaped one, so "I don't know either dimension" is
                // itself expressed as a `Geometry` with both fields nil,
                // rather than as a second, redundant way to say "unknown"
                // alongside the per-field nils. `is_tty` is the determinative
                // terminal test — a guessed 24x80 is never invented here,
                // each dimension is independently advisory.
                global.types.host_geometry.create_with_annex(
                    strand,
                    HostGeometry,
                    HostGeometryAnnex { rows, cols },
                    out,
                );
                Ok(())
            })
    }

    /// There is exactly one host console per VM, so having the type is having
    /// the object.
    fn eq<'a, 's>(
        _this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        let global = strand.state::<Global<'v>>();
        Ok(global.types.host_console.cast(other).is_some())
    }

    async fn sink<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
        Ok(())
    }

    async fn put<'a, 's>(
        _this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let bytes = encode_value(strand, &value)?;
        write_host(strand, &bytes).await
    }
}

/// Writes to the terminal writer.
///
/// A single `write_all` under a single lock: `echo` hands a line and its
/// terminator to one `write`, and splitting them across two critical sections
/// would let a concurrent strand interleave its own line between them.
async fn write_host<'v, 's>(strand: &mut Strand<'v, 's>, bytes: &[u8]) -> Result<'v, 's, ()> {
    let global = strand.state::<Global<'v>>();
    let mut writer = global.terminal.writer.lock().await;
    writer
        .write_all(bytes)
        .await
        .map_err(|error| error.into_sys(strand))
}

/// Installs the host console as `term`'s.
pub(crate) fn install<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    let mut console = Root::new(builder);
    global
        .types
        .host_console
        .create(builder, HostConsole, &mut console);
    dolang_ext_term::install_console(builder, &*console, global.terminal.ansi, LINE_ENDING);
}
