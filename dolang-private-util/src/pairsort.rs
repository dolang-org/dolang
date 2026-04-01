// Based on tiny-sort-rs by Lukas Bergdoll

use std::{marker::PhantomData, mem::MaybeUninit, ptr};

/// Stable sort two equal-length slices in lockstep, ordering by values in `left`.
///
/// `is_less(a, b)` must return `Ok(true)` when `a < b`.
///
/// # Panics
///
/// Panics if `left.len() != right.len()`.
pub fn sort_by<T, U, E, F>(left: &mut [T], right: &mut [U], mut is_less: F) -> Result<(), E>
where
    F: FnMut(&T, &T) -> Result<bool, E>,
{
    assert_eq!(left.len(), right.len(), "pairsort length mismatch");

    if left.len() < 2 {
        return Ok(());
    }

    unsafe { mergesort_main(SlicePair::from_slices(left, right), &mut is_less) }
}

struct SlicePair<'a, T, U> {
    left: *mut T,
    right: *mut U,
    len: usize,
    phantom: PhantomData<(&'a mut T, &'a mut U)>,
}

impl<'a, T, U> Copy for SlicePair<'a, T, U> {}

impl<'a, T, U> Clone for SlicePair<'a, T, U> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'a, T, U> SlicePair<'a, T, U> {
    /// # Safety
    ///
    /// `left` and `right` must each be valid for `len` elements.
    unsafe fn new(left: *mut T, right: *mut U, len: usize) -> Self {
        Self {
            left,
            right,
            len,
            phantom: PhantomData,
        }
    }

    fn from_slices(left: &'a mut [T], right: &'a mut [U]) -> Self {
        debug_assert_eq!(left.len(), right.len());
        // SAFETY: slices are valid for their full length.
        unsafe { Self::new(left.as_mut_ptr(), right.as_mut_ptr(), left.len()) }
    }

    fn len(self) -> usize {
        self.len
    }

    /// # Safety
    ///
    /// `mid` must be in bounds for this slice pair.
    unsafe fn split_at(self, mid: usize) -> (Self, Self) {
        debug_assert!(mid <= self.len);
        unsafe {
            (
                Self::new(self.left, self.right, mid),
                Self::new(self.left.add(mid), self.right.add(mid), self.len - mid),
            )
        }
    }

    /// # Safety
    ///
    /// `index` must be in bounds for this slice pair.
    unsafe fn left_at(self, index: usize) -> *mut T {
        debug_assert!(index < self.len);
        unsafe { self.left.add(index) }
    }

    /// # Safety
    ///
    /// `index` must be in bounds for this slice pair.
    unsafe fn right_at(self, index: usize) -> *mut U {
        debug_assert!(index < self.len);
        unsafe { self.right.add(index) }
    }

    /// # Safety
    ///
    /// `i` and `j` must both be in bounds for this slice pair.
    unsafe fn swap(self, i: usize, j: usize) {
        debug_assert!(i < self.len);
        debug_assert!(j < self.len);
        unsafe {
            ptr::swap(self.left_at(i), self.left_at(j));
            ptr::swap(self.right_at(i), self.right_at(j));
        }
    }

    /// # Safety
    ///
    /// `src_idx` must be in bounds for `self`; `dst_idx` must be in bounds for `dst`.
    unsafe fn copy_elem_to(self, dst: Self, src_idx: usize, dst_idx: usize) {
        debug_assert!(src_idx < self.len);
        debug_assert!(dst_idx < dst.len);
        unsafe {
            ptr::copy_nonoverlapping(self.left_at(src_idx), dst.left_at(dst_idx), 1);
            ptr::copy_nonoverlapping(self.right_at(src_idx), dst.right_at(dst_idx), 1);
        }
    }

    /// # Safety
    ///
    /// `src_start..src_start + count` must be in bounds for `self` and
    /// `dst_start..dst_start + count` must be in bounds for `dst`.
    unsafe fn copy_range_to(self, dst: Self, src_start: usize, count: usize, dst_start: usize) {
        debug_assert!(src_start <= self.len);
        debug_assert!(count <= self.len - src_start);
        debug_assert!(dst_start <= dst.len);
        debug_assert!(count <= dst.len - dst_start);
        unsafe {
            ptr::copy_nonoverlapping(self.left.add(src_start), dst.left.add(dst_start), count);
            ptr::copy_nonoverlapping(self.right.add(src_start), dst.right.add(dst_start), count);
        }
    }

    /// # Safety
    ///
    /// `src` and `dst` must have the same length.
    unsafe fn copy_back_to(self, dst: Self) {
        debug_assert_eq!(self.len, dst.len);
        unsafe { self.copy_range_to(dst, 0, self.len, 0) }
    }
}

struct Scratch<T, U> {
    left: Vec<MaybeUninit<T>>,
    right: Vec<MaybeUninit<U>>,
}

