use std::hash::{Hash, Hasher};

use crate::{
    arg::{Arg, Args},
    error::{Error, Result},
    object::{
        array_view::{ArrayLike, ArrayView},
        dict::Dict,
        kv,
        native::{Instance, Mut, Object, Ref, Type, TypeBuilder, Unpack},
        protocol::{Spread, SpreadContext},
    },
    strand::Strand,
    sym::Sym,
    unpack,
    value::{
        Empty, Input, Output, Slot, StrEmbryo, Value,
        fmt::{Align, Fill, Format, Kind, Pad, Sign, Spec},
    },
    vm::{Builder, State, Stateful, Vm},
};

/// Every symbol this module needs: the keyword parameter names, and the symbol
/// naming each enumerated option value.
struct Symbols<'v> {
    fill: Sym<'v, 'v>,
    align: Sym<'v, 'v>,
    sign: Sym<'v, 'v>,
    width: Sym<'v, 'v>,
    precision: Sym<'v, 'v>,
    alt: Sym<'v, 'v>,
    kind: Sym<'v, 'v>,
    source: Sym<'v, 'v>,
    zero: Sym<'v, 'v>,
    left: Sym<'v, 'v>,
    right: Sym<'v, 'v>,
    center: Sym<'v, 'v>,
    plus: Sym<'v, 'v>,
    space: Sym<'v, 'v>,
    str: Sym<'v, 'v>,
    dbg: Sym<'v, 'v>,
    verbatim: Sym<'v, 'v>,
    hex: Sym<'v, 'v>,
    oct: Sym<'v, 'v>,
    bin: Sym<'v, 'v>,
    dec: Sym<'v, 'v>,
    exp: Sym<'v, 'v>,
    fixed: Sym<'v, 'v>,
}

impl<'v> Symbols<'v> {
    fn new(builder: &mut Builder<'v>) -> Self {
        Self {
            fill: builder.sym("fill"),
            align: builder.sym("align"),
            sign: builder.sym("sign"),
            width: builder.sym("width"),
            precision: builder.sym("precision"),
            alt: builder.sym("alt"),
            kind: builder.sym("kind"),
            source: builder.sym("source"),
            zero: builder.sym("ZERO"),
            left: builder.sym("LEFT"),
            right: builder.sym("RIGHT"),
            center: builder.sym("CENTER"),
            plus: builder.sym("PLUS"),
            space: builder.sym("SPACE"),
            str: builder.sym(Kind::Str.symbol()),
            dbg: builder.sym(Kind::Dbg.symbol()),
            verbatim: builder.sym(Kind::Verbatim.symbol()),
            hex: builder.sym(Kind::Hex.symbol()),
            oct: builder.sym(Kind::Oct.symbol()),
            bin: builder.sym(Kind::Bin.symbol()),
            dec: builder.sym(Kind::Dec.symbol()),
            exp: builder.sym(Kind::Exp.symbol()),
            fixed: builder.sym(Kind::Fixed.symbol()),
        }
    }

    fn align_symbol(&self, align: Align) -> Sym<'v, 'v> {
        match align {
            Align::Left => self.left,
            Align::Right => self.right,
            Align::Center => self.center,
        }
    }

    fn sign_symbol(&self, sign: Sign) -> Sym<'v, 'v> {
        match sign {
            Sign::Plus => self.plus,
            Sign::Space => self.space,
        }
    }

    fn kind_symbol(&self, kind: Kind) -> Sym<'v, 'v> {
        match kind {
            Kind::Str => self.str,
            Kind::Dbg => self.dbg,
            Kind::Verbatim => self.verbatim,
            Kind::Hex => self.hex,
            Kind::Oct => self.oct,
            Kind::Bin => self.bin,
            Kind::Dec => self.dec,
            Kind::Exp => self.exp,
            Kind::Fixed => self.fixed,
        }
    }
}

/// A symbol-keyed enumeration of option values, sorted for binary search.
struct Table<'v, T, const N: usize>([(Sym<'v, 'v>, T); N]);

impl<'v, T: Copy, const N: usize> Table<'v, T, N> {
    fn new(mut entries: [(Sym<'v, 'v>, T); N]) -> Self {
        entries.sort_unstable_by_key(|(symbol, _)| *symbol);
        Self(entries)
    }

    /// Resolves a symbol to its value, or reports `name` as invalid.
    fn value<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        name: &str,
        symbol: Sym<'v, 'v>,
    ) -> Result<'v, 's, T> {
        match self.0.binary_search_by_key(&symbol, |(symbol, _)| *symbol) {
            Ok(index) => Ok(self.0[index].1),
            Err(_) => Err(Error::value(strand, format!("{name}: invalid symbol"))),
        }
    }
}

pub(crate) struct Types<'v> {
    pub(crate) spec: Type<'v, FmtSpec>,
    pub(crate) value: Type<'v, FmtValue>,
    pub(crate) param: Type<'v, FmtParam>,
    pub(crate) fmt: Type<'v, Fmt>,
}

pub(crate) struct Global<'v> {
    pub(crate) types: Types<'v>,
    symbols: Symbols<'v>,
    aligns: Table<'v, Align, 3>,
    signs: Table<'v, Sign, 2>,
    kinds: Table<'v, Kind, 9>,
}

impl<'v> Global<'v> {
    fn new(builder: &mut Builder<'v>, types: Types<'v>) -> Self {
        let symbols = Symbols::new(builder);
        let aligns = Table::new(
            [Align::Left, Align::Right, Align::Center]
                .map(|align| (symbols.align_symbol(align), align)),
        );
        let signs =
            Table::new([Sign::Plus, Sign::Space].map(|sign| (symbols.sign_symbol(sign), sign)));
        let kinds = Table::new(
            [
                Kind::Str,
                Kind::Dbg,
                Kind::Verbatim,
                Kind::Hex,
                Kind::Oct,
                Kind::Bin,
                Kind::Dec,
                Kind::Exp,
                Kind::Fixed,
            ]
            .map(|kind| (symbols.kind_symbol(kind), kind)),
        );
        Self {
            types,
            symbols,
            aligns,
            signs,
            kinds,
        }
    }
}

