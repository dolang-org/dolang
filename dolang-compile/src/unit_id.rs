use std::num::NonZeroU32;

/// Identifies a unit among those checked together.
///
/// Obtained from [`typeck::Builder::unit`](crate::typeck::Builder::unit); an ID
/// is meaningful only for the check that assigned it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UnitId(NonZeroU32);

impl UnitId {
    pub(crate) fn from_index(index: usize) -> Self {
        u32::try_from(index + 1)
            .ok()
            .and_then(NonZeroU32::new)
            .map(Self)
            .expect("too many units")
    }

    /// Zero-based position among the checked units, in the order they were added.
    pub fn index(self) -> usize {
        self.0.get() as usize - 1
    }
}
