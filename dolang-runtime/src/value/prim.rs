use std::{
    cmp::Ordering,
    convert::Into,
    fmt::{self, Display},
    hash::{DefaultHasher, Hash},
    mem,
};

use crate::{
    error::{Error, Result},
    strand::Strand,
};

#[derive(Clone, Copy, PartialEq, PartialOrd, Debug)]
pub enum Prim {
    Nil,
    I64(i64),
    F64(f64),
    Bool(bool),
}

impl From<bool> for Prim {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}

impl From<i64> for Prim {
    fn from(v: i64) -> Self {
        Self::I64(v)
    }
}

impl From<f64> for Prim {
    fn from(v: f64) -> Self {
        Self::F64(v)
    }
}

impl From<()> for Prim {
    fn from(_: ()) -> Self {
        Prim::Nil
    }
}

impl<T: Copy + Into<Prim>> From<&T> for Prim {
    fn from(value: &T) -> Self {
        (*value).into()
    }
}

impl Display for Prim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Prim::Nil => write!(f, "nil"),
            Prim::I64(v) => write!(f, "{v}"),
            Prim::F64(v) => write!(f, "{v}"),
            Prim::Bool(v) => write!(f, "{v}"),
        }
    }
}

impl Prim {
    #[inline]
    pub(crate) fn op_bool(self, _strand: &Strand) -> bool {
        match self {
            Prim::Nil => false,
            Prim::F64(v) => v != 0.0,
            Prim::I64(v) => v != 0,
            Prim::Bool(v) => v,
        }
    }

    #[inline]
    pub(crate) fn to_index<'v, 's>(self, strand: &Strand<'v, 's>) -> Result<'v, 's, usize> {
        match self {
            Prim::I64(v) => Ok(usize::try_from(v).map_err(|_| Error::overflow(strand))?),
            _ => Err(Error::type_error(
                strand,
                "non-integral type used as integer index",
            )),
        }
    }

    #[inline]
    pub(crate) fn op_neg<'v, 's>(&self, strand: &Strand<'v, 's>) -> Result<'v, 's, Self> {
        match self {
            Prim::I64(v) => v
                .checked_neg()
                .map(Prim::from)
                .ok_or_else(|| Error::overflow(strand)),
            Prim::F64(v) => Ok((-v).into()),
            _ => Err(Error::type_error(strand, "negation of non-integer")),
        }
    }

    #[inline]
    pub(crate) fn op_bnot<'v, 's>(&self, strand: &Strand<'v, 's>) -> Result<'v, 's, Self> {
        match self {
            Prim::I64(v) => Ok(Prim::from(!v)),
            Prim::Bool(v) => Ok(Prim::from(!v)),
            _ => Err(Error::type_error(
                strand,
                "bitwise inverse of non-integer, non-boolean",
            )),
        }
    }

    #[inline]
    pub(crate) fn op_eq<'v, 's>(&self, _strand: &Strand<'v, 's>, other: &Self) -> bool {
        match (self, other) {
            (Prim::Nil, Prim::Nil) => true,
            (Prim::Bool(l), Prim::Bool(r)) => l == r,
            (Prim::I64(l), Prim::I64(r)) => l == r,
            (Prim::F64(l), Prim::F64(r)) => l == r,
            (Prim::I64(l), Prim::F64(r)) => {
                matches!(Self::compare_i64_f64(*l, *r), Some(Ordering::Equal))
            }
            (Prim::F64(l), Prim::I64(r)) => {
                matches!(Self::compare_i64_f64(*r, *l), Some(Ordering::Equal))
            }
            _ => false,
        }
    }

    #[inline]
    pub(crate) fn op_ne<'v, 's>(&self, _strand: &Strand<'v, 's>, other: &Self) -> bool {
        !self.op_eq(_strand, other)
    }

