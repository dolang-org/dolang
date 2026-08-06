//! A nameable "set of symbols" native type.
//!
//! Unlike [`super::array_view`]/[`super::dict_view`], which are anonymous
//! lazy projections sharing one generic `Type` per helper, a flags value is a
//! real, independently nameable, constructible, downcastable type — one per
//! extension-defined flag domain. An extension implements [`FlagLike`] for
//! its Rust-side bitset representation and calls [`FlagLike::register`] to
//! get a Do-visible type with construction from symbols, set-algebra
//! operators (`|`, `&`, `^`, `~`), membership testing, iteration, and debug
//! output — all implemented once, generically, in [`Flags`].

use std::{
    future::Future,
    hash::{Hash, Hasher},
    marker::PhantomData,
    ops::{BitAnd, BitOr, BitXor, Not},
};

use crate::{
    arg::{Arg, Args},
    error::{Error, Result},
    object::{
        array,
        native::{Instance, Object, Type, TypeBuilder},
        protocol::{Spread, SpreadContext},
    },
    strand::Strand,
    sym::Sym,
    unpack,
    value::{Format, Output, Slot, Value},
    vm::Builder,
};

/// A Rust-side bitset that can be exposed to Do as a [`Flags`] native type.
///
/// Implement this for a plain bitflags-style type (e.g. a newtype over an
/// integer with hand-written or `bitflags!`-generated bitwise operators), then
/// call [`FlagLike::register`] once at VM setup to get a real, nameable,
/// constructible Do type back.
pub trait FlagLike:
    Copy
    + Eq
    + Hash
    + BitOr<Output = Self>
    + BitAnd<Output = Self>
    + BitXor<Output = Self>
    + Not<Output = Self>
    + 'static
{
    /// The empty set (no flags set).
    const ZERO: Self;
    /// Do module the registered type belongs to.
    const MODULE: &'static str;
    /// Do-visible name of the registered type.
    const NAME: &'static str;
    /// Name <-> bit table, in declaration order. Also defines validity:
    /// symbols outside this table are rejected on construction. A "named
    /// combination" entry (a value that is itself the union of other
    /// entries) is fine — [`Flags`]'s iteration/debug output includes any
    /// entry whose bits are a subset of the value being examined.
    const BITS: &'static [(&'static str, Self)];

    /// Registers `Flags<Self>` as a Do-visible native type.
    fn build_type<'v, 'a>(builder: &'a mut Builder<'v>) -> TypeBuilder<'v, 'a, Flags<Self>>
    where
        Self: Sized,
    {
        let mut entries = Vec::with_capacity(Self::BITS.len());
        let mut all = Self::ZERO;
        for &(name, bits) in Self::BITS {
            entries.push((builder.sym(name), bits));
            all = all | bits;
        }
        let table = FlagTable {
            entries: entries.into_boxed_slice(),
            all,
        };
        builder.build_type::<Flags<Self>>((), table)
    }

    /// Registers `Flags<Self>` as a Do-visible native type.
    fn register_type<'v>(builder: &mut Builder<'v>) -> Type<'v, Flags<Self>>
    where
        Self: Sized,
    {
        Self::build_type(builder).build()
    }
}

/// Native object for a set of symbols backed by `F`. Register with
/// [`FlagLike::register`]; construct/read values via [`FlagsTypeExt`].
pub struct Flags<F>(PhantomData<F>);

/// `TypeAnnex` for [`Flags`]: interned `Sym`s for every [`FlagLike::BITS`]
/// entry, plus their precomputed union. Opaque — fields are private.
pub struct FlagTable<'v, F> {
    entries: Box<[(Sym<'v, 'v>, F)]>,
    all: F,
}

impl<'v, F: FlagLike> FlagTable<'v, F> {
    fn resolve(&self, sym: Sym<'v, '_>) -> Option<F> {
        self.entries
            .iter()
            .find(|(s, _)| *s == sym)
            .map(|(_, bits)| *bits)
    }

    /// Every table entry whose bits are fully contained in `bits`.
    fn names(&self, bits: F) -> impl Iterator<Item = Sym<'v, 'v>> + '_ {
        self.entries
            .iter()
            .filter(move |(_, entry)| (*entry & bits) == *entry && *entry != F::ZERO)
            .map(|(sym, _)| *sym)
    }
}

