use std::{hint::unreachable_unchecked, num::NonZero, ptr::NonNull};

use crate::{
    gc::Base,
    object::protocol::{GcObj, Header, Protocol},
};

use super::prim::Prim;

pub(crate) enum Decode {
    Prim(Prim),
    Object(NonNull<Header>),
}

// Value tags (low-order bits)
pub(crate) mod tag {
    // Width of tag in bits
    pub(crate) const WIDTH: u32 = 2;
    // An object (remaining bits are address of GC header)
    pub(super) const OBJECT: usize = 0b00;
    // An immediate i64
    pub(super) const I64: usize = 0b01;
    // An immediate f64 (not representable on 32-bit systems)
    #[cfg(target_pointer_width = "64")]
    pub(super) const F64: usize = 0b10;
    // Tag mask
    pub(super) const MASK: usize = (1 << WIDTH) - 1;
}

// Special object-tagged values with implausibly low addresses (within first page of virtual
// address space) are used to encode well-known constants.  To keep the representation always
// non-zero (for compact `Option<Value>`), 0 is not used.
mod known {
    use super::tag;
    pub(super) const UNINIT: usize = 0b001 << tag::WIDTH;
    pub(super) const NIL: usize = 0b010 << tag::WIDTH;
    pub(super) const FALSE: usize = 0b011 << tag::WIDTH;
    pub(super) const TRUE: usize = 0b100 << tag::WIDTH;
    #[cfg(target_pointer_width = "64")]
    pub(super) const POS_INF: usize = 0b101 << tag::WIDTH;
    #[cfg(target_pointer_width = "64")]
    pub(super) const NEG_INF: usize = 0b110 << tag::WIDTH;
    #[cfg(target_pointer_width = "64")]
    pub(super) const NAN: usize = 0b111 << tag::WIDTH;
}

// Maximum and minimum representable i64 immediates
pub(crate) const IMAX: i64 = (isize::MAX >> tag::WIDTH) as i64;
pub(crate) const IMIN: i64 = (isize::MIN >> tag::WIDTH) as i64;

// Floating point encoding details.
//
// ## Exponent Shaving
//
// On 64-bit systems, we encode f64 by shaving bits off of the exponent, avoiding heap allocation
// for floating-point values under a certain magnitude.  This differs from "NaN-boxing" in that not
// all floats are representable unboxed; instead, a much larger range of integer values are
// representable without boxing.  This seemed like a good tradeoff for a language focused on systems
// automation and scripting rather than number-crunching.
//
// ## How it works:
//
// IEEE 754 doubles use 64 bits: 1 sign bit, 11 exponent bits, 52 mantissa bits.
// We use the low 2 bits of the pointer for type tagging (tag::F64).
// We "shave" 2 bits off the 11-bit exponent to make room, leaving 9 bits.
// This limits the representable exponent range but preserves full mantissa precision.
// Values outside the representable exponent range are boxed (heap allocated).
//
// ## Why shave exponent, not mantissa?
//
// Shaving the mantissa would either reduce precision, or require values to be boxed as significant
// bits accumulate over many operations. Shaving the exponent limits the dynamic range (min/max
// representable values) but preserves full precision.
//
// ## Special values:
//
// Infinity and NaN have the maximum exponent value (0x7FF) which doesn't fit in 9 bits. These are
// stored as special well-known constants (POS_INF, NEG_INF, NAN) tagged as OBJECT but with
// implausibly low addresses.  Different NaNs (e.g. signalling/non-signalling) are not
// distinguished.
//
// ## Exponent encoding:
//
// The IEEE 754 9-bit exponent uses a biased representation to handle both positive and negative
// exponents. The encoding/decoding functions (exp_encode/exp_decode) remove the bias to obtain a
// signed encoding, and handle exponent 0 (signed zero and subnormals) specially.  The leading
// 2 most-significant bits can then be truncated if the exponent is in representable range. Values
// outside this range are boxed on the heap instead.
#[cfg(target_pointer_width = "64")]
mod floating {
    use std::ops::RangeInclusive;

