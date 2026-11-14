use std::{marker::PhantomData, mem::MaybeUninit};

use crate::{
    error::{Error, Result},
    object::{protocol::GcObj, sym::SymObj},
    strand::Strand,
    sym::Sym,
    value::{self, Input, Slot, Value, ValueCell},
};

pub(crate) type OwnedItem<'v> = (Option<GcObj<'v, SymObj>>, ValueCell<'v>);

/// Stack-allocated array with one headroom slot at position 0.
/// `repr(C)` guarantees `head` is immediately followed by `tail` elements
/// with no padding (same-typed fields), enabling a contiguous slice view.
#[repr(C)]
struct HeadroomCells<T, const N: usize> {
    head: T,
    tail: [T; N],
}

impl<T, const N: usize> HeadroomCells<T, N> {
    fn as_slice(&self) -> &[T] {
        // SAFETY: repr(C) guarantees head + tail are laid out contiguously.
        // Pointer derived from &self has provenance over the entire struct.
        unsafe { std::slice::from_raw_parts(self as *const Self as *const T, N + 1) }
    }
}

/// Argument cells for [`call!`](crate::call) and [`method!`](crate::method), declared in
/// the caller's frame.
#[doc(hidden)]
pub struct ArgFrame<'v, 'i, const N: usize> {
    // N+1 contiguous cells: head is headroom (NIL), tail[0..N] hold the arguments
    cells: HeadroomCells<ValueCell<'v>, N>,
    // Where each argument moved from, restored after the call
    replace: [Option<Slot<'v, 'i>>; N],
}

impl<'v, 'i, const N: usize> ArgFrame<'v, 'i, N> {
    #[inline]
    pub fn new() -> Self {
        Self {
            cells: HeadroomCells {
                head: ValueCell::default(),
                tail: std::array::from_fn(|_| ValueCell::default()),
            },
            replace: [const { None }; N],
        }
    }

    /// Take `inputs` into the frame, returning the arguments and a guard that moves values
    /// taken from rooted locations back when dropped.
    #[inline]
    pub fn enter<'f>(
        &'f mut self,
        strand: &mut Strand<'v, '_>,
        inputs: &'i mut [&mut dyn Input<'v>; N],
        sig: &'f [Option<Sym<'v, 'f>>; N],
    ) -> (ArgGuard<'v, 'i, 'f>, Args<'v, 'f>) {
        marshal(strand, &self.cells.tail, inputs, &mut self.replace);
        let guard = ArgGuard {
            cells: &self.cells.tail,
            replace: &mut self.replace,
        };
        // Safety: the arguments are not scanned, here or during the call
        let args = unsafe { Args::new(self.cells.as_slice(), sig, 1) };
        (guard, args)
    }
}

impl<const N: usize> Default for ArgFrame<'_, '_, N> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

/// Moves arguments back to the rooted locations they were taken from when dropped.
#[doc(hidden)]
pub struct ArgGuard<'v, 'i, 'f> {
    cells: &'f [ValueCell<'v>],
    replace: &'f mut [Option<Slot<'v, 'i>>],
}

impl Drop for ArgGuard<'_, '_, '_> {
    #[inline]
    fn drop(&mut self) {
        restore(self.cells, self.replace);
    }
}

#[inline(never)]
fn marshal<'v, 'i>(
    strand: &mut Strand<'v, '_>,
    cells: &[ValueCell<'v>],
    inputs: &'i mut [&mut dyn Input<'v>],
    replace: &mut [Option<Slot<'v, 'i>>],
) {
    for ((cell, input), replace) in cells.iter().zip(inputs).zip(replace) {
        // Safety: nothing else accesses the cell while the slot lives
        let out = unsafe { cell.slot() };
        *replace = input.input_take(strand, out, value::private::Sealed);
    }
}

#[inline(never)]
fn restore<'v>(cells: &[ValueCell<'v>], replace: &mut [Option<Slot<'v, '_>>]) {
    for (cell, replace) in cells.iter().zip(replace) {
        if let Some(mut replace) = replace.take() {
            // Safety: the call is over, so nothing else accesses the cell
            replace.store(unsafe { cell.slot() }.take())
        }
    }
}

