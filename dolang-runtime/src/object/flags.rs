//! A nameable "set of symbols" native type.
//!
//! Unlike [`super::array_view`]/[`super::dict_view`], which are anonymous
//! lazy projections sharing one generic `Type` per helper, a flags value is a
//! real, independently nameable, constructible, downcastable type — one per
//! extension-defined flag domain. An extension implements [`FlagLike`] for
//! its Rust-side bitset representation and calls [`FlagLikeExt::register_type`] to
//! get a Do-visible type with construction from symbols, set-algebra
//! operators (`|`, `&`, `^`, `~`), membership testing, iteration, and debug
//! output — all implemented once, generically, in [`Flags`].
//!
//! A representation can opt into an integer form ([`FlagLike::INT`]), which
//! adds an `int` getter and construction from an `Int`, and into construction
//! from related flags types ([`FlagLike::COERCE`]).

use std::{
    cell::Cell,
    future::Future,
    hash::{Hash, Hasher},
    marker::PhantomData,
    ops::ControlFlow,
    ops::{BitAnd, BitOr, BitXor, Not},
};

use crate::value::{TypeObject, fmt::Format};

use crate::{
    arg::{Arg, Args},
    error::{Error, Result},
    gc::arena::Visit,
    object::{
        interface::{self, InterfaceTag, Request},
        native::{Instance, Object, Type, TypeBuilder},
        protocol::{Protocol, Recv, Spread, SpreadContext},
    },
    strand::Strand,
    sym::Sym,
    unpack,
    value::{Output, Slot, Value},
    vm::Register,
};

/// Which related flags types a [`Flags`] constructor accepts, by nominal
/// relation to the type being constructed.
///
/// A value of the same type is always accepted. An unrelated flags type, or
/// a related one in a direction this doesn't allow, is a type error.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FlagCoercion {
    /// Accept no other flags types.
    None,
    /// Accept values of the type's nominal supertypes.
    FromSupertypes,
    /// Accept values of the type's nominal subtypes.
    FromSubtypes,
    /// Accept values of nominal supertypes and subtypes.
    Both,
}

impl FlagCoercion {
    fn accepts_supertypes(self) -> bool {
        matches!(self, Self::FromSupertypes | Self::Both)
    }

    fn accepts_subtypes(self) -> bool {
        matches!(self, Self::FromSubtypes | Self::Both)
    }
}

/// A Rust-side bitset that can be exposed to Do as a [`Flags`] native type.
///
/// Implement this for a plain bitflags-style type (e.g. a newtype over an
/// integer with hand-written or `bitflags!`-generated bitwise operators), then
/// call [`FlagLikeExt::register_type`] once at VM setup to get a real, nameable,
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
    /// entries) is fine — [`Flags`]'s iteration/debug output decomposes
    /// values into the most granular matching entries.
    const BITS: &'static [(&'static str, Self)];

    /// Whether the type has an integer representation. When set, the type
    /// gets an `int` getter and its constructor accepts a single `Int`, both
    /// through [`to_i128`](Self::to_i128)/[`from_i128`](Self::from_i128),
    /// which the type must then implement.
    const INT: bool = false;

    /// Which related flags types the constructor accepts. When both types
    /// are [`INT`](Self::INT), the value converts through its integer form,
    /// so [`from_i128`](Self::from_i128) decides what happens to bits this
    /// type doesn't name; otherwise it converts by its symbols, and a name
    /// this type lacks is a value error.
    const COERCE: FlagCoercion = FlagCoercion::None;

    /// The integer form of `self`, or `None` if it has none. Called only
    /// when [`INT`](Self::INT) is set.
    fn to_i128(self) -> Option<i128> {
        None
    }

    /// The value for integer form `value`, or `None` (a value error) if it
    /// is out of range or otherwise invalid. Whether bits the type doesn't
    /// name are kept or rejected is the type's choice. Called only when
    /// [`INT`](Self::INT) is set.
    fn from_i128(value: i128) -> Option<Self> {
        let _ = value;
        None
    }

    /// The granularity of this set of bits. Lower-ranked entries are used
    /// first when decomposing a value for iteration and debug output.
    ///
    /// For ordinary integer bitflags, return the population count.
    fn rank(self) -> usize;

    /// Adds representation-specific methods, getters, or supertypes.
    fn build<'v, 'a>(builder: TypeBuilder<'v, 'a, Flags<Self>>) -> TypeBuilder<'v, 'a, Flags<Self>>
    where
        Self: Sized,
    {
        builder
    }
}

