use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    num::NonZero,
    str,
};

use quick_xml::{
    NsReader, Writer, XmlVersion,
    escape::{escape, resolve_predefined_entity},
    events::{BytesEnd, BytesStart, BytesText, Event},
    name::{PrefixDeclaration, ResolveResult},
};

use dolang::runtime::{
    Output, Slot, State, Strand, Value,
    error::{Error, Result, ResultExt},
    object::Ref,
    unpack,
    value::{Array, Nil, View},
    vm::Register,
};

use crate::{
    attr::{Attr, Name},
    global::Global,
    node::{ATTRS, CHILDREN, NAMESPACES, Node, XML_NS, create_node},
};

const XMLNS_NS: &str = "http://www.w3.org/2000/xmlns/";

struct ParsedElement {
    name: Name,
    attrs: Vec<Attr>,
    namespaces: Vec<(String, String)>,
}

struct NamespaceSnapshot {
    ordered: Vec<(String, String)>,
    map: HashMap<String, String>,
}

#[derive(Default)]
struct WalkState {
    ancestry: HashSet<NonZero<usize>>,
    generated: usize,
}

pub(crate) fn configure<'v>(builder: &mut Register<'v>, state: State<'v, Global<'v>>) {
    builder
        .module("xml")
        .function("decode", async move |strand, args, mut out| {
            let ([arg], []) = unpack!(strand, args, 1, 0)?;
            let src = arg
                .as_str(strand.vm())
                .ok_or_else(|| Error::type_error(strand, "expected Str"))?
                .pin();
            let mut reader = NsReader::from_str(&src);
            reader.config_mut().trim_text(false);

            loop {
                match reader.read_event().into_do(strand)? {
                    Event::Start(start) => {
                        let start = start.into_owned();
                        parse_element(strand, &mut reader, &start, state, &mut out)?;
                        break;
                    }
                    Event::Empty(start) => {
                        let start = start.into_owned();
                        create_parsed_node(strand, &reader, &start, state, &mut out)?;
                        break;
                    }
                    Event::Eof => return Err(Error::value(strand, "no root XML element")),
                    _ => {}
                }
            }
            verify_value(strand, &out, state)?;
            Ok(())
        })
        .function("encode", async move |strand, args, out| {
            let ([arg], []) = unpack!(strand, args, 1, 0)?;
            let mut writer = Some(Writer::new(Vec::new()));
            let mut walk = WalkState::default();
            walk_value(strand, &arg, state, &HashMap::new(), &mut walk, &mut writer)?;
            let bytes = writer.unwrap().into_inner();
            let text = str::from_utf8(&bytes).into_do(strand)?;
            Output::set(strand, out, text);
            Ok(())
        })
        .function("verify", async move |strand, args, out| {
            let ([arg], []) = unpack!(strand, args, 1, 0)?;
            if state.node_type.cast(&arg).is_none() {
                return Err(Error::type_error(strand, "expected xml.Node"));
            }
            verify_value(strand, &arg, state)?;
            Output::set(strand, out, Nil);
            Ok(())
        })
        .value("Attr", state.attr_type)
        .value("Node", state.node_type)
        .commit();
}

fn bytes_string<'v, 's>(strand: &mut Strand<'v, 's>, bytes: &[u8]) -> Result<'v, 's, String> {
    Ok(str::from_utf8(bytes).into_do(strand)?.to_owned())
}

fn resolved_namespace<'v, 's>(
    strand: &mut Strand<'v, 's>,
    resolved: ResolveResult<'_>,
) -> Result<'v, 's, Option<String>> {
    match resolved {
        ResolveResult::Unbound => Ok(None),
        ResolveResult::Bound(namespace) => bytes_string(strand, namespace.as_ref()).map(Some),
        ResolveResult::Unknown(prefix) => Err(Error::value(
            strand,
            format!(
                "unknown XML namespace prefix {}",
                String::from_utf8_lossy(&prefix)
            ),
        )),
    }
}