impl<T, U> Scratch<T, U> {
    fn new(len: usize) -> Self {
        Self {
            left: (0..len).map(|_| MaybeUninit::uninit()).collect(),
            right: (0..len).map(|_| MaybeUninit::uninit()).collect(),
        }
    }

    fn pair(&mut self) -> SlicePair<'_, T, U> {
        // SAFETY: vectors have length `len` after initialization above, and
        // `SlicePair` imposes no initialization invariants on the pointed-to storage.
        unsafe {
            SlicePair::new(
                self.left.as_mut_ptr().cast::<T>(),
                self.right.as_mut_ptr().cast::<U>(),
                self.left.len(),
            )
        }
    }
}

unsafe fn mergesort_main<T, U, E, F>(v: SlicePair<T, U>, is_less: &mut F) -> Result<(), E>
where
    F: FnMut(&T, &T) -> Result<bool, E>,
{
    let mut scratch = Scratch::new(v.len());
    unsafe { mergesort_core(v, scratch.pair(), is_less) }
}

unsafe fn mergesort_core<T, U, E, F>(
    v: SlicePair<T, U>,
    scratch: SlicePair<T, U>,
    is_less: &mut F,
) -> Result<(), E>
where
    F: FnMut(&T, &T) -> Result<bool, E>,
{
    let len = v.len();
    unsafe {
        if len > 2 {
            let mid = len / 2;
            let (left, right) = v.split_at(mid);
            let (left_scratch, right_scratch) = scratch.split_at(mid);
            mergesort_core(left, left_scratch, is_less)?;
            mergesort_core(right, right_scratch, is_less)?;
            merge(v, scratch, is_less, mid)?;
        } else if len == 2 && is_less(&*v.left_at(1), &*v.left_at(0))? {
            v.swap(0, 1);
        }
    }

    Ok(())
}

