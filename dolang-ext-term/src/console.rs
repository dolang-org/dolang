use crate::global::Tag;

use dolang::runtime::value::fmt::Format;
use dolang::runtime::{
    Error, Instance, Object, Output, Result, Slot, Strand, Value, call,
    error::ErrorKind,
    method,
    object::{Mut, Ref, TypeBuilder, fmt},
    unpack,
    value::{Nil, TypeObject, View},
};

use crate::geometry::create_geometry;
use crate::util::{data_bytes, write_data};

/// The console interface: where human-readable output goes.
///
/// A console is a write-only byte stream, an `io.Io`. `term.echo` always
/// terminates a line and `term.print` never does, so the terminator has to be
/// materialized into the byte stream — and it is the console that knows which
/// terminator to use, since that follows the device rather than the caller.
///
/// So a console *owns* the policy but does not *apply* it: `line_ending`
/// reports the terminator and `write` writes exactly the bytes it is given.
/// `write` takes any number of pieces, so a caller that wants a line passes the
/// line and its terminator to one `write`, which is also what keeps concurrent
/// writers from interleaving. `put`, `write_all` and the rest come from `Io`.
///
/// Native extension types cannot be abstract, so the methods here throw rather
/// than being absent. The concrete implementations are the host console
/// installed with [`crate::install_console`], `term.IoConsole` (an adapter
/// over any `Io`), and whatever Do code subclasses this with.
pub struct Console;

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
            .supertype(TypeObject::Io)
            .method("write", async move |_this, strand, _args, _out| {
                Err(Error::not_supported(strand))
            })
            .method("flush", async move |_this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                Err(Error::not_supported(strand))
            })
            // Unlike the write methods, the capability members have a safe
            // default, so a Do subclass that supplies only the two above still
            // answers them — by delegating to these.
            .get("writable", |_this, strand, out| {
                Output::set(strand, out, true);
                Ok(())
            })
            .get("line_ending", |_this, strand, out| {
                Output::set(strand, out, LINE_ENDING);
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
            .method("geometry", async move |_this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                create_geometry(strand, None, None, out);
                Ok(())
            })
    }
}

/// The strand's default output, exported as `term.default`.
///
/// A forwarder, not a destination: every operation resolves *at call time* to
/// whatever `term.output()` currently is — the host console, or an installed
/// capture. A host binds it as a strand's implicit output (see
/// [`crate::default_output`]) so that unnamed program output keeps following
/// capture and terminal takeover, the same way naming `term.output()` itself
/// would, without every caller having to re-resolve it.
///
/// It lends no byte handle of its own: a native consumer goes through `Io`'s
/// bridge, which calls `write`, so each write still resolves afresh.
///
/// Contrast `term.console`, which pins to the host and is never intercepted.
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
            .supertype(TypeObject::Io)
            .method("write", async move |_this, strand, args, out| {
                let bytes = write_data(strand, args)?;
                write(strand, &bytes).await?;
                Output::set(strand, out, bytes.len());
                Ok(())
            })
            .method("flush", async move |_this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                flush(strand).await
            })
            .get("writable", |_this, strand, out| {
                Output::set(strand, out, true);
                Ok(())
            })
            .get("line_ending", |_this, strand, mut out| {
                // Resolved at call time like everything else here, so it
                // follows an installed capture rather than the host.
                route_line_ending(strand, &mut out);
                Ok(())
            })
            .get("can_style", |_this, strand, out| {
                Output::set(strand, out, ansi(strand));
                Ok(())
            })
            .get("is_tty", |_this, strand, out| {
                let is_tty = is_tty(strand)?;
                Output::set(strand, out, is_tty);
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
        let global = strand.state::<Tag>();
        Ok(global.types.default.cast(other).is_some())
    }
}

/// The line ending of a console that has no opinion of its own.
///
/// LF on every platform: a console is a terminal-shaped stream, not a file, and
/// `echo` has always written LF on Windows too.
pub(crate) const LINE_ENDING: &str = "\n";