fn parse_start<'v, 's>(
    strand: &mut Strand<'v, 's>,
    reader: &NsReader<&[u8]>,
    start: &BytesStart<'_>,
) -> Result<'v, 's, ParsedElement> {
    let resolver = reader.resolver();
    let qname = start.name();
    let (resolved, local) = resolver.resolve_element(qname);
    let name = Name {
        local: bytes_string(strand, local.as_ref())?,
        namespace: resolved_namespace(strand, resolved)?,
        prefix: qname
            .prefix()
            .map(|prefix| bytes_string(strand, prefix.as_ref()))
            .transpose()?,
    };

    let mut attrs = Vec::new();
    for attr in start.attributes() {
        let attr = attr.into_do(strand)?;
        if attr.key.as_namespace_binding().is_some() {
            continue;
        }
        let (resolved, local) = resolver.resolve_attribute(attr.key);
        attrs.push(Attr {
            name: Name {
                local: bytes_string(strand, local.as_ref())?,
                namespace: resolved_namespace(strand, resolved)?,
                prefix: attr
                    .key
                    .prefix()
                    .map(|prefix| bytes_string(strand, prefix.as_ref()))
                    .transpose()?,
            },
            value: attr
                .normalized_value(XmlVersion::Implicit1_0)
                .into_do(strand)?
                .into_owned(),
        });
    }

    let mut namespaces = vec![("xml".to_owned(), XML_NS.to_owned())];
    for (prefix, namespace) in resolver.bindings() {
        let prefix = match prefix {
            PrefixDeclaration::Default => String::new(),
            PrefixDeclaration::Named(prefix) => bytes_string(strand, prefix)?,
        };
        namespaces.push((prefix, bytes_string(strand, namespace.as_ref())?));
    }
    Ok(ParsedElement {
        name,
        attrs,
        namespaces,
    })
}

fn create_parsed_node<'v, 's>(
    strand: &mut Strand<'v, 's>,
    reader: &NsReader<&[u8]>,
    start: &BytesStart<'_>,
    state: State<'v, Global<'v>>,
    out: &mut Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let parsed = parse_start(strand, reader, start)?;
    create_node(strand, state.node_type, parsed.name, Slot::reborrow(out))?;
    state
        .node_type
        .cast(&*out)
        .unwrap()
        .enter_sync(strand, |strand, node| {
            let borrow = node.borrow(strand)?;
            let attrs = Ref::slot::<ATTRS>(&borrow).as_array(strand).unwrap();
            let namespaces = Ref::slot::<NAMESPACES>(&borrow).as_dict(strand).unwrap();
            for (prefix, namespace) in &parsed.namespaces {
                namespaces.insert(strand, prefix.as_str(), namespace.as_str(), true)?;
            }
            strand.with_slots_sync(|strand, [mut item]| {
                for attr in parsed.attrs {
                    state.attr_type.create(strand, attr, &mut item);
                    attrs.push(strand, &mut item)?;
                }
                Ok(())
            })
        })
}

fn append_text<'v, 's>(
    strand: &mut Strand<'v, 's>,
    children: &Array<'v, '_>,
    scratch: &mut Slot<'v, '_>,
    text: &str,
) -> Result<'v, 's, ()> {
    if text.is_empty() {
        return Ok(());
    }
    let len = children.len(strand)?;
    if len > 0 {
        children.get(strand, len - 1, Slot::reborrow(scratch))?;
        if let Some(previous) = scratch.as_str(strand) {
            let mut combined = previous.to_string();
            combined.push_str(text);
            children.set(strand, len - 1, combined.as_str())?;
            return Ok(());
        }
    }
    children.push(strand, text)
}

