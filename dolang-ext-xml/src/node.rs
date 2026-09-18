use dolang::runtime::object::fmt;
use dolang::runtime::value::fmt::Format;

use dolang::runtime::{
    Args, Error, Instance, Object, Output, Result, Slot, Strand, Type, Value, call,
    object::{ArrayLike, ArrayView, Mut, Ref, TypeBuilder},
    unpack,
    value::{Empty, Nil, TypeObject},
};

use crate::{
    attr::{Attr, Name, optional_string, required_string},
    global::Global,
};

pub(crate) const CHILDREN: usize = 0;
pub(crate) const ATTRS: usize = 1;
pub(crate) const NAMESPACES: usize = 2;
pub(crate) const STACK: usize = 0;
pub(crate) const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";

pub(crate) struct Node {
    pub(crate) name: Name,
}

pub(crate) struct NodeAnnex;

struct Children;
struct Attrs;

macro_rules! array_like {
    ($view:ty, $slot:expr, $name:literal) => {
        impl<'v> ArrayLike<'v> for $view {
            type Object = Node;
            const MODULE: &'v str = "xml";
            const NAME: &'v str = $name;

            fn len(&self, this: Instance<'v, '_, Node>, strand: &mut Strand<'v, '_>) -> usize {
                Ref::slot::<$slot>(&this.borrow_unwrap())
                    .as_array(strand)
                    .unwrap()
                    .len(strand)
                    .expect("conflicting XML array borrow")
            }

            fn get<'a, 's>(
                &self,
                this: Instance<'v, '_, Node>,
                strand: &'a mut Strand<'v, 's>,
                index: usize,
                out: Slot<'v, 'a>,
            ) -> Result<'v, 's, ()> {
                let found = Ref::slot::<$slot>(&this.borrow(strand)?)
                    .as_array(strand)
                    .unwrap()
                    .get(strand, index, out)?;
                debug_assert!(found);
                Ok(())
            }

            fn set<'a, 's>(
                &self,
                this: Instance<'v, '_, Node>,
                strand: &'a mut Strand<'v, 's>,
                index: usize,
                value: Slot<'v, 'a>,
            ) -> Result<'v, 's, ()> {
                let found = Ref::slot::<$slot>(&this.borrow(strand)?)
                    .as_array(strand)
                    .unwrap()
                    .set(strand, index, value)?;
                debug_assert!(found);
                Ok(())
            }

            fn push<'a, 's>(
                &self,
                this: Instance<'v, '_, Node>,
                strand: &'a mut Strand<'v, 's>,
                values: &mut [Slot<'v, 'a>],
            ) -> Result<'v, 's, ()> {
                Ref::slot::<$slot>(&this.borrow(strand)?)
                    .as_array(strand)
                    .unwrap()
                    .push_all(strand, values)
            }

            fn insert<'a, 's>(
                &self,
                this: Instance<'v, '_, Node>,
                strand: &'a mut Strand<'v, 's>,
                index: usize,
                values: &mut [Slot<'v, 'a>],
            ) -> Result<'v, 's, ()> {
                let inserted = Ref::slot::<$slot>(&this.borrow(strand)?)
                    .as_array(strand)
                    .unwrap()
                    .insert(strand, index, values)?;
                debug_assert!(inserted);
                Ok(())
            }

            fn pop<'a, 's>(
                &self,
                this: Instance<'v, '_, Node>,
                strand: &'a mut Strand<'v, 's>,
                index: usize,
                out: Slot<'v, 'a>,
            ) -> Result<'v, 's, ()> {
                let popped = Ref::slot::<$slot>(&this.borrow(strand)?)
                    .as_array(strand)
                    .unwrap()
                    .pop_at(strand, index, out)?;
                debug_assert!(popped);
                Ok(())
            }

            fn delete<'s>(
                &self,
                this: Instance<'v, '_, Node>,
                strand: &mut Strand<'v, 's>,
                index: usize,
            ) -> Result<'v, 's, ()> {
                let deleted = Ref::slot::<$slot>(&this.borrow(strand)?)
                    .as_array(strand)
                    .unwrap()
                    .delete(strand, index)?;
                debug_assert!(deleted);
                Ok(())
            }

            fn clear<'s>(
                &self,
                this: Instance<'v, '_, Node>,
                strand: &mut Strand<'v, 's>,
            ) -> Result<'v, 's, ()> {
                Ref::slot::<$slot>(&this.borrow(strand)?)
                    .as_array(strand)
                    .unwrap()
                    .clear(strand)
            }
        }
    };
}

