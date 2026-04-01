use std::{
    hint::unreachable_unchecked,
    io,
    ptr::{self, NonNull},
    slice,
};

use super::{DecResult, Decode, EncResult, Encode, UnsafeDecode};

// Implementation of vint64 encoding by Tony Arcieri: https://crates.io/crates/vint64

pub(crate) type IVar = i64;
pub(crate) type UVar = u64;

const MAX_LEN: usize = 9;

const fn zag(x: IVar) -> UVar {
    ((x << 1) ^ (x >> (IVar::BITS - 1))) as UVar
}

const fn zig(x: UVar) -> IVar {
    (x >> 1) as IVar ^ -(x as IVar & 1)
}

fn encoded_len(x: UVar) -> usize {
    match x.leading_zeros() {
        57..65 => 1,
        50..57 => 2,
        43..50 => 3,
        36..43 => 4,
        29..36 => 5,
        22..29 => 6,
        15..22 => 7,
        8..15 => 8,
        0..8 => 9,
        _ => unsafe { unreachable_unchecked() },
    }
}

impl Encode for UVar {
    fn encode(&self, w: &mut impl io::Write) -> EncResult<()> {
        let len = encoded_len(*self);
        if len == 9 {
            let mut res = [0u8; MAX_LEN];
            res[1..].copy_from_slice(&self.to_le_bytes());
            w.write_all(&res)?;
        } else {
            let res = ((*self << 1 | 1) << (len - 1)).to_le_bytes();
            w.write_all(&res[..len])?;
        }
        Ok(())
    }
}

impl Decode for UVar {
    fn decode<R: io::Read + io::Seek>(r: &mut R) -> DecResult<Self> {
        let mut header = 0u8;
        let mut data = [0u8; UVar::BITS as usize / 8];
        r.read_exact(slice::from_mut(&mut header))?;
        let trailing = header.trailing_zeros() as usize;

        if trailing == 8 {
            r.read_exact(&mut data)?;
            Ok(UVar::from_le_bytes(data))
        } else if trailing == 0 {
            Ok(header as UVar >> 1)
        } else {
            data[0] = header;
            r.read_exact(&mut data[1..trailing + 1])?;
            Ok(UVar::from_le_bytes(data) >> (1 + trailing))
        }
    }
}

impl UnsafeDecode for UVar {
    unsafe fn decode(r: &mut NonNull<u8>) -> Self {
        unsafe {
            let header = *r.as_ptr();
            *r = r.add(1);
            let mut data = [0u8; UVar::BITS as usize / 8];
            let trailing = header.trailing_zeros() as usize;
            if trailing == 8 {
                ptr::copy_nonoverlapping(r.as_ptr(), data.as_mut_ptr(), size_of_val(&data));
                *r = r.add(size_of_val(&data));
                UVar::from_le_bytes(data)
            } else {
                data[0] = header;
                ptr::copy_nonoverlapping(r.as_ptr(), data[1..].as_mut_ptr(), trailing);
                *r = r.add(trailing);
                UVar::from_le_bytes(data) >> (1 + trailing)
            }
        }
    }
}

impl Encode for IVar {
    fn encode(&self, w: &mut impl io::Write) -> EncResult<()> {
        zag(*self).encode(w)
    }
}

impl Decode for IVar {
    fn decode<R: io::Read + io::Seek>(r: &mut R) -> DecResult<Self> {
        Decode::decode(r).map(zig)
    }
}

impl UnsafeDecode for IVar {
    unsafe fn decode(r: &mut NonNull<u8>) -> Self {
        zig(unsafe { UnsafeDecode::decode(r) })
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn push_unique<T: PartialEq>(items: &mut Vec<T>, value: T) {
        if !items.contains(&value) {
            items.push(value);
        }
    }

    fn len_bounds(len: usize) -> (UVar, UVar) {
        match len {
            1 => (0, (1 << 7) - 1),
            2 => (1 << 7, (1 << 14) - 1),
            3 => (1 << 14, (1 << 21) - 1),
            4 => (1 << 21, (1 << 28) - 1),
            5 => (1 << 28, (1 << 35) - 1),
            6 => (1 << 35, (1 << 42) - 1),
            7 => (1 << 42, (1 << 49) - 1),
            8 => (1 << 49, (1 << 56) - 1),
            9 => (1 << 56, UVar::MAX),
            _ => panic!("invalid encoding length"),
        }
    }

    fn spread_uvars() -> Vec<UVar> {
        let mut out = Vec::new();

        for len in 1..=MAX_LEN {
            let (lo, hi) = len_bounds(len);
            for value in [
                lo,
                lo.saturating_add(1),
                lo.saturating_add((hi - lo) / 2),
                hi.saturating_sub(1),
                hi,
            ] {
                push_unique(&mut out, value);
            }
        }

        for shift in (0..UVar::BITS).step_by(3) {
            if let Some(value) = 1u64.checked_shl(shift) {
                push_unique(&mut out, value);
                push_unique(&mut out, value.saturating_sub(1));
            }
            push_unique(&mut out, UVar::MAX >> shift);
        }

        out
    }

    fn spread_ivars() -> Vec<IVar> {
        let mut out = vec![IVar::MIN, IVar::MIN + 1, -1, 0, 1, IVar::MAX - 1, IVar::MAX];

        for value in spread_uvars() {
            push_unique(&mut out, zig(value));
        }

        out
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn zigzag() {
        for value in spread_ivars() {
            assert_eq!(value, zig(zag(value)));
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn encode_decode_uvar() {
        for value in spread_uvars() {
            let mut buf = Vec::new();
            value.encode(&mut buf).unwrap();
            assert_eq!(buf.len(), encoded_len(value));
            assert_eq!(
                value,
                Decode::decode(&mut io::Cursor::new(&buf[..])).unwrap()
            );
            assert_eq!(value, unsafe {
                UnsafeDecode::decode(&mut NonNull::from_ref(&buf[0]))
            });
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn encode_decode_ivar() {
        for value in spread_ivars() {
            let mut buf = Vec::new();
            value.encode(&mut buf).unwrap();
            assert_eq!(buf.len(), encoded_len(zag(value)));
            assert_eq!(
                value,
                Decode::decode(&mut io::Cursor::new(&buf[..])).unwrap()
            );
            assert_eq!(value, unsafe {
                UnsafeDecode::decode(&mut NonNull::from_ref(&buf[0]))
            });
        }
    }
}
