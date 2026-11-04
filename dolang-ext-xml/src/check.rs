//! Local well-formedness checks, applied as values enter a tree so that
//! serialization never meets an invalid name or string.

use std::result;

use dolang::runtime::{Error, Result, Strand};

use crate::attr::Name;

/// A failed check, with its message. Constructors raise it as `ValueError`;
/// `xml.decode` raises it as `xml.DecodeError`.
pub(crate) struct Invalid(pub(crate) String);

pub(crate) type Checked<T = ()> = result::Result<T, Invalid>;

/// Raises a failed check as `ValueError`.
pub(crate) trait OrValue<T> {
    fn or_value<'v, 's>(self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, T>;
}

impl<T> OrValue<T> for Checked<T> {
    fn or_value<'v, 's>(self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, T> {
        self.map_err(|Invalid(message)| Error::value(strand, message))
    }
}

pub(crate) const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
pub(crate) const XMLNS_NS: &str = "http://www.w3.org/2000/xmlns/";

pub(crate) fn validate_ncname(name: &str, kind: &str) -> Checked {
    let mut chars = name.chars();
    if !chars.next().is_some_and(is_name_start) || !chars.all(is_name_char) {
        return Err(Invalid(format!("invalid XML {kind} {name:?}")));
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

pub(crate) fn validate_text(text: &str, kind: &str) -> Checked {
    // A `str` has no surrogates, so XML 1.0 forbids only C0 controls other
    // than tab, LF, and CR, and U+FFFE and U+FFFF (`EF BF BE`, `EF BF BF`).
    // Scanning bytes finds both without decoding characters.
    let bytes = text.as_bytes();
    let forbidden = bytes.iter().enumerate().any(|(i, &b)| match b {
        b'\t' | b'\n' | b'\r' => false,
        0..0x20 => true,
        0xEF => {
            bytes[i + 1..].starts_with(&[0xBF, 0xBE]) || bytes[i + 1..].starts_with(&[0xBF, 0xBF])
        }
        _ => false,
    });
    if forbidden {
        return Err(Invalid(format!(
            "{kind} contains a character forbidden by XML 1.0"
        )));
    }
    Ok(())
}

pub(crate) fn validate_namespace(namespace: Option<&str>, kind: &str) -> Checked {
    let Some(namespace) = namespace else {
        return Ok(());
    };
    if namespace.is_empty() {
        return Err(Invalid(format!("{kind} has an empty namespace URI")));
    }
    if namespace == XMLNS_NS {
        return Err(Invalid(format!("{kind} uses the reserved xmlns namespace")));
    }
    validate_text(namespace, "namespace URI")
}

pub(crate) fn validate_name(name: &Name, kind: &str) -> Checked {
    validate_ncname(&name.local, kind)?;
    validate_namespace(name.namespace.as_deref(), kind)
}

/// Checks one namespace declaration; `""` is the default namespace, which an
/// empty URI undeclares.
pub(crate) fn validate_namespace_binding(prefix: &str, namespace: &str) -> Checked {
    validate_text(namespace, "namespace URI")?;
    if prefix == "xmlns" || namespace == XMLNS_NS {
        return Err(Invalid("the xmlns namespace is reserved".into()));
    }
    if prefix == "xml" && namespace != XML_NS {
        return Err(Invalid(
            "the xml prefix must map to the XML namespace".into(),
        ));
    }
    if namespace == XML_NS && prefix != "xml" {
        return Err(Invalid("the XML namespace must use the xml prefix".into()));
    }
    if !prefix.is_empty() {
        validate_ncname(prefix, "namespace prefix")?;
        if namespace.is_empty() {
            return Err(Invalid(
                "a named namespace prefix cannot map to an empty URI".into(),
            ));
        }
    }
    Ok(())
}