    // Facts about IEEE 754 doubles
    pub(super) const F64_MANTISSA_BITS: u32 = 52;
    pub(super) const F64_EXPONENT_BITS: u32 = 11;
    pub(super) const F64_EXPONENT_BIAS: i64 = 1023;
    // Special exponent value that indicates infinity/NaN
    pub(super) const F64_EXPONENT_INF_NAN: u64 = (1 << F64_EXPONENT_BITS) - 1;

    // Facts about immediate f64 encoding
    // Exponent bit width
    pub(super) const F64_IMM_EXPONENT_BITS: u32 = F64_EXPONENT_BITS - super::tag::WIDTH;
    // Maximum and minimum representable exponents
    pub(super) const F64_IMM_EXPONENT_MAX: u64 = exp_decode((1 << (F64_IMM_EXPONENT_BITS - 1)) - 1);
    pub(super) const F64_IMM_EXPONENT_MIN: u64 = exp_decode(1 << (F64_IMM_EXPONENT_BITS - 1));
    pub(super) const F64_IMM_EXPONENT_RANGE: RangeInclusive<u64> =
        F64_IMM_EXPONENT_MIN..=F64_IMM_EXPONENT_MAX;

    // Encode IEEE f64 exponent (which must be in representable range or special `0` value)
    // into f64 immediate representation
    pub(super) const fn exp_encode(exponent: u64) -> u64 {
        // Map special value `0` (signed 0 or subnormals) to `0`
        if exponent == 0 {
            return 0;
        }
        // Convert exponent to signed by subtracting bias
        let mut signed = exponent as i64 - F64_EXPONENT_BIAS;
        if signed < 1 {
            // Make room for `0` <-> `0` mapping by shifing exponents 0 or less
            signed -= 1
        }
        (signed as u64) & ((1 << F64_IMM_EXPONENT_BITS) - 1)
    }

    // Decode f64 immediate exponent back to f64 (IEEE) representation
    pub(super) const fn exp_decode(mut exponent: u64) -> u64 {
        // Map `0` back to `0` (signed 0 or subnormals)
        if exponent == 0 {
            return 0;
        }
        // Sign extend truncated portion of exponent
        if exponent & (1 << (F64_IMM_EXPONENT_BITS - 1)) != 0 {
            exponent |= !((1 << F64_IMM_EXPONENT_BITS) - 1)
        }
        // Interpret as signed
        let mut signed = exponent as i64;
        if signed < 0 {
            // Undo shift that made room for `0` <-> `0` mapping
            signed += 1
        }
        // Restore exponent bias
        (signed + F64_EXPONENT_BIAS) as u64
    }
}

#[cfg(target_pointer_width = "64")]
use floating::*;

#[derive(Clone)]
pub(crate) struct Repr(NonNull<u8>);

impl PartialEq for Repr {
    fn eq(&self, other: &Self) -> bool {
        // Preserve NaN behavior
        if self.0 == Self::NAN.0 || other.0 == Self::NAN.0 {
            false
        } else {
            self.0 == other.0
        }
    }
}

impl Repr {
    // Special value which is used when a transient "not a valid value" state
    // is needed.  It's unsafe to attempt to decode or otherwise operate on
    // this value other than testing that it is the uninit state.
    pub(crate) const UNINIT: Repr = Repr(NonNull::without_provenance(
        NonZero::new(known::UNINIT).unwrap(),
    ));
    // The unit type, but not called that for familiarity reasons
    pub(crate) const NIL: Repr = Repr(NonNull::without_provenance(
        NonZero::new(known::NIL).unwrap(),
    ));
    pub(crate) const FALSE: Repr = Repr(NonNull::without_provenance(
        NonZero::new(known::FALSE).unwrap(),
    ));
    pub(crate) const TRUE: Repr = Repr(NonNull::without_provenance(
        NonZero::new(known::TRUE).unwrap(),
    ));
    // Special float values that otherwise would not have an exponent that fits
    // in an immediate
    #[cfg(target_pointer_width = "64")]
    pub(crate) const POS_INF: Repr = Repr(NonNull::without_provenance(
        NonZero::new(known::POS_INF).unwrap(),
    ));
    #[cfg(target_pointer_width = "64")]
    pub(crate) const NEG_INF: Repr = Repr(NonNull::without_provenance(
        NonZero::new(known::NEG_INF).unwrap(),
    ));
    #[cfg(target_pointer_width = "64")]
    pub(crate) const NAN: Repr = Repr(NonNull::without_provenance(
        NonZero::new(known::NAN).unwrap(),
    ));