unsafe fn merge<T, U, E, F>(
    v: SlicePair<T, U>,
    scratch: SlicePair<T, U>,
    is_less: &mut F,
    mid: usize,
) -> Result<(), E>
where
    F: FnMut(&T, &T) -> Result<bool, E>,
{
    debug_assert!(mid > 0 && mid < v.len());
    debug_assert_eq!(v.len(), scratch.len());

    let len = v.len();
    let mut l = 0;
    let mut r = mid;
    let mut i = 0;

    while l < mid && r < len {
        let take_left = unsafe { !is_less(&*v.left_at(r), &*v.left_at(l))? };
        if take_left {
            unsafe { v.copy_elem_to(scratch, l, i) };
            l += 1;
        } else {
            unsafe { v.copy_elem_to(scratch, r, i) };
            r += 1;
        }
        i += 1;
    }

    if l < mid {
        unsafe { v.copy_range_to(scratch, l, mid - l, i) };
    } else if r < len {
        unsafe { v.copy_range_to(scratch, r, len - r, i) };
    }

    unsafe { scratch.copy_back_to(v) };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::sort_by;
    use std::cell::Cell;

    #[test]
    fn sort_basic_pairs() {
        let mut left = [3, 1, 2];
        let mut right = ['c', 'a', 'b'];

        sort_by(&mut left, &mut right, |a, b| Ok::<bool, ()>(a < b)).unwrap();

        assert_eq!(left, [1, 2, 3]);
        assert_eq!(right, ['a', 'b', 'c']);
    }

    #[test]
    fn sort_is_stable() {
        let mut left = [2, 1, 2, 1, 2];
        let mut right = ['a', 'b', 'c', 'd', 'e'];

        sort_by(&mut left, &mut right, |a, b| Ok::<bool, ()>(a < b)).unwrap();

        assert_eq!(left, [1, 1, 2, 2, 2]);
        assert_eq!(right, ['b', 'd', 'a', 'c', 'e']);
    }

    #[test]
    fn sort_handles_small_inputs() {
        let mut empty_left: [i32; 0] = [];
        let mut empty_right: [i32; 0] = [];
        sort_by(&mut empty_left, &mut empty_right, |a, b| {
            Ok::<bool, ()>(a < b)
        })
        .unwrap();

        let mut one_left = [42];
        let mut one_right = ['x'];
        sort_by(&mut one_left, &mut one_right, |a, b| Ok::<bool, ()>(a < b)).unwrap();
        assert_eq!(one_left, [42]);
        assert_eq!(one_right, ['x']);

        let mut two_left = [2, 1];
        let mut two_right = ['b', 'a'];
        sort_by(&mut two_left, &mut two_right, |a, b| Ok::<bool, ()>(a < b)).unwrap();
        assert_eq!(two_left, [1, 2]);
        assert_eq!(two_right, ['a', 'b']);
    }

    #[test]
    fn sort_with_zst_right_slice() {
        let mut left = [3, 1, 2, 1];
        let mut right = [(); 4];

        sort_by(&mut left, &mut right, |a, b| Ok::<bool, ()>(a < b)).unwrap();

        assert_eq!(left, [1, 1, 2, 3]);
        assert_eq!(right, [(), (), (), ()]);
    }

    #[test]
    fn sort_handles_sorted_and_reverse_sorted() {
        let mut sorted_left = [1, 2, 3, 4, 5];
        let mut sorted_right = [10, 20, 30, 40, 50];
        sort_by(&mut sorted_left, &mut sorted_right, |a, b| {
            Ok::<bool, ()>(a < b)
        })
        .unwrap();
        assert_eq!(sorted_left, [1, 2, 3, 4, 5]);
        assert_eq!(sorted_right, [10, 20, 30, 40, 50]);

        let mut rev_left = [5, 4, 3, 2, 1];
        let mut rev_right = [50, 40, 30, 20, 10];
        sort_by(&mut rev_left, &mut rev_right, |a, b| Ok::<bool, ()>(a < b)).unwrap();
        assert_eq!(rev_left, [1, 2, 3, 4, 5]);
        assert_eq!(rev_right, [10, 20, 30, 40, 50]);
    }

    #[test]
    fn comparator_error_propagates_and_preserves_pairs() {
        let original = [(3, 'c'), (1, 'a'), (2, 'b'), (4, 'd'), (0, 'z')];
        let mut left = original.map(|(k, _)| k);
        let mut right = original.map(|(_, v)| v);
        let mut calls = 0usize;

        let err = sort_by(&mut left, &mut right, |a, b| {
            calls += 1;
            if calls == 3 { Err("boom") } else { Ok(a < b) }
        })
        .unwrap_err();

        assert_eq!(err, "boom");

        let mut got: Vec<_> = left.into_iter().zip(right).collect();
        let mut want = original.to_vec();
        got.sort_unstable();
        want.sort_unstable();
        assert_eq!(got, want);
    }

    #[test]
    #[should_panic(expected = "pairsort length mismatch")]
    fn mismatched_lengths_panic() {
        let mut left = [1, 2];
        let mut right = [10];
        let _ = sort_by(&mut left, &mut right, |a, b| Ok::<_, ()>(a < b));
    }

    #[derive(Debug)]
    struct DropValue<'a> {
        key: i32,
        id: usize,
        drops: &'a Cell<usize>,
    }

    impl<'a> Drop for DropValue<'a> {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
        }
    }

    #[test]
    fn drop_safety_on_success_and_error() {
        let drops = Cell::new(0);
        {
            let drops = &drops;
            let mut left = [
                DropValue {
                    key: 3,
                    id: 0,
                    drops,
                },
                DropValue {
                    key: 1,
                    id: 1,
                    drops,
                },
                DropValue {
                    key: 2,
                    id: 2,
                    drops,
                },
            ];
            let mut right = [
                DropValue {
                    key: 30,
                    id: 10,
                    drops,
                },
                DropValue {
                    key: 10,
                    id: 11,
                    drops,
                },
                DropValue {
                    key: 20,
                    id: 12,
                    drops,
                },
            ];

            sort_by(&mut left, &mut right, |a, b| Ok::<bool, ()>(a.key < b.key)).unwrap();
            assert_eq!(left.iter().map(|v| v.key).collect::<Vec<_>>(), [1, 2, 3]);
            assert_eq!(
                right.iter().map(|v| v.key).collect::<Vec<_>>(),
                [10, 20, 30]
            );
            assert_eq!(left.iter().map(|v| v.id).collect::<Vec<_>>(), [1, 2, 0]);
            assert_eq!(right.iter().map(|v| v.id).collect::<Vec<_>>(), [11, 12, 10]);
        }
        assert_eq!(drops.get(), 6);

        let drops = Cell::new(0);
        {
            let drops = &drops;
            let original_left = [(3, 0usize), (1, 1), (2, 2), (4, 3)];
            let original_right = [(30, 10usize), (10, 11), (20, 12), (40, 13)];
            let mut left = original_left.map(|(key, id)| DropValue { key, id, drops });
            let mut right = original_right.map(|(key, id)| DropValue { key, id, drops });

            let mut calls = 0usize;
            let _ = sort_by(&mut left, &mut right, |a, b| {
                calls += 1;
                if calls == 2 {
                    Err("boom")
                } else {
                    Ok(a.key < b.key)
                }
            });

            let mut got: Vec<_> = left
                .iter()
                .zip(right.iter())
                .map(|(l, r)| ((l.key, l.id), (r.key, r.id)))
                .collect();
            let mut want: Vec<_> = original_left.into_iter().zip(original_right).collect();
            got.sort_unstable();
            want.sort_unstable();
            assert_eq!(got, want);
        }
        assert_eq!(drops.get(), 8);
    }
}
