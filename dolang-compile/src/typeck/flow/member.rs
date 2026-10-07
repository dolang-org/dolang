//! Member use: reading, writing and calling a receiver's members, indexing,
//! operators, calls through `(call)`, ranges and constructors. Each finds its
//! member with the solver's lookup ([`Solver::member`]) and checks the use as the
//! runtime makes it: as a call through the member, passing the receiver first to a
//! method. A use of a union's member is made of each alternative, all of which
//! must have it, and gives what they give. A receiver the lookup can't decide is
//! an explicit residual. A call through an overloaded method is a call of its
//! overloads as a whole, which the solver chooses among (see
//! [`Type::Overloaded`]); any other use of one is dynamic.
//!
//! [`Solver::member`]: crate::typeck::solver::Solver::member

use std::collections::VecDeque;

use super::{
    At, Flow, State,
    problem::{MemberUse, Problem},
    rule::Call,
};
use crate::{
    lex::Op,
    source::Span,
    typeck::{
        cfg::{Expr, ExprKind, Member},
        elab::Designated,
        solver::{
            Constructor, FoundKind, Issue, Lookup, Residual, Signature, Signatures, bound_method,
        },
        r#type::{Intrinsic, Literal, MemberKey, Scope, Type, TypeId, UnionMember},
    },
};

/// How a call reaches a callee (see [`Flow::call_target`]): the signature it
/// calls, passing `receivers` before its own arguments
pub(super) struct CallTarget {
    pub(super) signature: Signature,
    pub(super) receivers: Vec<(TypeId, Span)>,
    /// The member it calls, if any
    pub(super) member: Option<Member>,
    /// The instance a constructor with unchecked arguments gives
    instance: Option<TypeId>,
}

impl CallTarget {
    fn new(callee: Option<TypeId>) -> Self {
        Self {
            signature: Signature {
                overloads: Vec::new(),
                implementation: callee,
                function: None,
            },
            receivers: Vec::new(),
            member: None,
            instance: None,
        }
    }

    /// A dynamic callee, passed `leading` first
    fn dynamic(leading: &[(TypeId, Span)]) -> Self {
        Self {
            receivers: leading.to_vec(),
            ..Self::new(None)
        }
    }

    /// What the call gives, from what calling its signature gave
    pub(super) fn gives(&self, given: TypeId, bottom: TypeId) -> TypeId {
        match self.instance {
            Some(instance) if given != bottom => instance,
            _ => given,
        }
    }
}

/// A receiver's member, as a use sees it
enum Resolved {
    /// Nothing to check: the receiver is dynamic, or the lookup can't decide
    Dynamic,
    Missing,
    Field(TypeId),
    /// A method, and whether a call through the receiver passes it: an instance's
    /// instance method, or a class object's class method
    Method(Signature, bool),
    /// A property's accessors, and whether a call to them passes the receiver:
    /// unless they're already bound to it
    Property {
        getter: Option<Signature>,
        setter: Option<Signature>,
        passed: bool,
    },
    /// No member, but the instance's class has these for any ordinary name
    Fallback {
        get: Option<Signature>,
        set: Option<Signature>,
    },
}

impl Flow<'_, '_> {
    /// How calling `callee` reaches a signature: a function, or an overloaded
    /// one, directly, and a class object through its class-level `(call)` if it
    /// has one, and otherwise instantiation, which runs `(init)` and gives the
    /// instance. Any other callee is sent its `(call)` special method. An unknown
    /// or undecided callee is dynamic; a known one without `(call)` is reported
    /// at `span`.
    pub(super) fn call_target(&mut self, callee: (TypeId, Span), span: Span) -> CallTarget {
        let (ty, _) = callee;
        if self.callable(ty) || ty == self.db.bottom() {
            return CallTarget::new(Some(ty));
        }
        if let Some(class) = self.class_of(ty) {
            let generic = matches!(self.db.ty(ty), Type::Quantified { .. });
            return match self.solver().constructor(class) {
                Constructor::Call(Some(signature)) if !generic => CallTarget {
                    receivers: vec![callee],
                    member: Some(self.special("call")),
                    ..CallTarget::new(Some(signature))
                },
                Constructor::Init(Some(signature)) if signature.callee(self.db).is_some() => {
                    CallTarget {
                        signature,
                        member: Some(self.special("init")),
                        ..CallTarget::new(None)
                    }
                }
                Constructor::Init(_) if !generic => CallTarget {
                    instance: Some(self.db.intern(Type::Decl(class))),
                    ..CallTarget::new(None)
                },
                _ => CallTarget::new(None),
            };
        }
        let member = self.special("call");
        match self.resolve(ty, member, span) {
            Resolved::Method(signature, bound) => CallTarget {
                signature,
                receivers: if bound { vec![callee] } else { Vec::new() },
                member: Some(member),
                instance: None,
            },
            Resolved::Missing => {
                self.missing(ty, None, member, span);
                CallTarget::new(None)
            }
            _ => CallTarget::new(None),
        }
    }

