//! Win32 error code name/value lookup, generated from MS-ERREF.

macro_rules! error_codes {
    (
        $by_code:ident,
        $by_name:ident,
        $ty:ty,
        { $($value:literal => $name:literal,)* },
        { $($alias:literal => $alias_value:literal,)* }
    ) => {
        pub(crate) static $by_code: phf::Map<$ty, &'static str> = phf::phf_map! {
            $($value => $name,)*
        };
        pub(crate) static $by_name: phf::Map<&'static str, $ty> = phf::phf_map! {
            $($name => $value,)*
            $($alias => $alias_value,)*
        };
    };
}

mod generated;

/// Looks up the symbolic name of a Win32 error code (e.g.
/// `ERROR_FILE_NOT_FOUND` for `2`).
pub fn win_error_name(code: u32) -> Option<&'static str> {
    generated::WIN_ERROR_BY_CODE.get(&code).copied()
}

/// Looks up the numeric value of a Win32 error code by its symbolic name
/// (e.g. `2` for `ERROR_FILE_NOT_FOUND`), including known aliases.
pub fn win_error_code(name: &str) -> Option<u32> {
    generated::WIN_ERROR_BY_NAME.get(name).copied()
}

#[cfg(test)]
mod tests {
    use super::{win_error_code, win_error_name};

    #[test]
    fn known_codes_round_trip() {
        assert_eq!(win_error_name(2), Some("ERROR_FILE_NOT_FOUND"));
        assert_eq!(win_error_code("ERROR_FILE_NOT_FOUND"), Some(2));
    }

    #[test]
    fn unknown_code_returns_none() {
        assert_eq!(win_error_name(u32::MAX), None);
        assert_eq!(win_error_code("NOT_A_REAL_ERROR_CODE"), None);
    }
}
