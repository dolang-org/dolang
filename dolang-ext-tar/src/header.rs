//! Member header encoding: a ustar header block, preceded by a local pax extended
//! header when a value does not fit its ustar field.

use tar_framing::{
    BLOCK_SIZE, PaxKeyword, UstarKind,
    header::{
        CHECKSUM_RANGE, DEVMAJOR_RANGE, DEVMINOR_RANGE, GID_RANGE, GNAME_RANGE, IDENTITY_RANGE,
        LINK_NAME_RANGE, MODE_RANGE, MTIME_RANGE, NAME_RANGE, PREFIX_RANGE, SIZE_RANGE,
        TYPEFLAG_OFFSET, UID_RANGE, UNAME_RANGE, USTAR_IDENTITY,
    },
    write::{FramingWriteError, append_pax_record, payload_padding},
};

/// The metadata of one member to encode.
pub(crate) struct HeaderSpec<'a> {
    pub(crate) path: &'a str,
    pub(crate) kind: UstarKind,
    pub(crate) size: u64,
    pub(crate) mode: u32,
    pub(crate) uid: u64,
    pub(crate) gid: u64,
    pub(crate) mtime: u64,
    pub(crate) uname: Option<&'a str>,
    pub(crate) gname: Option<&'a str>,
    pub(crate) link: Option<&'a str>,
}

