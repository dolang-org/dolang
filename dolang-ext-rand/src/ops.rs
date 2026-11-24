//! Operations shared by the module functions and `Rng` methods.

use std::mem::MaybeUninit;

use dolang::runtime::{
    AllocExt, Args, Error, Instance, Result, Slot, Strand, unpack,
    value::{BinEmbryo, Value},
    with_slots,
};
use rand::{Rng, RngExt, seq::index};

use crate::{global::Global, rng::RngObj};

const DEFAULT_ALPHABET: &str = "_-0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";

/// Where an operation draws its randomness from.
#[derive(Clone, Copy)]
pub(crate) enum Source<'v, 'a> {
    /// The thread-local generator.
    Thread,
    /// A `rand.Rng` instance.
    Rng(Instance<'v, 'a, RngObj>),
}

impl<'v> Source<'v, '_> {
    /// Run `f` with the generator.
    ///
    /// An `Rng` stays borrowed only while `f` runs, so `f` must not call into
    /// the VM.
    fn with<'s, T>(
        self,
        strand: &mut Strand<'v, 's>,
        f: impl FnOnce(&mut dyn Rng) -> T,
    ) -> Result<'v, 's, T> {
        match self {
            Source::Thread => Ok(f(&mut rand::rng())),
            Source::Rng(this) => {
                let mut rng = this.borrow_mut(strand)?;
                Ok(f(rng.rng()))
            }
        }
    }
}

fn require_int<'v, 's>(strand: &mut Strand<'v, 's>, value: &Value<'v>) -> Result<'v, 's, i128> {
    value
        .as_int(strand)
        .ok_or_else(|| Error::type_error(strand, "expected `Int`"))
}

fn require_len<'v, 's>(strand: &mut Strand<'v, 's>, value: &Value<'v>) -> Result<'v, 's, usize> {
    let len = require_int(strand, value)?;
    usize::try_from(len).map_err(|_| Error::value(strand, "expected len to be non-negative"))
}

fn require_num<'v, 's>(strand: &mut Strand<'v, 's>, value: &Value<'v>) -> Result<'v, 's, f64> {
    if let Some(v) = value.as_f64(strand) {
        return Ok(v);
    }
    match value.as_int(strand) {
        Some(v) => Ok(v as f64),
        None => Err(Error::type_error(strand, "expected `Float` or `Int`")),
    }
}

/// Read the start, end and step of a `Range`.
fn range_parts<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    [mut start, mut end, mut step]: [Slot<'v, '_>; 3],
) -> Result<'v, 's, ()> {
    let range = value
        .as_range(strand)
        .ok_or_else(|| Error::type_error(strand, "expected `Range`"))?;
    range.parts(
        strand,
        [
            Slot::reborrow(&mut start),
            Slot::reborrow(&mut end),
            Slot::reborrow(&mut step),
        ],
    );
    Ok(())
}

fn empty_range<'v, 's>(strand: &mut Strand<'v, 's>) -> Error<'v, 's> {
    Error::value(strand, "expected a non-empty range")
}

