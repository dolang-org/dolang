use std::mem;

use dolang::runtime::object::fmt;
use dolang::runtime::value::fmt::Format;
use dolang::runtime::{
    Error, Input, Instance, Object, Output, Result, Slot, Strand, Sym, Value, method,
    object::{Mut, TypeBuilder},
    unpack,
    value::{TypeObject, View},
};
use tokio::io::AsyncWriteExt;

use crate::{
    error::ErrorExt as _,
    geometry::{HostGeometry, HostGeometryAnnex},
    global::Global,
    io_mode::{IoMode, encode_value, write_raw},
};

/// The console interface: where human-readable output goes.
///
/// A console is a *byte stream*, not merely a sink. `term.echo` always
/// terminates a line and `term.print` never does, so the terminator has to be
/// materialized into the byte stream rather than left to value framing — and it
/// is the console that knows which terminator to use, since that follows the
/// device rather than the caller.
///
/// So a console *owns* the policy but does not *apply* it: `line_ending`
/// reports the terminator and `write` writes exactly the bytes it is given.
/// A caller that wants a line assembles it and issues one `write`, which is
/// also what keeps concurrent writers from interleaving. `put` is layered on
/// top and writes a value's own bytes verbatim, so a console is usable as an
/// ordinary sink too.
///
/// Native extension types cannot be abstract, so the methods here throw rather
/// than being absent. The concrete implementations are `shell.Console` (the
/// host console), `term.SinkConsole` (an adapter over any sink), and whatever
/// Do code subclasses this with.
///
/// Distinct from `shell.stderr`, which is the process's error stream and
/// ignores both terminal takeover and capture.
pub(crate) struct Console;

impl<'v> Object<'v> for Console {
    const NAME: &'v str = "Console";
    const MODULE: &'v str = "term";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    /// Constructible so that Do classes can subclass it: a native supertype has
    /// to be initializable for `Console.(init) $self` to fill its slot.
    async fn new<'a, 's>(
        this: dolang::runtime::Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: dolang::runtime::Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([], []) = unpack!(strand, args, 0, 0)?;
        this.create(strand, Console, out);
        Ok(())
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .supertype(TypeObject::Sink)
            .method("write", async move |_this, strand, args, _out| {
                let ([_data], []) = unpack!(strand, args, 1, 0)?;
                Err(Error::not_supported(strand))
            })
            .method("flush", async move |_this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                Err(Error::not_supported(strand))
            })
            // Unlike the write methods, the capability members have a safe
            // default, so a Do subclass that supplies only the two above still
            // answers them — by delegating to these.
            .get("line_ending", |_this, strand, out| {
                // LF unless a subclass says otherwise: a console is a stream
                // for humans to read, and every terminal takes LF.
                Output::set(strand, out, "\n");
                Ok(())
            })
            .get("can_style", |_this, strand, out| {
                Output::set(strand, out, false);
                Ok(())
            })
            .get("is_tty", |_this, strand, out| {
                Output::set(strand, out, false);
                Ok(())
            })
            .method("geometry", async move |_this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                // Nil: a bare console is just a stream, which is a real answer
                // rather than a missing one.
                Ok(())
            })
    }

    async fn sink<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
        Ok(())
    }

    /// `put` on top of `write`, so a subclass only has to supply the byte
    /// methods to be a working sink. Nothing is added: a value contributes its
    /// own bytes and a terminator, if wanted, comes from `crimp`.
    async fn put<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = strand.state::<Global<'v>>();
        let bytes = encode_value(strand, &value)?;
        strand
            .with_slots(async move |strand, [mut rcvr, mut out]| {
                Output::set(strand, &mut rcvr, this);
                method!(strand, &rcvr, global.syms.write, &mut out, &bytes[..]).await
            })
            .await
    }
}

/// The host console, reachable as `term.console`.
///
/// Writes to the terminal writer, which extension terminal takeover
/// (`with_terminal`) swaps out, so output during a progress display goes through
/// the display's writer rather than fighting it.
///
/// Its line ending is LF on every platform: this is a terminal, not a file, and
/// `echo` has always written LF on Windows too.
pub(crate) struct HostConsole;