fn parse_element<'v, 's>(
    strand: &mut Strand<'v, 's>,
    reader: &mut NsReader<&[u8]>,
    start: &BytesStart<'_>,
    state: State<'v, Global<'v>>,
    out: &mut Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    create_parsed_node(strand, reader, start, state, out)?;
    strand.with_slots_sync(|strand, [mut child]| {
        state
            .node_type
            .cast(&*out)
            .unwrap()
            .enter_sync(strand, |strand, node| {
                let borrow = node.borrow(strand)?;
                let children = Ref::slot::<CHILDREN>(&borrow).as_array(strand).unwrap();
                loop {
                    match reader.read_event().into_do(strand)? {
                        Event::Start(start) => {
                            let start = start.into_owned();
                            parse_element(strand, reader, &start, state, &mut child)?;
                            children.push(strand, &mut child)?;
                        }
                        Event::Empty(start) => {
                            let start = start.into_owned();
                            create_parsed_node(strand, reader, &start, state, &mut child)?;
                            children.push(strand, &mut child)?;
                        }
                        Event::Text(text) => {
                            let text = text
                                .xml_content(XmlVersion::Implicit1_0)
                                .into_do(strand)?
                                .into_owned();
                            append_text(strand, &children, &mut child, &text)?;
                        }
                        Event::CData(text) => {
                            let text = bytes_string(strand, text.as_ref())?;
                            append_text(strand, &children, &mut child, &text)?;
                        }
                        Event::GeneralRef(reference) => {
                            if let Some(character) = reference.resolve_char_ref().into_do(strand)? {
                                let mut buffer = [0; 4];
                                append_text(
                                    strand,
                                    &children,
                                    &mut child,
                                    character.encode_utf8(&mut buffer),
                                )?;
                            } else {
                                let decoded = reference.decode().into_do(strand)?;
                                if let Some(text) = resolve_predefined_entity(&decoded) {
                                    append_text(strand, &children, &mut child, text)?;
                                } else {
                                    return Err(Error::value(
                                        strand,
                                        "custom XML entities are not supported",
                                    ));
                                }
                            }
                        }
                        Event::End(_) => break,
                        Event::Eof => {
                            return Err(Error::value(strand, "unexpected end of XML document"));
                        }
                        _ => {}
                    }
                }
                Ok(())
            })
    })
}

fn verify_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    state: State<'v, Global<'v>>,
) -> Result<'v, 's, ()> {
    let mut writer = None;
    let mut walk = WalkState::default();
    walk_value(
        strand,
        value,
        state,
        &HashMap::new(),
        &mut walk,
        &mut writer,
    )
}

fn value_error<'v, 's>(
    strand: &mut Strand<'v, 's>,
    message: impl Into<Cow<'v, str>>,
) -> Error<'v, 's> {
    Error::value(strand, message)
}

fn read_namespaces<'v, 's>(
    strand: &mut Strand<'v, 's>,
    node: dolang::runtime::Instance<'v, '_, Node>,
) -> Result<'v, 's, NamespaceSnapshot> {
    let borrow = node.borrow(strand)?;
    let namespace_value = Ref::slot::<NAMESPACES>(&borrow);
    let namespaces = namespace_value
        .as_dict(strand)
        .ok_or_else(|| value_error(strand, "Node.namespaces must be a dict"))?;
    let mut pairs = namespaces.pairs();
    let mut ordered = Vec::new();
    let mut map = HashMap::new();
    strand.with_slots_sync(|strand, [mut key, mut value]| {
        while pairs.next(strand, &mut key, &mut value)? {
            let key = key
                .as_str(strand)
                .ok_or_else(|| value_error(strand, "namespace prefix must be a str"))?
                .to_string();
            let value = value
                .as_str(strand)
                .ok_or_else(|| value_error(strand, "namespace URI must be a str"))?
                .to_string();
            if map.insert(key.clone(), value.clone()).is_some() {
                return Err(value_error(
                    strand,
                    format!("duplicate namespace prefix {key:?}"),
                ));
            }
            validate_namespace_binding(strand, &key, &value)?;
            ordered.push((key, value));
        }
        Ok(())
    })?;
    if map.get("xml").map(String::as_str) != Some(XML_NS) {
        return Err(value_error(
            strand,
            "the xml prefix must map to the XML namespace",
        ));
    }
    Ok(NamespaceSnapshot { ordered, map })
}

