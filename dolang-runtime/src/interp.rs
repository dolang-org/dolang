use std::{cell::UnsafeCell, collections::HashMap, hint::unreachable_unchecked, mem, ptr};

use dolang_bytecode::{Opcode, builtin};
use dolang_util::alias;

use crate::{
    arg::{Arg, Args, OwnedItem},
    bytecode::UnsafeInstDecoder,
    call,
    error::{Error, ErrorKind, Result},
    frame::{CallFrame, Upvars},
    gc::Gc,
    object::{
        arg::ArgIter,
        array::Array,
        class::{ClassEntry, ClassObject},
        dict::Dict,
        module::{Module, Namespace},
        protocol::{GcObj, Spread, SpreadContext},
    },
    sig::{self, Pack},
    strand::{Strand, StrandInner},
    sym::{self, Sym},
    unpack,
    value::{Output, Slot, Slots, Value},
    vm::Vm,
};

enum Status<'v> {
    Ret(Value<'v>),
    Running,
}

impl<'v> CallFrame<'v> {
    unsafe fn push(&self, value: Value<'v>) {
        unsafe { *self.slots.get_unchecked(self.sp.get()).get() = value }
        self.sp.update(|s| s + 1);
    }

    unsafe fn pop(&self) -> Value<'v> {
        self.sp.update(|s| s - 1);
        unsafe { (*self.slots.get_unchecked(self.sp.get()).get()).take() }
    }

    unsafe fn discard(&self, count: usize) {
        let end = self.sp.get();
        self.sp.update(|s| s - count);
        unsafe {
            for slot in self.slots.get_unchecked(self.sp.get()..end) {
                *slot.get() = Value::NIL;
            }
        }
    }

    unsafe fn dup(&self) {
        unsafe {
            let value = (*self.slots.get_unchecked(self.sp.get() - 1).get()).dup();
            self.push(value)
        }
    }

    unsafe fn swap(&self, i: usize, j: usize) {
        unsafe {
            ptr::swap(
                self.slots.get_unchecked(self.sp.get() - 1 - i).get(),
                self.slots.get_unchecked(self.sp.get() - 1 - j).get(),
            );
        }
    }

    unsafe fn load(&self, index: usize) -> Value<'v> {
        unsafe { (*self.slots.get_unchecked(index).get()).dup() }
    }

    unsafe fn take(&self, index: usize) -> Value<'v> {
        unsafe { (*self.slots.get_unchecked(index).get()).take() }
    }

    unsafe fn store(&self, index: usize, value: Value<'v>) {
        unsafe { *self.slots.get_unchecked(index).get() = value }
    }

    unsafe fn scratch1(&self) -> Slot<'v, '_> {
        unsafe { Slot::new(&mut *self.scratch1.get()) }
    }

    unsafe fn scratch2(&self) -> Slot<'v, '_> {
        unsafe { Slot::new(&mut *self.scratch2.get()) }
    }

    unsafe fn scratch3(&self) -> Slot<'v, '_> {
        unsafe { Slot::new(&mut *self.scratch3.get()) }
    }

    #[expect(clippy::mut_from_ref)]
    unsafe fn items(&self) -> &mut Vec<OwnedItem<'v>> {
        unsafe { &mut *self.items.get() }
    }
}

