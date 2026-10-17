use crate::global::Tag;

use dolang::runtime::object::fmt;
use dolang::runtime::value::fmt::Format;

use dolang::runtime::{
    Args, Error, Instance, Object, Output, Result, Slot, Strand, Type, Value,
    object::{ArrayLike, ArrayView, DictLike, DictView, DictViewSink, Mut, Ref, TypeBuilder},
    unpack,
    value::{Empty, Nil, TypeObject},
};

use crate::{
    attr::{Attr, Name, attr_key, key_string, optional_string, required_string},
    check::{
        validate_name, validate_namespace, validate_namespace_binding, validate_ncname,
        validate_text,
    },
};

pub(crate) const CHILDREN: usize = 0;
pub(crate) const STACK: usize = 0;

pub(crate) struct Node {
    pub(crate) name: Name,
    /// Attributes in document order, with distinct expanded names.
    pub(crate) attrs: Vec<Attr>,
    /// The namespace declarations this element makes itself, by prefix.
    pub(crate) namespaces: Vec<(String, String)>,
}

pub(crate) struct NodeAnnex;

struct Children;
struct Attrs;
struct Namespaces;

/// Rejects a child other than a `Node` or valid text.
fn check_child<'v, 's>(strand: &mut Strand<'v, 's>, value: &Value<'v>) -> Result<'v, 's, ()> {
    if strand.state::<Tag>().node_type.cast(value).is_some() {
        return Ok(());
    }
    let Some(text) = value.as_str(strand) else {
        return Err(Error::type_error(
            strand,
            "XML children must be Node or Str",
        ));
    };
    let text = text.pin();
    validate_text(strand, &text, "XML text")
}

impl<'v> ArrayLike<'v> for Children {
    type Object = Node;
    const MODULE: &'v str = "xml";
    const NAME: &'v str = "Children";

    fn len(&self, this: Instance<'v, '_, Node>, strand: &mut Strand<'v, '_>) -> usize {
        Ref::slot::<CHILDREN>(&this.borrow_unwrap())
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
        let found = Ref::slot::<CHILDREN>(&this.borrow(strand)?)
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
        check_child(strand, &value)?;
        let found = Ref::slot::<CHILDREN>(&this.borrow(strand)?)
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
        for value in values.iter() {
            check_child(strand, value)?;
        }
        Ref::slot::<CHILDREN>(&this.borrow(strand)?)
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
        for value in values.iter() {
            check_child(strand, value)?;
        }
        let inserted = Ref::slot::<CHILDREN>(&this.borrow(strand)?)
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
        let popped = Ref::slot::<CHILDREN>(&this.borrow(strand)?)
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
        let deleted = Ref::slot::<CHILDREN>(&this.borrow(strand)?)
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
        Ref::slot::<CHILDREN>(&this.borrow(strand)?)
            .as_array(strand)
            .unwrap()
            .clear(strand)
    }
}

impl<'v> DictLike<'v> for Attrs {
    type Object = Node;
    const MODULE: &'v str = "xml";
    const NAME: &'v str = "Attrs";

    fn len(&self, this: Instance<'v, '_, Node>, _strand: &mut Strand<'v, '_>) -> usize {
        this.borrow_unwrap().attrs.len()
    }