fn validate_namespace_binding<'v, 's>(
    strand: &mut Strand<'v, 's>,
    prefix: &str,
    namespace: &str,
) -> Result<'v, 's, ()> {
    validate_text(strand, namespace, "namespace URI")?;
    if prefix == "xmlns" || namespace == XMLNS_NS {
        return Err(value_error(strand, "the xmlns namespace is reserved"));
    }
    if prefix == "xml" && namespace != XML_NS {
        return Err(value_error(
            strand,
            "the xml prefix must map to the XML namespace",
        ));
    }
    if namespace == XML_NS && prefix != "xml" {
        return Err(value_error(
            strand,
            "the XML namespace must use the xml prefix",
        ));
    }
    if !prefix.is_empty() {
        validate_ncname(strand, prefix, "namespace prefix")?;
        if namespace.is_empty() {
            return Err(value_error(
                strand,
                "a named namespace prefix cannot map to an empty URI",
            ));
        }
    }
    Ok(())
}

fn validate_name<'v, 's>(
    strand: &mut Strand<'v, 's>,
    name: &Name,
    kind: &str,
) -> Result<'v, 's, ()> {
    validate_ncname(strand, &name.local, kind)?;
    if let Some(prefix) = &name.prefix {
        validate_ncname(strand, prefix, "prefix")?;
        if name.namespace.is_none() {
            return Err(value_error(
                strand,
                format!("{kind} has a prefix but no namespace"),
            ));
        }
    }
    if name.namespace.as_deref() == Some("") {
        return Err(value_error(
            strand,
            format!("{kind} has an empty namespace URI"),
        ));
    }
    if name.namespace.as_deref() == Some(XMLNS_NS) {
        return Err(value_error(
            strand,
            format!("{kind} uses the reserved xmlns namespace"),
        ));
    }
    if name.namespace.as_deref() == Some(XML_NS) && name.prefix.as_deref() != Some("xml") {
        return Err(value_error(
            strand,
            format!("{kind} in the XML namespace must use the xml prefix"),
        ));
    }
    if name.prefix.as_deref() == Some("xml") && name.namespace.as_deref() != Some(XML_NS) {
        return Err(value_error(
            strand,
            format!("{kind} with the xml prefix must use the XML namespace"),
        ));
    }
    if name.prefix.as_deref() == Some("xmlns") {
        return Err(value_error(strand, "the xmlns prefix is reserved"));
    }
    Ok(())
}

fn validate_ncname<'v, 's>(
    strand: &mut Strand<'v, 's>,
    name: &str,
    kind: &str,
) -> Result<'v, 's, ()> {
    let mut chars = name.chars();
    if !chars.next().is_some_and(is_name_start) || !chars.all(is_name_char) || name.contains(':') {
        return Err(value_error(strand, format!("invalid XML {kind} {name:?}")));
    }
    Ok(())
}

fn is_name_start(c: char) -> bool {
    matches!(
        c,
        'A'..='Z'
            | '_'
            | 'a'..='z'
            | '\u{00C0}'..='\u{00D6}'
            | '\u{00D8}'..='\u{00F6}'
            | '\u{00F8}'..='\u{02FF}'
            | '\u{0370}'..='\u{037D}'
            | '\u{037F}'..='\u{1FFF}'
            | '\u{200C}'..='\u{200D}'
            | '\u{2070}'..='\u{218F}'
            | '\u{2C00}'..='\u{2FEF}'
            | '\u{3001}'..='\u{D7FF}'
            | '\u{F900}'..='\u{FDCF}'
            | '\u{FDF0}'..='\u{FFFD}'
            | '\u{10000}'..='\u{EFFFF}'
    )
}