enum ArgContent<'v, 'a> {
    Borrowed {
        sig: &'a [Option<Sym<'v, 'a>>],
        slots: &'a [ValueCell<'v>],
    },
    Owned(&'a [OwnedItem<'v>]),
}

impl<'v, 'a> ArgContent<'v, 'a> {
    fn len(&self) -> usize {
        match self {
            ArgContent::Borrowed { slots, .. } => slots.len(),
            ArgContent::Owned(items) => items.len(),
        }
    }

    unsafe fn write_unchecked(&self, index: usize, value: Value<'v>) {
        unsafe {
            match self {
                ArgContent::Borrowed { slots, .. } => {
                    slots.get_unchecked(index).slot().store(value);
                }
                ArgContent::Owned(items) => {
                    items.get_unchecked(index).1.slot().store(value);
                }
            }
        }
    }

    unsafe fn get_unchecked(&self, index: usize, headroom: usize) -> Arg<'v, 'a> {
        unsafe {
            match self {
                ArgContent::Borrowed { slots, sig } => {
                    let slot = slots.get_unchecked(index).slot();
                    if index < headroom {
                        Arg::Pos(slot)
                    } else {
                        match sig.get_unchecked(index - headroom) {
                            Some(key) => Arg::Key(*key, slot),
                            None => Arg::Pos(slot),
                        }
                    }
                }
                ArgContent::Owned(items) => match items.get_unchecked(index) {
                    (Some(sym), value) => Arg::Key(Sym::from_tag(sym.tag()), value.slot()),
                    (None, value) => Arg::Pos(value.slot()),
                },
            }
        }
    }
}

/// Call arguments.
///
/// Acts as an iterator over the arguments in exact call order, including any interleaving of
/// positional and key arguments.  The [`unpack!()`](crate::unpack) macro provides a more convenient way to
/// extract arguments.
pub struct Args<'v, 'a> {
    content: ArgContent<'v, 'a>,
    phantom: PhantomData<&'v mut &'v ()>,
    mask: Vec<bool>,
    index: usize,
    headroom: usize,
}

/// Call argument
pub enum Arg<'v, 'a> {
    /// Positional argument
    Pos(Slot<'v, 'a>),
    /// Key argument
    /// - `0`: the key symbol
    /// - `1`: the value
    Key(Sym<'v, 'a>, Slot<'v, 'a>),
}

impl<'v, 'a> Iterator for Args<'v, 'a> {
    type Item = Arg<'v, 'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.index < self.content.len() {
                let index = self.index;
                self.index += 1;
                if !self.mask.is_empty() && self.mask[index] {
                    continue;
                }
                break Some(unsafe { self.content.get_unchecked(index, self.headroom) });
            } else {
                break None;
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.content.len()
            - self.index
            - if self.mask.is_empty() {
                0
            } else {
                self.mask[self.index..].iter().map(|b| *b as usize).sum()
            };
        (len, Some(len))
    }
}

impl<'v, 'a> ExactSizeIterator for Args<'v, 'a> {}

