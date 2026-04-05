#[inline]
fn relative(len: usize, index: i64) -> Option<usize> {
    if index >= 0 {
        usize::try_from(index).ok()
    } else {
        let len = i64::try_from(len).ok()?;
        usize::try_from(len.checked_add(index)?).ok()
    }
}

#[inline]
pub(crate) fn element(len: usize, index: i64) -> Option<usize> {
    let index = relative(len, index)?;
    (index < len).then_some(index)
}

#[inline]
pub(crate) fn position(len: usize, index: i64) -> Option<usize> {
    let index = relative(len, index)?;
    (index <= len).then_some(index)
}