    #[inline]
    pub(crate) fn op_band<'v, 's>(
        &self,
        strand: &Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        match (self, other) {
            (Prim::I64(a), Prim::I64(b)) => Ok(Prim::from(a & b)),
            (Prim::Bool(a), Prim::Bool(b)) => Ok(Prim::from(a & b)),
            (Prim::I64(a), Prim::Bool(b)) => Ok(Prim::from(a & *b as i64)),
            (Prim::Bool(a), Prim::I64(b)) => Ok(Prim::from(*a as i64 & b)),
            _ => Err(Error::type_error(
                strand,
                "bitwise and of non-integer, non-boolean",
            )),
        }
    }

    #[inline]
    pub(crate) fn op_bor<'v, 's>(
        &self,
        strand: &Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        match (self, other) {
            (Prim::I64(a), Prim::I64(b)) => Ok(Prim::from(a | b)),
            (Prim::Bool(a), Prim::Bool(b)) => Ok(Prim::from(a | b)),
            (Prim::I64(a), Prim::Bool(b)) => Ok(Prim::from(a | *b as i64)),
            (Prim::Bool(a), Prim::I64(b)) => Ok(Prim::from(*a as i64 | b)),
            _ => Err(Error::type_error(
                strand,
                "bitwise or of non-integer, non-boolean",
            )),
        }
    }

    #[inline]
    pub(crate) fn op_bxor<'v, 's>(
        &self,
        strand: &Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        match (self, other) {
            (Prim::I64(a), Prim::I64(b)) => Ok(Prim::from(a ^ b)),
            (Prim::Bool(a), Prim::Bool(b)) => Ok(Prim::from(a ^ b)),
            (Prim::I64(a), Prim::Bool(b)) => Ok(Prim::from(a ^ *b as i64)),
            (Prim::Bool(a), Prim::I64(b)) => Ok(Prim::from(*a as i64 ^ b)),
            _ => Err(Error::type_error(
                strand,
                "bitwise xor of non-integer, non-boolean",
            )),
        }
    }

    #[inline]
    pub(crate) fn op_add<'v, 's>(
        &self,
        strand: &Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        match (self, other) {
            (Prim::I64(a), Prim::I64(b)) => a
                .checked_add(*b)
                .map(|v| Ok(v.into()))
                .unwrap_or(Err(Error::overflow(strand))),
            (Prim::F64(a), Prim::F64(b)) => Ok(Prim::F64(a + b)),
            (Prim::I64(a), Prim::F64(b)) => Ok(Prim::F64(*a as f64 + b)),
            (Prim::F64(a), Prim::I64(b)) => Ok(Prim::F64(a + *b as f64)),
            _ => Err(Error::type_error(strand, "addition of non-numeric type")),
        }
    }

    #[inline]
    pub(crate) fn op_sub<'v, 's>(
        &self,
        strand: &Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        match (self, other) {
            (Prim::I64(a), Prim::I64(b)) => a
                .checked_sub(*b)
                .map(|v| Ok(v.into()))
                .unwrap_or(Err(Error::overflow(strand))),
            (Prim::F64(a), Prim::F64(b)) => Ok(Prim::F64(a - b)),
            (Prim::I64(a), Prim::F64(b)) => Ok(Prim::F64(*a as f64 - b)),
            (Prim::F64(a), Prim::I64(b)) => Ok(Prim::F64(a - *b as f64)),
            _ => Err(Error::type_error(strand, "subtraction of non-numeric type")),
        }
    }

    #[inline]
    pub(crate) fn op_mul<'v, 's>(
        &self,
        strand: &Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        match (self, other) {
            (Prim::I64(a), Prim::I64(b)) => a
                .checked_mul(*b)
                .map(|v| Ok(v.into()))
                .unwrap_or(Err(Error::overflow(strand))),
            (Prim::F64(a), Prim::F64(b)) => Ok(Prim::F64(a * b)),
            (Prim::I64(a), Prim::F64(b)) => Ok(Prim::F64(*a as f64 * b)),
            (Prim::F64(a), Prim::I64(b)) => Ok(Prim::F64(a * *b as f64)),
            _ => Err(Error::type_error(
                strand,
                "multiplication of non-numeric type",
            )),
        }
    }

