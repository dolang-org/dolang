use std::{
    borrow::Cow,
    error,
    fmt::{self, Display, Formatter},
    ops::ControlFlow,
};

use crate::value::fmt::Format;

use dolang_util::alias;

use crate::{
    arg::Args,
    error::{Error, ErrorKind, Result},
    gc::{Collect, arena::Visit},
    object::protocol::members,
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{Output, Slot, Value},
    vm::Vm,
};

use super::protocol::{Inspect, Protocol, Recv};

#[derive(Debug)]
pub(crate) enum Boxed<'v> {
    Unsupported,
    Immutable,
    Concurrency(Option<Cow<'v, str>>),
    Type(Cow<'v, str>),
    Value(Cow<'v, str>),
    State(Cow<'v, str>),
    Overflow,
    ZeroDiv,
    SinkStop,
    IterStop,
    Index,
    Canceled,
    TimedOut,
    Field(alias::Box<str>),
    UnexpectedPos(usize),
    UnexpectedKey(alias::Box<str>),
    DuplicateKey(alias::Box<str>),
    MissingPos(usize),
    MissingKey(alias::Box<str>),
    CyclicImport(alias::Box<str>),
    Import(alias::Box<str>),
    Compile(Box<dyn error::Error>),
    Bytecode(Box<dyn error::Error>),
    Runtime(Box<dyn error::Error>),
    Abort(Box<dyn error::Error>),
}

impl<'v> Boxed<'v> {
    pub(crate) fn kind(&self) -> ErrorKind {
        use Boxed::*;
        match self {
            Unsupported => ErrorKind::Unsupported,
            Immutable => ErrorKind::Immutable,
            Concurrency(_) => ErrorKind::Concurrency,
            Type(_) => ErrorKind::Type,
            Value(_) => ErrorKind::Value,
            State(_) => ErrorKind::State,
            Index => ErrorKind::Index,
            Field(_) => ErrorKind::Field,
            UnexpectedPos(_) => ErrorKind::UnexpectedPos,
            UnexpectedKey(_) | DuplicateKey(_) => ErrorKind::UnexpectedKey,
            MissingPos(_) => ErrorKind::MissingPos,
            MissingKey(_) => ErrorKind::MissingKey,
            Overflow => ErrorKind::Overflow,
            ZeroDiv => ErrorKind::ZeroDiv,
            SinkStop => ErrorKind::SinkStop,
            IterStop => ErrorKind::IterStop,
            CyclicImport(_) => ErrorKind::CyclicImport,
            Import(_) => ErrorKind::Import,
            Compile(_) => ErrorKind::Compile,
            Bytecode(_) => ErrorKind::Bytecode,
            Runtime(_) => ErrorKind::Runtime,
            Abort(_) => ErrorKind::Abort,
            Canceled => ErrorKind::Canceled,
            TimedOut => ErrorKind::TimedOut,
        }
    }
}

impl<'v> Display for Boxed<'v> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        use Boxed::*;

        match self {
            Unsupported => write!(f, "unsupported operation"),
            Immutable => write!(f, "object is immutable"),
            Concurrency(None) => write!(f, "conflicting concurrent operation"),
            Concurrency(Some(msg)) => write!(f, "conflicting concurrent operation: {msg}"),
            Type(msg) => write!(f, "type error: {msg}"),
            Value(msg) => write!(f, "value error: {msg}"),
            State(msg) => write!(f, "state error: {msg}"),
            Overflow => write!(f, "numeric overflow"),
            ZeroDiv => write!(f, "integer zero divisor"),
            SinkStop => write!(f, "sink stopped"),
            IterStop => write!(f, "iterator stopped"),
            Index => write!(f, "index out of range or invalid"),
            Canceled => write!(f, "strand canceled"),
            TimedOut => write!(f, "strand timed out"),
            Field(name) => write!(f, "no such field: {name}"),
            UnexpectedPos(i) => write!(f, "unexpected positional item: {i}"),
            UnexpectedKey(name) => write!(f, "unexpected key item: {name}"),
            DuplicateKey(name) => write!(f, "duplicate keyword argument: {name}"),
            MissingPos(i) => write!(f, "missing positional item: {i}"),
            MissingKey(name) => write!(f, "missing key item: {name}"),
            Import(name) => write!(f, "module not found: {name}"),
            CyclicImport(name) => {
                write!(f, "cycle detected importing module: {name}")
            }
            Compile(error) => write!(f, "compile error: {error}"),
            Bytecode(error) => write!(f, "bytecode error: {error}"),
            Runtime(error) => Display::fmt(error, f),
            Abort(error) => Display::fmt(error, f),
        }
    }
}

unsafe impl<'v> Collect for Boxed<'v> {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        unreachable!()
    }
}