impl Default for HostConsole {
    fn default() -> Self {
        Self
    }
}

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
                let ([data], []) = unpack!(strand, args, 1, 0)?;
                let global = strand.state::<Global<'v>>();
                let mut writer = global.terminal.writer.lock().await;
                write_raw(&mut *writer, data, strand, out).await
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
                Output::set(strand, out, HOST_LINE_ENDING);
                Ok(())
            })
            .get("can_style", |_this, strand, out| {
                let global = strand.state::<Global<'v>>();
                Output::set(strand, out, global.terminal.ansi);
                Ok(())
            })
            .get("is_tty", |_this, strand, out| {
                let global = strand.state::<Global<'v>>();
                Output::set(strand, out, global.terminal.stderr_is_terminal);
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
        write_host(strand, &bytes, false).await
    }
}

/// The strand's default output, exported as `term.default`.
///
/// A forwarder, not a destination: every operation resolves *at call time* to
/// whatever `term.output()` currently is — the host console, or an installed
/// capture. Bound as the main strand's implicit output when stdout is a
/// terminal (see [`crate::default_output`]), so that unnamed program output
/// keeps following capture and progress takeover for the life of the process,
/// the same way naming `term.output()` itself would, without every caller
/// having to re-resolve it.
///
/// Contrast `term.console`, which pins to the host and is never intercepted,
/// and `shell.stdout`, which is the literal stream and bypasses this
/// machinery entirely.
pub(crate) struct DefaultOutput;

impl Default for DefaultOutput {
    fn default() -> Self {
        Self
    }
}

impl<'v> Object<'v> for DefaultOutput {
    const NAME: &'v str = "Default";
    const MODULE: &'v str = "term";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .supertype(TypeObject::Sink)
            .method("write", async move |_this, strand, args, out| {
                let ([data], []) = unpack!(strand, args, 1, 0)?;
                let bytes = data_bytes(strand, &data)?;
                write(strand, &bytes).await?;
                Output::set(strand, out, bytes.len() as i64);
                Ok(())
            })
            .method("flush", async move |_this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                flush(strand).await
            })
            .get("line_ending", |_this, strand, out| {
                // Resolved at call time like everything else here, so it
                // follows an installed capture rather than the host.
                let ending = ambient_line_ending(strand)?;
                Output::set(strand, out, ending.as_slice());
                Ok(())
            })
            .get("can_style", |_this, strand, out| {
                Output::set(strand, out, ansi(strand));
                Ok(())
            })
            .get("is_tty", |_this, strand, out| {
                Output::set(strand, out, is_tty(strand));
                Ok(())
            })
            .method("geometry", async move |_this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                geometry(strand, out).await
            })
    }

    /// There is exactly one `term.default` per VM, so having the type is
    /// having the object.
    fn eq<'a, 's>(
        _this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        let global = strand.state::<Global<'v>>();
        Ok(global.types.default.cast(other).is_some())
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
        write(strand, &bytes).await
    }
}

/// The line ending of a console that has no opinion of its own.
///
/// LF on every platform: a console is a terminal-shaped stream, not a file, and
/// `echo` has always written LF on Windows too. Files get their ending from
/// their VFS target instead — see `shell.line_ending()`.
const HOST_LINE_ENDING: &str = "\n";

/// The ambient console's `line_ending`, as bytes to append.
///
/// The host's outside a capture; otherwise whatever the installed console
/// reports, so a Do-defined console's terminator is honored. Read live rather
/// than snapshotted at install: unlike `can_style`/`is_tty` it is not part of
/// the contract that it stays fixed, and it is only consulted on the `echo`
/// path, which is already dispatching a method.
pub(crate) fn ambient_line_ending<'v, 's>(strand: &mut Strand<'v, 's>) -> Result<'v, 's, Vec<u8>> {
    if !captured(strand) {
        return Ok(HOST_LINE_ENDING.as_bytes().to_vec());
    }
    let global = strand.state::<Global<'v>>();
    strand.with_slots_sync(|strand, [mut rcvr, mut value]| {
        let root = global.capture.slot(strand);
        Output::set(strand, &mut rcvr, &root);
        rcvr.get(strand, global.syms.line_ending, &mut value)?;
        data_bytes(strand, &value)
    })
}

/// The bytes of a `Str` or `Bin`, as any console's `write` accepts.
fn data_bytes<'v, 's>(strand: &mut Strand<'v, 's>, data: &Slot<'v, '_>) -> Result<'v, 's, Vec<u8>> {
    match data.view(strand) {
        View::Str(value) => Ok(value.pin().as_bytes().to_vec()),
        View::Bin(value) => Ok(value.pin().to_vec()),
        _ => Err(Error::type_error(strand, "expected `Str` or `Bin`")),
    }
}