    fn get<'a, 's>(
        &self,
        this: Instance<'v, '_, Node>,
        strand: &'a mut Strand<'v, 's>,
        key: &Value<'v>,
        instance: i64,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        // Expanded names are distinct.
        if !matches!(instance, 0 | -1) {
            return Ok(false);
        }
        let Some(key) = key_string(strand, key)? else {
            return Ok(false);
        };
        let node = this.borrow(strand)?;
        match find_attr(&node.attrs, &Name::from_clark(&key)) {
            Some(index) => {
                Output::set(strand, out, node.attrs[index].value.as_str());
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn set<'a, 's>(
        &self,
        this: Instance<'v, '_, Node>,
        strand: &'a mut Strand<'v, 's>,
        key: Slot<'v, 'a>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let name = attr_key(strand, &key)?;
        let value = required_string(strand, &value, "attribute value")?;
        let attr = Attr::checked(strand, name, value)?;
        set_attr(this, strand, attr)
    }

    fn delete<'s>(
        &self,
        this: Instance<'v, '_, Node>,
        strand: &mut Strand<'v, 's>,
        key: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        let Some(key) = key_string(strand, key)? else {
            return Ok(false);
        };
        let mut node = this.borrow_mut(strand)?;
        let index = find_attr(&node.attrs, &Name::from_clark(&key));
        Ok(index.map(|index| node.attrs.remove(index)).is_some())
    }

    fn pop<'a, 's>(
        &self,
        this: Instance<'v, '_, Node>,
        strand: &'a mut Strand<'v, 's>,
        key: &Value<'v>,
        instance: i64,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        if !self.get(this, strand, key, instance, Slot::reborrow(&mut out))? {
            return Ok(false);
        }
        self.delete(this, strand, key)
    }

    fn clear<'s>(
        &self,
        this: Instance<'v, '_, Node>,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, ()> {
        this.borrow_mut(strand)?.attrs.clear();
        Ok(())
    }

    fn flatten<'s>(
        &self,
        this: Instance<'v, '_, Node>,
        strand: &mut Strand<'v, 's>,
        sink: &mut DictViewSink<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let node = this.borrow(strand)?;
        for attr in &node.attrs {
            sink.push(strand, attr.name.clark().as_str(), attr.value.as_str());
        }
        Ok(())
    }
}

impl<'v> DictLike<'v> for Namespaces {
    type Object = Node;
    const MODULE: &'v str = "xml";
    const NAME: &'v str = "Namespaces";

    fn len(&self, this: Instance<'v, '_, Node>, _strand: &mut Strand<'v, '_>) -> usize {
        this.borrow_unwrap().namespaces.len()
    }