    #[inline]
    pub(crate) fn op_ediv<'v, 's>(
        &self,
        strand: &Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        match (self, other) {
            (Prim::I64(a), Prim::I64(b)) => a
                .checked_div_euclid(*b)
                .map(|v| Ok(v.into()))
                .unwrap_or(Err(Error::zero_div(strand))),
            (Prim::F64(a), Prim::F64(b)) => Ok(Prim::I64(a.div_euclid(*b) as i64)),
            (Prim::I64(a), Prim::F64(b)) => Ok(Prim::I64((*a as f64).div_euclid(*b) as i64)),
            (Prim::F64(a), Prim::I64(b)) => Ok(Prim::I64(a.div_euclid(*b as f64) as i64)),
            _ => Err(Error::type_error(
                strand,
                "Euclidean division of non-numeric type",
            )),
        }
    }

    #[inline]
    pub(crate) fn op_div<'v, 's>(
        &self,
        strand: &Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        match (self, other) {
            (Prim::I64(a), Prim::I64(b)) => {
                if *b == 0 {
                    Err(Error::zero_div(strand))
                } else {
                    Ok(Prim::F64(*a as f64 / *b as f64))
                }
            }
            (Prim::F64(a), Prim::F64(b)) => Ok(Prim::F64(a / b)),
            (Prim::I64(a), Prim::F64(b)) => Ok(Prim::F64(*a as f64 / b)),
            (Prim::F64(a), Prim::I64(b)) => Ok(Prim::F64(a / *b as f64)),
            _ => Err(Error::type_error(strand, "division of non-numeric type")),
        }
    }

    #[inline]
    pub(crate) fn op_mod<'v, 's>(
        &self,
        strand: &Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        match (self, other) {
            (Prim::I64(a), Prim::I64(b)) => a
                .checked_rem_euclid(*b)
                .map(|v| Ok(v.into()))
                .unwrap_or(Err(Error::zero_div(strand))),
            (Prim::F64(a), Prim::F64(b)) => Ok(Prim::F64(a.rem_euclid(*b))),
            (Prim::I64(a), Prim::F64(b)) => Ok(Prim::F64((*a as f64).rem_euclid(*b))),
            (Prim::F64(a), Prim::I64(b)) => Ok(Prim::F64(a.rem_euclid(*b as f64))),
            _ => Err(Error::type_error(
                strand,
                "Euclidean remainder of non-numeric type",
            )),
        }
    }

    // Reversed operations: compute `other op self` instead of `self op other`

    #[inline]
    pub(crate) fn op_rsub<'v, 's>(
        &self,
        strand: &Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        // other - self
        match (self, other) {
            (Prim::I64(a), Prim::I64(b)) => b
                .checked_sub(*a)
                .map(|v| Ok(v.into()))
                .unwrap_or(Err(Error::overflow(strand))),
            (Prim::F64(a), Prim::F64(b)) => Ok(Prim::F64(b - a)),
            (Prim::I64(a), Prim::F64(b)) => Ok(Prim::F64(b - *a as f64)),
            (Prim::F64(a), Prim::I64(b)) => Ok(Prim::F64(*b as f64 - a)),
            _ => Err(Error::type_error(strand, "subtraction of non-numeric type")),
        }
    }

    #[inline]
    pub(crate) fn op_rdiv<'v, 's>(
        &self,
        strand: &Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        // other / self
        match (self, other) {
            (Prim::I64(a), Prim::I64(b)) => {
                if *a == 0 {
                    Err(Error::zero_div(strand))
                } else {
                    Ok(Prim::F64(*b as f64 / *a as f64))
                }
            }
            (Prim::F64(a), Prim::F64(b)) => Ok(Prim::F64(b / a)),
            (Prim::I64(a), Prim::F64(b)) => Ok(Prim::F64(b / *a as f64)),
            (Prim::F64(a), Prim::I64(b)) => Ok(Prim::F64(*b as f64 / a)),
            _ => Err(Error::type_error(strand, "division of non-numeric type")),
        }
    }

