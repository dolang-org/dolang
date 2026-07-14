use std::{cell::UnsafeCell, marker::PhantomData, mem::MaybeUninit};

use crate::{
    error::{Error, Result},
    object::{protocol::GcObj, sym::SymObj},
    strand::Strand,
    sym::Sym,
    value::{self, Input, InputBy, Slot, Value},
};

pub(crate) type OwnedItem<'v> = (Option<GcObj<'v, SymObj>>, UnsafeCell<Value<'v>>);

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

enum ArgContent<'v, 'a> {
    Borrowed {
        sig: &'a [Option<Sym<'v, 'a>>],
        slots: &'a [UnsafeCell<Value<'v>>],
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
                    *slots.get_unchecked(index).get() = value;
                }
                ArgContent::Owned(items) => {
                    *items.get_unchecked(index).1.get() = value;
                }
            }
        }
    }

    unsafe fn get_unchecked(&self, index: usize, headroom: usize) -> Arg<'v, 'a> {
        unsafe {
            match self {
                ArgContent::Borrowed { slots, sig } => {
                    let slot = Slot::new(&mut *slots.get_unchecked(index).get());
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
                    (Some(sym), value) => {
                        Arg::Key(Sym::from_tag(sym.tag), Slot::new(&mut *value.get()))
                    }
                    (None, value) => Arg::Pos(Slot::new(&mut *value.get())),
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
        slots: &'a [UnsafeCell<Value<'v>>],
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
    #[inline]
    pub async fn with<'s, const N: usize, R>(
        strand: &mut Strand<'v, 's>,
        mut inputs: [&mut dyn Input<'v>; N],
        sig: [Option<Sym<'v, '_>>; N],
        f: impl for<'b> AsyncFnOnce(&mut Strand<'v, 's>, Args<'v, 'b>) -> R,
    ) -> R {
        // HeadroomCells gives us N+1 contiguous cells: head is headroom (NIL),
        // tail[0..N] hold the actual arguments.
        let mut cells = HeadroomCells {
            head: UnsafeCell::new(Value::NIL),
            tail: [const { UnsafeCell::new(Value::NIL) }; N],
        };
        let mut replace = [const { MaybeUninit::uninit() }; N];
        let inner = strand.inner;
        for ((cell, input), replace) in cells
            .tail
            .iter_mut()
            .zip(inputs.iter_mut())
            .zip(replace.iter_mut())
        {
            let (value, slot) = match input.input_take(inner.vm(), value::private::Sealed) {
                InputBy::Borrow(value) => (value.dup(), None),
                InputBy::Value(value, slot) => (value, slot),
            };
            *cell.get_mut() = value;
            replace.write(slot);
        }
        let res = f(strand, unsafe { Args::new(cells.as_slice(), &sig, 1) }).await;
        for (cell, replace) in cells.tail.iter_mut().zip(replace.iter_mut()) {
            if let Some(mut replace) = unsafe { replace.assume_init_mut().take() } {
                replace.store(cell.get_mut().take())
            }
        }
        res
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
        const VAR: bool,
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
        let mut i = 0usize;
        let mut seen = [false; M];
        let mut required = MaybeUninit::<[Slot<'v, 'a>; M]>::uninit();
        let mut optional = [const { None }; MO];

        if VAR && self.mask.is_empty() {
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
                        if VAR {
                            self.mask[self.index] = true;
                        }
                    } else if i < N + NO {
                        optional[i - N] = Some(value);
                        if VAR {
                            self.mask[self.index] = true;
                        }
                    } else if !VAR {
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
                                    unsafe {
                                        *(required.as_mut_ptr() as *mut Slot<'v, 'a>).add(i + N) =
                                            value
                                    }
                                } else {
                                    seen[i + N] = true;
                                    unsafe {
                                        (required.as_mut_ptr() as *mut Slot<'v, 'a>)
                                            .add(i + N)
                                            .write(value)
                                    }
                                }
                                break 'search true;
                            }
                        }
                        for (i, ksym) in koparam.iter().enumerate() {
                            if sym == *ksym {
                                optional[i + NO] = Some(value);
                                break 'search true;
                            }
                        }
                        break false;
                    };
                    if VAR && found {
                        self.mask[self.index] = true;
                    }
                    if !VAR && !found {
                        return Err(Error::unexpected_key(strand, sym));
                    }
                }
            }
            if VAR {
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
        let var = if VAR {
            self.index = self.mask[self.headroom..]
                .iter()
                .position(|b| !*b)
                .map(|i| i + self.headroom)
                .unwrap_or(len);
            Some(self)
        } else {
            None
        };
        Ok((unsafe { required.assume_init() }, optional, var))
    }
}

