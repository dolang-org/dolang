use crate::global::Tag;

use std::{borrow::Cow, mem, rc::Rc};

use dolang::runtime::object::fmt;

use dolang::runtime::{
    AllocExt, Args, Error, Input, Instance, Object, Result, Slot, State, Strand, Type, Value, call,
    object::{Mut, Ref, Rest, TypeBuilder, Unpack, UnpackItem},
    unpack,
    value::{
        Nil, PinStr, TypeObject, View,
        fmt::{Format, Kind, Pad},
    },
    vm::Register,
    with_instance, with_slots,
};
use regex as rx;

use crate::global::Global;

pub(crate) fn configure_vm<'v>(builder: &mut Register<'v>, global: State<'v, Global<'v>>) {
    builder
        .module("regex")
        .value("Regex", global.types.regex)
        .value("PatternError", global.types.pattern_error)
        .value("Captures", global.types.captures)
        .value("Match", global.types.match_)
        .commit();
}

pub(crate) struct Regex;

/// `regex.PatternError`: a pattern that `Regex` cannot compile.
pub(crate) struct PatternError;

/// Why the pattern was rejected.
#[derive(Clone, Copy)]
enum PatternKind {
    /// The pattern is not valid syntax.
    Syntax,
    /// The compiled pattern would exceed the size limit.
    TooBig,
}

pub(crate) struct PatternAnnex {
    message: String,
    kind: PatternKind,
}

/// Raises `regex.PatternError` for `error`.
fn pattern_error<'v, 's>(strand: &mut Strand<'v, 's>, error: rx::Error) -> Error<'v, 's> {
    let kind = match error {
        rx::Error::CompiledTooBig(_) => PatternKind::TooBig,
        _ => PatternKind::Syntax,
    };
    let global = strand.state::<Tag>();
    Error::object_with_annex(
        strand,
        global.types.pattern_error,
        PatternError,
        PatternAnnex {
            message: error.to_string(),
            kind,
        },
    )
}

impl<'v> Object<'v> for PatternError {
    const NAME: &'v str = "PatternError";
    const MODULE: &'v str = "regex";
    type Annex = PatternAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let syntax = builder.sym("SYNTAX");
        let too_big = builder.sym("TOO_BIG");
        builder
            .nominal_supertype(TypeObject::ValueError)
            .get("kind", move |this, strand, out| {
                let kind = match this.annex().kind {
                    PatternKind::Syntax => syntax,
                    PatternKind::TooBig => too_big,
                };
                strand.store(out, kind);
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
        fmt!(strand, w, "<regex.PatternError ")?;
        Self::display(this, strand, w)?;
        fmt!(strand, w, ">")
    }
}

pub(crate) struct RegexAnnex<'v> {
    global: State<'v, Global<'v>>,
    regex: rx::Regex,
    groups: Rc<Groups>,
}

