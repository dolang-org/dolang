use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    fmt::Display,
    mem,
    num::NonZero,
    result, slice, str,
};

use quick_xml::{
    Error as XmlError, NsReader, XmlVersion,
    errors::{IllFormedError, SyntaxError},
    escape::{EscapeError, escape, resolve_predefined_entity, unescape},
    events::{BytesStart, BytesText, Event, attributes::AttrError},
    name::{PrefixDeclaration, ResolveResult},
};

use dolang::runtime::AllocExt;
use dolang::runtime::{
    Instance, Object, Output, Slot, State, Strand, Value,
    error::{Error, Result},
    object::{Mut, Ref, TypeBuilder, fmt},
    unpack,
    value::{Array, StrEmbryo, TypeObject, View, fmt::Format},
    vm::Register,
};

use crate::{
    attr::{Attr, Name},
    check::{Invalid, XML_NS, validate_name, validate_namespace_binding, validate_text},
    comment::{Comment, validate_comment},
    document::create_document,
    element::{CHILDREN, check_new_attrs, create_element},
    global::{Global, Tag},
};

const ONE_ROOT: &str = "XML document must contain exactly one root element";
const CYCLE: &str = "cycle detected in XML tree";

/// `xml.DecodeError`: the input is not well-formed XML, or uses a feature
/// `xml.decode` rejects.
pub(crate) struct DecodeError;

/// What is wrong with the input.
#[derive(Clone, Copy)]
enum DecodeKind {
    /// A character is out of place.
    Syntax,
    /// The input ends inside markup, an element, or a reference.
    Eof,
    /// The markup parses but breaks an XML rule.
    IllFormed,
    /// An entity or character reference can't be expanded.
    Reference,
    /// A prefix is unbound, or a namespace binding is forbidden.
    Namespace,
}

pub(crate) struct DecodeAnnex {
    message: String,
    kind: DecodeKind,
    line: usize,
    /// 1-based, in characters.
    column: usize,
}

/// `xml.EncodeError`: the tree contains itself.
///
/// A `ValueError`: the fault is in the value the caller passed. Mutation
/// checks keep every other fault out of a tree, so the only kind is `:CYCLE:`.
pub(crate) struct EncodeError;

/// Failure while encoding.
enum Fault<'v, 's> {
    /// Raised as is.
    Do(Error<'v, 's>),
    /// Raised as `xml.EncodeError`, with the path [`WalkState::within`]
    /// collected.
    Cycle,
}

impl<'v, 's> From<Error<'v, 's>> for Fault<'v, 's> {
    fn from(error: Error<'v, 's>) -> Self {
        Self::Do(error)
    }
}

type EncodeResult<'v, 's, T> = result::Result<T, Fault<'v, 's>>;

struct ParsedElement {
    name: Name,
    attrs: Vec<Attr>,
    namespaces: Vec<(String, String)>,
}

struct WalkState<'v, 'p> {
    ancestry: HashSet<NonZero<usize>>,
    generated: usize,
    /// Nil, or the child indices leading to a fault, innermost first.
    path: Slot<'v, 'p>,
}

impl<'v> WalkState<'v, '_> {
    /// Records that `fault` lies under child `index`.
    fn within<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        fault: Fault<'v, 's>,
        index: usize,
    ) -> Fault<'v, 's> {
        if let Fault::Cycle = fault {
            if self.path.is_nil() {
                strand.new_array(&mut self.path);
            }
            let path = self.path.as_array(strand.vm()).expect("path is an Array");
            path.push(strand, index).expect("path is not shared");
        }
        fault
    }
}