    #[inline]
    pub(crate) fn op_rediv<'v, 's>(
        &self,
        strand: &Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        // other ediv self
        match (self, other) {
            (Prim::I64(a), Prim::I64(b)) => b
                .checked_div_euclid(*a)
                .map(|v| Ok(v.into()))
                .unwrap_or(Err(Error::zero_div(strand))),
            (Prim::F64(a), Prim::F64(b)) => Ok(Prim::I64(b.div_euclid(*a) as i64)),
            (Prim::I64(a), Prim::F64(b)) => Ok(Prim::I64(b.div_euclid(*a as f64) as i64)),
            (Prim::F64(a), Prim::I64(b)) => Ok(Prim::I64((*b as f64).div_euclid(*a) as i64)),
            _ => Err(Error::type_error(
                strand,
                "Euclidean division of non-numeric type",
            )),
        }
    }

    #[inline]
    pub(crate) fn op_rmod<'v, 's>(
        &self,
        strand: &Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        // other % self
        match (self, other) {
            (Prim::I64(a), Prim::I64(b)) => b
                .checked_rem_euclid(*a)
                .map(|v| Ok(v.into()))
                .unwrap_or(Err(Error::zero_div(strand))),
            (Prim::F64(a), Prim::F64(b)) => Ok(Prim::F64(b.rem_euclid(*a))),
            (Prim::I64(a), Prim::F64(b)) => Ok(Prim::F64(b.rem_euclid(*a as f64))),
            (Prim::F64(a), Prim::I64(b)) => Ok(Prim::F64((*b as f64).rem_euclid(*a))),
            _ => Err(Error::type_error(
                strand,
                "Euclidean remainder of non-numeric type",
            )),
        }
    }

    pub(crate) fn compare_i64_f64(i: i64, f: f64) -> Option<Ordering> {
        if f.is_nan() {
            return None;
        }
        if f.is_infinite() {
            return Some(if f.is_sign_positive() {
                Ordering::Less
            } else {
                Ordering::Greater
            });
        }
        let bits = f.to_bits();
        let exponent = (bits >> 52) & 0x7ff;
        let mantissa = bits & ((1u64 << 52) - 1);
        if exponent == 0 {
            return if mantissa == 0 {
                i.partial_cmp(&0)
            } else {
                Some(match i.cmp(&0) {
                    Ordering::Equal => {
                        if f.is_sign_positive() {
                            Ordering::Less
                        } else {
                            Ordering::Greater
                        }
                    }
                    other => other,
                })
            };
        }
        let exponent = (exponent - 1023) as i32;
        if exponent >= 63 {
            return Some(if f.is_sign_positive() {
                Ordering::Less
            } else {
                Ordering::Greater
            });
        }
        if f.trunc() == f {
            i.partial_cmp(&(f as i64))
        } else {
            let fl = f.floor() as i64;
            Some(if i <= fl {
                Ordering::Less
            } else {
                Ordering::Greater
            })
        }
    }

    #[inline]
    fn cmpop<'v, 's>(
        &self,
        strand: &Strand<'v, 's>,
        other: &Self,
        iop: fn(i64, i64) -> bool,
        fop: fn(f64, f64) -> bool,
        ifop: fn(i64, f64) -> bool,
        fiop: fn(f64, i64) -> bool,
    ) -> Result<'v, 's, Self> {
        use Prim::*;

        match (self, other) {
            (I64(l), I64(r)) => Ok(iop(*l, *r).into()),
            (F64(l), F64(r)) => Ok(fop(*l, *r).into()),
            (I64(l), F64(r)) => Ok(ifop(*l, *r).into()),
            (F64(l), I64(r)) => Ok(fiop(*l, *r).into()),
            (Bool(l), Bool(r)) => Ok(iop(*l as i64, *r as i64).into()),
            (Bool(l), F64(r)) => Ok(ifop(*l as i64, *r).into()),
            (F64(l), Bool(r)) => Ok(fiop(*l, *r as i64).into()),
            _ => Err(Error::type_error(strand, "comparison of non-numeric type")),
        }
    }