impl<'v> Object<'v> for Regex {
    const NAME: &'v str = "Regex";
    const MODULE: &'v str = "regex";
    type Annex = RegexAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        unpack!(strand, args => pattern);
        let pattern = pattern
            .as_str(strand.vm())
            .ok_or_else(|| Error::type_error(strand, "pattern: expected Str"))?
            .pin();
        let regex = rx::Regex::new(&pattern).map_err(|err| pattern_error(strand, err))?;
        let global = strand.state::<Tag>();
        let groups = Rc::new(Groups::new(&regex));
        this.create_with_annex(
            strand,
            Regex,
            RegexAnnex {
                global,
                regex,
                groups,
            },
            out,
        );
        Ok(())
    }

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let limit = builder.sym("limit");
        builder
            .method("match", async move |this, strand, args, mut out| {
                let annex = this.annex();
                unpack!(strand, args => haystack_value);
                let haystack = haystack_value
                    .as_str(strand.vm())
                    .ok_or_else(|| Error::type_error(strand, "expected `Str`"))?.pin();
                match annex.regex.captures(&haystack) {
                    Some(caps) => {
                        // SAFETY: We transmute the captures to have 'static lifetime.
                        // The haystack pin, owned by the Captures object, keeps the text
                        // alive and in place.
                        annex.global.types.captures.create_with_annex(
                            strand,
                            Captures {
                                caps: unsafe {
                                    mem::transmute::<rx::Captures<'_>, rx::Captures<'static>>(caps)
                                },
                                _haystack: haystack,
                            },
                            CapturesAnnex {
                                global: annex.global,
                                groups: annex.groups.clone(),
                            },
                            &mut out,
                        );

                        // Store the haystack in slot 0 of the Captures object
                        let cast = annex.global.types.captures.cast(&out).unwrap();
                        with_instance!(strand, cast, |inst| {
                                let mut captures = inst.borrow_mut_unwrap();
                                strand.store(Mut::slot_mut::<0>(&mut captures), haystack_value);
                            });
                    }
                    None => {
                        strand.store(out, Nil);
                    }
                }
                Ok(())
            })
            .method("find", async move |this, strand, args, mut out| {
                let annex = this.annex();
                unpack!(strand, args => haystack_value);
                let haystack = haystack_value
                    .as_str(strand.vm())
                    .ok_or_else(|| Error::type_error(strand, "expected `Str`"))?.pin();
                // Create the captures iterator
                let iter = annex.regex.captures_iter(&haystack);

                // SAFETY: We transmute the iterator to have 'static lifetime.
                // The regex is stored in slot 0, and the haystack pin, owned by the Find
                // object, keeps the text alive and in place.
                let iter = unsafe {
                    mem::transmute::<rx::CaptureMatches<'_, '_>, rx::CaptureMatches<'static, 'static>>(
                        iter,
                    )
                };

                annex.global.types.find.create_with_annex(
                    strand,
                    Find {
                        iter,
                        _haystack: haystack,
                    },
                    FindAnnex {
                        global: annex.global,
                        groups: annex.groups.clone(),
                    },
                    &mut out,
                );

                // Store regex instance in slot 0 and haystack in slot 1 of the Find object
                let cast = annex.global.types.find.cast(&out).unwrap();
                with_instance!(strand, cast, |inst| {
                        let mut iter = inst.borrow_mut_unwrap();
                        strand.store(Mut::slot_mut::<0>(&mut iter), this);
                        strand.store(Mut::slot_mut::<1>(&mut iter), haystack_value);
                    });
                Ok(())
            })
            .method_with_slots(
                "replace",
                async move |this, strand, args, out, [mut caps_slot, mut cb_out]| {
                    let annex = this.annex();

                    unpack!(strand, args => haystack_value, replacement, ?:limit);
                    let haystack = haystack_value
                        .as_str(strand.vm())
                        .ok_or_else(|| Error::type_error(strand, "expected `Str`"))?;
                    let haystack = haystack.pin();
                    let limit_val = limit
                        .map(|l| {
                            l.to_i64(strand)
                                .map_err(|_| Error::type_error(strand, "limit: expected `Int`"))
                        })
                        .transpose()?;

                    if let Some(rep) = replacement.as_str(strand) {
                        // A Str is literal replacement text. Capture expansion belongs to Fmt.
                        let result = match limit_val {
                            None => strand.access(|x| {
                                annex
                                    .regex
                                    .replace_all(&haystack, rx::NoExpand(rep.as_str(x)))
                            }),
                            Some(0) => Cow::Borrowed(&*haystack),
                            Some(n) if n > 0 => strand.access(|x| {
                                annex.regex.replacen(
                                    &haystack,
                                    n as usize,
                                    rx::NoExpand(rep.as_str(x)),
                                )
                            }),
                            Some(_) => {
                                return Err(Error::value(strand, "limit must be >= 0"))
                            }
                        };
                        strand.store(out, result.as_ref());
                    } else if replacement.as_fmt(strand.vm()).is_some() {
                        let max = replacement_limit(strand, limit_val)?;
                        let mut result = String::new();
                        let mut last_end = 0;
                        for (count, caps) in annex.regex.captures_iter(&haystack).enumerate() {
                            if count >= max {
                                break;
                            }
                            let matched = caps.get(0).unwrap();
                            result.push_str(&haystack[last_end..matched.start()]);
                            render_replacement(
                                strand,
                                &replacement,
                                &annex.groups,
                                &caps,
                                &mut result,
                            )?;
                            last_end = matched.end();
                        }
                        result.push_str(&haystack[last_end..]);
                        strand.store(out, result.as_str());
                    } else {
                        // Callback replacement: iterate matches manually
                        let max = replacement_limit(strand, limit_val)?;
                        let mut result = String::new();
                        let mut last_end = 0;
                        for (count, caps) in annex.regex.captures_iter(&haystack).enumerate() {
                            if count >= max {
                                break;
                            }
                            let m = caps.get(0).unwrap();
                            result.push_str(&haystack[last_end..m.start()]);
                            let callback_haystack =
                                haystack.clone();

                            // Create Captures object for the callback
                            annex.global.types.captures.create_with_annex(
                                strand,
                                Captures {
                                    caps: unsafe {
                                        mem::transmute::<rx::Captures<'_>, rx::Captures<'static>>(
                                            caps,
                                        )
                                    },
                                    _haystack: callback_haystack,
                                },
                                CapturesAnnex {
                                    global: annex.global,
                                    groups: annex.groups.clone(),
                                },
                                &mut caps_slot,
                            );
                            // Store the haystack in Captures slot 0
                            let cast = annex.global.types.captures.cast(&caps_slot).unwrap();
                            with_instance!(strand, cast, |inst| {
                                    let mut captures_mut = inst.borrow_mut_unwrap();
                                    strand.store(
                                        Mut::slot_mut::<0>(&mut captures_mut),
                                        &haystack_value,
                                    );
                                });

                            // Call the replacement function with the Captures
                            call!(strand, &replacement, &caps_slot, &mut cb_out)?;

                            let rep = cb_out
                                .as_str(strand)
                                .ok_or_else(|| {
                                    Error::type_error(
                                        strand,
                                        "replacement callback must return `Str`",
                                    )
                                })?;
                            strand.access(|x| result.push_str(rep.as_str(x)));

                            last_end = m.end();
                        }
                        result.push_str(&haystack[last_end..]);
                        strand.store(out, result.as_str());
                    }
                    Ok(())
                },
            )
            .method("split", async move |this, strand, args, mut out| {
                let annex = this.annex();
                unpack!(strand, args => haystack_value, ?:limit);
                let haystack = haystack_value
                    .as_str(strand.vm())
                    .ok_or_else(|| Error::type_error(strand, "expected `Str`"))?;
                let haystack = haystack.pin();
                let limit_i64 = limit
                    .map(|l| {
                        l.to_i64(strand)
                            .map_err(|_| Error::type_error(strand, "limit: expected `Int`"))
                    })
                    .transpose()?;

                let inner = match limit_i64 {
                    Some(l) if l < 0 => {
                        // Negative limit: N splits from the rear, yield forward
                        let n = l.unsigned_abs() as usize;
                        let matches: Vec<_> = annex.regex.find_iter(&haystack).collect();
                        let skip = matches.len().saturating_sub(n);
                        let splits = &matches[skip..];
                        let mut segs = Vec::with_capacity(splits.len() + 1);
                        let mut pos = 0;
                        // First segment: everything before the first kept split
                        if let Some(first) = splits.first() {
                            segs.push(haystack[..first.start()].to_string());
                            pos = first.end();
                        }
                        // Remaining segments between kept splits
                        for m in splits.iter().skip(1) {
                            segs.push(haystack[pos..m.start()].to_string());
                            pos = m.end();
                        }
                        // Final segment: everything after last split
                        if !splits.is_empty() {
                            segs.push(haystack[pos..].to_string());
                        } else {
                            segs.push(haystack.to_string());
                        }
                        RegexSplitInner::Buffered {
                            segments: segs,
                            index: 0,
                        }
                    }
                    _ => {
                        // Forward lazy: wrap rx::Split iterator
                        let limit_usize = limit_i64
                            .map(|l| {
                                l.try_into().map_err(|_| Error::overflow(strand))
                            })
                            .transpose()?
                            .unwrap_or(usize::MAX);
                        if limit_usize == usize::MAX {
                            // Unlimited: use rx::Regex::split
                            let iter = annex.regex.split(&haystack);
                            // SAFETY: transmute to 'static; haystack kept alive in slot 0
                            let iter = unsafe {
                                mem::transmute::<rx::Split<'_, '_>, rx::Split<'static, 'static>>(
                                    iter,
                                )
                            };
                            RegexSplitInner::Lazy(iter)
                        } else {
                            // Limited: use rx::Regex::splitn
                            let iter = annex.regex.splitn(&haystack, limit_usize + 1);
                            // SAFETY: transmute to 'static; haystack kept alive in slot 0
                            let iter = unsafe {
                                mem::transmute::<
                                    rx::SplitN<'_, '_>,
                                    rx::SplitN<'static, 'static>,
                                >(iter)
                            };
                            RegexSplitInner::LazyN(iter)
                        }
                    }
                };

                annex.global.types.split.create_with_annex(
                    strand,
                    RegexSplit {
                        inner,
                        _haystack: haystack,
                    },
                    RegexSplitAnnex,
                    &mut out,
                );

                // Store regex in slot 0, haystack in slot 1
                let cast = annex.global.types.split.cast(&out).unwrap();
                with_instance!(strand, cast, |inst| {
                        let mut borrow = inst.borrow_mut_unwrap();
                        strand.store(Mut::slot_mut::<0>(&mut borrow), this);
                        strand.store(Mut::slot_mut::<1>(&mut borrow), haystack_value);
                    });
                Ok(())
            })
            .method("rsplit", async move |this, strand, args, mut out| {
                let annex = this.annex();
                unpack!(strand, args => haystack_value, ?:limit);
                let haystack = haystack_value
                    .as_str(strand.vm())
                    .ok_or_else(|| Error::type_error(strand, "expected `Str`"))?;
                let haystack = haystack.pin();
                let limit_i64 = limit
                    .map(|l| {
                        l.to_i64(strand)
                            .map_err(|_| Error::type_error(strand, "limit: expected `Int`"))
                    })
                    .transpose()?;

                let inner = {
                    let matches: Vec<_> = annex.regex.find_iter(&haystack).collect();
                    let selected = match limit_i64 {
                        Some(l) if l < 0 => {
                            // Negative limit: N splits from front, yield backward
                            let n = l.unsigned_abs() as usize;
                            let end = matches.len().min(n);
                            &matches[..end]
                        }
                        Some(n) => {
                            // Positive limit: N splits from rear, yield backward
                            let skip = matches.len().saturating_sub(n as usize);
                            &matches[skip..]
                        }
                        None => &matches[..],
                    };
                    let mut segs = Vec::with_capacity(selected.len() + 1);
                    let mut pos = 0;
                    for m in selected {
                        segs.push(haystack[pos..m.start()].to_string());
                        pos = m.end();
                    }
                    segs.push(haystack[pos..].to_string());
                    segs.reverse();
                    RegexSplitInner::Buffered {
                        segments: segs,
                        index: 0,
                    }
                };

                annex.global.types.split.create_with_annex(
                    strand,
                    RegexSplit {
                        inner,
                        _haystack: haystack,
                    },
                    RegexSplitAnnex,
                    &mut out,
                );

                // Store regex in slot 0, haystack in slot 1
                let cast = annex.global.types.split.cast(&out).unwrap();
                with_instance!(strand, cast, |inst| {
                        let mut borrow = inst.borrow_mut_unwrap();
                        strand.store(Mut::slot_mut::<0>(&mut borrow), this);
                        strand.store(Mut::slot_mut::<1>(&mut borrow), haystack_value);
                    });
                Ok(())
            })
    }
}

