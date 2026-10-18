use dolang::runtime::{Error, Result, Strand, Value};

use crate::check::{validate_name, validate_text};

/// An expanded name: a local name and an optional namespace URI.
#[derive(Clone)]
pub(crate) struct Name {
    pub(crate) local: String,
    pub(crate) namespace: Option<String>,
}

impl Name {
    /// Splits Clark notation, `{namespace}local` or a bare `local`.
    ///
    /// The namespace ends at the last `}`, which a local name cannot contain.
    pub(crate) fn from_clark(key: &str) -> Self {
        match key.strip_prefix('{').and_then(|rest| rest.rsplit_once('}')) {
            Some((namespace, local)) => Self {
                local: local.to_owned(),
                namespace: Some(namespace.to_owned()),
            },
            None => Self {
                local: key.to_owned(),
                namespace: None,
            },
        }
    }

    pub(crate) fn clark(&self) -> String {
        match &self.namespace {
            Some(namespace) => format!("{{{namespace}}}{}", self.local),
            None => self.local.clone(),
        }
    }

    /// Whether this is the expanded name `{namespace}local`.
    pub(crate) fn is(&self, other: &Name) -> bool {
        self.local == other.local && self.namespace == other.namespace
    }
}

/// An attribute as stored in a [`Node`](crate::node::Node).
pub(crate) struct Attr {
    pub(crate) name: Name,
    pub(crate) value: String,
}

impl Attr {
    /// Creates an attribute after checking its name and value.
    pub(crate) fn checked<'v, 's>(
        strand: &mut Strand<'v, 's>,
        name: Name,
        value: String,
    ) -> Result<'v, 's, Self> {
        validate_name(strand, &name, "attribute name")?;
        if name.namespace.is_none() && name.local == "xmlns" {
            return Err(Error::value(strand, "the xmlns attribute name is reserved"));
        }
        validate_text(strand, &value, "attribute value")?;
        Ok(Self { name, value })
    }
}

/// Reads a `Str` or `Sym` key; bareword dict keys intern as the latter.
pub(crate) fn key_string<'v, 's>(
    strand: &mut Strand<'v, 's>,
    key: &Value<'v>,
) -> Result<'v, 's, Option<String>> {
    if key.as_str(strand).is_none() && key.as_sym(strand).is_none() {
        return Ok(None);
    }
    key.to_string(strand).map(Some)
}

/// Reads an attribute key in Clark notation.
pub(crate) fn attr_key<'v, 's>(
    strand: &mut Strand<'v, 's>,
    key: &Value<'v>,
) -> Result<'v, 's, Name> {
    match key_string(strand, key)? {
        Some(key) => Ok(Name::from_clark(&key)),
        None => Err(Error::type_error(
            strand,
            "attribute name: expected Str or Sym",
        )),
    }
}

pub(crate) fn required_string<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    field: &str,
) -> Result<'v, 's, String> {
    value
        .as_str(strand)
        .map(|value| value.to_string())
        .ok_or_else(|| Error::type_error(strand, format!("{field}: expected Str")))
}

pub(crate) fn optional_string<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: Option<&Value<'v>>,
    field: &str,
) -> Result<'v, 's, Option<String>> {
    match value {
        None => Ok(None),
        Some(value) if value.is_nil() => Ok(None),
        Some(value) => required_string(strand, value, field).map(Some),
    }
}
