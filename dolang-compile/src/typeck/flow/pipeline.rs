//! The signature a `strand.pipeline` call is checked against, built for the
//! call's own arguments. No written signature relates one stage's output to the
//! next stage's input, so the call's positional arguments are mirrored item by
//! item, as often as each is passed, and each pipe between stages is a binder
//! of its own.
//!
//! A conditional or repeated stage is folded in as an optional or repeated item:
//! the pipe after `?stage` holds what the pipe before it did as well as what the
//! stage writes, and a repeated stage reads and writes one pipe, which holds what
//! the pipe before it did. Each such join is a binder's bound, so what's written
//! into the pipe before is still inferred from the writes. A stage that may be
//! the first reads an `Iter`, the start's and the pipe's common supertype, and
//! one that may be the last writes a `Sink`.

use crate::typeck::{
    solver::CallArgument,
    r#type::{
        Argument, Binder, Binding, BoundRef, Database, DeclId, Element, Function, Kind, Literal,
        Multiplicity, SchemaItem, Type, TypeId, UnionMember, Variance,
    },
};

/// The declarations a pipeline's signature is built from
#[derive(Clone, Copy)]
pub(super) struct PipeRoles {
    pub(super) sender: DeclId,
    pub(super) receiver: DeclId,
    pub(super) iter: DeclId,
    pub(super) iterable: DeclId,
    pub(super) sink: DeclId,
    pub(super) sinkable: DeclId,
}

/// Where a stage reads from or writes to
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    /// The pipeline's input
    Start,
    /// The pipeline's output
    End,
    /// The pipe that is binder `slot`, and whether a path without stages
    /// reaches it from the start, and the end from it
    Pipe {
        slot: usize,
        from_start: bool,
        to_end: bool,
    },
}

struct Builder<'a> {
    db: &'a Database,
    roles: PipeRoles,
    binders: Vec<Binder>,
}

impl Builder<'_> {
    fn binder(&mut self, binding: Binding) -> usize {
        self.binders.push(Binder {
            kind: Kind::Type,
            binding,
            bound: None,
            default: None,
            variance: Variance::Invariant,
        });
        self.binders.len() - 1
    }

    fn bound(&self, slot: usize) -> TypeId {
        self.db.intern(Type::Bound {
            reference: BoundRef::new(0, slot),
            kind: Kind::Type,
        })
    }

    fn apply(&self, class: DeclId, arg: TypeId) -> TypeId {
        self.db.intern(Type::Apply {
            base: self.db.intern(Type::Decl(class)),
            args: [Argument::Positional(arg)].into_iter().collect(),
            kind: Kind::Type,
        })
    }

    fn pipe(&mut self, from_start: bool, to_end: bool) -> State {
        State::Pipe {
            slot: self.binder(Binding::Positional),
            from_start,
            to_end,
        }
    }
}

