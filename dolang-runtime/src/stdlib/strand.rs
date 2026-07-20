use std::{
    cell::{Cell, RefCell},
    collections::{HashSet, VecDeque},
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
    strand::{InheritKind, InterruptToken, Local, LocalKey, LocalRootKey, Redirect, Strand},
    unpack,
    value::{Empty, Output, Singleton, Slot, Value},
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

struct ResourceInner {
    id: u64,
    limit: usize,
    active: Cell<usize>,
    next_id: Cell<u64>,
    waiters: RefCell<VecDeque<(u64, Waker)>>,
}

impl ResourceInner {
    fn new(id: u64, limit: usize) -> Rc<Self> {
        Rc::new(Self {
            id,
            limit,
            next_id: Cell::new(0),
            active: Cell::new(0),
            waiters: Default::default(),
        })
    }

    fn next_id(&self) -> u64 {
        let id = self.next_id.get();
        self.next_id.set(id.strict_add(1));
        id
    }

    fn try_acquire(self: &Rc<Self>) -> Option<ResourcePermit> {
        if self.active.get() < self.limit && self.waiters.borrow().is_empty() {
            self.active.set(self.active.get() + 1);
            Some(ResourcePermit(self.clone()))
        } else {
            None
        }
    }

    fn release_one(&self) {
        let active = self.active.get();
        debug_assert!(active > 0);
        self.active.set(active - 1);
        if let Some((_, waker)) = self.waiters.borrow().front() {
            waker.wake_by_ref();
        }
    }
}

impl Drop for ResourceInner {
    fn drop(&mut self) {
        assert!(self.waiters.get_mut().is_empty());
    }
}

struct ResourcePermit(Rc<ResourceInner>);

impl Drop for ResourcePermit {
    fn drop(&mut self) {
        self.0.release_one()
    }
}

struct AcquireResource {
    resource: Rc<ResourceInner>,
    id: Option<u64>,
}

impl Future for AcquireResource {
    type Output = ResourcePermit;

    fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        if let Some(id) = self.id {
            let mut waiters = self.resource.waiters.borrow_mut();
            let is_front = waiters.front().is_some_and(|(waiter, _)| *waiter == id);
            if is_front && self.resource.active.get() < self.resource.limit {
                waiters.pop_front();
                self.resource.active.set(self.resource.active.get() + 1);
                if self.resource.active.get() < self.resource.limit
                    && let Some((_, waker)) = waiters.front()
                {
                    waker.wake_by_ref();
                }
                drop(waiters);
                self.id = None;
                return Poll::Ready(ResourcePermit(self.resource.clone()));
            }
            if let Some((_, waker)) = waiters.iter_mut().find(|(waiter, _)| *waiter == id) {
                waker.clone_from(cx.waker());
            }
            return Poll::Pending;
        }

        if let Some(permit) = self.resource.try_acquire() {
            return Poll::Ready(permit);
        }

        let id = self.resource.next_id();
        self.resource
            .waiters
            .borrow_mut()
            .push_back((id, cx.waker().clone()));
        self.id = Some(id);
        Poll::Pending
    }
}

impl Drop for AcquireResource {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            let mut waiters = self.resource.waiters.borrow_mut();
            let was_front = waiters.front().is_some_and(|(waiter, _)| *waiter == id);
            waiters.retain(|(waiter, _)| *waiter != id);
            if was_front
                && self.resource.active.get() < self.resource.limit
                && let Some((_, waker)) = waiters.front()
            {
                waker.wake_by_ref();
            }
        }
    }
}

fn acquire_resource(resource: Rc<ResourceInner>) -> AcquireResource {
    AcquireResource { resource, id: None }
}

struct HeldResource {
    resource: Rc<ResourceInner>,
    depth: usize,
    _permit: ResourcePermit,
}

#[derive(Default)]
struct ResourceLocal {
    held: RefCell<Vec<HeldResource>>,
    inherited: HashSet<u64>,
}

impl<'v> Local<'v> for ResourceLocal {
    fn init() -> Self {
        Self::default()
    }

    fn inherit(&self, _strand: &Strand<'v, '_>, kind: InheritKind) -> Self {
        match kind {
            InheritKind::Scoped => {
                let mut inherited = self.inherited.clone();
                inherited.extend(self.held.borrow().iter().map(|entry| entry.resource.id));
                Self {
                    held: RefCell::default(),
                    inherited,
                }
            }
            InheritKind::Background => Self::default(),
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

    fn inherit(&self, _strand: &Strand<'v, '_>, _kind: InheritKind) -> Self {
        self.cow.set(true);
        Self {
            cow: Cell::new(true),
        }
    }
}

struct StrandTypes<'v> {
    key: Type<'v, Key>,
    resource: Type<'v, Resource>,
}

struct StrandState<'v> {
    resource: LocalKey<'v, ResourceLocal>,
    next_resource_id: Cell<u64>,
    local: LocalKey<'v, StrandLocalData>,
    local_root: LocalRootKey<'v>,
    types: StrandTypes<'v>,
}