    fn get<'a, 's>(
        &self,
        this: Instance<'v, '_, Node>,
        strand: &'a mut Strand<'v, 's>,
        key: &Value<'v>,
        instance: i64,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        // Each prefix is declared at most once.
        if !matches!(instance, 0 | -1) {
            return Ok(false);
        }
        let Some(prefix) = key.as_str(strand).map(|key| key.to_string()) else {
            return Ok(false);
        };
        let node = this.borrow(strand)?;
        match find_namespace(&node.namespaces, &prefix) {
            Some(index) => {
                Output::set(strand, out, node.namespaces[index].1.as_str());
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn set<'a, 's>(
        &self,
        this: Instance<'v, '_, Node>,
        strand: &'a mut Strand<'v, 's>,
        key: Slot<'v, 'a>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let prefix = required_string(strand, &key, "namespace prefix")?;
        let namespace = required_string(strand, &value, "namespace URI")?;
        validate_namespace_binding(strand, &prefix, &namespace)?;
        let mut node = this.borrow_mut(strand)?;
        match find_namespace(&node.namespaces, &prefix) {
            Some(index) => node.namespaces[index].1 = namespace,
            None => node.namespaces.push((prefix, namespace)),
        }
        Ok(())
    }

    fn delete<'s>(
        &self,
        this: Instance<'v, '_, Node>,
        strand: &mut Strand<'v, 's>,
        key: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        let Some(prefix) = key.as_str(strand).map(|key| key.to_string()) else {
            return Ok(false);
        };
        let mut node = this.borrow_mut(strand)?;
        let index = find_namespace(&node.namespaces, &prefix);
        Ok(index.map(|index| node.namespaces.remove(index)).is_some())
    }

    fn pop<'a, 's>(
        &self,
        this: Instance<'v, '_, Node>,
        strand: &'a mut Strand<'v, 's>,
        key: &Value<'v>,
        instance: i64,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        if !self.get(this, strand, key, instance, Slot::reborrow(&mut out))? {
            return Ok(false);
        }
        self.delete(this, strand, key)
    }

    fn clear<'s>(
        &self,
        this: Instance<'v, '_, Node>,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, ()> {
        this.borrow_mut(strand)?.namespaces.clear();
        Ok(())
    }

    fn flatten<'s>(
        &self,
        this: Instance<'v, '_, Node>,
        strand: &mut Strand<'v, 's>,
        sink: &mut DictViewSink<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let node = this.borrow(strand)?;
        for (prefix, namespace) in &node.namespaces {
            sink.push(strand, prefix.as_str(), namespace.as_str());
        }
        Ok(())
    }
}

fn find_namespace(namespaces: &[(String, String)], prefix: &str) -> Option<usize> {
    namespaces.iter().position(|(key, _)| key == prefix)
}

/// Rejects `new` attributes whose expanded names repeat each other or one in
/// `existing`, ignoring the one at `replacing`.
pub(crate) fn check_new_attrs<'v, 's>(
    strand: &mut Strand<'v, 's>,
    existing: &[Attr],
    replacing: Option<usize>,
    new: &[Attr],
) -> Result<'v, 's, ()> {
    for (i, attr) in new.iter().enumerate() {
        let is = |other: &Attr| other.name.is(&attr.name);
        let clash = existing
            .iter()
            .enumerate()
            .any(|(j, other)| Some(j) != replacing && is(other))
            || new[..i].iter().any(is);
        if clash {
            return Err(Error::value(
                strand,
                format!("duplicate XML attribute {}", attr.name.clark()),
            ));
        }
    }
    Ok(())
}

fn find_attr(attrs: &[Attr], name: &Name) -> Option<usize> {
    attrs.iter().position(|attr| attr.name.is(name))
}

/// Replaces the value of the attribute with `attr`'s name, or appends `attr`.
fn set_attr<'v, 's>(
    this: Instance<'v, '_, Node>,
    strand: &mut Strand<'v, 's>,
    attr: Attr,
) -> Result<'v, 's, ()> {
    let mut node = this.borrow_mut(strand)?;
    match find_attr(&node.attrs, &attr.name) {
        Some(index) => node.attrs[index].value = attr.value,
        None => node.attrs.push(attr),
    }
    Ok(())
}

/// Creates a node; `declarations` are the namespace declarations the
/// element makes itself.
pub(crate) fn create_node<'v, 's>(
    strand: &mut Strand<'v, 's>,
    node_type: Type<'v, Node>,
    name: Name,
    attrs: Vec<Attr>,
    declarations: Vec<(String, String)>,
    mut out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let node = Node {
        name,
        attrs,
        namespaces: declarations,
    };
    node_type.create_with_annex(strand, node, NodeAnnex, &mut out);
    node_type
        .cast(&out)
        .unwrap()
        .enter_sync(strand, |strand, inst| {
            let mut borrow = inst.borrow_mut_unwrap();
            Output::set(strand, Mut::slot_mut::<CHILDREN>(&mut borrow), Empty::Array);
        });
    Ok(())
}

/// Reads the `Str` values of a dict with `Str` or `Sym` keys.
fn dict_pairs<'v, 's>(
    strand: &mut Strand<'v, 's>,
    dict: &Value<'v>,
    what: &str,
) -> Result<'v, 's, Vec<(String, String)>> {
    let dict = dict
        .as_dict(strand)
        .ok_or_else(|| Error::type_error(strand, format!("{what}: expected Dict")))?;
    let mut pairs = dict.pairs();
    let mut out = Vec::new();
    loop {
        let pair = strand.with_slots_sync(|strand, [mut key, mut value]| {
            if !pairs.next(strand, &mut key, &mut value)? {
                return Ok(None);
            }
            let Some(key) = key_string(strand, &key)? else {
                return Err(Error::type_error(
                    strand,
                    format!("{what}: expected Str or Sym keys"),
                ));
            };
            let value = required_string(strand, &value, what)?;
            Ok(Some((key, value)))
        })?;
        match pair {
            Some(pair) => out.push(pair),
            None => return Ok(out),
        }
    }
}

/// Appends one variadic constructor item: a `Dict` adds attributes keyed in
/// Clark notation, and anything else becomes a child.
fn append_item<'v, 's>(
    this: Instance<'v, '_, Node>,
    strand: &mut Strand<'v, 's>,
    item: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    if item.as_dict(strand).is_some() {
        let mut attrs = Vec::new();
        for (key, value) in dict_pairs(strand, &item, "attribute")? {
            attrs.push(Attr::checked(strand, Name::from_clark(&key), value)?);
        }
        let mut node = this.borrow_mut(strand)?;
        check_new_attrs(strand, &node.attrs, None, &attrs)?;
        node.attrs.extend(attrs);
        return Ok(());
    }
    check_child(strand, &item)?;
    let borrow = this.borrow(strand)?;
    let children = Ref::slot::<CHILDREN>(&borrow).as_array(strand).unwrap();
    children.push(strand, item)
}

impl<'v> Object<'v> for Node {
    const MODULE: &'static str = "xml";
    const NAME: &'static str = "Node";
    const SLOTS: usize = 1;
    type Annex = NodeAnnex;
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = strand.state::<Tag>();
        let namespace_sym = global.syms.namespace;
        let namespaces_sym = global.syms.namespaces;
        let ([tag], [namespace, namespaces], items) = unpack!(
            strand,
            args,
            1,
            0,
            namespace_sym = None,
            namespaces_sym = None,
            *
        )?;
        let name = Name {
            local: required_string(strand, &tag, "tag")?,
            namespace: optional_string(strand, namespace.as_deref(), "namespace")?,
        };
        validate_name(strand, &name, "element name")?;
        let mut declarations = Vec::new();
        if let Some(namespaces) = namespaces {
            for (prefix, namespace) in dict_pairs(strand, &namespaces, "namespaces")? {
                validate_namespace_binding(strand, &prefix, &namespace)?;
                match find_namespace(&declarations, &prefix) {
                    Some(index) => declarations[index].1 = namespace,
                    None => declarations.push((prefix, namespace)),
                }
            }
        }
        create_node(
            strand,
            this,
            name,
            Vec::new(),
            declarations,
            Slot::reborrow(&mut out),
        )?;
        let node = this.cast(&out).expect("freshly created node");
        node.enter_sync(strand, |strand, node| {
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
        fmt!(strand, w, "<xml.Node {}>", borrow.name.clark())
    }

    fn index<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if let Some(key) = index.as_str(strand) {
            let name = Name::from_clark(&key.to_string());
            let node = this.borrow(strand)?;
            if let Some(index) = find_attr(&node.attrs, &name) {
                Output::set(strand, out, node.attrs[index].value.as_str());
                Ok(())
            } else {
                Err(Error::index(strand))
            }
        } else {
            ArrayView::index(this, Children, strand, index, out)
        }
    }