fn sample_int<'v, 's>(
    strand: &mut Strand<'v, 's>,
    src: Source<'v, '_>,
    range: &Value<'v>,
) -> Result<'v, 's, i128> {
    let (start, end, step) = with_slots!(strand, |mut start, mut end, mut step| {
        range_parts(
            strand,
            range,
            [
                Slot::reborrow(&mut start),
                Slot::reborrow(&mut end),
                Slot::reborrow(&mut step),
            ],
        )?;
        let start = match start.is_nil() {
            true => i128::MIN,
            false => require_int(strand, &start)?,
        };
        let end = match end.is_nil() {
            true => None,
            false => Some(require_int(strand, &end)?),
        };
        let step = match step.is_nil() {
            true => 1,
            false => require_int(strand, &step)?,
        };
        (start, end, step)
    });
    if step == 0 {
        return Err(Error::value(strand, "expected a non-zero range step"));
    }
    if step == 1 {
        // The unstepped cases include `..`, whose 2^128 values don't fit a
        // `u128` count.
        return match end {
            Some(end) if end <= start => Err(empty_range(strand)),
            Some(end) => src.with(strand, |rng| rng.random_range(start..end)),
            None => src.with(strand, |rng| rng.random_range(start..=i128::MAX)),
        };
    }
    // Count the values `start`, `start + step`, ... before `end`, or up to and
    // including `i128::MAX` when the range has no end.
    let magnitude = step.unsigned_abs();
    let distance = match end {
        Some(end) if (step > 0 && end > start) || (step < 0 && end < start) => {
            Some(start.abs_diff(end) - 1)
        }
        Some(_) => None,
        None if step > 0 => Some(start.abs_diff(i128::MAX)),
        None => (start == i128::MAX).then_some(0),
    };
    let Some(distance) = distance else {
        return Err(empty_range(strand));
    };
    let count = distance / magnitude + 1;
    let offset = src.with(strand, |rng| rng.random_range(0..count))? * magnitude;
    let value = match step > 0 {
        true => start.checked_add_unsigned(offset),
        false => start.checked_sub_unsigned(offset),
    };
    Ok(value.expect("stepped sample stays within the range"))
}

fn sample_float<'v, 's>(
    strand: &mut Strand<'v, 's>,
    src: Source<'v, '_>,
    range: Option<&Value<'v>>,
) -> Result<'v, 's, f64> {
    let Some(range) = range else {
        return src.with(strand, |rng| rng.random::<f64>());
    };
    let (start, end) = with_slots!(strand, |mut start, mut end, mut step| {
        range_parts(
            strand,
            range,
            [
                Slot::reborrow(&mut start),
                Slot::reborrow(&mut end),
                Slot::reborrow(&mut step),
            ],
        )?;
        if !step.is_nil() {
            return Err(Error::value(strand, "expected a range without a step"));
        }
        if start.is_nil() || end.is_nil() {
            return Err(Error::value(strand, "expected a bounded range"));
        }
        (require_num(strand, &start)?, require_num(strand, &end)?)
    });
    if !start.is_finite() || !end.is_finite() {
        return Err(Error::value(strand, "expected finite range bounds"));
    }
    if end <= start {
        return Err(empty_range(strand));
    }
    src.with(strand, |rng| rng.random_range(start..end))
}

/// Get the length of a sequence through its `len` field.
fn seq_len<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: &Global<'v>,
    value: &Value<'v>,
) -> Result<'v, 's, usize> {
    with_slots!(strand, |mut len| {
        value.get(strand, global.syms.len, &mut len)?;
        len.to_usize(strand)
    })
}

/// `int range`
pub(crate) fn int<'v, 's>(
    strand: &mut Strand<'v, 's>,
    src: Source<'v, '_>,
    args: Args<'v, '_>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    unpack!(strand, args => range);
    let value = sample_int(strand, src, &range)?;
    strand.store(out, value);
    Ok(())
}

/// `float [range]`
pub(crate) fn float<'v, 's>(
    strand: &mut Strand<'v, 's>,
    src: Source<'v, '_>,
    args: Args<'v, '_>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    unpack!(strand, args => ?range);
    let value = sample_float(strand, src, range.as_deref())?;
    strand.store(out, value);
    Ok(())
}

/// `bool [p]`
pub(crate) fn bool<'v, 's>(
    strand: &mut Strand<'v, 's>,
    src: Source<'v, '_>,
    args: Args<'v, '_>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    unpack!(strand, args => ?p);
    let p = match p {
        Some(p) => require_num(strand, &p)?,
        None => 0.5,
    };
    if !(0.0..=1.0).contains(&p) {
        return Err(Error::value(strand, "expected p to be between 0 and 1"));
    }
    let value = src.with(strand, |rng| rng.random_bool(p))?;
    strand.store(out, value);
    Ok(())
}