/// A console over any `io.Io`, supplying the rest of the console interface.
///
/// Each `write` reaches the inner stream as one `write_all` of the joined
/// pieces, so a line and its terminator arrive together and a short write is
/// finished rather than lost. Nothing is translated on the way through.
pub(crate) struct IoConsole {
    /// Off unless the caller asked for styling, since the point of capturing
    /// into a stream is usually to assert on plain text.
    can_style: bool,
    /// Set by `close`, which also releases the inner stream.
    closed: bool,
}

impl<'v> Object<'v> for IoConsole {
    const NAME: &'v str = "IoConsole";
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
        let global = strand.state::<Tag>();
        let can_style_sym = global.syms.can_style;
        let ([target], [can_style]) = unpack!(strand, args, 1, 0, can_style_sym = None)?;
        let can_style = can_style
            .map(|value| value.to_bool(strand))
            .transpose()?
            .unwrap_or(false);
        if !target.is_instance_of(strand, TypeObject::Io) {
            return Err(Error::type_error(strand, "expected `io.Io`"));
        }
        create_io_console(strand, &target, can_style, out)
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("writable", |this, strand, out| {
                let closed = this.borrow(strand)?.closed;
                Output::set(strand, out, !closed);
                Ok(())
            })
            .get("can_style", |this, strand, out| {
                let can_style = this.borrow(strand)?.can_style;
                Output::set(strand, out, can_style);
                Ok(())
            })
            .get("is_tty", |_this, strand, out| {
                // An adapter over an arbitrary stream is never a terminal.
                Output::set(strand, out, false);
                Ok(())
            })
            .method("geometry", async move |_this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                create_geometry(strand, None, None, out);
                Ok(())
            })
            .get("line_ending", |_this, strand, out| {
                Output::set(strand, out, LINE_ENDING);
                Ok(())
            })
            .method_with_slots(
                "write",
                async move |this, strand, args, out, [mut stream, mut tmp]| {
                    let bytes = write_data(strand, args)?;
                    inner(this, strand, &mut stream)?;
                    let global = strand.state::<Tag>();
                    method!(strand, &stream, global.syms.write_all, &mut tmp, &bytes[..]).await?;
                    Output::set(strand, out, bytes.len());
                    Ok(())
                },
            )
            .method_with_slots(
                "flush",
                async move |this, strand, args, out, [mut stream, mut flush]| {
                    let ([], []) = unpack!(strand, args, 0, 0)?;
                    inner(this, strand, &mut stream)?;
                    // An `Io` need not flush; one without the member has
                    // nothing held back.
                    let global = strand.state::<Tag>();
                    match stream.get(strand, global.syms.flush, &mut flush) {
                        Ok(()) => call!(strand, &flush, out).await,
                        Err(error) if error.kind() == ErrorKind::Field => Ok(()),
                        Err(error) => Err(error),
                    }
                },
            )
            .method("close", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let mut me = this.borrow_mut(strand)?;
                me.closed = true;
                Output::set(strand, Mut::slot_mut::<0>(&mut me), Nil);
                Ok(())
            })
    }

    fn debug<'a, 's>(
        _this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "<io console>")
    }
}

/// Wraps `target`, an `io.Io`, in an [`IoConsole`], rooting it in the
/// adapter's slot.
pub(crate) fn create_io_console<'v, 's>(
    strand: &mut Strand<'v, 's>,
    target: &Value<'v>,
    can_style: bool,
    mut out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let global = strand.state::<Tag>();
    global.types.io_console.create(
        strand,
        IoConsole {
            can_style,
            closed: false,
        },
        &mut out,
    );
    global
        .types
        .io_console
        .cast(&out)
        .unwrap()
        .enter_sync(strand, |strand, inst| {
            let mut borrow = inst.borrow_mut(strand)?;
            Output::set(strand, Mut::slot_mut::<0>(&mut borrow), target);
            Ok(())
        })
}

/// Stores an [`IoConsole`]'s inner stream in `out`, raising once it is closed.
fn inner<'v, 's>(
    this: Instance<'v, '_, IoConsole>,
    strand: &mut Strand<'v, 's>,
    out: &mut Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let closed = {
        let me = this.borrow(strand)?;
        let closed = me.closed;
        Output::set(strand, &mut *out, Ref::slot::<0>(&me));
        closed
    };
    if closed {
        return Err(Error::state_error(strand, "console is closed"));
    }
    Ok(())
}