pub(crate) struct Tag;

impl<'v> Stateful<'v> for Global<'v> {
    type Tag = Tag;
}

/// Returns the [`FmtValue`] type object singleton, for
/// [`TypeObject::FmtValue`](crate::value::TypeObject).
pub(crate) fn fmt_value_singleton<'v, 'a>(vm: &'a Vm<'v>) -> &'a Value<'v> {
    vm.state::<Global<'v>>().types.value.singleton(vm)
}

/// Returns the [`FmtParam`] type object singleton, for
/// [`TypeObject::FmtParam`](crate::value::TypeObject).
pub(crate) fn fmt_param_singleton<'v, 'a>(vm: &'a Vm<'v>) -> &'a Value<'v> {
    vm.state::<Global<'v>>().types.param.singleton(vm)
}

/// Returns the [`Fmt`] type object singleton, for
/// [`TypeObject::Fmt`](crate::value::TypeObject).
pub(crate) fn fmt_singleton<'v, 'a>(vm: &'a Vm<'v>) -> &'a Value<'v> {
    vm.state::<Global<'v>>().types.fmt.singleton(vm)
}

/// Immutable per-instance data shared by [`FmtSpec`] and [`FmtValue`].
///
/// Carrying the specification alongside the global keeps both types unit
/// structs with nothing to borrow-check at runtime.
pub(crate) struct SpecAnnex<'v> {
    global: State<'v, Global<'v>>,
    spec: Spec,
}

pub(crate) struct FmtSpec;

pub(crate) struct FmtValue;

/// An unbound position in a [`Fmt`]: a named hole whose meaning is up to the
/// consumer of the sequence.
///
/// Named by an `Int` or a `Sym`. An explicit position is never renumbered, so a
/// number is a name that happens to be an integer.
pub(crate) struct FmtParam;

/// One `t"..."` sequence: literal text, bound interpolations, and unbound
/// parameters, in order.
///
/// Every element of the segment array in slot 0 is a `Str` of literal text, a
/// [`FmtValue`], or a [`FmtParam`]; [`create_fmt`] admits nothing else.
pub(crate) struct Fmt;

/// Immutable per-instance data for [`Fmt`].
pub(crate) struct FmtAnnex<'v> {
    global: State<'v, Global<'v>>,
}

impl<'v> Object<'v> for FmtSpec {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "FmtSpec";

    type Annex = SpecAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    /// Constructs a specification from keyword options alone. Binding a value
    /// is [`FmtValue`]'s job, so no positional is accepted here.
    async fn new<'a, 's>(
        _this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = strand.vm().state::<Global<'v>>();
        let spec = merge_spec(strand, global, Spec::default(), args)?;
        create_spec(strand, global, spec, out);
        Ok(())
    }

    async fn call<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        create(strand, annex.global, annex.spec, args, out).await
    }
}

impl<'v> Object<'v> for FmtValue {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "FmtValue";
    /// Slot 0 is the bound value; slot 1 the source text, or nil.
    const SLOTS: usize = 2;

    type Annex = SpecAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    /// Binds a value to keyword options. `source:` is accepted so a caller
    /// that has the originating text — the compiler, above all — can record it.
    async fn new<'a, 's>(
        _this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = strand.vm().state::<Global<'v>>();
        let source_sym = global.symbols.source;
        let ([value], [source], rest) = unpack!(strand, args, 1, 0, source_sym = None, ...)?;
        let spec = merge_spec(strand, global, Spec::default(), rest)?;
        create_value(strand, global, spec, value, source.as_deref(), out);
        Ok(())
    }

    /// Re-merges options over the bound value. The result is synthetic rather
    /// than source-derived, so it carries no `source`.
    async fn call<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        let global = annex.global;
        let spec = merge_spec(strand, global, annex.spec, args)?;
        let borrow = this.borrow(strand)?;
        create_value(strand, global, spec, Ref::slot::<0>(&borrow), None, out);
        Ok(())
    }

    fn verbatim<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        format_bound(this, strand, Kind::Verbatim, out)
    }

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        format_bound(this, strand, Kind::Str, out)
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        format_bound(this, strand, Kind::Dbg, out)
    }

    /// Two bindings are equal when they bind equal values to the same
    /// specification and record the same `source`.
    ///
    /// `source` is readable, and `dbg` renders through it, so two bindings that
    /// differ in it are already distinguishable — leaving it out would make
    /// equality disagree with what can be observed.
    fn eq<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        let Some(other) = this.annex().global.types.value.cast(other) else {
            return Ok(false);
        };
        if other.enter_sync(strand, |_, other| other.annex().spec) != this.annex().spec {
            return Ok(false);
        }
        let this_borrow = this.borrow(strand)?;
        other.enter_sync(strand, |strand, other| {
            let other_borrow = other.borrow(strand)?;
            if !Ref::slot::<1>(&this_borrow)
                .op_eq(strand, Ref::slot::<1>(&other_borrow))
                .to_bool(strand)
            {
                return Ok(false);
            }
            Ok(Ref::slot::<0>(&this_borrow)
                .op_eq(strand, Ref::slot::<0>(&other_borrow))
                .to_bool(strand))
        })
    }

    fn hash<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut impl Hasher,
    ) -> Result<'v, 's, ()> {
        this.annex().spec.hash(hasher);
        let borrow = this.borrow(strand)?;
        hasher.write_u64(kv::hash(strand, Ref::slot::<0>(&borrow))?);
        hasher.write_u64(kv::hash(strand, Ref::slot::<1>(&borrow))?);
        Ok(())
    }
}

impl<'v> Object<'v> for FmtParam {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "FmtParam";
    /// Slot 0 is the name; slot 1 the source text, or nil.
    const SLOTS: usize = 2;

