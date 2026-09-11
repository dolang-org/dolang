use std::{
    cmp::Ordering,
    convert::Into,
    fmt::{self, Display},
    hash::{DefaultHasher, Hash},
    mem,
};

use crate::{
    arg::Args,
    error::{Error, Result},
    strand::Strand,
    sym::Sym,
};

use super::{Slot, Value};

pub type Integer = i128;

#[derive(Clone, Copy, PartialEq, PartialOrd, Debug)]
pub enum Prim {
    Nil,
    Int(Integer),
    F64(f64),
    Bool(bool),
}

impl From<bool> for Prim {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}

impl From<Integer> for Prim {
    fn from(v: Integer) -> Self {
        Self::Int(v)
    }
}

impl From<i64> for Prim {
    fn from(v: i64) -> Self {
        Self::Int(v.into())
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
            Prim::Int(v) => write!(f, "{v}"),
            Prim::F64(v) => write!(f, "{v}"),
            Prim::Bool(v) => write!(f, "{v}"),
        }
    }
}

impl Prim {
    pub(crate) fn op_get<'v, 'a, 's>(
        self,
        receiver: &'a Value<'v>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        _out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match self {
            Prim::Int(_) => crate::object::num::int_get(strand, receiver, field, _out),
            Prim::F64(_) => crate::object::num::float_get(strand, receiver, field, _out),
            _ => Err(Error::type_error(strand, "field get not supported")),
        }
    }

    pub(crate) async fn op_mcall<'v, 'a, 's>(
        self,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match self {
            Prim::Int(value) => {
                let receiver = Value::from_prim(strand, self);
                crate::object::num::int_mcall(strand, &receiver, value, method, args, out).await
            }
            Prim::F64(value) => {
                let receiver = Value::from_prim(strand, self);
                crate::object::num::float_mcall(strand, &receiver, value, method, args, out).await
            }
            _ => Err(Error::type_error(strand, "method call not supported")),
        }
    }

    #[inline]
    pub(crate) fn op_bool(self, _strand: &mut Strand) -> bool {
        match self {
            Prim::Nil => false,
            Prim::F64(v) => v != 0.0,
            Prim::Int(v) => v != 0,
            Prim::Bool(v) => v,
        }
    }

    #[inline]
    pub(crate) fn to_index<'v, 's>(self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, usize> {
        match self {
            Prim::Int(v) => Ok(usize::try_from(v).map_err(|_| Error::overflow(strand))?),
            _ => Err(Error::type_error(
                strand,
                "non-integral type used as integer index",
            )),
        }
    }

    #[inline]
    pub(crate) fn op_neg<'v, 's>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, Self> {
        match self {
            Prim::Int(v) => v
                .checked_neg()
                .map(Prim::from)
                .ok_or_else(|| Error::overflow(strand)),
            Prim::F64(v) => Ok((-v).into()),
            _ => Err(Error::type_error(strand, "negation of non-integer")),
        }
    }

    #[inline]
    pub(crate) fn op_bnot<'v, 's>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, Self> {
        match self {
            Prim::Int(v) => Ok(Prim::from(!v)),
            Prim::Bool(v) => Ok(Prim::from(!v)),
            _ => Err(Error::type_error(
                strand,
                "bitwise inverse of non-integer, non-boolean",
            )),
        }
    }

    #[inline]
    pub(crate) fn op_eq<'v, 's>(&self, _strand: &mut Strand<'v, 's>, other: &Self) -> bool {
        match (self, other) {
            (Prim::Nil, Prim::Nil) => true,
            (Prim::Bool(l), Prim::Bool(r)) => l == r,
            (Prim::Int(l), Prim::Int(r)) => l == r,
            (Prim::F64(l), Prim::F64(r)) => l == r,
            (Prim::Int(l), Prim::F64(r)) => {
                matches!(Self::compare_int_f64(*l, *r), Some(Ordering::Equal))
            }
            (Prim::F64(l), Prim::Int(r)) => {
                matches!(Self::compare_int_f64(*r, *l), Some(Ordering::Equal))
            }
            _ => false,
        }
    }

    #[inline]
    pub(crate) fn op_ne<'v, 's>(&self, _strand: &mut Strand<'v, 's>, other: &Self) -> bool {
        !self.op_eq(_strand, other)
    }

    #[inline]
    pub(crate) fn op_band<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        match (self, other) {
            (Prim::Int(a), Prim::Int(b)) => Ok(Prim::from(a & b)),
            (Prim::Bool(a), Prim::Bool(b)) => Ok(Prim::from(a & b)),
            (Prim::Int(a), Prim::Bool(b)) => Ok(Prim::from(a & *b as i128)),
            (Prim::Bool(a), Prim::Int(b)) => Ok(Prim::from(*a as i128 & b)),
            _ => Err(Error::type_error(
                strand,
                "bitwise and of non-integer, non-boolean",
            )),
        }
    }

    #[inline]
    pub(crate) fn op_bor<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        match (self, other) {
            (Prim::Int(a), Prim::Int(b)) => Ok(Prim::from(a | b)),
            (Prim::Bool(a), Prim::Bool(b)) => Ok(Prim::from(a | b)),
            (Prim::Int(a), Prim::Bool(b)) => Ok(Prim::from(a | *b as i128)),
            (Prim::Bool(a), Prim::Int(b)) => Ok(Prim::from(*a as i128 | b)),
            _ => Err(Error::type_error(
                strand,
                "bitwise or of non-integer, non-boolean",
            )),
        }
    }

    #[inline]
    pub(crate) fn op_bxor<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        match (self, other) {
            (Prim::Int(a), Prim::Int(b)) => Ok(Prim::from(a ^ b)),
            (Prim::Bool(a), Prim::Bool(b)) => Ok(Prim::from(a ^ b)),
            (Prim::Int(a), Prim::Bool(b)) => Ok(Prim::from(a ^ *b as i128)),
            (Prim::Bool(a), Prim::Int(b)) => Ok(Prim::from(*a as i128 ^ b)),
            _ => Err(Error::type_error(
                strand,
                "bitwise xor of non-integer, non-boolean",
            )),
        }
    }

    #[inline]
    pub(crate) fn op_shl<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        let count = match other {
            Prim::Int(v) => match u32::try_from(*v) {
                Ok(count) if count < i128::BITS => count,
                _ => return Err(Error::overflow(strand)),
            },
            _ => return Err(Error::type_error(strand, "left shift by non-integer")),
        };
        match self {
            Prim::Int(v) => v
                .checked_shl(count)
                .map(Prim::from)
                .ok_or_else(|| Error::overflow(strand)),
            _ => Err(Error::type_error(strand, "left shift of non-integer")),
        }
    }

    #[inline]
    pub(crate) fn op_shr<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        let count = match other {
            Prim::Int(v) => match u32::try_from(*v) {
                Ok(count) if count < i128::BITS => count,
                _ => return Err(Error::overflow(strand)),
            },
            _ => return Err(Error::type_error(strand, "right shift by non-integer")),
        };
        match self {
            Prim::Int(v) => v
                .checked_shr(count)
                .map(Prim::from)
                .ok_or_else(|| Error::overflow(strand)),
            _ => Err(Error::type_error(strand, "right shift of non-integer")),
        }
    }

    #[inline]
    pub(crate) fn op_add<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        match (self, other) {
            (Prim::Int(a), Prim::Int(b)) => a
                .checked_add(*b)
                .map(|v| Ok(v.into()))
                .unwrap_or_else(|| Err(Error::overflow(strand))),
            (Prim::F64(a), Prim::F64(b)) => Ok(Prim::F64(a + b)),
            (Prim::Int(a), Prim::F64(b)) => Ok(Prim::F64(*a as f64 + b)),
            (Prim::F64(a), Prim::Int(b)) => Ok(Prim::F64(a + *b as f64)),
            _ => Err(Error::type_error(strand, "addition of non-numeric type")),
        }
    }

    #[inline]
    pub(crate) fn op_sub<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        match (self, other) {
            (Prim::Int(a), Prim::Int(b)) => a
                .checked_sub(*b)
                .map(|v| Ok(v.into()))
                .unwrap_or_else(|| Err(Error::overflow(strand))),
            (Prim::F64(a), Prim::F64(b)) => Ok(Prim::F64(a - b)),
            (Prim::Int(a), Prim::F64(b)) => Ok(Prim::F64(*a as f64 - b)),
            (Prim::F64(a), Prim::Int(b)) => Ok(Prim::F64(a - *b as f64)),
            _ => Err(Error::type_error(strand, "subtraction of non-numeric type")),
        }
    }

    #[inline]
    pub(crate) fn op_mul<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        match (self, other) {
            (Prim::Int(a), Prim::Int(b)) => a
                .checked_mul(*b)
                .map(|v| Ok(v.into()))
                .unwrap_or_else(|| Err(Error::overflow(strand))),
            (Prim::F64(a), Prim::F64(b)) => Ok(Prim::F64(a * b)),
            (Prim::Int(a), Prim::F64(b)) => Ok(Prim::F64(*a as f64 * b)),
            (Prim::F64(a), Prim::Int(b)) => Ok(Prim::F64(a * *b as f64)),
            _ => Err(Error::type_error(
                strand,
                "multiplication of non-numeric type",
            )),
        }
    }

    #[inline]
    pub(crate) fn op_ediv<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        match (self, other) {
            (Prim::Int(a), Prim::Int(b)) => a
                .checked_div_euclid(*b)
                .map(|v| Ok(v.into()))
                .unwrap_or_else(|| Err(Error::zero_div(strand))),
            (Prim::F64(a), Prim::F64(b)) => Ok(Prim::Int(a.div_euclid(*b) as i128)),
            (Prim::Int(a), Prim::F64(b)) => Ok(Prim::Int((*a as f64).div_euclid(*b) as i128)),
            (Prim::F64(a), Prim::Int(b)) => Ok(Prim::Int(a.div_euclid(*b as f64) as i128)),
            _ => Err(Error::type_error(
                strand,
                "Euclidean division of non-numeric type",
            )),
        }
    }

    #[inline]
    pub(crate) fn op_div<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        match (self, other) {
            (Prim::Int(a), Prim::Int(b)) => {
                if *b == 0 {
                    Err(Error::zero_div(strand))
                } else {
                    Ok(Prim::F64(*a as f64 / *b as f64))
                }
            }
            (Prim::F64(a), Prim::F64(b)) => Ok(Prim::F64(a / b)),
            (Prim::Int(a), Prim::F64(b)) => Ok(Prim::F64(*a as f64 / b)),
            (Prim::F64(a), Prim::Int(b)) => Ok(Prim::F64(a / *b as f64)),
            _ => Err(Error::type_error(strand, "division of non-numeric type")),
        }
    }

    #[inline]
    pub(crate) fn op_mod<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        match (self, other) {
            (Prim::Int(a), Prim::Int(b)) => a
                .checked_rem_euclid(*b)
                .map(|v| Ok(v.into()))
                .unwrap_or_else(|| Err(Error::zero_div(strand))),
            (Prim::F64(a), Prim::F64(b)) => Ok(Prim::F64(a.rem_euclid(*b))),
            (Prim::Int(a), Prim::F64(b)) => Ok(Prim::F64((*a as f64).rem_euclid(*b))),
            (Prim::F64(a), Prim::Int(b)) => Ok(Prim::F64(a.rem_euclid(*b as f64))),
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
        strand: &mut Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        // other - self
        match (self, other) {
            (Prim::Int(a), Prim::Int(b)) => b
                .checked_sub(*a)
                .map(|v| Ok(v.into()))
                .unwrap_or_else(|| Err(Error::overflow(strand))),
            (Prim::F64(a), Prim::F64(b)) => Ok(Prim::F64(b - a)),
            (Prim::Int(a), Prim::F64(b)) => Ok(Prim::F64(b - *a as f64)),
            (Prim::F64(a), Prim::Int(b)) => Ok(Prim::F64(*b as f64 - a)),
            _ => Err(Error::type_error(strand, "subtraction of non-numeric type")),
        }
    }

    #[inline]
    pub(crate) fn op_rdiv<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        // other / self
        match (self, other) {
            (Prim::Int(a), Prim::Int(b)) => {
                if *a == 0 {
                    Err(Error::zero_div(strand))
                } else {
                    Ok(Prim::F64(*b as f64 / *a as f64))
                }
            }
            (Prim::F64(a), Prim::F64(b)) => Ok(Prim::F64(b / a)),
            (Prim::Int(a), Prim::F64(b)) => Ok(Prim::F64(b / *a as f64)),
            (Prim::F64(a), Prim::Int(b)) => Ok(Prim::F64(*b as f64 / a)),
            _ => Err(Error::type_error(strand, "division of non-numeric type")),
        }
    }

    #[inline]
    pub(crate) fn op_rediv<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        // other ediv self
        match (self, other) {
            (Prim::Int(a), Prim::Int(b)) => b
                .checked_div_euclid(*a)
                .map(|v| Ok(v.into()))
                .unwrap_or_else(|| Err(Error::zero_div(strand))),
            (Prim::F64(a), Prim::F64(b)) => Ok(Prim::Int(b.div_euclid(*a) as i128)),
            (Prim::Int(a), Prim::F64(b)) => Ok(Prim::Int(b.div_euclid(*a as f64) as i128)),
            (Prim::F64(a), Prim::Int(b)) => Ok(Prim::Int((*b as f64).div_euclid(*a) as i128)),
            _ => Err(Error::type_error(
                strand,
                "Euclidean division of non-numeric type",
            )),
        }
    }

    #[inline]
    pub(crate) fn op_rmod<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        // other % self
        match (self, other) {
            (Prim::Int(a), Prim::Int(b)) => b
                .checked_rem_euclid(*a)
                .map(|v| Ok(v.into()))
                .unwrap_or_else(|| Err(Error::zero_div(strand))),
            (Prim::F64(a), Prim::F64(b)) => Ok(Prim::F64(b.rem_euclid(*a))),
            (Prim::Int(a), Prim::F64(b)) => Ok(Prim::F64(b.rem_euclid(*a as f64))),
            (Prim::F64(a), Prim::Int(b)) => Ok(Prim::F64((*b as f64).rem_euclid(*a))),
            _ => Err(Error::type_error(
                strand,
                "Euclidean remainder of non-numeric type",
            )),
        }
    }

    pub(crate) fn compare_int_f64(i: Integer, f: f64) -> Option<Ordering> {
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
        if exponent > 127 {
            return Some(if f.is_sign_positive() {
                Ordering::Less
            } else {
                Ordering::Greater
            });
        }
        if exponent == 127 {
            return Some(if f == Integer::MIN as f64 && i == Integer::MIN {
                Ordering::Equal
            } else if f.is_sign_positive() {
                Ordering::Less
            } else {
                Ordering::Greater
            });
        }
        if f.trunc() == f {
            i.partial_cmp(&(f as i128))
        } else {
            let fl = f.floor() as i128;
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
        strand: &mut Strand<'v, 's>,
        other: &Self,
        iop: fn(Integer, Integer) -> bool,
        fop: fn(f64, f64) -> bool,
        ifop: fn(Integer, f64) -> bool,
        fiop: fn(f64, Integer) -> bool,
    ) -> Result<'v, 's, Self> {
        use Prim::*;

        match (self, other) {
            (Int(l), Int(r)) => Ok(iop(*l, *r).into()),
            (F64(l), F64(r)) => Ok(fop(*l, *r).into()),
            (Int(l), F64(r)) => Ok(ifop(*l, *r).into()),
            (F64(l), Int(r)) => Ok(fiop(*l, *r).into()),
            (Bool(l), Bool(r)) => Ok(iop(*l as i128, *r as i128).into()),
            (Bool(l), F64(r)) => Ok(ifop(*l as i128, *r).into()),
            (F64(l), Bool(r)) => Ok(fiop(*l, *r as i128).into()),
            _ => Err(Error::type_error(strand, "comparison of non-numeric type")),
        }
    }

    pub(crate) fn op_lt<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        self.cmpop(
            strand,
            other,
            |l, r| l < r,
            |l, r| l < r,
            |i, f| matches!(Self::compare_int_f64(i, f), Some(Ordering::Less)),
            |f, i| matches!(Self::compare_int_f64(i, f), Some(Ordering::Greater)),
        )
    }

    pub(crate) fn op_lte<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        self.cmpop(
            strand,
            other,
            |l, r| l <= r,
            |l, r| l <= r,
            |i, f| {
                matches!(
                    Self::compare_int_f64(i, f),
                    Some(Ordering::Less | Ordering::Equal)
                )
            },
            |f, i| {
                matches!(
                    Self::compare_int_f64(i, f),
                    Some(Ordering::Greater | Ordering::Equal)
                )
            },
        )
    }

    pub(crate) fn op_gt<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        self.cmpop(
            strand,
            other,
            |l, r| l > r,
            |l, r| l > r,
            |i, f| matches!(Self::compare_int_f64(i, f), Some(Ordering::Greater)),
            |f, i| matches!(Self::compare_int_f64(i, f), Some(Ordering::Less)),
        )
    }

    pub(crate) fn op_gte<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        other: &Self,
    ) -> Result<'v, 's, Self> {
        self.cmpop(
            strand,
            other,
            |l, r| l >= r,
            |l, r| l >= r,
            |i, f| {
                matches!(
                    Self::compare_int_f64(i, f),
                    Some(Ordering::Greater | Ordering::Equal)
                )
            },
            |f, i| {
                matches!(
                    Self::compare_int_f64(i, f),
                    Some(Ordering::Less | Ordering::Equal)
                )
            },
        )
    }

    pub(crate) fn op_hash<'v, 's>(&self, _strand: &mut Strand<'v, 's>, hasher: &mut DefaultHasher) {
        mem::discriminant(self).hash(hasher);
        match self {
            Prim::Nil => 0u8.hash(hasher),
            Prim::Int(v) => v.hash(hasher),
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
        assert_eq!(Prim::compare_int_f64(0, f64::NAN), None);
    }

    #[test]
    fn infinities() {
        // Any integer is less than positive infinity
        assert_eq!(
            Prim::compare_int_f64(Integer::MAX, f64::INFINITY),
            Some(Ordering::Less)
        );
        assert_eq!(
            Prim::compare_int_f64(Integer::MIN, f64::INFINITY),
            Some(Ordering::Less)
        );

        // Any integer is greater than negative infinity
        assert_eq!(
            Prim::compare_int_f64(Integer::MAX, f64::NEG_INFINITY),
            Some(Ordering::Greater)
        );
        assert_eq!(
            Prim::compare_int_f64(Integer::MIN, f64::NEG_INFINITY),
            Some(Ordering::Greater)
        );
    }

    #[test]
    fn zero() {
        // Positive zero
        assert_eq!(Prim::compare_int_f64(0, 0.0), Some(Ordering::Equal));
        assert_eq!(Prim::compare_int_f64(1, 0.0), Some(Ordering::Greater));
        assert_eq!(Prim::compare_int_f64(-1, 0.0), Some(Ordering::Less));

        // Negative zero (should behave same as positive zero)
        assert_eq!(Prim::compare_int_f64(0, -0.0), Some(Ordering::Equal));
        assert_eq!(Prim::compare_int_f64(1, -0.0), Some(Ordering::Greater));
        assert_eq!(Prim::compare_int_f64(-1, -0.0), Some(Ordering::Less));
    }

    #[test]
    fn subnormals() {
        // Smallest positive subnormal
        let min_positive = f64::from_bits(1);
        assert_eq!(Prim::compare_int_f64(0, min_positive), Some(Ordering::Less));
        assert_eq!(
            Prim::compare_int_f64(1, min_positive),
            Some(Ordering::Greater)
        );
        assert_eq!(
            Prim::compare_int_f64(-1, min_positive),
            Some(Ordering::Less)
        );

        // Largest positive subnormal (just below f64::MIN_POSITIVE)
        let max_subnormal = f64::from_bits((1u64 << 52) - 1);
        assert_eq!(
            Prim::compare_int_f64(0, max_subnormal),
            Some(Ordering::Less)
        );
        assert_eq!(
            Prim::compare_int_f64(1, max_subnormal),
            Some(Ordering::Greater)
        );

        // Smallest negative subnormal (largest in magnitude)
        let min_negative = -min_positive;
        assert_eq!(
            Prim::compare_int_f64(0, min_negative),
            Some(Ordering::Greater)
        );
        assert_eq!(
            Prim::compare_int_f64(-1, min_negative),
            Some(Ordering::Less)
        );
        assert_eq!(
            Prim::compare_int_f64(1, min_negative),
            Some(Ordering::Greater)
        );

        // Largest negative subnormal (smallest in magnitude)
        let max_negative_subnormal = -max_subnormal;
        assert_eq!(
            Prim::compare_int_f64(0, max_negative_subnormal),
            Some(Ordering::Greater)
        );
        assert_eq!(
            Prim::compare_int_f64(-1, max_negative_subnormal),
            Some(Ordering::Less)
        );
    }

    #[test]
    fn small_integers() {
        // Simple cases
        assert_eq!(Prim::compare_int_f64(5, 5.0), Some(Ordering::Equal));
        assert_eq!(Prim::compare_int_f64(5, 4.0), Some(Ordering::Greater));
        assert_eq!(Prim::compare_int_f64(5, 6.0), Some(Ordering::Less));

        assert_eq!(Prim::compare_int_f64(-5, -5.0), Some(Ordering::Equal));
        assert_eq!(Prim::compare_int_f64(-5, -6.0), Some(Ordering::Greater));
        assert_eq!(Prim::compare_int_f64(-5, -4.0), Some(Ordering::Less));
    }

    #[test]
    fn fractional_values() {
        // Positive fractional values
        assert_eq!(Prim::compare_int_f64(5, 5.5), Some(Ordering::Less));
        assert_eq!(Prim::compare_int_f64(5, 4.5), Some(Ordering::Greater));
        assert_eq!(Prim::compare_int_f64(5, 5.1), Some(Ordering::Less));
        assert_eq!(Prim::compare_int_f64(5, 5.9), Some(Ordering::Less));

        // Negative fractional values
        assert_eq!(Prim::compare_int_f64(-5, -5.5), Some(Ordering::Greater));
        assert_eq!(Prim::compare_int_f64(-5, -4.5), Some(Ordering::Less));
        assert_eq!(Prim::compare_int_f64(-5, -5.1), Some(Ordering::Greater));
        assert_eq!(Prim::compare_int_f64(-5, -5.9), Some(Ordering::Greater));
    }

    #[test]
    fn exact_representation_boundary() {
        // 2^53 is the boundary where all integers can be exactly represented
        let boundary = (1 as Integer) << 53; // 9007199254740992

        assert_eq!(
            Prim::compare_int_f64(boundary, boundary as f64),
            Some(Ordering::Equal)
        );
        assert_eq!(
            Prim::compare_int_f64(boundary - 1, (boundary - 1) as f64),
            Some(Ordering::Equal)
        );

        // Negative boundary
        assert_eq!(
            Prim::compare_int_f64(-boundary, (-boundary) as f64),
            Some(Ordering::Equal)
        );
        assert_eq!(
            Prim::compare_int_f64(-boundary + 1, (-boundary + 1) as f64),
            Some(Ordering::Equal)
        );
    }

    #[test]
    fn beyond_exact_representation() {
        // Beyond 2^53, not all consecutive integers can be represented
        // 2^54 = 18014398509481984
        let val = (1 as Integer) << 54;

        // At this scale, floats have a gap of 2 between consecutive representable integers
        let f = val as f64;
        assert_eq!(Prim::compare_int_f64(val, f), Some(Ordering::Equal));

        // val+1 should round to either val or val+2 as a float
        // Let's check the actual behavior
        let val_plus_1_as_f64 = (val + 1) as f64;
        // This will round to val
        assert_eq!(
            Prim::compare_int_f64(val + 1, val_plus_1_as_f64),
            Some(Ordering::Greater)
        );
    }

    #[test]
    fn integer_max() {
        let max_as_f64 = Integer::MAX as f64;

        // Integer::MAX rounds up to 2^127, just outside the Integer domain.
        assert_eq!(
            Prim::compare_int_f64(Integer::MAX, max_as_f64),
            Some(Ordering::Less)
        );
    }

    #[test]
    fn integer_min() {
        let min_as_f64 = Integer::MIN as f64;
        assert_eq!(
            Prim::compare_int_f64(Integer::MIN, min_as_f64),
            Some(Ordering::Equal)
        );
    }

    #[test]
    fn float_near_integer_boundaries() {
        let positive_limit = 2.0f64.powi(127);
        assert_eq!(
            Prim::compare_int_f64(Integer::MAX, positive_limit),
            Some(Ordering::Less)
        );
        assert_eq!(
            Prim::compare_int_f64(0, positive_limit),
            Some(Ordering::Less)
        );

        let above_positive_limit = f64::from_bits(positive_limit.to_bits() + 1);
        assert_eq!(
            Prim::compare_int_f64(Integer::MAX, above_positive_limit),
            Some(Ordering::Less)
        );

        let negative_limit = -positive_limit;
        assert_eq!(
            Prim::compare_int_f64(Integer::MIN, negative_limit),
            Some(Ordering::Equal)
        );

        let below_negative_limit = f64::from_bits(negative_limit.to_bits() + 1);
        assert_eq!(
            Prim::compare_int_f64(Integer::MIN, below_negative_limit),
            Some(Ordering::Greater)
        );
        assert_eq!(
            Prim::compare_int_f64(0, below_negative_limit),
            Some(Ordering::Greater)
        );
    }

    #[test]
    fn fractional_near_boundaries() {
        // Test fractional values near large integer boundaries
        let large_int = (1 as Integer) << 60;
        let large_f = large_int as f64;

        // Since large_int is a power of 2, it's exactly representable
        assert_eq!(
            Prim::compare_int_f64(large_int, large_f),
            Some(Ordering::Equal)
        );

        let next_float = f64::from_bits(large_f.to_bits() + 1);
        assert_eq!(
            Prim::compare_int_f64(large_int, next_float),
            Some(Ordering::Less)
        );
    }

    #[test]
    fn powers_of_two() {
        // Powers of two should be exactly representable
        for exp in 0..127 {
            let val = (1 as Integer) << exp;
            assert_eq!(
                Prim::compare_int_f64(val, val as f64),
                Some(Ordering::Equal),
                "Failed at 2^{}",
                exp
            );
            assert_eq!(
                Prim::compare_int_f64(-val, (-val) as f64),
                Some(Ordering::Equal),
                "Failed at -2^{}",
                exp
            );
        }
    }

    #[test]
    fn cross_zero_comparisons() {
        assert_eq!(Prim::compare_int_f64(1, -1.0), Some(Ordering::Greater));
        assert_eq!(Prim::compare_int_f64(-1, 1.0), Some(Ordering::Less));
        assert_eq!(Prim::compare_int_f64(0, 1.0), Some(Ordering::Less));
        assert_eq!(Prim::compare_int_f64(0, -1.0), Some(Ordering::Greater));
    }
}