/// The console behind `term.sub` and `term.mute`: accumulates the byte stream
/// verbatim, or discards it.
///
/// No framing at all — `term.sub` reports exactly what was written, so
/// `print a`, `print b`, `echo c` is `"abc\n"`.
pub(crate) struct SubConsole {
    /// The text written so far, or `None` to discard it.
    text: Option<String>,
    can_style: bool,
}

impl SubConsole {
    pub(crate) fn new(can_style: bool) -> Self {
        Self {
            text: Some(String::new()),
            can_style,
        }
    }

    /// A console that discards everything written to it, for `term.mute`.
    pub(crate) fn discard() -> Self {
        Self {
            text: None,
            can_style: false,
        }
    }

    pub(crate) fn text(&self) -> &str {
        self.text.as_deref().unwrap_or("")
    }

    fn append<'v, 's>(&mut self, strand: &mut Strand<'v, 's>, bytes: &[u8]) -> Result<'v, 's, ()> {
        let Some(text) = &mut self.text else {
            return Ok(());
        };
        let bytes = std::str::from_utf8(bytes)
            .map_err(|_| Error::runtime(strand, "term.sub: captured invalid UTF-8"))?;
        text.push_str(bytes);
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
            .supertype(TypeObject::Io)
            .method("write", async move |this, strand, args, out| {
                let bytes = write_data(strand, args)?;
                this.borrow_mut(strand)?.append(strand, &bytes)?;
                Output::set(strand, out, bytes.len());
                Ok(())
            })
            .method("flush", async move |_this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                Ok(())
            })
            .get("writable", |_this, strand, out| {
                Output::set(strand, out, true);
                Ok(())
            })
            .get("line_ending", |_this, strand, out| {
                Output::set(strand, out, LINE_ENDING);
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
            .method("geometry", async move |_this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                create_geometry(strand, None, None, out);
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
}

/// Whether a capture is installed and should be dispatched to.
///
/// False while already dispatching into a console, so a console whose own
/// `write` calls `echo` falls through to the host instead of recursing.
fn captured<'v>(strand: &Strand<'v, '_>) -> bool {
    let global = strand.state::<Tag>();
    !global.capture.slot(strand).is_nil() && !global.local.get(strand).capturing()
}

/// Whether a console is installed for this strand at all, whether or not a
/// write is currently being dispatched into it.
pub(crate) fn is_captured<'v>(strand: &Strand<'v, '_>) -> bool {
    let global = strand.state::<Tag>();
    !global.capture.slot(strand).is_nil()
}

/// Stores the host console in `out`.
fn host<'v, 's>(strand: &mut Strand<'v, 's>, out: &mut Slot<'v, '_>) -> Result<'v, 's, ()> {
    let global = strand.state::<Tag>();
    let console = global.host.console.borrow();
    if console.is_nil() {
        return Err(Error::state_error(strand, "no console installed"));
    }
    Output::set(strand, out, &*console);
    Ok(())
}

/// Stores the console output currently goes to in `out`: the installed
/// capture, or else the host.
fn route<'v, 's>(strand: &mut Strand<'v, 's>, out: &mut Slot<'v, '_>) -> Result<'v, 's, ()> {
    if !captured(strand) {
        return host(strand, out);
    }
    let global = strand.state::<Tag>();
    let root = global.capture.slot(strand);
    Output::set(strand, out, &root);
    Ok(())
}

/// Stores the line ending of the console [`route`] picks in `out`, as read
/// when that console was installed.
fn route_line_ending<'v>(strand: &mut Strand<'v, '_>, out: &mut Slot<'v, '_>) {
    let global = strand.state::<Tag>();
    if captured(strand) {
        let ending = global.capture_line_ending.slot(strand);
        Output::set(strand, out, &ending);
    } else {
        let ending = global.host.line_ending.borrow();
        Output::set(strand, out, &*ending);
    }
}

/// Runs `f` with the recursion guard set, so that output produced from inside
/// a console's own methods falls through to the host.
async fn guarded<'v, 's, R>(
    strand: &mut Strand<'v, 's>,
    f: impl AsyncFnOnce(&mut Strand<'v, 's>) -> R,
) -> R {
    let global = strand.state::<Tag>();
    let prev = global.local.get(strand).set_capturing(true);
    let result = f(strand).await;
    global.local.get(strand).set_capturing(prev);
    result
}

