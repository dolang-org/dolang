use std::{
    fmt,
    hash::{Hash, Hasher},
};

use dolang::runtime::{
    Args, Error, Instance, Object, Output, Result, Slot, State, Strand, Type, Value,
    error::ResultExt,
    object::{ArrayLike, ArrayView, DictLike, DictView, DictViewSink, TypeBuilder},
    unpack,
    value::{Nil, Str},
    vm::Builder,
};
use percent_encoding::percent_decode_str;

use crate::global::Global;

pub(crate) struct Url;

pub(crate) struct UrlAnnex<'v> {
    inner: url::Url,
    global: State<'v, Global<'v>>,
}

enum UrlOrStr<'v, 'a> {
    Url(Instance<'v, 'a, Url>),
    Str(Str<'v, 'a>),
}

impl<'v, 'a> UrlOrStr<'v, 'a> {
    fn new<'s>(
        strand: &mut Strand<'v, 's>,
        global: State<'v, Global<'v>>,
        value: &'a Value<'v>,
    ) -> Result<'v, 's, Self> {
        if let Some(url) = global.types.url.downcast(value) {
            Ok(Self::Url(url))
        } else if let Some(str) = value.as_str(strand) {
            Ok(Self::Str(str))
        } else {
            Err(Error::type_error(strand, "expected Url or str"))
        }
    }

    fn to_url<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, url::Url> {
        match self {
            Self::Url(url) => Ok(url.annex().inner.clone()),
            Self::Str(str) => strand
                .access(|x| url::Url::parse(str.as_str(x)))
                .into_do(strand),
        }
    }
}

struct Segments;

impl<'v> ArrayLike<'v> for Segments {
    type Object = Url;
    const MODULE: &'v str = "url";
    const NAME: &'v str = "Segments";

    fn len(this: Instance<'v, '_, Url>, _strand: &mut Strand<'v, '_>) -> usize {
        this.annex()
            .inner
            .path_segments()
            .into_iter()
            .flatten()
            .count()
    }

    fn get<'a, 's>(
        this: Instance<'v, '_, Url>,
        strand: &'a mut Strand<'v, 's>,
        index: usize,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        let segment = annex
            .inner
            .path_segments()
            .into_iter()
            .flatten()
            .nth(index)
            .expect("array view index was normalized");
        Output::set(strand, out, decode_segment(segment).as_str());
        Ok(())
    }
}

struct Query;

impl<'v> DictLike<'v> for Query {
    type Object = Url;
    const MODULE: &'v str = "url";
    const NAME: &'v str = "Query";

    fn len(this: Instance<'v, '_, Url>, _strand: &mut Strand<'v, '_>) -> usize {
        this.annex().inner.query_pairs().count()
    }

    fn get<'a, 's>(
        this: Instance<'v, '_, Url>,
        strand: &'a mut Strand<'v, 's>,
        key: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let Some(key) = key.as_str(strand) else {
            return Ok(false);
        };
        let found = strand.access(|x| {
            this.annex()
                .inner
                .query_pairs()
                .find(|(candidate, _)| candidate == key.as_str(x))
                .map(|(_, value)| value.into_owned())
        });
        if let Some(value) = found {
            Output::set(strand, out, value.as_str());
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn flatten<'s>(
        this: Instance<'v, '_, Url>,
        strand: &mut Strand<'v, 's>,
        sink: &mut DictViewSink<'v, '_>,
    ) -> Result<'v, 's, ()> {
        for (key, value) in this.annex().inner.query_pairs() {
            sink.push(strand, key.as_ref(), value.as_ref());
        }
        Ok(())
    }
}

fn create_url_with_global<'v, 'a>(
    global: State<'v, Global<'v>>,
    strand: &mut Strand<'v, '_>,
    url: url::Url,
    out: Slot<'v, 'a>,
) {
    global
        .types
        .url
        .create_with_annex(strand, Url, UrlAnnex { inner: url, global }, out);
}

/// Creates a Do `url.Url` object from an owned `url::Url`.
pub fn create_url<'v, 'a>(strand: &mut Strand<'v, '_>, url: url::Url, out: Slot<'v, 'a>) {
    let global = strand.state::<Global<'v>>();
    create_url_with_global(global, strand, url, out);
}

/// Converts a Do `url.Url` or `str` value into an owned `url::Url`.
pub fn value_to_url<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
) -> Result<'v, 's, url::Url> {
    let global = strand.state::<Global<'v>>();
    UrlOrStr::new(strand, global, value)?.to_url(strand)
}

fn decode_segment(segment: &str) -> String {
    percent_decode_str(segment).decode_utf8_lossy().into_owned()
}

fn url_name(url: &url::Url) -> Option<String> {
    let segment = url.path_segments().into_iter().flatten().last()?;
    if segment.is_empty() {
        None
    } else {
        Some(decode_segment(segment))
    }
}

