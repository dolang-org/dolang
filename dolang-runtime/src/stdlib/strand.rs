use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    pin::Pin,
    rc::Rc,
    task::{self, Poll, Waker},
};

use futures::future::join_all;

use crate::{
    arg::Arg,
    call,
    error::{Error, ErrorKind, Result},
    method,
    object::{
        array::Array,
        backtrace, channel,
        native::{Object, Type, TypeBuilder},
        tuple,
    },
    strand::{InterruptToken, Local, LocalKey, LocalRootKey, Redirect, Strand},
    unpack,
    value::{Empty, Output, Slot, Value},
    vm::{Builder, State, Stateful},
};

/// Creates a channel pair using the VM's registered factory if set,
/// otherwise falls back to the default internal channel implementation.
fn pipe_pair<'v>(
    strand: &mut Strand<'v, '_>,
    mut send: impl Output<'v>,
    mut recv: impl Output<'v>,
) {
    if let Some(pipe) = strand.vm().pipe_handler.as_ref() {
        pipe(
            strand,
            Slot::from_output(&mut send),
            Slot::from_output(&mut recv),
        );
    } else {
        let vm = strand.vm();
        let (s, r) = channel::pair(vm, 1);
        Slot::from_output(&mut send).store(Value::from_object(s));
        Slot::from_output(&mut recv).store(Value::from_object(r));
    }
}

struct ForkLimitScope {
    limit: usize,
    active: Cell<usize>,
    next_id: Cell<u64>,
    waiters: RefCell<VecDeque<(u64, Waker)>>,
    parent: Option<Rc<ForkLimitScope>>,
}

impl ForkLimitScope {
    fn new(limit: usize, parent: Option<Rc<ForkLimitScope>>) -> Rc<Self> {
        Rc::new(Self {
            limit,
            next_id: Cell::new(0),
            active: Cell::new(0),
            waiters: Default::default(),
            parent,
        })
    }

    fn next_id(&self) -> u64 {
        let id = self.next_id.get();
        self.next_id.set(id.strict_add(1));
        id
    }

    fn try_acquire(&self, waker: &Waker) -> Option<u64> {
        if self.active.get() < self.limit {
            self.active.set(self.active.get() + 1);
            None
        } else {
            let id = self.next_id();
            self.waiters.borrow_mut().push_back((id, waker.clone()));
            Some(id)
        }
    }

    fn nevermind(&self, id: u64) {
        self.waiters.borrow_mut().retain(|(i, _)| *i != id)
    }

    fn acquire_fresh(&self) {
        debug_assert_eq!(self.active.get(), 0);
        self.active.set(1);
    }

    fn release_one(&self) {
        let active = self.active.get();
        debug_assert!(active > 0);
        self.active.set(active - 1);
        if let Some((_, waker)) = self.waiters.borrow_mut().pop_front() {
            waker.wake();
        }
    }
}

impl Drop for ForkLimitScope {
    fn drop(&mut self) {
        assert!(self.waiters.get_mut().is_empty());
    }
}

struct PermitChain {
    scopes: Vec<Rc<ForkLimitScope>>,
}

impl PermitChain {
    fn release_all(&self) {
        for scope in self.scopes.iter().rev() {
            scope.release_one();
        }
    }
}

struct ForkLimitLocal {
    scope: RefCell<Option<Rc<ForkLimitScope>>>,
    permit: RefCell<Option<PermitChain>>,
}

impl ForkLimitLocal {
    fn current_scope(&self) -> Option<Rc<ForkLimitScope>> {
        self.scope.borrow().clone()
    }

    fn replace_scope(&self, scope: Option<Rc<ForkLimitScope>>) -> Option<Rc<ForkLimitScope>> {
        self.scope.replace(scope)
    }

    fn take_permit(&self) -> Option<PermitChain> {
        self.permit.borrow_mut().take()
    }

    fn store_permit(&self, permit: PermitChain) {
        *self.permit.borrow_mut() = Some(permit);
    }

    fn has_permit(&self) -> bool {
        self.permit.borrow().is_some()
    }

    fn push_permit_scope(&self, scope: Rc<ForkLimitScope>) {
        let mut permit = self.permit.borrow_mut();
        let permit = permit.as_mut().expect("fork worker missing permit");
        scope.acquire_fresh();
        permit.scopes.push(scope);
    }

