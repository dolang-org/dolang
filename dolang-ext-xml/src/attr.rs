use crate::global::Tag;

use dolang::runtime::object::fmt;
use dolang::runtime::value::fmt::Format;

use dolang::runtime::{
    Args, Error, Instance, Object, Output, Result, Slot, Strand, Type, Value, object::TypeBuilder,
    unpack, value::Nil,
};

#[derive(Clone)]
pub(crate) struct Name {
    pub(crate) local: String,
    pub(crate) namespace: Option<String>,
    pub(crate) prefix: Option<String>,
}

impl Name {
    pub(crate) fn qname(&self) -> String {
        match &self.prefix {
            Some(prefix) => format!("{prefix}:{}", self.local),
            None => self.local.clone(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Attr {
    pub(crate) name: Name,
    pub(crate) value: String,
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

impl<'v> Object<'v> for Attr {
    const MODULE: &'static str = "xml";
    const NAME: &'static str = "Attr";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = strand.state::<Tag>();
        let namespace = global.syms.namespace;
        let prefix = global.syms.prefix;
        let ([name, value], [namespace, prefix]) =
            unpack!(strand, args, 2, 0, namespace = None, prefix = None)?;
        let name = Name {
            local: required_string(strand, &name, "name")?,
            namespace: optional_string(strand, namespace.as_deref(), "namespace")?,
            prefix: optional_string(strand, prefix.as_deref(), "prefix")?,
        };
        let value = required_string(strand, &value, "value")?;
        this.create(strand, Attr { name, value }, out);
        Ok(())
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        fmt!(strand, w, "<xml.Attr {}>", borrow.name.qname())
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("name", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                Output::set(strand, out, borrow.name.local.as_str());
                Ok(())
            })
            .get("value", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                Output::set(strand, out, borrow.value.as_str());
                Ok(())
            })
            .set("value", |this, strand, value| {
                this.borrow_mut(strand)?.value = required_string(strand, &value, "value")?;
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
            .get("prefix", |this, strand, out| {
                if let Some(prefix) = &this.borrow(strand)?.name.prefix {
                    Output::set(strand, out, prefix.as_str());
                } else {
                    Output::set(strand, out, Nil);
                }
                Ok(())
            })
            .get("qname", |this, strand, out| {
                let qname = this.borrow(strand)?.name.qname();
                Output::set(strand, out, qname.as_str());
                Ok(())
            })
    }
}