    /// Decode this representation into a matchable enum.
    ///
    /// This is the central dispatch point for value type interpretation.
    /// The decoding logic must match the encoding logic in `from_i64`,
    /// `from_f64`, and `from_object`.
    ///
    /// # Type Tagging Scheme
    ///
    /// Values are distinguished by the low 2 bits (tag::MASK):
    /// - tag::OBJECT: GC object pointer or well-known constant
    /// - tag::I64: Immediate signed 64-bit integer
    /// - tag::F64: Immediate 64-bit float (64-bit only)
    #[inline]
    pub(crate) fn decode(&self) -> Decode {
        let this = self.0;
        let tag = this.addr().get() & tag::MASK;
        match tag {
            tag::I64 => Decode::Prim(Prim::Int(
                ((this.addr().get() as isize) >> tag::WIDTH) as i64 as i128,
            )),
            #[cfg(target_pointer_width = "64")]
            tag::F64 => {
                // Pull immediate apart, decode exponent, put it back together
                let imm = this.addr().get() as u64 >> tag::WIDTH;
                let mantissa = imm & ((1 << F64_MANTISSA_BITS) - 1);
                let exponent =
                    exp_decode((imm >> F64_MANTISSA_BITS) & ((1 << F64_IMM_EXPONENT_BITS) - 1));
                let sign = (imm >> (F64_MANTISSA_BITS + F64_IMM_EXPONENT_BITS)) & 1;
                let bits = sign << (F64_MANTISSA_BITS + F64_EXPONENT_BITS)
                    | exponent << F64_MANTISSA_BITS
                    | mantissa;
                Decode::Prim(Prim::F64(f64::from_bits(bits)))
            }
            tag::OBJECT => {
                let addr = this.addr().get() & !tag::MASK;
                match addr {
                    // This catches unsafe use of uninit in debug builds
                    known::UNINIT => unsafe { unreachable_unchecked() },
                    known::NIL => Decode::Prim(Prim::Nil),
                    known::FALSE => Decode::Prim(Prim::Bool(false)),
                    known::TRUE => Decode::Prim(Prim::Bool(true)),
                    known::POS_INF => Decode::Prim(Prim::F64(f64::INFINITY)),
                    known::NEG_INF => Decode::Prim(Prim::F64(f64::NEG_INFINITY)),
                    known::NAN => Decode::Prim(Prim::F64(f64::NAN)),
                    _ => Decode::Object(this.cast::<Header>()),
                }
            }
            _ => unsafe { unreachable_unchecked() },
        }
    }

    #[inline]
    pub(crate) fn is_uninit(&self) -> bool {
        let this = self.0;
        let tag = this.addr().get() & tag::MASK;
        let addr = this.addr().get() & !tag::MASK;
        matches!((tag, addr), (tag::OBJECT, known::UNINIT))
    }

    #[inline]
    pub(crate) fn from_object<'v, T: ?Sized + Protocol<'v>>(o: GcObj<'v, T>) -> Self {
        Self(Base::into_raw(o).cast())
    }

    /// Encode i64 if it fits within the immediate representation
    #[inline]
    pub(crate) fn from_i64(value: i64) -> Option<Self> {
        if (IMIN..=IMAX).contains(&value) {
            unsafe {
                Some(Self(NonNull::without_provenance(NonZero::new_unchecked(
                    (value << tag::WIDTH) as usize | tag::I64,
                ))))
            }
        } else {
            None
        }
    }