    type Annex = SpecAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    /// Names a hole and the options it will impose once filled. `source:` is
    /// accepted for the same reason [`FmtValue`] accepts it.
    async fn new<'a, 's>(
        _this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = strand.vm().state::<Global<'v>>();
        create_fmt_param(strand, global, args, out)
    }

    /// Re-merges options over the named hole, as [`FmtValue::call`] does over a
    /// bound value. The result is synthetic, so it carries no `source`.
    async fn call<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        let global = annex.global;
        let spec = merge_spec(strand, global, annex.spec, args)?;
        let borrow = this.borrow(strand)?;
        let name = Ref::slot::<0>(&borrow).dup();
        create_param(strand, global, spec, &name, None, out)
    }

    /// A hole shows itself as the text it was written as.
    ///
    /// Unlike [`Fmt`], a parameter does not refuse conversion. The reason a
    /// sequence refuses is that flattening it yields text with its
    /// interpolations already substituted and nothing left to tell them from
    /// the literal skeleton; a parameter carries no bound value at all, so
    /// there is nothing to leak by showing it.
    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        write_param(this, strand, out)
    }

    fn verbatim<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        write_param(this, strand, out)
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        write_param(this, strand, out)
    }

    /// Two holes are equal when they name the same parameter under the same
    /// specification and were written the same way.
    fn eq<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        let Some(other) = this.annex().global.types.param.cast(other) else {
            return Ok(false);
        };
        if other.enter_sync(strand, |_, other| other.annex().spec) != this.annex().spec {
            return Ok(false);
        }
        let this_borrow = this.borrow(strand)?;
        other.enter_sync(strand, |strand, other| {
            let other_borrow = other.borrow(strand)?;
            if !Ref::slot::<1>(&this_borrow)
                .op_eq(strand, Ref::slot::<1>(&other_borrow))
                .to_bool(strand)
            {
                return Ok(false);
            }
            Ok(Ref::slot::<0>(&this_borrow)
                .op_eq(strand, Ref::slot::<0>(&other_borrow))
                .to_bool(strand))
        })
    }

    fn hash<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut impl Hasher,
    ) -> Result<'v, 's, ()> {
        this.annex().spec.hash(hasher);
        let borrow = this.borrow(strand)?;
        hasher.write_u64(kv::hash(strand, Ref::slot::<0>(&borrow))?);
        hasher.write_u64(kv::hash(strand, Ref::slot::<1>(&borrow))?);
        Ok(())
    }
}

/// Writes a parameter as it was written, or — when it was built at runtime and
/// so has no source — in the form it would have been.
fn write_param<'v, 's>(
    this: Instance<'v, '_, FmtParam>,
    strand: &mut Strand<'v, 's>,
    out: &mut dyn Format<'v>,
) -> Result<'v, 's, ()> {
    let borrow = this.borrow(strand)?;
    if let Some(source) = Ref::slot::<1>(&borrow).as_str_raw(strand) {
        let source = source.to_string();
        return out.write_str(strand, &source);
    }
    let name = Ref::slot::<0>(&borrow).dup();
    let name = param_name(strand, &name);
    out.write_str(strand, &format!("${{#{name}}}"))
}

/// Renders a parameter's name for a message or a synthesized source form.
fn param_name<'v>(strand: &mut Strand<'v, '_>, name: &Value<'v>) -> String {
    if let Some(sym) = name.as_sym(strand.vm()) {
        sym.as_str(strand.vm()).to_string()
    } else if let Some(int) = name.as_int(strand) {
        int.to_string()
    } else {
        "?".to_string()
    }
}

/// The segments of a [`Fmt`], viewed as a read-only array.
///
/// Only `len` and `get` are given: every mutator inherits the trait's
/// `ImmutableError`, which is the whole point of the view.
struct Segments;

impl<'v> ArrayLike<'v> for Segments {
    type Object = Fmt;

    const MODULE: &'v str = "std";
    const NAME: &'v str = "Segments";

    fn len(&self, this: Instance<'v, '_, Fmt>, strand: &mut Strand<'v, '_>) -> usize {
        Ref::slot::<0>(&this.borrow_unwrap())
            .as_array(strand)
            .unwrap()
            .len(strand)
            .expect("conflicting Fmt segment borrow")
    }

    fn get<'a, 's>(
        &self,
        this: Instance<'v, '_, Fmt>,
        strand: &'a mut Strand<'v, 's>,
        index: usize,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let found = Ref::slot::<0>(&this.borrow(strand)?)
            .as_array(strand)
            .unwrap()
            .get(strand, index, out)?;
        debug_assert!(found);
        Ok(())
    }
}

impl<'v> Object<'v> for Fmt {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Fmt";
    /// Slot 0 is the segment array.
    const SLOTS: usize = 1;