    fn pop_permit_scope(&self) {
        let mut permit_ref = self.permit.borrow_mut();
        let permit = permit_ref.as_mut().expect("fork worker missing permit");
        let scope = permit.scopes.pop().expect("permit chain missing scope");
        scope.release_one();
        let empty = permit.scopes.is_empty();
        drop(permit_ref);
        if empty {
            self.permit.borrow_mut().take();
        }
    }
}

impl<'v> Local<'v> for ForkLimitLocal {
    fn init() -> Self {
        Self {
            scope: RefCell::new(None),
            permit: RefCell::new(None),
        }
    }

    fn inherit(&self, _strand: &Strand<'v, '_>) -> Self {
        Self {
            scope: RefCell::new(self.current_scope()),
            permit: RefCell::new(None),
        }
    }
}

struct StrandLocalData {
    cow: Cell<bool>,
}

impl<'v> Local<'v> for StrandLocalData {
    fn init() -> Self {
        Self {
            cow: Cell::new(false),
        }
    }

    fn inherit(&self, _strand: &Strand<'v, '_>) -> Self {
        self.cow.set(true);
        Self {
            cow: Cell::new(true),
        }
    }
}

struct StrandTypes<'v> {
    key: Type<'v, Key>,
}

struct StrandState<'v> {
    fork_limit: LocalKey<'v, ForkLimitLocal>,
    local: LocalKey<'v, StrandLocalData>,
    local_root: LocalRootKey<'v>,
    types: StrandTypes<'v>,
}

struct StrandStateTag;

impl<'v> Stateful<'v> for StrandState<'v> {
    type Tag = StrandStateTag;
}

struct Key;

impl<'v> Object<'v> for Key {
    const NAME: &'v str = "Key";
    const MODULE: &'v str = "strand";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: crate::arg::Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([], []) = unpack!(strand, args, 0, 0)?;
        this.create(strand, Key, out);
        Ok(())
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get_with_slots("value", |this, strand, out, [mut key]| {
                let state = strand.state::<StrandState<'v>>();
                Output::set(strand, &mut key, this);
                let root = state.local_root.slot(strand);
                if root.is_nil() {
                    return Ok(());
                }
                let dict = root.as_dict(strand).expect("strand local store is a dict");
                let _ = dict.get(strand, &key, None, out)?;
                Ok(())
            })
            .set_with_slots("value", |this, strand, mut value, [mut key]| {
                let state = strand.state::<StrandState<'v>>();
                Output::set(strand, &mut key, this);
                let root = unique_local_root(state, strand)?;
                let dict = root
                    .as_dict(strand)
                    .expect("strand local store is not a dict");
                dict.insert(strand, &mut key, &mut value, true)?;
                Ok(())
            })
    }
}

fn clone_local_root<'v, 's>(
    state: State<'v, StrandState<'v>>,
    strand: &mut Strand<'v, 's>,
) -> Slot<'v, 's> {
    strand.with_slots_sync(|strand, [mut copied, mut key, mut value]| {
        let mut root = state.local_root.slot(strand);
        if root.is_nil() {
            Output::set(strand, &mut root, Empty::Dict);
            return root;
        }
        Output::set(strand, &mut copied, Empty::Dict);
        let prior_dict = root.as_dict(strand).expect("strand local store is a dict");
        let copied_dict = copied.as_dict(strand).unwrap();
        let mut pairs = prior_dict.pairs();
        while pairs
            .next(strand, &mut key, &mut value)
            .expect("strand local dict is not accessed concurrently")
        {
            copied_dict
                .insert(strand, &mut key, &mut value, false)
                .unwrap();
        }
        Output::set(strand, &mut root, copied);
        root
    })
}

fn unique_local_root<'v, 's>(
    state: State<'v, StrandState<'v>>,
    strand: &mut Strand<'v, 's>,
) -> Result<'v, 's, Slot<'v, 's>> {
    let local = state.local.get(strand);
    if local.cow.replace(false) {
        return Ok(clone_local_root(state, strand));
    }
    let mut root = state.local_root.slot(strand);
    if root.is_nil() {
        Output::set(strand, &mut root, Empty::Dict);
    }
    Ok(root)
}