fn replacement_limit<'v, 's>(
    strand: &mut Strand<'v, 's>,
    limit: Option<i64>,
) -> Result<'v, 's, usize> {
    match limit {
        None => Ok(usize::MAX),
        Some(n) if n >= 0 => Ok(n as usize),
        Some(_) => Err(Error::value(strand, "limit must be >= 0")),
    }
}

/// Renders a native template against one set of captures.
fn render_replacement<'v, 's>(
    strand: &mut Strand<'v, 's>,
    template: &Value<'v>,
    groups: &Groups,
    captures: &rx::Captures<'_>,
    out: &mut dyn Format<'v>,
) -> Result<'v, 's, ()> {
    let template = template.as_fmt(strand.vm()).unwrap();
    let len = template.len(strand)?;
    with_slots!(strand, |mut segment, mut value| {
        for index in 0..len {
            template.get(strand, index, &mut segment)?;
            match segment.view(strand.vm()) {
                View::Str(text) => {
                    let text = text.to_string();
                    out.write_str(strand, &text)?;
                }
                View::FmtValue(bound) => {
                    let mut spec = bound.spec(strand);
                    let kind = *spec.kind.get_or_insert(Kind::Str);
                    bound.value(strand, &mut value)?;
                    if kind == Kind::Str && value.as_fmt(strand.vm()).is_some() {
                        let mut padded = Pad::new(spec, out);
                        render_replacement(strand, &value, groups, captures, &mut padded)?;
                        padded.finish(strand)?;
                    } else {
                        value.fmt(strand, &spec, out)?;
                    }
                }
                View::FmtParam(param) => {
                    let mut spec = param.spec(strand);
                    spec.kind.get_or_insert(Kind::Str);
                    param.name(strand, &mut value)?;
                    let lookup = match value.view(strand.vm()) {
                        View::Int(index) => usize::try_from(index)
                            .map_or(Lookup::NoSuchGroup, |index| groups.pos(captures, index)),
                        View::Sym(name) => groups.key(captures, name.as_str(strand)),
                        _ => unreachable!("FmtParam names are Int or Sym"),
                    };
                    let Lookup::Present(matched) = lookup else {
                        return Err(missing_capture(strand, &value));
                    };
                    strand.store(&mut value, matched.as_str());
                    value.fmt(strand, &spec, out)?;
                }
                _ => unreachable!("Fmt segments are Str, FmtValue, or FmtParam"),
            }
        }
        Ok(())
    })
}