    type Annex = FmtAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    /// Builds a sequence from an iterable of segments.
    ///
    /// # Trust
    ///
    /// A sequence written as `t"..."` carries a guarantee its consumers rely
    /// on: the literal segments came from the program text and everything
    /// interpolated is a [`FmtValue`]. This constructor cannot make that
    /// guarantee — it accepts whatever segments it is given — so a `Str`
    /// built from untrusted input becomes literal text, indistinguishable
    /// from text the programmer wrote. Bind such a value as a [`FmtValue`]
    /// instead of splicing it in as a `Str`.
    async fn new<'a, 's>(
        _this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = strand.vm().state::<Global<'v>>();
        let ([items], []) = unpack!(strand, args, 1, 0)?;
        strand
            .with_slots(async move |strand, [mut segments, mut iter, mut item]| {
                Output::set(strand, &mut segments, Empty::Array);
                items.iter(strand, &mut iter).await?;
                while iter.next(strand, &mut item).await? {
                    append_segment(strand, global, &segments, &item)?;
                }
                create_from_segments(strand, global, &segments, out);
                Ok(())
            })
            .await
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("len", |this, strand, out| {
                let len = Segments.len(this, strand);
                Output::set(strand, out, len);
                Ok(())
            })
            // The way to ask for the expansion. A sequence has no implicit
            // string conversion, so this is the explicit one.
            //
            // Arguments fill the parameters first. This is a reference
            // implementation of holes; otherwise what a hole means is up to the
            // consumer of the sequence.
            .method("format", async |this, strand, args, out| {
                let global = this.annex().global;
                if args.len() == 0 {
                    let mut embryo = StrEmbryo::new();
                    render(this, strand, &mut embryo)?;
                    embryo.finish(strand, out);
                    return Ok(());
                }
                strand
                    .with_slots(
                        async move |strand, [mut bindings, mut consumed, mut filled]| {
                            let dict = Dict::from_args(strand, args)?;
                            strand
                                .builtin_types()
                                .dict
                                .create(strand, dict, &mut bindings);
                            Output::set(strand, &mut consumed, Empty::Set);
                            substitute(strand, global, this, &bindings, &consumed, &mut filled)?;
                            reject_unused(strand, &bindings, &consumed)?;
                            // A hole left unfilled raises when rendering reaches it.
                            let filled = global.types.fmt.cast(&filled).unwrap();
                            filled.enter_sync(strand, |strand, filled| {
                                let mut embryo = StrEmbryo::new();
                                render(filled, strand, &mut embryo)?;
                                embryo.finish(strand, out);
                                Ok(())
                            })
                        },
                    )
                    .await
            })
    }

    /// A sequence has no implicit string conversion.
    ///
    /// The point of keeping the segments apart is that a consumer can tell
    /// literal text from interpolated values and quote, bind, or style the
    /// latter. Expanding at a bare `str` or `"$x"` would hand that consumer a
    /// flat string with the distinction already lost — the shape of an
    /// injection bug — so expansion has to be asked for by name, with
    /// `format`.
    fn display<'a, 's>(
        _this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(
            strand,
            "Fmt: no implicit string conversion; use .format()",
        ))
    }

    /// The verbatim conversion gives the source form, exactly as `dbg` does.
    ///
    /// No conversion expands a sequence. Expansion is what
    /// [`format`](Self::build) is for, and it has to be asked for by name.
    fn verbatim<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        Self::debug(this, strand, w)
    }

    /// Writes the sequence back in the form it was written: literal text as
    /// it stands, each interpolation by its recorded source.
    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let _depth = strand.inner.push_call_depth()?;
        let global = this.annex().global;
        w.write_str(strand, "t\"")?;
        each_segment(this, strand, |strand, segment| {
            debug_segment(strand, global, segment, w)
        })?;
        w.write_str(strand, "\"")
    }

    fn index<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        ArrayView::index(this, Segments, strand, index, out)
    }

    /// A sequence is fixed once built, so a segment cannot be replaced.
    fn assign<'a, 's>(
        _this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _index: Slot<'v, 'a>,
        _value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        Err(Error::immutable(strand))
    }

    async fn iter<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        ArrayView::iter(this, Segments, strand, out)
    }

    async fn spread<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        ArrayView::spread(this, Segments, strand, context, sink)
    }

    async fn unpack<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        ArrayView::unpack(this, Segments, strand, unpack)
    }

    /// Two sequences are equal when their segments are, so how a sequence was
    /// assembled — one `t"..."` or several spliced together — does not show.
    fn eq<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        let Some(other) = this.annex().global.types.fmt.cast(other) else {
            return Ok(false);
        };
        other.enter_sync(strand, |strand, other| eq_segments(this, other, strand))
    }

    fn hash<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut impl Hasher,
    ) -> Result<'v, 's, ()> {
        hasher.write_usize(Segments.len(this, strand));
        each_segment(this, strand, |strand, segment| {
            hasher.write_u64(kv::hash(strand, segment)?);
            Ok(())
        })
    }
}

/// Expands the sequence: literal text as it stands, and each interpolation
/// through its own specification.
fn render<'v, 's>(
    this: Instance<'v, '_, Fmt>,
    strand: &mut Strand<'v, 's>,
    w: &mut dyn Format<'v>,
) -> Result<'v, 's, ()> {
    // A segment may bind a sequence of its own, so expansion recurses. Bound
    // by the ordinary call depth rather than a mechanism of its own.
    let _depth = strand.inner.push_call_depth()?;
    let global = this.annex().global;
    each_segment(this, strand, |strand, segment| {
        render_segment(strand, global, segment, w)
    })
}

/// Expands one segment.
fn render_segment<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    segment: &Value<'v>,
    w: &mut dyn Format<'v>,
) -> Result<'v, 's, ()> {
    if let Some(text) = segment.as_str_raw(strand) {
        let text = text.to_string();
        return w.write_str(strand, &text);
    }
    if let Some(param) = global.types.param.cast(segment) {
        // An unfilled hole has no rendering. The template is unfinished, so
        // there is nothing to emit that would not be a guess at what belongs
        // there. That is a parameter the caller never supplied, and it is
        // reported as one, exactly as filling reports it.
        let name = param.enter_sync(strand, |strand, param| {
            let borrow = param.borrow(strand)?;
            Ok(Ref::slot::<0>(&borrow).dup())
        })?;
        return Err(unmatched_error(strand, &name, true));
    }
    let bound = global
        .types
        .value
        .cast(segment)
        .expect("a segment is a Str, a FmtValue, or a FmtParam");
    bound.enter_sync(strand, |strand, bound| {
        let mut spec = bound.annex().spec;
        let kind = *spec.kind.get_or_insert(Kind::Str);
        let borrow = bound.borrow(strand)?;
        let value = Ref::slot::<0>(&borrow);
        if kind == Kind::Str
            && let Some(nested) = global.types.fmt.cast(value)
        {
            // A sequence bound inside a sequence expands too, and the binding
            // lays out what it expanded to. Asking for another kind — the
            // source form, say — is the bound value's business as usual.
            let mut buffer = String::new();
            nested.enter_sync(strand, |strand, nested| render(nested, strand, &mut buffer))?;
            let mut pad = Pad::new(spec, w);
            pad.write_str(strand, &buffer)?;
            return pad.finish(strand);
        }
        value.fmt(strand, &spec, w)
    })
}

