//! Callables: values called through a member rather than being functions.
//!
//! A class object is called through its class-level `(call)` if it has one, a
//! class's own object through its static `(call)` too, and otherwise by
//! instantiation, which runs `(init)` and gives the instance. Any
//! other instance is called through its `(call)`. Flow checks such calls with
//! these signatures, and the solver relates such values to function types with
//! them.
//!
//! A value relates to a function type through its class only if the class
//! reaches `Func`, which marks it as passable as a function. It's below the type
//! when one of its own `(call)`'s signatures is, chosen by trials when it has several (see
//! [`Solver::choose_left`]). A class that doesn't reach `Func`, or none of whose
//! signatures fits, isn't below it: some of its values are of the class itself.
//! That a subclass may reach `Func`, or widen its `(call)`'s parameters and
//! narrow its result, matters only to narrowing (see [`narrow`]).

use super::*;
use crate::typeck::r#type::{MemberKey, Scope};

/// A method's signatures, each with its receiver parameter
#[derive(Clone, Debug, Default)]
pub(crate) struct Signature {
    /// Its `@def` signatures, empty unless it's overloaded
    pub(crate) overloads: Vec<TypeId>,
    /// Its implementation's signature, if it has one
    pub(crate) implementation: Option<TypeId>,
    /// The method they're the signatures of, if any
    pub(crate) function: Option<DeclId>,
}

impl Signature {
    /// Its one signature, unless it's overloaded
    pub(crate) fn single(&self) -> Option<TypeId> {
        self.overloads.is_empty().then_some(self.implementation)?
    }

    /// What it's called as: its one signature, or its overloads as a whole.
    /// `None` if it has neither.
    pub(crate) fn callee(&self, db: &Database) -> Option<TypeId> {
        if self.overloads.is_empty() {
            return self.implementation;
        }
        Some(db.intern(Type::Overloaded {
            overloads: self.overloads.iter().copied().collect(),
            implementation: self.implementation,
            function: self.function,
        }))
    }
}

/// How a class object is called (see [`Solver::constructor`])
#[derive(Clone, Debug)]
pub(crate) enum Constructor {
    /// Its `(call)`, as [`Signature::callee`] says it's called: a class-level
    /// one, `bound` to the class object passed first, or the exact object's
    /// static one. `None` if a signature doesn't reify.
    Call {
        callee: Option<TypeId>,
        bound: bool,
    },
    /// Instantiation, running `(init)`: functions from `(init)`'s arguments to
    /// the instance, quantified over the class's binders followed by
    /// `(init)`'s own. `None` if a signature doesn't take a receiver.
    Init(Option<Signature>),
    Dynamic,
}