/// Writes bytes to the ambient console verbatim.
pub(crate) async fn write<'v, 's>(strand: &mut Strand<'v, 's>, bytes: &[u8]) -> Result<'v, 's, ()> {
    let global = strand.state::<Tag>();
    strand
        .with_slots(async move |strand, [mut rcvr, mut out]| {
            route(strand, &mut rcvr)?;
            guarded(strand, async |strand| {
                method!(strand, &rcvr, global.syms.write, &mut out, bytes).await
            })
            .await
        })
        .await
}

/// Writes a `Str` or `Bin` to the ambient console verbatim.
pub(crate) async fn write_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    data: &Value<'v>,
) -> Result<'v, 's, ()> {
    let global = strand.state::<Tag>();
    strand
        .with_slots(async move |strand, [mut rcvr, mut out]| {
            route(strand, &mut rcvr)?;
            guarded(strand, async |strand| {
                method!(strand, &rcvr, global.syms.write, &mut out, data).await
            })
            .await
        })
        .await
}

/// Writes a line to the ambient console, followed by its line ending.
///
/// The line and its terminator are two pieces of a *single* `write`: two
/// writes would let a concurrent strand slip its own line between the payload
/// and the terminator, which shows up as overlapping output.
pub(crate) async fn write_line<'v, 's>(
    strand: &mut Strand<'v, 's>,
    line: &Value<'v>,
) -> Result<'v, 's, ()> {
    let global = strand.state::<Tag>();
    strand
        .with_slots(async move |strand, [mut rcvr, mut ending, mut out]| {
            route(strand, &mut rcvr)?;
            route_line_ending(strand, &mut ending);
            guarded(strand, async |strand| {
                method!(strand, &rcvr, global.syms.write, &mut out, line, &ending).await
            })
            .await
        })
        .await
}

/// Writes bytes to the ambient console followed by its line ending, as with
/// [`write_line`].
pub(crate) async fn writeln<'v, 's>(
    strand: &mut Strand<'v, 's>,
    bytes: &[u8],
) -> Result<'v, 's, ()> {
    let global = strand.state::<Tag>();
    strand
        .with_slots(async move |strand, [mut rcvr, mut ending, mut out]| {
            route(strand, &mut rcvr)?;
            route_line_ending(strand, &mut ending);
            guarded(strand, async |strand| {
                method!(strand, &rcvr, global.syms.write, &mut out, bytes, &ending).await
            })
            .await
        })
        .await
}

/// Writes a line to `target`, a console snapshotted earlier, followed by
/// `line_ending`. A nil target denotes the host console.
pub(crate) async fn write_line_to<'v, 's>(
    strand: &mut Strand<'v, 's>,
    target: &Value<'v>,
    line_ending: &[u8],
    bytes: &[u8],
) -> Result<'v, 's, ()> {
    let global = strand.state::<Tag>();
    strand
        .with_slots(async move |strand, [mut rcvr, mut out]| {
            if target.is_nil() {
                host(strand, &mut rcvr)?;
            } else {
                Output::set(strand, &mut rcvr, target);
            }
            guarded(strand, async |strand| {
                method!(
                    strand,
                    &rcvr,
                    global.syms.write,
                    &mut out,
                    bytes,
                    line_ending
                )
                .await
            })
            .await
        })
        .await
}

/// Flushes the ambient console.
pub(crate) async fn flush<'v, 's>(strand: &mut Strand<'v, 's>) -> Result<'v, 's, ()> {
    let global = strand.state::<Tag>();
    strand
        .with_slots(async move |strand, [mut rcvr, mut out]| {
            route(strand, &mut rcvr)?;
            guarded(strand, async |strand| {
                method!(strand, &rcvr, global.syms.flush, &mut out).await
            })
            .await
        })
        .await
}