fn flatten_scope_chain(scope: Option<Rc<ForkLimitScope>>) -> Vec<Rc<ForkLimitScope>> {
    let mut scopes = Vec::new();
    let mut current = scope;
    while let Some(scope) = current {
        current = scope.parent.clone();
        scopes.push(scope);
    }
    scopes.reverse();
    scopes
}

struct Acquire<'a> {
    scopes: &'a [Rc<ForkLimitScope>],
    acquired: usize,
    id: Option<u64>,
}

impl<'a> Future for Acquire<'a> {
    type Output = PermitChain;

    fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        for scope in &self.scopes[self.acquired..] {
            if let Some(id) = self.id.take() {
                scope.nevermind(id);
            }
            if let Some(id) = scope.try_acquire(cx.waker()) {
                self.id = Some(id);
                return Poll::Pending;
            } else {
                self.acquired += 1;
            }
        }
        self.acquired = 0;
        Poll::Ready(PermitChain {
            scopes: self.scopes.to_vec(),
        })
    }
}

impl<'a> Drop for Acquire<'a> {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            self.scopes[self.acquired].nevermind(id)
        }
        for scope in self.scopes[..self.acquired].iter().rev() {
            scope.release_one();
        }
    }
}

fn acquire_scopes(scopes: &[Rc<ForkLimitScope>]) -> Acquire<'_> {
    Acquire {
        scopes,
        acquired: 0,
        id: None,
    }
}

async fn acquire_permit<'v, 's>(
    strand: &mut Strand<'v, 's>,
    state: State<'v, StrandState<'v>>,
) -> Result<'v, 's, ()> {
    let scopes = flatten_scope_chain(state.fork_limit.get(strand).current_scope());
    if scopes.is_empty() {
        return Ok(());
    }
    let permit = acquire_scopes(&scopes).await;
    state.fork_limit.get(strand).store_permit(permit);
    Ok(())
}

async fn restore_prior_permit<'v, 's>(
    strand: &mut Strand<'v, 's>,
    state: State<'v, StrandState<'v>>,
    prior: Option<PermitChain>,
) -> Result<'v, 's, ()> {
    let Some(prior) = prior else {
        return Ok(());
    };
    if prior.scopes.is_empty() {
        return Ok(());
    }
    let permit = acquire_scopes(&prior.scopes).await;
    state.fork_limit.get(strand).store_permit(permit);
    Ok(())
}