/// A console over an ordinary sink, supplying the rest of the interface.
///
/// This is a *bytestream-to-value* boundary, so it has to decide where to cut
/// the stream — mirroring [`io_mode::read_value`](crate::io_mode::read_value)
/// on the pull side. That framing is fixed when the adapter is built rather
/// than read from the surrounding context, so a capture drains the way it
/// buffered; `term.capture` wraps a plain sink in one of these and forwards its
/// own `mode:` to here.
///
/// Framing is all it does. Bytes are passed through exactly as written — the
/// terminator that `echo` put into the stream stays in the value, and a `\r\n`
/// is neither normalized nor produced. Removing it is `chomp`'s job, which the
/// receiving sink can ask for the same way it would for any other stream.
pub(crate) struct SinkConsole {
    /// Bytes written but not yet emitted as a value.
    ///
    /// Only ever non-empty in `:LINE:` mode, holding a partial final line.
    buf: Vec<u8>,
    /// How the byte stream is quantized into values, fixed at construction.
    mode: IoMode,
    /// Off unless the caller asked for styling, since the point of capturing
    /// into a sink is usually to assert on plain text.
    can_style: bool,
}

impl SinkConsole {
    /// Splits off whatever is now emittable, leaving any partial line behind.
    fn drain(&mut self, final_: bool) -> Vec<Vec<u8>> {
        match self.mode {
            IoMode::Chunk => {
                if self.buf.is_empty() {
                    Vec::new()
                } else {
                    vec![mem::take(&mut self.buf)]
                }
            }
            IoMode::Line => {
                let mut out = Vec::new();
                while let Some(at) = self.buf.iter().position(|&b| b == b'\n') {
                    out.push(self.buf.drain(..=at).collect());
                }
                if final_ && !self.buf.is_empty() {
                    out.push(mem::take(&mut self.buf));
                }
                out
            }
        }
    }
}

impl<'v> Object<'v> for SinkConsole {
    const NAME: &'v str = "SinkConsole";
    const MODULE: &'v str = "term";
    const SLOTS: usize = 1;
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        _this: dolang::runtime::Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: dolang::runtime::Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = strand.state::<Global<'v>>();
        let can_style_sym = global.syms.can_style;
        let mode_sym = global.syms.mode;
        let ([target], [can_style, mode]) =
            unpack!(strand, args, 1, 0, can_style_sym = None, mode_sym = None)?;
        let can_style = can_style.is_some_and(|value| value.to_bool(strand));
        let mode = parse_mode(strand, mode.as_deref())?;
        create_sink_console(strand, &target, can_style, mode, out).await
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .supertype(TypeObject::Sink)
            .get("can_style", |this, strand, out| {
                let can_style = this.borrow(strand)?.can_style;
                Output::set(strand, out, can_style);
                Ok(())
            })
            .get("is_tty", |_this, strand, out| {
                // An adapter over an arbitrary sink is never a terminal.
                Output::set(strand, out, false);
                Ok(())
            })
            .method("geometry", async move |_this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                Ok(())
            })
            .get("line_ending", |_this, strand, out| {
                Output::set(strand, out, HOST_LINE_ENDING);
                Ok(())
            })
            .method_with_slots(
                "write",
                async move |this, strand, args, out, [sink, item]| {
                    let ([data], []) = unpack!(strand, args, 1, 0)?;
                    let bytes = data_bytes(strand, &data)?;
                    let count = bytes.len();
                    feed(this, strand, &bytes, false, sink, item).await?;
                    Output::set(strand, out, count);
                    Ok(())
                },
            )
            .method_with_slots(
                "flush",
                async move |this, strand, args, _out, [sink, item]| {
                    let ([], []) = unpack!(strand, args, 0, 0)?;
                    feed(this, strand, &[], true, sink, item).await
                },
            )
    }

    fn debug<'a, 's>(
        _this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "<sink console>")
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
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let bytes = encode_value(strand, &value)?;
        strand
            .with_slots(async move |strand, [sink, item]| {
                feed(this, strand, &bytes, false, sink, item).await
            })
            .await
    }
}