impl<'v> Vm<'v> {
    pub(crate) async fn import_raw<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        name: &str,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if let Some(module) = self.native_modules.get(name) {
            out.store(module.dup());
            return Ok(());
        }
        match self.import_cache.borrow().get(name) {
            Some(None) => return Err(Error::cyclic_import(strand, name)),
            Some(Some(weak)) => {
                if let Some(strong) = weak.upgrade() {
                    out.store(strong);
                    return Ok(());
                }
            }
            _ => {}
        }
        // Take this opportunity to clean import cache
        {
            let mut borrow = self.import_cache.borrow_mut();
            borrow.retain(|_, w| w.as_ref().is_none_or(|w| !w.is_released()));
        }
        let mut err = Error::not_supported(strand);
        for importer in self.importers.iter() {
            {
                let mut borrow = self.import_cache.borrow_mut();
                // Insert a guard value
                borrow.insert(name.to_owned(), None);
                // Drop borrow before calling importer
            }
            if let Err(e) = call!(strand, importer, Slot::reborrow(&mut out), name).await {
                if e.kind() == ErrorKind::Import {
                    err = e;
                    continue;
                }
                return Err(e);
            }
            let mut borrow = self.import_cache.borrow_mut();
            borrow.insert(name.to_owned(), Some(out.downgrade()));
            return Ok(());
        }
        Err(err)
    }

    #[inline(never)]
    pub(crate) async fn import<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        mut args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        enum Mode {
            Get,
            Insert,
            Module,
        }

        let (name, mode) = match args.next() {
            Some(Arg::Key(key, slot)) if key.tag() == sym::GET => (slot, Mode::Get),
            Some(Arg::Key(key, slot)) if key.tag() == sym::INSERT => (slot, Mode::Insert),
            Some(Arg::Key(key, slot)) if key.tag() == sym::MODULE => (slot, Mode::Module),
            Some(Arg::Pos(_)) => return Err(Error::unexpected_positional(strand, 0)),
            _ => return Err(Error::missing_key(strand, Sym::well_known(sym::GET))),
        };
        let name = name
            .as_str(strand)
            .ok_or_else(|| Error::type_error(strand, "import must be a string"))?;
        let components: Vec<_> = name.split('.').collect();
        strand
            .with_slots(async move |strand, [mut raw]| {
                self.import_raw(strand, name, Slot::reborrow(&mut raw))
                    .await?;
                match mode {
                    Mode::Get => Slot::set(strand, out, raw),
                    Mode::Insert => {
                        let root = args
                            .next()
                            .ok_or_else(|| Error::missing_positional(strand, 0))?;
                        let root = match root {
                            Arg::Pos(slot) => slot,
                            Arg::Key(sym, _) => return Err(Error::unexpected_key(strand, sym)),
                        };
                        let ns = root
                            .downcast_ref(strand.builtin_types().namespace)
                            .ok_or_else(|| {
                                Error::type_error(strand, "import root must be a namespace")
                            })?;
                        ns.borrow_mut()
                            .ok_or_else(|| Error::concurrency(strand))?
                            .insert(strand, components.get(1..).unwrap_or(&[]), raw)?;
                    }
                    Mode::Module => {
                        let mut ns = Namespace::new(strand);
                        ns.insert(strand, &[], raw)?;
                        out.store(Value::from_object(GcObj::new(
                            strand.arena(),
                            strand.builtin_types().namespace,
                            ns,
                        )));
                    }
                }
                Ok(())
            })
            .await
    }

    async fn array<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        out.store(Value::from_object(GcObj::new(
            self.arena(),
            self.builtin_types().array,
            Array::from_builtin_args(strand, args).await?,
        )));
        Ok(())
    }

    async fn dict<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        out.store(Value::from_object(GcObj::new(
            self.arena(),
            self.builtin_types().dict,
            Dict::from_builtin_args(strand, args).await?,
        )));
        Ok(())
    }

    async fn iter<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let (_, [obj]) = unpack!(strand, args, 0, 1)?;
        if let Some(obj) = obj {
            obj.op_iter(strand, out).await
        } else {
            out.store(strand.inner.input());
            Ok(())
        }
    }

    fn concat_str<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let mut acc = String::new();
        for arg in args {
            let value = match arg {
                Arg::Pos(mut slot) => slot.take(),
                Arg::Key(sym, _) => return Err(Error::unexpected_key(strand, sym)),
            };
            value.op_display(strand, &mut acc)?;
        }
        out.store(Value::from_str(strand, &acc));
        Ok(())
    }

    fn concat_bin<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let mut acc: Vec<u8> = Vec::new();
        for arg in args {
            let value = match arg {
                Arg::Pos(mut slot) => slot.take(),
                Arg::Key(sym, _) => return Err(Error::unexpected_key(strand, sym)),
            };
            let bytes = value.as_u8_slice(strand).ok_or_else(|| {
                Error::type_error(strand, "expected binary or string value in binary concat")
            })?;
            acc.extend_from_slice(bytes);
        }
        out.store(Value::from_u8_slice(strand, &acc));
        Ok(())
    }

    fn concat_arg<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let mut acc = String::new();
        for arg in args {
            let value = match arg {
                Arg::Pos(mut slot) => slot.take(),
                Arg::Key(sym, _) => return Err(Error::unexpected_key(strand, sym)),
            };
            value.op_display_arg(strand, &mut acc)?;
        }
        out.store(Value::from_str(strand, &acc));
        Ok(())
    }

    fn args<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        args: Args<'v, '_>,
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let arg_iter = ArgIter::from_args(strand, args);
        out.store(Value::from_object(GcObj::new(
            strand.vm().arena(),
            strand.vm().builtin_types().arg_iter,
            arg_iter,
        )));
        Ok(())
    }

    #[inline(never)]
    fn class_create<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        mut args: Args<'v, '_>,
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let Some(Arg::Pos(name)) = args.next() else {
            return Err(Error::missing_positional(strand, 0));
        };

        if args.len() < 1 {
            return Err(Error::missing_positional(strand, 1));
        }

        // Extract class name
        let name: alias::Box<str> = name
            .as_str(strand)
            .ok_or_else(|| Error::type_error(strand, "class_create: expected string name"))?
            .into();

        // Construct superclass list.
        let len = args.len() - 1;
        let mut supers = Vec::with_capacity(len);
        for arg in (&mut args).take(len) {
            let mut slot = match arg {
                Arg::Pos(slot) => slot,
                Arg::Key(key, _) => return Err(Error::unexpected_key(strand, key)),
            };
            if !slot.is_instance_of(strand, &strand.singletons().type_obj.dup()) {
                return Err(Error::type_error(
                    strand,
                    "class_create: superclass must be a type object",
                ));
            }
            supers.push(slot.take());
        }

        let module = match args.next().unwrap() {
            Arg::Pos(slot) => slot,
            Arg::Key(key, _) => return Err(Error::unexpected_key(strand, key)),
        };

        // Build native_supers and entries in a single left-to-right MRO pass.
        // native_supers: non-abstract native type objects; index == ClassInstance native slot.
        // Abstract entries store the type-object value directly (no separate list needed).
        // entry_map: built left-to-right with first-insertion-wins (MRO order).
        let mut native_supers: Vec<Value<'v>> = Vec::new();
        let mut seen_abstract: Vec<Value<'v>> = Vec::new(); // for dedup only
        let mut entry_map: HashMap<Sym<'v, 'static>, ClassEntry<'v>> = HashMap::new();
        let mut field_defaults: Vec<Value<'v>> = Vec::new();

        for sup in supers.iter() {
            if let Some(cls) = sup.downcast_ref(strand.builtin_types().class_object) {
                let cls = cls.get();
                // Inherit parent's native supers (dedup by repr_eq).
                for type_obj in cls.native_supers.iter() {
                    if native_supers.iter().any(|s| s.repr_eq(strand, type_obj)) {
                        continue;
                    }
                    native_supers.push(type_obj.dup());
                }
                // Merge parent's entries (left-wins). Abstract entries copy the value directly.
                for (sym, entry) in cls.entries.iter() {
                    if entry_map.contains_key(sym) {
                        continue; // left wins
                    }
                    let new_entry = match entry {
                        ClassEntry::Field(old_slot) => {
                            let default = cls.field_defaults[*old_slot].dup();
                            let new_slot = field_defaults.len();
                            field_defaults.push(default);
                            ClassEntry::Field(new_slot)
                        }
                        ClassEntry::Method(v) => ClassEntry::Method(v.dup()),
                        ClassEntry::Delegate(parent_slot) => {
                            // Remap via parent's native_supers → our native_supers.
                            let type_obj = &cls.native_supers[*parent_slot];
                            let our_slot = native_supers
                                .iter()
                                .position(|s| s.repr_eq(strand, type_obj))
                                .expect("bug: parent Delegate slot not found in our native_supers");
                            ClassEntry::Delegate(our_slot)
                        }
                        ClassEntry::Abstract(type_obj) => ClassEntry::Abstract(type_obj.dup()),
                    };
                    entry_map.insert(*sym, new_entry);
                }
            } else {
                // Direct native super. Skip if already seen (inherited via a ClassObject parent).
                if native_supers.iter().any(|s| s.repr_eq(strand, sup))
                    || seen_abstract.iter().any(|s| s.repr_eq(strand, sup))
                {
                    continue;
                }
                let inspect = sup.op_inspect(strand).ok_or_else(|| {
                    Error::type_error(strand, "inheritance not supported by superclass")
                })?;
                if inspect.is_abstract {
                    // Abstract super: store the type-object directly in each entry.
                    seen_abstract.push(sup.dup());
                    for sym in inspect.members.iter() {
                        let static_sym = unsafe { sym.into_static_scope_unchecked() };
                        entry_map
                            .entry(static_sym)
                            .or_insert_with(|| ClassEntry::Abstract(sup.dup()));
                    }
                } else {
                    // Concrete native super: members dispatched via instance native slot.
                    let our_slot = native_supers.len();
                    native_supers.push(sup.dup());
                    for sym in inspect.members.iter() {
                        let static_sym = unsafe { sym.into_static_scope_unchecked() };
                        entry_map
                            .entry(static_sym)
                            .or_insert(ClassEntry::Delegate(our_slot));
                    }
                }
            }
        }

        // --- Phase 3: Apply this class's own module entries (always win) ---
        let module = module
            .downcast_ref(strand.builtin_types().module)
            .ok_or_else(|| Error::type_error(strand, "class_create: expected module"))?;
        let module = module.get();
        let program = module.loaded.clone();
        for (sym, value) in module.entries() {
            let entry = if value
                .downcast_ref(strand.builtin_types().function)
                .is_some()
            {
                ClassEntry::Method(value)
            } else {
                let slot = field_defaults.len();
                field_defaults.push(value);
                ClassEntry::Field(slot)
            };
            entry_map.insert(sym, entry);
        }

        // --- Phase 4: Sort entries by sym ---
        let mut entries: Vec<(Sym<'v, 'static>, ClassEntry<'v>)> = entry_map.into_iter().collect();
        entries.sort_by_key(|(s, _)| *s);

        let class_obj = ClassObject {
            program,
            name,
            supers: supers.into(),
            entries: entries.into(),
            field_defaults: field_defaults.into(),
            native_supers: native_supers.into(),
        };

        out.store(Value::from_object(GcObj::new(
            strand.arena(),
            strand.builtin_types().class_object,
            class_obj,
        )));

        Ok(())
    }

    #[inline(never)]
    async fn guard<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        mut args: Args<'v, 'a>,
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        // Calling convention:
        //   arg 0: body closure (0 params)
        //   arg 1: catch-all closure (1 param) or nil
        //   arg 2: finally closure (0 params) or nil
        //   args 3..N (pairs): class_value, handler_closure (1 param)

        // Read fixed args
        let (body, catch_all, finally) = match (args.next(), args.next(), args.next()) {
            (Some(Arg::Pos(body)), Some(Arg::Pos(catch_all)), Some(Arg::Pos(finally))) => {
                (body, catch_all, finally)
            }
            _ => return Err(Error::runtime(strand, "guard: invalid argument count")),
        };
        strand
            .with_slots(async |strand, [mut tmp]| {
                // 1. Call body
                let mut status = call!(strand, &body, &mut out).await;
                let _handled_backtrace = match &mut status {
                    Err(err) if err.catchable() => Some(
                        strand
                            .inner
                            .push_handled_backtrace(err.clone_backtrace(strand)),
                    ),
                    _ => None,
                };

                // 2. On catchable error, try handlers
                status = match status {
                    Ok(()) => Ok(()),
                    Err(mut err) if err.catchable() => {
                        err.get_value(strand, &mut tmp);

                        let mut cause = Some(err);
                        let res = loop {
                            let (ty, handler) = match (args.next(), args.next()) {
                                (None, None) => {
                                    break if !catch_all.is_nil() {
                                        call!(strand, &catch_all, &mut out, &tmp).await
                                    } else {
                                        Err(cause.take().unwrap())
                                    };
                                }
                                (Some(Arg::Pos(c)), Some(Arg::Pos(h))) => (c, h),
                                _ => {
                                    return Err(Error::runtime(
                                        strand,
                                        "guard: invalid argument count",
                                    ));
                                }
                            };
                            if tmp.is_instance_of(strand, &ty) {
                                break call!(strand, &handler, &mut out, &tmp).await;
                            }
                        };

                        if let Err(err2) = res {
                            if let Some(cause) = cause {
                                Err(err2.caused_by(strand, cause))
                            } else {
                                Err(err2)
                            }
                        } else {
                            res
                        }
                    }
                    err => err,
                };

                // 3. Run finally under cancel mask
                if !finally.is_nil() {
                    let finally_status = strand
                        .with_cancel_mask(true, async |strand| {
                            call!(strand, &finally, &mut tmp).await
                        })
                        .await;
                    match (status, finally_status) {
                        (Ok(()), Ok(())) => Ok(()),
                        (Err(e), Ok(())) | (_, Err(e)) => Err(e),
                    }
                } else {
                    status
                }
            })
            .await
    }

    pub(crate) fn check_interrupt<'s>(&self, strand: &Strand<'v, 's>) -> Result<'v, 's, ()> {
        if let Some(int) = self.interrupt.as_ref() {
            int(strand)?
        }
        Ok(())
    }

    pub(crate) fn check_interrupt_gc<'s>(&self, strand: &Strand<'v, 's>) -> Result<'v, 's, ()> {
        if self.arena.collect() {
            self.sym_gc();
        }
        self.check_interrupt(strand)
    }

    async unsafe fn expand_args<'s>(
        inner: &'s StrandInner<'v>,
        frame: &CallFrame<'v>,
        slot: &Value<'v>,
    ) -> Result<'v, 's, ()> {
        unsafe {
            Strand::async_for_frame(inner, frame, async |strand| {
                struct ArgSpread<'b, 'v> {
                    items: &'b mut Vec<OwnedItem<'v>>,
                }

                impl<'b, 'v, 's> Spread<'v, 's> for ArgSpread<'b, 'v> {
                    fn positional(
                        &mut self,
                        _strand: &mut Strand<'v, 's>,
                        mut value: Slot<'v, '_>,
                    ) -> Result<'v, 's, ()> {
                        self.items.push((None, UnsafeCell::new(value.take())));
                        Ok(())
                    }

                    fn symbol(
                        &mut self,
                        strand: &mut Strand<'v, 's>,
                        key: Sym<'v, '_>,
                        mut value: Slot<'v, '_>,
                    ) -> Result<'v, 's, ()> {
                        self.items
                            .push((Some(strand.sym_obj(key)), UnsafeCell::new(value.take())));
                        Ok(())
                    }

                    fn keyed(
                        &mut self,
                        strand: &mut Strand<'v, 's>,
                        key: Slot<'v, '_>,
                        mut value: Slot<'v, '_>,
                    ) -> Result<'v, 's, ()> {
                        if let Some(sym) = key.as_sym(strand) {
                            self.items
                                .push((Some(strand.sym_obj(sym)), UnsafeCell::new(value.take())));
                            Ok(())
                        } else {
                            Err(Error::unexpected_key(strand, key))
                        }
                    }
                }

                let mut sink = ArgSpread {
                    items: frame.items(),
                };
                slot.op_spread(strand, SpreadContext::Args, &mut sink).await
            })
            .await?;
        }
        Ok(())
    }

    fn throw<'a, 's>(
        &self,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([value], []) = unpack!(strand, args, 1, 0)?;
        Err(Error::from_value(strand, value))
    }

    #[inline(never)]
    async unsafe fn marshal_var_args<'a, 's>(
        inner: &'s StrandInner<'v>,
        frame: &'a CallFrame<'v>,
        slice: &[UnsafeCell<Value<'v>>],
        args: &Vec<sig::Arg<'v, '_>>,
        headroom: usize,
    ) -> Result<'v, 's, Args<'v, 'a>> {
        unsafe {
            for _ in 0..headroom {
                frame.items().push((None, UnsafeCell::new(Value::NIL)));
            }
            for (slot, arg) in slice[headroom..].iter().zip(args.iter()) {
                match arg {
                    sig::Arg::Pos => frame
                        .items()
                        .push((None, UnsafeCell::new((*slot.get()).take()))),
                    sig::Arg::Key(sym) => frame.items().push((
                        Some(inner.vm().sym_obj(*sym)),
                        UnsafeCell::new((*slot.get()).take()),
                    )),
                    sig::Arg::Expand => Self::expand_args(inner, frame, &*slot.get()).await?,
                }
            }
            Ok(Args::new_owned(frame.items(), headroom))
        }
    }

    #[inline]
    async unsafe fn marshal_args<'a, 's>(
        inner: &'s StrandInner<'v>,
        frame: &'a CallFrame<'v>,
        sig: &'a Pack<'v, 'a>,
        headroom: usize,
    ) -> Result<'v, 's, Args<'v, 'a>> {
        unsafe {
            let depth = frame.sp.get();
            let slice = frame
                .slots
                .get_unchecked(depth - sig.len() - headroom..depth);
            match sig {
                Pack::Fixed(syms) => Ok(Args::new(slice, syms, headroom)),
                Pack::Var(args) => {
                    Self::marshal_var_args(inner, frame, slice, args, headroom).await
                }
            }
        }
    }

    /// Execute a single bytecode instruction.
    ///
    /// ## Safety
    ///
    /// This function uses unsafe code extensively because it:
    /// - Decodes bytecode directly without bounds checking (relies on verifier)
    /// - Manipulates raw pointers into the value stack
    ///
    /// The safety invariants are:
    /// - `frame` must be valid and have sufficient stack space for the operation
    /// - `reader` must be positioned at a valid instruction opcode
    /// - The bytecode must have passed verification
    /// - Decoding instructions must be correct to avoid desyncing the reader
    /// - Scratch slots must be properly managed (see below)
    ///
    /// # Scratch Slot Management
    ///
    /// Many operations use scratch slots (temporary storage in the frame) to hold
    /// values across await points. The pattern is:
    /// 1. Store value in scratch slot before async call
    /// 2. Perform async operation
    /// 3. Clear scratch slot after use
    ///
    /// This ensures values are scannable by the cycle collector during suspension.
    async unsafe fn step<'s>(
        &self,
        inner: &'s StrandInner<'v>,
        frame: &mut CallFrame<'v>,
        reader: &mut UnsafeInstDecoder<'_>,
    ) -> Result<'v, 's, Status<'v>> {
        use Opcode::*;
        let symtab = &frame.program.symtab;
        let consttab = &frame.program.consttab;

        // SAFETY: This large unsafe block is necessary for performance reasons:
        // - Bytecode decoding uses unchecked accesses
        // - Bounds checking (on operand stack, upvars, etc.) is avoided where possible
        // - Runtime borrow checking is avoided
        //
        // The verifier ensures:
        // - All reachable instruction offsets are valid
        // - Operand stack depth is always in bounds
        // - Local variable indices are in bounds
        unsafe {
            match reader.opcode() {
                Pop => {
                    mem::drop(frame.pop());
                }
                Dup => frame.dup(),
                Swap => {
                    let i = reader.usize();
                    let j = reader.usize();
                    frame.swap(i, j);
                }
                LoadConst => {
                    let index = reader.usize();
                    frame.push(consttab.get_unchecked(index).dup())
                }
                LoadLocal => {
                    let index = reader.usize();
                    let value = frame.load(index);
                    frame.push(value)
                }
                LoadUpvar => {
                    let index = reader.usize();
                    let depth = reader.usize();
                    let value = Upvars::get_unchecked(&frame.upvars, index, depth);
                    frame.push(value)
                }
                StoreLocal => {
                    let index = reader.usize();
                    let value = frame.pop();
                    frame.store(index, value)
                }
                StoreUpvar => {
                    let index = reader.usize();
                    let depth = reader.usize();
                    let value = frame.pop();
                    Upvars::set_unchecked(&frame.upvars, index, depth, value);
                }
                Get => {
                    frame.pc = reader.offset();
                    let index = reader.usize();
                    let sym = *symtab.get_unchecked(index);
                    let mut obj = frame.scratch1();
                    let mut res = frame.scratch2();
                    obj.store(frame.pop());
                    Strand::for_frame(inner, frame, |strand| {
                        obj.op_get(strand, sym, Slot::reborrow(&mut res))
                    })?;
                    frame.push(res.take());
                    obj.store(Value::NIL);
                }
                Set => {
                    frame.pc = reader.offset();
                    let index = reader.usize();
                    let sym = *symtab.get_unchecked(index);
                    let mut value = frame.scratch1();
                    let mut obj = frame.scratch2();
                    value.store(frame.pop());
                    obj.store(frame.pop());
                    Strand::for_frame(inner, frame, |strand| {
                        obj.op_set(strand, sym, Slot::reborrow(&mut value))
                    })?;
                    value.store(Value::NIL);
                    obj.store(Value::NIL);
                }
                Index => {
                    frame.pc = reader.offset();
                    let mut index = frame.scratch1();
                    index.store(frame.pop());
                    let mut obj = frame.scratch2();
                    obj.store(frame.pop());
                    let mut res = frame.scratch3();
                    Strand::for_frame(inner, frame, |strand| {
                        obj.op_index(strand, &index, Slot::reborrow(&mut res))
                    })?;
                    frame.push(res.take());
                    index.store(Value::NIL);
                    obj.store(Value::NIL);
                }
                Assign => {
                    frame.pc = reader.offset();
                    let mut value = frame.scratch1();
                    let mut index = frame.scratch2();
                    let mut obj = frame.scratch3();
                    value.store(frame.pop());
                    index.store(frame.pop());
                    obj.store(frame.pop());
                    Strand::for_frame(inner, frame, |strand| {
                        obj.op_assign(
                            strand,
                            Slot::reborrow(&mut index),
                            Slot::reborrow(&mut value),
                        )
                    })?;
                    value.store(Value::NIL);
                    index.store(Value::NIL);
                    obj.store(Value::NIL);
                }
                PushUpvars => {
                    let count = reader.usize();
                    frame.upvars = Some(Gc::new(
                        self.arena(),
                        Upvars {
                            parent: frame.upvars.clone(),
                            vars: (0..count).map(|_| Value::NIL).collect(),
                            stale: false,
                        },
                    ))
                }
                PopUpvars => {
                    frame.upvars = frame
                        .upvars
                        .take()
                        .unwrap()
                        .borrow()
                        .unwrap()
                        .parent
                        .clone()
                }
                Call => {
                    // Set current PC in frame header in case a backtrace is needed
                    frame.pc = reader.offset();
                    let index = reader.usize();
                    let sig = frame.program.packtab.get_unchecked(index);
                    let count = sig.len() + 1;
                    let depth = frame.sp.get();
                    let mut func = frame.scratch1();
                    let mut res = frame.scratch2();
                    func.store(frame.take(depth - count));
                    let args = Self::marshal_args(inner, frame, sig, 1).await?;
                    Strand::async_for_frame(inner, frame, async |strand| {
                        self.check_interrupt_gc(strand)?;
                        func.op_call(strand, args, Slot::reborrow(&mut res)).await
                    })
                    .await?;
                    func.store(Value::NIL);
                    frame.items().clear();
                    frame.discard(count);
                    frame.push(res.take());
                }
                MethodCall => {
                    frame.pc = reader.offset();
                    let sym = reader.usize();
                    let index = reader.usize();
                    let method = *symtab.get_unchecked(sym);
                    let sig = frame.program.packtab.get_unchecked(index);
                    let count = sig.len() + 1;
                    let depth = frame.sp.get();
                    let mut obj = frame.scratch1();
                    let mut res = frame.scratch2();
                    obj.store(frame.take(depth - count));
                    let args = Self::marshal_args(inner, frame, sig, 1).await?;
                    Strand::async_for_frame(inner, frame, async |strand| {
                        self.check_interrupt_gc(strand)?;
                        obj.op_mcall(strand, method, args, Slot::reborrow(&mut res))
                            .await
                    })
                    .await?;
                    frame.discard(count);
                    frame.push(res.take());
                    frame.items().clear();
                    obj.store(Value::NIL);
                }
                Builtin => {
                    frame.pc = reader.offset();
                    let index = reader.usize();
                    let sig = reader.usize();
                    let sig = frame.program.packtab.get_unchecked(sig);
                    let count = sig.len();
                    let args = Self::marshal_args(inner, frame, sig, 0).await?;
                    let mut res = frame.scratch1();
                    Strand::async_for_frame(inner, frame, async |strand| match index {
                        builtin::IMPORT => {
                            self.import(strand, args, Slot::reborrow(&mut res)).await
                        }
                        builtin::ARRAY => self.array(strand, args, Slot::reborrow(&mut res)).await,
                        builtin::DICT => self.dict(strand, args, Slot::reborrow(&mut res)).await,
                        builtin::ITER => self.iter(strand, args, Slot::reborrow(&mut res)).await,
                        builtin::CONCAT_STR => {
                            self.concat_str(strand, args, Slot::reborrow(&mut res))
                        }
                        builtin::CONCAT_ARG => {
                            self.concat_arg(strand, args, Slot::reborrow(&mut res))
                        }
                        builtin::ARGS => self.args(strand, args, Slot::reborrow(&mut res)),
                        builtin::CLASS_CREATE => {
                            self.class_create(strand, args, Slot::reborrow(&mut res))
                        }
                        builtin::GUARD => self.guard(strand, args, Slot::reborrow(&mut res)).await,
                        builtin::THROW => self.throw(strand, args),
                        builtin::CONCAT_BIN => {
                            self.concat_bin(strand, args, Slot::reborrow(&mut res))
                        }
                        _ => unreachable_unchecked(),
                    })
                    .await?;
                    frame.discard(count);
                    frame.push(res.take());
                    frame.items().clear();
                }
                inst @ (Neg | Not | BitNot) => {
                    frame.pc = reader.offset();
                    let slot = frame.pop();
                    frame.pc = reader.offset();
                    let res = Strand::for_frame(inner, frame, |strand| match inst {
                        Neg => slot.op_neg(strand),
                        Not => slot.op_not(strand),
                        BitNot => slot.op_bnot(strand),
                        _ => unreachable_unchecked(),
                    })?;
                    frame.push(res);
                }
                Close => {
                    let index = reader.usize();
                    let value = Strand::for_frame_infallible(inner, frame, |strand| {
                        Value::function(strand, frame.program.clone(), frame.upvars.clone(), index)
                    });
                    frame.push(value);
                }
                inst @ (BitAnd | BitOr | BitXor | Add | Sub | Mul | Div | Ediv | Mod | Eq | Ne
                | Lt | Gt | Lte | Gte) => {
                    frame.pc = reader.offset();
                    let mut left = frame.scratch1();
                    let mut right = frame.scratch2();
                    right.store(frame.pop());
                    left.store(frame.pop());
                    let res = Strand::for_frame(inner, frame, |strand| match inst {
                        BitAnd => left.op_band(strand, &right),
                        BitOr => left.op_bor(strand, &right),
                        BitXor => left.op_bxor(strand, &right),
                        Add => left.op_add(strand, &right),
                        Div => left.op_div(strand, &right),
                        Ediv => left.op_ediv(strand, &right),
                        Mod => left.op_mod(strand, &right),
                        Mul => left.op_mul(strand, &right),
                        Sub => left.op_sub(strand, &right),
                        Eq => Ok(left.op_eq(strand, &right)),
                        Ne => Ok(left.op_ne(strand, &right)),
                        Lt => left.op_lt(strand, &right),
                        Gt => left.op_gt(strand, &right),
                        Lte => left.op_lte(strand, &right),
                        Gte => left.op_gte(strand, &right),
                        _ => unreachable_unchecked(),
                    })?;
                    frame.push(res)
                }
                Ret => return Ok(Status::Ret(frame.pop())),
                Branch => {
                    let offset = reader.isize();
                    if offset < 0 {
                        frame.pc = reader.offset();
                        Strand::for_frame(inner, frame, |strand| self.check_interrupt_gc(strand))?
                    }
                    reader.seek(offset)
                }
                op @ (BranchTrue | BranchFalse) => {
                    frame.pc = reader.offset();
                    let offset = reader.isize();
                    let mut slot = frame.scratch1();
                    slot.store(frame.pop());
                    if offset < 0 {
                        Strand::for_frame(inner, frame, |strand| self.check_interrupt_gc(strand))?
                    }
                    let value =
                        Strand::for_frame_infallible(inner, frame, |strand| slot.op_bool(strand));
                    slot.store(Value::NIL);
                    if (op == BranchTrue) == value {
                        reader.seek(offset)
                    }
                }
                Reify => {
                    let index = reader.usize();
                    let sig = frame.program.packtab.get_unchecked(index);
                    let upvars = frame.upvars.take().unwrap();
                    frame.upvars = upvars.borrow().unwrap().parent.clone();
                    let value = Module::from_upvars_syms(
                        frame.program.clone(),
                        upvars,
                        match sig {
                            Pack::Fixed(syms) => syms.iter().cloned(),
                            Pack::Var(_) => unreachable!(),
                        },
                    );
                    frame.push(Value::from_object(GcObj::new(
                        inner.vm().arena(),
                        self.builtin_types().module,
                        value,
                    )));
                }
                Next => {
                    frame.pc = reader.offset();
                    let mut value = frame.scratch1();
                    let mut out = frame.scratch2();
                    value.store(frame.pop());
                    let flag = Strand::async_for_frame(inner, frame, async |strand| {
                        value.op_next(strand, Slot::reborrow(&mut out)).await
                    })
                    .await?;
                    frame.push(out.take());
                    frame.push(Value::from_bool(flag));
                    value.store(Value::NIL);
                }
                Unpack => {
                    // Set current PC in frame header in case a backtrace is needed
                    frame.pc = reader.offset();
                    let index = reader.usize();
                    let value = frame.pop();
                    let sig = frame.program.unpacktab.get_unchecked(index);
                    let depth = frame.sp.get();
                    let slice = frame.slots.get_unchecked(depth..depth + sig.len());
                    frame.sp.update(|s| s + sig.len());
                    Strand::async_for_frame(inner, frame, async |strand| {
                        value.op_unpack(strand, sig, Slots::new(slice)).await
                    })
                    .await?;
                }
                // Non-local branch: jump to a target frame outside the current call stack.
                // Used to implement break/continue/return across closure boundaries.
                //
                // The `indicator` value tells the target what kind of jump this is:
                // - 0: break
                // - 1: continue
                // - 2: return
                NlBranch => {
                    let depth = reader.usize();
                    let indicator = reader.usize() as u8;
                    let target = Upvars::at_depth(&frame.upvars, depth);
                    if target.borrow().unwrap_unchecked().stale {
                        return Err(Error::runtime_raw(inner, "stale non-local branch"));
                    }
                    let weak = Gc::downgrade(&target);
                    return Err(Error::non_local_jump(indicator, weak));
                }
                // Non-local guard: creates a closure to mark an NlBranch boundary and
                // immediately invokes it, catching an NlBranch exception.
                //
                // Stack effect: ... -> ... result indicator
                // - If the guarded function returns normally: pushes result, NIL indicator
                // - If an NlBranch is caught: pushes NIL result, indicator value
                NlGuard => {
                    frame.pc = reader.offset();
                    // Push marker upvar frame used as NlBranch target
                    frame.upvars = Some(Gc::new(
                        self.arena(),
                        Upvars {
                            parent: frame.upvars.clone(),
                            vars: Default::default(),
                            stale: false,
                        },
                    ));
                    let index = reader.usize();
                    let mut func = frame.scratch1();
                    let mut res = frame.scratch2();
                    // Create and call the closure that will execute the guarded code
                    let call_result = Strand::async_for_frame(inner, frame, async |strand| {
                        func.store(Value::function(
                            strand,
                            frame.program.clone(),
                            frame.upvars.clone(),
                            index,
                        ));
                        func.op_call(strand, Args::new(&[], &[], 0), Slot::reborrow(&mut res))
                            .await
                    })
                    .await;
                    func.store(Value::NIL);
                    let upvars = frame.upvars.as_ref().unwrap_unchecked();
                    // Mark this guard as stale so future jumps to it fail
                    upvars.borrow_mut().unwrap_unchecked().stale = true;
                    // Check if this was a normal return or a caught NlBranch
                    let res = match call_result {
                        Ok(()) => {
                            frame.push(res.take());
                            frame.push(Value::NIL);
                            Ok(())
                        }
                        Err(err) => {
                            if let Some((indicator, weak)) = err.as_nl_branch()
                                && weak.ptr_eq_strong(upvars)
                            {
                                frame.push(Value::NIL);
                                frame.push(Value::from_i64(inner.vm(), indicator as i64));
                                Ok(())
                            } else {
                                Err(err)
                            }
                        }
                    };
                    // Pop the guard's upvar frame
                    frame.upvars = frame
                        .upvars
                        .take()
                        .unwrap()
                        .borrow()
                        .unwrap()
                        .parent
                        .clone();
                    res?
                }
            }
        }

        Ok(Status::Running)
    }

    pub(crate) async fn run<'s>(
        &self,
        inner: &'s StrandInner<'v>,
        frame: &mut CallFrame<'v>,
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let _depth_guard = inner.push_call_depth()?;
        let loaded = frame.program.clone();
        let mut reader =
            UnsafeInstDecoder::new(&loaded.bytecode[loaded.funcs[frame.func].0.bytecode.clone()]);
        loop {
            match unsafe { self.step(inner, frame, &mut reader) }.await? {
                Status::Ret(value) => {
                    out.store(value);
                    break Ok(());
                }
                Status::Running => (),
            }
        }
    }
}
