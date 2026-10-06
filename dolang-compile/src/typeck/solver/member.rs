//! Member lookup.
//!
//! A receiver's member is found as the runtime finds it: in the receiver's class
//! and then its ancestors in MRO order, left to right and depth first, the first
//! member of the name winning. Instance members and type-object members are
//! separate namespaces. An instance falls back to its class's `(get)` and `(set)`
//! methods when it has no member of an ordinary name. A class object, whose type
//! is `Type[C]`, has `C`'s class members, its static members only on `C` itself,
//! and then `C`'s instance methods, unbound.
//!
//! A found member is interpreted with the arguments its class is reached with. A
//! method is lifted over its class's binders, so those are split off its group
//! and applied, leaving it quantified over its own; its receiver parameter stays,
//! and a call passes the receiver as its first argument.
//!
//! A generic class's object, `[S] Type[C[S]]`, has the members of `C`'s object
//! applied to its rigids, quantified over `C`'s binders before their own. Since
//! the object is every application of `C`, a class-level method or property is
//! bound to it, dropping its receiver parameter, and a class-level field's type
//! takes `C`'s binders as `Unknown`.

use super::{callable::drop_receiver, *};
use crate::typeck::r#type::{Member, MemberKey, Scope};

/// What a receiver's member is
#[derive(Clone, Debug)]
pub(crate) enum Lookup {
    Found(Found),
    /// No member, but an instance's class falls back to these for any ordinary name
    Fallback {
        get: Option<Found>,
        set: Option<Found>,
    },
    Missing,
    /// The receiver, or an ancestor searched before the member is found, is dynamic
    Dynamic,
}

#[derive(Clone, Debug)]
pub(crate) struct Found {
    /// The class that declares it
    pub(crate) class: DeclId,
    pub(crate) scope: Scope,
    /// Whether it is public. Only a public member can be replaced in a subclass,
    /// so only access to one may dispatch.
    #[cfg_attr(not(test), expect(dead_code, reason = "read by tests"))]
    pub(crate) public: bool,
    /// Whether its signatures are already bound to the receiver, without their
    /// receiver parameter, as a generic class object's class-level ones are
    pub(crate) bound: bool,
    pub(crate) kind: FoundKind,
}

#[derive(Clone, Debug)]
pub(crate) enum FoundKind {
    Field(Term),
    Method(Signatures),
    /// The signatures of a computed field's getter and setter
    Property {
        getter: Option<Signatures>,
        setter: Option<Signatures>,
    },
    /// A method its decorators replace with a value of unknown type
    Unknown,
}

/// A method's signatures, each with its receiver parameter
#[derive(Clone, Debug)]
pub(crate) struct Signatures {
    /// Its `@def` signatures, empty unless it's overloaded
    pub(crate) overloads: Vec<Term>,
    /// Its implementation's signature, unless it's overloaded without one
    pub(crate) implementation: Option<Term>,
}

/// Where a receiver's members are looked up
enum Receiver {
    Instance(Nominal),
    /// A class object, with its class
    Object(Nominal),
    /// A generic class's object, `[S] Type[C[S]]`, with its class
    Generic(DeclId),
    Missing,
    Dynamic,
}

