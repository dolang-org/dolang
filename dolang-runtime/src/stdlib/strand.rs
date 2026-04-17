use std::cell::RefCell;

use futures::future::join_all;

use crate::{
    arg::Arg,
    call,
    error::{Error, ErrorKind},
    method,
    object::{array::Array, backtrace, channel, protocol::GcObj, tuple},
    strand::{CancelToken, Redirect},
    unpack,
    value::{Output, Slot, Value},
    vm::{Builder, Vm},
};

/// Creates a channel pair using the VM's registered factory if set,
/// otherwise falls back to the default internal channel implementation.
fn pipe_pair<'v>(vm: &Vm<'v>, mut send: impl Output<'v>, mut recv: impl Output<'v>) {
    if let Some(pipe) = &vm.pipe_handler {
        pipe(
            vm,
            Slot::from_output(&mut send),
            Slot::from_output(&mut recv),
        );
    } else {
        let (s, r) = channel::pair(vm, 1);
        Slot::from_output(&mut send).store(Value::from_object(s));
        Slot::from_output(&mut recv).store(Value::from_object(r));
    }
}

pub(crate) fn configure<'v>(builder: &mut Builder<'v>) {
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
                .with_cancel_mask(true, async move |strand| {
                    let mut last_recv = Value::NIL;
                    let mut out = Some(out);
                    let mut strands = Vec::new();
                    let cancel = strand.cancel_token().nested();
                    for (i, thunk) in thunks.into_iter().enumerate() {
                        let (send, recv, mut out, redir_output) = if i < count - 1 {
                            let mut s = Value::NIL;
                            let mut r = Value::NIL;
                            pipe_pair(strand, Slot::new(&mut s), Slot::new(&mut r));
                            (Some(s), Some(r), None, None)
                        } else {
                            (None, None, out.take(), redir_output.take())
                        };
                        let redir_input = if i == 0 { redir_input.take() } else { None };

                        strands.push(strand.spawn_scoped(
                            Some(cancel.clone()),
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
                                    strand.cancel_token().cancel();
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
                                if matches!(e.kind(), ErrorKind::IterStop | ErrorKind::SinkStop) {
                                    continue;
                                }
                                if cancel.is_canceled() && e.kind() == ErrorKind::Canceled {
                                    continue;
                                }
                            }
                            return Err(e);
                        }
                    }
                    Ok(())
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
                    strand.check_interrupt_gc()?
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
                    strand.check_interrupt_gc()?
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
                    strand.check_interrupt_gc()?
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
                        strand.check_interrupt_gc()?
                    }
                    out.store(arg.take());
                } else {
                    let mut acc = Vec::new();
                    while input.next(strand, &mut item).await? {
                        acc.push(item.take())
                    }
                    out.store(Value::from_object(GcObj::new(
                        strand.arena(),
                        strand.builtin_types().array,
                        Array { inner: acc },
                    )));
                }
                Ok(())
            },
        )
        .function("spawn", async move |strand, args, mut out| {
            let ([mut callable], []) = unpack!(strand, args, 1, 0)?;
            let handle = strand.spawn_background_raw(callable.take(), CancelToken::new(), None)?;
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
                CancelToken::new(),
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
        .function("fork", async move |strand, args, mut out| {
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
                .with_cancel_mask(true, async move |strand| {
                    let results = RefCell::new((0..count).map(|_| Value::NIL).collect::<Vec<_>>());
                    let work = RefCell::new(thunks.into_iter().enumerate());
                    let num_workers = limit.unwrap_or(count).min(count);
                    let mut strands = Vec::new();
                    let cancel = strand.cancel_token().nested();
                    for _ in 0..num_workers {
                        let results = &results;
                        let work = &work;
                        strands.push(strand.spawn_scoped(
                            Some(cancel.clone()),
                            async move |strand| {
                                while let Some((i, thunk)) = { work.borrow_mut().next() } {
                                    if let Err(e) = strand
                                        .with_slots(async move |strand, [mut tmp]| {
                                            call!(strand, thunk, &mut tmp).await?;
                                            results.borrow_mut()[i] = tmp.take();
                                            Ok(())
                                        })
                                        .await
                                    {
                                        strand.cancel_token().cancel();
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
                    out.store(Value::from_object(GcObj::new(
                        strand.arena(),
                        strand.builtin_types().array,
                        Array {
                            inner: results.into_inner(),
                        },
                    )));
                    Ok(())
                })
                .await
        })
        .commit();
}