/// Non-customizable registration helpers for [`FlagLike`] representations.
pub trait FlagLikeExt: FlagLike {
    /// Starts building `Flags<Self>` as a Do-visible native type.
    fn build_type<'v, 'a>(builder: &'a mut Register<'v>) -> TypeBuilder<'v, 'a, Flags<Self>> {
        let mut entries = Vec::with_capacity(Self::BITS.len());
        let mut all = Self::ZERO;
        for &(name, bits) in Self::BITS {
            entries.push(FlagEntry {
                sym: builder.sym(name),
                bits,
                rank: bits.rank(),
                non_decomposable: false,
            });
            all = all | bits;
        }
        for index in 0..entries.len() {
            if entries[index].rank > 1 {
                entries[index].non_decomposable =
                    !can_decompose(&entries, entries[index].bits, index);
            }
        }
        entries.sort_by_key(|entry| {
            (
                if entry.non_decomposable {
                    0
                } else if entry.rank == 1 {
                    1
                } else {
                    2
                },
                entry.rank,
            )
        });
        let table = FlagTable {
            entries: entries.into_boxed_slice(),
            all,
        };
        builder.build_type::<Flags<Self>>((), table)
    }

    /// Registers `Flags<Self>` as a Do-visible native type.
    fn register_type<'v>(builder: &mut Register<'v>) -> Type<'v, Flags<Self>> {
        Self::build_type(builder).build()
    }
}

impl<F: FlagLike> FlagLikeExt for F {}

/// Native object for a set of symbols backed by `F`. Register with
/// [`FlagLikeExt::register_type`]; construct/read values via [`FlagsTypeExt`].
pub struct Flags<F>(PhantomData<F>);

/// `TypeAnnex` for [`Flags`]: interned `Sym`s for every [`FlagLike::BITS`]
/// entry, plus their precomputed union. Opaque — fields are private.
pub struct FlagTable<'v, F> {
    entries: Box<[FlagEntry<'v, F>]>,
    all: F,
}

struct FlagEntry<'v, F> {
    sym: Sym<'v, 'v>,
    bits: F,
    rank: usize,
    non_decomposable: bool,
}

impl<'v, F: FlagLike> FlagTable<'v, F> {
    fn resolve(&self, sym: Sym<'v, '_>) -> Option<F> {
        self.entries
            .iter()
            .find(|entry| entry.sym == sym)
            .map(|entry| entry.bits)
    }

    /// Decomposes `bits` in the precomputed granularity order.
    fn names(&self, bits: F) -> Vec<Sym<'v, 'v>> {
        let mut remaining = bits;
        let mut names = Vec::new();
        for entry in &self.entries {
            if entry.rank != 0 && (entry.bits & remaining) == entry.bits {
                remaining = remaining & !entry.bits;
                names.push(entry.sym);
            }
        }
        names
    }
}

fn can_decompose<F: FlagLike>(entries: &[FlagEntry<'_, F>], bits: F, skip: usize) -> bool {
    if bits == F::ZERO {
        return true;
    }
    entries.iter().enumerate().any(|(index, entry)| {
        index != skip
            && entry.rank != 0
            && (entry.bits & bits) == entry.bits
            && can_decompose(entries, bits & !entry.bits, skip)
    })
}

/// Shared iterator for the symbols yielded by any [`Flags`] value.
///
/// Each flags type has its own `FlagTable`, but all flag iterators share this
/// one runtime type. The small boxed slice keeps the iterator independent of
/// the source flags object.
pub(crate) struct Iter<'v> {
    symbols: Box<[Sym<'v, 'v>]>,
    index: Cell<usize>,
}

