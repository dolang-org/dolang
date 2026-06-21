use crate::{
    error::{Error, Result},
    strand::Strand,
    sym,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BinaryIntFormat {
    U8,
    I8,
    U16Le,
    U16Be,
    I16Le,
    I16Be,
    U32Le,
    U32Be,
    I32Le,
    I32Be,
    U64Le,
    U64Be,
    I64Le,
    I64Be,
}

#[allow(dead_code)]
pub(crate) const METHOD_TAGS: [sym::Tag; 14] = [
    sym::FROM_U8,
    sym::FROM_I8,
    sym::FROM_U16_LE,
    sym::FROM_U16_BE,
    sym::FROM_I16_LE,
    sym::FROM_I16_BE,
    sym::FROM_U32_LE,
    sym::FROM_U32_BE,
    sym::FROM_I32_LE,
    sym::FROM_I32_BE,
    sym::FROM_U64_LE,
    sym::FROM_U64_BE,
    sym::FROM_I64_LE,
    sym::FROM_I64_BE,
];

pub(crate) fn format_for_method(tag: sym::Tag) -> Option<BinaryIntFormat> {
    Some(match tag {
        sym::FROM_U8 => BinaryIntFormat::U8,
        sym::FROM_I8 => BinaryIntFormat::I8,
        sym::FROM_U16_LE => BinaryIntFormat::U16Le,
        sym::FROM_U16_BE => BinaryIntFormat::U16Be,
        sym::FROM_I16_LE => BinaryIntFormat::I16Le,
        sym::FROM_I16_BE => BinaryIntFormat::I16Be,
        sym::FROM_U32_LE => BinaryIntFormat::U32Le,
        sym::FROM_U32_BE => BinaryIntFormat::U32Be,
        sym::FROM_I32_LE => BinaryIntFormat::I32Le,
        sym::FROM_I32_BE => BinaryIntFormat::I32Be,
        sym::FROM_U64_LE => BinaryIntFormat::U64Le,
        sym::FROM_U64_BE => BinaryIntFormat::U64Be,
        sym::FROM_I64_LE => BinaryIntFormat::I64Le,
        sym::FROM_I64_BE => BinaryIntFormat::I64Be,
        _ => return None,
    })
}

fn expected_len(format: BinaryIntFormat) -> usize {
    match format {
        BinaryIntFormat::U8 | BinaryIntFormat::I8 => 1,
        BinaryIntFormat::U16Le
        | BinaryIntFormat::U16Be
        | BinaryIntFormat::I16Le
        | BinaryIntFormat::I16Be => 2,
        BinaryIntFormat::U32Le
        | BinaryIntFormat::U32Be
        | BinaryIntFormat::I32Le
        | BinaryIntFormat::I32Be => 4,
        BinaryIntFormat::U64Le
        | BinaryIntFormat::U64Be
        | BinaryIntFormat::I64Le
        | BinaryIntFormat::I64Be => 8,
    }
}

pub(crate) fn decode<'v, 's>(
    strand: &mut Strand<'v, 's>,
    bytes: &[u8],
    format: BinaryIntFormat,
) -> Result<'v, 's, i128> {
    let len = expected_len(format);
    if bytes.len() != len {
        return Err(Error::type_error(
            strand,
            format!("expected `bin` of length {len}"),
        ));
    }
    Ok(match format {
        BinaryIntFormat::U8 => i128::from(bytes[0]),
        BinaryIntFormat::I8 => i128::from(i8::from_ne_bytes([bytes[0]])),
        BinaryIntFormat::U16Le => i128::from(u16::from_le_bytes(bytes.try_into().unwrap())),
        BinaryIntFormat::U16Be => i128::from(u16::from_be_bytes(bytes.try_into().unwrap())),
        BinaryIntFormat::I16Le => i128::from(i16::from_le_bytes(bytes.try_into().unwrap())),
        BinaryIntFormat::I16Be => i128::from(i16::from_be_bytes(bytes.try_into().unwrap())),
        BinaryIntFormat::U32Le => i128::from(u32::from_le_bytes(bytes.try_into().unwrap())),
        BinaryIntFormat::U32Be => i128::from(u32::from_be_bytes(bytes.try_into().unwrap())),
        BinaryIntFormat::I32Le => i128::from(i32::from_le_bytes(bytes.try_into().unwrap())),
        BinaryIntFormat::I32Be => i128::from(i32::from_be_bytes(bytes.try_into().unwrap())),
        BinaryIntFormat::U64Le => i128::from(u64::from_le_bytes(bytes.try_into().unwrap())),
        BinaryIntFormat::U64Be => i128::from(u64::from_be_bytes(bytes.try_into().unwrap())),
        BinaryIntFormat::I64Le => i128::from(i64::from_le_bytes(bytes.try_into().unwrap())),
        BinaryIntFormat::I64Be => i128::from(i64::from_be_bytes(bytes.try_into().unwrap())),
    })
}

pub(crate) fn encode<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: i128,
    format: BinaryIntFormat,
) -> Result<'v, 's, Vec<u8>> {
    Ok(match format {
        BinaryIntFormat::U8 => [u8::try_from(value).map_err(|_| Error::overflow(strand))?].to_vec(),
        BinaryIntFormat::I8 => i8::try_from(value)
            .map_err(|_| Error::overflow(strand))?
            .to_ne_bytes()
            .to_vec(),
        BinaryIntFormat::U16Le => u16::try_from(value)
            .map_err(|_| Error::overflow(strand))?
            .to_le_bytes()
            .to_vec(),
        BinaryIntFormat::U16Be => u16::try_from(value)
            .map_err(|_| Error::overflow(strand))?
            .to_be_bytes()
            .to_vec(),
        BinaryIntFormat::I16Le => i16::try_from(value)
            .map_err(|_| Error::overflow(strand))?
            .to_le_bytes()
            .to_vec(),
        BinaryIntFormat::I16Be => i16::try_from(value)
            .map_err(|_| Error::overflow(strand))?
            .to_be_bytes()
            .to_vec(),
        BinaryIntFormat::U32Le => u32::try_from(value)
            .map_err(|_| Error::overflow(strand))?
            .to_le_bytes()
            .to_vec(),
        BinaryIntFormat::U32Be => u32::try_from(value)
            .map_err(|_| Error::overflow(strand))?
            .to_be_bytes()
            .to_vec(),
        BinaryIntFormat::I32Le => i32::try_from(value)
            .map_err(|_| Error::overflow(strand))?
            .to_le_bytes()
            .to_vec(),
        BinaryIntFormat::I32Be => i32::try_from(value)
            .map_err(|_| Error::overflow(strand))?
            .to_be_bytes()
            .to_vec(),
        BinaryIntFormat::U64Le => u64::try_from(value)
            .map_err(|_| Error::overflow(strand))?
            .to_le_bytes()
            .to_vec(),
        BinaryIntFormat::U64Be => u64::try_from(value)
            .map_err(|_| Error::overflow(strand))?
            .to_be_bytes()
            .to_vec(),
        BinaryIntFormat::I64Le => i64::try_from(value)
            .map_err(|_| Error::overflow(strand))?
            .to_le_bytes()
            .to_vec(),
        BinaryIntFormat::I64Be => i64::try_from(value)
            .map_err(|_| Error::overflow(strand))?
            .to_be_bytes()
            .to_vec(),
    })
}