    pub(crate) fn op_lt<'v, 's>(
        &self,
        strand: &Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        self.cmpop(
            strand,
            other,
            |l, r| l < r,
            |l, r| l < r,
            |i, f| matches!(Self::compare_i64_f64(i, f), Some(Ordering::Less)),
            |f, i| matches!(Self::compare_i64_f64(i, f), Some(Ordering::Greater)),
        )
    }

    pub(crate) fn op_lte<'v, 's>(
        &self,
        strand: &Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        self.cmpop(
            strand,
            other,
            |l, r| l <= r,
            |l, r| l <= r,
            |i, f| {
                matches!(
                    Self::compare_i64_f64(i, f),
                    Some(Ordering::Less | Ordering::Equal)
                )
            },
            |f, i| {
                matches!(
                    Self::compare_i64_f64(i, f),
                    Some(Ordering::Greater | Ordering::Equal)
                )
            },
        )
    }

    pub(crate) fn op_gt<'v, 's>(
        &self,
        strand: &Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        self.cmpop(
            strand,
            other,
            |l, r| l > r,
            |l, r| l > r,
            |i, f| matches!(Self::compare_i64_f64(i, f), Some(Ordering::Greater)),
            |f, i| matches!(Self::compare_i64_f64(i, f), Some(Ordering::Less)),
        )
    }

    pub(crate) fn op_gte<'v, 's>(
        &self,
        strand: &Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        self.cmpop(
            strand,
            other,
            |l, r| l >= r,
            |l, r| l >= r,
            |i, f| {
                matches!(
                    Self::compare_i64_f64(i, f),
                    Some(Ordering::Greater | Ordering::Equal)
                )
            },
            |f, i| {
                matches!(
                    Self::compare_i64_f64(i, f),
                    Some(Ordering::Less | Ordering::Equal)
                )
            },
        )
    }