fn missing_capture<'v, 's>(strand: &mut Strand<'v, 's>, name: &Value<'v>) -> Error<'v, 's> {
    match name
        .as_int(strand)
        .and_then(|index| usize::try_from(index).ok())
    {
        Some(index) => Error::missing_positional(strand, index),
        None => Error::missing_key(strand, name),
    }
}

/// How a `Regex` numbers its groups: the overall match, then each unnamed group,
/// are positional; named groups are keyed and take no number.
pub(crate) struct Groups {
    /// Crate indices of the positional items.
    positional: Box<[usize]>,
    /// Named groups in pattern order, with their crate indices. A group's place
    /// in this list is its ordinal.
    named: Box<[(Box<str>, usize)]>,
}

/// The result of looking up a group in a set of captures.
enum Lookup<'h> {
    /// The pattern has no such group.
    NoSuchGroup,
    /// The group didn't participate in the match.
    Absent,
    Present(rx::Match<'h>),
}

impl Groups {
    fn new(regex: &rx::Regex) -> Self {
        let mut positional = Vec::new();
        let mut named = Vec::new();
        for (index, name) in regex.capture_names().enumerate() {
            match name {
                Some(name) => named.push((Box::from(name), index)),
                None => positional.push(index),
            }
        }
        Self {
            positional: positional.into(),
            named: named.into(),
        }
    }

    fn ordinal(&self, name: &str) -> Option<usize> {
        self.named.iter().position(|(group, _)| &**group == name)
    }

