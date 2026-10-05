use std::rc::Rc;

use dolang::runtime::{
    Error, Instance, Object, Output, Result, Slot, Strand, Value,
    object::{ArrayLike, ArrayView, Spread, SpreadContext, TypeBuilder, Unpack},
    value::TypeObject,
};

pub(crate) type ArgsData = Rc<[Box<str>]>;

pub(crate) struct Args;

struct ArgsView;

impl<'v> ArrayLike<'v> for ArgsView {
    type Object = Args;
    const MODULE: &'v str = "shell";
    const NAME: &'v str = "Args";

    fn len(&self, this: Instance<'v, '_, Args>, _strand: &mut Strand<'v, '_>) -> usize {
        this.annex().len()
    }

    fn get<'a, 's>(
        &self,
        this: Instance<'v, '_, Args>,
        strand: &'a mut Strand<'v, 's>,
        index: usize,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this.annex()[index].as_ref());
        Ok(())
    }
}

impl<'v> Object<'v> for Args {
    const MODULE: &'v str = "shell";
    const NAME: &'v str = "Args";
    type Annex = ArgsData;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .supertype(TypeObject::Iterable)
            .get("len", |this, strand, out| {
                Output::set(strand, out, this.annex().len());
                Ok(())
            })
    }

    fn bool<'a, 's>(
        this: Instance<'v, 'a, Self>,
        _strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, bool> {
        Ok(!this.annex().is_empty())
    }

    fn index<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        ArrayView::index(this, ArgsView, strand, index, out)
    }

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
        ArrayView::iter(this, ArgsView, strand, out)
    }

    async fn spread<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        ArrayView::spread(this, ArgsView, strand, context, sink)
    }

    async fn unpack<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        ArrayView::unpack(this, ArgsView, strand, unpack)
    }
}