pub(crate) fn configure<'v>(builder: &mut Builder<'v>) {
    let fork_limit = builder.local();
    let local = builder.local();
    let local_root = builder.local_root();
    let key = builder.register_type();
    let state = builder.register_state(StrandState {
        fork_limit,
        local,
        local_root,
        types: StrandTypes { key },
    });
    let input_key = builder.sym("input");
    let output_key = builder.sym("output");
    let backtrace_key = builder.sym("backtrace");
    let close = builder.sym("close");
    let limit_key = builder.sym("limit");
    let default_key = builder.sym("default");
    let else_key = builder.sym("else");
    let strand_class = builder.singletons().strand.dup();
    let backtrace_class = builder.singletons().backtrace.dup();

    builder
        .module("strand")
        .value("Strand", &strand_class)
        .value("Backtrace", &backtrace_class)
        .value("Key", state.types.key)
        // Implicit I/O functions (moved from former std.iter)
        .function_with_slots("put", async move |strand, args, _, [mut tmp]| {
            let ([value], []) = unpack!(strand, args, 1, 0)?;
            strand.output(&mut tmp);
            tmp.put(strand, value).await
        })
        .function_with_slots("next", async move |strand, args, mut out, [mut tmp]| {
            let ([], [default, or_else]) =
                unpack!(strand, args, 0, 0, default_key = None, else_key = None)?;
            if default.is_some() && or_else.is_some() {
                return Err(Error::unexpected_key(strand, else_key));
            }
            strand.input(&mut tmp);
            let res = tmp.next(strand, &mut out).await?;
            if !res {
                if let Some(mut default) = default {
                    out.store(default.take());
                    return Ok(());
                } else if let Some(or_else) = or_else {
                    return call!(strand, or_else, out).await;
                }
                return Err(Error::iter_stop(strand));
            }
            Ok(())
        })
        .function("input", async move |strand, args, out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            strand.input(out);
            Ok(())
        })
        .function("output", async move |strand, args, out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            strand.output(out);
            Ok(())
        })
        .function_without_frame("backtrace", async move |strand, args, out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            let entries = strand.backtrace_entries();
            backtrace::create(strand, entries, out);
            Ok(())
        })
        .function_without_frame("error_backtrace", async move |strand, args, out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            let Some(mut entries) = strand.inner.handled_backtrace() else {
                return Err(Error::state_error(strand, "no active handled exception"));
            };
            entries.extend(strand.backtrace_entries());
            backtrace::create(strand, entries, out);
            Ok(())
        })
        .function_without_frame("throw", async move |strand, args, _| {
            let ([value], [backtrace]) = unpack!(strand, args, 1, 0, backtrace_key = None)?;
            let err = if let Some(backtrace) = backtrace {
                Error::from_value_backtrace(strand, value, backtrace)?
            } else {
                Error::from_value(strand, value)
            };
            Err(err)
        })
        .function_with_slots("redirect", async move |strand, mut args, out, [mut tmp]| {
            let mut arg_input = None;
            let mut arg_output = None;
            let block = loop {
                match args.next() {
                    None => return Err(Error::missing_positional(strand, 0)),
                    Some(Arg::Pos(slot)) => break slot,
                    Some(Arg::Key(sym, slot)) if sym == input_key => arg_input = Some(slot),
                    Some(Arg::Key(sym, slot)) if sym == output_key => arg_output = Some(slot),
                    Some(Arg::Key(sym, _)) => {
                        return Err(Error::unexpected_key(strand, sym));
                    }
                }
            };
            let mut redir = Redirect::new(strand);
            if let Some(input) = arg_input {
                input.iter(&mut redir, &mut tmp).await?;
                redir = redir.input(&mut tmp);
            }
            if let Some(output) = arg_output {
                output.sink(&mut redir, &mut tmp).await?;
                redir = redir.output(&mut tmp);
            }
            redir
                .enter(async move |strand| block.call(strand, args, out).await)
                .await
        })
        .function("channel", async move |strand, args, mut out| {
            let (_, [limit]) = unpack!(strand, args, 0, 1)?;
            let limit = match limit {
                None => 1,
                Some(limit) => limit.to_index(strand)?,
            };
            let (send, recv) = channel::pair(strand, limit);
            out.store(Value::from_object(tuple::tuple(
                strand,
                [Value::from_object(send), Value::from_object(recv)],
            )));
            Ok(())
        })
        .function("limit", async move |strand, args, mut out| {
            let ([limit, block], []) = unpack!(strand, args, 2, 0)?;
            let limit = limit.to_index(strand)?;
            if limit == 0 {
                return Err(Error::value(strand, "strand.limit: limit must be positive"));
            }
            let scope = ForkLimitScope::new(limit, state.fork_limit.get(strand).current_scope());
            let prev_scope = state
                .fork_limit
                .get(strand)
                .replace_scope(Some(scope.clone()));
            let had_permit = state.fork_limit.get(strand).has_permit();
            if had_permit {
                state.fork_limit.get(strand).push_permit_scope(scope);
            }
            let res = call!(strand, block, &mut out).await;
            if had_permit {
                state.fork_limit.get(strand).pop_permit_scope();
            }
            state.fork_limit.get(strand).replace_scope(prev_scope);
            res
        })
        .function("pipeline", async move |strand, args, out| {
            let mut thunks = Vec::new();
            let mut redir_input = None;
            let mut redir_output = None;
            for arg in args {
                match arg {
                    Arg::Pos(thunk) => thunks.push(thunk),
                    Arg::Key(sym, slot) if sym == input_key => redir_input = Some(slot),
                    Arg::Key(sym, slot) if sym == output_key => redir_output = Some(slot),
                    Arg::Key(sym, _) => return Err(Error::unexpected_key(strand, sym)),
                }
            }

            let count = thunks.len();
            if count == 0 {
                return Err(Error::missing_positional(strand, 0));
            }

            // We must avoid being dropped until we've awaited all strands we create
            strand
                .with_interrupt_mask(true, async move |strand| {
                    let prior_permit = state.fork_limit.get(strand).take_permit();
                    if let Some(permit) = prior_permit.as_ref() {
                        permit.release_all();
                    }

                    let result = async {
                        let mut last_recv = Value::NIL;
                        let mut out = Some(out);
                        let mut strands = Vec::new();
                        let mut pipes = Vec::with_capacity(count.saturating_sub(1));
                        let interrupt = strand.interrupt_token().nested();
                        for _ in 0..count.saturating_sub(1) {
                            let mut send = Value::NIL;
                            let mut recv = Value::NIL;
                            pipe_pair(strand, Slot::new(&mut send), Slot::new(&mut recv));
                            pipes.push(Some((send, recv)));
                        }
                        for (i, thunk) in thunks.into_iter().enumerate() {
                            let (send, recv, mut out, redir_output) = if i < count - 1 {
                                let (send, recv) = pipes[i].take().unwrap();
                                (Some(send), Some(recv), None, None)
                            } else {
                                (None, None, out.take(), redir_output.take())
                            };
                            let redir_input = if i == 0 { redir_input.take() } else { None };

                            strands.push(strand.spawn_scoped(
                                Some(interrupt.clone()),
                                async move |strand| {
                                    if let Err(e) = strand
                                        .with_slots(
                                            async move |strand, [mut s, mut r, mut tmp, mut tmp2]| {
                                                let mut redir = Redirect::new(strand);
                                                if !last_recv.is_nil() {
                                                    r.store(last_recv);
                                                    redir = redir.input(&*r);
                                                } else if let Some(redir_input) = redir_input {
                                                    redir_input.iter(&mut redir, &mut tmp).await?;
                                                    redir = redir.input(&mut tmp);
                                                }
                                                if let Some(send) = send {
                                                    s.store(send);
                                                    redir = redir.output(&*s);
                                                } else if let Some(redir_output) = redir_output {
                                                    redir_output.sink(&mut redir, &mut tmp).await?;
                                                    redir = redir.output(&mut tmp);
                                                }
                                                let res = redir
                                                    .enter(async move |strand| {
                                                        call!(
                                                            strand,
                                                            thunk,
                                                            out.as_mut().unwrap_or(&mut tmp)
                                                        )
                                                        .await
                                                    })
                                                    .await;
                                                if !r.is_nil() {
                                                    method!(strand, r, close, &mut tmp2).await?;
                                                }
                                                if !s.is_nil() {
                                                    method!(strand, s, close, &mut tmp2).await?;
                                                }
                                                res
                                            },
                                        )
                                        .await
                                        && !matches!(
                                            e.kind(),
                                            ErrorKind::IterStop | ErrorKind::SinkStop
                                        )
                                    {
                                        strand.interrupt_token().cancel();
                                        return Err(e);
                                    }
                                    Ok(())
                                },
                            ));
                            last_recv = recv.unwrap_or(Value::NIL);
                        }
                        for (i, res) in join_all(strands).await.into_iter().enumerate() {
                            if let Err(e) = res {
                                // Suppress Input/SinkStop errors (broken pipe) or cancellation from all
                                // but the final element
                                if i != count - 1 {
                                    if matches!(e.kind(), ErrorKind::IterStop | ErrorKind::SinkStop)
                                    {
                                        continue;
                                    }
                                    if interrupt.is_canceled() && e.kind() == ErrorKind::Canceled {
                                        continue;
                                    }
                                }
                                return Err(e);
                            }
                        }
                        Ok(())
                    }
                    .await;

                    restore_prior_permit(strand, state, prior_permit).await?;
                    result
                })
                .await
        })
        .function_with_slots(
            "each",
            async move |strand, args, _, [mut value, mut tmp, mut input, mut output]| {
                let ([func], _) = unpack!(strand, args, 1, 0)?;
                strand.input(&mut input);
                strand.output(&mut output);
                while input.next(strand, &mut value).await? {
                    call!(strand, &func, &mut tmp, &mut value).await?;
                    output.put(strand, &mut tmp).await?;
                    strand.check_trap_gc()?
                }
                Ok(())
            },
        )
        .function_with_slots(
            "where",
            async move |strand, args, _, [mut value, mut tmp, mut input, mut output]| {
                let ([pred], _) = unpack!(strand, args, 1, 0)?;
                strand.input(&mut input);
                strand.output(&mut output);
                while input.next(strand, &mut value).await? {
                    call!(strand, &pred, &mut tmp, &*value).await?;
                    if tmp.to_bool(strand) {
                        output.put(strand, &mut value).await?;
                    }
                    strand.check_trap_gc()?
                }
                Ok(())
            },
        )
        .function_with_slots(
            "from",
            async move |strand, args, _, [mut input, mut output, mut value]| {
                let ([arg], []) = unpack!(strand, args, 1, 0)?;
                arg.iter(strand, &mut input).await?;
                strand.output(&mut output);
                while input.next(strand, &mut value).await? {
                    output.put(strand, &mut value).await?;
                    strand.check_trap_gc()?
                }
                Ok(())
            },
        )
        .function_with_slots(
            "collect",
            async move |strand, args, mut out, [mut input, mut item, mut output]| {
                let ([], [arg]) = unpack!(strand, args, 0, 1)?;
                strand.input(&mut input);
                if let Some(mut arg) = arg {
                    arg.sink(strand, &mut output).await?;
                    while input.next(strand, &mut item).await? {
                        output.put(strand, &mut item).await?;
                        strand.check_trap_gc()?
                    }
                    out.store(arg.take());
                } else {
                    let mut acc = Vec::new();
                    while input.next(strand, &mut item).await? {
                        acc.push(item.take())
                    }
                    strand
                        .vm()
                        .builtin_types()
                        .array
                        .create(strand, Array { inner: acc }, out);
                }
                Ok(())
            },
        )
        .function("spawn", async move |strand, args, mut out| {
            let ([mut callable], []) = unpack!(strand, args, 1, 0)?;
            let handle =
                strand.spawn_background_raw(callable.take(), InterruptToken::new(), None)?;
            out.store(Value::from_object(handle));
            Ok(())
        })
        .function("stream", async move |strand, args, mut out| {
            let ([mut callable], []) = unpack!(strand, args, 1, 0)?;

            // Channel A: handle writes → strand reads
            // Channel B: strand writes → handle reads
            // Safety: stack Values are fresh, acyclic channel objects
            let mut input_sender = Value::NIL;
            let mut input_receiver = Value::NIL;
            pipe_pair(
                strand,
                Slot::new(&mut input_sender),
                Slot::new(&mut input_receiver),
            );
            let mut output_sender = Value::NIL;
            let mut output_receiver = Value::NIL;
            pipe_pair(
                strand,
                Slot::new(&mut output_sender),
                Slot::new(&mut output_receiver),
            );

            let handle = strand.spawn_background_raw(
                callable.take(),
                InterruptToken::new(),
                Some((input_receiver, output_sender)),
            )?;

            // Wire up the handle-facing channel ends
            {
                let mut h = handle
                    .borrow_mut()
                    .expect("fresh handle is always borrowable");
                h.stream_input = input_sender;
                h.stream_output = output_receiver;
            }

            out.store(Value::from_object(handle));
            Ok(())
        })
        .function("fork", async move |strand, args, out| {
            let mut thunks = Vec::new();
            let mut limit = None;
            for arg in args {
                match arg {
                    Arg::Pos(thunk) => thunks.push(thunk),
                    Arg::Key(sym, slot) if sym == limit_key => limit = Some(slot.to_index(strand)?),
                    Arg::Key(sym, _) => return Err(Error::unexpected_key(strand, sym)),
                }
            }

            let count = thunks.len();
            // We must avoid being dropped until we've awaited all strands we create
            strand
                .with_interrupt_mask(true, async move |strand| {
                    let prior_permit = state.fork_limit.get(strand).take_permit();
                    if let Some(permit) = prior_permit.as_ref() {
                        permit.release_all();
                    }

                    let result = async {
                        let results =
                            RefCell::new((0..count).map(|_| Value::NIL).collect::<Vec<_>>());
                        let work = RefCell::new(thunks.into_iter().enumerate());
                        let num_workers = limit.unwrap_or(count).min(count);
                        let mut strands = Vec::new();
                        let interrupt = strand.interrupt_token().nested();
                        for _ in 0..num_workers {
                            let results = &results;
                            let work = &work;
                            strands.push(strand.spawn_scoped(
                                Some(interrupt.clone()),
                                async move |strand| {
                                    while let Some((i, thunk)) = { work.borrow_mut().next() } {
                                        if let Err(e) = acquire_permit(strand, state).await {
                                            strand.interrupt_token().cancel();
                                            return Err(e);
                                        }
                                        let res = strand
                                            .with_slots(async move |strand, [mut tmp]| {
                                                call!(strand, thunk, &mut tmp).await?;
                                                results.borrow_mut()[i] = tmp.take();
                                                Ok(())
                                            })
                                            .await;
                                        if let Some(permit) =
                                            state.fork_limit.get(strand).take_permit()
                                        {
                                            permit.release_all();
                                        }
                                        if let Err(e) = res {
                                            strand.interrupt_token().cancel();
                                            return Err(e);
                                        }
                                    }
                                    Ok(())
                                },
                            ));
                        }
                        let mut first_err: Option<Error<'v, '_>> = None;
                        for res in join_all(strands).await {
                            if let Err(e) = res
                                && first_err.as_ref().is_none_or(|prev| {
                                    prev.kind() == ErrorKind::Canceled
                                        && e.kind() != ErrorKind::Canceled
                                })
                            {
                                first_err = Some(e);
                            }
                        }
                        if let Some(e) = first_err {
                            return Err(e);
                        }
                        Ok(results.into_inner())
                    }
                    .await;

                    restore_prior_permit(strand, state, prior_permit).await?;

                    let results = result?;
                    strand
                        .vm()
                        .builtin_types()
                        .array
                        .create(strand, Array { inner: results }, out);
                    Ok(())
                })
                .await
        })
        .commit();
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        task::{Context, Poll},
    };

    use futures::task::noop_waker;

    use super::{ForkLimitScope, acquire_scopes};

    #[test]
    fn spurious_acquire_poll_does_not_duplicate_waiter() {
        let scope = ForkLimitScope::new(1, None);
        scope.acquire_fresh();
        let scopes = [scope.clone()];
        let mut acquire = Box::pin(acquire_scopes(&scopes));
        let waker = noop_waker();
        let mut context = Context::from_waker(&waker);

        assert!(acquire.as_mut().poll(&mut context).is_pending());
        assert!(acquire.as_mut().poll(&mut context).is_pending());
        assert_eq!(scope.waiters.borrow().len(), 1);

        scope.release_one();
        let Poll::Ready(permit) = acquire.as_mut().poll(&mut context) else {
            panic!("released slot did not wake the acquire future");
        };
        permit.release_all();
        assert_eq!(scope.active.get(), 0);
    }

    #[test]
    fn limit_scope_wakes_waiters_in_arrival_order() {
        let scope = ForkLimitScope::new(1, None);
        scope.acquire_fresh();
        let waker = noop_waker();

        let first = scope.try_acquire(&waker).unwrap();
        let second = scope.try_acquire(&waker).unwrap();
        scope.release_one();

        let waiters = scope.waiters.borrow();
        assert_eq!(waiters.len(), 1);
        assert_eq!(waiters.front().unwrap().0, second);
        assert_ne!(first, second);
        drop(waiters);
        scope.nevermind(second);
    }

    #[test]
    fn dropping_partial_acquire_releases_outer_permits() {
        let outer = ForkLimitScope::new(1, None);
        let inner = ForkLimitScope::new(1, Some(outer.clone()));
        inner.acquire_fresh();
        let scopes = [outer.clone(), inner.clone()];
        let mut acquire = Box::pin(acquire_scopes(&scopes));
        let waker = noop_waker();
        let mut context = Context::from_waker(&waker);

        assert!(acquire.as_mut().poll(&mut context).is_pending());
        assert_eq!(outer.active.get(), 1);
        assert_eq!(inner.waiters.borrow().len(), 1);

        drop(acquire);
        assert_eq!(outer.active.get(), 0);
        assert!(inner.waiters.borrow().is_empty());
        inner.release_one();
    }
}