/// The ambient console's line ending, as bytes to append.
pub(crate) fn ambient_line_ending<'v, 's>(strand: &mut Strand<'v, 's>) -> Result<'v, 's, Vec<u8>> {
    strand.with_slots_sync(|strand, [mut ending]| {
        route_line_ending(strand, &mut ending);
        if ending.is_nil() {
            return Err(Error::state_error(strand, "no console installed"));
        }
        data_bytes(strand, &ending)
    })
}

/// Stores the console an enclosing capture installed in `out`, or `nil` for
/// the host console.
pub(crate) fn capture_root<'v>(strand: &mut Strand<'v, '_>, out: impl Output<'v>) {
    let global = strand.state::<Tag>();
    let capture = global.capture.slot(strand);
    Output::set(strand, out, &capture);
}

/// Stores the ambient console in `out`: whatever an enclosing capture
/// installed, else the host.
pub(crate) fn ambient<'v, 's>(
    strand: &mut Strand<'v, 's>,
    out: &mut Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let global = strand.state::<Tag>();
    let capture = global.capture.slot(strand);
    if capture.is_nil() {
        return host(strand, out);
    }
    Output::set(strand, out, &capture);
    Ok(())
}

/// Stores the host console in `out`, or `nil` if none is installed.
pub(crate) fn host_or_nil<'v>(strand: &mut Strand<'v, '_>, out: impl Output<'v>) {
    let global = strand.state::<Tag>();
    let console = global.host.console.borrow();
    Output::set(strand, out, &*console);
}

/// Whether ANSI styling should be emitted to the ambient console.
///
/// This is the `can_style` the console reported when it was installed — for a
/// capture, off by default, since a capture is not a terminal and a test
/// asserting on `echo`ed text would otherwise pass piped and fail on a
/// developer's terminal.
pub(crate) fn ansi<'v>(strand: &Strand<'v, '_>) -> bool {
    let global = strand.state::<Tag>();
    if captured(strand) {
        return global.local.get(strand).capture_can_style();
    }
    global.host.can_style.get()
}

/// Reads a console's `can_style`, for snapshotting when it is installed.
pub(crate) fn can_style<'v, 's>(
    strand: &mut Strand<'v, 's>,
    console: &Value<'v>,
) -> Result<'v, 's, bool> {
    let sym = strand.state::<Tag>().syms.can_style;
    strand.with_slots_sync(|strand, [mut value]| {
        console.get(strand, sym, &mut value)?;
        value
            .as_bool(strand)
            .ok_or_else(|| Error::type_error(strand, "can_style: expected `Bool`"))
    })
}

/// Reads a console's `line_ending` into `out`, for snapshotting when it is
/// installed.
pub(crate) fn line_ending<'v, 's>(
    strand: &mut Strand<'v, 's>,
    console: &Value<'v>,
    out: &mut Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let sym = strand.state::<Tag>().syms.line_ending;
    console.get(strand, sym, &mut *out)?;
    match out.view(strand) {
        View::Str(_) | View::Bin(_) => Ok(()),
        _ => Err(Error::type_error(
            strand,
            "line_ending: expected `Str` or `Bin`",
        )),
    }
}

/// Whether the ambient console is usable as a terminal.
///
/// Read live from the console rather than snapshotted: the host console stops
/// being usable as a terminal while an extension has taken it over.
pub(crate) fn is_tty<'v, 's>(strand: &mut Strand<'v, 's>) -> Result<'v, 's, bool> {
    let sym = strand.state::<Tag>().syms.is_tty;
    strand.with_slots_sync(|strand, [mut rcvr, mut value]| {
        route(strand, &mut rcvr)?;
        rcvr.get(strand, sym, &mut value)?;
        value
            .as_bool(strand)
            .ok_or_else(|| Error::type_error(strand, "is_tty: expected `Bool`"))
    })
}

/// The ambient console's `geometry()`.
///
/// Forwards to whatever `term.output()` resolves to, the same way `write` and
/// `flush` do: the host outside a capture, the installed console inside one.
pub(crate) async fn geometry<'v, 's, 'a>(
    strand: &'a mut Strand<'v, 's>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    let global = strand.state::<Tag>();
    strand
        .with_slots(async move |strand, [mut rcvr]| {
            route(strand, &mut rcvr)?;
            method!(strand, &rcvr, global.syms.geometry, out).await
        })
        .await
}
