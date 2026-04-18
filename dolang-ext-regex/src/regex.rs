use std::{borrow::Cow, fmt, mem};

use dolang::runtime::{
    Args, Error, Instance, Object, Output, Result, Slot, State, Strand, Type, Value, call,
    error::ResultExt,
    object::{Mut, Ref, TypeBuilder, Unpack, UnpackItem},
    unpack,
    value::{Nil, PinStr, TypeObject},
    vm::Builder,
};
use regex as rx;

use crate::global::Global;

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    builder
        .module("regex")
        .value("Regex", global.types.regex)
        .commit();
}

pub(crate) struct Regex;

pub(crate) struct RegexAnnex<'v> {
    global: State<'v, Global<'v>>,
    regex: rx::Regex,
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
        let ([pattern], []) = unpack!(strand, args, 1, 0)?;
        let pattern = pattern
            .as_str(strand.vm())
            .ok_or_else(|| Error::type_error(strand, "pattern: expected str"))?
            .pin();
        let regex = rx::Regex::new(&pattern).into_do(strand)?;
        let global = strand.state::<Global<'v>>();
        this.create_with_annex(strand, Regex, RegexAnnex { global, regex }, out);
        Ok(())
    }

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let limit_sym = builder.sym("limit");
        builder
            .method("match", async move |this, strand, args, mut out| {
                let annex = this.annex();
                let ([haystack_value], []) = unpack!(strand, args, 1, 0)?;
                let haystack = haystack_value
                    .as_str(strand.vm())
                    .ok_or_else(|| Error::type_error(strand, "expected `str`"))?.pin();
                match annex.regex.captures(&haystack) {
                    Some(caps) => {
                        // SAFETY: We transmute the captures to have 'static lifetime.
                        // The haystack GC object is stored in slot 0 to keep it alive, and
                        // the transmuted pin guard is owned by the Captures object.
                        annex.global.types.captures.create_with_annex(
                            strand,
                            Captures,
                            CapturesAnnex {
                                caps: unsafe {
                                    mem::transmute::<rx::Captures<'_>, rx::Captures<'static>>(caps)
                                },
                                _haystack: unsafe {
                                    mem::transmute::<PinStr<'v, '_>, PinStr<'v, 'static>>(
                                        haystack,
                                    )
                                },
                                global: annex.global,
                            },
                            &mut out,
                        );

                        // Store haystack GC object in slot 0 of the Captures object
                        let mut captures = annex
                            .global
                            .types
                            .captures
                            .downcast(&out)
                            .unwrap()
                            .borrow_mut_unwrap();
                        Output::set(strand, Mut::slot_mut::<0>(&mut captures), haystack_value);
                    }
                    None => {
                        Output::set(strand, out, Nil);
                    }
                }
                Ok(())
            })
            .method("find", async move |this, strand, args, mut out| {
                let annex = this.annex();
                let ([haystack_value], []) = unpack!(strand, args, 1, 0)?;
                let haystack = haystack_value
                    .as_str(strand.vm())
                    .ok_or_else(|| Error::type_error(strand, "expected `str`"))?.pin();
                // Create the captures iterator
                let iter = annex.regex.captures_iter(&haystack);

                // SAFETY: We transmute the iterator to have 'static lifetime.
                // The regex is stored in slot 0, haystack in slot 1, and the pin guard
                // is owned by the Find object.
                let iter = unsafe {
                    mem::transmute::<rx::CaptureMatches<'_, '_>, rx::CaptureMatches<'static, 'static>>(
                        iter,
                    )
                };

                annex.global.types.find.create_with_annex(
                    strand,
                    Find {
                        iter,
                        _haystack: unsafe {
                            mem::transmute::<PinStr<'v, '_>, PinStr<'v, 'static>>(haystack)
                        },
                    },
                    FindAnnex {
                        global: annex.global,
                    },
                    &mut out,
                );

                // Store regex instance in slot 0 and haystack in slot 1 of the Find object
                let mut iter = annex
                    .global
                    .types
                    .find
                    .downcast(&out)
                    .unwrap()
                    .borrow_mut_unwrap();
                Output::set(strand, Mut::slot_mut::<0>(&mut iter), this);
                Output::set(strand, Mut::slot_mut::<1>(&mut iter), haystack_value);
                Ok(())
            })
            .method_with_slots(
                "replace",
                async move |this, strand, args, out, [mut caps_slot, mut cb_out]| {
                    let annex = this.annex();

                    let ([haystack_value, replacement], [limit]) =
                        unpack!(strand, args, 2, 0, limit_sym = None)?;
                    let haystack = haystack_value
                        .as_str(strand.vm())
                        .ok_or_else(|| Error::type_error(strand, "expected `str`"))?;
                    let haystack = haystack.pin();
                    let limit_val = limit
                        .map(|l| {
                            l.as_i64(strand)
                                .ok_or_else(|| Error::type_error(strand, "limit: expected `int`"))
                        })
                        .transpose()?;

                    if let Some(rep) = replacement.as_str(strand) {
                        // String replacement: delegate to regex crate
                        let result = match limit_val {
                            None => strand.access(|x| annex.regex.replace_all(&haystack, rep.as_str(x))),
                            Some(0) => Cow::Borrowed(&*haystack),
                            Some(n) if n > 0 => strand.access(|x| annex.regex.replacen(&haystack, n as usize, rep.as_str(x))),
                            Some(_) => {
                                return Err(Error::value(strand, "limit must be >= 0"))
                            }
                        };
                        Output::set(strand, out, result.as_ref());
                    } else {
                        // Callback replacement: iterate matches manually
                        let max = match limit_val {
                            None => usize::MAX,
                            Some(0) => 0,
                            Some(n) if n > 0 => n as usize,
                            Some(_) => {
                                return Err(Error::value(strand, "limit must be >= 0"))
                            }
                        };
                        let mut result = String::new();
                        let mut last_end = 0;
                        for (count, caps) in annex.regex.captures_iter(&haystack).enumerate() {
                            if count >= max {
                                break;
                            }
                            let m = caps.get(0).unwrap();
                            result.push_str(&haystack[last_end..m.start()]);
                            let callback_haystack = unsafe {
                                mem::transmute::<PinStr<'v, '_>, PinStr<'v, 'static>>(
                                    haystack.clone()
                                )
                            };

                            // Create Captures object for the callback
                            annex.global.types.captures.create_with_annex(
                                strand,
                                Captures,
                                CapturesAnnex {
                                    caps: unsafe {
                                        mem::transmute::<rx::Captures<'_>, rx::Captures<'static>>(
                                            caps,
                                        )
                                    },
                                    _haystack: callback_haystack,
                                    global: annex.global,
                                },
                                &mut caps_slot,
                            );
                            // Store haystack in Captures slot 0
                            let mut captures_mut = annex
                                .global
                                .types
                                .captures
                                .downcast(&caps_slot)
                                .unwrap()
                                .borrow_mut_unwrap();
                            Output::set(
                                strand,
                                Mut::slot_mut::<0>(&mut captures_mut),
                                &haystack_value,
                            );
                            drop(captures_mut);

                            // Call the replacement function with the Captures
                            call!(strand, &replacement, &mut cb_out, &caps_slot).await?;

                            let rep = cb_out
                                .as_str(strand)
                                .ok_or_else(|| {
                                    Error::type_error(
                                        strand,
                                        "replacement callback must return `str`",
                                    )
                                })?;
                            strand.access(|x| result.push_str(rep.as_str(x)));

                            last_end = m.end();
                        }
                        result.push_str(&haystack[last_end..]);
                        Output::set(strand, out, result.as_str());
                    }
                    Ok(())
                },
            )
            .method("split", async move |this, strand, args, mut out| {
                let annex = this.annex();
                let ([haystack_value], [limit]) = unpack!(strand, args, 1, 0, limit_sym = None)?;
                let haystack = haystack_value
                    .as_str(strand.vm())
                    .ok_or_else(|| Error::type_error(strand, "expected `str`"))?;
                let haystack = haystack.pin();
                let limit_i64 = limit
                    .map(|l| {
                        l.as_i64(strand)
                            .ok_or_else(|| Error::type_error(strand, "limit: expected `int`"))
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
                        _haystack: unsafe {
                            mem::transmute::<PinStr<'v, '_>, PinStr<'v, 'static>>(haystack)
                        },
                    },
                    RegexSplitAnnex,
                    &mut out,
                );

                // Store regex in slot 0, haystack in slot 1
                let mut borrow = annex
                    .global
                    .types
                    .split
                    .downcast(&out)
                    .unwrap()
                    .borrow_mut_unwrap();
                Output::set(strand, Mut::slot_mut::<0>(&mut borrow), this);
                Output::set(strand, Mut::slot_mut::<1>(&mut borrow), haystack_value);
                Ok(())
            })
            .method("rsplit", async move |this, strand, args, mut out| {
                let annex = this.annex();
                let ([haystack_value], [limit]) = unpack!(strand, args, 1, 0, limit_sym = None)?;
                let haystack = haystack_value
                    .as_str(strand.vm())
                    .ok_or_else(|| Error::type_error(strand, "expected `str`"))?;
                let haystack = haystack.pin();
                let limit_i64 = limit
                    .map(|l| {
                        l.as_i64(strand)
                            .ok_or_else(|| Error::type_error(strand, "limit: expected `int`"))
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
                        _haystack: unsafe {
                            mem::transmute::<PinStr<'v, '_>, PinStr<'v, 'static>>(haystack)
                        },
                    },
                    RegexSplitAnnex,
                    &mut out,
                );

                // Store regex in slot 0, haystack in slot 1
                let mut borrow = annex
                    .global
                    .types
                    .split
                    .downcast(&out)
                    .unwrap()
                    .borrow_mut_unwrap();
                Output::set(strand, Mut::slot_mut::<0>(&mut borrow), this);
                Output::set(strand, Mut::slot_mut::<1>(&mut borrow), haystack_value);
                Ok(())
            })
    }
}