fn resolve_sym<'v, 's, F: FlagLike>(
    strand: &mut Strand<'v, 's>,
    table: &FlagTable<'v, F>,
    sym: Sym<'v, '_>,
) -> Result<'v, 's, F> {
    table.resolve(sym).ok_or_else(|| {
        let name = sym.as_str(strand.vm());
        Error::value(strand, format!("unrecognized flag: {name}"))
    })
}

/// Spreads a plain sequence of symbols (`SpreadContext::Sequence`) into a
/// `F` accumulator. Any dict-like/keyed item is rejected — a flags value can
/// only be built from symbols.
struct FlagsSpread<'t, 'v, F> {
    table: &'t FlagTable<'v, F>,
    bits: F,
}

impl<'t, 'v, 's, F: FlagLike> Spread<'v, 's> for FlagsSpread<'t, 'v, F> {
    fn positional(
        &mut self,
        strand: &mut Strand<'v, 's>,
        value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let sym = value
            .as_sym(strand.vm())
            .ok_or_else(|| Error::type_error(strand, "expected symbol"))?;
        self.bits = self.bits | resolve_sym(strand, self.table, sym)?;
        Ok(())
    }

    fn symbol(
        &mut self,
        strand: &mut Strand<'v, 's>,
        _key: Sym<'v, '_>,
        _value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(strand, "expected symbol"))
    }

    fn keyed(
        &mut self,
        strand: &mut Strand<'v, 's>,
        _key: Slot<'v, '_>,
        _value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(strand, "expected symbol"))
    }
}

fn positional_arg<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    arg: Arg<'v, 'a>,
) -> Result<'v, 's, Slot<'v, 'a>> {
    match arg {
        Arg::Pos(slot) => Ok(slot),
        Arg::Key(key, _) => Err(Error::unexpected_key(strand, key)),
    }
}

impl<'v, F: FlagLike> Object<'v> for Flags<F> {
    const NAME: &'v str = F::NAME;
    const MODULE: &'v str = F::MODULE;
    type Annex = F;
    type Type = ();
    type TypeAnnex = FlagTable<'v, F>;

    /// Accepts either variadic `Sym` arguments (`MyFlags(:A:, :B:)`) or a
    /// single iterable (`MyFlags(list)`/`MyFlags(...list)`) — unambiguous
    /// because every valid positional argument here is a `Sym`, and a bare
    /// `Sym` is never itself a meaningful sequence to spread.
    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let table = this.annex(strand.vm());
        let bits = if args.len() == 1 {
            let slot = positional_arg(strand, args.next().unwrap())?;
            if let Some(sym) = slot.as_sym(strand.vm()) {
                resolve_sym(strand, table, sym)?
            } else {
                let mut spread = FlagsSpread {
                    table,
                    bits: F::ZERO,
                };
                slot.op_spread(strand, SpreadContext::Sequence, &mut spread)
                    .await?;
                spread.bits
            }
        } else {
            let mut bits = F::ZERO;
            for arg in args {
                let slot = positional_arg(strand, arg)?;
                let sym = slot
                    .as_sym(strand.vm())
                    .ok_or_else(|| Error::type_error(strand, "expected symbol"))?;
                bits = bits | resolve_sym(strand, table, sym)?;
            }
            bits
        };
        this.create_flags(strand, bits, out);
        Ok(())
    }

    fn eq<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        let ty = this.ty(strand.vm());
        Ok(match ty.cast(other) {
            Some(cast) => cast.enter_sync(strand, |_strand, inst| *inst.annex()) == *this.annex(),
            None => false,
        })
    }

    fn hash<'a, 's>(
        this: Instance<'v, 'a, Self>,
        _strand: &'a mut Strand<'v, 's>,
        hasher: &mut impl Hasher,
    ) -> Result<'v, 's, ()> {
        this.annex().hash(hasher);
        Ok(())
    }

    fn bnot<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ty = this.ty(strand.vm());
        let all = ty.annex(strand.vm()).all;
        ty.create_flags(strand, !*this.annex() & all, out);
        Ok(())
    }

    fn band<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        binop(this, strand, other, out, BitAnd::bitand)
    }

    fn bor<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        binop(this, strand, other, out, BitOr::bitor)
    }

    fn bxor<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        binop(this, strand, other, out, BitXor::bitxor)
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let vm = strand.vm();
        let table = this.ty(vm).annex(vm);
        crate::fmt!(strand, w, "<{}.{}:", F::MODULE, F::NAME)?;
        for (i, sym) in table.names(*this.annex()).enumerate() {
            let sep = if i == 0 { " " } else { "|" };
            crate::fmt!(strand, w, "{sep}{}", sym.as_str(vm))?;
        }
        crate::fmt!(strand, w, ">")
    }

    async fn iter<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let vm = strand.vm();
        let table = this.ty(vm).annex(vm);
        let mut array = array::Array::new();
        for sym in table.names(*this.annex()) {
            array.inner.push(Value::from_input(strand, sym));
        }
        let mut arr_val = Value::NIL;
        strand
            .builtin_types()
            .array
            .create(strand, array, Slot::new(&mut arr_val));
        arr_val.op_iter(strand, out).await
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.method("contains", async move |this, strand, args, mut out| {
            let ([sym], []) = unpack!(strand, args, 1, 0)?;
            let sym = sym
                .as_sym(strand.vm())
                .ok_or_else(|| Error::type_error(strand, "expected symbol"))?;
            let vm = strand.vm();
            let table = this.ty(vm).annex(vm);
            let bit = resolve_sym(strand, table, sym)?;
            Output::set(strand, &mut out, (*this.annex() & bit) == bit);
            Ok(())
        })
    }
}

