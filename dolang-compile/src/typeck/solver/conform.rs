//! Conformance of a declaration to its supertypes.
//!
//! A class or protocol conforms to a supertype when each public member the
//! supertype has is matched by a compatible member of its own: an override
//! declared in it, or what it inherits. A class's members are what the runtime
//! inherits, so a member it only claims through a protocol is missing. Members
//! are compared by kind: fields invariantly, as they are mutable, methods and
//! property accessors by their implementations' signatures, and a protocol's
//! field by a getter and a setter as well as a field. An overload below a
//! required signature satisfies it in place of the implementation, as overloads
//! are unchecked assertions narrowing it. The supertype's signature
//! takes the conforming declaration's own type as its receiver, since only calls
//! on its instances matter.
//!
//! Every requirement is a set of ordinary subtype judgments for the caller to
//! constrain, so no verdict is cached here.

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
    /// Each pair's actual must be below its expected
    Relate(Vec<(Term, Term)>),
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

/// How a class inherits a class that a supertype names
#[derive(Clone, Debug)]
pub(crate) enum Inheritance {
    /// It doesn't inherit it at runtime
    Unreached,
    /// The class as inherited must be below the class as named
    Relate(Term, Term),
    Undecided(Issue),
}

impl Solver<'_> {
    /// The requirements `supertype`'s members place on the declaration whose
    /// instances have the closed type `instance`. With `runtime`, only what the
    /// runtime inherits provides members, as for a class; a protocol provides
    /// what it claims too.
    pub(crate) fn conformance(
        &self,
        instance: TypeId,
        supertype: Term,
        runtime: bool,
    ) -> Result<Vec<Requirement>, Issue> {
        let init = self.db.intern_symbol("init");
        let mut keys = Vec::new();
        for decl in self.lineage(supertype)? {
            for (key, member) in self.db.declaration(decl).members.iter() {
                let scope = member.scope();
                if key.private
                    || scope == Scope::Static
                    || (key.special && key.name == init)
                    || keys.contains(&(*key, scope))
                {
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
            // A class-scope member's receiver is a class object, and stays as
            // written
            let receiver = (scope == Scope::Instance).then_some((instance, supertype));
            let kind = match &provided {
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

    /// Compare a provided member with a required one of the same name. With
    /// `receiver`, an instance type and the supertype the required member was
    /// found through, the required signatures' receivers are narrowed to the
    /// instance type's class.
    fn requirement(
        &self,
        provided: &Found,
        required: &Found,
        receiver: Option<(TypeId, Term)>,
    ) -> Result<RequirementKind, Issue> {
        // The required signature, called on the conforming declaration's instances
        let expected = |signatures: &Signatures| match signatures.implementation {
            Some(signature) => self.rewrite(signature, receiver, None, None).map(Some),
            None => Ok(None),
        };
        let changed = || RequirementKind::Changed {
            provided: describe(&provided.kind),
            required: describe(&required.kind),
        };
        let mut pairs = Vec::new();
        // An overload below the required signature satisfies it, since an
        // overload is an unchecked assertion that narrows the implementation.
        // Otherwise the implementation must be below it.
        let relate = |pairs: &mut Vec<_>, actual: &Signatures, expected: Option<Term>| {
            let (Some(implementation), Some(expected)) = (actual.implementation, expected) else {
                return Ok::<_, Issue>(());
            };
            if !actual.overloads.is_empty() {
                let required = self.reify(expected)?;
                for &overload in &actual.overloads {
                    if self.probe(self.reify(overload)?, required) == Ok(Status::Proven) {
                        return Ok(());
                    }
                }
            }
            pairs.push((implementation, expected));
            Ok(())
        };
        match (&provided.kind, &required.kind) {
            (FoundKind::Unknown, _) | (_, FoundKind::Unknown) => {}
            (&FoundKind::Field(actual), &FoundKind::Field(expected)) => {
                pairs.push((actual, expected));
                pairs.push((expected, actual));
            }
            (FoundKind::Method(actual), FoundKind::Method(required)) => {
                relate(&mut pairs, actual, expected(required)?)?;
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
                        relate(&mut pairs, actual, expected(required)?)?;
                    }
                }
            }
            // A protocol's field is upheld by a property that reads and writes it
            (
                FoundKind::Property {
                    getter: Some(getter),
                    setter: Some(setter),
                },
                &FoundKind::Field(field),
            ) if self.is_protocol(required.class) => {
                let field = self.reify(field)?;
                if let Some(getter) = getter.implementation {
                    let expected = self.rewrite(getter, None, None, Some(field))?;
                    pairs.push((getter, expected));
                }
                if let Some(setter) = setter.implementation {
                    let expected = self.rewrite(setter, None, Some(field), None)?;
                    pairs.push((setter, expected));
                }
            }
            _ => return Ok(changed()),
        }
        Ok(RequirementKind::Relate(pairs))
    }

    /// A method signature with its receiver narrowed as [`Self::narrowed`] does,
    /// or its first parameter after the receiver or its result replaced by
    /// closed types
    fn rewrite(
        &self,
        signature: Term,
        receiver: Option<(TypeId, Term)>,
        value: Option<TypeId>,
        result: Option<TypeId>,
    ) -> Result<Term, Issue> {
        let ty = self.reify(signature)?;
        let (binders, body) = match self.db.ty(ty) {
            Type::Quantified { binders, body } => (Some(binders.clone()), *body),
            _ => (None, ty),
        };
        let Type::Function(function) = self.db.ty(body) else {
            return Err(Residual::Unsupported("a method that isn't a function").into());
        };
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
            if let Some((instance, through)) = receiver {
                let Some(ty) = positions.next() else {
                    return Err(Residual::Unsupported("a method without a receiver").into());
                };
                if let Some(narrowed) = self.narrowed(*ty, instance, through)? {
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
        Ok(self.closed(match binders {
            Some(binders) => self.db.intern(Type::Quantified { binders, body }),
            None => body,
        }))
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
        let apply = |args: Vec<Term>| -> Result<Term, Issue> {
            let args = args
                .into_iter()
                .map(|arg| self.reify(arg).map(Argument::Positional))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(self.closed(self.db.intern(Type::Apply {
                base: self.db.intern(Type::Decl(class)),
                args: args.into(),
                kind: Kind::Type,
            })))
        };
        Ok(Some(Inheritance::Relate(
            apply(inherited)?,
            apply(claimed)?,
        )))
    }

    fn is_protocol(&self, decl: DeclId) -> bool {
        self.db.declaration(decl).source.kind == DeclKind::Protocol
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
        FoundKind::Unknown => "a decorated member",
    }
}