unsafe impl<'v> crate::gc::ImmState for Iter<'v> {
    const CYCLIC: bool = false;
}

impl<'v> crate::gc::Annex for Iter<'v> {
    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }
}

pub(crate) type IterObj<'v> = crate::gc::Imm<Iter<'v>>;

impl<'v> Protocol<'v> for IterObj<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().input_iter)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<flags iterator>")
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, &this);
        Ok(())
    }

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let iter = this.get();
        let index = iter.index.get();
        let Some(sym) = iter.symbols.get(index) else {
            return Ok(false);
        };
        Output::set(strand, out, *sym);
        iter.index.set(index + 1);
        Ok(true)
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        super::iter::iter_get(strand, &this, field, out)
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        super::iter::iter_mcall(strand, &this, method, args, out).await
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: super::protocol::SpreadContext,
        sink: &'a mut dyn super::protocol::Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        super::iter::spread_iter(strand, this.clone(), sink).await
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

/// The integer form a flags value lends through [`FlagsTag`].
#[derive(Clone, Copy)]
enum FlagsInt {
    /// The type isn't [`FlagLike::INT`].
    None,
    /// [`FlagLike::to_i128`] failed.
    Invalid,
    Value(i128),
}

/// What any [`Flags`] value tells a constructor of another flags type.
#[derive(Clone, Copy)]
struct FlagsSource {
    module: &'static str,
    name: &'static str,
    int: FlagsInt,
}

/// Interface tag under which every [`Flags`] value provides a [`FlagsSource`].
struct FlagsTag;

impl InterfaceTag for FlagsTag {
    type Interface<'v, 'a> = FlagsSource;
}

fn int_error<'v, 's, F: FlagLike>(strand: &mut Strand<'v, 's>) -> Error<'v, 's> {
    Error::value(
        strand,
        format!(
            "`{}.{}` value has no integer representation",
            F::MODULE,
            F::NAME
        ),
    )
}

fn from_int<'v, 's, F: FlagLike>(strand: &mut Strand<'v, 's>, value: i128) -> Result<'v, 's, F> {
    F::from_i128(value).ok_or_else(|| {
        Error::value(
            strand,
            format!("invalid `{}.{}` integer: {value}", F::MODULE, F::NAME),
        )
    })
}

/// Whether `ty` is a nominal subtype of `value`'s type.
fn is_supertype_of<'v, F: FlagLike>(
    ty: Type<'v, Flags<F>>,
    strand: &mut Strand<'v, '_>,
    value: &Value<'v>,
) -> bool {
    strand.with_slots_sync(|strand, [mut source_ty, mut target_ty]| {
        value.op_type(strand, Slot::reborrow(&mut source_ty));
        Output::set(strand, &mut target_ty, ty);
        target_ty.op_subtype(strand, &source_ty)
    })
}

