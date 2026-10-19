use dolang::runtime::{Error, Result, Strand, Value};

use crate::check::{validate_name, validate_text};

/// An expanded name: a local name and an optional namespace URI.
#[derive(Clone)]
pub(crate) struct Name {
    pub(crate) local: String,
    pub(crate) namespace: Option<String>,
}

impl Name {
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

/// An attribute as stored in a [`Element`](crate::element::Element).
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

/// Reads a canonical attribute key, without validating XML names.
pub(crate) fn attr_name<'v, 's>(
    strand: &mut Strand<'v, 's>,
    key: &Value<'v>,
) -> Result<'v, 's, Option<Name>> {
    if let Some(local) = key.as_str(strand) {
        return Ok(Some(Name {
            local: local.to_string(),
            namespace: None,
        }));
    }
    let Some(tuple) = key.as_tuple(strand) else {
        return Ok(None);
    };
    if tuple.len() != 2 {
        return Ok(None);
    }
    strand.with_slots_sync(|strand, [mut namespace, mut local]| {
        tuple.get(strand, 0, &mut namespace)?;
        tuple.get(strand, 1, &mut local)?;
        let Some(namespace) = namespace.as_str(strand).map(|v| v.to_string()) else {
            return Ok(None);
        };
        let Some(local) = local.as_str(strand).map(|v| v.to_string()) else {
            return Ok(None);
        };
        Ok(Some(Name {
            local,
            namespace: Some(namespace),
        }))
    })
}

/// Requires a canonical attribute key.
pub(crate) fn attr_key<'v, 's>(
    strand: &mut Strand<'v, 's>,
    key: &Value<'v>,
) -> Result<'v, 's, Name> {
    attr_name(strand, key)?
        .ok_or_else(|| Error::type_error(strand, "attribute name: expected Str or (Str, Str)"))
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