pub(crate) struct Captures;

pub(crate) struct CapturesAnnex<'v> {
    // SAFETY: The captures has 'static lifetime but actually borrows from the haystack
    // stored in slot 0 and pinned by `haystack`.
    caps: rx::Captures<'static>,
    _haystack: PinStr<'v, 'static>,
    global: State<'v, Global<'v>>,
}

impl<'v> Object<'v> for Captures {
    const NAME: &'v str = "Captures";
    const MODULE: &'v str = "regex";
    const SLOTS: usize = 1;
    type Annex = CapturesAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "{}", &this.annex().caps[0]).into_do(strand)
    }

    fn index<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();

        let cap = if let Some(idx) = index.as_i64(strand) {
            annex
                .caps
                .get(idx.try_into().map_err(|_| Error::overflow(strand))?)
        } else if let Some(name) = index.as_str(strand) {
            strand.access(|x| annex.caps.name(name.as_str(x)))
        } else {
            return Err(Error::type_error(strand, "expected `int` or `str`"));
        };

        match cap {
            Some(m) => {
                // SAFETY: We transmute the match to have 'static lifetime.
                // The haystack is stored in slot 0 to keep it alive.
                let m = unsafe { mem::transmute::<rx::Match<'_>, rx::Match<'static>>(m) };

                let borrow = this.borrow(strand)?;
                let haystack = unsafe {
                    mem::transmute::<PinStr<'v, '_>, PinStr<'v, 'static>>(
                        Ref::slot::<0>(&borrow).as_str(strand.vm()).unwrap().pin(),
                    )
                };

                // Create the Match object directly in out
                annex.global.types.match_.create_with_annex(
                    strand,
                    Match,
                    MatchAnnex {
                        match_: m,
                        _haystack: haystack,
                    },
                    &mut out,
                );

                // Store haystack in slot 0 of the Match object
                let mut match_borrow = annex
                    .global
                    .types
                    .match_
                    .downcast(&out)
                    .unwrap()
                    .borrow_mut_unwrap();
                Output::set(
                    strand,
                    Mut::slot_mut::<0>(&mut match_borrow),
                    Ref::slot::<0>(&borrow),
                );

                Ok(())
            }
            None => Err(Error::index(strand)),
        }
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("start", |this, strand, out| {
                let annex = this.annex();
                let input = TryInto::<i64>::try_into(annex.caps.get_match().start())
                    .map_err(|_| Error::overflow(strand))?;
                Output::set(strand, out, input);
                Ok(())
            })
            .get("end", |this, strand, out| {
                let annex = this.annex();
                let input = TryInto::<i64>::try_into(annex.caps.get_match().end())
                    .map_err(|_| Error::overflow(strand))?;
                Output::set(strand, out, input);
                Ok(())
            })
    }
}