fn is_name_char(c: char) -> bool {
    is_name_start(c)
        || matches!(
            c,
            '-' | '.'
                | '0'..='9'
                | '\u{00B7}'
                | '\u{0300}'..='\u{036F}'
                | '\u{203F}'..='\u{2040}'
        )
}

fn validate_text<'v, 's>(
    strand: &mut Strand<'v, 's>,
    text: &str,
    kind: &str,
) -> Result<'v, 's, ()> {
    if text.chars().any(|c| {
        !matches!(
            c,
            '\u{9}' | '\u{A}' | '\u{D}' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}'
                | '\u{10000}'..='\u{10FFFF}'
        )
    }) {
        return Err(value_error(
            strand,
            format!("{kind} contains a character forbidden by XML 1.0"),
        ));
    }
    Ok(())
}

fn declare(
    declarations: &mut Vec<(String, String)>,
    current: &mut HashMap<String, String>,
    prefix: String,
    namespace: String,
) {
    current.insert(prefix.clone(), namespace.clone());
    if let Some((_, value)) = declarations.iter_mut().find(|(key, _)| key == &prefix) {
        *value = namespace;
    } else {
        declarations.push((prefix, namespace));
    }
}

fn existing_prefix(current: &HashMap<String, String>, namespace: &str) -> Option<String> {
    let mut prefixes = current
        .iter()
        .filter(|(prefix, uri)| !prefix.is_empty() && uri.as_str() == namespace)
        .map(|(prefix, _)| prefix.as_str())
        .collect::<Vec<_>>();
    prefixes.sort_unstable();
    prefixes.first().map(|prefix| (*prefix).to_owned())
}

fn generated_prefix(walk: &mut WalkState, current: &HashMap<String, String>) -> String {
    loop {
        let prefix = format!("ns{}", walk.generated);
        walk.generated += 1;
        if !current.contains_key(&prefix) {
            return prefix;
        }
    }
}

fn choose_qname(
    name: &Name,
    is_attribute: bool,
    current: &mut HashMap<String, String>,
    declarations: &mut Vec<(String, String)>,
    walk: &mut WalkState,
) -> String {
    let Some(namespace) = name.namespace.as_deref() else {
        if !is_attribute && current.get("").is_some_and(|value| !value.is_empty()) {
            declare(declarations, current, String::new(), String::new());
        }
        return name.local.clone();
    };
    if namespace == XML_NS {
        return format!("xml:{}", name.local);
    }

    if let Some(prefix) = &name.prefix
        && current.get(prefix).map(String::as_str) == Some(namespace)
    {
        return format!("{prefix}:{}", name.local);
    }
    if let Some(prefix) = &name.prefix
        && !current.contains_key(prefix)
    {
        declare(declarations, current, prefix.clone(), namespace.to_owned());
        return format!("{prefix}:{}", name.local);
    }
    if !is_attribute
        && name.prefix.is_none()
        && current.get("").map(String::as_str) == Some(namespace)
    {
        return name.local.clone();
    }
    if !is_attribute && name.prefix.is_none() && !current.contains_key("") {
        declare(declarations, current, String::new(), namespace.to_owned());
        return name.local.clone();
    }
    if let Some(prefix) = existing_prefix(current, namespace) {
        return format!("{prefix}:{}", name.local);
    }
    let prefix = generated_prefix(walk, current);
    declare(declarations, current, prefix.clone(), namespace.to_owned());
    format!("{prefix}:{}", name.local)
}