/// Wraps any sink in a [`SinkConsole`], rooting it in the adapter's slot.
pub(crate) async fn create_sink_console<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    target: &Value<'v>,
    can_style: bool,
    mode: IoMode,
    mut out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    let global = strand.state::<Global<'v>>();
    global.types.sink_console.create(
        strand,
        SinkConsole {
            buf: Vec::new(),
            mode,
            can_style,
        },
        &mut out,
    );
    strand
        .with_slots(async move |strand, [mut downstream]| {
            target.sink(strand, &mut downstream).await?;
            global
                .types
                .sink_console
                .cast(&out)
                .unwrap()
                .enter_sync(strand, |strand, inst| {
                    let mut borrow = inst.borrow_mut(strand)?;
                    Output::set(strand, Mut::slot_mut::<0>(&mut borrow), &downstream);
                    Ok(())
                })
        })
        .await
}

/// Appends bytes and forwards whatever that completes to the downstream sink.
///
/// `final_` also emits a trailing partial line, which is what makes an
/// unterminated `print` visible once the capture scope ends.
async fn feed<'v, 'a, 's>(
    this: Instance<'v, 'a, SinkConsole>,
    strand: &mut Strand<'v, 's>,
    bytes: &[u8],
    final_: bool,
    mut sink: Slot<'v, 'a>,
    mut item: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    let (mode, pending) = {
        let mut me = this.borrow_mut(strand)?;
        me.buf.extend_from_slice(bytes);
        let mode = me.mode;
        let pending = me.drain(final_);
        let downstream = Mut::slot_mut::<0>(&mut me);
        Output::set(strand, &mut sink, &downstream);
        (mode, pending)
    };

    for unit in pending {
        match mode {
            IoMode::Line => {
                // The terminator stays: it is what the writer put into the
                // stream, so the value reproduces the bytes exactly.
                let text = String::from_utf8(unit)
                    .map_err(|_| Error::runtime(strand, "console capture: invalid UTF-8"))?;
                Output::set(strand, &mut item, text.as_str());
            }
            IoMode::Chunk => Output::set(strand, &mut item, unit.as_slice()),
        }
        sink.put(strand, &mut item).await?;
    }
    Ok(())
}

/// Decodes a `:LINE:`/`:CHUNK:` framing argument, defaulting to line framing.
pub(crate) fn parse_mode<'v, 's>(
    strand: &mut Strand<'v, 's>,
    mode: Option<&Value<'v>>,
) -> Result<'v, 's, IoMode> {
    let global = strand.state::<Global<'v>>();
    match mode {
        None => Ok(IoMode::Line),
        Some(value) => match value.as_sym(strand) {
            Some(sym) if sym == global.syms.line => Ok(IoMode::Line),
            Some(sym) if sym == global.syms.chunk => Ok(IoMode::Chunk),
            _ => Err(Error::value(strand, "mode must be :LINE: or :CHUNK:")),
        },
    }
}

/// The console behind `term.sub`: accumulates the byte stream verbatim.
///
/// No framing at all — `term.sub` reports exactly what was written, so
/// `print a`, `print b`, `echo c` is `"abc\n"`.
pub(crate) struct SubConsole {
    text: String,
    can_style: bool,
}

impl SubConsole {
    pub(crate) fn new(can_style: bool) -> Self {
        Self {
            text: String::new(),
            can_style,
        }
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    fn append<'v, 's>(&mut self, strand: &mut Strand<'v, 's>, bytes: &[u8]) -> Result<'v, 's, ()> {
        let text = std::str::from_utf8(bytes)
            .map_err(|_| Error::runtime(strand, "term.sub: captured invalid UTF-8"))?;
        self.text.push_str(text);
        Ok(())
    }
}

impl<'v> Object<'v> for SubConsole {
    const NAME: &'v str = "SubConsole";
    const MODULE: &'v str = "term";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .supertype(TypeObject::Sink)
            .method("write", async move |this, strand, args, out| {
                let ([data], []) = unpack!(strand, args, 1, 0)?;
                let bytes = data_bytes(strand, &data)?;
                this.borrow_mut(strand)?.append(strand, &bytes)?;
                Output::set(strand, out, bytes.len());
                Ok(())
            })
            .method("flush", async move |_this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                Ok(())
            })
            .get("line_ending", |_this, strand, out| {
                Output::set(strand, out, HOST_LINE_ENDING);
                Ok(())
            })
            .get("can_style", |this, strand, out| {
                let can_style = this.borrow(strand)?.can_style;
                Output::set(strand, out, can_style);
                Ok(())
            })
            .get("is_tty", |_this, strand, out| {
                // A capture buffer is never a terminal.
                Output::set(strand, out, false);
                Ok(())
            })
            .method("geometry", async move |_this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                Ok(())
            })
    }

    fn debug<'a, 's>(
        _this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "<sub console>")
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
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let bytes = encode_value(strand, &value)?;
        this.borrow_mut(strand)?.append(strand, &bytes)
    }
}