    /// A special member's key
    fn special(&self, name: &str) -> Member {
        Member {
            key: MemberKey {
                name: self.db.intern_symbol(name),
                special: true,
                private: false,
            },
            class: None,
        }
    }

    /// Look up a receiver's member. What the lookup can't decide is recorded at
    /// `span` as an unresolved check.
    fn resolve(&mut self, receiver: TypeId, member: Member, span: Span) -> Resolved {
        if matches!(self.db.ty(receiver), Type::Unknown(_)) {
            return Resolved::Dynamic;
        }
        let solver = self.solver();
        let term = solver.closed(receiver);
        let lookup = match member.class {
            Some(class) => solver.private_member(term, class, member.key),
            None => solver.member(term, member.key),
        };
        let lookup = match lookup {
            Ok(lookup) => lookup,
            Err(issue) => {
                if let Issue::Residual(residual) = issue {
                    self.undecided(span, residual);
                }
                return Resolved::Dynamic;
            }
        };
        // A method whose signatures don't all reify is dynamic
        let signature = |signatures: &Signatures| solver.reified(signatures).unwrap_or_default();
        let method = |kind: &FoundKind| match kind {
            FoundKind::Method(signatures) => signature(signatures),
            _ => Signature::default(),
        };
        let signatures =
            |signatures: Option<Signatures>| signatures.map(|signatures| signature(&signatures));
        match lookup {
            Lookup::Found(found) => match found.kind {
                FoundKind::Field(ty) => solver.reify(ty).map_or(Resolved::Dynamic, Resolved::Field),
                ref kind @ FoundKind::Method(_) => {
                    // A class object reaches an instance method unbound
                    let object = self.class_of(receiver).is_some();
                    let bound = !found.bound
                        && match found.scope {
                            Scope::Instance => !object,
                            Scope::Class => true,
                            Scope::Static => false,
                        };
                    Resolved::Method(method(kind), bound)
                }
                FoundKind::Property { getter, setter } => Resolved::Property {
                    getter: signatures(getter),
                    setter: signatures(setter),
                    passed: !found.bound,
                },
                FoundKind::Unknown => Resolved::Dynamic,
            },
            Lookup::Fallback { get, set } => Resolved::Fallback {
                get: get.map(|found| method(&found.kind)),
                set: set.map(|found| method(&found.kind)),
            },
            Lookup::Missing => Resolved::Missing,
            Lookup::Dynamic => Resolved::Dynamic,
        }
    }

    /// A union receiver's alternatives, each of which a member use is made of,
    /// with a union alias among them expanded to its own. One that projects a
    /// schema is undecided, recorded at `span`. `None` if the receiver isn't a
    /// union.
    fn alternatives(&mut self, receiver: TypeId, span: Span) -> Option<Vec<Option<TypeId>>> {
        let mut alternatives = Vec::new();
        if !self.expand(receiver, &mut alternatives, 0) {
            return None;
        }
        if alternatives.contains(&None) {
            self.undecided(
                span,
                Residual::Unsupported("a projection in a union receiver"),
            );
        }
        Some(alternatives)
    }

