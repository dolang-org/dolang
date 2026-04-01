use dolang::runtime::{
    Args, Error, Instance, Object, Output, Result, Slot, Strand, Type, Value, object::TypeBuilder,
    unpack, value::TypeObject, vm::Builder,
};

pub(crate) struct DigestState;
pub(crate) struct Blake3State(blake3::Hasher);

trait Algorithm: digest::Digest + Default + Clone + 'static {
    const TYPE_NAME: &'static str;
}

pub(crate) struct Digestible<T>(T);

impl Algorithm for md5::Md5 {
    const TYPE_NAME: &'static str = "Md5";
}

impl Algorithm for sha1::Sha1 {
    const TYPE_NAME: &'static str = "Sha1";
}

impl Algorithm for sha2::Sha256 {
    const TYPE_NAME: &'static str = "Sha256";
}

impl Algorithm for sha2::Sha512 {
    const TYPE_NAME: &'static str = "Sha512";
}

fn bytes_arg<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    value: &'a Value<'v>,
    msg: &'v str,
) -> Result<'v, 's, &'a [u8]> {
    value
        .as_u8_slice(strand)
        .ok_or_else(|| Error::type_error(strand, msg))
}

fn digest_single<'v, 's, T: Algorithm>(
    strand: &mut Strand<'v, 's>,
    value: Slot<'v, '_>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let bytes = bytes_arg(strand, &value, "expected str or bin")?;
    let digest = T::digest(bytes);
    Output::set(strand, out, digest.as_slice());
    Ok(())
}

fn digest_update<'v, 's, T: Algorithm>(
    strand: &mut Strand<'v, 's>,
    this: Instance<'v, '_, Digestible<T>>,
    value: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let bytes = bytes_arg(strand, &value, "update: expected str or bin")?;
    this.borrow_mut(strand)?.0.update(bytes);
    Ok(())
}

fn blake3_single<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: Slot<'v, '_>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let bytes = bytes_arg(strand, &value, "expected str or bin")?;
    let digest = blake3::hash(bytes);
    Output::set(strand, out, digest.as_slice());
    Ok(())
}

fn blake3_update<'v, 's>(
    strand: &mut Strand<'v, 's>,
    this: Instance<'v, '_, Blake3State>,
    value: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let bytes = bytes_arg(strand, &value, "update: expected str or bin")?;
    this.borrow_mut(strand)?.0.update(bytes);
    Ok(())
}

impl<'v> Object<'v> for DigestState {
    const NAME: &'v str = "State";
    const MODULE: &'v str = "digest";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();
}

impl<'v> Object<'v> for Blake3State {
    const NAME: &'v str = "Blake3";
    const MODULE: &'v str = "digest";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([], []) = unpack!(strand, args, 0, 0)?;
        this.create(strand, Self(blake3::Hasher::new()), out);
        Ok(())
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .supertype(TypeObject::Sink)
            .method("update", async move |this, strand, args, out| {
                let ([value], []) = unpack!(strand, args, 1, 0)?;
                blake3_update(strand, this, value)?;
                Output::set(strand, out, this);
                Ok(())
            })
            .method("digest", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let digest = this.borrow(strand)?.0.clone().finalize();
                Output::set(strand, out, digest.as_slice());
                Ok(())
            })
    }

    async fn output<'a, 's>(
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
        blake3_update(strand, this, value)
    }
}

impl<'v, T: Algorithm> Object<'v> for Digestible<T> {
    const NAME: &'v str = T::TYPE_NAME;
    const MODULE: &'v str = "digest";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([], []) = unpack!(strand, args, 0, 0)?;
        this.create(strand, Self(T::default()), out);
        Ok(())
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .supertype(TypeObject::Sink)
            .method("update", async move |this, strand, args, out| {
                let ([value], []) = unpack!(strand, args, 1, 0)?;
                digest_update(strand, this, value)?;
                Output::set(strand, out, this);
                Ok(())
            })
            .method("digest", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let digest = this.borrow(strand)?.0.clone().finalize();
                Output::set(strand, out, digest.as_slice());
                Ok(())
            })
    }

    async fn output<'a, 's>(
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
        digest_update(strand, this, value)
    }
}

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>) {
    let state = builder.register_type::<DigestState>();
    let blake3 = builder
        .build_type::<Blake3State>((), ())
        .nominal_supertype(state)
        .build();
    let md5 = builder
        .build_type::<Digestible<md5::Md5>>((), ())
        .nominal_supertype(state)
        .build();
    let sha1 = builder
        .build_type::<Digestible<sha1::Sha1>>((), ())
        .nominal_supertype(state)
        .build();
    let sha256 = builder
        .build_type::<Digestible<sha2::Sha256>>((), ())
        .nominal_supertype(state)
        .build();
    let sha512 = builder
        .build_type::<Digestible<sha2::Sha512>>((), ())
        .nominal_supertype(state)
        .build();

    builder
        .module("digest")
        .value("State", state)
        .value("Blake3", blake3)
        .value("Md5", md5)
        .value("Sha1", sha1)
        .value("Sha256", sha256)
        .value("Sha512", sha512)
        .function("blake3", async move |strand, args, out| {
            let ([value], []) = unpack!(strand, args, 1, 0)?;
            blake3_single(strand, value, out)
        })
        .function("md5", async move |strand, args, out| {
            let ([value], []) = unpack!(strand, args, 1, 0)?;
            digest_single::<md5::Md5>(strand, value, out)
        })
        .function("sha1", async move |strand, args, out| {
            let ([value], []) = unpack!(strand, args, 1, 0)?;
            digest_single::<sha1::Sha1>(strand, value, out)
        })
        .function("sha256", async move |strand, args, out| {
            let ([value], []) = unpack!(strand, args, 1, 0)?;
            digest_single::<sha2::Sha256>(strand, value, out)
        })
        .function("sha512", async move |strand, args, out| {
            let ([value], []) = unpack!(strand, args, 1, 0)?;
            digest_single::<sha2::Sha512>(strand, value, out)
        })
        .commit();
}