pub(crate) struct Match;

pub(crate) struct MatchAnnex<'v> {
    // SAFETY: The match has 'static lifetime but actually borrows from the haystack
    // stored in slot 0 and pinned by `haystack`.
    match_: rx::Match<'static>,
    _haystack: PinStr<'v, 'static>,
}

impl<'v> Object<'v> for Match {
    const NAME: &'v str = "Match";
    const MODULE: &'v str = "regex";
    const SLOTS: usize = 1;
    type Annex = MatchAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "{}", this.annex().match_.as_str()).into_do(strand)
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("start", |this, strand, out| {
                let annex = this.annex();
                let input = TryInto::<i64>::try_into(annex.match_.start())
                    .map_err(|_| Error::overflow(strand))?;
                Output::set(strand, out, input);
                Ok(())
            })
            .get("end", |this, strand, out| {
                let annex = this.annex();
                let input = TryInto::<i64>::try_into(annex.match_.end())
                    .map_err(|_| Error::overflow(strand))?;
                Output::set(strand, out, input);
                Ok(())
            })
    }
}

pub(crate) struct Find<'v> {
    // SAFETY: The iterator has 'static lifetime but actually borrows from the regex
    // (stored in slot 0) and the haystack (stored in slot 1). The haystack is
    // also pinned by `haystack`.
    iter: rx::CaptureMatches<'static, 'static>,
    _haystack: PinStr<'v, 'static>,
}

