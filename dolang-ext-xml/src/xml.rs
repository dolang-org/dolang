use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    mem,
    num::NonZero,
    str,
};

use quick_xml::{
    NsReader, XmlVersion,
    escape::{escape, resolve_predefined_entity, unescape},
    events::{BytesStart, Event},
    name::{PrefixDeclaration, ResolveResult},
};

use dolang::runtime::{
    Slot, State, Strand, Value,
    error::{Error, Result, ResultExt},
    object::Ref,
    unpack,
    value::{Array, StrEmbryo, View},
    vm::Register,
};

use crate::{
    attr::{Attr, Name},
    check::{XML_NS, validate_name, validate_namespace_binding, validate_text},
    global::Global,
    node::{CHILDREN, check_new_attrs, create_node},
};

struct ParsedElement {
    name: Name,
    attrs: Vec<Attr>,
    namespaces: Vec<(String, String)>,
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
                        parse_element(strand, &mut reader, &start, state, &mut out)?;
                        break;
                    }
                    Event::Empty(start) => {
                        create_parsed_node(strand, &reader, &start, state, &mut out)?;
                        break;
                    }
                    Event::Eof => return Err(Error::value(strand, "no root XML element")),
                    _ => {}
                }
            }
            Ok(())
        })
        .function("encode", async move |strand, args, out| {
            let ([arg], []) = unpack!(strand, args, 1, 0)?;
            // Nodes check their text as it is added; a lone string has not been.
            if let Some(text) = arg.as_str(strand) {
                let text = text.pin();
                validate_text(strand, &text, "XML text")?;
            }
            let mut xml = StrEmbryo::new();
            let mut walk = WalkState::default();
            walk_value(strand, &arg, state, &HashMap::new(), &mut walk, &mut xml)?;
            xml.finish(strand, out);
            Ok(())
        })
        .value("Node", state.node_type)
        .commit();
}

fn bytes_string<'v, 's>(strand: &mut Strand<'v, 's>, bytes: &[u8]) -> Result<'v, 's, String> {
    Ok(str::from_utf8(bytes).into_do(strand)?.to_owned())
}

/// Decodes a namespace URI; the resolver yields it still escaped.
fn namespace_string<'v, 's>(strand: &mut Strand<'v, 's>, bytes: &[u8]) -> Result<'v, 's, String> {
    let raw = str::from_utf8(bytes).into_do(strand)?;
    Ok(unescape(raw).into_do(strand)?.into_owned())
}