    /// Encode f64 if it fits within the immediate representation
    #[inline]
    #[cfg(target_pointer_width = "64")]
    pub(crate) fn from_f64(value: f64) -> Option<Self> {
        // Pull the f64 apart into components
        let bits = value.to_bits();
        let exponent = (bits >> F64_MANTISSA_BITS) & ((1 << F64_EXPONENT_BITS) - 1);
        let mantissa = bits & ((1 << F64_MANTISSA_BITS) - 1);
        let sign = (bits >> (F64_MANTISSA_BITS + F64_EXPONENT_BITS)) & 1;
        // Handle special values
        if exponent == F64_EXPONENT_INF_NAN {
            if mantissa == 0 {
                if sign != 0 {
                    Some(Self::NEG_INF)
                } else {
                    Some(Self::POS_INF)
                }
            } else {
                Some(Self::NAN)
            }
        } else if exponent == 0 || F64_IMM_EXPONENT_RANGE.contains(&exponent) {
            // Exponent is 0 (signed 0 or subnormals) or within representable range, encode it
            // and pack components together
            let imm = sign << (F64_MANTISSA_BITS + F64_IMM_EXPONENT_BITS)
                | exp_encode(exponent) << F64_MANTISSA_BITS
                | mantissa;
            unsafe {
                Some(Self(NonNull::without_provenance(NonZero::new_unchecked(
                    (imm << tag::WIDTH) as usize | tag::F64,
                ))))
            }
        } else {
            // Not representable
            None
        }
    }

    #[cfg(not(target_pointer_width = "64"))]
    #[inline]
    pub(crate) fn from_f64(value: f64) -> Option<Self> {
        None
    }
}

impl Default for Repr {
    fn default() -> Self {
        Self::NIL
    }
}

#[cfg(test)]
mod test {
    use std::hash::{DefaultHasher, Hash, Hasher};

    use super::*;

    #[test]
    fn uninit_is_uninit() {
        assert!(Repr::UNINIT.is_uninit())
    }

    #[test]
    fn encode_decode_known() {
        assert!(matches!(Repr::NIL.decode(), Decode::Prim(Prim::Nil)));
        assert!(matches!(
            Repr::FALSE.decode(),
            Decode::Prim(Prim::Bool(false))
        ));
        assert!(matches!(
            Repr::TRUE.decode(),
            Decode::Prim(Prim::Bool(true))
        ));
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn encode_decode_i64() {
        let mut hasher = DefaultHasher::new();
        for counter in 0..100000usize {
            counter.hash(&mut hasher);
            let value = hasher.finish() as i64;
            if let Some(repr) = Repr::from_i64(value) {
                let out = match repr.decode() {
                    Decode::Prim(Prim::Int(out)) => out as i64,
                    _ => panic!("not an i64"),
                };
                assert_eq!(value, out)
            }
        }
    }

    #[test]
    fn encode_decode_i64_extremes() {
        let imin = IMIN as i128;
        let imax = IMAX as i128;
        assert!(matches!(
            Repr::from_i64(IMIN).unwrap().decode(),
            Decode::Prim(Prim::Int(v)) if v == imin
        ));
        assert!(matches!(
            Repr::from_i64(IMAX).unwrap().decode(),
            Decode::Prim(Prim::Int(v)) if v == imax
        ));
    }
}

#[cfg(all(test, target_pointer_width = "64"))]
mod test_f64 {
    use std::hash::{DefaultHasher, Hash, Hasher};

    use super::*;

    #[test]
    fn encode_decode_f64_exp() {
        for i in F64_IMM_EXPONENT_RANGE {
            assert_eq!(exp_decode(exp_encode(i)), i)
        }
        assert_eq!(exp_decode(exp_encode(0)), 0)
    }

    #[test]
    fn encode_decode_f64_special() {
        assert!(
            matches!(Repr::from_f64(f64::NAN).unwrap().decode(), Decode::Prim(Prim::F64(v)) if v.is_nan())
        );
        assert!(matches!(
            Repr::from_f64(f64::INFINITY).unwrap().decode(),
            Decode::Prim(Prim::F64(f64::INFINITY))
        ));
        assert!(matches!(
            Repr::from_f64(f64::NEG_INFINITY).unwrap().decode(),
            Decode::Prim(Prim::F64(f64::NEG_INFINITY))
        ))
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn encode_decode_f64() {
        let mut hasher = DefaultHasher::new();
        for counter in 0..100000usize {
            counter.hash(&mut hasher);
            let value = f64::from_bits(hasher.finish());
            if let Some(repr) = Repr::from_f64(value) {
                let out = match repr.decode() {
                    Decode::Prim(Prim::F64(out)) => out,
                    _ => panic!("not an f64"),
                };
                if value.is_nan() {
                    assert!(out.is_nan())
                } else {
                    assert_eq!(value, out)
                }
            }
        }
    }
}