impl<'v> Protocol<'v> for Boxed<'v> {
    fn op_type<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) {
        let kind = this.get().kind();
        out.store(variant_singleton(strand, kind).dup())
    }

    fn op_display<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "{}", this.get())
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<error: {}>", this.get())
    }
}

// ── Error Class ─────────────────────────────────────────────────

pub(crate) struct Type;

unsafe impl Collect for Type {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for Type {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().type_obj)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<type Error>")
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: true,
            members: &[],
            type_members: members![
                Method(sym::VERBATIM_METHOD),
                Method(sym::STR_METHOD),
                Method(sym::DBG_METHOD),
                Method(sym::CALL_METHOD),
            ],
        })
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        _out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([value], []) = unpack!(strand, args, 1, 0)?;
        Err(Error::from_value(strand, value))
    }
}

// ── Error Variant Classes ───────────────────────────────────────

pub(crate) struct VariantType(pub(crate) ErrorKind);

unsafe impl Collect for VariantType {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for VariantType {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().type_obj)
    }

    fn op_subtype<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &crate::value::Value<'v>,
    ) -> bool {
        supertype.eq(strand, &this)
            || strand.singletons().error.eq(strand, supertype)
            || (is_runtime_superkind(this.get().0)
                && strand.singletons().error_runtime.eq(strand, supertype))
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<type std.{}>", variant_name(this.get().0))
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: false,
            members: members![
                Method(sym::STR_METHOD),
                Method(sym::DBG_METHOD),
                Method(sym::FMT_METHOD),
            ],
            type_members: members![
                Method(sym::VERBATIM_METHOD),
                Method(sym::STR_METHOD),
                Method(sym::DBG_METHOD),
                Method(sym::CALL_METHOD),
            ],
        })
    }

    async fn op_call<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let mut error = construct(this.get().0, strand, args)?;
        error.get_value(strand, out);
        Ok(())
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let kind = this.get().0;
        match method.tag() {
            sym::INIT_METHOD => {
                if !is_constructible(kind) {
                    return Err(Error::type_error(
                        strand,
                        format!("{} cannot be subclassed", variant_name(kind)),
                    ));
                }
                let ([self_val], [], rest) = unpack!(strand, args, 1, 0, ...)?;
                let mut error = construct(kind, strand, rest)?;
                error.get_value(strand, Slot::reborrow(&mut out));
                let native = out.take();
                self_val.op_fill(strand, variant_singleton(strand, kind), native)
            }
            _ => {
                strand
                    .with_slots(async move |strand, [mut func]| {
                        Self::op_get(this, strand, method, Slot::reborrow(&mut func))?;
                        func.op_call(strand, args, out).await
                    })
                    .await
            }
        }
    }
}