#[cfg(test)]
mod tests {
    use super::{Arg, Args};
    use crate::Value;
    use std::cell::UnsafeCell;

    #[test]
    fn prepend_self_reuses_a_consumed_leading_argument() {
        let slots = [UnsafeCell::new(Value::NIL), UnsafeCell::new(Value::NIL)];
        let sig = [None];
        let mut args = unsafe { Args::new(&slots, &sig, 1) };

        assert!(matches!(args.next(), Some(Arg::Pos(_))));
        assert!(args.next().is_none());

        args.prepend_self(Value::NIL);
        assert!(matches!(args.next(), Some(Arg::Pos(_))));
        assert!(args.next().is_none());
    }
}

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
#[macro_export]
macro_rules! unpack {
    (impl $strand: expr, $args: expr, $n: expr, $k: expr, $no: expr, $ko: expr,
     { $(, $kargs: tt)* }, { $(, $koargs: tt)* }, {}) => {
        $args.unpack::<{$n}, {$k}, {$no}, {$ko}, {$n + $k}, {$no + $ko}, false>($strand, [$($kargs),*], [$($koargs),*]).map(|(req, opt, _)| (req, opt))
    };
    (impl $strand: expr, $args: expr, $n: expr, $k: expr, $no: expr, $ko: expr,
     { $(, $kargs: tt)* }, { $(, $koargs: tt)* }, {...}) => {
        $args.unpack::<{$n}, {$k}, {$no}, {$ko}, {$n + $k}, {$no + $ko}, true>($strand, [$($kargs),*], [$($koargs),*]).map(|(req, opt, var)| (req, opt, var.unwrap()))
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
/// call!(strand, rcvr, &mut out, 42i64).await?;
/// # Ok(())
/// # }
/// ```
/// where:
/// - `strand`: the current [`Strand`]
/// - `rcvr`: the call receiver, a [`&Value`](Value) (or something that derefs to one)
/// - `out`: where to place the output value on success, anything that implements
///   [`Output`](value::Output)
/// - `pos`: positional argument, anything that implements [`Input`]
/// - `key`: key for a key argument, which should be an in-scope [`Sym`] (typically obtained from
///   [`Builder::sym`](crate::vm::Builder::sym)).
/// - `val`: value for key argument, the same as positional
#[macro_export]
macro_rules! call {
    (impl $strand: expr, $rcvr: expr, $out: expr,
     { $(, $slots: tt)* }, { $(, $sig: tt)* }, {}) => {
        $crate::arg::Args::with($strand, [$($slots),*], [$($sig),*], {
            let rcvr = $rcvr;
            let out = $out;
            async move |strand, args| rcvr.call(strand, args, out).await
        })
    };
    (impl $strand: expr, $rcvr: expr, $out: expr,
     { $(, $slots: tt)* }, { $(, $sig: tt)* }, {$arg: expr, $($rest: tt)*}) => {
        call!(impl $strand, $rcvr, $out, { $(, $slots)*, (&mut { let it = $arg; it }) }, { $(, $sig)*, None }, {$($rest)*})
    };
    (impl $strand: expr, $rcvr: expr, $out: expr,
     { $(, $slots: tt)* }, { $(, $sig: tt)* }, {$key: ident : $arg: expr, $($rest: tt)*}) => {
        call!(impl $strand, $rcvr, $out, { $(, $slots)*, (&mut { let it = $arg; it }) }, { $(, $sig)*, (Some($key)) }, {$($rest)*})
    };
    (impl $strand: expr, $rcvr: expr, $out: expr,
     { $(, $slots: tt)* }, { $(, $sig: tt)* }, {$arg: expr}) => {
        call!(impl $strand, $rcvr, $out, { $(, $slots)*, (&mut { let it = $arg; it }) }, { $(, $sig)*, None }, {})
    };
    (impl $strand: expr, $rcvr: expr, $out: expr,
     { $(, $slots: tt)* }, { $(, $sig: tt)* }, {$key: ident : $arg: expr}) => {
        call!(impl $strand, $rcvr, $out, { $(, $slots)*, (&mut { let it = $arg; it }) }, { $(, $sig)*, (Some($key)) }, {})
    };
    ($strand: expr, $rcvr: expr, $out: expr, $($rest: tt)*) => {
        call!(impl $strand, $rcvr, $out, {}, {}, { $($rest)* })
    };
    ($strand: expr, $rcvr: expr, $out: expr) => {
        call!(impl $strand, $rcvr, $out, {}, {}, { })
    };
}

/// Invoke method on Do value.
///
/// Invoke as:
///
/// ```rust,no_run
/// # use dolang_runtime::{method, strand::Strand, sym::Sym, value::{Value, Slot}};
/// # async fn example<'v, 's, 'a>(strand: &mut Strand<'v, 's>, rcvr: &Value<'v>, method_sym: Sym<'v, 'a>, mut out: Slot<'v, 'a>) -> dolang_runtime::error::Result<'v, 's, ()> {
/// method!(strand, rcvr, method_sym, &mut out, 42i64).await?;
/// # Ok(())
/// # }
/// ```
/// where:
/// - `strand`: the current [`Strand`]
/// - `rcvr`: the method call receiver, a [`&Value`](Value) (or something that derefs to one)
/// - `method`: a [`Sym`] representing the method to invoke
/// - `out`: where to place the output value on success, anything that implements
///   [`Output`](value::Output)
/// - `pos`: positional argument, anything that implements [`Input`]
/// - `key`: key for a key argument, which should be an in-scope [`Sym`] (typically obtained from
///   [`Builder::sym`](crate::vm::Builder::sym)).
/// - `val`: value for key argument, the same as positional
#[macro_export]
macro_rules! method {
    (impl $strand: expr, $rcvr: expr, $method: expr, $out: expr,
     { $(, $slots: tt)* }, { $(, $sig: tt)* }, {}) => {
        $crate::arg::Args::with($strand, [$($slots),*], [$($sig),*], {
            let rcvr = $rcvr;
            let method = $method;
            let out = $out;
            async move |strand, args| rcvr.method(strand, method, args, out).await
        })
    };
    (impl $strand: expr, $rcvr: expr, $method: expr, $out: expr,
     { $(, $slots: tt)* }, { $(, $sig: tt)* }, {$arg: expr, $($rest: tt)*}) => {
        method!(impl $strand, $rcvr, $method, $out, { $(, $slots)*, (&mut { let it = $arg; it }) }, { $(, $sig)*, None }, {$($rest)*})
    };
    (impl $strand: expr, $rcvr: expr, $method: expr, $out: expr,
     { $(, $slots: tt)* }, { $(, $sig: tt)* }, {$key: ident : $arg: expr, $($rest: tt)*}) => {
        method!(impl $strand, $rcvr, $method, $out, { $(, $slots)*, (&mut { let it = $arg; it }) }, { $(, $sig)*, (Some($key)) }, {$($rest)*})
    };
    (impl $strand: expr, $rcvr: expr, $method: expr, $out: expr,
     { $(, $slots: tt)* }, { $(, $sig: tt)* }, {$arg: expr}) => {
        method!(impl $strand, $rcvr, $method, $out, { $(, $slots)*, (&mut { let it = $arg; it }) }, { $(, $sig)*, None }, {})
    };
    (impl $strand: expr, $rcvr: expr, $method: expr, $out: expr,
     { $(, $slots: tt)* }, { $(, $sig: tt)* }, {$key: ident : $arg: expr}) => {
        method!(impl $strand, $rcvr, $method, $out, { $(, $slots)*, (&mut { let it = $arg; it }) }, { $(, $sig)*, (Some($key)) }, {})
    };
    ($strand: expr, $rcvr: expr, $method: expr, $out: expr, $($rest: tt)*) => {
        method!(impl $strand, $rcvr, $method, $out, {}, {}, { $($rest)* })
    };
    ($strand: expr, $rcvr: expr, $method: expr, $out: expr) => {
        method!(impl $strand, $rcvr, $method, $out, {}, {}, { })
    };
}
