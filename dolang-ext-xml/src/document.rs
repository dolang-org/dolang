use crate::{check::validate_text, global::Tag};
use dolang::runtime::{
    Args, Error, Instance, Object, Output, Result, Slot, Strand, Type, Value,
    object::{ArrayLike, ArrayView, Mut, Ref, TypeBuilder},
    unpack,
    value::Empty,
};
pub(crate) const CHILDREN: usize = 0;
pub(crate) struct Document;
struct Children;

fn child_root<'v, 's>(strand: &mut Strand<'v, 's>, value: &Value<'v>) -> Result<'v, 's, usize> {
    let global = strand.state::<Tag>();
    if global.element_type.cast(value).is_some() {
        return Ok(1);
    }
    if global.comment_type.cast(value).is_some() {
        return Ok(0);
    }
    let text = value
        .as_str(strand)
        .ok_or_else(|| Error::type_error(strand, "expected Element, Comment, or Str"))?
        .pin();
    validate_text(strand, &text, "XML document text")?;
    if !text
        .bytes()
        .all(|c| matches!(c, b' ' | b'\t' | b'\r' | b'\n'))
    {
        return Err(Error::value(
            strand,
            "document text must contain only XML whitespace",
        ));
    }
    Ok(0)
}

fn check_replacement<'v, 's>(
    this: Instance<'v, '_, Document>,
    strand: &mut Strand<'v, 's>,
    replaced: Option<usize>,
    values: &[Slot<'v, '_>],
) -> Result<'v, 's, ()> {
    let borrow = this.borrow(strand)?;
    let children = Ref::slot::<CHILDREN>(&borrow).as_array(strand).unwrap();
    let mut roots = 0;
    strand.with_slots_sync(|strand, [mut child]| {
        for i in 0..children.len(strand)? {
            if Some(i) != replaced {
                children.get(strand, i, &mut child)?;
                roots += child_root(strand, &child)?;
            }
        }
        for value in values {
            roots += child_root(strand, value)?;
        }
        if roots != 1 {
            return Err(Error::value(
                strand,
                "XML document must contain exactly one root element",
            ));
        }
        Ok(())
    })
}
fn check_removal<'v, 's>(
    this: Instance<'v, '_, Document>,
    strand: &mut Strand<'v, 's>,
    index: usize,
) -> Result<'v, 's, ()> {
    let borrow = this.borrow(strand)?;
    let children = Ref::slot::<CHILDREN>(&borrow).as_array(strand).unwrap();
    strand.with_slots_sync(|strand, [mut child]| {
        children.get(strand, index, &mut child)?;
        if child_root(strand, &child)? == 1 {
            return Err(Error::state_error(
                strand,
                "cannot remove the XML document root",
            ));
        }
        Ok(())
    })
}

pub(crate) fn create_document<'v, 's>(
    strand: &mut Strand<'v, 's>,
    ty: Type<'v, Document>,
    children: &Value<'v>,
    mut out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let array = children.as_array(strand).unwrap();
    let mut roots = 0;
    strand.with_slots_sync(|strand, [mut child]| {
        for i in 0..array.len(strand)? {
            array.get(strand, i, &mut child)?;
            roots += child_root(strand, &child)?;
        }
        Ok(())
    })?;
    if roots != 1 {
        return Err(Error::value(
            strand,
            "XML document must contain exactly one root element",
        ));
    }
    ty.create(strand, Document, &mut out);
    ty.cast(&out).unwrap().enter_sync(strand, |strand, this| {
        Output::set(
            strand,
            Mut::slot_mut::<CHILDREN>(&mut this.borrow_mut_unwrap()),
            children,
        );
    });
    Ok(())
}
impl<'v> ArrayLike<'v> for Children {
    type Object = Document;
    const MODULE: &'v str = "xml";
    const NAME: &'v str = "Children";

    fn len(&self, this: Instance<'v, '_, Document>, strand: &mut Strand<'v, '_>) -> usize {
        Ref::slot::<CHILDREN>(&this.borrow_unwrap())
            .as_array(strand)
            .unwrap()
            .len(strand)
            .expect("conflicting XML array borrow")
    }

    fn get<'a, 's>(
        &self,
        this: Instance<'v, '_, Document>,
        strand: &'a mut Strand<'v, 's>,
        index: usize,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let found = Ref::slot::<CHILDREN>(&this.borrow(strand)?)
            .as_array(strand)
            .unwrap()
            .get(strand, index, out)?;
        debug_assert!(found);
        Ok(())
    }

