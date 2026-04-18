use std::fmt;

use dolang::runtime::{
    Args, Error, Instance, Object, Output, Result, Slot, State, Strand, Type, Value,
    error::ResultExt,
    object::{Mut, Ref, TypeBuilder},
    unpack,
    value::{Empty, Nil, TypeObject},
};

use crate::global::Global;

pub(crate) const CHILDREN: usize = 0;
pub(crate) const STACK: usize = 0;

pub(crate) struct Node {
    pub(crate) tag: String,
    pub(crate) attrs: Vec<(String, String)>,
}

pub(crate) struct NodeAnnex<'v> {
    pub(crate) global: State<'v, Global<'v>>,
}

impl<'v> Object<'v> for Node {
    const MODULE: &'static str = "xml";
    const NAME: &'static str = "Node";
    const SLOTS: usize = 1;
    type Annex = NodeAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([tag], []) = unpack!(strand, args, 1, 0)?;
        let tag = tag
            .as_str(strand)
            .ok_or_else(|| Error::type_error(strand, "expected str"))?
            .to_string();
        let state = strand.state::<Global<'v>>();
        this.create_with_annex(
            strand,
            Node {
                tag,
                attrs: Vec::new(),
            },
            NodeAnnex { global: state },
            &mut out,
        );
        let mut borrow = this.downcast(&out).unwrap().borrow_mut_unwrap();
        Output::set(strand, Mut::slot_mut::<CHILDREN>(&mut borrow), Empty::Array);
        Ok(())
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        write!(w, "<xml.Node {}>", borrow.tag).into_do(strand)
    }

    fn index<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let key = index
            .as_str(strand)
            .ok_or_else(|| Error::type_error(strand, "index: expected str"))?;
        let borrow = this.borrow(strand)?;
        if let Some((_, val)) = strand.access(|x| {
            let key = key.as_str(x);
            borrow.attrs.iter().find(|(k, _)| k == key)
        }) {
            Output::set(strand, out, val.as_str());
            Ok(())
        } else {
            Err(Error::index(strand))
        }
    }

    fn assign<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: Slot<'v, 'a>,
        value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let key = index
            .as_str(strand)
            .ok_or_else(|| Error::type_error(strand, "index: expected str"))?
            .to_string();
        let val = value
            .as_str(strand)
            .ok_or_else(|| Error::type_error(strand, "value: expected str"))?
            .to_string();
        let mut borrow = this.borrow_mut(strand)?;
        if let Some(pair) = borrow.attrs.iter_mut().find(|(k, _)| k == &key) {
            pair.1 = val;
        } else {
            borrow.attrs.push((key, val));
        }
        Ok(())
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("tag", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                Output::set(strand, out, borrow.tag.as_str());
                Ok(())
            })
            .set("tag", |this, strand, value| {
                this.borrow_mut(strand)?.tag = value
                    .as_str(strand)
                    .ok_or_else(|| Error::type_error(strand, "tag: expected str"))?
                    .to_string();
                Ok(())
            })
            .method("attrs", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let borrow = this.borrow(strand)?;
                let attrs = borrow.attrs.clone();
                let ty = this.annex().global.attrs_iter_type;
                ty.create(strand, AttrsIter { attrs, index: 0 }, out);
                Ok(())
            })
            .method("children", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                Ref::slot::<CHILDREN>(&this.borrow(strand)?)
                    .iter(strand, out)
                    .await
            })
            .method("push", async move |this, strand, args, out| {
                let ([child], []) = unpack!(strand, args, 1, 0)?;
                let borrow = this.borrow(strand)?;
                let arr = Ref::slot::<CHILDREN>(&borrow).as_array(strand).unwrap();
                arr.push(strand, child)?;
                Output::set(strand, out, Nil);
                Ok(())
            })
            .method("traverse", async move |this, strand, args, mut out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let global = strand.state::<Global<'v>>();
                global
                    .traverse_iter_type
                    .create(strand, TraverseIter, &mut out);
                let mut borrow = global
                    .traverse_iter_type
                    .downcast(&out)
                    .unwrap()
                    .borrow_mut_unwrap();
                Output::set(strand, Mut::slot_mut::<STACK>(&mut borrow), Empty::Array);
                drop(borrow);
                let borrow = global
                    .traverse_iter_type
                    .downcast(&out)
                    .unwrap()
                    .borrow(strand)?;
                let stack = Ref::slot::<STACK>(&borrow).as_array(strand).unwrap();
                stack.push(strand, this)?;
                Ok(())
            })
    }
}

/// Depth-first, parent-first traversal iterator over a Node tree.
pub(crate) struct TraverseIter;

impl<'v> Object<'v> for TraverseIter {
    const MODULE: &'static str = "xml";
    const NAME: &'static str = "TraverseIter";
    const SLOTS: usize = 1; // STACK: GC array of pending nodes/values
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.supertype(TypeObject::Iter)
    }

    async fn input<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
        Ok(())
    }

    async fn next<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let global = strand.state::<Global<'v>>();
        let borrow = this.borrow(strand)?;
        let stack = Ref::slot::<STACK>(&borrow).as_array(strand).unwrap();
        strand.with_slots_sync(|strand, [mut tmp]| {
            // Pop the top of the stack into `out`.
            if !stack.pop(strand, &mut out)? {
                return Ok(false);
            }
            // If it's a Node, push its children in reverse so first child is next.
            if let Some(node_inst) = global.node_type.downcast(&out) {
                let node_borrow = node_inst.borrow(strand)?;
                let children = Ref::slot::<CHILDREN>(&node_borrow)
                    .as_array(strand)
                    .unwrap();
                let children_len = children.len(strand)?;
                for i in (0..children_len).rev() {
                    children.get(strand, i, &mut tmp)?;
                    stack.push(strand, &mut tmp)?;
                }
            }
            Ok(true)
        })
    }
}

/// Lazy iterator over a node's attributes, yielding `[key, val]` arrays per item.
pub(crate) struct AttrsIter {
    pub(crate) attrs: Vec<(String, String)>,
    pub(crate) index: usize,
}

impl<'v> Object<'v> for AttrsIter {
    const MODULE: &'static str = "xml";
    const NAME: &'static str = "AttrsIter";
    const SLOTS: usize = 0;
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.supertype(TypeObject::Iter)
    }

    async fn input<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
        Ok(())
    }

    async fn next<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let mut borrow = this.borrow_mut(strand)?;
        let idx = borrow.index;
        if idx >= borrow.attrs.len() {
            return Ok(false);
        }
        let (key, val) = &borrow.attrs[idx];
        // Yield a 2-element array [key, val] so callers can do `for k v = node.attrs()`
        Output::set(strand, &mut out, Empty::Array);
        let arr = out.as_array(strand).unwrap();
        arr.push(strand, key.as_str()).unwrap();
        arr.push(strand, val.as_str()).unwrap();
        borrow.index += 1;
        Ok(true)
    }
}