    pub(crate) fn op_hash<'v, 's>(&self, _strand: &Strand<'v, 's>, hasher: &mut DefaultHasher) {
        mem::discriminant(self).hash(hasher);
        match self {
            Prim::Nil => 0u8.hash(hasher),
            Prim::I64(v) => v.hash(hasher),
            Prim::F64(v) => {
                if v.is_nan() {
                    // Canonicalize NaN (not that putting NaN in a hash table is a good idea)
                    f64::NAN.to_bits().hash(hasher)
                } else {
                    v.to_bits().hash(hasher)
                }
            }
            Prim::Bool(v) => v.hash(hasher),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn nan() {
        assert_eq!(Prim::compare_i64_f64(0, f64::NAN), None);
    }

    #[test]
    fn infinities() {
        // Any integer is less than positive infinity
        assert_eq!(
            Prim::compare_i64_f64(i64::MAX, f64::INFINITY),
            Some(Ordering::Less)
        );
        assert_eq!(
            Prim::compare_i64_f64(i64::MIN, f64::INFINITY),
            Some(Ordering::Less)
        );

        // Any integer is greater than negative infinity
        assert_eq!(
            Prim::compare_i64_f64(i64::MAX, f64::NEG_INFINITY),
            Some(Ordering::Greater)
        );
        assert_eq!(
            Prim::compare_i64_f64(i64::MIN, f64::NEG_INFINITY),
            Some(Ordering::Greater)
        );
    }

    #[test]
    fn zero() {
        // Positive zero
        assert_eq!(Prim::compare_i64_f64(0, 0.0), Some(Ordering::Equal));
        assert_eq!(Prim::compare_i64_f64(1, 0.0), Some(Ordering::Greater));
        assert_eq!(Prim::compare_i64_f64(-1, 0.0), Some(Ordering::Less));

        // Negative zero (should behave same as positive zero)
        assert_eq!(Prim::compare_i64_f64(0, -0.0), Some(Ordering::Equal));
        assert_eq!(Prim::compare_i64_f64(1, -0.0), Some(Ordering::Greater));
        assert_eq!(Prim::compare_i64_f64(-1, -0.0), Some(Ordering::Less));
    }

    #[test]
    fn subnormals() {
        // Smallest positive subnormal
        let min_positive = f64::from_bits(1);
        assert_eq!(Prim::compare_i64_f64(0, min_positive), Some(Ordering::Less));
        assert_eq!(
            Prim::compare_i64_f64(1, min_positive),
            Some(Ordering::Greater)
        );
        assert_eq!(
            Prim::compare_i64_f64(-1, min_positive),
            Some(Ordering::Less)
        );

        // Largest positive subnormal (just below f64::MIN_POSITIVE)
        let max_subnormal = f64::from_bits((1u64 << 52) - 1);
        assert_eq!(
            Prim::compare_i64_f64(0, max_subnormal),
            Some(Ordering::Less)
        );
        assert_eq!(
            Prim::compare_i64_f64(1, max_subnormal),
            Some(Ordering::Greater)
        );

        // Smallest negative subnormal (largest in magnitude)
        let min_negative = -min_positive;
        assert_eq!(
            Prim::compare_i64_f64(0, min_negative),
            Some(Ordering::Greater)
        );
        assert_eq!(
            Prim::compare_i64_f64(-1, min_negative),
            Some(Ordering::Less)
        );
        assert_eq!(
            Prim::compare_i64_f64(1, min_negative),
            Some(Ordering::Greater)
        );

        // Largest negative subnormal (smallest in magnitude)
        let max_negative_subnormal = -max_subnormal;
        assert_eq!(
            Prim::compare_i64_f64(0, max_negative_subnormal),
            Some(Ordering::Greater)
        );
        assert_eq!(
            Prim::compare_i64_f64(-1, max_negative_subnormal),
            Some(Ordering::Less)
        );
    }

    #[test]
    fn small_integers() {
        // Simple cases
        assert_eq!(Prim::compare_i64_f64(5, 5.0), Some(Ordering::Equal));
        assert_eq!(Prim::compare_i64_f64(5, 4.0), Some(Ordering::Greater));
        assert_eq!(Prim::compare_i64_f64(5, 6.0), Some(Ordering::Less));

        assert_eq!(Prim::compare_i64_f64(-5, -5.0), Some(Ordering::Equal));
        assert_eq!(Prim::compare_i64_f64(-5, -6.0), Some(Ordering::Greater));
        assert_eq!(Prim::compare_i64_f64(-5, -4.0), Some(Ordering::Less));
    }

    #[test]
    fn fractional_values() {
        // Positive fractional values
        assert_eq!(Prim::compare_i64_f64(5, 5.5), Some(Ordering::Less));
        assert_eq!(Prim::compare_i64_f64(5, 4.5), Some(Ordering::Greater));
        assert_eq!(Prim::compare_i64_f64(5, 5.1), Some(Ordering::Less));
        assert_eq!(Prim::compare_i64_f64(5, 5.9), Some(Ordering::Less));

        // Negative fractional values
        assert_eq!(Prim::compare_i64_f64(-5, -5.5), Some(Ordering::Greater));
        assert_eq!(Prim::compare_i64_f64(-5, -4.5), Some(Ordering::Less));
        assert_eq!(Prim::compare_i64_f64(-5, -5.1), Some(Ordering::Greater));
        assert_eq!(Prim::compare_i64_f64(-5, -5.9), Some(Ordering::Greater));
    }

    #[test]
    fn exact_representation_boundary() {
        // 2^53 is the boundary where all integers can be exactly represented
        let boundary = 1i64 << 53; // 9007199254740992

        assert_eq!(
            Prim::compare_i64_f64(boundary, boundary as f64),
            Some(Ordering::Equal)
        );
        assert_eq!(
            Prim::compare_i64_f64(boundary - 1, (boundary - 1) as f64),
            Some(Ordering::Equal)
        );

        // Negative boundary
        assert_eq!(
            Prim::compare_i64_f64(-boundary, (-boundary) as f64),
            Some(Ordering::Equal)
        );
        assert_eq!(
            Prim::compare_i64_f64(-boundary + 1, (-boundary + 1) as f64),
            Some(Ordering::Equal)
        );
    }

    #[test]
    fn beyond_exact_representation() {
        // Beyond 2^53, not all consecutive integers can be represented
        // 2^54 = 18014398509481984
        let val = 1i64 << 54;

        // At this scale, floats have a gap of 2 between consecutive representable integers
        let f = val as f64;
        assert_eq!(Prim::compare_i64_f64(val, f), Some(Ordering::Equal));

        // val+1 should round to either val or val+2 as a float
        // Let's check the actual behavior
        let val_plus_1_as_f64 = (val + 1) as f64;
        // This will round to val
        assert_eq!(
            Prim::compare_i64_f64(val + 1, val_plus_1_as_f64),
            Some(Ordering::Greater)
        );
    }

    #[test]
    fn i64_max() {
        // i64::MAX = 9223372036854775807 = 2^63 - 1
        // As f64, this loses precision
        let max_as_f64 = i64::MAX as f64;

        // The float representation rounds to 2^63 (which is outside i64 range)
        // So i64::MAX should compare as less than max_as_f64
        assert_eq!(
            Prim::compare_i64_f64(i64::MAX, max_as_f64),
            Some(Ordering::Less)
        );
    }

    #[test]
    fn i64_min() {
        let min_as_f64 = i64::MIN as f64;
        assert_eq!(
            Prim::compare_i64_f64(i64::MIN, min_as_f64),
            Some(Ordering::Greater)
        );
    }

    #[test]
    fn float_too_large_for_i64() {
        // Floats with exponent >= 63 are outside i64 range
        let too_large_positive = 1e63f64; // Exactly 2^63
        assert_eq!(
            Prim::compare_i64_f64(i64::MAX, too_large_positive),
            Some(Ordering::Less)
        );
        assert_eq!(
            Prim::compare_i64_f64(0, too_large_positive),
            Some(Ordering::Less)
        );

        let way_too_large = 1e100f64;
        assert_eq!(
            Prim::compare_i64_f64(i64::MAX, way_too_large),
            Some(Ordering::Less)
        );

        let too_large_negative = -1e63f64 - 1.0; // Less than -2^63
        assert_eq!(
            Prim::compare_i64_f64(i64::MIN, too_large_negative),
            Some(Ordering::Greater)
        );
        assert_eq!(
            Prim::compare_i64_f64(0, too_large_negative),
            Some(Ordering::Greater)
        );
    }

    #[test]
    fn fractional_near_boundaries() {
        // Test fractional values near large integer boundaries
        let large_int = 1i64 << 60;
        let large_f = large_int as f64;

        // Since large_int is a power of 2, it's exactly representable
        assert_eq!(
            Prim::compare_i64_f64(large_int, large_f),
            Some(Ordering::Equal)
        );

        // Add a small epsilon (though at this scale, the float may not be able to represent it)
        let large_f_plus = large_f + 0.5;
        if large_f_plus != large_f {
            // If the float can represent the difference
            assert_eq!(
                Prim::compare_i64_f64(large_int, large_f_plus),
                Some(Ordering::Less)
            );
        }
    }

    #[test]
    fn powers_of_two() {
        // Powers of two should be exactly representable
        for exp in 0..63 {
            let val = 1i64 << exp;
            assert_eq!(
                Prim::compare_i64_f64(val, val as f64),
                Some(Ordering::Equal),
                "Failed at 2^{}",
                exp
            );
            assert_eq!(
                Prim::compare_i64_f64(-val, (-val) as f64),
                Some(Ordering::Equal),
                "Failed at -2^{}",
                exp
            );
        }
    }

    #[test]
    fn cross_zero_comparisons() {
        assert_eq!(Prim::compare_i64_f64(1, -1.0), Some(Ordering::Greater));
        assert_eq!(Prim::compare_i64_f64(-1, 1.0), Some(Ordering::Less));
        assert_eq!(Prim::compare_i64_f64(0, 1.0), Some(Ordering::Less));
        assert_eq!(Prim::compare_i64_f64(0, -1.0), Some(Ordering::Greater));
    }
}
