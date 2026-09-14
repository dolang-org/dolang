use std::hash::{Hash, Hasher};

use dolang::runtime::value::fmt::Format;

use dolang::runtime::{
    Alloc, Args, Error, Instance, Object, Output, Result, Slot, State, Strand, Type, Value,
    object::TypeBuilder, unpack, value::View, vm::Register,
};

use crate::global::Global;

pub(crate) struct Uuid;

pub(crate) struct UuidAnnex<'v> {
    inner: uuid::Uuid,
    global: State<'v, Global<'v>>,
}

fn create_uuid_with_global<'v, 'a>(
    global: State<'v, Global<'v>>,
    strand: &mut Strand<'v, '_>,
    id: uuid::Uuid,
    out: Slot<'v, 'a>,
) {
    global
        .types
        .uuid
        .create_with_annex(strand, Uuid, UuidAnnex { inner: id, global }, out);
}

/// Creates a Do `uuid.Uuid` object from an owned `uuid::Uuid`.
pub fn create_uuid<'v, 'a>(strand: &mut Strand<'v, '_>, id: uuid::Uuid, out: Slot<'v, 'a>) {
    let global = strand.force_state::<Global<'v>>();
    create_uuid_with_global(global, strand, id, out);
}

/// Returns the wrapped `uuid::Uuid` if `value` is a Do `uuid.Uuid` object, or
/// `None` otherwise. Unlike [`value_to_uuid`], does not accept `Str`/`Bin`
/// forms.
pub fn cast_uuid<'v>(strand: &mut Strand<'v, '_>, value: &Value<'v>) -> Option<uuid::Uuid> {
    // No `Uuid` can exist before the extension is initialized
    let global = strand.try_state::<Global<'v>>()?;
    let inst = global.types.uuid.cast(value)?;
    Some(inst.enter_sync(strand, |_strand, inst| inst.annex().inner))
}

/// Converts a Do `uuid.Uuid`, `Str`, or `Bin` value into an owned `uuid::Uuid`.
///
/// Text is parsed via `uuid::Uuid::parse_str`, which accepts the hyphenated,
/// simple, braced, and URN forms. Binary values must be exactly 16 bytes.
pub fn value_to_uuid<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
) -> Result<'v, 's, uuid::Uuid> {
    let global = strand.try_state::<Global<'v>>();
    if let Some(inst) = global.and_then(|global| global.types.uuid.cast(value)) {
        return Ok(inst.enter_sync(strand, |_strand, inst| inst.annex().inner));
    }
    if let Some(str) = value.as_str(strand) {
        return strand
            .access(|x| uuid::Uuid::parse_str(str.as_str(x)))
            .map_err(|e| Error::value(strand, e.to_string()));
    }
    match value.view(strand.vm()) {
        View::Bin(bin) => {
            if bin.len() != 16 {
                return Err(Error::value(
                    strand,
                    "uuid binary form must be exactly 16 bytes",
                ));
            }
            let bytes = bin.to_vec();
            uuid::Uuid::from_slice(&bytes).map_err(|e| Error::value(strand, e.to_string()))
        }
        _ => Err(Error::type_error(strand, "expected Uuid, Str, or Bin")),
    }
}

fn variant_sym<'v>(
    global: State<'v, Global<'v>>,
    variant: uuid::Variant,
) -> dolang::runtime::Sym<'v, 'v> {
    match variant {
        uuid::Variant::NCS => global.syms.ncs,
        uuid::Variant::RFC4122 => global.syms.rfc4122,
        uuid::Variant::Microsoft => global.syms.microsoft,
        uuid::Variant::Future => global.syms.future,
        _ => global.syms.future,
    }
}

impl<'v> Object<'v> for Uuid {
    const NAME: &'v str = "Uuid";
    const MODULE: &'v str = "uuid";
    type Annex = UuidAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        _this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([value], []) = unpack!(strand, args, 1, 0)?;
        let id = value_to_uuid(strand, &value)?;
        create_uuid(strand, id, out);
        Ok(())
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        dolang::runtime::object::fmt!(
            strand,
            w,
            "<uuid.Uuid {:?}>",
            this.annex().inner.to_string()
        )
    }

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        dolang::runtime::object::fmt!(strand, w, "{}", this.annex().inner)
    }

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let version_sym = builder.sym("version");
        let variant_sym_kw = builder.sym("variant");

        builder
            .get("bytes", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.as_bytes().as_slice());
                Ok(())
            })
            .get("hex", |this, strand, out| {
                let hex = this.annex().inner.simple().to_string();
                Output::set(strand, out, hex.as_str());
                Ok(())
            })
            .get("version", |this, strand, out| {
                let version = this.annex().inner.get_version_num();
                Output::set(strand, out, version as u8);
                Ok(())
            })
            .get("variant", |this, strand, out| {
                let global = this.annex().global;
                let variant = this.annex().inner.get_variant();
                Output::set(strand, out, variant_sym(global, variant));
                Ok(())
            })
            .type_get("NIL", |_this, strand, out| {
                let global = strand.state::<Global<'v>>();
                create_uuid_with_global(global, strand, uuid::Uuid::nil(), out);
                Ok(())
            })
            .type_get("MAX", |_this, strand, out| {
                let global = strand.state::<Global<'v>>();
                create_uuid_with_global(global, strand, uuid::Uuid::max(), out);
                Ok(())
            })
            .type_method("generate", async move |_this, strand, args, out| {
                let ([], [version, variant]) =
                    unpack!(strand, args, 0, 0, version_sym = None, variant_sym_kw = None)?;
                if version.is_some() || variant.is_some() {
                    return Err(Error::value(
                        strand,
                        "version:/variant: are reserved for future use; only the default (random, v4) generation is currently supported",
                    ));
                }
                let global = strand.state::<Global<'v>>();
                create_uuid_with_global(global, strand, uuid::Uuid::new_v4(), out);
                Ok(())
            })
    }

    fn eq<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        if let Some(other) = this.annex().global.types.uuid.cast(other) {
            Ok(other.enter_sync(strand, |_strand, other| {
                this.annex().inner == other.annex().inner
            }))
        } else {
            Err(Error::not_supported(strand))
        }
    }

    fn hash<'a, 's>(
        this: Instance<'v, 'a, Self>,
        _strand: &'a mut Strand<'v, 's>,
        hasher: &mut impl Hasher,
    ) -> Result<'v, 's, ()> {
        this.annex().inner.hash(hasher);
        Ok(())
    }

    fn lt<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        if let Some(other) = this.annex().global.types.uuid.cast(other) {
            Ok(other.enter_sync(strand, |_strand, other| {
                this.annex().inner < other.annex().inner
            }))
        } else {
            Err(Error::not_supported(strand))
        }
    }
}

pub(crate) fn configure_vm<'v>(builder: &mut Register<'v>, global: State<'v, Global<'v>>) {
    builder
        .module("uuid")
        .value("Uuid", global.types.uuid)
        .value("Guid", global.types.guid)
        .commit();
}