fn walk_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    state: State<'v, Global<'v>>,
    parent_namespaces: &HashMap<String, String>,
    walk: &mut WalkState,
    writer: &mut Option<Writer<Vec<u8>>>,
) -> Result<'v, 's, ()> {
    let Some(cast) = state.node_type.cast(value) else {
        let text = value
            .as_str(strand)
            .ok_or_else(|| value_error(strand, "XML children must be Node or Str"))?
            .to_string();
        validate_text(strand, &text, "XML text")?;
        if let Some(writer) = writer {
            writer
                .write_event(Event::Text(BytesText::new(&text)))
                .into_do(strand)?;
        }
        return Ok(());
    };
    let id = match value.view(strand.vm()) {
        View::Object(object) => object.id().addr(),
        _ => unreachable!(),
    };
    if !walk.ancestry.insert(id) {
        return Err(value_error(strand, "cycle detected in XML tree"));
    }

    let result = cast.enter_sync(strand, |strand, node| {
        let name = node.borrow(strand)?.name.clone();
        validate_name(strand, &name, "element name")?;
        let snapshot = read_namespaces(strand, node)?;
        let mut current = snapshot.map;
        let mut declarations = snapshot
            .ordered
            .into_iter()
            .filter(|(prefix, namespace)| {
                prefix != "xml"
                    && parent_namespaces.get(prefix).map(String::as_str) != Some(namespace.as_str())
            })
            .collect::<Vec<_>>();
        if parent_namespaces
            .get("")
            .is_some_and(|namespace| !namespace.is_empty())
            && current.get("").is_none_or(String::is_empty)
        {
            declare(
                &mut declarations,
                &mut current,
                String::new(),
                String::new(),
            );
        }
        let element_qname = choose_qname(&name, false, &mut current, &mut declarations, walk);

        let borrow = node.borrow(strand)?;
        let attrs = Ref::slot::<ATTRS>(&borrow).as_array(strand).unwrap();
        let children = Ref::slot::<CHILDREN>(&borrow).as_array(strand).unwrap();
        let mut emitted_attrs = Vec::new();
        let mut expanded = HashSet::new();
        strand.with_slots_sync(|strand, [mut item]| {
            for index in 0..attrs.len(strand)? {
                attrs.get(strand, index, &mut item)?;
                let attr = state
                    .attr_type
                    .cast(&item)
                    .ok_or_else(|| value_error(strand, "Node.attrs must contain Attr objects"))?;
                let attr =
                    attr.enter_sync(strand, |strand, attr| Ok(attr.borrow(strand)?.clone()))?;
                validate_name(strand, &attr.name, "attribute name")?;
                if attr.name.namespace.is_none()
                    && attr.name.prefix.is_none()
                    && attr.name.local == "xmlns"
                {
                    return Err(value_error(strand, "the xmlns attribute name is reserved"));
                }
                validate_text(strand, &attr.value, "attribute value")?;
                if !expanded.insert((attr.name.namespace.clone(), attr.name.local.clone())) {
                    return Err(value_error(
                        strand,
                        format!("duplicate XML attribute {}", attr.name.local),
                    ));
                }
                let qname = choose_qname(&attr.name, true, &mut current, &mut declarations, walk);
                emitted_attrs.push((qname, attr.value));
            }
            Ok(())
        })?;

        if let Some(writer) = writer {
            let mut start = BytesStart::new(element_qname.as_str());
            let mut output_attrs = declarations
                .iter()
                .map(|(prefix, namespace)| {
                    let key = if prefix.is_empty() {
                        "xmlns".to_owned()
                    } else {
                        format!("xmlns:{prefix}")
                    };
                    (key, escape(namespace).into_owned())
                })
                .collect::<Vec<_>>();
            output_attrs.extend(
                emitted_attrs
                    .iter()
                    .map(|(key, value)| (key.clone(), escape(value).into_owned())),
            );
            for (key, value) in &output_attrs {
                start.push_attribute((key.as_str(), value.as_str()));
            }
            writer.write_event(Event::Start(start)).into_do(strand)?;
        }

        strand.with_slots_sync(|strand, [mut child]| {
            for index in 0..children.len(strand)? {
                children.get(strand, index, &mut child)?;
                walk_value(strand, &child, state, &current, walk, writer)?;
            }
            Ok(())
        })?;

        if let Some(writer) = writer {
            writer
                .write_event(Event::End(BytesEnd::new(element_qname.as_str())))
                .into_do(strand)?;
        }
        Ok(())
    });
    walk.ancestry.remove(&id);
    result
}