    /// Add a union's alternatives to `alternatives`, expanding a union alias's.
    /// Whether `ty` is a union.
    pub(super) fn expand(
        &self,
        ty: TypeId,
        alternatives: &mut Vec<Option<TypeId>>,
        depth: usize,
    ) -> bool {
        // A union alias that expands to itself is ill-formed
        const DEPTH: usize = 16;
        let ty = match self.db.ty(ty) {
            Type::Decl(_) | Type::Apply { .. } if depth < DEPTH => {
                match self.solver().exposed(ty) {
                    Some(exposed) => exposed,
                    None => return false,
                }
            }
            _ => ty,
        };
        let Type::Union(members) = self.db.ty(ty) else {
            return false;
        };
        if members.is_empty() {
            return false;
        }
        for member in members.iter() {
            let alternative = match *member {
                UnionMember::Type(ty) if self.expand(ty, alternatives, depth + 1) => continue,
                UnionMember::Type(ty) => Some(ty),
                _ => None,
            };
            if !alternatives.contains(&alternative) {
                alternatives.push(alternative);
            }
        }
        true
    }

    /// Report a missing member, of an alternative of the union `within` if given.
    /// The alternatives a use finds without it are reported together.
    fn missing(&mut self, receiver: TypeId, within: Option<TypeId>, member: Member, span: Span) {
        if !self.observing() {
            return;
        }
        let receiver = self.tables.render_type(self.db, receiver);
        let within = within.map(|union| self.tables.render_type(self.db, union));
        let name = self.member_name(member);
        if within.is_some()
            && let Some(results) = &mut self.results
            && let Some(receivers) = results
                .problems
                .iter_mut()
                .find_map(|problem| match problem {
                    Problem::MissingMember {
                        span: at,
                        receivers,
                        within: union,
                        name: missing,
                    } if (*at, &*union, &*missing) == (span, &within, &name) => Some(receivers),
                    _ => None,
                })
        {
            if let Err(at) = receivers.binary_search(&receiver) {
                receivers.insert(at, receiver);
            }
            return;
        }
        self.problem(Problem::MissingMember {
            span,
            receivers: vec![receiver],
            within,
            name,
        });
    }

    fn misuse(&mut self, member: Member, span: Span, misuse: MemberUse) {
        if !self.observing() {
            return;
        }
        let name = self.member_name(member);
        self.problem(Problem::MemberUse { span, name, misuse });
    }

    pub(super) fn member_name(&self, member: Member) -> String {
        let name = self.db.symbol(member.key.name);
        match member.key.special {
            true => format!("({name})"),
            false => name.to_owned(),
        }
    }

    /// A member's name, as the `(get)` and `(set)` fallbacks are passed it
    fn name_literal(&self, member: Member) -> TypeId {
        self.db.intern(Type::Literal(Literal::Sym(member.key.name)))
    }

    /// A call through a method's signatures, passing `receivers` first
    pub(super) fn call_signature(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        signature: &Signature,
        receivers: &[(TypeId, Span)],
        call: Call<'_>,
    ) -> TypeId {
        let callee = signature.callee(self.db).unwrap_or(self.db.unknown());
        self.call_with(at, state, operands, callee, receivers, call)
    }

    /// Reading a member: a field's value, a getter's or `(get)`'s result, or a
    /// method bound to the receiver
    pub(super) fn get(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        expr: &Expr,
        expected: Option<TypeId>,
    ) -> TypeId {
        let ExprKind::Get { object, member } = &expr.kind else {
            unreachable!("a member read")
        };
        let (member, span) = (*member, expr.span);
        let receiver = self.eval(at, state, operands, object);
        if receiver == self.db.bottom() {
            return receiver;
        }
        let call = Call {
            args: &[],
            expected,
            span,
            callee: None,
        };
        let Some(alternatives) = self.alternatives(receiver, span) else {
            let receiver = (receiver, object.span);
            return self.got(at, state, operands, receiver, None, member, call);
        };
        let mut result = self.db.bottom();
        for alternative in alternatives {
            let given = match alternative {
                Some(ty) => {
                    let alternative = (ty, object.span);
                    self.got(
                        at,
                        state,
                        operands,
                        alternative,
                        Some(receiver),
                        member,
                        call,
                    )
                }
                None => self.db.unknown(),
            };
            result = self.solver().lub(result, given);
        }
        result
    }