    fn lookup<'h>(captures: &rx::Captures<'h>, index: Option<usize>) -> Lookup<'h> {
        match index {
            None => Lookup::NoSuchGroup,
            Some(index) => captures.get(index).map_or(Lookup::Absent, Lookup::Present),
        }
    }

    /// Looks up positional item `n`.
    fn pos<'h>(&self, captures: &rx::Captures<'h>, n: usize) -> Lookup<'h> {
        Self::lookup(captures, self.positional.get(n).copied())
    }

    /// Looks up the named group with ordinal `ordinal`.
    fn named<'h>(&self, captures: &rx::Captures<'h>, ordinal: usize) -> Lookup<'h> {
        Self::lookup(captures, self.named.get(ordinal).map(|(_, index)| *index))
    }

    /// Looks up the group named `name`.
    fn key<'h>(&self, captures: &rx::Captures<'h>, name: &str) -> Lookup<'h> {
        match self.ordinal(name) {
            Some(ordinal) => self.named(captures, ordinal),
            None => Lookup::NoSuchGroup,
        }
    }

    /// Looks up the group that indexing `captures` with `key` reads.
    fn index<'v, 's, 'h>(
        &self,
        strand: &mut Strand<'v, 's>,
        captures: &rx::Captures<'h>,
        key: &Value<'v>,
    ) -> Result<'v, 's, Lookup<'h>> {
        if let Ok(n) = key.to_i64(strand) {
            let n: usize = n.try_into().map_err(|_| Error::overflow(strand))?;
            Ok(self.pos(captures, n))
        } else if let Some(name) = key.as_str(strand.vm()) {
            let name = name.pin();
            Ok(self.key(captures, &name))
        } else if let Some(name) = key.as_sym(strand.vm()) {
            Ok(self.key(captures, name.as_str(strand)))
        } else {
            Err(Error::type_error(strand, "expected `Int`, `Str` or `Sym`"))
        }
    }
}

/// Stores a `Match` for `matched`, a span of `haystack`, in `out`.
fn store_match<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    haystack: &Value<'v>,
    matched: rx::Match<'_>,
    mut out: Slot<'v, '_>,
) {
    // SAFETY: We transmute the match to have 'static lifetime. The haystack pin,
    // owned by the Match object, keeps the text alive and in place.
    let matched = unsafe { mem::transmute::<rx::Match<'_>, rx::Match<'static>>(matched) };
    let pin = haystack.as_str(strand.vm()).unwrap().pin();
    global.types.match_.create_with_annex(
        strand,
        Match {
            match_: matched,
            _haystack: pin,
        },
        MatchAnnex,
        &mut out,
    );
    // Store the haystack in slot 0 of the Match object
    with_instance!(strand, global.types.match_.cast(&out).unwrap(), |inst| {
        let mut borrow = inst.borrow_mut_unwrap();
        strand.store(Mut::slot_mut::<0>(&mut borrow), haystack);
    });
}

/// The items of a `Captures` that a value unpacks.
#[derive(Clone)]
pub(crate) struct CapturesView {
    /// The first positional item, or `None` for no positional items.
    pos_from: Option<usize>,
    /// For each named group, whether it was already taken, or `None` for no keyed
    /// items.
    taken: Option<Box<[bool]>>,
}

impl CapturesView {
    fn full(groups: &Groups) -> Self {
        Self {
            pos_from: Some(0),
            taken: Some(vec![false; groups.named.len()].into()),
        }
    }
}

/// The source of an unpack: a set of captures and the items of it visible.
struct UnpackSource<'v, 'r> {
    global: State<'v, Global<'v>>,
    groups: &'r Groups,
    captures: &'r rx::Captures<'r>,
    haystack: &'r Value<'v>,
    view: &'r CapturesView,
}

