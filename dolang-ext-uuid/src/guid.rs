use std::hash::{Hash, Hasher};

use dolang::runtime::value::fmt::Format;

use dolang::runtime::{
    AllocExt, Args, Error, Instance, Object, Output, Result, Slot, State, Strand, Type, Value,
    object::TypeBuilder, unpack, value::View,
};

use crate::global::Global;

pub(crate) struct Guid;

pub(crate) struct GuidAnnex<'v> {
    inner: dolang_winterop::guid::Guid,
    global: State<'v, Global<'v>>,
}

fn create_guid_with_global<'v, 'a>(
    global: State<'v, Global<'v>>,
    strand: &mut Strand<'v, '_>,
    id: dolang_winterop::guid::Guid,
    out: Slot<'v, 'a>,
) {
    global
        .types
        .guid
        .create_with_annex(strand, Guid, GuidAnnex { inner: id, global }, out);
}

/// Creates a Do `uuid.Guid` object from an owned `dolang_winterop::guid::Guid`.
pub fn create_guid<'v, 'a>(
    strand: &mut Strand<'v, '_>,
    id: dolang_winterop::guid::Guid,
    out: Slot<'v, 'a>,
) {
    let global = strand.force_state::<Global<'v>>();
    create_guid_with_global(global, strand, id, out);
}

/// Downcasts a Do `uuid.Guid` value into an owned native GUID.
pub fn downcast_guid<'v>(
    strand: &mut Strand<'v, '_>,
    value: &Value<'v>,
) -> Option<dolang_winterop::guid::Guid> {
    // No `Guid` can exist before the extension is initialized
    let global = strand.try_state::<Global<'v>>()?;
    global
        .types
        .guid
        .cast(value)
        .map(|inst| inst.enter_sync(strand, |_strand, inst| inst.annex().inner))
}

/// Converts a Do `uuid.Guid`, `Str`, or `Bin` value into an owned
/// `dolang_winterop::guid::Guid`.
///
/// Text must be the canonical hyphenated form. Binary values must be
/// exactly 16 bytes (the native Windows in-memory GUID layout).
pub fn value_to_guid<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
) -> Result<'v, 's, dolang_winterop::guid::Guid> {
    if let Some(guid) = downcast_guid(strand, value) {
        return Ok(guid);
    }
    if let Some(str) = value.as_str(strand) {
        return strand
            .access(|x| str.as_str(x).parse::<dolang_winterop::guid::Guid>())
            .map_err(|e| Error::value(strand, e.to_string()));
    }
    match value.view(strand.vm()) {
        View::Bin(bin) => {
            if bin.len() != 16 {
                return Err(Error::value(
                    strand,
                    "guid binary form must be exactly 16 bytes",
                ));
            }
            let bytes = bin.to_vec();
            dolang_winterop::guid::Guid::from_bytes(&bytes)
                .map_err(|e| Error::value(strand, e.to_string()))
        }
        _ => Err(Error::type_error(strand, "expected Guid, Str, or Bin")),
    }
}

impl<'v> Object<'v> for Guid {
    const NAME: &'v str = "Guid";
    const MODULE: &'v str = "uuid";
    type Annex = GuidAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        _this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([value], []) = unpack!(strand, args, 1, 0)?;
        let id = value_to_guid(strand, &value)?;
        create_guid(strand, id, out);
        Ok(())
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        dolang::runtime::object::fmt!(strand, w, "<uuid.Guid {}>", this.annex().inner)
    }

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        dolang::runtime::object::fmt!(strand, w, "{}", this.annex().inner)
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("bytes", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.to_bytes().as_slice());
                Ok(())
            })
            .get("data1", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.data1);
                Ok(())
            })
            .get("data2", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.data2);
                Ok(())
            })
            .get("data3", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.data3);
                Ok(())
            })
            .get("data4", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.data4.as_slice());
                Ok(())
            })
            .type_get("NIL", |_this, strand, out| {
                let global = strand.state::<Global<'v>>();
                let nil = dolang_winterop::guid::Guid {
                    data1: 0,
                    data2: 0,
                    data3: 0,
                    data4: [0; 8],
                };
                create_guid_with_global(global, strand, nil, out);
                Ok(())
            })
            .type_method("generate", async move |_this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let global = strand.state::<Global<'v>>();
                create_guid_with_global(global, strand, dolang_winterop::guid::Guid::new_v4(), out);
                Ok(())
            })
    }

    fn eq<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        if let Some(other) = this.annex().global.types.guid.cast(other) {
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
        if let Some(other) = this.annex().global.types.guid.cast(other) {
            Ok(other.enter_sync(strand, |_strand, other| {
                this.annex().inner.to_bytes() < other.annex().inner.to_bytes()
            }))
        } else {
            Err(Error::not_supported(strand))
        }
    }
}