    /// Reading a member of a receiver that isn't a union, or of an alternative of
    /// the union `within`
    #[expect(clippy::too_many_arguments, reason = "a member read's parts")]
    fn got(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        receiver: (TypeId, Span),
        within: Option<TypeId>,
        member: Member,
        call: Call<'_>,
    ) -> TypeId {
        let unknown = self.db.unknown();
        let span = call.span;
        let leading = [receiver];
        let receiver = receiver.0;
        match self.resolve(receiver, member, span) {
            Resolved::Dynamic => unknown,
            Resolved::Missing | Resolved::Fallback { get: None, .. } => {
                self.missing(receiver, within, member, span);
                unknown
            }
            Resolved::Field(ty) => ty,
            Resolved::Method(signature, bound) => match (signature.single(), bound) {
                (None, _) => unknown,
                (Some(signature), true) => bound_method(self.db, signature).unwrap_or(unknown),
                (Some(signature), false) => signature,
            },
            Resolved::Property {
                getter: Some(getter),
                passed,
                ..
            } => {
                let leading = if passed { &leading[..] } else { &[] };
                let call = Call {
                    callee: Some(member),
                    ..call
                };
                self.call_signature(at, state, operands, &getter, leading, call)
            }
            Resolved::Property { getter: None, .. } => {
                self.misuse(member, span, MemberUse::Read);
                unknown
            }
            Resolved::Fallback { get: Some(get), .. } => {
                let name = (self.name_literal(member), span);
                let call = Call {
                    callee: Some(self.special("get")),
                    ..call
                };
                self.call_signature(at, state, operands, &get, &[leading[0], name], call)
            }
        }
    }

    /// A method call: the member got from the receiver, called with the call's
    /// arguments. A method is passed the receiver first.
    pub(super) fn invoke(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        expr: &Expr,
        expected: Option<TypeId>,
    ) -> TypeId {
        let ExprKind::Invoke {
            receiver: object,
            member,
            args,
        } = &expr.kind
        else {
            unreachable!("a method call")
        };
        let receiver = self.eval(at, state, operands, object);
        let call = Call {
            args,
            expected,
            span: expr.span,
            callee: None,
        };
        self.send(
            at,
            state,
            operands,
            (receiver, object.span),
            *member,
            &[],
            call,
        )
    }

    /// The type of the item a `for` takes, as the runtime gets it: the `(next)` of
    /// its iteratee's `(iter)`, or without an iteratee, of the function's ambient
    /// input. A function that doesn't declare its input reads it dynamically.
    pub(super) fn next(
        &mut self,
        at: At,
        state: &mut State,
        iteratee: Option<TypeId>,
        span: Span,
    ) -> TypeId {
        let iterator = match iteratee {
            Some(iteratee) => self.send_special(at, state, iteratee, "iter", span),
            None => match self.channels(at).0 {
                Some(input) => input,
                None => return self.db.unknown(),
            },
        };
        self.send_special(at, state, iterator, "next", span)
    }

    /// Call a receiver's special member with no arguments
    fn send_special(
        &mut self,
        at: At,
        state: &mut State,
        receiver: TypeId,
        name: &str,
        span: Span,
    ) -> TypeId {
        let call = Call {
            args: &[],
            expected: None,
            span,
            callee: None,
        };
        let member = self.special(name);
        let mut operands = VecDeque::new();
        self.send(
            at,
            state,
            &mut operands,
            (receiver, span),
            member,
            &[],
            call,
        )
    }

    /// Call a receiver's member with `leading` arguments, already evaluated, and
    /// then `call`'s own
    #[expect(clippy::too_many_arguments, reason = "a call through a member")]
    fn send(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        receiver: (TypeId, Span),
        member: Member,
        leading: &[(TypeId, Span)],
        call: Call<'_>,
    ) -> TypeId {
        let bottom = self.db.bottom();
        if receiver.0 == bottom {
            return self.call_with(at, state, operands, bottom, leading, call);
        }
        let targets =
            self.member_targets(at, state, operands, receiver, member, leading, call.span);
        self.call_targets(at, state, operands, targets, call)
    }