/// Unpacks the items of `source` visible in its view.
///
/// Positional items keep their indices: one that didn't participate takes its
/// default and doesn't shift the rest. A rest is a view of the items left over,
/// holes included, over `object`, the `Captures` they come from.
fn unpack_captures<'v, 's>(
    strand: &mut Strand<'v, 's>,
    source: UnpackSource<'v, '_>,
    object: impl Input<'v> + Copy,
    mut unpack: Unpack<'v, '_>,
) -> Result<'v, 's, ()> {
    let UnpackSource {
        global,
        groups,
        captures,
        haystack,
        view,
    } = source;
    let pos_rest = unpack.pos_rest();
    let key_rest = unpack.key_rest();
    let mut position = view.pos_from;
    let mut index = 0;
    let mut taken = view.taken.clone();

    for item in unpack.iter() {
        match item {
            UnpackItem::Pos { slot, default } => {
                let lookup = position.map_or(Lookup::NoSuchGroup, |n| groups.pos(captures, n));
                match (lookup, default) {
                    (Lookup::Present(matched), _) => {
                        store_match(strand, global, haystack, matched, slot)
                    }
                    (_, Some(default)) => strand.store(slot, default),
                    (_, None) => return Err(Error::missing_positional(strand, index)),
                }
                if let Some(n) = &mut position {
                    *n += 1;
                }
                index += 1;
            }
            UnpackItem::SymKey { key, slot, default } => {
                let ordinal = groups.ordinal(key.as_str(strand));
                let found = take(groups, captures, &mut taken, ordinal);
                match (found, default) {
                    (Some(matched), _) => store_match(strand, global, haystack, matched, slot),
                    (None, Some(default)) => strand.store(slot, default),
                    (None, None) => return Err(Error::missing_key(strand, key)),
                }
            }
            UnpackItem::ConstKey { key, slot, default } => {
                let ordinal = key
                    .as_str(strand.vm())
                    .and_then(|name| groups.ordinal(&name.pin()));
                let found = take(groups, captures, &mut taken, ordinal);
                match (found, default) {
                    (Some(matched), _) => store_match(strand, global, haystack, matched, slot),
                    (None, Some(default)) => strand.store(slot, default),
                    (None, None) => return Err(Error::missing_key(strand, key)),
                }
            }
            UnpackItem::Rest { slot } => {
                let view = CapturesView {
                    pos_from: position,
                    taken: taken.clone(),
                };
                store_rest(strand, global, object, view, slot);
            }
            UnpackItem::PosRest { slot } => {
                let view = CapturesView {
                    pos_from: position,
                    taken: None,
                };
                store_rest(strand, global, object, view, slot);
            }
            UnpackItem::KeyRest { slot } => {
                let view = CapturesView {
                    pos_from: None,
                    taken: taken.clone(),
                };
                store_rest(strand, global, object, view, slot);
            }
        }
    }

    // A hole is a missing item, so only a participating group is left over.
    if pos_rest == Rest::None
        && let Some(position) = position
        && let Some(extra) = (position..groups.positional.len())
            .find(|&n| matches!(groups.pos(captures, n), Lookup::Present(_)))
    {
        return Err(Error::unexpected_positional(
            strand,
            index + extra - position,
        ));
    }
    if key_rest == Rest::None
        && let Some(taken) = &taken
        && let Some(extra) = (0..taken.len())
            .find(|&o| !taken[o] && matches!(groups.named(captures, o), Lookup::Present(_)))
    {
        return with_slots!(strand, |mut key| {
            strand.intern_sym(&groups.named[extra].0, &mut key);
            Err(Error::unexpected_key(strand, &key))
        });
    }
    Ok(())
}

/// Takes the participating named group with `ordinal` if it is visible and not
/// already taken.
fn take<'h>(
    groups: &Groups,
    captures: &rx::Captures<'h>,
    taken: &mut Option<Box<[bool]>>,
    ordinal: Option<usize>,
) -> Option<rx::Match<'h>> {
    let ordinal = ordinal?;
    let taken = taken.as_mut()?;
    if taken[ordinal] {
        return None;
    }
    let Lookup::Present(matched) = groups.named(captures, ordinal) else {
        return None;
    };
    taken[ordinal] = true;
    Some(matched)
}

/// Stores a `CapturesRest` over `object` with `view` in `out`.
fn store_rest<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    object: impl Input<'v>,
    view: CapturesView,
    mut out: Slot<'v, '_>,
) {
    let ty = global.types.captures_rest;
    ty.create_with_annex(
        strand,
        CapturesRest,
        CapturesRestAnnex { global, view },
        &mut out,
    );
    with_instance!(strand, ty.cast(&out).unwrap(), |inst| {
        let mut borrow = inst.borrow_mut_unwrap();
        strand.store(Mut::slot_mut::<0>(&mut borrow), object);
    });
}

pub(crate) struct Captures<'v> {
    // SAFETY: `caps` is lifetime-transmuted to `'static` but actually borrows from
    // the haystack, which `_haystack` keeps alive and in place; it is declared after
    // `caps`, so it drops after. Slot 0 holds the haystack value.
    caps: rx::Captures<'static>,
    _haystack: PinStr<'v>,
}

pub(crate) struct CapturesAnnex<'v> {
    global: State<'v, Global<'v>>,
    groups: Rc<Groups>,
}

impl<'v> Object<'v> for Captures<'v> {
    const NAME: &'v str = "Captures";
    const MODULE: &'v str = "regex";
    const SLOTS: usize = 1;
    type Annex = CapturesAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        fmt!(strand, w, "{}", &borrow.caps[0])
    }

    fn index<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        let borrow = this.borrow(strand)?;
        match annex.groups.index(strand, &borrow.caps, index)? {
            Lookup::Present(matched) => {
                store_match(strand, annex.global, Ref::slot::<0>(&borrow), matched, out);
                Ok(())
            }
            Lookup::Absent => Err(missing_capture(strand, index)),
            Lookup::NoSuchGroup => Err(Error::index(strand)),
        }
    }

    async fn unpack<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        let borrow = this.borrow(strand)?;
        let view = CapturesView::full(&annex.groups);
        let source = UnpackSource {
            global: annex.global,
            groups: &annex.groups,
            captures: &borrow.caps,
            haystack: Ref::slot::<0>(&borrow),
            view: &view,
        };
        unpack_captures(strand, source, this, unpack)
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .method("get", async move |this, strand, args, out| {
                unpack!(strand, args => key);
                let annex = this.annex();
                let borrow = this.borrow(strand)?;
                match annex.groups.index(strand, &borrow.caps, &key)? {
                    Lookup::Present(matched) => {
                        store_match(strand, annex.global, Ref::slot::<0>(&borrow), matched, out)
                    }
                    Lookup::Absent => strand.store(out, Nil),
                    Lookup::NoSuchGroup => return Err(Error::index(strand)),
                }
                Ok(())
            })
            .get("start", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                let input = TryInto::<i64>::try_into(borrow.caps.get_match().start())
                    .map_err(|_| Error::overflow(strand))?;
                strand.store(out, input);
                Ok(())
            })
            .get("end", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                let input = TryInto::<i64>::try_into(borrow.caps.get_match().end())
                    .map_err(|_| Error::overflow(strand))?;
                strand.store(out, input);
                Ok(())
            })
    }
}

