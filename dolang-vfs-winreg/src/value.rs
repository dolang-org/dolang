//! Typed registry values.

use serde::{Deserialize, Serialize};

/// A typed registry value.
///
/// [`Value::Other`] round-trips any `REG_*` kind this crate doesn't give a
/// dedicated variant to (e.g. `REG_LINK`, `REG_RESOURCE_LIST`) losslessly,
/// so structured `get`/type queries never lose information.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Value {
    Sz(String),
    ExpandSz(String),
    MultiSz(Vec<String>),
    Dword(u32),
    DwordBigEndian(u32),
    Qword(u64),
    Binary(Vec<u8>),
    None,
    Other { kind: u32, data: Vec<u8> },
}

#[cfg(windows)]
mod raw {
    use std::{
        ffi::OsString,
        os::windows::ffi::{OsStrExt, OsStringExt},
    };

    use windows_sys::Win32::System::Registry::{
        REG_BINARY, REG_DWORD, REG_DWORD_BIG_ENDIAN, REG_EXPAND_SZ, REG_MULTI_SZ, REG_NONE,
        REG_QWORD, REG_SZ,
    };

    use super::Value;

    /// Splits a `REG_MULTI_SZ` byte buffer (a run of NUL-terminated UTF-16
    /// strings, itself terminated by an empty string) into its parts.
    fn split_multi_sz(units: &[u16]) -> Vec<String> {
        units
            .split(|unit| *unit == 0)
            .filter(|part| !part.is_empty())
            .map(|part| OsString::from_wide(part).to_string_lossy().into_owned())
            .collect()
    }

    fn wide_nul(value: &str) -> Vec<u16> {
        std::ffi::OsStr::new(value)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn units_to_bytes(units: &[u16]) -> Vec<u8> {
        units.iter().flat_map(|unit| unit.to_le_bytes()).collect()
    }

    fn bytes_to_units(data: &[u8]) -> Vec<u16> {
        data.chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect()
    }

    fn units_to_string(units: &[u16]) -> String {
        let end = units
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(units.len());
        OsString::from_wide(&units[..end])
            .to_string_lossy()
            .into_owned()
    }

    impl Value {
        /// Converts a raw `(kind, data)` pair from `RegQueryValueExW`/
        /// `RegEnumValueW` into a typed [`Value`].
        pub(crate) fn from_raw(kind: u32, data: &[u8]) -> Value {
            match kind {
                REG_SZ => Value::Sz(units_to_string(&bytes_to_units(data))),
                REG_EXPAND_SZ => Value::ExpandSz(units_to_string(&bytes_to_units(data))),
                REG_MULTI_SZ => Value::MultiSz(split_multi_sz(&bytes_to_units(data))),
                REG_DWORD => Value::Dword(u32::from_le_bytes(
                    data.get(..4).unwrap_or(&[0; 4]).try_into().unwrap(),
                )),
                REG_DWORD_BIG_ENDIAN => Value::DwordBigEndian(u32::from_be_bytes(
                    data.get(..4).unwrap_or(&[0; 4]).try_into().unwrap(),
                )),
                REG_QWORD => Value::Qword(u64::from_le_bytes(
                    data.get(..8).unwrap_or(&[0; 8]).try_into().unwrap(),
                )),
                REG_BINARY => Value::Binary(data.to_vec()),
                REG_NONE => Value::None,
                other => Value::Other {
                    kind: other,
                    data: data.to_vec(),
                },
            }
        }

        /// Converts a typed [`Value`] into the raw `(kind, data)` pair
        /// `RegSetValueExW` expects.
        pub(crate) fn to_raw(&self) -> (u32, Vec<u8>) {
            match self {
                Value::Sz(s) => (REG_SZ, units_to_bytes(&wide_nul(s))),
                Value::ExpandSz(s) => (REG_EXPAND_SZ, units_to_bytes(&wide_nul(s))),
                Value::MultiSz(parts) => {
                    let mut units = Vec::new();
                    for part in parts {
                        units.extend(wide_nul(part).into_iter().filter(|&u| u != 0));
                        units.push(0);
                    }
                    units.push(0);
                    (REG_MULTI_SZ, units_to_bytes(&units))
                }
                Value::Dword(v) => (REG_DWORD, v.to_le_bytes().to_vec()),
                Value::DwordBigEndian(v) => (REG_DWORD_BIG_ENDIAN, v.to_be_bytes().to_vec()),
                Value::Qword(v) => (REG_QWORD, v.to_le_bytes().to_vec()),
                Value::Binary(data) => (REG_BINARY, data.clone()),
                Value::None => (REG_NONE, Vec::new()),
                Value::Other { kind, data } => (*kind, data.clone()),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::super::Value;

        #[test]
        fn round_trips_through_raw_kind_and_bytes() {
            let values = [
                Value::Sz("hello".into()),
                Value::ExpandSz("%PATH%".into()),
                Value::MultiSz(vec!["a".into(), "bee".into()]),
                Value::Dword(0x1234_5678),
                Value::DwordBigEndian(0x1234_5678),
                Value::Qword(u64::MAX),
                Value::Binary(vec![1, 2, 3]),
                Value::None,
                Value::Other {
                    kind: 99,
                    data: vec![9, 9, 9],
                },
            ];
            for value in values {
                let (kind, data) = value.to_raw();
                assert_eq!(Value::from_raw(kind, &data), value);
            }
        }
    }
}