pub(crate) fn configure<'v>(builder: &mut Register<'v>, state: State<'v, Global<'v>>) {
    builder
        .module("xml")
        .function("decode", async move |strand, args, out| {
            let ([arg], []) = unpack!(strand, args, 1, 0)?;
            let src = arg
                .as_str(strand.vm())
                .ok_or_else(|| Error::type_error(strand, "expected Str"))?
                .pin();
            decode(strand, &src, state, out)
        })
        .function("encode", async move |strand, args, out| {
            let ([arg], []) = unpack!(strand, args, 1, 0)?;
            let document = state
                .document_type
                .cast(&arg)
                .ok_or_else(|| Error::type_error(strand, "expected xml.Document"))?;
            strand.with_slots_sync(|strand, [path, mut tuple, mut error]| {
                let mut xml = StrEmbryo::new();
                let mut walk = WalkState {
                    ancestry: HashSet::new(),
                    generated: 0,
                    path,
                };
                let result = document.enter_sync(strand, |strand, document| {
                    let borrow = document.borrow(strand)?;
                    let children = Ref::slot::<{ crate::document::CHILDREN }>(&borrow)
                        .as_array(strand)
                        .unwrap();
                    strand.with_slots_sync(|strand, [mut child]| {
                        for i in 0..children.len(strand)? {
                            children.get(strand, i, &mut child)?;
                            walk_value(strand, &child, state, &HashMap::new(), &mut walk, &mut xml)
                                .map_err(|fault| walk.within(strand, fault, i))?;
                        }
                        Ok::<(), Fault<'_, '_>>(())
                    })
                });
                match result {
                    Ok(()) => {}
                    Err(Fault::Do(error)) => return Err(error),
                    Err(Fault::Cycle) => {
                        // A cycle lies under some document child.
                        let path = walk.path.as_array(strand.vm()).expect("path is an Array");
                        path.reverse(strand).expect("path is not shared");
                        path.to_tuple(strand, &mut tuple)
                            .expect("path is not shared");
                        let ty = state.encode_error_type;
                        ty.create(strand, EncodeError, &mut error);
                        ty.cast(&error)
                            .unwrap()
                            .enter_sync(strand, |strand, instance| {
                                let mut borrow = instance.borrow_mut_unwrap();
                                Output::set(strand, Mut::slot_mut::<0>(&mut borrow), &tuple);
                            });
                        return Err(Error::from_value(strand, &error));
                    }
                }
                xml.finish(strand, out);
                Ok(())
            })
        })
        .value("Element", state.element_type)
        .value("Comment", state.comment_type)
        .value("Document", state.document_type)
        .value("DecodeError", state.decode_error_type)
        .value("EncodeError", state.encode_error_type)
        .commit();
}

/// Reads a slice of the input that quick-xml split at ASCII delimiters, so it
/// is still UTF-8.
fn utf8(bytes: &[u8]) -> &str {
    str::from_utf8(bytes).expect("quick-xml splits a str only at ASCII delimiters")
}

fn unknown_entity(name: &str) -> String {
    format!("unknown XML entity &{name}; (custom entities are not supported)")
}

fn escape_message(error: &EscapeError) -> String {
    match error {
        EscapeError::UnrecognizedEntity(_, name) => unknown_entity(name),
        EscapeError::UnterminatedEntity(_) => {
            "entity or character reference not closed: `;` not found".to_owned()
        }
        EscapeError::InvalidCharRef(error) => format!("invalid character reference: {error}"),
        EscapeError::TooManyNestedEntities => "too many nested XML entities".to_owned(),
    }
}

fn is_xml_whitespace(text: &str) -> bool {
    text.bytes()
        .all(|c| matches!(c, b' ' | b'\t' | b'\r' | b'\n'))
}

/// Reads a document, tracking where each fault lies.
struct Decoder<'a> {
    src: &'a str,
    reader: NsReader<&'a [u8]>,
    /// Byte offset of the event read last: its `<`, `&`, or first text byte.
    start: usize,
}