/// Writes one segment as it was written: literal text as it stands, an
/// interpolation by its recorded source, or — when it was built at runtime and
/// so has none — the bound value's debug form.
fn debug_segment<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    segment: &Value<'v>,
    w: &mut dyn Format<'v>,
) -> Result<'v, 's, ()> {
    if let Some(text) = segment.as_str_raw(strand) {
        let text = text.to_string();
        return w.write_str(strand, &text);
    }
    if let Some(cast) = global.types.value.cast(segment) {
        let source = cast.enter_sync(strand, |strand, this| {
            let borrow = this.borrow(strand)?;
            Ok(Ref::slot::<1>(&borrow)
                .as_str_raw(strand)
                .map(str::to_string))
        })?;
        if let Some(source) = source {
            return w.write_str(strand, &source);
        }
    }
    segment.debug(strand, w)
}

/// Visits each segment in order, one rooted slot at a time.
fn each_segment<'v, 's>(
    this: Instance<'v, '_, Fmt>,
    strand: &mut Strand<'v, 's>,
    mut visit: impl FnMut(&mut Strand<'v, 's>, &Value<'v>) -> Result<'v, 's, ()>,
) -> Result<'v, 's, ()> {
    let borrow = this.borrow(strand)?;
    let segments = Ref::slot::<0>(&borrow).as_array(strand).unwrap();
    strand.with_slots_sync(|strand, [mut item]| {
        for index in 0..segments.len(strand)? {
            segments.get(strand, index, &mut item)?;
            visit(strand, &item)?;
        }
        Ok(())
    })
}

fn eq_segments<'v, 's>(
    this: Instance<'v, '_, Fmt>,
    other: Instance<'v, '_, Fmt>,
    strand: &mut Strand<'v, 's>,
) -> Result<'v, 's, bool> {
    let this_borrow = this.borrow(strand)?;
    let other_borrow = other.borrow(strand)?;
    let this_segments = Ref::slot::<0>(&this_borrow).as_array(strand).unwrap();
    let other_segments = Ref::slot::<0>(&other_borrow).as_array(strand).unwrap();
    let len = this_segments.len(strand)?;
    if len != other_segments.len(strand)? {
        return Ok(false);
    }
    strand.with_slots_sync(|strand, [mut left, mut right]| {
        for index in 0..len {
            this_segments.get(strand, index, &mut left)?;
            other_segments.get(strand, index, &mut right)?;
            if !left.op_eq(strand, &right).to_bool(strand) {
                return Ok(false);
            }
        }
        Ok(true)
    })
}

/// Fills the parameters `bindings` names, and leaves the rest as they are.
///
/// The routine decides nothing. It substitutes what it can, copies through what
/// it cannot, and adds each name it consumed to `consumed`; `format` rejects
/// what is left over. Substitution is functional, so a pass abandoned partway
/// leaves the original untouched.
fn substitute<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    this: Instance<'v, '_, Fmt>,
    bindings: &Value<'v>,
    consumed: &Value<'v>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    // A bound value may carry a sequence of its own, so substitution recurses.
    // Bound by the ordinary call depth rather than a mechanism of its own.
    strand.recursion_guard_sync(|strand| {
        strand.with_slots_sync(|strand, [mut segments, mut built]| {
            Output::set(strand, &mut segments, Empty::Array);
            each_segment(this, strand, |strand, segment| {
                substitute_segment(strand, global, bindings, consumed, &segments, segment)
            })?;
            create_from_segments(strand, global, &segments, Slot::reborrow(&mut built));
            Output::set(strand, out, &built);
            Ok(())
        })
    })
}

/// Substitutes one segment, appending the result to the array under construction.
fn substitute_segment<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    bindings: &Value<'v>,
    consumed: &Value<'v>,
    segments: &Value<'v>,
    segment: &Value<'v>,
) -> Result<'v, 's, ()> {
    if let Some(param) = global.types.param.cast(segment) {
        return strand.with_slots_sync(|strand, [mut name, mut source, mut value, mut filled]| {
            let spec = param.enter_sync(strand, |strand, param| {
                let borrow = param.borrow(strand)?;
                Output::set(strand, &mut name, Ref::slot::<0>(&borrow));
                Output::set(strand, &mut source, Ref::slot::<1>(&borrow));
                Ok(param.annex().spec)
            })?;
            let found = bindings
                .as_dict(strand)
                .unwrap()
                .get(strand, &name, None, &mut value)?;
            if !found {
                return append_segment(strand, global, segments, segment);
            }
            consumed.as_set(strand).unwrap().add(strand, &name)?;
            // The filled hole keeps the parameter's own specification and
            // source: how it is laid out, and how it was written, belong to the
            // template rather than to the value that arrived late.
            create_value(
                strand,
                global,
                spec,
                &value,
                Some(&source),
                Slot::reborrow(&mut filled),
            );
            append_segment(strand, global, segments, &filled)
        });
    }
    let Some(bound) = global.types.value.cast(segment) else {
        return append_segment(strand, global, segments, segment);
    };
    strand.with_slots_sync(
        |strand, [mut inner, mut source, mut rebuilt, mut wrapped]| {
            let spec = bound.enter_sync(strand, |strand, bound| {
                let borrow = bound.borrow(strand)?;
                Output::set(strand, &mut inner, Ref::slot::<0>(&borrow));
                Output::set(strand, &mut source, Ref::slot::<1>(&borrow));
                Ok(bound.annex().spec)
            })?;
            // A sequence bound inside a sequence has holes of its own, and they are
            // reached before anything past the binding — so the order holes are
            // visited is depth first, matching the order they read in.
            let Some(nested) = global.types.fmt.cast(&inner) else {
                return append_segment(strand, global, segments, segment);
            };
            nested.enter_sync(strand, |strand, nested| {
                substitute(strand, global, nested, bindings, consumed, &mut rebuilt)
            })?;
            create_value(
                strand,
                global,
                spec,
                &rebuilt,
                Some(&source),
                Slot::reborrow(&mut wrapped),
            );
            append_segment(strand, global, segments, &wrapped)
        },
    )
}