/// Whether a capture is installed and should be dispatched to.
///
/// False while already dispatching into a console, so a console whose own
/// `write` calls `echo` falls through to the host instead of recursing.
fn captured<'v>(strand: &Strand<'v, '_>) -> bool {
    let global = strand.state::<Global<'v>>();
    !global.capture.slot(strand).is_nil() && !global.local.get(strand).capturing()
}

/// Writes bytes to the ambient console verbatim.
///
/// Used by `term.print`, diagnostics, and the child-stdio byte pump, which
/// copies a child's output straight through without any value framing.
pub(crate) async fn write<'v, 's>(strand: &mut Strand<'v, 's>, bytes: &[u8]) -> Result<'v, 's, ()> {
    if !captured(strand) {
        return write_host(strand, bytes, false).await;
    }
    let global = strand.state::<Global<'v>>();
    dispatch(strand, global.syms.write, bytes).await
}

/// Writes bytes to the ambient console followed by its own line ending.
///
/// Only the console knows its terminator, which is why the caller cannot just
/// write `b"...\n"` itself. The terminated buffer goes out in a *single*
/// `write`: two writes would let a concurrent strand slip its own line between
/// the payload and the terminator, which shows up as overlapping output.
pub(crate) async fn writeln<'v, 's>(
    strand: &mut Strand<'v, 's>,
    bytes: &[u8],
) -> Result<'v, 's, ()> {
    if !captured(strand) {
        return write_host(strand, bytes, true).await;
    }
    let mut line = Vec::with_capacity(bytes.len() + 2);
    line.extend_from_slice(bytes);
    line.extend_from_slice(&ambient_line_ending(strand)?);
    let global = strand.state::<Global<'v>>();
    dispatch(strand, global.syms.write, &line[..]).await
}

/// Flushes the ambient console.
pub(crate) async fn flush<'v, 's>(strand: &mut Strand<'v, 's>) -> Result<'v, 's, ()> {
    if !captured(strand) {
        let global = strand.state::<Global<'v>>();
        return global
            .terminal
            .writer
            .lock()
            .await
            .flush()
            .await
            .map_err(|error| error.into_sys(strand));
    }
    let global = strand.state::<Global<'v>>();
    dispatch_no_args(strand, global.syms.flush).await
}

/// Whether ANSI styling should be emitted to the ambient console.
///
/// Under a capture this is the `can_style` the installed console reported when
/// it was installed — off by default, since a capture is not a terminal and a
/// test asserting on `echo`ed text would otherwise pass piped and fail on a
/// developer's terminal.
pub(crate) fn ansi<'v>(strand: &Strand<'v, '_>) -> bool {
    let global = strand.state::<Global<'v>>();
    if captured(strand) {
        return global.local.get(strand).capture_can_style();
    }
    global.terminal.ansi
}

/// Reads a console's `can_style`, for snapshotting when it is installed.
pub(crate) fn can_style<'v, 's>(
    strand: &mut Strand<'v, 's>,
    console: &Value<'v>,
) -> Result<'v, 's, bool> {
    let sym = strand.state::<Global<'v>>().syms.can_style;
    strand.with_slots_sync(|strand, [mut value]| {
        console.get(strand, sym, &mut value)?;
        value
            .as_bool(strand)
            .ok_or_else(|| Error::type_error(strand, "can_style: expected `Bool`"))
    })
}

/// Whether the ambient console is a real terminal.
///
/// Under a capture this is the `is_tty` the installed console reported
/// when it was installed, mirroring [`ansi`]/`can_style`. Distinct from
/// [`crate::stderr_is_tty`], which answers the process-wide, capture-blind
/// question of whether stderr itself is a terminal.
pub(crate) fn is_tty<'v>(strand: &Strand<'v, '_>) -> bool {
    let global = strand.state::<Global<'v>>();
    if captured(strand) {
        return global.local.get(strand).capture_is_tty();
    }
    global.terminal.stderr_is_terminal
}