/// Builds the boxed error a variant type's constructor produces.
///
/// Shared by [`VariantType::op_call`] and the `(init)` case of
/// [`VariantType::op_mcall`], so a Do subclass chaining to its supertype gets
/// exactly the arguments the direct constructor takes.
fn construct<'v, 's>(
    kind: ErrorKind,
    strand: &mut Strand<'v, 's>,
    args: Args<'v, '_>,
) -> Result<'v, 's, Error<'v, 's>> {
    Ok(match kind {
        ErrorKind::Unsupported => {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            Error::not_supported(strand)
        }
        ErrorKind::Immutable => {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            Error::immutable(strand)
        }
        ErrorKind::Concurrency => {
            let ([], [msg]) = unpack!(strand, args, 0, 1)?;
            match msg {
                Some(msg) => {
                    let msg = expect_string(strand, &msg)?;
                    Error::concurrency_msg(strand, msg)
                }
                None => Error::concurrency(strand),
            }
        }
        ErrorKind::Type => {
            let ([msg], []) = unpack!(strand, args, 1, 0)?;
            let msg = expect_string(strand, &msg)?;
            Error::type_error(strand, msg)
        }
        ErrorKind::Value => {
            let ([msg], []) = unpack!(strand, args, 1, 0)?;
            let msg = expect_string(strand, &msg)?;
            Error::value(strand, msg)
        }
        ErrorKind::State => {
            let ([msg], []) = unpack!(strand, args, 1, 0)?;
            let msg = expect_string(strand, &msg)?;
            Error::state_error(strand, msg)
        }
        ErrorKind::Index => {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            Error::index(strand)
        }
        ErrorKind::Field => {
            let ([name], []) = unpack!(strand, args, 1, 0)?;
            let name = name
                .as_sym(strand)
                .ok_or_else(|| Error::type_error(strand, "expected `Sym`"))?;
            Error::field(strand, name)
        }
        ErrorKind::UnexpectedPos => {
            let ([index], []) = unpack!(strand, args, 1, 0)?;
            let index = expect_index(strand, &index)?;
            Error::unexpected_positional(strand, index)
        }
        ErrorKind::UnexpectedKey => {
            let ([key], []) = unpack!(strand, args, 1, 0)?;
            Error::unexpected_key(strand, key)
        }
        ErrorKind::MissingPos => {
            let ([index], []) = unpack!(strand, args, 1, 0)?;
            let index = expect_index(strand, &index)?;
            Error::missing_positional(strand, index)
        }
        ErrorKind::MissingKey => {
            let ([key], []) = unpack!(strand, args, 1, 0)?;
            Error::missing_key(strand, key)
        }
        ErrorKind::Overflow => {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            Error::overflow(strand)
        }
        ErrorKind::ZeroDiv => {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            Error::zero_div(strand)
        }
        ErrorKind::SinkStop => {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            Error::sink_stop(strand)
        }
        ErrorKind::IterStop => {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            Error::iter_stop(strand)
        }
        ErrorKind::CyclicImport => {
            let ([name], []) = unpack!(strand, args, 1, 0)?;
            let name = expect_string(strand, &name)?;
            Error::cyclic_import(strand, name.as_ref())
        }
        ErrorKind::Import => {
            let ([name], []) = unpack!(strand, args, 1, 0)?;
            let name = expect_string(strand, &name)?;
            Error::import(strand, name.as_ref())
        }
        ErrorKind::Compile => {
            let ([msg], []) = unpack!(strand, args, 1, 0)?;
            let msg = expect_string(strand, &msg)?;
            Error::compile(strand, msg)
        }
        ErrorKind::Bytecode | ErrorKind::Abort => {
            return Err(Error::type_error(
                strand,
                format!("{} is not instantiable", variant_name(kind)),
            ));
        }
        ErrorKind::Runtime => {
            let ([msg], []) = unpack!(strand, args, 1, 0)?;
            let msg = expect_string(strand, &msg)?;
            Error::runtime(strand, msg)
        }
        ErrorKind::Canceled => {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            Error::canceled(strand)
        }
        ErrorKind::TimedOut => {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            Error::timed_out(strand)
        }
    })
}

/// Whether a variant type has a constructor, and so can be subclassed.
///
/// `Abort` and `Bytecode` are sealed: `Abort` carries native content nothing in
/// Do can supply, and letting either be subclassed would let a script mint
/// values that [`Error::catchable`] reports as uncatchable.
fn is_constructible(kind: ErrorKind) -> bool {
    !matches!(kind, ErrorKind::Bytecode | ErrorKind::Abort)
}