pub(crate) struct FindAnnex<'v> {
    global: State<'v, Global<'v>>,
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

    async fn input<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
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
                let haystack = unsafe {
                    mem::transmute::<PinStr<'v, '_>, PinStr<'v, 'static>>(
                        Mut::slot::<1>(&borrow).as_str(strand.vm()).unwrap().pin(),
                    )
                };
                annex.global.types.captures.create_with_annex(
                    strand,
                    Captures,
                    CapturesAnnex {
                        caps,
                        _haystack: haystack,
                        global: annex.global,
                    },
                    &mut out,
                );

                // Copy haystack from Find slot 1 to Captures slot 0
                let mut captures_mut = annex
                    .global
                    .types
                    .captures
                    .downcast(&out)
                    .unwrap()
                    .borrow_mut_unwrap();
                Output::set(
                    strand,
                    Mut::slot_mut::<0>(&mut captures_mut),
                    Mut::slot::<1>(&borrow),
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

    /// Returns the number of remaining items. Only valid for `Buffered`.
    fn remaining(&self) -> usize {
        match self {
            RegexSplitInner::Buffered { segments, index } => segments.len() - index,
            _ => panic!("remaining() called on non-buffered iterator"),
        }
    }
}

pub(crate) struct RegexSplit<'v> {
    inner: RegexSplitInner,
    _haystack: PinStr<'v, 'static>,
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
        builder.supertype(TypeObject::Iter)
    }

    async fn input<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
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
                Output::set(strand, out, segment);
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

        let fallible = unpack.required() > 0 || unpack.exhaustive();
        let mut borrow = this.borrow_mut(strand)?;

        if fallible {
            // Materialize lazy iterators so we can validate before writing slots
            borrow.inner.materialize();

            let remaining = borrow.inner.remaining();
            if remaining < unpack.required() {
                return Err(Error::missing_positional(strand, unpack.required()));
            }
            if unpack.exhaustive() && remaining > unpack.required() + unpack.optional() {
                return Err(Error::unexpected_positional(
                    strand,
                    unpack.required() + unpack.optional(),
                ));
            }
        }

        // All validation passed — proceed without possibility of failure
        for item in unpack.iter() {
            match item {
                UnpackItem::Pos { slot, default } => match borrow.inner.next_str() {
                    Some(segment) => Output::set(strand, slot, segment),
                    None => Output::set(strand, slot, default.unwrap()),
                },
                UnpackItem::SymKey { slot, default, .. }
                | UnpackItem::ConstKey { slot, default, .. } => {
                    Output::set(strand, slot, default.unwrap());
                }
                UnpackItem::Rest { slot } => {
                    Output::set(strand, slot, this);
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}
