use std::ops::Deref;

#[repr(transparent)]
pub struct Verified<T>(T);

impl<T> Verified<T> {
    /// # Safety
    /// The caller must verify that T has been verified/sanitized
    pub unsafe fn new(value: T) -> Self {
        Verified(value)
    }

    /// # Safety
    /// The caller must ensure that the provided closure projects part
    /// of the provided inner value, and that the signaled verification
    /// property holds for the part if it holds for the whole.
    pub unsafe fn project<U>(this: &Self, f: impl for<'a> FnOnce(&'a T) -> &'a U) -> Verified<&U> {
        unsafe { Verified::new(f(&this.0)) }
    }

    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> AsRef<T> for Verified<T> {
    fn as_ref(&self) -> &T {
        &self.0
    }
}

impl<T> Deref for Verified<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