array_like!(Children, CHILDREN, "Children");
array_like!(Attrs, ATTRS, "Attrs");

pub(crate) fn create_node<'v, 's>(
    strand: &mut Strand<'v, 's>,
    node_type: Type<'v, Node>,
    name: Name,
    mut out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let binding = name
        .namespace
        .as_ref()
        .map(|namespace| (name.prefix.clone().unwrap_or_default(), namespace.clone()));
    node_type.create_with_annex(strand, Node { name }, NodeAnnex, &mut out);
    node_type
        .cast(&out)
        .unwrap()
        .enter_sync(strand, |strand, inst| {
            let mut borrow = inst.borrow_mut_unwrap();
            Output::set(strand, Mut::slot_mut::<CHILDREN>(&mut borrow), Empty::Array);
            Output::set(strand, Mut::slot_mut::<ATTRS>(&mut borrow), Empty::Array);
            Output::set(
                strand,
                Mut::slot_mut::<NAMESPACES>(&mut borrow),
                Empty::Dict,
            );
            let namespace_slot = Mut::slot_mut::<NAMESPACES>(&mut borrow);
            let namespaces = namespace_slot.as_dict(strand).unwrap();
            namespaces.insert(strand, "xml", XML_NS, true)?;
            if let Some((prefix, namespace)) = &binding {
                namespaces.insert(strand, prefix.as_str(), namespace.as_str(), true)?;
            }
            Ok(())
        })
}

fn find_attr<'v, 's>(
    this: Instance<'v, '_, Node>,
    strand: &mut Strand<'v, 's>,
    local: &str,
    namespace: Option<&str>,
) -> Result<'v, 's, Option<(usize, String)>> {
    let global = strand.state::<Global<'v>>();
    let borrow = this.borrow(strand)?;
    let attrs = Ref::slot::<ATTRS>(&borrow).as_array(strand).unwrap();
    strand.with_slots_sync(|strand, [mut item]| {
        for index in 0..attrs.len(strand)? {
            attrs.get(strand, index, &mut item)?;
            let Some(attr) = global.attr_type.cast(&item) else {
                continue;
            };
            let found = attr.enter_sync(strand, |strand, attr| {
                let attr = attr.borrow(strand)?;
                Ok(
                    (attr.name.local == local && attr.name.namespace.as_deref() == namespace)
                        .then(|| attr.value.clone()),
                )
            })?;
            if let Some(value) = found {
                return Ok(Some((index, value)));
            }
        }
        Ok(None)
    })
}

fn append_attr<'v, 's>(
    this: Instance<'v, '_, Node>,
    strand: &mut Strand<'v, 's>,
    attr: Attr,
) -> Result<'v, 's, ()> {
    let global = strand.state::<Global<'v>>();
    let borrow = this.borrow(strand)?;
    let attrs = Ref::slot::<ATTRS>(&borrow).as_array(strand).unwrap();
    strand.with_slots_sync(|strand, [mut item]| {
        global.attr_type.create(strand, attr, &mut item);
        attrs.push(strand, &mut item)
    })
}