/// Reports the first binding no parameter consumed.
///
/// Naming the binding rather than the hole it failed to reach describes the
/// whole picture: a caller who misspelled a name wants to hear about the name
/// it wrote, not about a parameter it has never heard of.
fn reject_unused<'v, 's>(
    strand: &mut Strand<'v, 's>,
    bindings: &Value<'v>,
    consumed: &Value<'v>,
) -> Result<'v, 's, ()> {
    let mut pairs = bindings.as_dict(strand).unwrap().pairs();
    strand.with_slots_sync(|strand, [mut key, mut value]| {
        while pairs.next(strand, &mut key, &mut value)? {
            if !consumed.as_set(strand).unwrap().contains(strand, &key)? {
                return Err(unmatched_error(strand, &key, false));
            }
        }
        Ok(())
    })
}

/// Names an unmatched parameter or binding, positionally when its name is an
/// integer and by key otherwise.
fn unmatched_error<'v, 's>(
    strand: &mut Strand<'v, 's>,
    name: &Value<'v>,
    missing: bool,
) -> Error<'v, 's> {
    match name
        .as_int(strand)
        .and_then(|index| usize::try_from(index).ok())
    {
        Some(index) if missing => Error::missing_positional(strand, index),
        Some(index) => Error::unexpected_positional(strand, index),
        None if missing => Error::missing_key(strand, name),
        None => Error::unexpected_key(strand, name),
    }
}

/// Builds a sequence from the arguments the `fmt` builtin was handed.
///
/// This is the path a `t"..."` takes: the compiler emits one argument per
/// segment, with runs of literal text already folded into single constants,
/// so there is nothing here to normalize.
pub(crate) fn create_fmt<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    strand.with_slots_sync(|strand, [mut segments]| {
        Output::set(strand, &mut segments, Empty::Array);
        for arg in args {
            match arg {
                Arg::Pos(segment) => append_segment(strand, global, &segments, &segment)?,
                Arg::Key(key, _) => return Err(Error::unexpected_key(strand, key)),
            }
        }
        create_from_segments(strand, global, &segments, out);
        Ok(())
    })
}

/// Appends one segment to the array under construction.
///
/// A segment is literal text, a bound value, or an unbound parameter, and
/// nothing else. Which of the three it is is exactly what a consumer acts on,
/// so a value that is none of them is a mistake to report rather than
/// something to coerce into one.
fn append_segment<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    segments: &Value<'v>,
    segment: &Value<'v>,
) -> Result<'v, 's, ()> {
    if segment.as_str_raw(strand).is_none()
        && global.types.value.cast(segment).is_none()
        && global.types.param.cast(segment).is_none()
    {
        return Err(Error::type_error(
            strand,
            "Fmt: expected Str, FmtValue, or FmtParam",
        ));
    }
    segments.as_array(strand).unwrap().push(strand, segment)
}

fn create_from_segments<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    segments: &Value<'v>,
    mut out: Slot<'v, '_>,
) {
    let ty = global.types.fmt;
    ty.create_with_annex(strand, Fmt, FmtAnnex { global }, &mut out);
    ty.cast(&out).unwrap().enter_sync(strand, |strand, this| {
        let mut borrow = this.borrow_mut_unwrap();
        Output::set(strand, Mut::slot_mut::<0>(&mut borrow), segments);
    });
}

/// Formats the bound value, letting the surrounding conversion supply an unset kind.
fn format_bound<'v, 's>(
    this: Instance<'v, '_, FmtValue>,
    strand: &mut Strand<'v, 's>,
    kind: Kind,
    out: &mut dyn Format<'v>,
) -> Result<'v, 's, ()> {
    // A bound value may itself be bound, so rendering recurses. Bound by the
    // ordinary call depth rather than a mechanism of its own.
    let _depth = strand.inner.push_call_depth()?;
    let mut spec = this.annex().spec;
    spec.kind.get_or_insert(kind);
    let borrow = this.borrow(strand)?;
    Ref::slot::<0>(&borrow).fmt(strand, &spec, out)
}

fn build_spec_members<'v, 'a, T>(builder: TypeBuilder<'v, 'a, T>) -> TypeBuilder<'v, 'a, T>
where
    T: Object<'v, Annex = SpecAnnex<'v>>,
{
    builder
        .get("fill", |this, strand, mut out| {
            let annex = this.annex();
            match annex.spec.fill {
                Fill::Default => out.store(Value::NIL),
                Fill::Zero => Output::set(strand, out, annex.global.symbols.zero),
                Fill::Char(ch) => {
                    let mut buf = [0; 4];
                    Output::set(strand, out, &*ch.encode_utf8(&mut buf));
                }
            }
            Ok(())
        })
        .get("align", |this, strand, mut out| {
            let annex = this.annex();
            match annex.spec.align {
                None => out.store(Value::NIL),
                Some(align) => Output::set(strand, out, annex.global.symbols.align_symbol(align)),
            }
            Ok(())
        })
        .get("sign", |this, strand, mut out| {
            let annex = this.annex();
            match annex.spec.sign {
                None => out.store(Value::NIL),
                Some(sign) => Output::set(strand, out, annex.global.symbols.sign_symbol(sign)),
            }
            Ok(())
        })
        .get("width", |this, strand, mut out| {
            match this.annex().spec.width {
                Some(width) => Output::set(strand, out, width),
                None => out.store(Value::NIL),
            }
            Ok(())
        })
        .get("precision", |this, strand, mut out| {
            match this.annex().spec.precision {
                Some(precision) => Output::set(strand, out, precision),
                None => out.store(Value::NIL),
            }
            Ok(())
        })
        .get("alt", |this, strand, out| {
            let alt = this.annex().spec.alt;
            Output::set(strand, out, alt);
            Ok(())
        })
        .get("kind", |this, strand, mut out| {
            let annex = this.annex();
            match annex.spec.kind {
                None => out.store(Value::NIL),
                Some(kind) => Output::set(strand, out, annex.global.symbols.kind_symbol(kind)),
            }
            Ok(())
        })
        .method("pad", async |this, strand, args, out| {
            let ([value], []) = unpack!(strand, args, 1, 0)?;
            let value = value
                .as_str_raw(strand)
                .ok_or_else(|| Error::type_error(strand, "FmtSpec.pad: expected Str"))?;
            let spec = this.annex().spec;
            let mut embryo = StrEmbryo::new();
            let mut pad = Pad::new(spec, &mut embryo);
            pad.write_str(strand, value)?;
            pad.finish(strand)?;
            embryo.finish(strand, out);
            Ok(())
        })
}

