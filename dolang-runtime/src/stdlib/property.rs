use crate::{
    arg::Args,
    call,
    error::Result,
    object::native::{Mut, Object, Ref, Type, TypeBuilder},
    strand::Strand,
    unpack,
    value::{Output, Slot, TypeObject},
    vm::Builder,
};

pub(crate) struct Getter;

impl<'v> Object<'v> for Getter {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "getter";
    const SLOTS: usize = 1;

    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([func], []) = unpack!(strand, args, 1, 0)?;
        this.create(strand, Getter, &mut out);
        let mut borrow = this.downcast(&out).unwrap().borrow_mut_unwrap();
        Output::set(strand, Mut::slot_mut::<0>(&mut borrow), func);
        Ok(())
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .nominal_supertype(TypeObject::Getter)
            .method_with_slots("get", async move |this, strand, args, out, [mut tmp]| {
                let ([obj], []) = unpack!(strand, args, 1, 0)?;
                let borrow = this.borrow(strand)?;
                Output::set(strand, &mut tmp, Ref::slot::<0>(&borrow));
                drop(borrow);
                call!(strand, &tmp, out, obj).await
            })
    }
}

pub(crate) struct Setter;

impl<'v> Object<'v> for Setter {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "setter";
    const SLOTS: usize = 1;

    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([func], []) = unpack!(strand, args, 1, 0)?;
        this.create(strand, Setter, &mut out);
        let mut borrow = this.downcast(&out).unwrap().borrow_mut_unwrap();
        Output::set(strand, Mut::slot_mut::<0>(&mut borrow), func);
        Ok(())
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .nominal_supertype(TypeObject::Setter)
            .method_with_slots(
                "set",
                async move |this, strand, args, mut out, [mut tmp]| {
                    let ([obj, value], []) = unpack!(strand, args, 2, 0)?;
                    let borrow = this.borrow(strand)?;
                    Output::set(strand, &mut tmp, Ref::slot::<0>(&borrow));
                    drop(borrow);
                    call!(strand, &tmp, &mut out, obj, value).await
                },
            )
    }
}

pub(crate) struct PropertyTypes<'v> {
    pub(crate) getter: Type<'v, Getter>,
    pub(crate) setter: Type<'v, Setter>,
}

pub(crate) fn register<'v>(builder: &mut Builder<'v>) -> PropertyTypes<'v> {
    PropertyTypes {
        getter: builder.register_type(),
        setter: builder.register_type(),
    }
}