impl<'a> Decoder<'a> {
    /// Raises `xml.DecodeError` for a fault at byte `offset`.
    fn fail<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        kind: DecodeKind,
        offset: usize,
        message: impl Display,
    ) -> Error<'v, 's> {
        let before = self.src.get(..offset).unwrap_or(self.src);
        let line_start = before.rfind('\n').map_or(0, |i| i + 1);
        let line = before.matches('\n').count() + 1;
        let column = before[line_start..].chars().count() + 1;
        let global = strand.state::<Tag>();
        Error::object_with_annex(
            strand,
            global.decode_error_type,
            DecodeError,
            DecodeAnnex {
                message: format!("{message} at line {line} column {column}"),
                kind,
                line,
                column,
            },
        )
    }

    /// Raises a failed check on the event read last.
    fn invalid<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        kind: DecodeKind,
        Invalid(message): Invalid,
    ) -> Error<'v, 's> {
        self.fail(strand, kind, self.start, message)
    }

    /// Raises a quick-xml error found at `offset`.
    fn quick<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        error: &XmlError,
        offset: usize,
    ) -> Error<'v, 's> {
        let (kind, message) = match error {
            XmlError::Syntax(SyntaxError::InvalidBangMarkup) => {
                (DecodeKind::Syntax, error.to_string())
            }
            // The other syntax errors are markup the input ends inside.
            XmlError::Syntax(_) | XmlError::IllFormed(IllFormedError::UnclosedReference) => {
                (DecodeKind::Eof, error.to_string())
            }
            XmlError::IllFormed(_) => (DecodeKind::IllFormed, error.to_string()),
            XmlError::InvalidAttr(error) => return self.attr_error(strand, error),
            XmlError::Escape(error) => (DecodeKind::Reference, escape_message(error)),
            XmlError::Namespace(_) => (DecodeKind::Namespace, error.to_string()),
            // I/O and encoding errors can't arise from a `str`.
            _ => (DecodeKind::Syntax, error.to_string()),
        };
        self.fail(strand, kind, offset, message)
    }

    /// Raises an error in the attributes of the start tag read last.
    fn attr_error<'v, 's>(&self, strand: &mut Strand<'v, 's>, error: &AttrError) -> Error<'v, 's> {
        let (kind, pos, message) = match *error {
            AttrError::ExpectedEq(pos) => (
                DecodeKind::Syntax,
                pos,
                "attribute name must be followed by `=`".to_owned(),
            ),
            AttrError::ExpectedValue(pos) => (
                DecodeKind::Syntax,
                pos,
                "`=` must be followed by an attribute value".to_owned(),
            ),
            AttrError::UnquotedValue(pos) => (
                DecodeKind::Syntax,
                pos,
                "attribute value must be enclosed in `\"` or `'`".to_owned(),
            ),
            AttrError::ExpectedQuote(pos, quote) => (
                DecodeKind::Syntax,
                pos,
                format!(
                    "missing closing quote `{}` in attribute value",
                    char::from(quote)
                ),
            ),
            AttrError::Duplicated(pos, _) => (
                DecodeKind::IllFormed,
                pos,
                "duplicate XML attribute".to_owned(),
            ),
        };
        // Positions count from the byte after the tag's `<`.
        self.fail(strand, kind, self.start + 1 + pos, message)
    }

    fn read<'v, 's>(&mut self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, Event<'a>> {
        self.start = self.reader.buffer_position() as usize;
        match self.reader.read_event() {
            Ok(event) => Ok(event),
            Err(error) => {
                // Namespace errors come from the resolver, which doesn't
                // record a position.
                let offset = match error {
                    XmlError::Namespace(_) => self.start,
                    _ => self.reader.error_position() as usize,
                };
                Err(self.quick(strand, &error, offset))
            }
        }
    }

    /// Decodes a namespace URI; the resolver yields it still escaped.
    fn namespace_uri<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        bytes: &[u8],
    ) -> Result<'v, 's, String> {
        match unescape(utf8(bytes)) {
            Ok(uri) => Ok(uri.into_owned()),
            Err(error) => Err(self.fail(
                strand,
                DecodeKind::Reference,
                self.start,
                escape_message(&error),
            )),
        }
    }

    fn resolved_namespace<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        resolved: ResolveResult<'_>,
    ) -> Result<'v, 's, Option<String>> {
        match resolved {
            ResolveResult::Unbound => Ok(None),
            ResolveResult::Bound(namespace) => {
                self.namespace_uri(strand, namespace.as_ref()).map(Some)
            }
            ResolveResult::Unknown(prefix) => Err(self.fail(
                strand,
                DecodeKind::Namespace,
                self.start,
                format!(
                    "unknown XML namespace prefix {}",
                    String::from_utf8_lossy(&prefix)
                ),
            )),
        }
    }

    fn parse_start<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        start: &BytesStart<'_>,
    ) -> Result<'v, 's, ParsedElement> {
        // Bindings come first, so that a forbidden one is reported as such
        // rather than as a name using it.
        let mut namespaces = Vec::new();
        let mut plain = Vec::new();
        for attr in start.attributes() {
            let attr = attr.map_err(|error| self.attr_error(strand, &error))?;
            let Some(binding) = attr.key.as_namespace_binding() else {
                plain.push(attr);
                continue;
            };
            let prefix = match binding {
                PrefixDeclaration::Default => String::new(),
                PrefixDeclaration::Named(prefix) => utf8(prefix).to_owned(),
            };
            let namespace = self.namespace_uri(strand, &attr.value)?;
            validate_namespace_binding(&prefix, &namespace)
                .map_err(|error| self.invalid(strand, DecodeKind::Namespace, error))?;
            namespaces.push((prefix, namespace));
        }

        let resolver = self.reader.resolver();
        let (resolved, local) = resolver.resolve_element(start.name());
        let name = Name {
            local: utf8(local.as_ref()).to_owned(),
            namespace: self.resolved_namespace(strand, resolved)?,
        };
        validate_name(&name, "element name")
            .map_err(|error| self.invalid(strand, DecodeKind::IllFormed, error))?;

        let mut attrs = Vec::new();
        for attr in plain {
            let (resolved, local) = resolver.resolve_attribute(attr.key);
            let name = Name {
                local: utf8(local.as_ref()).to_owned(),
                namespace: self.resolved_namespace(strand, resolved)?,
            };
            // A forbidden character the raw value lacks came from a reference.
            validate_text(utf8(&attr.value), "attribute value")
                .map_err(|error| self.invalid(strand, DecodeKind::IllFormed, error))?;
            let value = attr
                .normalized_value(XmlVersion::Implicit1_0)
                .map_err(|error| self.quick(strand, &error, self.start))?
                .into_owned();
            validate_text(&value, "attribute value").map_err(|_| {
                self.fail(
                    strand,
                    DecodeKind::Reference,
                    self.start,
                    "attribute value has a character reference forbidden by XML 1.0",
                )
            })?;
            let attr = Attr::checked(name, value)
                .map_err(|error| self.invalid(strand, DecodeKind::IllFormed, error))?;
            // Distinct prefixes can bind the same namespace.
            check_new_attrs(&attrs, None, slice::from_ref(&attr))
                .map_err(|error| self.invalid(strand, DecodeKind::IllFormed, error))?;
            attrs.push(attr);
        }

        Ok(ParsedElement {
            name,
            attrs,
            namespaces,
        })
    }

    fn create_element<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        start: &BytesStart<'_>,
        state: State<'v, Global<'v>>,
        out: &mut Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let parsed = self.parse_start(strand, start)?;
        create_element(
            strand,
            state.element_type,
            parsed.name,
            parsed.attrs,
            parsed.namespaces,
            Slot::reborrow(out),
        )
    }

    fn comment<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        content: &BytesText<'_>,
        state: State<'v, Global<'v>>,
        out: &mut Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let text = content
            .xml_content(XmlVersion::Implicit1_0)
            .expect("input is UTF-8")
            .into_owned();
        validate_comment(&text)
            .map_err(|error| self.invalid(strand, DecodeKind::IllFormed, error))?;
        state.comment_type.create(strand, Comment { text }, out);
        Ok(())
    }

    /// Reads the content and end tag of the element `start` opens.
    fn parse_element<'v, 's>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        start: &BytesStart<'_>,
        state: State<'v, Global<'v>>,
        out: &mut Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        self.create_element(strand, start, state, out)?;
        strand.with_slots_sync(|strand, [mut child, mut chunk]| {
            state
                .element_type
                .cast(&*out)
                .unwrap()
                .enter_sync(strand, |strand, node| {
                    let borrow = node.borrow(strand)?;
                    let children = Ref::slot::<CHILDREN>(&borrow).as_array(strand).unwrap();
                    // Adjacent text, CDATA, and references coalesce into one
                    // child.
                    let mut text = StrEmbryo::new();
                    loop {
                        match self.read(strand)? {
                            Event::Start(start) => {
                                flush_text(strand, &children, &mut text, &mut chunk)?;
                                self.parse_element(strand, &start, state, &mut child)?;
                                children.push(strand, &mut child)?;
                            }
                            Event::Empty(start) => {
                                flush_text(strand, &children, &mut text, &mut chunk)?;
                                self.create_element(strand, &start, state, &mut child)?;
                                children.push(strand, &mut child)?;
                            }
                            Event::Comment(content) => {
                                flush_text(strand, &children, &mut text, &mut chunk)?;
                                self.comment(strand, &content, state, &mut child)?;
                                children.push(strand, &mut child)?;
                            }
                            Event::Text(content) => {
                                let content = content
                                    .xml_content(XmlVersion::Implicit1_0)
                                    .expect("input is UTF-8");
                                validate_text(&content, "XML text").map_err(|error| {
                                    self.invalid(strand, DecodeKind::IllFormed, error)
                                })?;
                                text.extend(strand, &content);
                            }
                            Event::CData(content) => {
                                let content = utf8(content.as_ref());
                                validate_text(content, "CDATA").map_err(|error| {
                                    self.invalid(strand, DecodeKind::IllFormed, error)
                                })?;
                                text.extend(strand, content);
                            }
                            Event::GeneralRef(reference) => {
                                let name = reference.decode().expect("input is UTF-8");
                                match reference.resolve_char_ref() {
                                    Ok(Some(character)) => {
                                        let mut buf = [0; 4];
                                        let character = character.encode_utf8(&mut buf);
                                        if validate_text(character, "character reference").is_err()
                                        {
                                            return Err(self.fail(
                                                strand,
                                                DecodeKind::Reference,
                                                self.start,
                                                format!(
                                                    "character reference &{name}; is forbidden \
                                                     by XML 1.0"
                                                ),
                                            ));
                                        }
                                        text.extend(strand, character);
                                    }
                                    Ok(None) => match resolve_predefined_entity(&name) {
                                        Some(resolved) => text.extend(strand, resolved),
                                        None => {
                                            return Err(self.fail(
                                                strand,
                                                DecodeKind::Reference,
                                                self.start,
                                                unknown_entity(&name),
                                            ));
                                        }
                                    },
                                    Err(error) => {
                                        return Err(self.quick(strand, &error, self.start));
                                    }
                                }
                            }
                            Event::End(_) => {
                                flush_text(strand, &children, &mut text, &mut chunk)?;
                                break;
                            }
                            Event::Eof => {
                                return Err(self.fail(
                                    strand,
                                    DecodeKind::Eof,
                                    self.src.len(),
                                    "unexpected end of XML document",
                                ));
                            }
                            _ => {}
                        }
                    }
                    Ok(())
                })
        })
    }
}