fn variant_singleton<'v>(strand: &Strand<'v, '_>, kind: ErrorKind) -> &'v Value<'v> {
    let class = strand.singletons();
    match kind {
        ErrorKind::Unsupported => &class.error_unsupported,
        ErrorKind::Immutable => &class.error_immutable,
        ErrorKind::Concurrency => &class.error_concurrency,
        ErrorKind::Type => &class.error_type,
        ErrorKind::Value => &class.error_value,
        ErrorKind::State => &class.error_state,
        ErrorKind::Index => &class.error_index,
        ErrorKind::Field => &class.error_field,
        ErrorKind::UnexpectedPos => &class.error_unexpected_pos,
        ErrorKind::UnexpectedKey => &class.error_unexpected_key,
        ErrorKind::MissingPos => &class.error_missing_pos,
        ErrorKind::MissingKey => &class.error_missing_key,
        ErrorKind::Overflow => &class.error_overflow,
        ErrorKind::ZeroDiv => &class.error_zerodiv,
        ErrorKind::SinkStop => &class.error_sink_stop,
        ErrorKind::IterStop => &class.error_iter_stop,
        ErrorKind::CyclicImport => &class.error_cyclic_import,
        ErrorKind::Import => &class.error_import,
        ErrorKind::Compile => &class.error_compile,
        ErrorKind::Bytecode => &class.error_bytecode,
        ErrorKind::Runtime => &class.error_runtime,
        ErrorKind::Abort => &class.error_abort,
        ErrorKind::Canceled => &class.error_canceled,
        ErrorKind::TimedOut => &class.error_timed_out,
    }
}

fn variant_name(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::Unsupported => "Unsupported",
        ErrorKind::Immutable => "Immutable",
        ErrorKind::Concurrency => "Concurrency",
        ErrorKind::Type => "Type",
        ErrorKind::Value => "Value",
        ErrorKind::State => "State",
        ErrorKind::Index => "Index",
        ErrorKind::Field => "Field",
        ErrorKind::UnexpectedPos => "UnexpectedPos",
        ErrorKind::UnexpectedKey => "UnexpectedKey",
        ErrorKind::MissingPos => "MissingPos",
        ErrorKind::MissingKey => "MissingKey",
        ErrorKind::Overflow => "Overflow",
        ErrorKind::ZeroDiv => "ZeroDiv",
        ErrorKind::SinkStop => "SinkStop",
        ErrorKind::IterStop => "IterStop",
        ErrorKind::CyclicImport => "CyclicImport",
        ErrorKind::Import => "Import",
        ErrorKind::Compile => "Compile",
        ErrorKind::Bytecode => "Bytecode",
        ErrorKind::Runtime => "Runtime",
        ErrorKind::Abort => "Abort",
        ErrorKind::Canceled => "Canceled",
        ErrorKind::TimedOut => "TimedOut",
    }
}

fn is_runtime_superkind(kind: ErrorKind) -> bool {
    matches!(
        kind,
        ErrorKind::Unsupported
            | ErrorKind::Immutable
            | ErrorKind::Concurrency
            | ErrorKind::Type
            | ErrorKind::Value
            | ErrorKind::State
            | ErrorKind::Index
            | ErrorKind::Field
            | ErrorKind::UnexpectedPos
            | ErrorKind::UnexpectedKey
            | ErrorKind::MissingPos
            | ErrorKind::MissingKey
            | ErrorKind::Overflow
            | ErrorKind::ZeroDiv
            | ErrorKind::CyclicImport
            | ErrorKind::Import
            | ErrorKind::Compile
            | ErrorKind::Bytecode
            | ErrorKind::Runtime
            | ErrorKind::TimedOut
    )
}

fn expect_index<'v, 's>(strand: &mut Strand<'v, 's>, value: &Value<'v>) -> Result<'v, 's, usize> {
    let index = value
        .to_i64(strand)
        .map_err(|_| Error::type_error(strand, "expected `Int`"))?;
    usize::try_from(index).map_err(|_| Error::value(strand, "expected non-negative value"))
}

fn expect_string<'v, 's>(strand: &mut Strand<'v, 's>, value: &Value<'v>) -> Result<'v, 's, String> {
    if let Some(str) = value.as_str_raw(strand) {
        Ok(str.to_owned())
    } else {
        Err(Error::type_error(strand, "expected `Str`"))
    }
}