fn resolved_namespace<'v, 's>(
    strand: &mut Strand<'v, 's>,
    resolved: ResolveResult<'_>,
) -> Result<'v, 's, Option<String>> {
    match resolved {
        ResolveResult::Unbound => Ok(None),
        ResolveResult::Bound(namespace) => namespace_string(strand, namespace.as_ref()).map(Some),
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
    let (resolved, local) = resolver.resolve_element(start.name());
    let name = Name {
        local: bytes_string(strand, local.as_ref())?,
        namespace: resolved_namespace(strand, resolved)?,
    };
    validate_name(strand, &name, "element name")?;

    let mut attrs = Vec::new();
    let mut namespaces = Vec::new();
    for attr in start.attributes() {
        let attr = attr.into_do(strand)?;
        if let Some(binding) = attr.key.as_namespace_binding() {
            let prefix = match binding {
                PrefixDeclaration::Default => String::new(),
                PrefixDeclaration::Named(prefix) => bytes_string(strand, prefix)?,
            };
            let namespace = namespace_string(strand, &attr.value)?;
            validate_namespace_binding(strand, &prefix, &namespace)?;
            namespaces.push((prefix, namespace));
            continue;
        }
        let (resolved, local) = resolver.resolve_attribute(attr.key);
        let name = Name {
            local: bytes_string(strand, local.as_ref())?,
            namespace: resolved_namespace(strand, resolved)?,
        };
        let value = attr
            .normalized_value(XmlVersion::Implicit1_0)
            .into_do(strand)?
            .into_owned();
        let attr = Attr::checked(strand, name, value)?;
        // Distinct prefixes can bind the same namespace.
        check_new_attrs(strand, &attrs, None, std::slice::from_ref(&attr))?;
        attrs.push(attr);
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
    create_node(
        strand,
        state.node_type,
        parsed.name,
        parsed.attrs,
        parsed.namespaces,
        Slot::reborrow(out),
    )
}

fn flush_text<'v, 's>(
    strand: &mut Strand<'v, 's>,
    children: &Array<'v, '_>,
    text: &mut StrEmbryo<'v>,
    out: &mut Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    if text.is_empty() {
        return Ok(());
    }
    // Character references can produce characters XML text forbids.
    validate_text(strand, text.as_str(), "XML text")?;
    mem::take(text).finish(strand, &mut *out);
    children.push(strand, &mut *out)
}

fn parse_element<'v, 's>(
    strand: &mut Strand<'v, 's>,
    reader: &mut NsReader<&[u8]>,
    start: &BytesStart<'_>,
    state: State<'v, Global<'v>>,
    out: &mut Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    create_parsed_node(strand, reader, start, state, out)?;
    strand.with_slots_sync(|strand, [mut child, mut chunk]| {
        state
            .node_type
            .cast(&*out)
            .unwrap()
            .enter_sync(strand, |strand, node| {
                let borrow = node.borrow(strand)?;
                let children = Ref::slot::<CHILDREN>(&borrow).as_array(strand).unwrap();
                // Adjacent text, CDATA, and references coalesce into one child.
                let mut text = StrEmbryo::new();
                loop {
                    match reader.read_event().into_do(strand)? {
                        Event::Start(start) => {
                            flush_text(strand, &children, &mut text, &mut chunk)?;
                            parse_element(strand, reader, &start, state, &mut child)?;
                            children.push(strand, &mut child)?;
                        }
                        Event::Empty(start) => {
                            flush_text(strand, &children, &mut text, &mut chunk)?;
                            create_parsed_node(strand, reader, &start, state, &mut child)?;
                            children.push(strand, &mut child)?;
                        }
                        Event::Text(content) => {
                            let content = content
                                .xml_content(XmlVersion::Implicit1_0)
                                .into_do(strand)?;
                            text.extend(strand, &content);
                        }
                        Event::CData(content) => {
                            let content = str::from_utf8(content.as_ref()).into_do(strand)?;
                            text.extend(strand, content);
                        }
                        Event::GeneralRef(reference) => {
                            if let Some(character) = reference.resolve_char_ref().into_do(strand)? {
                                text.extend(strand, character.encode_utf8(&mut [0; 4]));
                            } else {
                                let decoded = reference.decode().into_do(strand)?;
                                if let Some(resolved) = resolve_predefined_entity(&decoded) {
                                    text.extend(strand, resolved);
                                } else {
                                    return Err(Error::value(
                                        strand,
                                        "custom XML entities are not supported",
                                    ));
                                }
                            }
                        }
                        Event::End(_) => {
                            flush_text(strand, &children, &mut text, &mut chunk)?;
                            break;
                        }
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

/// Namespace bindings in scope, copied only when an element changes them.
type Scope<'a> = Cow<'a, HashMap<String, String>>;

fn declare(
    declarations: &mut Vec<(String, String)>,
    current: &mut Scope<'_>,
    prefix: String,
    namespace: String,
) {
    current.to_mut().insert(prefix.clone(), namespace.clone());
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
    current: &mut Scope<'_>,
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
    let default = current.get("").map(String::as_str);
    if !is_attribute && default == Some(namespace) {
        return name.local.clone();
    }
    if let Some(prefix) = existing_prefix(current, namespace) {
        return format!("{prefix}:{}", name.local);
    }
    // Rebinding the default affects only this element's subtree, where every
    // name is chosen against the new scope, but it must not replace a default
    // the element declares itself.
    if !is_attribute && !declarations.iter().any(|(prefix, _)| prefix.is_empty()) {
        declare(declarations, current, String::new(), namespace.to_owned());
        return name.local.clone();
    }
    let prefix = generated_prefix(walk, current);
    declare(declarations, current, prefix.clone(), namespace.to_owned());
    format!("{prefix}:{}", name.local)
}

/// Writes `="value"`, escaping the value.
fn write_attribute_value<'v>(strand: &mut Strand<'v, '_>, xml: &mut StrEmbryo<'v>, value: &str) {
    xml.extend(strand, "=\"");
    xml.extend(strand, &escape(value));
    xml.extend(strand, "\"");
}

/// Writes a tree, which mutation checks keep valid apart from cycles.
fn walk_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    state: State<'v, Global<'v>>,
    parent_namespaces: &HashMap<String, String>,
    walk: &mut WalkState,
    xml: &mut StrEmbryo<'v>,
) -> Result<'v, 's, ()> {
    let Some(cast) = state.node_type.cast(value) else {
        let text = value
            .as_str(strand)
            .ok_or_else(|| Error::type_error(strand, "expected xml.Node or Str"))?
            .pin();
        xml.extend(strand, &escape(&*text));
        return Ok(());
    };
    let id = match value.view(strand.vm()) {
        View::Object(object) => object.id().addr(),
        _ => unreachable!(),
    };
    if !walk.ancestry.insert(id) {
        return Err(Error::value(strand, "cycle detected in XML tree"));
    }

    let result = cast.enter_sync(strand, |strand, node| {
        let node = node.borrow(strand)?;
        let mut current = Cow::Borrowed(parent_namespaces);
        let mut declarations = Vec::new();
        for (prefix, namespace) in &node.namespaces {
            // Undeclaring a default namespace that is not in scope is a no-op.
            let unchanged = match current.get(prefix) {
                Some(bound) => bound == namespace,
                None => prefix.is_empty() && namespace.is_empty(),
            };
            if prefix != "xml" && !unchanged {
                declare(
                    &mut declarations,
                    &mut current,
                    prefix.clone(),
                    namespace.clone(),
                );
            }
        }
        let element_qname = choose_qname(&node.name, false, &mut current, &mut declarations, walk);
        let attr_qnames = node
            .attrs
            .iter()
            .map(|attr| choose_qname(&attr.name, true, &mut current, &mut declarations, walk))
            .collect::<Vec<_>>();

        xml.extend(strand, "<");
        xml.extend(strand, &element_qname);
        for (prefix, namespace) in &declarations {
            xml.extend(strand, " xmlns");
            if !prefix.is_empty() {
                xml.extend(strand, ":");
                xml.extend(strand, prefix);
            }
            write_attribute_value(strand, xml, namespace);
        }
        for (qname, attr) in attr_qnames.iter().zip(&node.attrs) {
            xml.extend(strand, " ");
            xml.extend(strand, qname);
            write_attribute_value(strand, xml, &attr.value);
        }
        xml.extend(strand, ">");

        let children = Ref::slot::<CHILDREN>(&node).as_array(strand).unwrap();
        strand.with_slots_sync(|strand, [mut child]| {
            for index in 0..children.len(strand)? {
                children.get(strand, index, &mut child)?;
                walk_value(strand, &child, state, &current, walk, xml)?;
            }
            Ok(())
        })?;

        xml.extend(strand, "</");
        xml.extend(strand, &element_qname);
        xml.extend(strand, ">");
        Ok(())
    });
    walk.ancestry.remove(&id);
    result
}
