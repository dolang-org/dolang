#![deny(warnings)]

mod console;
mod extension;
mod geometry;
mod global;
mod local;
mod term;
mod util;

use dolang::runtime::{Args, Input, Output, Result, Slot, Strand, Type, Value, vm::Builder};

pub use crate::{console::Console, extension::TermExt, geometry::Geometry};

use crate::global::Global;

/// Installs the host console: the console `term.console` names, and the one
/// output goes to when nothing is captured.
///
/// `can_style` and `line_ending` must agree with what the console itself
/// reports. They are read once, here, rather than on every write.
///
/// Until a console is installed, `term.console` is `nil` and output to the
/// host raises a `StateError`.
pub fn install_console<'v>(
    builder: &mut Builder<'v>,
    console: impl Input<'v>,
    can_style: bool,
    line_ending: &str,
) {
    let global = builder.state::<Global<'v>>();
    Output::set(builder, &mut *global.host.console.borrow_mut(), console);
    Output::set(
        builder,
        &mut *global.host.line_ending.borrow_mut(),
        line_ending,
    );
    global.host.can_style.set(can_style);
}

/// The `term.Console` type object.
///
/// A native console type registers this as a nominal supertype. The extension
/// registering the subtype must name [`TermExt`] in its `DEPENDS`, or this type
/// may not exist yet.
pub fn console_type<'v>(builder: &Builder<'v>) -> Type<'v, Console> {
    builder.state::<Global<'v>>().types.console
}

/// The `term.Geometry` type object.
///
/// A native geometry type registers this as a nominal supertype, subject to
/// the same `DEPENDS` requirement as [`console_type`].
pub fn geometry_type<'v>(builder: &Builder<'v>) -> Type<'v, Geometry> {
    builder.state::<Global<'v>>().types.geometry
}

/// Collects the arguments of a `Console.write` call — any number of `Str` or
/// `Bin` values — as the bytes to write, in order.
pub fn write_data<'v, 's>(
    strand: &mut Strand<'v, 's>,
    args: Args<'v, '_>,
) -> Result<'v, 's, Vec<u8>> {
    util::write_data(strand, args)
}

/// Writes bytes verbatim to the ambient console: the one an enclosing capture
/// installed, else the host console.
pub async fn write<'v, 's>(strand: &mut Strand<'v, 's>, bytes: &[u8]) -> Result<'v, 's, ()> {
    console::write(strand, bytes).await
}

/// Writes bytes to the ambient console followed by its line ending, in a
/// single `write`.
pub async fn writeln<'v, 's>(strand: &mut Strand<'v, 's>, bytes: &[u8]) -> Result<'v, 's, ()> {
    console::writeln(strand, bytes).await
}

/// Flushes the ambient console.
pub async fn flush<'v, 's>(strand: &mut Strand<'v, 's>) -> Result<'v, 's, ()> {
    console::flush(strand).await
}

/// Whether ANSI styling should be emitted to the ambient console, as that
/// console reported when it was installed.
pub fn ansi_enabled<'v>(strand: &Strand<'v, '_>) -> bool {
    console::ansi(strand)
}

/// Whether an enclosing `term.capture`, `term.sub` or `term.mute` has installed
/// a console for this strand.
pub fn is_captured<'v>(strand: &Strand<'v, '_>) -> bool {
    console::is_captured(strand)
}

/// Instantiates `term.default`, the output handle that forwards to the ambient
/// console at call time.
pub fn default_output<'v>(strand: &mut Strand<'v, '_>, out: impl Output<'v>) {
    let global = strand.state::<Global<'v>>();
    global
        .types
        .default
        .create(strand, console::DefaultOutput, out)
}

/// Whether `value` is `term.default`.
pub fn is_default_output<'v>(strand: &Strand<'v, '_>, value: &Value<'v>) -> bool {
    let global = strand.state::<Global<'v>>();
    global.types.default.cast(value).is_some()
}

/// Stores the console an enclosing capture installed in `out`, or `nil` for the
/// host console.
///
/// The caller is responsible for keeping that value rooted for as long as it
/// needs to keep using this output.
pub fn terminal_output<'v>(strand: &mut Strand<'v, '_>, out: impl Output<'v>) {
    console::capture_root(strand, out)
}

/// Returns the line ending of the ambient console.
pub fn terminal_line_ending<'v, 's>(strand: &mut Strand<'v, 's>) -> Result<'v, 's, Vec<u8>> {
    console::ambient_line_ending(strand)
}

/// Writes a line through a console obtained from [`terminal_output`],
/// terminated with `line_ending`.
pub async fn write_terminal_line<'v, 's>(
    strand: &mut Strand<'v, 's>,
    output: &Value<'v>,
    line_ending: &[u8],
    line: &str,
) -> Result<'v, 's, ()> {
    console::write_line_to(strand, output, line_ending, line.as_bytes()).await
}

/// Filters ANSI-formatted text for output: SGR styling is kept when `ansi` is
/// set and dropped otherwise, and every other control sequence is dropped.
pub fn filter_preformatted<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &str,
    ansi: bool,
) -> Result<'v, 's, String> {
    term::filter_preformatted(strand, value, ansi)
}

/// Creates a `term.Text` from ANSI-formatted text, as `term.preformat` does.
pub fn preformatted_text<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &str,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let global = strand.state::<Global<'v>>();
    term::create_preformatted_text(strand, global, value, out)
}