impl<'v, 'a> Args<'v, 'a> {
    // Safety:
    // - Concurrent mutable reference into base must not be formed
    // - slots.len() must equal sig.len() + headroom
    pub(crate) unsafe fn new(
        slots: &'a [ValueCell<'v>],
        sig: &'a [Option<Sym<'v, 'a>>],
        headroom: usize,
    ) -> Self {
        Self {
            content: ArgContent::Borrowed { sig, slots },
            phantom: PhantomData,
            headroom,
            index: headroom,
            mask: Vec::new(),
        }
    }

    pub(crate) fn new_owned(items: &'a [OwnedItem<'v>], headroom: usize) -> Self {
        Self {
            content: ArgContent::Owned(items),
            phantom: PhantomData,
            index: headroom,
            headroom,
            mask: Vec::new(),
        }
    }

    /// Iterates the remaining arguments without consuming them, so they can be iterated
    /// again afterward.
    pub(crate) fn reborrow(&mut self) -> Args<'v, '_> {
        let content = match &self.content {
            ArgContent::Borrowed { sig, slots } => ArgContent::Borrowed { sig, slots },
            ArgContent::Owned(items) => ArgContent::Owned(items),
        };
        Args {
            content,
            phantom: PhantomData,
            mask: self.mask.clone(),
            index: self.index,
            headroom: self.headroom,
        }
    }

    /// Insert a positional value into the headroom slot, expanding the
    /// visible argument range by one.
    ///
    /// Panics if no leading slot remains. Consumed leading arguments can be
    /// reused as headroom, including arguments skipped by a trailing pack.
    pub(crate) fn prepend_self(&mut self, value: Value<'v>) {
        assert!(
            self.index > 0,
            "prepend_self called with no available headroom"
        );
        self.index -= 1;
        unsafe { self.content.write_unchecked(self.index, value) };
        if !self.mask.is_empty() {
            self.mask[self.index] = false;
        }
    }

    #[doc(hidden)]
    #[expect(clippy::type_complexity)]
    #[inline]
    pub fn unpack<
        's,
        const N: usize,
        const K: usize,
        const NO: usize,
        const KO: usize,
        const M: usize,
        const MO: usize,
        const POS: bool,
        const KEY: bool,
    >(
        mut self,
        strand: &mut Strand<'v, 's>,
        kparam: [Sym<'v, 'a>; K],
        koparam: [Sym<'v, 'a>; KO],
    ) -> Result<'v, 's, ([Slot<'v, 'a>; M], [Option<Slot<'v, 'a>>; MO], Option<Self>)> {
        const {
            assert!(N + K == M);
            assert!(NO + KO == MO);
        }
        let var = POS || KEY;
        let mut i = 0usize;
        let mut seen = [false; M];
        let mut required = MaybeUninit::<[Slot<'v, 'a>; M]>::uninit();
        let mut optional = [const { None }; MO];

        // Matched arguments are masked out of the returned rest, unless the
        // only ones are leading positionals, which the rest can start after
        let masked = var && (KEY || K + KO > 0 || !self.mask.is_empty());
        let mut rest_start = self.index;
        if masked && self.mask.is_empty() {
            self.mask.resize(self.content.len(), false);
            self.mask[0..self.index].fill(true);
        }

        let len = self.content.len();
        while self.index != len {
            let arg = unsafe { self.content.get_unchecked(self.index, self.headroom) };
            match arg {
                Arg::Pos(value) => {
                    if i < N {
                        unsafe {
                            (required.as_mut_ptr() as *mut Slot<'v, 'a>)
                                .add(i)
                                .write(value)
                        }
                        seen[i] = true;
                        if masked {
                            self.mask[self.index] = true;
                        }
                        rest_start = self.index + 1;
                    } else if i < N + NO {
                        optional[i - N] = Some(value);
                        if masked {
                            self.mask[self.index] = true;
                        }
                        rest_start = self.index + 1;
                    } else if !POS {
                        return Err(Error::unexpected_positional(strand, i));
                    }
                    i += 1;
                }
                Arg::Key(sym, value) => {
                    #[expect(clippy::never_loop)]
                    let found = 'search: loop {
                        for (i, ksym) in kparam.iter().enumerate() {
                            if sym == *ksym {
                                if seen[i + N] {
                                    if !KEY {
                                        return Err(Error::duplicate_key(strand, sym));
                                    }
                                    break 'search false;
                                }
                                seen[i + N] = true;
                                unsafe {
                                    (required.as_mut_ptr() as *mut Slot<'v, 'a>)
                                        .add(i + N)
                                        .write(value)
                                }
                                break 'search true;
                            }
                        }
                        for (i, ksym) in koparam.iter().enumerate() {
                            if sym == *ksym {
                                if optional[i + NO].is_some() {
                                    if !KEY {
                                        return Err(Error::duplicate_key(strand, sym));
                                    }
                                    break 'search false;
                                }
                                optional[i + NO] = Some(value);
                                break 'search true;
                            }
                        }
                        break false;
                    };
                    if masked && found {
                        self.mask[self.index] = true;
                    }
                    if !KEY && !found {
                        return Err(Error::unexpected_key(strand, sym));
                    }
                }
            }
            if !self.mask.is_empty() {
                self.index = self.mask[self.index + 1..]
                    .iter()
                    .position(|b| !*b)
                    .map(|i| i + self.index + 1)
                    .unwrap_or(len);
            } else {
                self.index += 1;
            }
        }
        for (i, s) in seen.into_iter().enumerate() {
            if !s {
                if i < N {
                    return Err(Error::missing_positional(strand, i));
                } else {
                    return Err(Error::missing_key(strand, kparam[i - N]));
                }
            }
        }
        let rest = if masked {
            self.index = self.mask[self.headroom..]
                .iter()
                .position(|b| !*b)
                .map(|i| i + self.headroom)
                .unwrap_or(len);
            Some(self)
        } else if var {
            self.index = rest_start;
            Some(self)
        } else {
            None
        };
        Ok((unsafe { required.assume_init() }, optional, rest))
    }

    #[doc(hidden)]
    pub fn into_pos(self) -> PosArgs<'v, 'a> {
        PosArgs(self)
    }

    #[doc(hidden)]
    pub fn into_keys(self) -> KeyArgs<'v, 'a> {
        KeyArgs(self)
    }
}