/// Appends the pairs of an `attrs:` dict as unnamespaced attributes.
///
/// Keys may be `Str` or `Sym`, since bareword dict keys intern as the latter;
/// values must be `Str`, matching `Attr` and `set_attr`.
fn append_attrs_dict<'v, 's>(
    this: Instance<'v, '_, Node>,
    strand: &mut Strand<'v, 's>,
    attrs: &Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let dict = attrs
        .as_dict(strand)
        .ok_or_else(|| Error::type_error(strand, "attrs: expected Dict"))?;
    let mut pairs = dict.pairs();
    loop {
        let pair = strand.with_slots_sync(|strand, [mut key, mut value]| {
            if !pairs.next(strand, &mut key, &mut value)? {
                return Ok(None);
            }
            let local = key.to_string(strand)?;
            let value = required_string(strand, &value, "attribute value")?;
            Ok(Some((local, value)))
        })?;
        let Some((local, value)) = pair else {
            return Ok(());
        };
        append_attr(
            this,
            strand,
            Attr {
                name: Name {
                    local,
                    namespace: None,
                    prefix: None,
                },
                value,
            },
        )?;
    }
}

/// Appends one variadic constructor item: an `Attr` becomes an attribute and
/// anything else becomes a child.
///
/// Children are not type-checked here, matching `push` and `children.push`:
/// `verify` and `encode` remain the single place a tree is validated. Checking
/// eagerly would also reject Do subclasses of `Node`, which cast as neither
/// `Node` nor `Str`.
fn append_item<'v, 's>(
    this: Instance<'v, '_, Node>,
    strand: &mut Strand<'v, 's>,
    item: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let global = strand.state::<Global<'v>>();
    if let Some(attr) = global.attr_type.cast(&item) {
        let attr = attr.enter_sync(strand, |strand, attr| Ok(attr.borrow(strand)?.clone()))?;
        return append_attr(this, strand, attr);
    }
    let borrow = this.borrow(strand)?;
    let children = Ref::slot::<CHILDREN>(&borrow).as_array(strand).unwrap();
    children.push(strand, item)
}