impl Solver<'_> {
    /// How a class object is called: the class's `exact` object, or a `Type` of
    /// the class, which only a class-level `(call)` calls. Members are looked up
    /// on the class applied to its rigids, and the constructor's type abstracts
    /// them again.
    pub(crate) fn constructor(&self, class: DeclId, exact: bool) -> Constructor {
        if let Some(constructor) = self.cached(|solver| &solver.constructors, &(class, exact)) {
            return constructor;
        }
        let constructor = self.construct(class, exact);
        (self.constructors.borrow_mut()).insert((class, exact), constructor.clone());
        constructor
    }

    fn construct(&self, class: DeclId, exact: bool) -> Constructor {
        let db = self.db;
        let rigids = (db.rigids(class).into_iter())
            .map(Argument::Positional)
            .collect();
        let object = db.meta(class, rigids);
        let object = match exact {
            true => object,
            false => match db.widen_meta(object) {
                Some(object) => object,
                None => return Constructor::Dynamic,
            },
        };
        let solver = self.side_query(class);
        match solver.reach_member(solver.closed(object), None, self.call_key(), Access::Class) {
            Ok(Lookup::Found(found)) if found.scope != Scope::Instance => {
                let signature = match &found.kind {
                    FoundKind::Method(signatures) => solver.reified(signatures),
                    _ => None,
                };
                return Constructor::Call {
                    callee: signature.and_then(|signature| signature.callee(db)),
                    bound: found.scope == Scope::Class,
                };
            }
            Ok(Lookup::Found(_) | Lookup::Missing) => {}
            Ok(Lookup::Dynamic | Lookup::Fallback { .. }) | Err(_) => return Constructor::Dynamic,
        }
        match self.default_construction(class) {
            Some(signature) => Constructor::Init(signature),
            None => Constructor::Dynamic,
        }
    }

    /// A class `C[Bs]` applied to references to its own binders, as its
    /// declaration's group interprets them
    fn instance(&self, class: DeclId) -> TypeId {
        let db = self.db;
        let binders: &[Binder] = match db.ty(db.declaration(class).ty) {
            Type::Quantified { binders, .. } => binders,
            _ => &[],
        };
        let base = db.intern(Type::Decl(class));
        if binders.is_empty() {
            return base;
        }
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

    /// A class's default construction, which runs `(init)` whatever `(call)`
    /// the class has: `Type.(call)`'s, and a class object's without a `(call)`.
    /// `None` if it's dynamic, and `Some(None)` if a signature doesn't reify or
    /// take a receiver.
    pub(crate) fn default_construction(&self, class: DeclId) -> Option<Option<Signature>> {
        if let Some(construction) = self.cached(|solver| &solver.default_constructions, &class) {
            return construction;
        }
        let construction = self.construct_default(class);
        (self.default_constructions.borrow_mut()).insert(class, construction.clone());
        construction
    }

    fn construct_default(&self, class: DeclId) -> Option<Option<Signature>> {
        let db = self.db;
        db.intrinsic(Intrinsic::Type)?;
        let binders: &[Binder] = match db.ty(db.declaration(class).ty) {
            Type::Quantified { binders, .. } => binders,
            _ => &[],
        };
        let instance = db.substitute(self.instance(class), &db.rigids(class));
        let solver = self.side_query(class);
        let key = MemberKey {
            name: db.intern_symbol("init"),
            special: true,
            private: false,
        };
        // A function quantified over the class's binders, with its rigids
        // abstracted
        let abstracted = |ty| Some(db.merge_groups(binders, db.abstract_rigids(ty, class).ok()?));
        let signatures = match solver.member(solver.closed(instance), key) {
            Ok(Lookup::Found(found)) => match found.kind {
                FoundKind::Method(signatures) => signatures,
                _ => return Some(None),
            },
            Ok(Lookup::Missing) => {
                let constructor = db.intern(Type::Function(Function {
                    params: db.intern(Type::Schema(Vec::new().into())),
                    result: instance,
                    // It does no I/O, so its channels take any caller's
                    input: Some(db.top()),
                    output: Some(db.top()),
                }));
                return Some(abstracted(constructor).map(|constructor| Signature {
                    overloads: Vec::new(),
                    implementation: Some(constructor),
                    function: None,
                }));
            }
            Ok(Lookup::Dynamic | Lookup::Fallback { .. }) | Err(_) => return None,
        };
        // `(init)`'s signature taking the arguments a class object is called
        // with, and giving the instance
        let constructor = |signature: Term| {
            let signature = solver.reify(signature).ok()?;
            let (binders, body) = match db.ty(signature) {
                Type::Quantified { binders, body } => (Some(binders.clone()), *body),
                _ => (None, signature),
            };
            let (function, _) = drop_receiver(db, body)?;
            let Type::Function(function) = db.ty(function) else {
                unreachable!("a function without its receiver")
            };
            let function = db.intern(Type::Function(Function {
                result: instance,
                ..function.clone()
            }));
            abstracted(match binders {
                Some(binders) => db.intern(Type::Quantified {
                    binders,
                    body: function,
                }),
                None => function,
            })
        };
        let function = signatures.function;
        let overloads: Option<Vec<TypeId>> =
            signatures.overloads.into_iter().map(constructor).collect();
        let implementation = match signatures.implementation {
            Some(signature) => constructor(signature).map(Some),
            None => Some(None),
        };
        Some(
            overloads
                .zip(implementation)
                .map(|(overloads, implementation)| Signature {
                    overloads,
                    implementation,
                    function,
                }),
        )
    }

    /// Signatures reified, or `None` if any doesn't reify
    pub(crate) fn reified(&self, signatures: &Signatures) -> Option<Signature> {
        let reify = |&term| self.reify(term).ok();
        let overloads = (signatures.overloads.iter())
            .map(reify)
            .collect::<Option<_>>()?;
        let implementation = match &signatures.implementation {
            Some(term) => Some(reify(term)?),
            None => None,
        };
        Some(Signature {
            overloads,
            implementation,
            function: signatures.function,
        })
    }

    /// Whether a type is one a callable value relates to through its signatures:
    /// a function type, or one quantified over its binders
    pub(super) fn callee(&self, ty: TypeId) -> bool {
        match self.db.ty(ty) {
            Type::Function(_) => true,
            Type::Quantified { body, .. } => matches!(self.db.ty(*body), Type::Function(_)),
            _ => false,
        }
    }

    /// Relate a nominal value or a metatype to a function type, `expected` or
    /// quantified as `view` is, through the signatures it's called with
    pub(super) fn callable(
        &self,
        callee: Head,
        actual: Term,
        view: TypeView,
        expected: Term,
        obligation: ObligationId,
    ) -> Result<(), Issue> {
        if let Type::Quantified { binders, body } = self.db.ty(view.ty) {
            return self.skolemization(view, binders, *body, actual, obligation);
        }
        let Some(signatures) = self.call_signatures(callee)? else {
            return Ok(());
        };
        let signatures = match signatures.overloads.is_empty() {
            true => signatures.implementation.into_iter().collect(),
            false => signatures.overloads,
        };
        self.choose_left(
            obligation,
            signatures,
            expected,
            Step::Callable,
            Issue::Contradiction(Contradiction::NoOverload),
        )
    }

    /// The signatures a nominal value, or a class's metatype, is called with as
    /// a function, without their receivers. `None` if they're dynamic.
    pub(super) fn call_signatures(&self, callee: Head) -> Result<Option<Signatures>, Issue> {
        let nominal = match callee {
            Head::Meta(class) => return self.object_signatures(class, true),
            Head::Nominal(nominal) => nominal,
            _ => unreachable!("a callee that isn't nominal"),
        };
        if self.is_intrinsic(nominal.declaration, Intrinsic::Type) {
            let [class] = nominal.arguments[..] else {
                return Err(Residual::Unsupported("a type object with several arguments").into());
            };
            let class = match self.head(class)? {
                Head::Nominal(class) => class,
                Head::Infer(_) => return Err(Residual::Inference.into()),
                head if self.is_unknown(&head) => return Ok(None),
                _ => return Err(Residual::Unsupported("a type object of a structural type").into()),
            };
            return self.object_signatures(class, false);
        }
        let key = self.call_key();
        let func = (self.db.intrinsic(Intrinsic::Func))
            .ok_or(Residual::MissingIntrinsic(Intrinsic::Func))?;
        let &Type::Decl(func) = self.db.ty(func) else {
            return Err(Residual::MissingIntrinsic(Intrinsic::Func).into());
        };
        // Only a class reaching `Func` is a function, as narrowing by `Func` takes it
        if self
            .ancestor(nominal.clone(), func, &mut FxHashSet::default(), 0)?
            .is_none()
        {
            return Err(Issue::Contradiction(Contradiction::UnrelatedNominals));
        }
        match self.instance_member(nominal, key)? {
            Lookup::Found(found) => self.bound(&found.kind),
            Lookup::Missing => Err(Residual::Unsupported("a `Func` without `(call)`").into()),
            Lookup::Dynamic | Lookup::Fallback { .. } => Ok(None),
        }
    }

    /// The signatures a class object is called with: its class-level `(call)`,
    /// and otherwise, only for the `exact` object, its class's own static
    /// `(call)` or instantiation through `(init)`. A `Type[C]` may hold a
    /// subclass's object, which inherits no static and whose `(init)` needn't
    /// conform, so without a class-level `(call)` it isn't callable.
    fn object_signatures(&self, class: Nominal, exact: bool) -> Result<Option<Signatures>, Issue> {
        match self.object_member(class.clone(), exact, self.call_key(), Access::Class)? {
            Lookup::Found(found) if found.scope == Scope::Class => {
                return self.bound(&found.kind);
            }
            Lookup::Found(found) if found.scope == Scope::Static => {
                return self.unbound(&found.kind);
            }
            Lookup::Found(_) | Lookup::Missing => {}
            Lookup::Dynamic | Lookup::Fallback { .. } => return Ok(None),
        }
        if !exact {
            return Err(Issue::Contradiction(Contradiction::Outside));
        }
        let signatures = match self.default_construction(class.declaration) {
            Some(Some(signatures)) => signatures,
            Some(None) => {
                return Err(Residual::Unsupported("an `(init)` without a receiver").into());
            }
            None => return Ok(None),
        };
        // Applied to the class's arguments, as a method is
        let count = match self.db.ty(self.db.declaration(class.declaration).ty) {
            Type::Quantified { binders, .. } => binders.len(),
            _ => 0,
        };
        let applied = |ty| self.view(self.db.split(ty, count), class.environment);
        Ok(Some(Signatures {
            overloads: signatures.overloads.into_iter().map(applied).collect(),
            implementation: signatures.implementation.map(applied),
            function: signatures.function,
        }))
    }

    /// `(call)`'s key
    fn call_key(&self) -> MemberKey {
        MemberKey {
            name: self.db.intern_symbol("call"),
            special: true,
            private: false,
        }
    }

    /// A static method's signatures, which take no receiver. `None` if they're
    /// dynamic.
    fn unbound(&self, kind: &FoundKind) -> Result<Option<Signatures>, Issue> {
        match kind {
            FoundKind::Method(signatures) => Ok(Some(signatures.clone())),
            FoundKind::Unknown => Ok(None),
            FoundKind::Field(_) | FoundKind::Property { .. } => {
                Err(Residual::Unsupported("a `(call)` that isn't a method").into())
            }
        }
    }

    /// A method's signatures bound to their receiver. `None` if they're dynamic.
    fn bound(&self, kind: &FoundKind) -> Result<Option<Signatures>, Issue> {
        let signatures = match kind {
            FoundKind::Method(signatures) => signatures,
            FoundKind::Unknown => return Ok(None),
            FoundKind::Field(_) | FoundKind::Property { .. } => {
                return Err(Residual::Unsupported("a `(call)` that isn't a method").into());
            }
        };
        let bound = |signature: Term| {
            let Term::View(view) = signature else {
                return None;
            };
            let ty = bound_method(self.db, view.ty)?;
            Some(self.view(ty, view.environment))
        };
        let unbound = || Residual::Unsupported("a method whose receiver mentions its own binders");
        let overloads = (signatures.overloads.iter().copied())
            .map(bound)
            .collect::<Option<_>>()
            .ok_or_else(unbound)?;
        let implementation = match signatures.implementation {
            Some(signature) => Some(bound(signature).ok_or_else(unbound)?),
            None => None,
        };
        Ok(Some(Signatures {
            overloads,
            implementation,
            function: signatures.function,
        }))
    }
}

/// A method bound to its receiver: its signature without its receiver
/// parameter. `None` if that parameter mentions the method's own binders, which
/// binding would have to solve, or it has none.
pub(crate) fn bound_method(db: &Database, signature: TypeId) -> Option<TypeId> {
    let (binders, body) = match db.ty(signature) {
        Type::Quantified { binders, body } => (Some(binders.clone()), *body),
        _ => (None, signature),
    };
    let (function, receiver) = drop_receiver(db, body)?;
    let Some(binders) = binders else {
        return Some(function);
    };
    let mut own = false;
    db.walk(receiver, |node, depth| {
        if let Type::Bound { reference, .. } = *db.ty(node) {
            own |= u32::from(reference.depth) == depth;
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
pub(crate) fn drop_receiver(db: &Database, function: TypeId) -> Option<(TypeId, TypeId)> {
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
