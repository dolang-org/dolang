use crate::{
    arg::Args,
    call,
    error::{Error, Result},
    object::native::{Mut, Object, Ref, Type, TypeBuilder},
    strand::Strand,
    unpack,
    value::{Output, Slot, TypeObject},
    vm::Builder,
};

pub(crate) struct Property;

impl<'v> Object<'v> for Property {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "property";
    const SLOTS: usize = 2;

    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([getter], [setter]) = unpack!(strand, args, 1, 1)?;
        this.create(strand, Property, &mut out);
        let mut borrow = this.downcast(&out).unwrap().borrow_mut_unwrap();
        Output::set(strand, Mut::slot_mut::<0>(&mut borrow), getter);
        if let Some(setter) = setter {
            Output::set(strand, Mut::slot_mut::<1>(&mut borrow), setter);
        }
        Ok(())
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .nominal_supertype(TypeObject::Descriptor)
            .method_with_slots("get", async move |this, strand, args, out, [mut tmp]| {
                let ([obj], []) = unpack!(strand, args, 1, 0)?;
                let borrow = this.borrow(strand)?;
                Output::set(strand, &mut tmp, Ref::slot::<0>(&borrow));
                drop(borrow);
                call!(strand, &tmp, out, obj).await
            })
            .method_with_slots(
                "set",
                async move |this, strand, args, mut out, [mut tmp]| {
                    let ([obj, value], []) = unpack!(strand, args, 2, 0)?;
                    let borrow = this.borrow(strand)?;
                    Output::set(strand, &mut tmp, Ref::slot::<1>(&borrow));
                    drop(borrow);
                    if tmp.is_nil() {
                        return Err(Error::type_error(strand, "property has no setter"));
                    }
                    call!(strand, &tmp, &mut out, obj, value).await
                },
            )
            .method("setter", async move |this, strand, args, mut out| {
                let ([func], []) = unpack!(strand, args, 1, 0)?;
                let mut borrow = this.borrow_mut(strand)?;
                Output::set(strand, Mut::slot_mut::<1>(&mut borrow), func);
                drop(borrow);
                Output::set(strand, &mut out, this);
                Ok(())
            })
    }
}

pub(crate) fn register<'v>(builder: &mut Builder<'v>) -> Type<'v, Property> {
    builder.register_type()
}
