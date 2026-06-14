use std::str;

use quick_xml::{
    Reader, Writer, XmlVersion,
    escape::{escape, resolve_predefined_entity},
    events::{BytesEnd, BytesStart, BytesText, Event},
};

use dolang::runtime::{
    Output, Slot, State, Strand, Value,
    error::{Error, Result, ResultExt},
    object::{Mut, Ref},
    unpack,
    value::Empty,
    vm::Builder,
};

use crate::{
    global::Global,
    node::{CHILDREN, Node, NodeAnnex},
};

pub(crate) fn configure<'v>(builder: &mut Builder<'v>, state: State<'v, Global<'v>>) {
    builder
        .module("xml")
        .function("from_str", async move |strand, args, mut out| {
            let ([arg], []) = unpack!(strand, args, 1, 0)?;
            let src = arg
                .as_str(strand.vm())
                .ok_or_else(|| Error::type_error(strand, "expected str"))?
                .pin();

            let mut reader = Reader::from_str(&src);
            reader.config_mut().trim_text(false);

            loop {
                match reader.read_event().into_do(strand)? {
                    Event::Start(e) => {
                        let e = e.into_owned();
                        parse_element(strand, &mut reader, &e, state, &mut out)?;
                        break;
                    }
                    Event::Empty(e) => {
                        let e = e.into_owned();
                        create_node(strand, &e, state, &mut out)?;
                        break;
                    }
                    Event::Eof => {
                        return Err(Error::type_error(strand, "no root XML element"));
                    }
                    _ => {}
                }
            }
            Ok(())
        })
        .function("to_str", async move |strand, args, out| {
            let ([arg], []) = unpack!(strand, args, 1, 0)?;
            let mut writer = Writer::new(Vec::new());
            serialize_node(strand, &mut writer, &arg, state)?;
            let bytes = writer.into_inner();
            let s = str::from_utf8(&bytes).into_do(strand)?;
            Output::set(strand, out, s);
            Ok(())
        })
        .value("Node", state.node_type)
        .commit();
}

/// Parse attributes from a `BytesStart` and create a new Node object in `out`.
fn create_node<'v, 's>(
    strand: &mut Strand<'v, 's>,
    start: &BytesStart<'_>,
    state: State<'v, Global<'v>>,
    out: &mut Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let tag = str::from_utf8(start.name().as_ref())
        .into_do(strand)?
        .to_owned();
    let mut attrs = Vec::new();
    for attr in start.attributes().flatten() {
        let key = str::from_utf8(attr.key.into_inner())
            .into_do(strand)?
            .to_owned();
        let val = attr
            .normalized_value(XmlVersion::Implicit1_0)
            .into_do(strand)?
            .into_owned();
        attrs.push((key, val));
    }
    create_node_inner(strand, tag, attrs, state, out)
}

/// Create a Node object with the given tag, attrs, and an empty children array.
fn create_node_inner<'v, 's>(
    strand: &mut Strand<'v, 's>,
    tag: String,
    attrs: Vec<(String, String)>,
    state: State<'v, Global<'v>>,
    out: &mut Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    state.node_type.create_with_annex(
        strand,
        Node { tag, attrs },
        NodeAnnex { global: state },
        &mut *out,
    );
    let mut borrow = state.node_type.downcast(&*out).unwrap().borrow_mut_unwrap();
    Output::set(strand, Mut::slot_mut::<CHILDREN>(&mut borrow), Empty::Array);
    Ok(())
}

/// Parse a full element (Start already consumed) including child events into a Node.
fn parse_element<'v, 's>(
    strand: &mut Strand<'v, 's>,
    reader: &mut Reader<&[u8]>,
    start: &BytesStart<'_>,
    state: State<'v, Global<'v>>,
    out: &mut Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    strand.with_slots_sync(|strand, [mut child]| {
        create_node(strand, start, state, out)?;

        // Obtain a shared borrow of the node to access the children array.
        // Ref<'v, 'a, Node> does not borrow from `strand`, so it can be held
        // across strand method calls throughout the loop.
        let inst = state.node_type.downcast(&*out).unwrap();
        let borrow = inst.borrow(strand)?;
        let arr = Ref::slot::<CHILDREN>(&borrow).as_array(strand).unwrap();

        loop {
            match reader.read_event().into_do(strand)? {
                Event::Start(e) => {
                    let e = e.into_owned();
                    parse_element(strand, reader, &e, state, &mut child)?;
                    arr.push(strand, &mut child)?
                }
                Event::Empty(e) => {
                    let e = e.into_owned();
                    create_node(strand, &e, state, &mut child)?;
                    arr.push(strand, &mut child)?
                }
                Event::Text(t) => {
                    let text = t
                        .xml_content(XmlVersion::Implicit1_0)
                        .into_do(strand)?
                        .into_owned();
                    if !text.is_empty() {
                        arr.push(strand, text.as_str())?;
                    }
                }
                Event::CData(cd) => {
                    let text = str::from_utf8(cd.as_ref()).into_do(strand)?.to_owned();
                    if !text.is_empty() {
                        arr.push(strand, text.as_str())?;
                    }
                }
                Event::GeneralRef(ent) => {
                    if let Some(text) = resolve_predefined_entity(&ent.decode().into_do(strand)?)
                        && !text.is_empty()
                    {
                        arr.push(strand, text)?;
                    } else {
                        return Err(Error::runtime(
                            strand,
                            "non-predefined entities not supported",
                        ));
                    }
                }
                Event::End(_) | Event::Eof => break,
                _ => {}
            }
        }

        Ok(())
    })
}

/// Serialize a Do value as an XML node.  Accepts an `xml.Node` or a bare `str`.
fn serialize_node<'v, 's>(
    strand: &mut Strand<'v, 's>,
    writer: &mut Writer<Vec<u8>>,
    value: &Value<'v>,
    state: State<'v, Global<'v>>,
) -> Result<'v, 's, ()> {
    if let Some(inst) = state.node_type.downcast(value) {
        strand.with_slots_sync(|strand, [mut child]| {
            let borrow = inst.borrow(strand)?;
            let tag = borrow.tag.clone();
            let attrs = borrow.attrs.clone();
            let arr = Ref::slot::<CHILDREN>(&borrow).as_array(strand).unwrap();

            let mut start = BytesStart::new(tag.as_str());
            for (k, v) in &attrs {
                let escaped_v = escape(v.as_str());
                start.push_attribute((k.as_str(), escaped_v.as_ref()));
            }
            writer
                .write_event(Event::Start(start.to_owned()))
                .into_do(strand)?;

            let len = arr.len(strand)?;
            for i in 0..len {
                if arr.get(strand, i, &mut child)? {
                    serialize_node(strand, writer, &child, state)?;
                }
            }

            writer
                .write_event(Event::End(BytesEnd::new(tag.as_str())))
                .into_do(strand)
        })
    } else {
        let str = value
            .as_str(strand.vm())
            .ok_or_else(|| Error::type_error(strand, "expected xml.Node or str"))?
            .pin();
        writer
            .write_event(Event::Text(BytesText::new(&str)))
            .into_do(strand)
    }
}