impl Solver<'_> {
    /// Look up an ordinary or special member of `receiver`. A private member is
    /// found with [`Self::private_member`].
    pub(crate) fn member(&self, receiver: Term, key: MemberKey) -> Result<Lookup, Issue> {
        assert!(!key.private, "a private member is its class's own");
        match self.receiver(receiver)? {
            Receiver::Instance(nominal) => self.instance_member(nominal, key),
            Receiver::Object(nominal) => self.object_member(nominal, key),
            Receiver::Generic(class) => self.generic_member(class, None, key),
            Receiver::Missing => Ok(Lookup::Missing),
            Receiver::Dynamic => Ok(Lookup::Dynamic),
        }
    }

    /// Look up a private member of `class` through `receiver`. Private access is
    /// resolved lexically, so the receiver is walked to the class that names it.
    pub(crate) fn private_member(
        &self,
        receiver: Term,
        class: DeclId,
        key: MemberKey,
    ) -> Result<Lookup, Issue> {
        assert!(key.private, "an ordinary member is looked up by name");
        let (nominal, instance) = match self.receiver(receiver)? {
            Receiver::Instance(nominal) => (nominal, true),
            Receiver::Object(nominal) => (nominal, false),
            Receiver::Generic(generic) => return self.generic_member(generic, Some(class), key),
            Receiver::Missing => return Ok(Lookup::Missing),
            Receiver::Dynamic => return Ok(Lookup::Dynamic),
        };
        let Some(nominal) = self.ancestor(nominal, class, &mut HashSet::new(), 0)? else {
            return Ok(Lookup::Missing);
        };
        let found = self.members(&nominal).find(|(found, member)| {
            *found == key && (member.scope() == Scope::Instance) == instance
        });
        Ok(match found {
            Some((_, member)) => Lookup::Found(self.found(&nominal, member)),
            None => Lookup::Missing,
        })
    }

    /// The first member of `key` in `scope` along an instance's MRO, as a
    /// subclass inherits it: a class-scope member is its type object's, and a
    /// static member is only its own class's. With `runtime`, only what the
    /// runtime inherits is searched, so what a class claims to be is not an
    /// implementation. Nothing falls back to `(get)` or `(set)`.
    pub(crate) fn inherited_member(
        &self,
        instance: Term,
        key: MemberKey,
        scope: Scope,
        runtime: bool,
    ) -> Result<Lookup, Issue> {
        let nominal = match self.receiver(instance)? {
            Receiver::Instance(nominal) => nominal,
            Receiver::Object(_) | Receiver::Generic(_) => {
                return Err(Residual::Unsupported("the members a type object inherits").into());
            }
            Receiver::Missing => return Ok(Lookup::Missing),
            Receiver::Dynamic => return Ok(Lookup::Dynamic),
        };
        let class = nominal.declaration;
        self.search_in(nominal, key, runtime, |owner, member| {
            member.scope() == scope && (scope != Scope::Static || owner == class)
        })
    }

    /// The declarations along an instance's MRO, in order, each once. A supertype
    /// that isn't nominal contributes none.
    pub(crate) fn lineage(&self, instance: Term) -> Result<Vec<DeclId>, Issue> {
        let nominal = match self.receiver(instance)? {
            Receiver::Instance(nominal) => nominal,
            Receiver::Object(_) | Receiver::Generic(_) | Receiver::Missing | Receiver::Dynamic => {
                return Ok(Vec::new());
            }
        };
        let mut lineage = Vec::new();
        self.preorder(nominal, &mut HashSet::new(), 0, &mut |visited| {
            if let Visited::Nominal(nominal) = visited
                && !lineage.contains(&nominal.declaration)
            {
                lineage.push(nominal.declaration);
            }
            Ok(None::<()>)
        })?;
        Ok(lineage)
    }

    /// Where a receiver's members are looked up, walking a rigid through its bound
    /// and a literal or function to its intrinsic class
    fn receiver(&self, mut term: Term) -> Result<Receiver, Issue> {
        for depth in 0.. {
            self.depth(depth)?;
            self.spend()?;
            let view = match self.head(term)? {
                Head::Infer(_) => return Err(Residual::Inference.into()),
                Head::Nominal(nominal) => {
                    if !self.is_intrinsic(nominal.declaration, Intrinsic::Type) {
                        return Ok(Receiver::Instance(nominal));
                    }
                    let [class] = nominal.arguments[..] else {
                        return Err(
                            Residual::Unsupported("a type object with several arguments").into(),
                        );
                    };
                    return match self.head(class)? {
                        Head::Infer(_) => Err(Residual::Inference.into()),
                        Head::Skolem(_) => {
                            Err(Residual::Unsupported("a type object of a skolem").into())
                        }
                        Head::Nominal(class) => Ok(Receiver::Object(class)),
                        Head::Structural(view)
                            if matches!(self.db.ty(view.ty), Type::Unknown(_)) =>
                        {
                            Ok(Receiver::Dynamic)
                        }
                        Head::Structural(_) => {
                            Err(Residual::Unsupported("a type object of a structural type").into())
                        }
                    };
                }
                Head::Skolem(id) => match self.skolems[id.0].bound.get() {
                    Some(bound) => {
                        term = bound;
                        continue;
                    }
                    None => return Ok(Receiver::Missing),
                },
                Head::Structural(view) => view,
            };
            if let Some(class) = self.generic_object(view.ty) {
                return Ok(Receiver::Generic(class));
            }
            let mut ty = view.ty;
            while let Type::Quantified { body, .. } = self.db.ty(ty) {
                self.spend()?;
                ty = *body;
            }
            let intrinsic = match self.db.ty(ty) {
                Type::Unknown(_) => return Ok(Receiver::Dynamic),
                // `Value` declares nothing
                Type::Top => return Ok(Receiver::Missing),
                // Bottom has no values, so any member is vacuously fine
                Type::Union(members) if members.is_empty() => return Ok(Receiver::Dynamic),
                Type::Rigid { .. } => {
                    self.rigid(view.ty)?;
                    match self.rigid_bound(view.ty) {
                        Some(bound) => {
                            term = self.closed(bound);
                            continue;
                        }
                        None => return Ok(Receiver::Missing),
                    }
                }
                // `Func` applied to the function's parts
                Type::Function(_) => {
                    let class = (self.db.func_class(view.ty))
                        .ok_or(Residual::MissingIntrinsic(Intrinsic::Func))?;
                    term = self.view(class, view.environment);
                    continue;
                }
                Type::Literal(literal) => literal.intrinsic(),
                // Flow makes a use of a union's member of each alternative
                Type::Union(_) => {
                    return Err(Residual::Unsupported("a member of a union receiver").into());
                }
                _ if ty != view.ty => {
                    return Err(Residual::Unsupported("a member of a quantified type").into());
                }
                _ => return Err(Residual::Unsupported("a member of a structural type").into()),
            };
            let Some(backing) = self.db.intrinsic(intrinsic) else {
                return Err(Residual::MissingIntrinsic(intrinsic).into());
            };
            term = self.closed(backing);
        }
        unreachable!()
    }

    /// The class whose object a type is, if it's a generic class's object as flow
    /// gives it: `[S] Type[C[S]]`, quantified over `C`'s own binders and applying
    /// `C` to them in order
    fn generic_object(&self, ty: TypeId) -> Option<DeclId> {
        let Type::Quantified { binders, body } = self.db.ty(ty) else {
            return None;
        };
        let Type::Apply { base, args, .. } = self.db.ty(*body) else {
            return None;
        };
        let (true, [Argument::Positional(instance)]) =
            (Some(*base) == self.db.intrinsic(Intrinsic::Type), &args[..])
        else {
            return None;
        };
        let Type::Apply { base, args, .. } = self.db.ty(*instance) else {
            return None;
        };
        let &Type::Decl(class) = self.db.ty(*base) else {
            return None;
        };
        let declaration = self.db.declaration(class);
        let Type::Quantified { binders: own, .. } = self.db.ty(declaration.ty) else {
            return None;
        };
        let canonical = declaration.source.kind.nominal()
            && own == binders
            && args.len() == binders.len()
            && (args.iter().enumerate()).all(|(slot, arg)| match arg {
                Argument::Positional(arg) => matches!(
                    *self.db.ty(*arg),
                    Type::Bound { reference, .. } if reference == BoundRef::new(0, slot)
                ),
                _ => false,
            });
        canonical.then_some(class)
    }

    /// A member of a generic class's object, or a private member of `private`
    /// through it, looked up on the object of the class applied to its rigids and
    /// quantified over its binders again
    fn generic_member(
        &self,
        class: DeclId,
        private: Option<DeclId>,
        key: MemberKey,
    ) -> Result<Lookup, Issue> {
        let cached = (class, private, key);
        if let Some(lookup) = self.generic_members.borrow().get(&cached) {
            return lookup.clone();
        }
        let lookup = self.lift_member(class, private, key);
        (self.generic_members.borrow_mut()).insert(cached, lookup.clone());
        lookup
    }

    fn lift_member(
        &self,
        class: DeclId,
        private: Option<DeclId>,
        key: MemberKey,
    ) -> Result<Lookup, Issue> {
        let db = self.db;
        let class_type =
            (db.intrinsic(Intrinsic::Type)).ok_or(Residual::MissingIntrinsic(Intrinsic::Type))?;
        let Type::Quantified { binders, .. } = db.ty(db.declaration(class).ty) else {
            unreachable!("a generic class without binders")
        };
        let args = (db.rigids(class).into_iter())
            .map(Argument::Positional)
            .collect();
        let instance = db.intern(Type::Apply {
            base: db.intern(Type::Decl(class)),
            args,
            kind: Kind::Type,
        });
        let object = db.intern(Type::Apply {
            base: class_type,
            args: vec![Argument::Positional(instance)].into(),
            kind: Kind::Type,
        });
        let solver = self.side_query(class);
        let receiver = solver.closed(object);
        let found = match private {
            Some(owner) => solver.private_member(receiver, owner, key)?,
            None => solver.member(receiver, key)?,
        };
        let Lookup::Found(found) = found else {
            return Ok(found);
        };
        // What it is with the class's binders in place of its rigids
        let abstracted = |term| -> Result<TypeId, Issue> {
            let ty = solver.reify(term)?;
            Ok(db
                .abstract_rigids(ty, class)
                .map_err(|_| Residual::Escape)?)
        };
        let bound = found.scope == Scope::Class
            && matches!(
                found.kind,
                FoundKind::Method(_) | FoundKind::Property { .. }
            );
        let signature = |term| -> Result<Term, Issue> {
            let mut signature = abstracted(term)?;
            if bound {
                signature = unbind(db, signature).ok_or(Residual::Unsupported(
                    "a class-level method without a receiver",
                ))?;
            }
            Ok(self.closed(db.merge_groups(binders, signature)))
        };
        let signatures = |signatures: Signatures| -> Result<Signatures, Issue> {
            Ok(Signatures {
                overloads: (signatures.overloads.into_iter())
                    .map(signature)
                    .collect::<Result<_, _>>()?,
                implementation: signatures.implementation.map(signature).transpose()?,
            })
        };
        let kind = match found.kind {
            FoundKind::Field(ty) => {
                let unknowns: Vec<_> = (binders.iter())
                    .map(|binder| db.unknown_of(binder.kind))
                    .collect();
                FoundKind::Field(self.closed(db.substitute(abstracted(ty)?, &unknowns)))
            }
            FoundKind::Method(method) => FoundKind::Method(signatures(method)?),
            FoundKind::Property { getter, setter } => FoundKind::Property {
                getter: getter.map(signatures).transpose()?,
                setter: setter.map(signatures).transpose()?,
            },
            FoundKind::Unknown => FoundKind::Unknown,
        };
        Ok(Lookup::Found(Found {
            bound,
            kind,
            ..found
        }))
    }

    pub(super) fn is_intrinsic(&self, decl: DeclId, intrinsic: Intrinsic) -> bool {
        self.db
            .intrinsic(intrinsic)
            .is_some_and(|ty| *self.db.ty(ty) == Type::Decl(decl))
    }

    pub(super) fn instance_member(
        &self,
        nominal: Nominal,
        key: MemberKey,
    ) -> Result<Lookup, Issue> {
        let found = self.search(nominal.clone(), key, |_, member| {
            member.scope() == Scope::Instance
        })?;
        if !matches!(found, Lookup::Missing) || key.special {
            return Ok(found);
        }
        let fallback = |name| {
            let key = MemberKey {
                name: self.db.intern_symbol(name),
                special: true,
                private: false,
            };
            self.search(nominal.clone(), key, |_, member| {
                member.scope() == Scope::Instance
            })
        };
        Ok(match (fallback("get")?, fallback("set")?) {
            (Lookup::Dynamic, _) | (_, Lookup::Dynamic) => Lookup::Dynamic,
            (Lookup::Missing, Lookup::Missing) => Lookup::Missing,
            (get, set) => {
                let found = |lookup| match lookup {
                    Lookup::Found(found) => Some(found),
                    _ => None,
                };
                Lookup::Fallback {
                    get: found(get),
                    set: found(set),
                }
            }
        })
    }

    pub(super) fn object_member(&self, nominal: Nominal, key: MemberKey) -> Result<Lookup, Issue> {
        let class = nominal.declaration;
        // A static member belongs to its class alone
        let found = self.search(nominal.clone(), key, |owner, member| match member.scope() {
            Scope::Instance => false,
            Scope::Class => true,
            Scope::Static => owner == class,
        })?;
        if !matches!(found, Lookup::Missing) {
            return Ok(found);
        }
        // Only an instance method is reached through its class, unbound
        Ok(
            match self.search(nominal, key, |_, member| member.scope() == Scope::Instance)? {
                Lookup::Found(found)
                    if matches!(found.kind, FoundKind::Method(_) | FoundKind::Unknown) =>
                {
                    Lookup::Found(found)
                }
                Lookup::Dynamic => Lookup::Dynamic,
                _ => Lookup::Missing,
            },
        )
    }

    /// The first member of `key` in MRO order that `admits`
    fn search(
        &self,
        nominal: Nominal,
        key: MemberKey,
        admits: impl Fn(DeclId, &Member) -> bool,
    ) -> Result<Lookup, Issue> {
        self.search_in(nominal, key, false, admits)
    }

    /// [`Self::search`], following only the supertypes the runtime inherits from
    /// when `runtime`
    fn search_in(
        &self,
        nominal: Nominal,
        key: MemberKey,
        runtime: bool,
        admits: impl Fn(DeclId, &Member) -> bool,
    ) -> Result<Lookup, Issue> {
        let found = self.mro(nominal, runtime, &mut HashSet::new(), 0, &mut |visited| {
            Ok(match visited {
                Visited::Nominal(nominal) => self
                    .members(nominal)
                    .find(|&(found, member)| found == key && admits(nominal.declaration, member))
                    .map(|(_, member)| Lookup::Found(self.found(nominal, member))),
                Visited::Structural => Some(Lookup::Dynamic),
            })
        })?;
        Ok(found.unwrap_or(Lookup::Missing))
    }

    fn members(&self, nominal: &Nominal) -> impl Iterator<Item = (MemberKey, &Member)> {
        self.db
            .declaration(nominal.declaration)
            .members
            .iter()
            .map(|(key, member)| (*key, member))
    }

    /// A member of `nominal`'s class, interpreted with its arguments
    fn found(&self, nominal: &Nominal, member: &Member) -> Found {
        let signatures = |decl| self.signatures(nominal, decl);
        let kind = match *member {
            Member::Field { ty, .. } => FoundKind::Field(self.view(ty, nominal.environment)),
            Member::Method { decl, .. } => FoundKind::Method(signatures(decl)),
            Member::Property { getter, setter, .. } => FoundKind::Property {
                getter: getter.map(signatures),
                setter: setter.map(signatures),
            },
            Member::Decorated { .. } => FoundKind::Unknown,
        };
        Found {
            class: nominal.declaration,
            scope: member.scope(),
            public: member.public(),
            bound: false,
            kind,
        }
    }

    /// The signatures of a method of `nominal`'s class, with the class's binders
    /// split off and applied
    fn signatures(&self, nominal: &Nominal, decl: DeclId) -> Signatures {
        let class = match self.db.ty(self.db.declaration(nominal.declaration).ty) {
            Type::Quantified { binders, .. } => binders.len(),
            _ => 0,
        };
        let signature = |decl: DeclId| {
            let ty = self.db.split(self.db.declaration(decl).ty, class);
            self.view(ty, nominal.environment)
        };
        Signatures {
            overloads: self
                .db
                .overloads(decl)
                .iter()
                .copied()
                .map(signature)
                .collect(),
            implementation: self.db.implementation(decl).map(signature),
        }
    }
}

/// A signature without its receiver parameter, keeping all its binders. Unlike
/// [`bound_method`], the receiver may mention them: a generic class's object,
/// which is every application of its class, is passed for it.
fn unbind(db: &Database, signature: TypeId) -> Option<TypeId> {
    let (binders, body) = match db.ty(signature) {
        Type::Quantified { binders, body } => (Some(binders.clone()), *body),
        _ => (None, signature),
    };
    let (function, _) = drop_receiver(db, body)?;
    Some(match binders {
        Some(binders) => db.intern(Type::Quantified {
            binders,
            body: function,
        }),
        None => function,
    })
}