impl<'v> Object<'v> for Url {
    const NAME: &'v str = "Url";
    const MODULE: &'v str = "url";
    type Annex = UrlAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([value], []) = unpack!(strand, args, 1, 0)?;
        let url = value_to_url(strand, &value)?;
        let global = strand.state::<Global<'v>>();
        this.create_with_annex(strand, Url, UrlAnnex { inner: url, global }, out);
        Ok(())
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<url.Url {:?}>", this.annex().inner.as_str()).into_do(strand)
    }

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "{}", this.annex().inner).into_do(strand)
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("scheme", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.scheme());
                Ok(())
            })
            .get("username", |this, strand, out| {
                let annex = this.annex();
                let username = annex.inner.username();
                if username.is_empty() {
                    Output::set(strand, out, Nil);
                } else {
                    Output::set(strand, out, username);
                }
                Ok(())
            })
            .get("password", |this, strand, out| {
                if let Some(password) = this.annex().inner.password() {
                    Output::set(strand, out, password);
                } else {
                    Output::set(strand, out, Nil);
                }
                Ok(())
            })
            .get("host", |this, strand, out| {
                if let Some(host) = this.annex().inner.host_str() {
                    Output::set(strand, out, host);
                } else {
                    Output::set(strand, out, Nil);
                }
                Ok(())
            })
            .get("port", |this, strand, out| {
                if let Some(port) = this.annex().inner.port() {
                    Output::set(strand, out, port);
                } else {
                    Output::set(strand, out, Nil);
                }
                Ok(())
            })
            .get("path", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.path());
                Ok(())
            })
            .get("name", |this, strand, out| {
                if let Some(name) = url_name(&this.annex().inner) {
                    Output::set(strand, out, name.as_str());
                } else {
                    Output::set(strand, out, Nil);
                }
                Ok(())
            })
            .get("fragment", |this, strand, out| {
                if let Some(fragment) = this.annex().inner.fragment() {
                    Output::set(strand, out, fragment);
                } else {
                    Output::set(strand, out, Nil);
                }
                Ok(())
            })
            .get("query_raw", |this, strand, out| {
                if let Some(query) = this.annex().inner.query() {
                    Output::set(strand, out, query);
                } else {
                    Output::set(strand, out, Nil);
                }
                Ok(())
            })
            .get("segments", |this, strand, out| {
                Output::set(strand, out, ArrayView::<Segments>::new(this));
                Ok(())
            })
            .get("query", |this, strand, out| {
                Output::set(strand, out, DictView::<Query>::new(this));
                Ok(())
            })
            .method("with_query_raw", async move |this, strand, args, out| {
                let ([query], []) = unpack!(strand, args, 1, 0)?;
                let mut url = this.annex().inner.clone();
                if query.is_nil() {
                    url.set_query(None);
                } else {
                    let query = query
                        .as_str(strand)
                        .ok_or_else(|| Error::type_error(strand, "expected str or nil"))?;
                    strand.access(|x| url.set_query(Some(query.as_str(x))))
                }
                create_url_with_global(this.annex().global, strand, url, out);
                Ok(())
            })
            .method("with_fragment", async move |this, strand, args, out| {
                let ([fragment], []) = unpack!(strand, args, 1, 0)?;
                let mut url = this.annex().inner.clone();
                if fragment.is_nil() {
                    url.set_fragment(None);
                } else {
                    let fragment = fragment
                        .as_str(strand)
                        .ok_or_else(|| Error::type_error(strand, "expected str or nil"))?;
                    strand.access(|x| url.set_fragment(Some(fragment.as_str(x))));
                }
                create_url_with_global(this.annex().global, strand, url, out);
                Ok(())
            })
            .method_with_slots(
                "with_query",
                async move |this, strand, args, out, [mut iter, mut item, mut key, mut value]| {
                    let ([pairs], []) = unpack!(strand, args, 1, 0)?;
                    let mut url = this.annex().inner.clone();
                    {
                        let mut query = url.query_pairs_mut();
                        query.clear();
                        pairs.iter(strand, &mut iter).await?;
                        while iter.next(strand, &mut item).await? {
                            item.index(strand, 0, &mut key)?;
                            item.index(strand, 1, &mut value)?;
                            query.append_pair(&key.to_string(strand)?, &value.to_string(strand)?);
                        }
                    }
                    create_url_with_global(this.annex().global, strand, url, out);
                    Ok(())
                },
            )
    }

    fn eq<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        if let Some(other) = this.annex().global.types.url.downcast(other) {
            Ok(this.annex().inner == other.annex().inner)
        } else {
            Err(Error::not_supported(strand))
        }
    }

    fn hash<'a, 's>(
        this: Instance<'v, 'a, Self>,
        _strand: &'a mut Strand<'v, 's>,
        hasher: &mut impl Hasher,
    ) -> Result<'v, 's, ()> {
        this.annex().inner.hash(hasher);
        Ok(())
    }

    fn lt<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        if let Some(other) = this.annex().global.types.url.downcast(other) {
            Ok(this.annex().inner < other.annex().inner)
        } else {
            Err(Error::not_supported(strand))
        }
    }

    fn div<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let segment = other
            .as_str(strand)
            .ok_or_else(|| Error::type_error(strand, "expected `str`"))?
            .to_string();
        let mut url = this.annex().inner.clone();
        if segment.starts_with('/') {
            url = url.join(&segment).into_do(strand)?;
        } else {
            url.path_segments_mut()
                .map_err(|_| Error::runtime(strand, "URL cannot be extended with path segments"))?
                .push(&segment);
        }
        create_url_with_global(this.annex().global, strand, url, out);
        Ok(())
    }
}

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    builder
        .module("url")
        .value("Url", global.types.url)
        .commit();
}