struct StrandStateTag;

impl<'v> Stateful<'v> for StrandState<'v> {
    type Tag = StrandStateTag;
}

struct Resource;

struct ResourceAnnex {
    inner: Rc<ResourceInner>,
}

impl<'v> Object<'v> for Resource {
    const NAME: &'v str = "Resource";
    const MODULE: &'v str = "strand";
    type Annex = ResourceAnnex;
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: crate::arg::Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([limit], []) = unpack!(strand, args, 1, 0)?;
        let limit = limit.to_index(strand)?;
        if limit == 0 {
            return Err(Error::value(
                strand,
                "strand.Resource: count must be positive",
            ));
        }
        let state = strand.state::<StrandState<'v>>();
        let id = state.next_resource_id.get();
        state.next_resource_id.set(id.strict_add(1));
        this.create_with_annex(
            strand,
            Resource,
            ResourceAnnex {
                inner: ResourceInner::new(id, limit),
            },
            out,
        );
        Ok(())
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.method("with", async move |this, strand, args, out| {
            let ([block], []) = unpack!(strand, args, 1, 0)?;
            resource_with(strand, this.annex().inner.clone(), block, out).await
        })
    }
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

async fn resource_with<'v, 's>(
    strand: &mut Strand<'v, 's>,
    resource: Rc<ResourceInner>,
    block: Slot<'v, '_>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let state = strand.state::<StrandState<'v>>();

    if state.resource.get(strand).inherited.contains(&resource.id) {
        return strand
            .interrupt_guard(async move |strand| call!(strand, block, out).await)
            .await;
    }

    let reentrant = {
        let mut held = state.resource.get(strand).held.borrow_mut();
        if let Some(entry) = held
            .iter_mut()
            .find(|entry| entry.resource.id == resource.id)
        {
            entry.depth += 1;
            true
        } else {
            false
        }
    };

    if !reentrant {
        let acquire = resource.clone();
        let permit = strand
            .interrupt_guard(async move |_strand| Ok(acquire_resource(acquire).await))
            .await?;
        state
            .resource
            .get(strand)
            .held
            .borrow_mut()
            .push(HeldResource {
                resource: resource.clone(),
                depth: 1,
                _permit: permit,
            });
    }

    let result = strand
        .interrupt_guard(async move |strand| call!(strand, block, out).await)
        .await;

    let mut held = state.resource.get(strand).held.borrow_mut();
    let index = held
        .iter()
        .position(|entry| entry.resource.id == resource.id)
        .expect("resource scope missing on exit");
    if held[index].depth == 1 {
        held.remove(index);
    } else {
        held[index].depth -= 1;
    }
    result
}

async fn map_workers<'v, 's>(
    strand: &mut Strand<'v, 's>,
    count: usize,
    input: Slot<'v, '_>,
    output: Slot<'v, '_>,
    block: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    strand
        .with_interrupt_mask(true, async move |strand| {
            let shared_input = &input;
            let shared_output = &output;
            let shared_block = &block;
            let mut strands = Vec::with_capacity(count);
            let interrupt = strand.interrupt_token().nested();
            for _ in 0..count {
                strands.push(
                    strand.spawn_scoped(Some(interrupt.clone()), async move |strand| {
                        let result = strand
                            .with_slots(
                                async move |strand,
                                            [
                                    mut input,
                                    mut output,
                                    mut block,
                                    mut item,
                                    mut mapped,
                                ]| {
                                    Output::set(strand, &mut input, shared_input);
                                    Output::set(strand, &mut output, shared_output);
                                    Output::set(strand, &mut block, shared_block);
                                    while input.next(strand, &mut item).await? {
                                        call!(strand, &block, &mut mapped, &item).await?;
                                        output.put(strand, &mut mapped).await?;
                                        strand.check_trap_gc()?;
                                    }
                                    Ok(())
                                },
                            )
                            .await;
                        if result.is_err() {
                            strand.interrupt_token().cancel();
                        }
                        result
                    }),
                );
            }
            let mut first_err: Option<Error<'v, '_>> = None;
            for result in join_all(strands).await {
                if let Err(error) = result
                    && first_err.as_ref().is_none_or(|previous| {
                        previous.kind() == ErrorKind::Canceled
                            && error.kind() != ErrorKind::Canceled
                    })
                {
                    first_err = Some(error);
                }
            }
            if let Some(error) = first_err {
                Err(error)
            } else {
                Ok(())
            }
        })
        .await
}