fn binop<'v, 'a, 's, F: FlagLike>(
    this: Instance<'v, 'a, Flags<F>>,
    strand: &'a mut Strand<'v, 's>,
    other: &Value<'v>,
    out: Slot<'v, 'a>,
    op: impl FnOnce(F, F) -> F,
) -> Result<'v, 's, ()> {
    let ty = this.ty(strand.vm());
    let Some(cast) = ty.cast(other) else {
        return Err(Error::type_error(
            strand,
            format!("expected `{}.{}`", F::MODULE, F::NAME),
        ));
    };
    let other_bits = cast.enter_sync(strand, |_strand, inst| *inst.annex());
    ty.create_flags(strand, op(*this.annex(), other_bits), out);
    Ok(())
}

/// Convenience methods for a registered [`Flags`] type, hiding
/// `create_with_annex`/downcast/annex mechanics from extension authors.
pub trait FlagsTypeExt<'v, F: FlagLike> {
    /// Constructs a `Flags<F>` value directly from Rust-side bits.
    fn create_flags(&self, strand: &mut Strand<'v, '_>, bits: F, out: impl Output<'v>);

    /// Downcasts `value` and copies out its bits, or `None` if `value` isn't
    /// an instance of this type.
    fn cast_flags(&self, value: &Value<'v>) -> Option<F>;

    /// Accepts either an existing instance of this type (its bits are
    /// copied out) or a plain iterable of symbols (spread the same way
    /// [`Flags::new`]'s single-argument sequence form does), and returns the
    /// resulting `F` bits either way. Returns a type error if `value` is
    /// neither.
    fn coerce<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        value: &Value<'v>,
    ) -> impl Future<Output = Result<'v, 's, F>>;
}

impl<'v, F: FlagLike> FlagsTypeExt<'v, F> for Type<'v, Flags<F>> {
    fn create_flags(&self, strand: &mut Strand<'v, '_>, bits: F, out: impl Output<'v>) {
        self.create_with_annex(strand, Flags(PhantomData), bits, out);
    }

    fn cast_flags(&self, value: &Value<'v>) -> Option<F> {
        self.cast(value)
            .map(|cast| cast.enter_finalize(|inst| *inst.annex()))
    }

    async fn coerce<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        value: &Value<'v>,
    ) -> Result<'v, 's, F> {
        if let Some(bits) = self.cast_flags(value) {
            return Ok(bits);
        }
        let table = self.annex(strand.vm());
        if let Some(sym) = value.as_sym(strand.vm()) {
            return resolve_sym(strand, table, sym);
        }
        let mut spread = FlagsSpread {
            table,
            bits: F::ZERO,
        };
        value
            .op_spread(strand, SpreadContext::Sequence, &mut spread)
            .await?;
        Ok(spread.bits)
    }
}