    /// The targets a call through a receiver's member reaches, passing `leading`
    /// before the call's own arguments: one for each alternative of a union, or of
    /// a member's value that is one. A getter or `(get)` is called here, with no
    /// arguments, for the value it gives.
    #[expect(clippy::too_many_arguments, reason = "a call through a member")]
    fn member_targets(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        receiver: (TypeId, Span),
        member: Member,
        leading: &[(TypeId, Span)],
        span: Span,
    ) -> Vec<CallTarget> {
        let Some(alternatives) = self.alternatives(receiver.0, span) else {
            return self.reached(at, state, operands, receiver, None, member, leading, span);
        };
        let mut targets = Vec::new();
        for alternative in alternatives {
            match alternative {
                Some(ty) => targets.extend(self.reached(
                    at,
                    state,
                    operands,
                    (ty, receiver.1),
                    Some(receiver.0),
                    member,
                    leading,
                    span,
                )),
                None => targets.push(CallTarget::dynamic(leading)),
            }
        }
        targets
    }

    /// [`Self::member_targets`] for a receiver that isn't a union, or an
    /// alternative of the union `within`
    #[expect(clippy::too_many_arguments, reason = "a call through a member")]
    fn reached(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        receiver: (TypeId, Span),
        within: Option<TypeId>,
        member: Member,
        leading: &[(TypeId, Span)],
        span: Span,
    ) -> Vec<CallTarget> {
        // A getter's result, or `(get)`'s, called with the arguments
        let got = Call {
            args: &[],
            expected: None,
            span,
            callee: None,
        };
        match self.resolve(receiver.0, member, span) {
            Resolved::Dynamic => vec![CallTarget::dynamic(leading)],
            Resolved::Missing | Resolved::Fallback { get: None, .. } => {
                self.missing(receiver.0, within, member, span);
                vec![CallTarget::dynamic(leading)]
            }
            Resolved::Field(ty) => self.value_targets((ty, receiver.1), leading, span),
            Resolved::Method(signature, bound) => {
                let receivers = match bound {
                    true => [receiver].iter().chain(leading).copied().collect(),
                    false => leading.to_vec(),
                };
                vec![CallTarget {
                    signature,
                    receivers,
                    member: Some(member),
                    instance: None,
                }]
            }
            Resolved::Property {
                getter: Some(getter),
                passed,
                ..
            } => {
                let receivers = if passed { &[receiver][..] } else { &[] };
                let got = Call {
                    callee: Some(member),
                    ..got
                };
                let value = self.call_signature(at, state, operands, &getter, receivers, got);
                self.value_targets((value, receiver.1), leading, span)
            }
            Resolved::Property { getter: None, .. } => {
                self.misuse(member, span, MemberUse::Read);
                vec![CallTarget::dynamic(leading)]
            }
            Resolved::Fallback { get: Some(get), .. } => {
                let name = (self.name_literal(member), span);
                let got = Call {
                    callee: Some(self.special("get")),
                    ..got
                };
                let value = self.call_signature(at, state, operands, &get, &[receiver, name], got);
                self.value_targets((value, receiver.1), leading, span)
            }
        }
    }

    /// Writing a member: a field's type must admit the value, and a setter or
    /// `(set)` is called with it
    #[expect(clippy::too_many_arguments, reason = "a write's parts")]
    pub(super) fn set(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        object: &Expr,
        member: Member,
        value: &Expr,
        span: Span,
    ) {
        let receiver = self.eval(at, state, operands, object);
        // Each alternative's member, as a union's alternatives are written
        let (within, alternatives) = match self.alternatives(receiver, span) {
            _ if receiver == self.db.bottom() => (None, Vec::new()),
            Some(alternatives) => (Some(receiver), alternatives),
            None => (None, vec![Some(receiver)]),
        };
        let resolved: Vec<_> = (alternatives.into_iter().flatten())
            .map(|ty| (ty, self.resolve(ty, member, span)))
            .collect();
        // The value is expected to be what every field takes, if they agree
        let mut fields = resolved.iter().map(|(_, resolved)| match resolved {
            Resolved::Field(ty) => Some(*ty),
            _ => None,
        });
        let expected = fields
            .next()
            .flatten()
            .filter(|&ty| fields.all(|other| other == Some(ty)));
        let written = self.expect(at, state, operands, value, expected);
        let written = (written, value.span);
        for (receiver, resolved) in resolved {
            let receiver = (receiver, object.span);
            self.written(
                at, state, operands, receiver, within, member, resolved, written, span,
            );
        }
    }