pub(crate) fn configure<'v>(builder: &mut Builder<'v>) {
    let resource = builder.local();
    let local = builder.local();
    let local_root = builder.local_root();
    let key = builder.register_type();
    let resource_type = builder.register_type();
    let state = builder.register_state(StrandState {
        resource,
        next_resource_id: Cell::new(0),
        local,
        local_root,
        types: StrandTypes {
            key,
            resource: resource_type,
        },
    });
    let input_key = builder.sym("input");
    let output_key = builder.sym("output");
    let backtrace_key = builder.sym("backtrace");
    let close = builder.sym("close");
    let default_key = builder.sym("default");
    let else_key = builder.sym("else");
    let strand_class = builder.singletons().strand.dup();
    let backtrace_class = builder.singletons().backtrace.dup();

    builder
        .module("strand")
        .value("Strand", &strand_class)
        .value("Backtrace", &backtrace_class)
        .value("Key", state.types.key)
        .value("Resource", state.types.resource)
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
                    async {
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
                    .await
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
        .function_with_slots(
            "map",
            async move |strand, args, _, [mut input, mut output]| {
                let ([count, block], [arg_input, arg_output]) =
                    unpack!(strand, args, 2, 0, input_key = None, output_key = None)?;
                let count = count.to_index(strand)?;
                if count == 0 {
                    return Err(Error::value(strand, "strand.map: count must be positive"));
                }
                if let Some(arg_input) = arg_input {
                    arg_input.iter(strand, &mut input).await?;
                } else {
                    strand.input(&mut input);
                }
                if let Some(arg_output) = arg_output {
                    arg_output.sink(strand, &mut output).await?;
                } else {
                    strand.output(&mut output);
                }

                map_workers(strand, count, input, output, block).await
            },
        )
        .function_with_slots(
            "pool",
            async move |strand, args, _, [mut input, mut output]| {
                let ([count, arg_input, block], []) = unpack!(strand, args, 3, 0)?;
                let count = count.to_index(strand)?;
                if count == 0 {
                    return Err(Error::value(strand, "strand.pool: count must be positive"));
                }
                arg_input.iter(strand, &mut input).await?;
                Output::set(strand, &mut output, Singleton::IterNull);
                map_workers(strand, count, input, output, block).await
            },
        )
        .function("fork", async move |strand, args, out| {
            let mut thunks = Vec::new();
            for arg in args {
                match arg {
                    Arg::Pos(thunk) => thunks.push(thunk),
                    Arg::Key(sym, _) => return Err(Error::unexpected_key(strand, sym)),
                }
            }

            let count = thunks.len();
            // We must avoid being dropped until we've awaited all strands we create
            strand
                .with_interrupt_mask(true, async move |strand| {
                    let result = async {
                        let results =
                            RefCell::new((0..count).map(|_| Value::NIL).collect::<Vec<_>>());
                        let mut strands = Vec::with_capacity(count);
                        let interrupt = strand.interrupt_token().nested();
                        for (i, thunk) in thunks.into_iter().enumerate() {
                            let results = &results;
                            strands.push(strand.spawn_scoped(
                                Some(interrupt.clone()),
                                async move |strand| {
                                    let result = strand
                                        .with_slots(async move |strand, [mut tmp]| {
                                            call!(strand, thunk, &mut tmp).await?;
                                            results.borrow_mut()[i] = tmp.take();
                                            Ok(())
                                        })
                                        .await;
                                    if result.is_err() {
                                        strand.interrupt_token().cancel();
                                    }
                                    result
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

    use super::{ResourceInner, acquire_resource};

    #[test]
    fn spurious_acquire_poll_does_not_duplicate_waiter() {
        let resource = ResourceInner::new(0, 1);
        let held = resource.try_acquire().unwrap();
        let mut acquire = Box::pin(acquire_resource(resource.clone()));
        let waker = noop_waker();
        let mut context = Context::from_waker(&waker);

        assert!(acquire.as_mut().poll(&mut context).is_pending());
        assert!(acquire.as_mut().poll(&mut context).is_pending());
        assert_eq!(resource.waiters.borrow().len(), 1);

        drop(held);
        let Poll::Ready(permit) = acquire.as_mut().poll(&mut context) else {
            panic!("released resource did not wake the acquire future");
        };
        drop(permit);
        assert_eq!(resource.active.get(), 0);
    }

    #[test]
    fn resource_wakes_waiters_in_arrival_order() {
        let resource = ResourceInner::new(0, 1);
        let held = resource.try_acquire().unwrap();
        let waker = noop_waker();
        let mut context = Context::from_waker(&waker);
        let mut first = Box::pin(acquire_resource(resource.clone()));
        let mut second = Box::pin(acquire_resource(resource.clone()));

        assert!(first.as_mut().poll(&mut context).is_pending());
        assert!(second.as_mut().poll(&mut context).is_pending());
        drop(held);

        let Poll::Ready(first_permit) = first.as_mut().poll(&mut context) else {
            panic!("first waiter was not admitted");
        };
        assert!(second.as_mut().poll(&mut context).is_pending());
        drop(first_permit);
        let Poll::Ready(second_permit) = second.as_mut().poll(&mut context) else {
            panic!("second waiter was not admitted");
        };
        drop(second_permit);
    }
}