/// Converts the single-argument form of a [`Flags`] constructor: a value of
/// the same type, a symbol, an `Int` (when [`FlagLike::INT`]), a value of a
/// related flags type (as [`FlagLike::COERCE`] allows), or an iterable of
/// symbols.
async fn convert<'v, 's, F: FlagLike>(
    ty: Type<'v, Flags<F>>,
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
) -> Result<'v, 's, F> {
    if let Some(bits) = ty.cast_flags(value) {
        return Ok(bits);
    }
    let table = ty.annex(strand.vm());
    if let Some(sym) = value.as_sym(strand.vm()) {
        return resolve_sym(strand, table, sym);
    }
    if F::INT
        && let Some(int) = value.as_int(strand)
    {
        return from_int(strand, int);
    }
    if let Some(source) =
        interface::try_query::<FlagsTag, _>(value, strand, |_strand, source| Ok(*source))?
    {
        let allowed = if value.is_instance_of(strand, ty) {
            F::COERCE.accepts_subtypes()
        } else if is_supertype_of(ty, strand, value) {
            F::COERCE.accepts_supertypes()
        } else {
            false
        };
        if !allowed {
            return Err(Error::type_error(
                strand,
                format!(
                    "cannot convert `{}.{}` to `{}.{}`",
                    source.module,
                    source.name,
                    F::MODULE,
                    F::NAME
                ),
            ));
        }
        if F::INT {
            match source.int {
                FlagsInt::Value(int) => return from_int(strand, int),
                FlagsInt::Invalid => {
                    return Err(Error::value(
                        strand,
                        format!(
                            "`{}.{}` value has no integer representation",
                            source.module, source.name
                        ),
                    ));
                }
                FlagsInt::None => {}
            }
        }
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
    /// single argument converted as [`FlagsTypeExt::coerce`] does: a `Sym`,
    /// an `Int` (when [`FlagLike::INT`]), a flags value of this or a related
    /// type, or an iterable of symbols (`MyFlags(list)`). These are
    /// unambiguous: a `Sym` is never a meaningful sequence to spread, and an
    /// `Int` or flags value is never a symbol.
    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let table = this.annex(strand.vm());
        let bits = if args.len() == 1 {
            let slot = positional_arg(strand, args.next().unwrap())?;
            convert(this, strand, &slot).await?
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

    async fn spread<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn super::protocol::Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        super::iter::spread_iter(strand, this, sink).await
    }

    fn provide<'a, 's>(
        this: Instance<'v, 'a, Self>,
        request: &mut Request<'v, 's, '_>,
    ) -> Result<'v, 's, ()> {
        let int = if F::INT {
            match this.annex().to_i128() {
                Some(int) => FlagsInt::Value(int),
                None => FlagsInt::Invalid,
            }
        } else {
            FlagsInt::None
        };
        request.provide::<FlagsTag>(&mut FlagsSource {
            module: F::MODULE,
            name: F::NAME,
            int,
        })
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

    fn rband<'a, 's>(
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

    fn rbor<'a, 's>(
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

    fn rbxor<'a, 's>(
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
        for (i, sym) in table.names(*this.annex()).into_iter().enumerate() {
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
        let symbols = table.names(*this.annex()).into_boxed_slice();
        strand.builtin_types().flags_iter.create_imm(
            strand,
            Iter {
                symbols,
                index: Cell::new(0),
            },
            out,
        );
        Ok(())
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let builder = if F::INT {
            builder.get("int", |this, strand, out| {
                let int = this
                    .annex()
                    .to_i128()
                    .ok_or_else(|| int_error::<F>(strand))?;
                Output::set(strand, out, int);
                Ok(())
            })
        } else {
            builder
        };
        F::build(builder.supertype(TypeObject::Iterable).method(
            "contains",
            async move |this, strand, args, mut out| {
                let ([sym], []) = unpack!(strand, args, 1, 0)?;
                let sym = sym
                    .as_sym(strand.vm())
                    .ok_or_else(|| Error::type_error(strand, "expected symbol"))?;
                let vm = strand.vm();
                let table = this.ty(vm).annex(vm);
                let bit = resolve_sym(strand, table, sym)?;
                Output::set(strand, &mut out, (*this.annex() & bit) == bit);
                Ok(())
            },
        ))
    }
}

/// Copies the representation stored by a [`Flags`] instance.
pub trait FlagsInstanceExt<F: FlagLike> {
    fn flags(&self) -> F;
}

impl<F: FlagLike> FlagsInstanceExt<F> for Instance<'_, '_, Flags<F>> {
    fn flags(&self) -> F {
        *self.annex()
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

    /// Converts `value` as [`Flags::new`]'s single-argument form does: an
    /// instance of this type (its bits are copied out), a symbol, an `Int`
    /// (when [`FlagLike::INT`]), a value of a related flags type (as
    /// [`FlagLike::COERCE`] allows), or an iterable of symbols. Returns a
    /// type error if `value` is none of these.
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
        convert(*self, strand, value).await
    }
}
