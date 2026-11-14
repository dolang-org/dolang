//! The `inspect` module: source locations of running code

use std::hash::{Hash, Hasher};

use dolang_bytecode::span::Span;

use crate::{
    error::{Error, Result},
    object::{
        class::{ClassEntry, ClassTypeEntry},
        native::{Instance, Object, Type, TypeBuilder},
        protocol::MemberKind,
    },
    strand::Strand,
    unpack,
    value::{Output, Value, fmt::Format},
    vm::{AllocExt, Builder, StateTag, Vm},
};

/// Bound on the wrappers [`definition`] unwraps, as a class's method table can hold any value
const MAX_UNWRAP: usize = 16;

struct InspectState<'v> {
    location: Type<'v, Location>,
}

struct InspectStateTag;

impl StateTag for InspectStateTag {
    type State<'v> = InspectState<'v>;
}

/// A source file and a span within it
struct Location;

struct LocationAnnex {
    source: Box<str>,
    span: Span,
}

impl<'v> Object<'v> for Location {
    const NAME: &'v str = "Location";
    const MODULE: &'v str = "inspect";
    const FINAL: bool = true;
    type Annex = LocationAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("source", |this, strand, out| {
                Output::set(strand, out, &*this.annex().source);
                Ok(())
            })
            .get("line", |this, strand, out| {
                Output::set(strand, out, this.annex().span.start().line_offset());
                Ok(())
            })
            .get("column", |this, strand, out| {
                Output::set(strand, out, this.annex().span.start().column_offset());
                Ok(())
            })
            .get("len", |this, strand, out| {
                Output::set(strand, out, this.annex().span.len());
                Ok(())
            })
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let LocationAnnex { source, span } = &*this.annex();
        let start = span.start();
        crate::fmt!(
            strand,
            w,
            "<inspect.Location {source}:{}:{}+{}>",
            start.line_number(),
            start.column_number(),
            span.len()
        )
    }

    fn eq<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        let Some(other) = strand.state::<InspectStateTag>().location.cast(other) else {
            return Err(Error::unsupported(strand, "`==`"));
        };
        Ok(other.enter_sync(strand, |_strand, other| {
            let (this, other) = (this.annex(), other.annex());
            this.span == other.span && this.source == other.source
        }))
    }

    fn hash<'a, 's>(
        this: Instance<'v, 'a, Self>,
        _strand: &'a mut Strand<'v, 's>,
        hasher: &mut impl Hasher,
    ) -> Result<'v, 's, ()> {
        let LocationAnnex { source, span } = &*this.annex();
        source.hash(hasher);
        span.hash(hasher);
        Ok(())
    }
}

/// Create an `inspect.Location`.
pub(crate) fn location<'v>(
    strand: &mut Strand<'v, '_>,
    source: Box<str>,
    span: Span,
    out: impl Output<'v>,
) {
    strand
        .force_state::<InspectStateTag>()
        .location
        .create_with_annex(strand, Location, LocationAnnex { source, span }, out);
}

/// Where the Do function behind `value` is defined, unwrapping bound methods.
///
/// Reads only class tables and debug info, so it runs no user code.
fn definition<'v>(vm: &Vm<'v>, value: &Value<'v>) -> Option<(Box<str>, Span)> {
    let types = vm.builtin_types();
    let mut current = value.dup();
    for _ in 0..MAX_UNWRAP {
        if let Some(function) = current.downcast_ref(types.function) {
            return function.read(|function| {
                let function = function.get();
                let program = &function.module;
                let debug = program.funcdebugs().get(function.id)?;
                let source = &program.debug_strtab()[debug.file.clone()];
                Some((source.into(), debug.definition))
            });
        }
        let bound = current.downcast_ref(types.bound_method)?;
        // SAFETY: a transient pin; with only a `Vm`, nothing here can collect
        let bound = unsafe { bound.pin_unchecked() };
        let bound = bound.get();
        let receiver = bound.receiver();
        let method = bound.method();
        let next = if let Some(instance) = receiver.downcast_ref(types.class_instance) {
            match instance.class().entry(method)? {
                ClassEntry::Method(value) => value.dup(),
                _ => return None,
            }
        } else {
            // As `ClassObject::op_mcall` dispatches: an inherited native method
            // shadows the type namespace.
            let class = receiver.downcast_ref(types.class_object)?;
            let class = class.annex();
            if let Some(
                ClassEntry::Abstract(_, MemberKind::Method)
                | ClassEntry::Delegate(_, MemberKind::Method),
            ) = class.entry(method)
            {
                return None;
            }
            match class.type_entry(method)? {
                ClassTypeEntry::Method { value, .. } => value.dup(),
                _ => return None,
            }
        };
        current = next;
    }
    None
}

pub(crate) fn configure<'v>(builder: &mut Builder<'v>) {
    builder.lazy::<InspectStateTag>(&["inspect"], |reg| {
        let location_type = reg.register_type();
        let state = reg.register_state::<InspectStateTag>(InspectState {
            location: location_type,
        });
        let backtrace = reg.singletons().backtrace.dup();
        let frame = reg.singletons().backtrace_frame.dup();

        reg.module("inspect")
            .value("Backtrace", &backtrace)
            .value("Frame", &frame)
            .value("Location", state.location)
            .function("definition", async move |strand, args, mut out| {
                let ([func], []) = unpack!(strand, args, 1, 0)?;
                match definition(strand.vm(), &func) {
                    Some((source, span)) => location(strand, source, span, out),
                    None => out.store(Value::NIL),
                }
                Ok(())
            })
            .commit();
    });
}
