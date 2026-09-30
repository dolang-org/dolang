//! Member use: reading, writing and calling a receiver's members, indexing,
//! operators, ranges and constructors. Each finds its member with the solver's
//! lookup ([`Solver::member`]) and checks the use as the runtime makes it: as a
//! call through the member, passing the receiver first to a method. A receiver
//! the lookup can't decide, such as a union, is an explicit residual. A call
//! through an overloaded method chooses among its overloads (see
//! [`Flow::call_overloaded`]); any other use of one is dynamic.
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
        solver::{FoundKind, Issue, Lookup, Signatures},
        r#type::{
            Argument, Binder, BoundRef, DeclId, Element, Function, Intrinsic, Kind, Literal,
            MemberKey, Multiplicity, Scope, Type, TypeId,
        },
    },
};

/// A method's signatures, each with its receiver parameter. A call through an
/// overloaded method chooses among its overloads (see [`Flow::call_overloaded`]);
/// any other use of one is dynamic.
#[derive(Clone, Default)]
struct Signature {
    /// Its `@def` signatures, empty unless it's overloaded
    overloads: Vec<TypeId>,
    /// Its implementation's signature, if it has one
    implementation: Option<TypeId>,
}

impl Signature {
    /// Its one signature, unless it's overloaded
    fn single(&self) -> Option<TypeId> {
        self.overloads.is_empty().then_some(self.implementation)?
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
    Property {
        getter: Option<Signature>,
        setter: Option<Signature>,
    },
    /// No member, but the instance's class has these for any ordinary name
    Fallback {
        get: Option<Signature>,
        set: Option<Signature>,
    },
}

/// How a class object is called
enum Constructor {
    /// Its class-level `(call)`, which is passed the class object first. `None`
    /// if it's overloaded.
    Call(Option<TypeId>),
    /// Instantiation, running `(init)`: a function from `(init)`'s arguments to
    /// the instance, quantified over the class's binders. `None` if `(init)` is
    /// overloaded or its signature doesn't take a receiver.
    Init(Option<TypeId>),
    Dynamic,
}

impl Flow<'_, '_> {
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
        let signature = |signatures: &Signatures| {
            let reify = |&term| solver.reify(term).ok();
            let overloads = (signatures.overloads.iter())
                .map(reify)
                .collect::<Option<_>>();
            let implementation = signatures.implementation.as_ref().map(reify);
            match (overloads, implementation) {
                (Some(overloads), None) => Signature {
                    overloads,
                    implementation: None,
                },
                (Some(overloads), Some(Some(implementation))) => Signature {
                    overloads,
                    implementation: Some(implementation),
                },
                _ => Signature::default(),
            }
        };
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
                    let bound = match found.scope {
                        Scope::Instance => !object,
                        Scope::Class => true,
                        Scope::Static => false,
                    };
                    Resolved::Method(method(kind), bound)
                }
                FoundKind::Property { getter, setter } => Resolved::Property {
                    getter: signatures(getter),
                    setter: signatures(setter),
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

    /// Report a missing member
    fn missing(&mut self, receiver: TypeId, member: Member, span: Span) {
        if !self.observing() {
            return;
        }
        let receiver = self.tables.render_type(self.db, receiver);
        let name = self.member_name(member);
        self.problem(Problem::MissingMember {
            span,
            receiver,
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

    fn member_name(&self, member: Member) -> String {
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
    fn call_signature(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        signature: &Signature,
        receivers: &[(TypeId, Span)],
        call: Call<'_>,
    ) -> TypeId {
        let Signature {
            overloads,
            implementation,
        } = signature;
        if overloads.is_empty() {
            let callee = implementation.unwrap_or(self.db.unknown());
            return self.call_with(at, state, operands, callee, receivers, call);
        }
        self.call_overloaded(
            at,
            state,
            operands,
            overloads,
            *implementation,
            receivers,
            call,
        )
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
        let unknown = self.db.unknown();
        let receiver = self.eval(at, state, operands, object);
        if receiver == self.db.bottom() {
            return receiver;
        }
        let call = Call {
            args: &[],
            expected,
            span,
        };
        let leading = [(receiver, object.span)];
        match self.resolve(receiver, member, span) {
            Resolved::Dynamic => unknown,
            Resolved::Missing | Resolved::Fallback { get: None, .. } => {
                self.missing(receiver, member, span);
                unknown
            }
            Resolved::Field(ty) => ty,
            Resolved::Method(signature, bound) => match (signature.single(), bound) {
                (None, _) => unknown,
                (Some(signature), true) => self.bound_method(signature).unwrap_or(unknown),
                (Some(signature), false) => signature,
            },
            Resolved::Property {
                getter: Some(getter),
                ..
            } => self.call_signature(at, state, operands, &getter, &leading, call),
            Resolved::Property { getter: None, .. } => {
                self.misuse(member, span, MemberUse::Read);
                unknown
            }
            Resolved::Fallback { get: Some(get), .. } => {
                let name = (self.name_literal(member), span);
                self.call_signature(at, state, operands, &get, &[leading[0], name], call)
            }
        }
    }

    /// A method bound to its receiver: its signature without its receiver
    /// parameter. `None` if that parameter mentions the method's own binders, which
    /// binding would have to solve.
    fn bound_method(&self, signature: TypeId) -> Option<TypeId> {
        let db = self.db;
        let (binders, body) = match db.ty(signature) {
            Type::Quantified { binders, body } => (Some(binders.clone()), *body),
            _ => (None, signature),
        };
        let (function, receiver) = self.drop_receiver(body)?;
        let Some(binders) = binders else {
            return Some(function);
        };
        let mut own = false;
        db.walk(receiver, |node, depth| {
            if let Type::Bound { reference, .. } = *db.ty(node) {
                own |= u32::from(reference.depth) >= depth;
            }
        });
        (!own).then(|| {
            db.intern(Type::Quantified {
                binders,
                body: function,
            })
        })
    }

    /// A function type without its first parameter, a required positional one,
    /// and that parameter's type
    fn drop_receiver(&self, function: TypeId) -> Option<(TypeId, TypeId)> {
        let db = self.db;
        let Type::Function(function) = db.ty(function) else {
            return None;
        };
        let Type::Schema(items) = db.ty(function.params) else {
            return None;
        };
        let (first, rest) = items.split_first()?;
        let (Multiplicity::Required, &Element::Positional(receiver)) =
            (first.multiplicity, &first.element)
        else {
            return None;
        };
        let params = db.intern(Type::Schema(rest.iter().cloned().collect()));
        let dropped = db.intern(Type::Function(Function {
            params,
            ..function.clone()
        }));
        Some((dropped, receiver))
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
        let unknown = self.db.unknown();
        let span = call.span;
        if receiver.0 == bottom {
            return self.call_with(at, state, operands, bottom, leading, call);
        }
        let with_receiver: Vec<_> = [receiver]
            .into_iter()
            .chain(leading.iter().copied())
            .collect();
        // A getter's result, or `(get)`'s, called with the arguments
        let got = Call {
            args: &[],
            expected: None,
            span,
        };
        match self.resolve(receiver.0, member, span) {
            Resolved::Dynamic => self.call_with(at, state, operands, unknown, leading, call),
            Resolved::Missing | Resolved::Fallback { get: None, .. } => {
                self.missing(receiver.0, member, span);
                self.call_with(at, state, operands, unknown, leading, call)
            }
            Resolved::Field(ty) => self.call_with(at, state, operands, ty, leading, call),
            Resolved::Method(signature, true) => {
                self.call_signature(at, state, operands, &signature, &with_receiver, call)
            }
            Resolved::Method(signature, false) => {
                self.call_signature(at, state, operands, &signature, leading, call)
            }
            Resolved::Property {
                getter: Some(getter),
                ..
            } => {
                let value = self.call_signature(at, state, operands, &getter, &[receiver], got);
                self.call_with(at, state, operands, value, leading, call)
            }
            Resolved::Property { getter: None, .. } => {
                self.misuse(member, span, MemberUse::Read);
                self.call_with(at, state, operands, unknown, leading, call)
            }
            Resolved::Fallback { get: Some(get), .. } => {
                let name = (self.name_literal(member), span);
                let value = self.call_signature(at, state, operands, &get, &[receiver, name], got);
                self.call_with(at, state, operands, value, leading, call)
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
        let resolved = match receiver == self.db.bottom() {
            true => Resolved::Dynamic,
            false => self.resolve(receiver, member, span),
        };
        let expected = match resolved {
            Resolved::Field(ty) => Some(ty),
            _ => None,
        };
        let written = self.expect(at, state, operands, value, expected);
        let call = Call {
            args: &[],
            expected: None,
            span,
        };
        let receiver = (receiver, object.span);
        let written = (written, value.span);
        match resolved {
            Resolved::Dynamic => {}
            Resolved::Missing | Resolved::Fallback { set: None, .. } => {
                self.missing(receiver.0, member, span);
            }
            Resolved::Field(ty) => self.store(at, written.0, ty, written.1),
            Resolved::Method(..) => self.misuse(member, span, MemberUse::Method),
            Resolved::Property {
                setter: Some(setter),
                ..
            } => {
                let receivers = [receiver, written];
                self.call_signature(at, state, operands, &setter, &receivers, call);
            }
            Resolved::Property { setter: None, .. } => {
                self.misuse(member, span, MemberUse::Write);
            }
            Resolved::Fallback { set: Some(set), .. } => {
                let receivers = [receiver, (self.name_literal(member), span), written];
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
        };
        let (member, reflected) = (self.special(name), self.special(reflected));
        let (lhs, rhs) = ((lhs, left.span), (rhs, right.span));
        let reflect = matches!(self.resolve(lhs.0, member, call.span), Resolved::Missing)
            && !matches!(self.resolve(rhs.0, reflected, call.span), Resolved::Missing);
        let result = match reflect {
            true => self.send(at, state, operands, rhs, reflected, &[lhs], call),
            false => self.send(at, state, operands, lhs, member, &[rhs], call),
        };
        match compared && result != bottom {
            true => boolean,
            false => result,
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
        };
        match self.constructor(class) {
            Constructor::Init(Some(constructor)) => {
                self.call_with(at, state, operands, constructor, &bounds, call)
            }
            _ => self.db.unknown(),
        }
    }

    /// Calling a class object: its class-level `(call)` if it has one, and
    /// otherwise instantiation, which runs `(init)` and gives the instance
    pub(super) fn construct(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        object: TypeId,
        class: DeclId,
        call: Call<'_>,
    ) -> TypeId {
        let unknown = self.db.unknown();
        let generic = matches!(self.db.ty(object), Type::Quantified { .. });
        match self.constructor(class) {
            Constructor::Call(Some(signature)) if !generic => {
                let leading = [(object, call.span)];
                self.call_with(at, state, operands, signature, &leading, call)
            }
            Constructor::Init(Some(constructor)) => {
                self.call_with(at, state, operands, constructor, &[], call)
            }
            Constructor::Init(None) if !generic => {
                // Its arguments are unchecked, but it gives an instance
                let result = self.call_with(at, state, operands, unknown, &[], call);
                match result == self.db.bottom() {
                    true => result,
                    false => self.db.intern(Type::Decl(class)),
                }
            }
            _ => self.call_with(at, state, operands, unknown, &[], call),
        }
    }

    /// How a class object is called. `(init)` is looked up on the class applied to
    /// its rigids, and the constructor's type abstracts them again.
    fn constructor(&self, class: DeclId) -> Constructor {
        let db = self.db;
        let binders: Vec<Binder> = match db.ty(db.declaration(class).ty) {
            Type::Quantified { binders, .. } => binders.to_vec(),
            _ => Vec::new(),
        };
        let base = db.intern(Type::Decl(class));
        let instance = match binders.is_empty() {
            true => base,
            false => {
                let args = (binders.iter().enumerate())
                    .map(|(slot, binder)| {
                        Argument::Positional(db.intern(Type::Bound {
                            reference: BoundRef::new(0, slot),
                            kind: binder.kind,
                        }))
                    })
                    .collect();
                db.intern(Type::Apply {
                    base,
                    args,
                    kind: Kind::Type,
                })
            }
        };
        let instance = db.substitute(instance, &db.rigids(class));
        let Some(class_type) = db.intrinsic(Intrinsic::Type) else {
            return Constructor::Dynamic;
        };
        let object = db.intern(Type::Apply {
            base: class_type,
            args: vec![Argument::Positional(instance)].into(),
            kind: Kind::Type,
        });
        let mut solver = self.solver();
        solver.assume(class);
        let key = |name| MemberKey {
            name: db.intern_symbol(name),
            special: true,
            private: false,
        };
        let signature = |kind: &FoundKind| match kind {
            FoundKind::Method(Signatures {
                overloads,
                implementation: Some(signature),
            }) if overloads.is_empty() => solver.reify(*signature).ok(),
            _ => None,
        };
        match solver.member(solver.closed(object), key("call")) {
            Ok(Lookup::Found(found)) if found.scope == Scope::Class => {
                return Constructor::Call(signature(&found.kind));
            }
            Ok(Lookup::Found(_) | Lookup::Missing) => {}
            Ok(Lookup::Dynamic | Lookup::Fallback { .. }) | Err(_) => return Constructor::Dynamic,
        }
        let initializer = match solver.member(solver.closed(instance), key("init")) {
            Ok(Lookup::Found(found)) => match signature(&found.kind) {
                Some(signature) => self.initializer(signature, instance),
                None => return Constructor::Init(None),
            },
            Ok(Lookup::Missing) => Some(db.intern(Type::Function(Function {
                params: db.intern(Type::Schema(Vec::new().into())),
                result: instance,
                input: None,
                output: None,
            }))),
            Ok(Lookup::Dynamic | Lookup::Fallback { .. }) | Err(_) => {
                return Constructor::Dynamic;
            }
        };
        let constructor = initializer
            .and_then(|ty| db.abstract_rigids(ty, class).ok())
            .map(|ty| db.merge_groups(&binders, ty));
        Constructor::Init(constructor)
    }

    /// `(init)`'s signature taking the arguments a class object is called with,
    /// and giving the instance
    fn initializer(&self, signature: TypeId, instance: TypeId) -> Option<TypeId> {
        let db = self.db;
        let (binders, body) = match db.ty(signature) {
            Type::Quantified { binders, body } => (Some(binders.clone()), *body),
            _ => (None, signature),
        };
        let (function, _) = self.drop_receiver(body)?;
        let Type::Function(function) = db.ty(function) else {
            unreachable!("a function without its receiver")
        };
        let function = db.intern(Type::Function(Function {
            result: instance,
            ..function.clone()
        }));
        Some(match binders {
            Some(binders) => db.intern(Type::Quantified {
                binders,
                body: function,
            }),
            None => function,
        })
    }
}