/// Reads a console's `is_tty`, for snapshotting when it is installed.
pub(crate) fn console_is_tty<'v, 's>(
    strand: &mut Strand<'v, 's>,
    console: &Value<'v>,
) -> Result<'v, 's, bool> {
    let sym = strand.state::<Global<'v>>().syms.is_tty;
    strand.with_slots_sync(|strand, [mut value]| {
        console.get(strand, sym, &mut value)?;
        value
            .as_bool(strand)
            .ok_or_else(|| Error::type_error(strand, "is_tty: expected `Bool`"))
    })
}

/// The ambient console's `geometry()`.
///
/// Forwards to whatever `term.output()` resolves to, the same way
/// `write`/`writeln`/`flush` do: the host outside a capture, the installed
/// console inside one.
pub(crate) async fn geometry<'v, 's, 'a>(
    strand: &'a mut Strand<'v, 's>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    let global = strand.state::<Global<'v>>();
    strand
        .with_slots(async move |strand, [mut rcvr]| {
            let root = global.capture.slot(strand);
            if root.is_nil() {
                global
                    .types
                    .host_console
                    .create(strand, HostConsole, &mut rcvr);
            } else {
                Output::set(strand, &mut rcvr, &root);
            }
            method!(strand, &rcvr, global.syms.geometry, out).await
        })
        .await
}

/// Writes to the host console, optionally terminating the line.
///
/// The terminator goes out in the *same* critical section as the payload. Two
/// `write_all`s under two locks would let a concurrent strand interleave its
/// own line between them, which shows up as overlapping output — plain-mode
/// progress lines running together being the way it is usually noticed.
async fn write_host<'v, 's>(
    strand: &mut Strand<'v, 's>,
    bytes: &[u8],
    terminate: bool,
) -> Result<'v, 's, ()> {
    let global = strand.state::<Global<'v>>();
    let mut writer = global.terminal.writer.lock().await;
    writer
        .write_all(bytes)
        .await
        .map_err(|error| error.into_sys(strand))?;
    if terminate {
        writer
            .write_all(HOST_LINE_ENDING.as_bytes())
            .await
            .map_err(|error| error.into_sys(strand))?;
    }
    Ok(())
}

/// Writes a line to `target`, using the line ending that was active when the
/// target was captured. A nil target denotes the host console.
pub(crate) async fn write_line_to<'v, 's>(
    strand: &mut Strand<'v, 's>,
    target: &Value<'v>,
    line_ending: &[u8],
    bytes: &[u8],
) -> Result<'v, 's, ()> {
    if target.is_nil() {
        return write_host(strand, bytes, true).await;
    }
    let mut line = Vec::with_capacity(bytes.len() + line_ending.len());
    line.extend_from_slice(bytes);
    line.extend_from_slice(line_ending);
    let write = strand.state::<Global<'v>>().syms.write;
    dispatch_to(strand, target, write, &line[..]).await
}

async fn dispatch<'v, 's>(
    strand: &mut Strand<'v, 's>,
    method: Sym<'v, 'v>,
    arg: impl Input<'v>,
) -> Result<'v, 's, ()> {
    let global = strand.state::<Global<'v>>();
    let root = global.capture.slot(strand);
    dispatch_to(strand, &root, method, arg).await
}

async fn dispatch_to<'v, 's>(
    strand: &mut Strand<'v, 's>,
    target: &Value<'v>,
    method: Sym<'v, 'v>,
    arg: impl Input<'v>,
) -> Result<'v, 's, ()> {
    let global = strand.state::<Global<'v>>();
    strand
        .with_slots(async move |strand, [mut rcvr, mut out]| {
            Output::set(strand, &mut rcvr, target);
            let prev = global.local.get(strand).set_capturing(true);
            let result = method!(strand, &rcvr, method, &mut out, arg).await;
            global.local.get(strand).set_capturing(prev);
            result
        })
        .await
}

async fn dispatch_no_args<'v, 's>(
    strand: &mut Strand<'v, 's>,
    method: Sym<'v, 'v>,
) -> Result<'v, 's, ()> {
    let global = strand.state::<Global<'v>>();
    strand
        .with_slots(async move |strand, [mut rcvr, mut out]| {
            let root = global.capture.slot(strand);
            Output::set(strand, &mut rcvr, &root);
            let prev = global.local.get(strand).set_capturing(true);
            let result = method!(strand, &rcvr, method, &mut out).await;
            global.local.get(strand).set_capturing(prev);
            result
        })
        .await
}