pub(crate) fn register<'v>(builder: &mut Builder<'v>) -> State<'v, Global<'v>> {
    let spec = build_spec_members(builder.build_type::<FmtSpec>((), ())).build();
    let value = build_spec_members(builder.build_type::<FmtValue>((), ()))
        .get("value", |this, strand, out| {
            let borrow = this.borrow(strand)?;
            Output::set(strand, out, Ref::slot::<0>(&borrow));
            Ok(())
        })
        .get("source", |this, strand, out| {
            let borrow = this.borrow(strand)?;
            Output::set(strand, out, Ref::slot::<1>(&borrow));
            Ok(())
        })
        .build();
    let param = build_spec_members(builder.build_type::<FmtParam>((), ()))
        .get("name", |this, strand, out| {
            let borrow = this.borrow(strand)?;
            Output::set(strand, out, Ref::slot::<0>(&borrow));
            Ok(())
        })
        .get("source", |this, strand, out| {
            let borrow = this.borrow(strand)?;
            Output::set(strand, out, Ref::slot::<1>(&borrow));
            Ok(())
        })
        .build();
    let fmt = builder.build_type::<Fmt>((), ()).build();
    let global = Global::new(
        builder,
        Types {
            spec,
            value,
            param,
            fmt,
        },
    );
    builder.register_state(global)
}

/// Merges keyword options over `base`, producing a new `FmtSpec` or, when a
/// positional value is supplied, a `FmtValue` bound to it.
pub(crate) async fn create<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    base: Spec,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    let source_sym = global.symbols.source;
    let ([], [value, source], rest) = unpack!(strand, args, 0, 1, source_sym = None, ...)?;
    let spec = merge_spec(strand, global, base, rest)?;
    let Some(value) = value else {
        if source.is_some() {
            return Err(Error::type_error(
                strand,
                "source: requires a value to bind it to",
            ));
        }
        create_spec(strand, global, spec, out);
        return Ok(());
    };
    create_value(strand, global, spec, value, source.as_deref(), out);
    Ok(())
}

/// Extracts the specification carried by a Do [`FmtSpec`] or [`FmtValue`].
///
/// This is the inverse of [`reify_spec`]: it accepts the value a Do `(fmt)`
/// method was handed and recovers the specification for a native formatter.
pub(crate) fn spec_of<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
) -> Result<'v, 's, Spec> {
    let global = strand.vm().state::<Global<'v>>();
    if let Some(cast) = global.types.spec.cast(value) {
        return Ok(cast.enter_sync(strand, |_, this| this.annex().spec));
    }
    if let Some(cast) = global.types.value.cast(value) {
        return Ok(cast.enter_sync(strand, |_, this| this.annex().spec));
    }
    Err(Error::type_error(strand, "expected FmtSpec"))
}

/// Reifies a specification as a Do [`FmtSpec`], looking up the module state.
///
/// This is the entry point for code outside this module — notably the class
/// protocol, which hands a `FmtSpec` to a Do-defined `(fmt)` method.
pub(crate) fn reify_spec<'v>(strand: &mut Strand<'v, '_>, spec: Spec, out: Slot<'v, '_>) {
    let global = strand.vm().state::<Global<'v>>();
    create_spec(strand, global, spec, out);
}

fn create_spec<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    spec: Spec,
    out: Slot<'v, '_>,
) {
    global
        .types
        .spec
        .create_with_annex(strand, FmtSpec, SpecAnnex { global, spec }, out);
}

/// Builds a [`FmtParam`] from a name, an optional `source:`, and options.
///
/// This is both the `FmtParam` constructor and the path a `${#...}` takes.
pub(crate) fn create_fmt_param<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    let source_sym = global.symbols.source;
    let ([name], [source], rest) = unpack!(strand, args, 1, 0, source_sym = None, ...)?;
    let spec = merge_spec(strand, global, Spec::default(), rest)?;
    create_param(strand, global, spec, &name, source.as_deref(), out)
}

/// Builds a [`FmtParam`], rejecting a name that is neither `Int` nor `Sym`.
///
/// Nothing else can be a name: binding is keyed lookup, and those are the two
/// key kinds an argument pack and a dict literal can supply.
fn create_param<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    spec: Spec,
    name: &Value<'v>,
    source: Option<&Value<'v>>,
    mut out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    if name.as_sym(strand.vm()).is_none() && !name.is_int(strand) {
        return Err(Error::type_error(
            strand,
            "FmtParam: expected Int or Sym name",
        ));
    }
    let ty = global.types.param;
    ty.create_with_annex(strand, FmtParam, SpecAnnex { global, spec }, &mut out);
    ty.cast(&out)
        .unwrap()
        .enter_sync(strand, |strand, instance| {
            let mut borrow = instance.borrow_mut_unwrap();
            Output::set(strand, Mut::slot_mut::<0>(&mut borrow), name);
            if let Some(source) = source {
                Output::set(strand, Mut::slot_mut::<1>(&mut borrow), source);
            }
        });
    Ok(())
}