/// A reason a member header cannot be encoded.
#[derive(Debug)]
pub(crate) enum HeaderError {
    /// A string value contains a NUL byte.
    Nul(&'static str),
    /// The pax extended header could not be framed.
    Framing(FramingWriteError),
}

impl std::fmt::Display for HeaderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Nul(field) => write!(f, "tar entry {field} must not contain NUL"),
            Self::Framing(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for HeaderError {}

impl From<FramingWriteError> for HeaderError {
    fn from(err: FramingWriteError) -> Self {
        Self::Framing(err)
    }
}

type Block = [u8; BLOCK_SIZE];

/// Encodes the header of `spec` into `buf`, replacing its contents. `sequence`
/// numbers the pax extended header's name when one is needed.
///
/// The caller writes the payload and its [`payload_padding`] after `buf`.
pub(crate) fn encode(
    spec: &HeaderSpec<'_>,
    sequence: u64,
    buf: &mut Vec<u8>,
) -> Result<(), HeaderError> {
    for (field, value) in [
        ("path", Some(spec.path)),
        ("owner_name", spec.uname),
        ("group_name", spec.gname),
        ("link target", spec.link),
    ] {
        if value.is_some_and(|value| value.contains('\0')) {
            return Err(HeaderError::Nul(field));
        }
    }

    // Collect the pax records for values the ustar fields cannot hold.
    let mut records = Vec::new();
    let split = split_path(spec.path.as_bytes());
    if split.is_none() {
        append_pax_record(&mut records, &PaxKeyword::Path, spec.path.as_bytes())?;
    }
    let link = spec.link.unwrap_or_default().as_bytes();
    if link.len() > LINK_NAME_RANGE.len() {
        append_pax_record(&mut records, &PaxKeyword::LinkPath, link)?;
    }
    for (keyword, value, field) in [
        (PaxKeyword::Uname, spec.uname, UNAME_RANGE),
        (PaxKeyword::Gname, spec.gname, GNAME_RANGE),
    ] {
        let value = value.unwrap_or_default().as_bytes();
        if value.len() > field.len() {
            append_pax_record(&mut records, &keyword, value)?;
        }
    }
    for (keyword, value, field) in [
        (PaxKeyword::Uid, spec.uid, UID_RANGE),
        (PaxKeyword::Gid, spec.gid, GID_RANGE),
        (PaxKeyword::Size, spec.size, SIZE_RANGE),
        (PaxKeyword::Mtime, spec.mtime, MTIME_RANGE),
    ] {
        if !fits_octal(field.len(), value) {
            append_pax_record(&mut records, &keyword, value.to_string().as_bytes())?;
        }
    }

    buf.clear();
    if !records.is_empty() {
        let mut header = [0; BLOCK_SIZE];
        let name = format!("PaxHeaders/{sequence}");
        header[NAME_RANGE][..name.len()].copy_from_slice(name.as_bytes());
        let size = records.len() as u64;
        fill_numbers(&mut header, 0o644, 0, 0, size, 0);
        finish_block(&mut header, b'x');
        buf.extend_from_slice(&header);
        buf.extend_from_slice(&records);
        buf.extend_from_slice(payload_padding(size));
    }

    let mut header = [0; BLOCK_SIZE];
    match split {
        Some((prefix, name)) => {
            header[PREFIX_RANGE][..prefix.len()].copy_from_slice(prefix);
            header[NAME_RANGE][..name.len()].copy_from_slice(name);
        }
        None => {
            // The pax `path` record holds the name; keep as much of it as fits.
            let path = spec.path.as_bytes();
            let tail = &path[path.len().saturating_sub(NAME_RANGE.len())..];
            header[NAME_RANGE][..tail.len()].copy_from_slice(tail);
        }
    }
    let link = &link[..link.len().min(LINK_NAME_RANGE.len())];
    header[LINK_NAME_RANGE][..link.len()].copy_from_slice(link);
    for (value, field) in [(spec.uname, UNAME_RANGE), (spec.gname, GNAME_RANGE)] {
        let value = value.unwrap_or_default().as_bytes();
        if value.len() <= field.len() {
            header[field][..value.len()].copy_from_slice(value);
        }
    }
    fill_numbers(
        &mut header,
        u64::from(spec.mode),
        spec.uid,
        spec.gid,
        spec.size,
        spec.mtime,
    );
    finish_block(&mut header, typeflag(spec.kind));
    buf.extend_from_slice(&header);
    Ok(())
}

fn typeflag(kind: UstarKind) -> u8 {
    match kind {
        UstarKind::Regular => b'0',
        UstarKind::HardLink => b'1',
        UstarKind::SymbolicLink => b'2',
        UstarKind::CharacterDevice => b'3',
        UstarKind::BlockDevice => b'4',
        UstarKind::Directory => b'5',
        UstarKind::Fifo => b'6',
        UstarKind::Contiguous => b'7',
    }
}

/// Fills the numeric fields, writing 0 for any value a pax record carries instead.
fn fill_numbers(header: &mut Block, mode: u64, uid: u64, gid: u64, size: u64, mtime: u64) {
    for (value, field) in [
        (mode & 0o7777, MODE_RANGE),
        (uid, UID_RANGE),
        (gid, GID_RANGE),
        (size, SIZE_RANGE),
        (mtime, MTIME_RANGE),
        (0, DEVMAJOR_RANGE),
        (0, DEVMINOR_RANGE),
    ] {
        let value = if fits_octal(field.len(), value) {
            value
        } else {
            0
        };
        encode_octal(&mut header[field], value);
    }
}

/// Sets the typeflag and identity, then the checksum over the finished block.
fn finish_block(header: &mut Block, typeflag: u8) {
    header[TYPEFLAG_OFFSET] = typeflag;
    header[IDENTITY_RANGE].copy_from_slice(USTAR_IDENTITY);
    header[CHECKSUM_RANGE].fill(b' ');
    let sum = header.iter().map(|&byte| u64::from(byte)).sum::<u64>();
    // At most 512 * 255, which fits in six octal digits; NUL and space follow.
    encode_octal(
        &mut header[CHECKSUM_RANGE.start..CHECKSUM_RANGE.end - 1],
        sum,
    );
}

/// Whether `value` fits in a NUL-terminated octal field of `len` bytes.
fn fits_octal(len: usize, value: u64) -> bool {
    let digits = (len - 1) as u32;
    digits >= 22 || value < 1 << (3 * digits)
}

/// Writes `value` as zero-padded octal digits followed by a NUL. The value must fit.
fn encode_octal(field: &mut [u8], value: u64) {
    let (terminator, digits) = field.split_last_mut().unwrap();
    *terminator = 0;
    let mut value = value;
    for digit in digits.iter_mut().rev() {
        *digit = b'0' + (value & 7) as u8;
        value >>= 3;
    }
    debug_assert_eq!(value, 0);
}

/// Splits `path` into ustar prefix and name fields, or `None` when it fits neither way.
fn split_path(path: &[u8]) -> Option<(&[u8], &[u8])> {
    if path.len() <= NAME_RANGE.len() {
        return Some((&[], path));
    }
    // Try separators from the right: earlier ones leave a longer name.
    (0..path.len())
        .rev()
        .filter(|&i| path[i] == b'/')
        .map(|separator| (&path[..separator], &path[separator + 1..]))
        .find(|(prefix, name)| {
            !prefix.is_empty()
                && prefix.len() <= PREFIX_RANGE.len()
                && !name.is_empty()
                && name.len() <= NAME_RANGE.len()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(path: &str) -> HeaderSpec<'_> {
        HeaderSpec {
            path,
            kind: UstarKind::Regular,
            size: 0,
            mode: 0o644,
            uid: 0,
            gid: 0,
            mtime: 0,
            uname: None,
            gname: None,
            link: None,
        }
    }

    fn encoded(spec: &HeaderSpec<'_>) -> Vec<u8> {
        let mut buf = Vec::new();
        encode(spec, 0, &mut buf).unwrap();
        buf
    }

    #[test]
    fn octal_fields_hold_up_to_their_digits() {
        assert!(fits_octal(8, 0o7777777));
        assert!(!fits_octal(8, 0o10000000));
        assert!(fits_octal(12, 0o77777777777));
        assert!(!fits_octal(12, 0o100000000000));
        let mut field = [0xff; 8];
        encode_octal(&mut field, 0o755);
        assert_eq!(&field, b"0000755\0");
    }

    #[test]
    fn paths_split_into_prefix_and_name() {
        let name = "n".repeat(100);
        assert_eq!(
            split_path(name.as_bytes()),
            Some((&b""[..], name.as_bytes()))
        );
        let path = format!("{}/{}", "p".repeat(155), "n".repeat(100));
        let (prefix, name) = split_path(path.as_bytes()).unwrap();
        assert_eq!((prefix.len(), name.len()), (155, 100));
        assert_eq!(
            split_path(format!("{}/x", "p".repeat(156)).as_bytes()),
            None
        );
        assert_eq!(split_path("n".repeat(101).as_bytes()), None);
    }

    #[test]
    fn pax_header_only_when_a_value_does_not_fit() {
        let owner = "o".repeat(32);
        assert_eq!(
            encoded(&HeaderSpec {
                uname: Some(&owner),
                uid: 0o7777777,
                ..spec(&"n".repeat(100))
            })
            .len(),
            BLOCK_SIZE
        );

        let long_owner = "o".repeat(33);
        let long_path = "n".repeat(101);
        for spec in [
            spec(&long_path),
            HeaderSpec {
                uname: Some(&long_owner),
                ..spec("a")
            },
            HeaderSpec {
                uid: 0o10000000,
                ..spec("a")
            },
        ] {
            let buf = encoded(&spec);
            assert_eq!(buf[TYPEFLAG_OFFSET], b'x');
            assert_eq!(buf.len() % BLOCK_SIZE, 0);
            assert_eq!(buf[buf.len() - BLOCK_SIZE + TYPEFLAG_OFFSET], b'0');
        }
    }

    #[test]
    fn nul_is_rejected() {
        let mut buf = Vec::new();
        assert!(matches!(
            encode(&spec("a\0b"), 0, &mut buf),
            Err(HeaderError::Nul("path"))
        ));
    }
}