/// Positional call arguments, left over by [`unpack!()`](crate::unpack) with a `*` rest.
pub struct PosArgs<'v, 'a>(Args<'v, 'a>);

impl<'v, 'a> Iterator for PosArgs<'v, 'a> {
    type Item = Slot<'v, 'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.0.next()? {
            Arg::Pos(slot) => Some(slot),
            Arg::Key(..) => unreachable!("`*` rest holds a key argument"),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl<'v, 'a> ExactSizeIterator for PosArgs<'v, 'a> {}

/// Key call arguments, left over by [`unpack!()`](crate::unpack) with a `**` rest.
pub struct KeyArgs<'v, 'a>(Args<'v, 'a>);

impl<'v, 'a> Iterator for KeyArgs<'v, 'a> {
    type Item = (Sym<'v, 'a>, Slot<'v, 'a>);

    fn next(&mut self) -> Option<Self::Item> {
        match self.0.next()? {
            Arg::Key(sym, slot) => Some((sym, slot)),
            Arg::Pos(_) => unreachable!("`**` rest holds a positional argument"),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl<'v, 'a> ExactSizeIterator for KeyArgs<'v, 'a> {}

/// Unpacks [`Args`] into required and optional positional and key arguments
///
/// Invoke as:
///
/// ```rust,no_run
/// # use dolang_runtime::{arg::Args, strand::Strand, sym::Sym, value::Slot, unpack};
/// # async fn example<'v, 's, 'a>(strand: &mut Strand<'v, 's>, args: Args<'v, 'a>, key1: Sym<'v, 'a>, key2: Sym<'v, 'a>) -> dolang_runtime::error::Result<'v, 's, ()> {
/// let ([req1, req2], [opt1]) = unpack!(strand, args, 1, 0, key1, key2 = None)?;
/// # Ok(())
/// # }
/// ```
/// where:
/// - `args`: the [`Args`] pack
/// - `n`: the number of required positional parameters
/// - `no`: the number of optional positional parameters
/// - `key1`: a required key argument, which should be an in-scope [`Sym`] (typically obtained from
///   [`Builder::sym`](crate::vm::Builder::sym)).
/// - `key2`: like the above, but optional
/// - `req1`, ...: required arguments; positional first, then key arguments in provided order
/// - `opt1`, ...: optional arguments; positional first, then key arguments in provided order
///
/// The first occurrence of a named keyword binds its parameter. Further occurrences
/// go to the keyed rest (`**` or `...`), or raise `UnexpectedKeyError` without one.
/// Keywords captured by a rest retain their order.
///
/// Required arguments are of type [`Slot`] and optional are of type [`Option<Slot>`].
///
/// To capture remaining arguments, add `...` to the end of the invocation:
///
/// ```rust,no_run
/// # use dolang_runtime::{arg::Args, strand::Strand, sym::Sym, value::Slot, unpack};
/// # async fn example<'v, 's, 'a>(strand: &mut Strand<'v, 's>, args: Args<'v, 'a>) -> dolang_runtime::error::Result<'v, 's, ()> {
/// let ([req], [opt], rest) = unpack!(strand, args, 1, 1, ...)?;
/// # Ok(())
/// # }
/// ```
///
/// `rest` is then an [`Args`] with the unmatched arguments in call order.  End the invocation
/// with `*` instead to capture only positional arguments as a [`PosArgs`], or with `**` to capture
/// only key arguments as a [`KeyArgs`]; an unmatched argument of the other kind is an error, as
/// without a rest:
///
/// ```rust,no_run
/// # use dolang_runtime::{arg::Args, strand::Strand, sym::Sym, value::Slot, unpack};
/// # async fn example<'v, 's, 'a>(strand: &mut Strand<'v, 's>, args: Args<'v, 'a>, key: Sym<'v, 'a>) -> dolang_runtime::error::Result<'v, 's, ()> {
/// let ([first], [opt], values) = unpack!(strand, args, 1, 0, key = None, *)?;
/// for value in values {
///     // ...
/// }
/// # Ok(())
/// # }
/// ```
#[macro_export]
macro_rules! unpack {
    (impl $strand: expr, $args: expr, $n: expr, $k: expr, $no: expr, $ko: expr,
     { $(, $kargs: tt)* }, { $(, $koargs: tt)* }, {}) => {
        $args.unpack::<{$n}, {$k}, {$no}, {$ko}, {$n + $k}, {$no + $ko}, false, false>($strand, [$($kargs),*], [$($koargs),*]).map(|(req, opt, _)| (req, opt))
    };
    (impl $strand: expr, $args: expr, $n: expr, $k: expr, $no: expr, $ko: expr,
     { $(, $kargs: tt)* }, { $(, $koargs: tt)* }, {...}) => {
        $args.unpack::<{$n}, {$k}, {$no}, {$ko}, {$n + $k}, {$no + $ko}, true, true>($strand, [$($kargs),*], [$($koargs),*]).map(|(req, opt, var)| (req, opt, var.unwrap()))
    };
    (impl $strand: expr, $args: expr, $n: expr, $k: expr, $no: expr, $ko: expr,
     { $(, $kargs: tt)* }, { $(, $koargs: tt)* }, {*}) => {
        $args.unpack::<{$n}, {$k}, {$no}, {$ko}, {$n + $k}, {$no + $ko}, true, false>($strand, [$($kargs),*], [$($koargs),*]).map(|(req, opt, var)| (req, opt, var.unwrap().into_pos()))
    };
    (impl $strand: expr, $args: expr, $n: expr, $k: expr, $no: expr, $ko: expr,
     { $(, $kargs: tt)* }, { $(, $koargs: tt)* }, {**}) => {
        $args.unpack::<{$n}, {$k}, {$no}, {$ko}, {$n + $k}, {$no + $ko}, false, true>($strand, [$($kargs),*], [$($koargs),*]).map(|(req, opt, var)| (req, opt, var.unwrap().into_keys()))
    };
    (impl $strand: expr, $args: expr, $n: expr, $k: expr, $no: expr, $ko: expr,
     { $($kargs: tt)* }, { $($koargs: tt)* }, { $key: ident } ) => {
        unpack!(impl $strand, $args, $n, $k + 1, $no, $ko, { $($kargs)*, $key }, { $($koargs)* }, {})
    };
    (impl $strand: expr, $args: expr, $n: expr, $k: expr, $no: expr, $ko: expr,
     { $($kargs: tt)* }, { $($koargs: tt)* }, { $key: ident, $($rest: tt)* } ) => {
        unpack!(impl $strand, $args, $n, $k + 1, $no, $ko, { $($kargs)*, $key }, { $($koargs)* }, { $($rest)* })
    };
    (impl $strand: expr, $args: expr, $n: expr, $k: expr, $no: expr, $ko: expr,
     { $($kargs: tt)* }, { $($koargs: tt)* }, { $key: ident = None } ) => {
        unpack!(impl $strand, $args, $n, $k, $no, $ko + 1, { $($kargs)* }, { $($koargs)*, $key }, {})
    };
    (impl $strand: expr, $args: expr, $n: expr, $k: expr, $no: expr, $ko: expr,
     { $($kargs: tt)* }, { $($koargs: tt)* }, { $key: ident = None, $($rest: tt)* } ) => {
        unpack!(impl $strand, $args, $n, $k, $no, $ko + 1, { $($kargs)* }, { $($koargs)*, $key }, { $($rest)* })
    };
    ($strand: expr, $args: expr, $n: expr, $no: expr, $($rest: tt)*) => {
        unpack!(impl $strand, $args, $n, 0, $no, 0, {}, {}, { $($rest)* })
    };
    ($strand: expr, $args: expr, $n: expr, $no: expr) => {
        unpack!(impl $strand, $args, $n, 0, $no, 0, {}, {}, {})
    }
}

/// Call callable Do value.
///
/// Invoke as:
///
/// ```rust,no_run
/// # use dolang_runtime::{call, strand::Strand, value::{Value, Slot}};
/// # async fn example<'v, 's, 'a>(strand: &mut Strand<'v, 's>, rcvr: &Value<'v>, mut out: Slot<'v, 'a>) -> dolang_runtime::error::Result<'v, 's, ()> {
/// call!(strand, rcvr, 42i64, &mut out)?;
/// # Ok(())
/// # }
/// ```
/// where:
/// - `strand`: the current [`Strand`]
/// - `rcvr`: the call receiver, a [`&Value`](Value) (or something that derefs to one)
/// - `pos`: positional argument, anything that implements [`Input`]
/// - `key`: key for a key argument, which should be an in-scope [`Sym`] (typically obtained from
///   [`Builder::sym`](crate::vm::Builder::sym)).
/// - `val`: value for key argument, the same as positional
/// - `out`: where to place the output value on success, anything that implements
///   [`Output`](value::Output)
///
/// The call runs and is awaited inline, so the macro must be used in an async context and
/// evaluates to the call's [`Result`](crate::error::Result). An argument moved out of a
/// [`Slot`](value::Slot) is moved back when the call finishes or is dropped.
#[macro_export]
macro_rules! call {
    (impl $strand: expr, $rcvr: expr,
     { $(, $slots: tt)* }, { $(, $sig: tt)* }, {$arg: expr, $($rest: tt)+}) => {
        call!(impl $strand, $rcvr, { $(, $slots)*, (&mut { let it = $arg; it }) }, { $(, $sig)*, None }, {$($rest)+})
    };
    (impl $strand: expr, $rcvr: expr,
     { $(, $slots: tt)* }, { $(, $sig: tt)* }, {$key: ident : $arg: expr, $($rest: tt)+}) => {
        call!(impl $strand, $rcvr, { $(, $slots)*, (&mut { let it = $arg; it }) }, { $(, $sig)*, (Some($key)) }, {$($rest)+})
    };
    (impl $strand: expr, $rcvr: expr,
     { $(, $slots: tt)* }, { $(, $sig: tt)* }, {$out: expr $(,)?}) => {
        {
            let mut inputs: [&mut dyn $crate::value::Input<'_>; _] = [$($slots),*];
            let sig = [$($sig),*];
            let rcvr = $rcvr;
            let mut out = $out;
            let strand: &mut $crate::strand::Strand<'_, '_> = $strand;
            let mut frame = $crate::arg::ArgFrame::new();
            let (_guard, args) = frame.enter(strand, &mut inputs, &sig);
            let out = $crate::value::Slot::from_output(&mut out);
            $crate::value::Value::op_call(&rcvr, strand, args, out).await
        }
    };
    ($strand: expr, $rcvr: expr, $($rest: tt)+) => {
        call!(impl $strand, $rcvr, {}, {}, { $($rest)+ })
    };
}

/// Invoke method on Do value.
///
/// Invoke as:
///
/// ```rust,no_run
/// # use dolang_runtime::{method, strand::Strand, sym::Sym, value::{Value, Slot}};
/// # async fn example<'v, 's, 'a>(strand: &mut Strand<'v, 's>, rcvr: &Value<'v>, method_sym: Sym<'v, 'a>, mut out: Slot<'v, 'a>) -> dolang_runtime::error::Result<'v, 's, ()> {
/// method!(strand, rcvr, method_sym, 42i64, &mut out)?;
/// # Ok(())
/// # }
/// ```
/// where:
/// - `strand`: the current [`Strand`]
/// - `rcvr`: the method call receiver, a [`&Value`](Value) (or something that derefs to one)
/// - `method`: a [`Sym`] representing the method to invoke
/// - `pos`: positional argument, anything that implements [`Input`]
/// - `key`: key for a key argument, which should be an in-scope [`Sym`] (typically obtained from
///   [`Builder::sym`](crate::vm::Builder::sym)).
/// - `val`: value for key argument, the same as positional
/// - `out`: where to place the output value on success, anything that implements
///   [`Output`](value::Output)
///
/// The call runs and is awaited inline, so the macro must be used in an async context and
/// evaluates to the call's [`Result`](crate::error::Result). An argument moved out of a
/// [`Slot`](value::Slot) is moved back when the call finishes or is dropped.
#[macro_export]
macro_rules! method {
    (impl $strand: expr, $rcvr: expr, $method: expr,
     { $(, $slots: tt)* }, { $(, $sig: tt)* }, {$arg: expr, $($rest: tt)+}) => {
        method!(impl $strand, $rcvr, $method, { $(, $slots)*, (&mut { let it = $arg; it }) }, { $(, $sig)*, None }, {$($rest)+})
    };
    (impl $strand: expr, $rcvr: expr, $method: expr,
     { $(, $slots: tt)* }, { $(, $sig: tt)* }, {$key: ident : $arg: expr, $($rest: tt)+}) => {
        method!(impl $strand, $rcvr, $method, { $(, $slots)*, (&mut { let it = $arg; it }) }, { $(, $sig)*, (Some($key)) }, {$($rest)+})
    };
    (impl $strand: expr, $rcvr: expr, $method: expr,
     { $(, $slots: tt)* }, { $(, $sig: tt)* }, {$out: expr $(,)?}) => {
        {
            let mut inputs: [&mut dyn $crate::value::Input<'_>; _] = [$($slots),*];
            let sig = [$($sig),*];
            let rcvr = $rcvr;
            let method = $method;
            let mut out = $out;
            let strand: &mut $crate::strand::Strand<'_, '_> = $strand;
            let mut frame = $crate::arg::ArgFrame::new();
            let (_guard, args) = frame.enter(strand, &mut inputs, &sig);
            let out = $crate::value::Slot::from_output(&mut out);
            $crate::value::Value::op_mcall(&rcvr, strand, method, args, out).await
        }
    };
    ($strand: expr, $rcvr: expr, $method: expr, $($rest: tt)+) => {
        method!(impl $strand, $rcvr, $method, {}, {}, { $($rest)+ })
    };
}

#[cfg(test)]
mod tests {
    use super::{Arg, Args};
    use crate::Value;
    use crate::value::ValueCell;

    #[test]
    fn prepend_self_reuses_a_consumed_leading_argument() {
        let slots = [ValueCell::default(), ValueCell::default()];
        let sig = [None];
        let mut args = unsafe { Args::new(&slots, &sig, 1) };

        assert!(matches!(args.next(), Some(Arg::Pos(_))));
        assert!(args.next().is_none());

        args.prepend_self(Value::NIL);
        assert!(matches!(args.next(), Some(Arg::Pos(_))));
        assert!(args.next().is_none());
    }
}