    async fn spread<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        context: dolang::runtime::object::SpreadContext,
        sink: &'a mut dyn dolang::runtime::object::Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        ArrayView::spread(this, Children, strand, context, sink)
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
        let name = attr_key(strand, &index)?;
        let value = required_string(strand, &value, "value")?;
        let attr = Attr::checked(strand, name, value)?;
        set_attr(this, strand, attr)
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("tag", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                Output::set(strand, out, borrow.name.local.as_str());
                Ok(())
            })
            .set("tag", |this, strand, value| {
                let tag = required_string(strand, &value, "tag")?;
                validate_ncname(strand, &tag, "element name")?;
                this.borrow_mut(strand)?.name.local = tag;
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
                let namespace = optional_string(strand, Some(&value), "namespace")?;
                validate_namespace(strand, namespace.as_deref(), "element name")?;
                this.borrow_mut(strand)?.name.namespace = namespace;
                Ok(())
            })
            .get("attrs", |this, strand, out| {
                Output::set(strand, out, DictView::new(this, Attrs));
                Ok(())
            })
            .get("children", |this, strand, out| {
                Output::set(strand, out, ArrayView::new(this, Children));
                Ok(())
            })
            .get("namespaces", |this, strand, out| {
                Output::set(strand, out, DictView::new(this, Namespaces));
                Ok(())
            })
            .method("push", async move |this, strand, args, out| {
                let ([child], []) = unpack!(strand, args, 1, 0)?;
                check_child(strand, &child)?;
                let borrow = this.borrow(strand)?;
                let arr = Ref::slot::<CHILDREN>(&borrow).as_array(strand).unwrap();
                arr.push(strand, child)?;
                Output::set(strand, out, Nil);
                Ok(())
            })
            .method("traverse", async move |this, strand, args, mut out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let global = strand.state::<Tag>();
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
        let global = strand.state::<Tag>();
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