fn decode<'v, 's>(
    strand: &mut Strand<'v, 's>,
    src: &str,
    state: State<'v, Global<'v>>,
    mut out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let mut reader = NsReader::from_str(src);
    reader.config_mut().trim_text(false);
    let mut decoder = Decoder {
        src,
        reader,
        start: 0,
    };
    strand.with_slots_sync(|strand, [mut children, mut child]| {
        strand.new_array(&mut children);
        let array = children.as_array(strand).unwrap();
        let mut rooted = false;
        loop {
            let event = decoder.read(strand)?;
            if let Event::Start(_) | Event::Empty(_) = event {
                if rooted {
                    let start = decoder.start;
                    return Err(decoder.fail(strand, DecodeKind::IllFormed, start, ONE_ROOT));
                }
                rooted = true;
            }
            match event {
                Event::Start(start) => {
                    decoder.parse_element(strand, &start, state, &mut child)?;
                    array.push(strand, &mut child)?;
                }
                Event::Empty(start) => {
                    decoder.create_element(strand, &start, state, &mut child)?;
                    array.push(strand, &mut child)?;
                }
                Event::Comment(content) => {
                    decoder.comment(strand, &content, state, &mut child)?;
                    array.push(strand, &mut child)?;
                }
                Event::Text(content) => {
                    let text = content
                        .xml_content(XmlVersion::Implicit1_0)
                        .expect("input is UTF-8");
                    if !is_xml_whitespace(&text) {
                        return Err(decoder.fail(
                            strand,
                            DecodeKind::IllFormed,
                            decoder.start,
                            "document text must contain only XML whitespace",
                        ));
                    }
                    Output::set(strand, &mut child, text.as_ref());
                    array.push(strand, &mut child)?;
                }
                Event::Eof => break,
                Event::Decl(_) | Event::PI(_) | Event::DocType(_) => {}
                _ => {
                    return Err(decoder.fail(
                        strand,
                        DecodeKind::IllFormed,
                        decoder.start,
                        "invalid content outside XML root",
                    ));
                }
            }
        }
        if !rooted {
            return Err(decoder.fail(strand, DecodeKind::IllFormed, src.len(), ONE_ROOT));
        }
        create_document(
            strand,
            state.document_type,
            &children,
            Slot::reborrow(&mut out),
        )
    })
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
    mem::take(text).finish(strand, &mut *out);
    children.push(strand, &mut *out)
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

fn generated_prefix(walk: &mut WalkState<'_, '_>, current: &HashMap<String, String>) -> String {
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
    walk: &mut WalkState<'_, '_>,
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
    walk: &mut WalkState<'v, '_>,
    xml: &mut StrEmbryo<'v>,
) -> EncodeResult<'v, 's, ()> {
    if let Some(comment) = state.comment_type.cast(value) {
        return Ok(comment.enter_sync(strand, |strand, comment| {
            xml.extend(strand, "<!--");
            let borrow = comment.borrow(strand)?;
            xml.extend(strand, &borrow.text);
            xml.extend(strand, "-->");
            Ok::<(), Error<'v, 's>>(())
        })?);
    }
    let Some(cast) = state.element_type.cast(value) else {
        let text = value
            .as_str(strand)
            .ok_or_else(|| Error::type_error(strand, "expected xml.Element, xml.Comment, or Str"))?
            .pin();
        xml.extend(strand, &escape(&*text));
        return Ok(());
    };
    let id = match value.view(strand.vm()) {
        View::Object(object) => object.id().addr(),
        _ => unreachable!(),
    };
    if !walk.ancestry.insert(id) {
        return Err(Fault::Cycle);
    }

    let result = cast.enter_sync(strand, |strand, node| -> EncodeResult<'v, 's, ()> {
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
                walk_value(strand, &child, state, &current, walk, xml)
                    .map_err(|fault| walk.within(strand, fault, index))?;
            }
            Ok::<(), Fault<'v, 's>>(())
        })?;

        xml.extend(strand, "</");
        xml.extend(strand, &element_qname);
        xml.extend(strand, ">");
        Ok(())
    });
    walk.ancestry.remove(&id);
    result
}