/// `bin len`
pub(crate) fn bin<'v, 's>(
    strand: &mut Strand<'v, 's>,
    src: Source<'v, '_>,
    args: Args<'v, '_>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    unpack!(strand, args => len);
    let len = require_len(strand, &len)?;
    if len > isize::MAX as usize {
        return Err(Error::overflow(strand));
    }
    let mut acc = BinEmbryo::new_with_capacity(strand, len);
    let spare = &mut acc.spare_capacity_mut()[..len];
    spare.fill(MaybeUninit::new(0));
    // SAFETY: every byte of `spare` was just initialized
    let bytes = unsafe { &mut *(spare as *mut [MaybeUninit<u8>] as *mut [u8]) };
    src.with(strand, |rng| rng.fill_bytes(bytes))?;
    // SAFETY: the first `len` bytes of spare capacity were written above
    unsafe { acc.advance(len) };
    acc.finish(strand, out);
    Ok(())
}

/// `str len :alphabet`
pub(crate) fn str<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: &Global<'v>,
    src: Source<'v, '_>,
    args: Args<'v, '_>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let alphabet = global.syms.alphabet;
    unpack!(strand, args => len, ?:alphabet);
    let len = require_len(strand, &len)?;
    let chars = match &alphabet {
        Some(alphabet) => {
            let alphabet = alphabet
                .as_str(strand)
                .ok_or_else(|| Error::type_error(strand, "expected `Str`"))?;
            strand.access(|x| alphabet.as_str(x).chars().collect::<Vec<_>>())
        }
        None => DEFAULT_ALPHABET.chars().collect(),
    };
    if chars.is_empty() {
        return Err(Error::value(strand, "expected alphabet to be non-empty"));
    }
    let text = src.with(strand, |rng| {
        (0..len)
            .map(|_| chars[rng.random_range(0..chars.len())])
            .collect::<String>()
    })?;
    strand.store(out, text.as_str());
    Ok(())
}

/// `pick collection`
pub(crate) fn pick<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: &Global<'v>,
    src: Source<'v, '_>,
    args: Args<'v, '_>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    unpack!(strand, args => collection);
    let len = seq_len(strand, global, &collection)?;
    if len == 0 {
        return Err(Error::value(strand, "expected a non-empty sequence"));
    }
    let index = src.with(strand, |rng| rng.random_range(0..len))?;
    collection.index(strand, index, out)
}

/// `sample collection k`
pub(crate) fn sample<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: &Global<'v>,
    src: Source<'v, '_>,
    args: Args<'v, '_>,
    mut out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    unpack!(strand, args => collection, k);
    let k = require_int(strand, &k)?;
    let k =
        usize::try_from(k).map_err(|_| Error::value(strand, "expected k to be non-negative"))?;
    let len = seq_len(strand, global, &collection)?;
    if k > len {
        return Err(Error::value(
            strand,
            "expected k to be at most the sequence length",
        ));
    }
    let indices = src.with(strand, |rng| index::sample(rng, len, k))?;
    strand.new_array(&mut out);
    with_slots!(strand, |mut item| {
        for index in indices {
            collection.index(strand, index, &mut item)?;
            let array = out.as_array(strand).expect("new array");
            array.push(strand, &item)?;
        }
    });
    Ok(())
}

/// `shuffle array`
pub(crate) fn shuffle<'v, 's>(
    strand: &mut Strand<'v, 's>,
    src: Source<'v, '_>,
    args: Args<'v, '_>,
) -> Result<'v, 's, ()> {
    unpack!(strand, args => value);
    let array = value
        .as_array(strand)
        .ok_or_else(|| Error::type_error(strand, "expected `Array`"))?;
    let len = array.len(strand)?;
    with_slots!(strand, |mut left, mut right| {
        for i in (1..len).rev() {
            let j = src.with(strand, |rng| rng.random_range(0..=i))?;
            if i == j {
                continue;
            }
            array.get(strand, i, &mut left)?;
            array.get(strand, j, &mut right)?;
            array.set(strand, i, &right)?;
            array.set(strand, j, &left)?;
        }
    });
    Ok(())
}
