use std::{
    cell::{Cell, RefCell},
    collections::{HashSet, VecDeque},
    ops::ControlFlow,
    pin::Pin,
    rc::Rc,
    task::{self, Poll, Waker},
};

use futures::future::join_all;

use crate::{
    arg::Arg,
    call,
    error::{Error, ErrorKind, Result},
    gc::arena::Visit,
    method,
    object::{
        backtrace, channel,
        native::{Mut, Object, Ref, Type, TypeBuilder},
        strand::{StrandInfo, StrandRef, Stream, StreamIter, StreamSink},
        tuple,
    },
    redirect,
    strand::{
        InheritKind, InterruptMask, InterruptToken, Local, LocalKey, LocalRootKey, Strand,
        with_scan, with_slots_dynamic,
    },
    unpack,
    value::{Output, Singleton, Slot, Value},
    vm::{AllocExt, Builder, State, StateTag},
    with_instance, with_interrupt_mask, with_slots,
};

/// Creates a channel pair using the VM's registered factory if set,
/// otherwise falls back to the default internal channel implementation.
fn pipe_pair<'v>(
    strand: &mut Strand<'v, '_>,
    mut send: impl Output<'v>,
    mut recv: impl Output<'v>,
) {
    if let Some(pipe) = strand.vm().pipe_handler.borrow().as_ref() {
        pipe(
            strand,
            Slot::from_output(&mut send),
            Slot::from_output(&mut recv),
        );
    } else {
        channel::pair(strand, 1, send, recv);
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

impl StateTag for StrandStateTag {
    type State<'v> = StrandState<'v>;
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
        unpack!(strand, args => limit);
        let limit = limit.to_index(strand)?;
        if limit == 0 {
            return Err(Error::value(
                strand,
                "strand.Resource: count must be positive",
            ));
        }
        let state = strand.state::<StrandStateTag>();
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
            unpack!(strand, args => block);
            resource_with(strand, this.annex().inner.clone(), block, out).await
        })
    }
}

struct Key;

impl<'v> Object<'v> for Key {
    const NAME: &'v str = "Key";
    const MODULE: &'v str = "strand";
    const SLOTS: usize = 1;
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: crate::arg::Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        unpack!(strand, args => ?initial);
        this.create(strand, Key, &mut out);
        if let Some(initial) = initial {
            with_instance!(strand, this.cast(&out).unwrap(), |inst| {
                let mut borrow = inst.borrow_mut_unwrap();
                strand.store(Mut::slot_mut::<0>(&mut borrow), initial);
            });
        }
        Ok(())
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get_with_slots("value", |this, strand, mut out, [mut key]| {
                let state = strand.state::<StrandStateTag>();
                strand.store(&mut key, this);
                let root = state.local_root.slot(strand);
                if !root.is_nil() {
                    let dict = root.as_dict(strand).expect("strand local store is a dict");
                    if dict.get(strand, &key, None, &mut out)? {
                        return Ok(());
                    }
                }
                let borrow = this.borrow(strand)?;
                strand.store(out, Ref::slot::<0>(&borrow));
                Ok(())
            })
            .set_with_slots("value", |this, strand, mut value, [mut key]| {
                let state = strand.state::<StrandStateTag>();
                strand.store(&mut key, this);
                let root = unique_local_root(state, strand)?;
                let dict = root
                    .as_dict(strand)
                    .expect("strand local store is not a dict");
                dict.insert(strand, &key, &mut value, true)?;
                Ok(())
            })
            .method_with_slots(
                "with",
                async |this, strand, args, out, [mut key, mut prior]| {
                    unpack!(strand, args => mut value, block);
                    let state = strand.state::<StrandStateTag>();
                    strand.store(&mut key, this);
                    let root = state.local_root.slot(strand);
                    let had_prior = if root.is_nil() {
                        false
                    } else {
                        let dict = root.as_dict(strand).expect("strand local store is a dict");
                        dict.get(strand, &key, None, &mut prior)?
                    };
                    let root = unique_local_root(state, strand)?;
                    let dict = root.as_dict(strand).expect("strand local store is a dict");
                    dict.insert(strand, &key, &mut value, true)?;

                    let result = call!(strand, block, out);

                    let root = unique_local_root(state, strand)?;
                    let dict = root.as_dict(strand).expect("strand local store is a dict");
                    if had_prior {
                        dict.insert(strand, &key, &mut prior, true)?;
                    } else {
                        dict.delete(strand, &key)?;
                    }
                    result
                },
            )
    }
}