    /// Writing `written` to a resolved member of a receiver that isn't a union, or
    /// of an alternative of the union `within`
    #[expect(clippy::too_many_arguments, reason = "a write's parts")]
    fn written(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        receiver: (TypeId, Span),
        within: Option<TypeId>,
        member: Member,
        resolved: Resolved,
        written: (TypeId, Span),
        span: Span,
    ) {
        let call = Call {
            args: &[],
            expected: None,
            span,
            callee: None,
        };
        match resolved {
            Resolved::Dynamic => {}
            Resolved::Missing | Resolved::Fallback { set: None, .. } => {
                self.missing(receiver.0, within, member, span);
            }
            Resolved::Field(ty) => self.store(at, written.0, ty, written.1),
            Resolved::Method(..) => self.misuse(member, span, MemberUse::Method),
            Resolved::Property {
                setter: Some(setter),
                passed,
                ..
            } => {
                let receivers = [receiver, written];
                let receivers = &receivers[usize::from(!passed)..];
                let call = Call {
                    callee: Some(member),
                    ..call
                };
                self.call_signature(at, state, operands, &setter, receivers, call);
            }
            Resolved::Property { setter: None, .. } => {
                self.misuse(member, span, MemberUse::Write);
            }
            Resolved::Fallback { set: Some(set), .. } => {
                let receivers = [receiver, (self.name_literal(member), span), written];
                let call = Call {
                    callee: Some(self.special("set")),
                    ..call
                };
                self.call_signature(at, state, operands, &set, &receivers, call);
            }
        }
    }

    /// Indexing: `(index)` called with the index
    pub(super) fn index(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        expr: &Expr,
        expected: Option<TypeId>,
    ) -> TypeId {
        let ExprKind::Index { object, index } = &expr.kind else {
            unreachable!("an index")
        };
        let receiver = self.eval(at, state, operands, object);
        let key = self.eval(at, state, operands, index);
        let call = Call {
            args: &[],
            expected,
            span: expr.span,
            callee: None,
        };
        let member = self.special("index");
        let receiver = (receiver, object.span);
        self.send(
            at,
            state,
            operands,
            receiver,
            member,
            &[(key, index.span)],
            call,
        )
    }

    /// Assigning at an index: `(assign)` called with the index and the value
    pub(super) fn assign_index(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        [object, index, value]: [&Expr; 3],
        span: Span,
    ) {
        let receiver = self.eval(at, state, operands, object);
        let key = self.eval(at, state, operands, index);
        let written = self.eval(at, state, operands, value);
        let call = Call {
            args: &[],
            expected: None,
            span,
            callee: None,
        };
        let member = self.special("assign");
        let leading = [(key, index.span), (written, value.span)];
        self.send(
            at,
            state,
            operands,
            (receiver, object.span),
            member,
            &leading,
            call,
        );
    }

    /// A unary operator: `!` is a `Bool`, and the others call the operand's
    /// special method
    pub(super) fn unary(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        expr: &Expr,
    ) -> TypeId {
        let ExprKind::Unary { op, operand } = &expr.kind else {
            unreachable!("a unary operator")
        };
        let value = self.eval(at, state, operands, operand);
        if value == self.db.bottom() {
            return value;
        }
        let name = match op {
            Op::Bang => return self.intrinsic(Intrinsic::Bool),
            Op::Minus => "neg",
            Op::Tilde => "bnot",
            _ => return self.db.unknown(),
        };
        let call = Call {
            args: &[],
            expected: None,
            span: expr.span,
            callee: None,
        };
        let member = self.special(name);
        self.send(
            at,
            state,
            operands,
            (value, operand.span),
            member,
            &[],
            call,
        )
    }