impl<'v> Object<'v> for Node {
    const MODULE: &'static str = "xml";
    const NAME: &'static str = "Node";
    const SLOTS: usize = 3;
    type Annex = NodeAnnex;
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = strand.state::<Global<'v>>();
        let namespace_sym = global.syms.namespace;
        let prefix_sym = global.syms.prefix;
        let attrs_sym = global.syms.attrs;
        let ([tag], [namespace, prefix, attrs], items) = unpack!(
            strand,
            args,
            1,
            0,
            namespace_sym = None,
            prefix_sym = None,
            attrs_sym = None,
            *
        )?;
        let name = Name {
            local: required_string(strand, &tag, "tag")?,
            namespace: optional_string(strand, namespace.as_deref(), "namespace")?,
            prefix: optional_string(strand, prefix.as_deref(), "prefix")?,
        };
        create_node(strand, this, name, Slot::reborrow(&mut out))?;
        let node = this.cast(&out).expect("freshly created node");
        node.enter_sync(strand, |strand, node| {
            // `attrs:` lands ahead of any positional `Attr`, so the bulk form
            // reads as the element's own attribute list regardless of where the
            // keyword appears in the call.
            if let Some(attrs) = attrs {
                append_attrs_dict(node, strand, &attrs)?;
            }
            for item in items {
                append_item(node, strand, item)?;
            }
            Ok(())
        })
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        fmt!(strand, w, "<xml.Node {}>", borrow.name.qname())
    }

    fn index<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if let Some(key) = index.as_str(strand) {
            let key = key.to_string();
            if let Some((_, value)) = find_attr(this, strand, &key, None)? {
                Output::set(strand, out, value.as_str());
                Ok(())
            } else {
                Err(Error::index(strand))
            }
        } else {
            ArrayView::index(this, Children, strand, index, out)
        }
    }

    async fn iter<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        ArrayView::iter(this, Children, strand, out)
    }

    fn assign<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: Slot<'v, 'a>,
        value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let key = required_string(strand, &index, "index")?;
        let value = required_string(strand, &value, "value")?;
        if let Some((attr_index, _)) = find_attr(this, strand, &key, None)? {
            let global = strand.state::<Global<'v>>();
            let borrow = this.borrow(strand)?;
            let attrs = Ref::slot::<ATTRS>(&borrow).as_array(strand).unwrap();
            strand.with_slots_sync(|strand, [mut item]| {
                attrs.get(strand, attr_index, &mut item)?;
                global
                    .attr_type
                    .cast(&item)
                    .unwrap()
                    .enter_sync(strand, |strand, attr| {
                        attr.borrow_mut(strand)?.value = value;
                        Ok(())
                    })
            })
        } else {
            append_attr(
                this,
                strand,
                Attr {
                    name: Name {
                        local: key,
                        namespace: None,
                        prefix: None,
                    },
                    value,
                },
            )
        }
    }

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let namespace_sym = builder.sym("namespace");
        let prefix_sym = builder.sym("prefix");
        let default_sym = builder.sym("default");
        let else_sym = builder.sym("else");
        builder
            .get("tag", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                Output::set(strand, out, borrow.name.local.as_str());
                Ok(())
            })
            .set("tag", |this, strand, value| {
                this.borrow_mut(strand)?.name.local = required_string(strand, &value, "tag")?;
                Ok(())
            })
            .get("namespace", |this, strand, out| {
                if let Some(namespace) = &this.borrow(strand)?.name.namespace {
                    Output::set(strand, out, namespace.as_str());
                } else {
                    Output::set(strand, out, Nil);
                }
                Ok(())
            })
            .set("namespace", |this, strand, value| {
                this.borrow_mut(strand)?.name.namespace =
                    optional_string(strand, Some(&value), "namespace")?;
                Ok(())
            })
            .get("prefix", |this, strand, out| {
                if let Some(prefix) = &this.borrow(strand)?.name.prefix {
                    Output::set(strand, out, prefix.as_str());
                } else {
                    Output::set(strand, out, Nil);
                }
                Ok(())
            })
            .set("prefix", |this, strand, value| {
                this.borrow_mut(strand)?.name.prefix =
                    optional_string(strand, Some(&value), "prefix")?;
                Ok(())
            })
            .get("qname", |this, strand, out| {
                let qname = this.borrow(strand)?.name.qname();
                Output::set(strand, out, qname.as_str());
                Ok(())
            })
            .get("attrs", |this, strand, out| {
                Output::set(strand, out, ArrayView::new(this, Attrs));
                Ok(())
            })
            .get("children", |this, strand, out| {
                Output::set(strand, out, ArrayView::new(this, Children));
                Ok(())
            })
            .get("namespaces", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                Output::set(strand, out, Ref::slot::<NAMESPACES>(&borrow));
                Ok(())
            })
            .method("attr", async move |this, strand, args, out| {
                let ([name], [namespace, default, else_value]) = unpack!(
                    strand,
                    args,
                    1,
                    0,
                    namespace_sym = None,
                    default_sym = None,
                    else_sym = None
                )?;
                if default.is_some() && else_value.is_some() {
                    return Err(Error::unexpected_key(strand, else_sym));
                }
                let name = required_string(strand, &name, "name")?;
                let namespace = optional_string(strand, namespace.as_deref(), "namespace")?;
                if let Some((_, value)) = find_attr(this, strand, &name, namespace.as_deref())? {
                    Output::set(strand, out, value.as_str());
                } else if let Some(default) = default {
                    Output::set(strand, out, default);
                } else if let Some(else_value) = else_value {
                    call!(strand, else_value, out).await?;
                } else {
                    Output::set(strand, out, Nil);
                }
                Ok(())
            })
            .method("set_attr", async move |this, strand, args, out| {
                let ([name, value], [namespace, prefix]) =
                    unpack!(strand, args, 2, 0, namespace_sym = None, prefix_sym = None)?;
                let name = required_string(strand, &name, "name")?;
                let value = required_string(strand, &value, "value")?;
                let namespace = optional_string(strand, namespace.as_deref(), "namespace")?;
                let prefix = prefix
                    .as_deref()
                    .map(|prefix| optional_string(strand, Some(prefix), "prefix"))
                    .transpose()?;
                if let Some((index, _)) = find_attr(this, strand, &name, namespace.as_deref())? {
                    let global = strand.state::<Global<'v>>();
                    let borrow = this.borrow(strand)?;
                    let attrs = Ref::slot::<ATTRS>(&borrow).as_array(strand).unwrap();
                    if let Some(prefix) = prefix {
                        strand.with_slots_sync(|strand, [mut item]| {
                            global.attr_type.create(
                                strand,
                                Attr {
                                    name: Name {
                                        local: name,
                                        namespace,
                                        prefix,
                                    },
                                    value,
                                },
                                &mut item,
                            );
                            let replaced = attrs.set(strand, index, &mut item)?;
                            debug_assert!(replaced);
                            Ok(())
                        })?;
                    } else {
                        strand.with_slots_sync(|strand, [mut item]| {
                            attrs.get(strand, index, &mut item)?;
                            global.attr_type.cast(&item).unwrap().enter_sync(
                                strand,
                                |strand, attr| {
                                    attr.borrow_mut(strand)?.value = value;
                                    Ok(())
                                },
                            )
                        })?;
                    }
                } else {
                    append_attr(
                        this,
                        strand,
                        Attr {
                            name: Name {
                                local: name,
                                namespace,
                                prefix: prefix.flatten(),
                            },
                            value,
                        },
                    )?;
                }
                Output::set(strand, out, Nil);
                Ok(())
            })
            .method("delete_attr", async move |this, strand, args, out| {
                let ([name], [namespace]) = unpack!(strand, args, 1, 0, namespace_sym = None)?;
                let name = required_string(strand, &name, "name")?;
                let namespace = optional_string(strand, namespace.as_deref(), "namespace")?;
                let global = strand.state::<Global<'v>>();
                let borrow = this.borrow(strand)?;
                let attrs = Ref::slot::<ATTRS>(&borrow).as_array(strand).unwrap();
                let deleted = strand.with_slots_sync(|strand, [mut item]| {
                    let mut deleted = false;
                    for index in (0..attrs.len(strand)?).rev() {
                        attrs.get(strand, index, &mut item)?;
                        let Some(attr) = global.attr_type.cast(&item) else {
                            continue;
                        };
                        let matches = attr.enter_sync(strand, |strand, attr| {
                            let attr = attr.borrow(strand)?;
                            Ok(attr.name.local == name && attr.name.namespace == namespace)
                        })?;
                        if matches {
                            attrs.delete(strand, index)?;
                            deleted = true;
                        }
                    }
                    Ok(deleted)
                })?;
                Output::set(strand, out, deleted);
                Ok(())
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
                global
                    .traverse_iter_type
                    .cast(&out)
                    .unwrap()
                    .enter_sync(strand, |strand, inst| {
                        {
                            let mut borrow = inst.borrow_mut_unwrap();
                            Output::set(strand, Mut::slot_mut::<STACK>(&mut borrow), Empty::Array);
                        }
                        let borrow = inst.borrow(strand)?;
                        let stack = Ref::slot::<STACK>(&borrow).as_array(strand).unwrap();
                        stack.push(strand, this)?;
                        Ok(())
                    })
            })
    }
}

/// Depth-first, parent-first traversal iterator over a Node tree.
pub(crate) struct TraverseIter;

impl<'v> Object<'v> for TraverseIter {
    const MODULE: &'static str = "xml";
    const NAME: &'static str = "TraverseIter";
    const SLOTS: usize = 1;
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.supertype(TypeObject::Iter)
    }

    async fn iter<'a, 's>(
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
            if !stack.pop(strand, &mut out)? {
                return Ok(false);
            }
            if let Some(node_cast) = global.node_type.cast(&out) {
                node_cast.enter_sync(strand, |strand, node_inst| {
                    let node_borrow = node_inst.borrow(strand)?;
                    let children = Ref::slot::<CHILDREN>(&node_borrow)
                        .as_array(strand)
                        .unwrap();
                    let children_len = children.len(strand)?;
                    for i in (0..children_len).rev() {
                        children.get(strand, i, &mut tmp)?;
                        stack.push(strand, &mut tmp)?;
                    }
                    Ok(())
                })?;
            }
            Ok(true)
        })
    }
}