    fn set<'a, 's>(
        &self,
        this: Instance<'v, '_, Document>,
        strand: &'a mut Strand<'v, 's>,
        index: usize,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        check_replacement(this, strand, Some(index), std::slice::from_ref(&value))?;
        let found = Ref::slot::<CHILDREN>(&this.borrow(strand)?)
            .as_array(strand)
            .unwrap()
            .set(strand, index, value)?;
        debug_assert!(found);
        Ok(())
    }

    fn push<'a, 's>(
        &self,
        this: Instance<'v, '_, Document>,
        strand: &'a mut Strand<'v, 's>,
        values: &mut [Slot<'v, 'a>],
    ) -> Result<'v, 's, ()> {
        check_replacement(this, strand, None, values)?;
        Ref::slot::<CHILDREN>(&this.borrow(strand)?)
            .as_array(strand)
            .unwrap()
            .push_all(strand, values)
    }

    fn insert<'a, 's>(
        &self,
        this: Instance<'v, '_, Document>,
        strand: &'a mut Strand<'v, 's>,
        index: usize,
        values: &mut [Slot<'v, 'a>],
    ) -> Result<'v, 's, ()> {
        check_replacement(this, strand, None, values)?;
        let inserted = Ref::slot::<CHILDREN>(&this.borrow(strand)?)
            .as_array(strand)
            .unwrap()
            .insert(strand, index, values)?;
        debug_assert!(inserted);
        Ok(())
    }

    fn pop<'a, 's>(
        &self,
        this: Instance<'v, '_, Document>,
        strand: &'a mut Strand<'v, 's>,
        index: usize,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        check_removal(this, strand, index)?;
        let popped = Ref::slot::<CHILDREN>(&this.borrow(strand)?)
            .as_array(strand)
            .unwrap()
            .pop_at(strand, index, out)?;
        debug_assert!(popped);
        Ok(())
    }

    fn delete<'s>(
        &self,
        this: Instance<'v, '_, Document>,
        strand: &mut Strand<'v, 's>,
        index: usize,
    ) -> Result<'v, 's, ()> {
        check_removal(this, strand, index)?;
        let deleted = Ref::slot::<CHILDREN>(&this.borrow(strand)?)
            .as_array(strand)
            .unwrap()
            .delete(strand, index)?;
        debug_assert!(deleted);
        Ok(())
    }

    fn clear<'s>(
        &self,
        this: Instance<'v, '_, Document>,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let _ = this;
        Err(Error::state_error(strand, "cannot clear an XML document"))
    }
}

impl<'v> Object<'v> for Document {
    const MODULE: &'static str = "xml";
    const NAME: &'static str = "Document";
    const SLOTS: usize = 1;
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();
    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([], [], items) = unpack!(strand, args, 0, 0, *)?;
        strand.with_slots_sync(|strand, [mut children]| {
            Output::set(strand, &mut children, Empty::Array);
            let array = children.as_array(strand).unwrap();
            for item in items {
                array.push(strand, item)?;
            }
            create_document(strand, this, &children, out)
        })
    }
    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("children", |this, strand, out| {
                Output::set(strand, out, ArrayView::new(this, Children));
                Ok(())
            })
            .get("root", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                let array = Ref::slot::<CHILDREN>(&borrow).as_array(strand).unwrap();
                strand.with_slots_sync(|strand, [mut child]| {
                    for i in 0..array.len(strand)? {
                        array.get(strand, i, &mut child)?;
                        if strand.state::<Tag>().element_type.cast(&child).is_some() {
                            Output::set(strand, out, &child);
                            return Ok(());
                        }
                    }
                    unreachable!("document invariant")
                })
            })
            .set("root", |this, strand, value| {
                if strand.state::<Tag>().element_type.cast(&value).is_none() {
                    return Err(Error::type_error(strand, "root must be an Element"));
                }
                let borrow = this.borrow(strand)?;
                let array = Ref::slot::<CHILDREN>(&borrow).as_array(strand).unwrap();
                strand.with_slots_sync(|strand, [mut child]| {
                    for i in 0..array.len(strand)? {
                        array.get(strand, i, &mut child)?;
                        if strand.state::<Tag>().element_type.cast(&child).is_some() {
                            array.set(strand, i, value)?;
                            return Ok(());
                        }
                    }
                    unreachable!("document invariant")
                })
            })
    }
}