    /// A binary operator: `==` and `!=` are `Bool`s, and the comparisons require
    /// `(lt)` and are `Bool`s. The others call their special methods. As the
    /// runtime does, an operator its left operand lacks is dispatched on its right:
    /// a commutative one calls the same method with the operands swapped, and
    /// another its reflected method, such as `(rsub)` for `(sub)`.
    pub(super) fn binary(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        expr: &Expr,
    ) -> TypeId {
        let ExprKind::Binary { op, operands: pair } = &expr.kind else {
            unreachable!("a binary operator")
        };
        let bottom = self.db.bottom();
        let [left, right] = &**pair;
        let lhs = self.eval(at, state, operands, left);
        let rhs = self.eval(at, state, operands, right);
        if lhs == bottom || rhs == bottom {
            return bottom;
        }
        let boolean = self.intrinsic(Intrinsic::Bool);
        // The method, the one the right operand is dispatched to, and whether it's a
        // comparison
        let (name, reflected, compared) = match op {
            Op::EqEq | Op::BangEq => return boolean,
            Op::Lt | Op::LtEq | Op::Gt | Op::GtEq => ("lt", "lt", true),
            Op::Plus => ("add", "add", false),
            Op::Minus => ("sub", "rsub", false),
            Op::Star => ("mul", "mul", false),
            Op::Slash => ("div", "rdiv", false),
            Op::SlashSlash => ("ediv", "rediv", false),
            Op::Percent => ("mod", "rmod", false),
            Op::Amp => ("band", "band", false),
            Op::Bar => ("bor", "bor", false),
            Op::Caret => ("bxor", "bxor", false),
            Op::LtLt => ("shl", "shl", false),
            Op::GtGt => ("shr", "shr", false),
            _ => return self.db.unknown(),
        };
        let call = Call {
            args: &[],
            expected: None,
            span: expr.span,
            callee: None,
        };
        let (member, reflected) = (self.special(name), self.special(reflected));
        let (lhs, rhs) = ((lhs, left.span), (rhs, right.span));
        // Each alternative of the left operand dispatches on its own
        let (within, alternatives) = match self.alternatives(lhs.0, call.span) {
            Some(alternatives) => (Some(lhs.0), alternatives),
            None => (None, vec![Some(lhs.0)]),
        };
        let mut targets = Vec::new();
        for alternative in alternatives {
            let Some(ty) = alternative else {
                targets.push(CallTarget::dynamic(&[]));
                continue;
            };
            let alternative = (ty, lhs.1);
            let span = call.span;
            let reflect = self.lacks(ty, member, span) && !self.lacks(rhs.0, reflected, span);
            targets.extend(match reflect {
                true => {
                    self.member_targets(at, state, operands, rhs, reflected, &[alternative], span)
                }
                false => self.reached(
                    at,
                    state,
                    operands,
                    alternative,
                    within,
                    member,
                    &[rhs],
                    span,
                ),
            });
        }
        let result = self.call_targets(at, state, operands, targets, call);
        match compared && result != bottom {
            true => boolean,
            false => result,
        }
    }

    /// Whether `ty` lacks a member: each alternative, if it's a union
    fn lacks(&mut self, ty: TypeId, member: Member, span: Span) -> bool {
        match self.alternatives(ty, span) {
            Some(alternatives) => alternatives.into_iter().all(|alternative| {
                alternative
                    .is_some_and(|ty| matches!(self.resolve(ty, member, span), Resolved::Missing))
            }),
            None => matches!(self.resolve(ty, member, span), Resolved::Missing),
        }
    }

    /// A range: a `Range` constructed from its bounds. Since both have the same
    /// type, the bounds present are passed first.
    pub(super) fn range(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        expr: &Expr,
    ) -> TypeId {
        let ExprKind::Range { bounds } = &expr.kind else {
            unreachable!("a range")
        };
        let bottom = self.db.bottom();
        let bounds: Vec<_> = (bounds.iter().flatten())
            .map(|bound| (self.eval(at, state, operands, bound), bound.span))
            .collect();
        if bounds.iter().any(|&(ty, _)| ty == bottom) {
            return bottom;
        }
        let Some(class) = self.designated(Designated::Range) else {
            return self.db.unknown();
        };
        let call = Call {
            args: &[],
            expected: None,
            span: expr.span,
            callee: None,
        };
        match self.solver().constructor(class) {
            Constructor::Init(Some(signature))
                if let Some(constructor) = signature.callee(self.db) =>
            {
                self.call_with(at, state, operands, constructor, &bounds, call)
            }
            _ => self.db.unknown(),
        }
    }
}