/// The rest of an unpacked `Captures`, which unpacks the items left over.
///
/// Slot 0 holds the `Captures`.
pub(crate) struct CapturesRest;

pub(crate) struct CapturesRestAnnex<'v> {
    global: State<'v, Global<'v>>,
    view: CapturesView,
}

impl<'v> Object<'v> for CapturesRest {
    const NAME: &'v str = "CapturesRest";
    const MODULE: &'v str = "regex";
    const SLOTS: usize = 1;
    type Annex = CapturesRestAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    async fn unpack<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        let borrow = this.borrow(strand)?;
        let captures = annex
            .global
            .types
            .captures
            .cast(Ref::slot::<0>(&borrow))
            .unwrap();
        with_instance!(strand, captures, |inst| {
            let captures_annex = inst.annex();
            let captures_borrow = inst.borrow(strand)?;
            let source = UnpackSource {
                global: annex.global,
                groups: &captures_annex.groups,
                captures: &captures_borrow.caps,
                haystack: Ref::slot::<0>(&captures_borrow),
                view: &annex.view,
            };
            unpack_captures(strand, source, inst, unpack)
        })
    }
}

pub(crate) struct Match<'v> {
    // SAFETY: `match_` is lifetime-transmuted to `'static` but actually borrows
    // from the haystack, which `_haystack` keeps alive and in place; it is declared
    // after `match_`, so it drops after.
    match_: rx::Match<'static>,
    _haystack: PinStr<'v>,
}

pub(crate) struct MatchAnnex;

impl<'v> Object<'v> for Match<'v> {
    const NAME: &'v str = "Match";
    const MODULE: &'v str = "regex";
    const SLOTS: usize = 1;
    type Annex = MatchAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        fmt!(strand, w, "{}", borrow.match_.as_str())
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("start", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                let input = TryInto::<i64>::try_into(borrow.match_.start())
                    .map_err(|_| Error::overflow(strand))?;
                strand.store(out, input);
                Ok(())
            })
            .get("end", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                let input = TryInto::<i64>::try_into(borrow.match_.end())
                    .map_err(|_| Error::overflow(strand))?;
                strand.store(out, input);
                Ok(())
            })
    }
}

pub(crate) struct Find<'v> {
    // SAFETY: The iterator has 'static lifetime but actually borrows from the regex
    // (stored in slot 0) and the haystack (stored in slot 1). The haystack is
    // also pinned by `haystack`. Runtime finalization drops both before the slots are cleared.
    iter: rx::CaptureMatches<'static, 'static>,
    _haystack: PinStr<'v>,
}

pub(crate) struct FindAnnex<'v> {
    global: State<'v, Global<'v>>,
    groups: Rc<Groups>,
}

