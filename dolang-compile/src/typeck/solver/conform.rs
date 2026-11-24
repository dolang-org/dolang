//! Conformance of a declaration to its supertypes.
//!
//! A class or protocol conforms to a supertype when each public member the
//! supertype has is matched by a compatible member of its own: an override
//! declared in it, or what it inherits. A class's members are what the runtime
//! inherits, so a member it only claims through a protocol is missing. Members
//! are compared by kind: fields invariantly, as they are mutable, methods and
//! property accessors by the signatures they're called with. A field is also
//! upheld by a property with a getter and a setter, and a property by a field,
//! each compared as accessors that read and set the field. The supertype's
//! signature takes the conforming declaration's own type as its receiver, or
//! `Type` of it for a class method, since only calls on its instances or its
//! object matter.
//!
//! A method is called with its overloads, or its implementation's signature if
//! it has none: an implementation behind overloads is never what callers see.
//! So each required signature must be below one provided signature, chosen by
//! trials as when an overloaded function is passed as a function. A required
//! signature covered only by several provided ones together, such as
//! `(Int | Str) -> (Int | Str)` by `(Int) -> Int` and `(Str) -> Str`, is not
//! covered.
//!
//! A generic class reached more than once in a declaration's ancestry is reached
//! only as its first instance, so that instance must be below the others it
//! masks, by the class's variances, for the ancestor walk to stop there.
//!
//! A class that doesn't name a protocol is judged below it by the same
//! requirements, with its own type as each required signature's receiver.
//!
//! Every requirement is a set of ordinary subtype judgments for the caller to
//! constrain, so no verdict is cached here.

use std::ops::ControlFlow;

use super::member::{Found, FoundKind};
use super::*;
use crate::typeck::r#type::{MemberKey, Scope};

/// What a member of a supertype requires of a conforming declaration
#[derive(Clone, Debug)]
pub(crate) struct Requirement {
    pub(crate) key: MemberKey,
    pub(crate) scope: Scope,
    /// The class declaring the member that provides it
    pub(crate) provider: Option<DeclId>,
    /// The class declaring the member required
    pub(crate) required: DeclId,
    pub(crate) kind: RequirementKind,
}

#[derive(Clone, Debug)]
pub(crate) enum RequirementKind {
    /// Each pair's actual must be below its expected. A pair is reversed when
    /// it relates the required member's type below the provided one's, as a
    /// field's second pair does.
    Relate(Vec<(Term, Term, bool)>),
    /// Nothing provides a protocol's member
    Missing,
    /// The provider is a different kind of member, or a property lacks an
    /// accessor the required member needs. Each is described as "a field", "a
    /// read-only property" and so on.
    Changed {
        provided: &'static str,
        required: &'static str,
    },
    Undecided(Issue),
}

/// How to narrow a required signature's receiver: to instances of the class of
/// the closed `instance`, which found the member through its supertype `through`,
/// inside `Type` for a class method's (`object`). A `structural` receiver is
/// `instance` itself, which doesn't reach the supertype.
#[derive(Clone, Copy)]
struct Narrowing {
    instance: TypeId,
    through: Term,
    object: bool,
    structural: bool,
}

/// How a class inherits a class that a supertype names
#[derive(Clone, Debug)]
pub(crate) enum Inheritance {
    /// It doesn't inherit it at runtime
    Unreached,
    /// The class as inherited must be below the class as named
    Relate(Term, Term),
    Undecided(Issue),
}

/// An instance of a generic class in a declaration's ancestry
#[derive(Clone, Debug)]
pub(crate) struct Reached {
    pub(crate) ty: TypeId,
    /// The index of the declaration's supertype it's reached through
    pub(crate) supertype: usize,
    /// Whether it is that supertype itself
    pub(crate) direct: bool,
    /// Whether the runtime inherits it, through every edge on the way
    pub(crate) inherited: bool,
}

/// A later instance of a generic class that the first instance masks
#[derive(Clone, Debug)]
pub(crate) struct Masked {
    pub(crate) first: Reached,
    pub(crate) other: Reached,
    /// `first` is only claimed, but `other` is inherited at runtime
    pub(crate) reorder: bool,
}