fn create_value<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    spec: Spec,
    value: impl Input<'v>,
    source: Option<&Value<'v>>,
    mut out: Slot<'v, '_>,
) {
    let ty = global.types.value;
    ty.create_with_annex(strand, FmtValue, SpecAnnex { global, spec }, &mut out);
    ty.cast(&out)
        .unwrap()
        .enter_sync(strand, |strand, instance| {
            let mut borrow = instance.borrow_mut_unwrap();
            Output::set(strand, Mut::slot_mut::<0>(&mut borrow), value);
            if let Some(source) = source {
                Output::set(strand, Mut::slot_mut::<1>(&mut borrow), source);
            }
        });
}

/// Merges the keyword options in `args` over `base`.
fn merge_spec<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    base: Spec,
    args: Args<'v, 'a>,
) -> Result<'v, 's, Spec> {
    let Symbols {
        fill,
        align,
        sign,
        width,
        precision,
        alt,
        kind,
        ..
    } = global.symbols;
    let ([], [fill, align, sign, width, precision, alt, kind]) = unpack!(
        strand,
        args,
        0,
        0,
        fill = None,
        align = None,
        sign = None,
        width = None,
        precision = None,
        alt = None,
        kind = None
    )?;
    let mut spec = base;
    if let Some(value) = fill {
        spec.fill = parse_fill(strand, global, &value)?;
    }
    if let Some(value) = align {
        spec.align = parse_symbol(strand, &global.aligns, "align", &value)?;
    }
    if let Some(value) = sign {
        spec.sign = parse_symbol(strand, &global.signs, "sign", &value)?;
    }
    if let Some(value) = width {
        spec.width = parse_size(strand, &value)?;
    }
    if let Some(value) = precision {
        spec.precision = parse_size(strand, &value)?;
    }
    if let Some(value) = alt {
        spec.alt = if value.is_nil() {
            false
        } else {
            value
                .as_bool(strand)
                .ok_or_else(|| Error::type_error(strand, "alt: expected Bool or nil"))?
        };
    }
    if let Some(value) = kind {
        spec.kind = parse_symbol(strand, &global.kinds, "kind", &value)?;
    }
    Ok(spec)
}

fn parse_fill<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: &Value<'v>,
) -> Result<'v, 's, Fill> {
    if value.is_nil() {
        return Ok(Fill::Default);
    }
    if value.as_sym(strand) == Some(global.symbols.zero) {
        return Ok(Fill::Zero);
    }
    let value = value
        .as_str_raw(strand)
        .ok_or_else(|| Error::type_error(strand, "fill: expected Str, :ZERO:, or nil"))?;
    let mut chars = value.chars();
    let Some(ch) = chars.next() else {
        return Err(Error::value(strand, "fill: expected one Unicode scalar"));
    };
    if chars.next().is_some() {
        return Err(Error::value(strand, "fill: expected one Unicode scalar"));
    }
    Ok(Fill::Char(ch))
}

fn parse_symbol<'v, 's, T: Copy, const N: usize>(
    strand: &mut Strand<'v, 's>,
    table: &Table<'v, T, N>,
    name: &str,
    value: &Value<'v>,
) -> Result<'v, 's, Option<T>> {
    if value.is_nil() {
        return Ok(None);
    }
    let symbol = value
        .as_sym(strand)
        .ok_or_else(|| Error::type_error(strand, format!("{name}: expected Sym or nil")))?;
    table.value(strand, name, symbol).map(Some)
}

fn parse_size<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
) -> Result<'v, 's, Option<u32>> {
    if value.is_nil() {
        Ok(None)
    } else {
        value.to_u32(strand).map(Some)
    }
}

// Accessors backing the format views in `crate::value::view`. The views are
// the public surface; these keep slot numbering and the annex layout in this
// module, as the container views keep their storage details in `object`.

/// Number of segments in a sequence.
pub(crate) fn view_fmt_len<'v, 's>(
    this: Instance<'v, '_, Fmt>,
    strand: &mut Strand<'v, 's>,
) -> Result<'v, 's, usize> {
    Ref::slot::<0>(&this.borrow(strand)?)
        .as_array(strand)
        .unwrap()
        .len(strand)
}

/// Writes segment `index` to `out`, reporting `false` when out of bounds.
pub(crate) fn view_fmt_segment<'v, 's>(
    this: Instance<'v, '_, Fmt>,
    strand: &mut Strand<'v, 's>,
    index: usize,
    out: impl Output<'v>,
) -> Result<'v, 's, bool> {
    Ref::slot::<0>(&this.borrow(strand)?)
        .as_array(strand)
        .unwrap()
        .get(strand, index, out)
}

/// Writes the bound value of an interpolation to `out`.
pub(crate) fn view_value_bound<'v, 's>(
    this: Instance<'v, '_, FmtValue>,
    strand: &mut Strand<'v, 's>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let borrow = this.borrow(strand)?;
    Output::set(strand, out, Ref::slot::<0>(&borrow));
    Ok(())
}

/// Writes the name of a parameter to `out`.
pub(crate) fn view_param_name<'v, 's>(
    this: Instance<'v, '_, FmtParam>,
    strand: &mut Strand<'v, 's>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let borrow = this.borrow(strand)?;
    Output::set(strand, out, Ref::slot::<0>(&borrow));
    Ok(())
}

/// The specification carried by an interpolation or a parameter.
pub(crate) fn view_spec<'v, T>(this: Instance<'v, '_, T>) -> Spec
where
    T: Object<'v, Annex = SpecAnnex<'v>>,
{
    this.annex().spec
}

/// The source text an interpolation or a parameter was written as, if it came
/// from program text rather than being built at runtime.
pub(crate) fn view_source<'v, 's, T>(
    this: Instance<'v, '_, T>,
    strand: &mut Strand<'v, 's>,
) -> Result<'v, 's, Option<String>>
where
    T: Object<'v, Annex = SpecAnnex<'v>>,
{
    let borrow = this.borrow(strand)?;
    Ok(Ref::slot::<1>(&borrow)
        .as_str_raw(strand)
        .map(str::to_string))
}