/// The signature a call of `strand.pipeline` passing `arguments` is checked
/// against (see the module documentation)
pub(super) fn pipeline_signature(
    db: &Database,
    roles: PipeRoles,
    arguments: &[(Multiplicity, CallArgument)],
) -> TypeId {
    let mut builder = Builder {
        db,
        roles,
        binders: Vec::new(),
    };
    let keyword = |name: &str| {
        let name = db.intern_symbol(name);
        arguments.iter().any(|&(multiplicity, argument)| {
            multiplicity == Multiplicity::Required
                && matches!(argument, CallArgument::Keyword(key, _) if key == name)
        })
    };
    let (input, output) = (keyword("input"), keyword("output"));
    let mut stages: Vec<Multiplicity> = arguments
        .iter()
        .filter_map(|&(multiplicity, argument)| match argument {
            CallArgument::Positional(_) => Some(multiplicity),
            CallArgument::Spread(_) => Some(Multiplicity::Repeated),
            CallArgument::Keyword(..) | CallArgument::Pair(..) => None,
        })
        .collect();
    // A pipeline needs a stage that always runs: without one, the call is
    // missing it
    if !stages.contains(&Multiplicity::Required) {
        stages.push(Multiplicity::Required);
    }
    let last = stages.len() - 1;
    let result =
        (stages[last] != Multiplicity::Repeated).then(|| builder.binder(Binding::Positional));

    // The ends, and the keywords that give them
    let mut keyed = Vec::new();
    let start = builder.binder(match input {
        true => Binding::Positional,
        false => Binding::Implicit,
    });
    let start_type = match input {
        true => {
            let item = builder.bound(start);
            let iterable = builder.apply(roles.iterable, item);
            keyed.push(("input", iterable));
            builder.apply(roles.iter, item)
        }
        false => builder.bound(start),
    };
    let end = builder.binder(match output {
        true => Binding::Positional,
        false => Binding::Implicit,
    });
    let end_type = match output {
        true => {
            let item = builder.bound(end);
            let sinkable = builder.apply(roles.sinkable, item);
            keyed.push(("output", sinkable));
            builder.apply(roles.sink, item)
        }
        false => builder.bound(end),
    };

    // That `from`'s items are readable from `to`, as its binder's bound
    let link = |builder: &mut Builder<'_>, from: State, to: State| match (from, to) {
        (State::Pipe { slot, .. }, State::Pipe { slot: to, .. }) => {
            builder.binders[slot].bound = Some(builder.bound(to));
        }
        (State::Start, State::Pipe { slot: to, .. }) => {
            let pipe = builder.bound(to);
            builder.binders[start].bound = Some(match input {
                true => pipe,
                false => builder.apply(builder.roles.iter, pipe),
            });
        }
        (State::Pipe { slot, .. }, State::End) => {
            let pipe = builder.bound(slot);
            match output {
                true => builder.binders[slot].bound = Some(builder.bound(end)),
                false => builder.binders[end].bound = Some(builder.apply(builder.roles.sink, pipe)),
            }
        }
        _ => unreachable!("a pipeline has a stage that always runs"),
    };
    let read = |builder: &Builder<'_>, state: State| match state {
        State::Start => start_type,
        State::Pipe {
            slot, from_start, ..
        } => {
            let class = match from_start {
                true => builder.roles.iter,
                false => builder.roles.receiver,
            };
            builder.apply(class, builder.bound(slot))
        }
        State::End => unreachable!("nothing reads the pipeline's output"),
    };
    let write = |builder: &Builder<'_>, state: State| match state {
        State::End => end_type,
        State::Pipe { slot, to_end, .. } => {
            let class = match to_end {
                true => builder.roles.sink,
                false => builder.roles.sender,
            };
            builder.apply(class, builder.bound(slot))
        }
        State::Start => unreachable!("nothing writes the pipeline's input"),
    };

    let nil = db.intern(Type::Literal(Literal::Nil));
    let required_within =
        |range: std::ops::Range<usize>| stages[range].contains(&Multiplicity::Required);
    let empty = db.intern(Type::Schema(Default::default()));
    let mut state = State::Start;
    let mut items = Vec::new();
    for (index, &multiplicity) in stages.iter().enumerate() {
        let from_start = !required_within(0..index);
        let to_end = !required_within(index + 1..stages.len());
        let (from, to) = match multiplicity {
            Multiplicity::Required => {
                let to = match index == last {
                    true => State::End,
                    false => builder.pipe(false, to_end),
                };
                (state, to)
            }
            Multiplicity::Optional => {
                let to = builder.pipe(from_start, to_end);
                link(&mut builder, state, to);
                (state, to)
            }
            Multiplicity::Repeated => {
                let pipe = builder.pipe(from_start, to_end);
                link(&mut builder, state, pipe);
                (pipe, pipe)
            }
        };
        let returns = match (index == last, result) {
            (true, Some(result)) => builder.bound(result),
            _ => nil,
        };
        let stage = db.intern(Type::Function(Function {
            params: empty,
            result: returns,
            input: Some(read(&builder, from)),
            output: Some(write(&builder, to)),
        }));
        items.push(SchemaItem {
            multiplicity,
            element: Element::Positional(stage),
        });
        state = to;
    }
    if state != State::End {
        link(&mut builder, state, State::End);
    }

    let keyed = keyed.into_iter().map(|(name, value)| SchemaItem {
        multiplicity: Multiplicity::Required,
        element: Element::Keyed {
            key: db.intern(Type::Literal(Literal::Sym(db.intern_symbol(name)))),
            value,
        },
    });
    let params = db.intern(Type::Schema(keyed.chain(items).collect()));
    let result = match (stages[last], result) {
        (Multiplicity::Required, Some(result)) => builder.bound(result),
        (Multiplicity::Optional, Some(result)) => db.intern(Type::Union(
            [
                UnionMember::Type(builder.bound(result)),
                UnionMember::Type(nil),
            ]
            .into_iter()
            .collect(),
        )),
        _ => nil,
    };
    let function = db.intern(Type::Function(Function {
        params,
        result,
        input: (!input).then(|| builder.bound(start)),
        output: (!output).then(|| builder.bound(end)),
    }));
    db.intern(Type::Quantified {
        binders: builder.binders.into(),
        body: function,
    })
}