fn clone_local_root<'v, 's>(
    state: State<'v, StrandState<'v>>,
    strand: &mut Strand<'v, 's>,
) -> Slot<'v, 's> {
    with_slots!(strand, |mut copied, mut key, mut value| {
        let mut root = state.local_root.slot(strand);
        if root.is_nil() {
            strand.new_dict(&mut root);
            return root;
        }
        strand.new_dict(&mut copied);
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
        strand.store(&mut root, copied);
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
        strand.new_dict(&mut root);
    }
    Ok(root)
}

async fn resource_with<'v, 's>(
    strand: &mut Strand<'v, 's>,
    resource: Rc<ResourceInner>,
    block: Slot<'v, '_>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let state = strand.state::<StrandStateTag>();

    if state.resource.get(strand).inherited.contains(&resource.id) {
        return strand
            .interrupt_guard(async move |strand| call!(strand, block, out))
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
        .interrupt_guard(async move |strand| call!(strand, block, out))
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
    input: &Value<'v>,
    output: &Value<'v>,
    block: &Value<'v>,
    disconnect_input: bool,
    disconnect_output: bool,
) -> Result<'v, 's, ()> {
    with_interrupt_mask!(strand, InterruptMask::all(), {
        redirect!(
            strand,
            ?input: disconnect_input.then_some(Singleton::Null),
            ?output: disconnect_output.then_some(Singleton::Null),
            {
                let mut strands = Vec::with_capacity(count);
                let interrupt = strand.interrupt_token().nested(InterruptMask::empty());
                for _ in 0..count {
                    strands.push(
                        strand.spawn_scoped(Some(interrupt.clone()), async move |strand| {
                            let result = async {
                                with_slots!(strand, |mut item, mut mapped| {
                                    while input.next(strand, &mut item).await? {
                                        call!(strand, block, &item, &mut mapped)?;
                                        output.put(strand, &mut mapped).await?;
                                        strand.check_trap_gc()?;
                                    }
                                    Ok(())
                                })
                            }
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
            }
        )
    })
}

pub(crate) fn configure<'v>(builder: &mut Builder<'v>) {
    let resource = builder.local();
    let local = builder.local();
    let local_root = builder.local_root();
    builder.lazy::<StrandStateTag>(&["strand"], move |reg| {
        let key = reg.register_type();
        let resource_type = reg.register_type();
        let state = reg.register_state::<StrandStateTag>(StrandState {
            resource,
            next_resource_id: Cell::new(0),
            local,
            local_root,
            types: StrandTypes {
                key,
                resource: resource_type,
            },
        });
        let input_key = reg.sym("input");
        let output_key = reg.sym("output");
        let backtrace_key = reg.sym("backtrace");
        let close = reg.sym("close");
        let default = reg.sym("default");
        let else_key = reg.sym("else");
        let strand_class = &reg.singletons().strand;
        let task_class = &reg.singletons().task;
        let stream_class = &reg.singletons().stream;
        let strand_info_class = &reg.singletons().strand_info;

        reg.module("strand")
            .value("Strand", strand_class)
            .value("Task", task_class)
            .value("Stream", stream_class)
            .value("StrandInfo", strand_info_class)
            .value("Key", state.types.key)
            .value("Resource", state.types.resource)
            // Implicit I/O functions (moved from former std.iter)
            .function_with_slots("put", async move |strand, args, _, [mut tmp]| {
                unpack!(strand, args => value);
                strand.output(&mut tmp);
                tmp.put(strand, value).await
            })
            .function_with_slots("next", async move |strand, args, mut out, [mut tmp]| {
                unpack!(strand, args => ?:default, ?else_key: or_else);
                if default.is_some() && or_else.is_some() {
                    return Err(Error::unexpected_key(strand, else_key));
                }
                strand.input(&mut tmp);
                let res = tmp.next(strand, &mut out).await?;
                if !res {
                    if let Some(mut default) = default {
                        strand.store(out, &mut default);
                        return Ok(());
                    } else if let Some(or_else) = or_else {
                        return call!(strand, or_else, out);
                    }
                    return Err(Error::iter_stop(strand));
                }
                Ok(())
            })
            .function("input", async move |strand, args, out| {
                unpack!(strand, args =>);
                strand.input(out);
                Ok(())
            })
            .function("output", async move |strand, args, out| {
                unpack!(strand, args =>);
                strand.output(out);
                Ok(())
            })
            .function_without_frame("backtrace", async move |strand, args, out| {
                unpack!(strand, args =>);
                // SAFETY: the entries move straight into the allocated object
                backtrace::create(strand, |strand| unsafe { strand.backtrace_entries() }, out);
                Ok(())
            })
            .function_without_frame("current", async move |strand, args, out| {
                unpack!(strand, args =>);
                let cell = strand.inner.cell();
                let ty = strand.builtin_types().strand_ref;
                ty.create_imm(strand, StrandRef::new(cell), out);
                Ok(())
            })
            .function_without_frame("snapshot", async move |strand, args, out| {
                unpack!(strand, args =>);
                // Walk first, then build values: the walk must not allocate
                // SAFETY: the entries are rooted by the scan frame entered below,
                // before anything allocates
                let raw: Vec<_> = unsafe { strand.snapshot_raw() }
                    .into_iter()
                    .map(|raw| RefCell::new(Some(raw)))
                    .collect();
                let ty = strand.builtin_types().strand_info;
                let visit = |visitor: &mut dyn Visit| {
                    for raw in &raw {
                        // Never borrowed across an allocation
                        if let Some(raw) = &*raw.borrow() {
                            for entry in &raw.entries {
                                entry.accept(visitor)?;
                            }
                        }
                    }
                    ControlFlow::Continue(())
                };
                // Safety: `visit` visits each staged entry's program reference;
                // an entry leaves its cell only after its object is allocated
                unsafe {
                    with_scan!(
                        strand,
                        visit,
                        with_slots_dynamic!(strand, raw.len(), |mut infos| {
                            for (i, raw) in raw.iter().enumerate() {
                                let embryo = ty.embryo(strand);
                                embryo.finish_imm(
                                    strand,
                                    StrandInfo::new(raw.take().expect("taken once")),
                                    infos.at(i),
                                );
                            }
                            let len = infos.len();
                            let embryo = tuple::embryo(strand, len);
                            embryo.finish_moving(strand, infos.iter_mut(), out);
                        })
                    )
                };
                Ok(())
            })
            .function_without_frame("error_backtrace", async move |strand, args, out| {
                unpack!(strand, args =>);
                if strand.inner.focused_backtrace().is_none() {
                    return Err(Error::state_error(strand, "no active handled exception"));
                }
                // SAFETY: the entries move straight into the allocated object
                backtrace::create(
                    strand,
                    |strand| unsafe {
                        let focused = strand.inner.focused_backtrace().unwrap_unchecked();
                        let mut entries: Vec<_> = focused.iter().map(|e| e.dup()).collect();
                        entries.extend(strand.backtrace_entries());
                        entries
                    },
                    out,
                );
                Ok(())
            })
            .function_without_frame("throw", async move |strand, args, _| {
                unpack!(strand, args => value, ?backtrace_key: backtrace);
                let err = if let Some(backtrace) = backtrace {
                    Error::from_value_backtrace(strand, value, backtrace)?
                } else {
                    Error::from_value(strand, value)
                };
                Err(err)
            })
            .function_with_slots(
                "redirect",
                async move |strand, args, out, [mut input, mut output]| {
                    unpack!(strand, args => block, ?input_key: arg_input, ?output_key: arg_output);
                    if let Some(arg) = &arg_input {
                        arg.iter(strand, &mut input).await?;
                    }
                    if let Some(arg) = &arg_output {
                        arg.sink(strand, &mut output).await?;
                    }
                    redirect!(
                        strand,
                        ?input: arg_input.map(|_| &input),
                        ?output: arg_output.map(|_| &output),
                        call!(strand, block, out)
                    )
                },
            )
            .function("with_io", async move |strand, args, out| {
                unpack!(strand, args => block, ?input_key: arg_input, ?output_key: arg_output);
                redirect!(strand, ?input: arg_input, ?output: arg_output, call!(strand, block, out))
            })
            .function("channel", async move |strand, args, out| {
                unpack!(strand, args => ?limit);
                let limit = match limit {
                    None => 1,
                    Some(limit) => limit.to_index(strand)?,
                };
                with_slots!(strand, |mut send, mut recv| {
                    channel::pair(strand, limit, &mut send, &mut recv);
                    tuple::create_pair(strand, &send, &recv, out);
                });
                Ok(())
            })
            .function_with_slots("pipeline", async move |strand, args, out, [mut tmp]| {
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
                with_interrupt_mask!(strand, InterruptMask::all(), {
                    // The pipe ends stay rooted here until every stage has finished;
                    // each end moves into the one stage that uses it
                    with_slots_dynamic!(strand, 2 * count.saturating_sub(1), |pipe_slots| {
                        async {
                            let mut out = Some(out);
                            let mut strands = Vec::new();
                            let interrupt = strand.interrupt_token().nested(InterruptMask::empty());
                            // Resolve the caller's redirects up front, in place.
                            // Doing this in the stage body would put a fallible
                            // await ahead of the call, and an early return from it
                            // would skip closing the stage's pipe ends.
                            if let Some(input) = &mut redir_input {
                                input.iter(strand, &mut tmp).await?;
                                strand.store(input, &mut tmp);
                            }
                            if let Some(output) = &mut redir_output {
                                output.sink(strand, &mut tmp).await?;
                                strand.store(output, &mut tmp);
                            }
                            let mut ends = pipe_slots.into_iter();
                            let mut pipes = Vec::with_capacity(count - 1);
                            while let (Some(mut send), Some(mut recv)) = (ends.next(), ends.next())
                            {
                                pipe_pair(strand, &mut send, &mut recv);
                                pipes.push((send, recv));
                            }
                            let mut pipes = pipes.into_iter();
                            // The receiving end of the previous stage's pipe
                            let mut next_recv = None;
                            for (i, thunk) in thunks.into_iter().enumerate() {
                                let (send, after, mut out, redir_output) = match pipes.next() {
                                    Some((send, recv)) => (Some(send), Some(recv), None, None),
                                    None => (None, None, out.take(), redir_output.take()),
                                };
                                let recv = std::mem::replace(&mut next_recv, after);
                                let redir_input = if i == 0 { redir_input.take() } else { None };

                                strands.push(strand.spawn_scoped(
                                    Some(interrupt.clone()),
                                    async move |strand| {
                                        if let Err(e) = async {
                                            with_slots!(strand, |mut tmp, mut tmp2| {
                                                let input =
                                                    recv.as_deref().or(redir_input.as_deref());
                                                let output =
                                                    send.as_deref().or(redir_output.as_deref());
                                                let res = redirect!(
                                                    strand,
                                                    ?input: input,
                                                    ?output: output,
                                                    call!(
                                                        strand,
                                                        thunk,
                                                        out.as_mut().map_or_else(
                                                            || Slot::reborrow(&mut tmp),
                                                            Slot::reborrow
                                                        )
                                                    )
                                                );
                                                // Close both ends even if the
                                                // first close fails, so a
                                                // failure cannot strand the
                                                // other end open.
                                                let closed_recv = match &recv {
                                                    Some(recv) => {
                                                        method!(strand, recv, close, &mut tmp2)
                                                    }
                                                    None => Ok(()),
                                                };
                                                let closed_send = match &send {
                                                    Some(send) => {
                                                        method!(strand, send, close, &mut tmp2)
                                                    }
                                                    None => Ok(()),
                                                };
                                                closed_recv?;
                                                closed_send?;
                                                res
                                            })
                                        }
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
                            }
                            for (i, res) in join_all(strands).await.into_iter().enumerate() {
                                if let Err(e) = res {
                                    // Suppress Input/SinkStop errors (broken pipe) or cancellation from all
                                    // but the final element
                                    if i != count - 1 {
                                        if matches!(
                                            e.kind(),
                                            ErrorKind::IterStop | ErrorKind::SinkStop
                                        ) {
                                            continue;
                                        }
                                        if interrupt.is_canceled()
                                            && e.kind() == ErrorKind::Canceled
                                        {
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
                })
            })
            .function_with_slots(
                "each",
                async move |strand, args, _, [mut value, mut tmp, mut input, mut output]| {
                    unpack!(strand, args => func);
                    strand.input(&mut input);
                    strand.output(&mut output);
                    redirect!(
                        strand,
                        input: Singleton::Null,
                        output: Singleton::Null,
                        {
                            while input.next(strand, &mut value).await? {
                                call!(strand, &func, &value, &mut tmp)?;
                                output.put(strand, &mut tmp).await?;
                                strand.check_trap_gc()?
                            }
                            Ok(())
                        }
                    )
                },
            )
            .function_with_slots(
                "where",
                async move |strand, args, _, [mut value, mut tmp, mut input, mut output]| {
                    unpack!(strand, args => pred);
                    strand.input(&mut input);
                    strand.output(&mut output);
                    redirect!(
                        strand,
                        input: Singleton::Null,
                        output: Singleton::Null,
                        {
                            while input.next(strand, &mut value).await? {
                                call!(strand, &pred, &value, &mut tmp)?;
                                if tmp.to_bool(strand)? {
                                    output.put(strand, &mut value).await?;
                                }
                                strand.check_trap_gc()?
                            }
                            Ok(())
                        }
                    )
                },
            )
            .function_with_slots(
                "from",
                async move |strand, args, _, [mut input, mut output, mut value]| {
                    unpack!(strand, args => arg);
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
                    unpack!(strand, args => ?arg);
                    strand.input(&mut input);
                    if let Some(mut arg) = arg {
                        arg.sink(strand, &mut output).await?;
                        while input.next(strand, &mut item).await? {
                            output.put(strand, &mut item).await?;
                            strand.check_trap_gc()?
                        }
                        strand.store(out, &mut arg);
                    } else {
                        // Allocate the array first, so each item is rooted as soon as it
                        // arrives
                        strand.new_array(&mut out);
                        while input.next(strand, &mut item).await? {
                            let vm = strand.vm();
                            let array = out.as_array(vm).expect("collect accumulator");
                            array.push(strand, &mut item)?;
                        }
                    }
                    Ok(())
                },
            )
            .function("spawn", async move |strand, args, out| {
                unpack!(strand, args => mut callable);
                strand.spawn_background_raw(
                    &mut callable,
                    InterruptToken::new(),
                    None,
                    out,
                    async move |strand, arg, out| call!(strand, arg, out),
                )
            })
            .function("stream", async move |strand, args, mut out| {
                unpack!(strand, args => mut callable);

                // Channel A: handle writes → strand reads
                // Channel B: strand writes → handle reads
                with_slots!(strand, |mut in_send,
                                     mut in_recv,
                                     mut out_send,
                                     mut out_recv,
                                     mut handle,
                                     mut input,
                                     mut output| {
                    pipe_pair(strand, &mut in_send, &mut in_recv);
                    pipe_pair(strand, &mut out_send, &mut out_recv);
                    strand.spawn_background_raw(
                        &mut callable,
                        InterruptToken::new(),
                        Some((&in_recv, &out_send)),
                        &mut handle,
                        async move |strand, arg, out| call!(strand, arg, out),
                    )?;

                    // Wrap the endpoints up front so they keep `handle` GC-reachable
                    // for as long as they're reachable themselves, even once the
                    // `Stream` returned below is not.
                    let builtin_types = strand.builtin_types();
                    let ty = builtin_types.strand_handle;
                    let task = handle.downcast_ref(ty).expect("strand handle");
                    // SAFETY (here and below): the handle and the endpoints move straight
                    // into the allocated object
                    let embryo = builtin_types.stream_sink.embryo(strand);
                    let sink = unsafe { StreamSink::new(in_send.take(strand), task.dup()) };
                    embryo.finish(strand, sink, &mut input);
                    let embryo = builtin_types.stream_iter.embryo(strand);
                    let iter = unsafe { StreamIter::new(out_recv.take(strand), task.dup()) };
                    embryo.finish(strand, iter, &mut output);
                    let embryo = builtin_types.stream.embryo(strand);
                    let stream = unsafe {
                        Stream {
                            handle: task.dup(),
                            input: input.take(strand),
                            output: output.take(strand),
                        }
                    };
                    embryo.finish_imm(strand, stream, &mut out);
                    Ok(())
                })
            })
            .function_with_slots(
                "map",
                async move |strand, args, _, [mut input, mut output]| {
                    unpack!(strand, args =>
                        count,
                        block,
                        ?input_key: arg_input,
                        ?output_key: arg_output
                    );
                    let count = count.to_index(strand)?;
                    if count == 0 {
                        return Err(Error::value(strand, "strand.map: count must be positive"));
                    }
                    let disconnect_input = arg_input.is_none();
                    let disconnect_output = arg_output.is_none();
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

                    map_workers(
                        strand,
                        count,
                        &input,
                        &output,
                        &block,
                        disconnect_input,
                        disconnect_output,
                    )
                    .await
                },
            )
            .function_with_slots(
                "pool",
                async move |strand, args, _, [mut input, mut output]| {
                    unpack!(strand, args => count, input_or_block, ?block);
                    let disconnect_input = block.is_none();
                    let block = if let Some(block) = block {
                        input_or_block.iter(strand, &mut input).await?;
                        block
                    } else {
                        strand.input(&mut input);
                        input_or_block
                    };
                    let count = count.to_index(strand)?;
                    if count == 0 {
                        return Err(Error::value(strand, "strand.pool: count must be positive"));
                    }
                    strand.store(&mut output, Singleton::Null);
                    map_workers(
                        strand,
                        count,
                        &input,
                        &output,
                        &block,
                        disconnect_input,
                        false,
                    )
                    .await
                },
            )
            .function("fork", async move |strand, args, out| {
                unpack!(strand, args => *thunks);
                let thunks: Vec<_> = thunks.collect();

                let count = thunks.len();
                // Each child writes its result into its own slot
                with_slots_dynamic!(strand, count, |mut results| {
                    // We must avoid being dropped until we've awaited all strands we create
                    with_interrupt_mask!(strand, InterruptMask::all(), {
                        let result = async {
                            let mut strands = Vec::with_capacity(count);
                            let interrupt = strand.interrupt_token().nested(InterruptMask::empty());
                            for (thunk, mut result_slot) in
                                thunks.into_iter().zip(results.iter_mut())
                            {
                                strands.push(strand.spawn_scoped(
                                    Some(interrupt.clone()),
                                    async move |strand| {
                                        let result =
                                            async { call!(strand, thunk, &mut result_slot) }.await;
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
                            Ok(())
                        }
                        .await;
                        result?;
                        let embryo = tuple::embryo(strand, count);
                        embryo.finish_moving(strand, results.iter_mut(), out);
                        Ok(())
                    })
                })
            })
            .commit();
    });
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
