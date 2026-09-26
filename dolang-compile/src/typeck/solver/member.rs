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

use super::*;
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
    pub(crate) public: bool,
    pub(crate) kind: FoundKind,
}

#[derive(Clone, Debug)]
pub(crate) enum FoundKind {
    Field(Term),
    /// Each signature of the method, with its receiver parameter
    Method(Vec<Term>),
    /// The signatures of a computed field's getter and setter
    Property {
        getter: Option<Vec<Term>>,
        setter: Option<Vec<Term>>,
    },
    /// A method its decorators replace with a value of unknown type
    Unknown,
}

/// Where a receiver's members are looked up
enum Receiver {
    Instance(Nominal),
    /// A class object, with its class
    Object(Nominal),
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
                        return Err(Residual::Unsupported.into());
                    };
                    return match self.head(class)? {
                        Head::Infer(_) => Err(Residual::Inference.into()),
                        Head::Nominal(class) => Ok(Receiver::Object(class)),
                        Head::Structural(view)
                            if matches!(self.db.ty(view.ty), Type::Unknown(_)) =>
                        {
                            Ok(Receiver::Dynamic)
                        }
                        Head::Structural(_) => Err(Residual::Unsupported.into()),
                    };
                }
                Head::Structural(view) => view,
            };
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
                Type::Function(_) => Intrinsic::Func,
                Type::Literal(Literal::Nil) => Intrinsic::Nil,
                Type::Literal(Literal::Bool(_)) => Intrinsic::Bool,
                Type::Literal(Literal::Int(_)) => Intrinsic::Int,
                Type::Literal(Literal::Str(_)) => Intrinsic::Str,
                Type::Literal(Literal::Sym(_)) => Intrinsic::Sym,
                // A union's members are judged by #742's policy
                _ => return Err(Residual::Unsupported.into()),
            };
            let Some(backing) = self.db.intrinsic(intrinsic) else {
                return Err(Residual::MissingIntrinsic(intrinsic).into());
            };
            term = self.closed(backing);
        }
        unreachable!()
    }

    fn is_intrinsic(&self, decl: DeclId, intrinsic: Intrinsic) -> bool {
        self.db
            .intrinsic(intrinsic)
            .is_some_and(|ty| *self.db.ty(ty) == Type::Decl(decl))
    }

    fn instance_member(&self, nominal: Nominal, key: MemberKey) -> Result<Lookup, Issue> {
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

    fn object_member(&self, nominal: Nominal, key: MemberKey) -> Result<Lookup, Issue> {
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
        let found = self.preorder(nominal, &mut HashSet::new(), 0, &mut |visited| {
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
            kind,
        }
    }

    /// Each signature of a method of `nominal`'s class, with the class's binders
    /// split off and applied
    fn signatures(&self, nominal: &Nominal, decl: DeclId) -> Vec<Term> {
        let class = match self.db.ty(self.db.declaration(nominal.declaration).ty) {
            Type::Quantified { binders, .. } => binders.len(),
            _ => 0,
        };
        let overloads = self.db.overloads(decl);
        let decls = if overloads.is_empty() {
            &[decl][..]
        } else {
            overloads
        };
        decls
            .iter()
            .map(|&decl| {
                let ty = self.db.split(self.db.declaration(decl).ty, class);
                self.view(ty, nominal.environment)
            })
            .collect()
    }
}