impl<'v> Object<'v> for DecodeError {
    const NAME: &'v str = "DecodeError";
    const MODULE: &'v str = "xml";
    type Annex = DecodeAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let syntax = builder.sym("SYNTAX");
        let eof = builder.sym("EOF");
        let ill_formed = builder.sym("ILL_FORMED");
        let reference = builder.sym("REFERENCE");
        let namespace = builder.sym("NAMESPACE");
        builder
            .nominal_supertype(TypeObject::DataError)
            .get("kind", move |this, strand, out| {
                let kind = match this.annex().kind {
                    DecodeKind::Syntax => syntax,
                    DecodeKind::Eof => eof,
                    DecodeKind::IllFormed => ill_formed,
                    DecodeKind::Reference => reference,
                    DecodeKind::Namespace => namespace,
                };
                Output::set(strand, out, kind);
                Ok(())
            })
            .get("line", |this, strand, out| {
                Output::set(strand, out, this.annex().line);
                Ok(())
            })
            .get("column", |this, strand, out| {
                Output::set(strand, out, this.annex().column);
                Ok(())
            })
    }

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "{}", this.annex().message)
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "<xml.DecodeError ")?;
        Self::display(this, strand, w)?;
        fmt!(strand, w, ">")
    }
}

impl<'v> Object<'v> for EncodeError {
    const NAME: &'v str = "EncodeError";
    const MODULE: &'v str = "xml";
    /// Slot 0 is the path tuple.
    const SLOTS: usize = 1;
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let cycle = builder.sym("CYCLE");
        builder
            .nominal_supertype(TypeObject::ValueError)
            .get("kind", move |_this, strand, out| {
                Output::set(strand, out, cycle);
                Ok(())
            })
            .get("path", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                Output::set(strand, out, Ref::slot::<0>(&borrow));
                Ok(())
            })
    }

    fn display<'a, 's>(
        _this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "{CYCLE}")
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "<xml.EncodeError ")?;
        Self::display(this, strand, w)?;
        fmt!(strand, w, ">")
    }
}