impl Solver<'_> {
    /// The requirements `supertype`'s members place on the declaration whose
    /// instances have the closed type `instance`. With `runtime`, only what the
    /// runtime inherits provides members, as for a class; a protocol provides
    /// what it claims too. With `structural`, `instance` is judged below a
    /// protocol it doesn't reach, so the required signatures take `instance`
    /// itself as their receiver, and keep the supertype's arguments unsolved.
    pub(crate) fn conformance(
        &self,
        instance: TypeId,
        supertype: Term,
        runtime: bool,
        structural: bool,
    ) -> Result<Vec<Requirement>, Issue> {
        let mut keys = Vec::new();
        for decl in self.lineage(supertype)? {
            for (key, member) in self.db.declaration(decl).members.iter() {
                let scope = member.scope();
                if !self.db.required_member(*key, member) || keys.contains(&(*key, scope)) {
                    continue;
                }
                keys.push((*key, scope));
            }
        }
        let mut requirements = Vec::new();
        for (key, scope) in keys {
            let required = match self.inherited_member(supertype, key, scope, false)? {
                Lookup::Found(found) => found,
                _ => continue,
            };
            let provided = match self.inherited_member(self.closed(instance), key, scope, runtime) {
                Ok(Lookup::Found(found)) => Some(found),
                Ok(Lookup::Missing | Lookup::Fallback { .. }) => None,
                Ok(Lookup::Dynamic) => continue,
                Err(issue) => {
                    requirements.push(Requirement {
                        key,
                        scope,
                        provider: None,
                        required: required.class,
                        kind: RequirementKind::Undecided(issue),
                    });
                    continue;
                }
            };
            // A class method's receiver is `Type` of an instance, narrowed inside
            let receiver = Some(Narrowing {
                instance,
                through: supertype,
                object: scope == Scope::Class,
                structural,
            });
            let kind = match &provided {
                // A class's member is upheld by being below the class, which a
                // structural judgment derives for each class the protocol names
                _ if structural && !self.is_protocol(required.class) => continue,
                // The member required, as the supertype has it, meets itself
                Some(provided) if self.same_member(provided, &required) => continue,
                Some(provided) => self
                    .requirement(provided, &required, receiver)
                    .unwrap_or_else(RequirementKind::Undecided),
                None if self.is_protocol(required.class) => RequirementKind::Missing,
                // A class a class claims is checked by `claimed_classes`
                None => continue,
            };
            if matches!(&kind, RequirementKind::Relate(pairs) if pairs.is_empty()) {
                continue;
            }
            requirements.push(Requirement {
                key,
                scope,
                provider: provided.map(|found| found.class),
                required: required.class,
                kind,
            });
        }
        Ok(requirements)
    }

    /// Judge the closed `actual`, a class or protocol, below `protocol`, a
    /// protocol it doesn't reach, by what the protocol requires: each member is
    /// related as [`Self::conformance`] finds it, and `actual` must be below
    /// each class the protocol's ancestry names, as the protocol names it. A
    /// class provides what the runtime inherits, and a protocol what it claims
    /// too. Each is derived from `obligation`, so the protocol's arguments are
    /// solved through the members' types.
    pub(super) fn structural_conformance(
        &self,
        obligation: ObligationId,
        actual: TypeId,
        protocol: Term,
        runtime: bool,
    ) -> Result<(), Issue> {
        let requirements = self.conformance(actual, protocol, runtime, true)?;
        // A missing or changed member decides it whatever else is undecided
        let mut undecided = None;
        for requirement in &requirements {
            let key = requirement.key;
            match requirement.kind {
                RequirementKind::Missing => {
                    trace!(
                        self,
                        "{} lacks `{}` of {}",
                        self.render(self.closed(actual)),
                        self.db.member_name(key),
                        self.render(protocol)
                    );
                    return Err(Issue::Contradiction(Contradiction::MissingMember(key)));
                }
                RequirementKind::Changed { provided, required } => {
                    return Err(Issue::Contradiction(Contradiction::ChangedMember {
                        key,
                        provided,
                        required,
                    }));
                }
                RequirementKind::Undecided(issue) => {
                    undecided.get_or_insert(issue);
                }
                RequirementKind::Relate(_) => {}
            }
        }
        if let Some(issue) = undecided {
            return Err(issue);
        }
        let mut claimed = Vec::new();
        let value = self.db.intrinsic_decl(Intrinsic::Value);
        for decl in self.lineage(protocol)? {
            if self.db.declaration(decl).source.kind != DeclKind::Class || Some(decl) == value {
                continue;
            }
            let Reach::Reached(arguments) = self.reach(protocol, decl)? else {
                continue;
            };
            let class = match arguments.is_empty() {
                true => self.closed(self.db.intern(Type::Decl(decl))),
                false => self.view(
                    self.instance(decl),
                    self.intern_environment(self.empty_environment(), arguments),
                ),
            };
            claimed.push(class);
        }
        for requirement in requirements {
            let RequirementKind::Relate(pairs) = requirement.kind else {
                continue;
            };
            for (provided, required, reversed) in pairs {
                let step = Step::Member {
                    key: requirement.key,
                    reversed,
                };
                self.derive(obligation, provided, required, step);
            }
        }
        for class in claimed {
            self.derive(obligation, self.closed(actual), class, Step::Claimed);
        }
        Ok(())
    }

    /// Compare a provided member with a required one of the same name. With
    /// `receiver`, the required signatures' receivers are narrowed to the instance
    /// type's class.
    fn requirement(
        &self,
        provided: &Found,
        required: &Found,
        receiver: Option<Narrowing>,
    ) -> Result<RequirementKind, Issue> {
        let changed = || RequirementKind::Changed {
            provided: describe(&provided.kind),
            required: describe(&required.kind),
        };
        let accessor = |signatures: &Signatures| accessor_signature(signatures);
        let mut pairs = Vec::new();
        // Each required signature, called on the conforming declaration's
        // instances, must be below what the provided member is called as: one
        // of its overloads, chosen by trials as for any overloaded function
        let relate = |pairs: &mut Vec<_>, actual: &Signatures, required: &Signatures| {
            let Some(actual) = self.overload_set(actual)? else {
                return Ok::<_, Issue>(());
            };
            for signature in interface(required) {
                let expected = self.rewrite(signature, receiver, None, None)?;
                pairs.push((actual, expected, false));
            }
            Ok(())
        };
        match (&provided.kind, &required.kind) {
            (FoundKind::Unknown(_), _) | (_, FoundKind::Unknown(_)) => {}
            (&FoundKind::Field(actual), &FoundKind::Field(expected)) => {
                pairs.push((actual, expected, false));
                pairs.push((expected, actual, true));
            }
            (FoundKind::Method(actual), FoundKind::Method(required)) => {
                relate(&mut pairs, actual, required)?;
            }
            (
                FoundKind::Property { getter, setter },
                FoundKind::Property {
                    getter: required_getter,
                    setter: required_setter,
                },
            ) => {
                if (required_getter.is_some() && getter.is_none())
                    || (required_setter.is_some() && setter.is_none())
                {
                    return Ok(changed());
                }
                for (actual, required) in [(getter, required_getter), (setter, required_setter)] {
                    if let (Some(actual), Some(required)) = (actual, required) {
                        relate(&mut pairs, actual, required)?;
                    }
                }
            }
            // A field is upheld by a property that reads and writes it: the
            // getter may narrow what's read, but the setter must take all the
            // field holds
            (
                FoundKind::Property {
                    getter: Some(getter),
                    setter: Some(setter),
                },
                &FoundKind::Field(field),
            ) => {
                // A field whose type isn't solved yet, as a structural judgment
                // leaves its protocol's arguments, is related to what the getter
                // returns and what the setter takes
                let (getter, setter) = (accessor(getter)?, accessor(setter)?);
                let field = match self.reify(field) {
                    Ok(field) => field,
                    Err(Residual::Inference) => {
                        if let Some(getter) = getter {
                            pairs.push((self.accessor_part(getter, false)?, field, false));
                        }
                        if let Some(setter) = setter {
                            pairs.push((field, self.accessor_part(setter, true)?, true));
                        }
                        return Ok(RequirementKind::Relate(pairs));
                    }
                    Err(residual) => return Err(residual.into()),
                };
                if let Some(getter) = getter {
                    let expected = self.rewrite(getter, None, None, Some(field))?;
                    pairs.push((getter, expected, false));
                }
                if let Some(setter) = setter {
                    let expected = self.rewrite(setter, None, Some(field), None)?;
                    pairs.push((setter, expected, false));
                }
            }
            // A property is upheld by a field, as by accessors that read and
            // set it: what it holds must be readable, and what's set held
            (
                &FoundKind::Field(field),
                FoundKind::Property {
                    getter: required_getter,
                    setter: required_setter,
                },
            ) => {
                let field = self.reify(field)?;
                for (accessor, value) in [(required_getter, false), (required_setter, true)] {
                    let Some(signature) = accessor.as_ref().map(accessor_signature).transpose()?
                    else {
                        continue;
                    };
                    let Some(signature) = signature else {
                        continue;
                    };
                    let (value, result) = match value {
                        true => (Some(field), None),
                        false => (None, Some(field)),
                    };
                    let actual = self.rewrite(signature, receiver, value, result)?;
                    let expected = self.rewrite(signature, receiver, None, None)?;
                    pairs.push((actual, expected, false));
                }
            }
            _ => return Ok(changed()),
        }
        Ok(RequirementKind::Relate(pairs))
    }

    /// Whether a provided member is the required one, with the same types: the
    /// declaration inherits it as the supertype has it
    fn same_member(&self, provided: &Found, required: &Found) -> bool {
        fn terms(kind: &FoundKind) -> Option<Vec<Term>> {
            fn signatures(signatures: &Signatures) -> impl Iterator<Item = Term> + '_ {
                let overloads = signatures.overloads.iter().copied();
                overloads.chain(signatures.implementation)
            }
            Some(match kind {
                &FoundKind::Field(ty) => vec![ty],
                FoundKind::Method(method) => signatures(method).collect(),
                FoundKind::Property { getter, setter } => [getter, setter]
                    .into_iter()
                    .flatten()
                    .flat_map(signatures)
                    .collect(),
                FoundKind::Unknown(_) => return None,
            })
        }
        if provided.class != required.class || provided.bound != required.bound {
            return false;
        }
        let (Some(provided), Some(required)) = (terms(&provided.kind), terms(&required.kind))
        else {
            return false;
        };
        provided.len() == required.len()
            && provided
                .into_iter()
                .zip(required)
                .all(|(provided, required)| {
                    matches!(
                        (self.reify(provided), self.reify(required)),
                        (Ok(provided), Ok(required)) if provided == required
                    )
                })
    }

    /// What a provided member is called as: its one signature, or its overloads
    /// as an overloaded function without its implementation, which callers
    /// don't see. `None` if it has neither.
    fn overload_set(&self, signatures: &Signatures) -> Result<Option<Term>, Issue> {
        let overloads = &signatures.overloads;
        let (&first, rest) = match overloads.split_first() {
            Some(split) => split,
            None => return Ok(signatures.implementation),
        };
        if rest.is_empty() {
            return Ok(Some(first));
        }
        let shared = match first {
            Term::View(view) => overloads
                .iter()
                .all(|overload| {
                    matches!(overload, Term::View(other) if other.environment == view.environment)
                })
                .then_some(view.environment),
            _ => None,
        };
        let types = overloads
            .iter()
            .map(|&overload| match (overload, shared) {
                (Term::View(view), Some(_)) => Ok(view.ty),
                _ => self.reify(overload),
            })
            .collect::<Result<_, _>>()?;
        let ty = self.db.intern(Type::Overloaded {
            overloads: types,
            implementation: None,
            function: signatures.function,
        });
        Ok(Some(match shared {
            Some(environment) => self.view(ty, environment),
            None => self.closed(ty),
        }))
    }

    /// A provided accessor's result, or with `value` a setter's value
    /// parameter. An accessor may be generic, as in its ambient channels, in
    /// what it neither reads nor sets.
    fn accessor_part(&self, signature: Term, value: bool) -> Result<Term, Issue> {
        let signature = self.reify(signature)?;
        let (binders, body) = match self.db.ty(signature) {
            Type::Quantified { binders, body } => (Some(binders), *body),
            _ => (None, signature),
        };
        let Type::Function(function) = self.db.ty(body) else {
            return Err(Residual::Unsupported("a method that isn't a function").into());
        };
        let part = match value {
            false => function.result,
            true => {
                let Type::Schema(items) = self.db.ty(function.params) else {
                    return Err(Residual::Unsupported("a method without a parameter list").into());
                };
                let mut positions = items.iter().filter_map(|item| match item.element {
                    Element::Positional(ty) => Some(ty),
                    _ => None,
                });
                positions.nth(1).ok_or(Residual::Unsupported(
                    "an accessor without a value parameter",
                ))?
            }
        };
        let Some(binders) = binders else {
            return Ok(self.closed(part));
        };
        let own = self.db.walk(part, |node, depth| match *self.db.ty(node) {
            Type::Bound { reference, .. } if u32::from(reference.depth) == depth => {
                ControlFlow::Break(())
            }
            _ => ControlFlow::Continue(()),
        });
        if own.is_break() {
            let generic = "an accessor generic in what it reads or sets";
            return Err(Residual::Unsupported(generic).into());
        }
        let unknowns: Vec<_> = binders
            .iter()
            .map(|binder| self.db.unknown_of(binder.kind))
            .collect();
        Ok(self.closed(self.db.instantiate(part, &unknowns)))
    }

    /// A method signature with its receiver narrowed as [`Self::narrowed`] does,
    /// or its first parameter after the receiver or its result replaced by
    /// closed types
    fn rewrite(
        &self,
        signature: Term,
        receiver: Option<Narrowing>,
        value: Option<TypeId>,
        result: Option<TypeId>,
    ) -> Result<Term, Issue> {
        // A structural receiver needs no narrowing, so the signature is kept as a
        // view: its supertype's arguments may be unsolved. A closed type spliced
        // into it holds no binder references for the view to resolve.
        let (ty, environment) = match (receiver, signature) {
            (
                Some(Narrowing {
                    structural: true, ..
                }),
                Term::View(view),
            ) => (view.ty, Some(view.environment)),
            _ => (self.reify(signature)?, None),
        };
        let (binders, body) = match self.db.ty(ty) {
            Type::Quantified { binders, body } => (Some(binders.clone()), *body),
            _ => (None, ty),
        };
        let Type::Function(function) = self.db.ty(body) else {
            return Err(Residual::Unsupported("a method that isn't a function").into());
        };
        // The depth the signature's scope refers to its class's binders at
        let outer = usize::from(binders.is_some());
        let mut function = function.clone();
        if receiver.is_some() || value.is_some() {
            let Type::Schema(items) = self.db.ty(function.params) else {
                return Err(Residual::Unsupported("a method without a parameter list").into());
            };
            let mut items = items.to_vec();
            let mut positions = items.iter_mut().filter_map(|item| match &mut item.element {
                Element::Positional(ty) => Some(ty),
                _ => None,
            });
            if let Some(receiver) = receiver {
                let Some(ty) = positions.next() else {
                    return Err(Residual::Unsupported("a method without a receiver").into());
                };
                if let Some(narrowed) = self.narrowed_receiver(*ty, receiver, outer)? {
                    *ty = narrowed;
                }
            } else if value.is_some() {
                positions.next();
            }
            if let Some(value) = value {
                let Some(ty) = positions.next() else {
                    return Err(
                        Residual::Unsupported("an accessor without a value parameter").into(),
                    );
                };
                *ty = value;
            }
            function.params = self.db.intern(Type::Schema(items.into()));
        }
        if let Some(result) = result {
            function.result = result;
        }
        let body = self.db.intern(Type::Function(function));
        let ty = match binders {
            Some(binders) => self.db.intern(Type::Quantified { binders, body }),
            None => body,
        };
        Ok(match environment {
            Some(environment) => self.view(ty, environment),
            None => self.closed(ty),
        })
    }

    /// A receiver narrowed as [`Self::narrowed`] does, inside `Type` for a class
    /// method's. `None` leaves it as written. A structural receiver is the
    /// instance in place of the class's own receiver, written in a scope that
    /// refers to the class's binders at depth `outer`; an overload's specialized
    /// receiver is left as written, as it describes only some instances.
    fn narrowed_receiver(
        &self,
        receiver: TypeId,
        narrowing: Narrowing,
        outer: usize,
    ) -> Result<Option<TypeId>, Issue> {
        let Narrowing {
            instance,
            through,
            object,
            structural,
        } = narrowing;
        let class_type = self.db.intrinsic(Intrinsic::Type);
        if structural {
            let own = match (object, self.db.ty(receiver)) {
                (false, _) => self.own_receiver(receiver, outer),
                (true, Type::Apply { base, args, .. }) if Some(*base) == class_type => {
                    matches!(&args[..], &[Argument::Positional(inner)]
                        if self.own_receiver(inner, outer))
                }
                (true, _) => false,
            };
            if !own {
                return Ok(None);
            }
            return Ok(Some(match (object, class_type) {
                (false, _) => instance,
                (true, Some(class_type)) => self.db.intern(Type::Apply {
                    base: class_type,
                    args: vec![Argument::Positional(instance)].into(),
                    kind: Kind::Type,
                }),
                (true, None) => return Err(Residual::MissingIntrinsic(Intrinsic::Type).into()),
            }));
        }
        if !object {
            return self.narrowed(receiver, instance, through);
        }
        let inner = match self.db.ty(receiver) {
            Type::Apply { base, args, .. } if Some(*base) == class_type => match &args[..] {
                &[Argument::Positional(inner)] => inner,
                _ => return Ok(None),
            },
            _ => return Ok(None),
        };
        let Some(narrowed) = self.narrowed(inner, instance, through)? else {
            return Ok(None);
        };
        Ok(Some(self.db.intern(Type::Apply {
            base: class_type.expect("the receiver applies `Type`"),
            args: vec![Argument::Positional(narrowed)].into(),
            kind: Kind::Type,
        })))
    }

    /// Whether a receiver is its class applied to its own binders, which the
    /// signature's scope refers to at depth `outer`, or every value's
    fn own_receiver(&self, receiver: TypeId, outer: usize) -> bool {
        let own = |slot: usize, ty: TypeId| {
            matches!(self.db.ty(ty), Type::Bound { reference, .. }
                if usize::from(reference.depth) == outer && usize::from(reference.slot) == slot)
        };
        match self.db.ty(receiver) {
            Type::Decl(_) => true,
            Type::Apply { base, args, .. } if matches!(self.db.ty(*base), Type::Decl(_)) => {
                args.iter().enumerate().all(|(slot, arg)| match *arg {
                    Argument::Positional(ty) | Argument::Expand(ty) => own(slot, ty),
                    Argument::Keyword(_, ty) => own(slot, ty),
                })
            }
            _ => receiver == self.db.top(),
        }
    }

    /// A receiver `C[a…]`, written in a signature's scope, narrowed to instances
    /// of the class `D` of the closed `instance`: `D` applied to the arguments
    /// that make it reach `C[a…]` through its supertype `through`, the way the
    /// member was found. Each of `D`'s arguments that the walk reaches `C` with
    /// in some position takes the receiver's argument there, and the others stay
    /// as `instance` has them. So a default receiver becomes `instance`, and
    /// `Iterable[T]`, written in `Iterable`, narrowed to `Iter[X]` becomes
    /// `Iter[T]`. `None` leaves the
    /// receiver as written: it isn't a class application, the walk doesn't reach
    /// its class, or `D`'s arguments can't be matched to it.
    fn narrowed(
        &self,
        receiver: TypeId,
        instance: TypeId,
        through: Term,
    ) -> Result<Option<TypeId>, Issue> {
        let applied = |ty: TypeId| -> Option<(DeclId, Vec<TypeId>)> {
            match self.db.ty(ty) {
                &Type::Decl(decl) => Some((decl, Vec::new())),
                Type::Apply { base, args, .. } => {
                    let &Type::Decl(decl) = self.db.ty(*base) else {
                        return None;
                    };
                    let args = args
                        .iter()
                        .map(|arg| match *arg {
                            Argument::Positional(ty) => Some(ty),
                            _ => None,
                        })
                        .collect::<Option<Vec<_>>>()?;
                    Some((decl, args))
                }
                _ => None,
            }
        };
        // `Value`'s receiver is every value, which includes the instances
        if receiver == self.db.top() {
            return Ok(Some(instance));
        }
        let (Some((class, args)), Some((own, rigids))) = (applied(receiver), applied(instance))
        else {
            return Ok(None);
        };
        if !self.db.declaration(class).source.kind.nominal() {
            return Ok(None);
        }
        let Reach::Reached(reached) = self.reach(through, class)? else {
            return Ok(None);
        };
        if reached.len() != args.len() {
            return Ok(None);
        }
        let mut narrowed = rigids.clone();
        let mut matched = vec![false; rigids.len()];
        for (&arg, reached) in args.iter().zip(reached) {
            let reached = self.reify(reached)?;
            match rigids.iter().position(|&rigid| rigid == reached) {
                Some(slot) if !matched[slot] || narrowed[slot] == arg => {
                    narrowed[slot] = arg;
                    matched[slot] = true;
                }
                None if reached == arg => {}
                _ => return Ok(None),
            }
        }
        Ok(Some(match narrowed.is_empty() {
            true => instance,
            false => self.db.intern(Type::Apply {
                base: self.db.intern(Type::Decl(own)),
                args: narrowed
                    .into_iter()
                    .map(Argument::Positional)
                    .collect::<Vec<_>>()
                    .into(),
                kind: Kind::Type,
            }),
        }))
    }

    /// For each class `supertype` names, down its ancestry, how the declaration
    /// whose instances are `instance` inherits it at runtime
    pub(crate) fn claimed_classes(
        &self,
        instance: Term,
        supertype: Term,
    ) -> Result<Vec<(DeclId, Inheritance)>, Issue> {
        let mut classes = Vec::new();
        for decl in self.lineage(supertype)? {
            if self.db.declaration(decl).source.kind != DeclKind::Class {
                continue;
            }
            let inheritance = match self.inheritance(instance, supertype, decl) {
                Ok(Some(inheritance)) => inheritance,
                Ok(None) => continue,
                Err(issue) => Inheritance::Undecided(issue),
            };
            classes.push((decl, inheritance));
        }
        Ok(classes)
    }

    fn inheritance(
        &self,
        instance: Term,
        supertype: Term,
        class: DeclId,
    ) -> Result<Option<Inheritance>, Issue> {
        let Reach::Reached(claimed) = self.reach(supertype, class)? else {
            return Ok(None);
        };
        let inherited = match self.inherits(instance, class)? {
            Reach::Reached(inherited) => inherited,
            Reach::Unreached => return Ok(Some(Inheritance::Unreached)),
            Reach::Dynamic => return Ok(None),
        };
        if claimed.is_empty() {
            return Ok(None);
        }
        Ok(Some(Inheritance::Relate(
            self.closed(self.applied(class, &inherited)?),
            self.closed(self.applied(class, &claimed)?),
        )))
    }

    /// The instances of generic classes in the ancestry of the declaration whose
    /// instances have the closed type `instance` that the first instance of
    /// each class masks, which the first must be below. With
    /// `runtime`, as for a class, a claimed instance masked by one the runtime
    /// inherits is left to [`Self::claimed_classes`]. Instances reached through
    /// the same supertype were checked at it, so only pairs reached through
    /// different supertypes are returned.
    pub(crate) fn masked(&self, instance: Term, runtime: bool) -> Result<Vec<Masked>, Issue> {
        let Head::Nominal(nominal) = self.head(instance)? else {
            return Ok(Vec::new());
        };
        let declaration = nominal.declaration;
        let mut path = FxHashSet::default();
        path.insert(declaration);
        // Each class's instances, in the order the walk reaches them
        let mut reached: Vec<(DeclId, Vec<Reached>)> = Vec::new();
        let supertypes = &self.db.declaration(declaration).supertypes;
        for (index, supertype) in supertypes.iter().enumerate() {
            let Head::Nominal(next) = self.head(self.view(supertype.ty, nominal.environment))?
            else {
                continue;
            };
            let mut direct = true;
            self.mro_flagged(
                next,
                false,
                supertype.runtime,
                &mut path,
                1,
                &mut |visited, inherited| {
                    let Visited::Nominal(found) = visited else {
                        return Ok(None::<()>);
                    };
                    let is_direct = std::mem::take(&mut direct);
                    if found.arguments.is_empty() {
                        return Ok(None);
                    }
                    let instance = Reached {
                        ty: self.applied(found.declaration, &found.arguments)?,
                        supertype: index,
                        direct: is_direct,
                        inherited,
                    };
                    match reached
                        .iter_mut()
                        .find(|(class, _)| *class == found.declaration)
                    {
                        Some((_, instances)) => instances.push(instance),
                        None => reached.push((found.declaration, vec![instance])),
                    }
                    Ok(None)
                },
            )?;
        }
        let mut masked = Vec::new();
        for (_, instances) in reached {
            let Some((first, rest)) = instances.split_first() else {
                continue;
            };
            for other in rest {
                if other.supertype == first.supertype
                    || other.ty == first.ty
                    || (runtime && first.inherited && !other.inherited)
                {
                    continue;
                }
                masked.push(Masked {
                    first: first.clone(),
                    other: other.clone(),
                    reorder: runtime && !first.inherited && other.inherited,
                });
            }
        }
        Ok(masked)
    }

    /// `class` applied to `args`
    fn applied(&self, class: DeclId, args: &[Term]) -> Result<TypeId, Issue> {
        let args = args
            .iter()
            .map(|&arg| self.reify(arg).map(Argument::Positional))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(self.db.intern(Type::Apply {
            base: self.db.intern(Type::Decl(class)),
            args: args.into(),
            kind: Kind::Type,
        }))
    }

    fn is_protocol(&self, decl: DeclId) -> bool {
        self.db.declaration(decl).source.kind == DeclKind::Protocol
    }
}

/// The signatures a member is called with: its overloads, or its
/// implementation's signature if it has none
fn interface(signatures: &Signatures) -> Vec<Term> {
    match signatures.overloads.is_empty() {
        true => signatures.implementation.into_iter().collect(),
        false => signatures.overloads.clone(),
    }
}

/// An accessor's one signature, related to a field's type as it reads or sets
/// it. An overloaded accessor has no one signature to relate.
fn accessor_signature(signatures: &Signatures) -> Result<Option<Term>, Issue> {
    match signatures.overloads.is_empty() {
        true => Ok(signatures.implementation),
        false => Err(Residual::Unsupported("an overloaded accessor beside a field").into()),
    }
}

/// A kind of member, for diagnostics
fn describe(kind: &FoundKind) -> &'static str {
    match kind {
        FoundKind::Field(_) => "a field",
        FoundKind::Method(_) => "a method",
        FoundKind::Property {
            getter: Some(_),
            setter: Some(_),
        } => "a property",
        FoundKind::Property { setter: None, .. } => "a read-only property",
        FoundKind::Property { getter: None, .. } => "a write-only property",
        FoundKind::Unknown(_) => "a decorated member",
    }
}