impl<'v> Object<'v> for Find<'v> {
    const NAME: &'v str = "Find";
    const MODULE: &'v str = "regex";
    // Slot 0: Regex instance
    // Slot 1: Haystack string
    const SLOTS: usize = 2;
    type Annex = FindAnnex<'v>;
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
        strand.store(out, this);
        Ok(())
    }

    async fn next<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let annex = this.annex();

        // Get the iterator and advance it
        let mut borrow = this.borrow_mut(strand)?;
        match borrow.iter.next() {
            Some(caps) => {
                let haystack = Mut::slot::<1>(&borrow).as_str(strand.vm()).unwrap().pin();
                annex.global.types.captures.create_with_annex(
                    strand,
                    Captures {
                        caps,
                        _haystack: haystack,
                    },
                    CapturesAnnex {
                        global: annex.global,
                        groups: annex.groups.clone(),
                    },
                    &mut out,
                );

                // Copy the haystack from Find slot 1 to Captures slot 0
                with_instance!(
                    strand,
                    annex.global.types.captures.cast(&out).unwrap(),
                    |inst| {
                        let mut captures_mut = inst.borrow_mut_unwrap();
                        strand.store(
                            Mut::slot_mut::<0>(&mut captures_mut),
                            Mut::slot::<1>(&borrow),
                        );
                    }
                );
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

enum RegexSplitInner {
    // SAFETY: The iterators have 'static lifetime but actually borrow from the regex
    // (stored in slot 0) and the haystack (stored in slot 1). This is safe as long
    // as both are kept alive and the RegexSplit object is dropped before the slots
    // are cleared.
    Lazy(rx::Split<'static, 'static>),
    LazyN(rx::SplitN<'static, 'static>),
    Buffered { segments: Vec<String>, index: usize },
}

impl RegexSplitInner {
    fn next_str(&mut self) -> Option<&str> {
        match self {
            RegexSplitInner::Lazy(iter) => iter.next(),
            RegexSplitInner::LazyN(iter) => iter.next(),
            RegexSplitInner::Buffered { segments, index } => {
                if *index >= segments.len() {
                    return None;
                }
                let s = &segments[*index];
                *index += 1;
                Some(s)
            }
        }
    }

    /// Collect remaining lazy iterator items into buffered form.
    fn materialize(&mut self) {
        let items: Vec<String> = match self {
            RegexSplitInner::Lazy(iter) => iter.map(String::from).collect(),
            RegexSplitInner::LazyN(iter) => iter.map(String::from).collect(),
            RegexSplitInner::Buffered { .. } => return,
        };
        *self = RegexSplitInner::Buffered {
            segments: items,
            index: 0,
        };
    }

    /// Returns the remaining items. Only valid for `Buffered`.
    fn remaining(&self) -> &[String] {
        match self {
            RegexSplitInner::Buffered { segments, index } => &segments[*index..],
            _ => panic!("remaining() called on non-buffered iterator"),
        }
    }
}

pub(crate) struct RegexSplit<'v> {
    inner: RegexSplitInner,
    _haystack: PinStr<'v>,
}

pub(crate) struct RegexSplitAnnex;

impl<'v> Object<'v> for RegexSplit<'v> {
    const NAME: &'v str = "RegexSplit";
    const MODULE: &'v str = "regex";
    // Slot 0: Regex instance
    // Slot 1: Haystack string
    const SLOTS: usize = 2;
    type Annex = RegexSplitAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .supertype(TypeObject::Iter)
            // Already unpackable, so buffering would only add a wrapper.
            .method("buffer", async |this, strand, args, out| {
                unpack!(strand, args =>);
                strand.store(out, this);
                Ok(())
            })
    }

    async fn iter<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        strand.store(out, this);
        Ok(())
    }

    async fn next<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let mut borrow = this.borrow_mut(strand)?;
        match borrow.inner.next_str() {
            Some(segment) => {
                strand.store(out, segment);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn unpack<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        // Fail fast: split doesn't yield keys
        if let Some(key) = unpack.first_required_key() {
            return Err(Error::missing_key(strand, key));
        }

        // Materializing changes only the representation, not the remaining
        // items, so unpacking still leaves the iterator observably unchanged.
        let mut borrow = this.borrow_mut(strand)?;
        borrow.inner.materialize();
        let remaining = borrow.inner.remaining();
        if remaining.len() < unpack.required() {
            return Err(Error::missing_positional(strand, unpack.required()));
        }
        if unpack.pos_rest() == Rest::None
            && remaining.len() > unpack.required() + unpack.optional()
        {
            return Err(Error::unexpected_positional(
                strand,
                unpack.required() + unpack.optional(),
            ));
        }

        // All validation passed — proceed without possibility of failure
        let mut pos = 0;
        for item in unpack.iter() {
            match item {
                UnpackItem::Pos { slot, default } => match remaining.get(pos) {
                    Some(segment) => {
                        strand.store(slot, segment.as_str());
                        pos += 1;
                    }
                    None => strand.store(slot, default.unwrap()),
                },
                UnpackItem::SymKey { slot, default, .. }
                | UnpackItem::ConstKey { slot, default, .. } => {
                    strand.store(slot, default.unwrap());
                }
                UnpackItem::Rest { mut slot } | UnpackItem::PosRest { mut slot } => {
                    // The rest is a fresh iterator over the remainder
                    let segments = remaining[pos..].to_vec();
                    let haystack = Mut::slot::<1>(&borrow).as_str(strand.vm()).unwrap().pin();
                    let ty = strand.state::<Tag>().types.split;
                    ty.create_with_annex(
                        strand,
                        RegexSplit {
                            inner: RegexSplitInner::Buffered { segments, index: 0 },
                            _haystack: haystack,
                        },
                        RegexSplitAnnex,
                        &mut slot,
                    );
                    with_instance!(strand, ty.cast(&slot).unwrap(), |inst| {
                        let mut rest = inst.borrow_mut_unwrap();
                        strand.store(Mut::slot_mut::<0>(&mut rest), Mut::slot::<0>(&borrow));
                        strand.store(Mut::slot_mut::<1>(&mut rest), Mut::slot::<1>(&borrow));
                    });
                }
                UnpackItem::KeyRest { slot } => Unpack::empty_key_rest(strand, slot),
            }
        }
        Ok(())
    }
}
