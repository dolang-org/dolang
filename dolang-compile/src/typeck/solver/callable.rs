//! Callables: values called through a member rather than being functions.
//!
//! A class object is called through its class-level `(call)` if it has one, and
//! otherwise by instantiation, which runs `(init)` and gives the instance. Any
//! other instance is called through its `(call)`. Flow checks such calls with
//! these signatures, and the solver relates such values to function types with
//! them.
//!
//! A value relates to a function type through its class only if the class
//! reaches `Func`, which marks it as passable as a function. It's below the type
//! when one of its signatures is, chosen by trials when it has several (see
//! [`Solver::choose_left`]). A class that doesn't reach `Func`, or none of whose
//! signatures fits, refutes nothing: a subclass may reach `Func`, and may widen
//! its `(call)`'s parameters and narrow its result. A judgment through a chosen
//! signature is likewise never contradicted (see [`Step::Callable`]). Refuting
//! one waits on final classes.

use super::*;
use crate::typeck::r#type::{MemberKey, Scope};

/// What relating a callable is when none of its signatures fits
pub(super) const UNFIT: Residual =
    Residual::Unsupported("a callable none of whose signatures fits");

/// A method's signatures, each with its receiver parameter
#[derive(Clone, Debug, Default)]
pub(crate) struct Signature {
    /// Its `@def` signatures, empty unless it's overloaded
    pub(crate) overloads: Vec<TypeId>,
    /// Its implementation's signature, if it has one
    pub(crate) implementation: Option<TypeId>,
}

impl Signature {
    /// Its one signature, unless it's overloaded
    pub(crate) fn single(&self) -> Option<TypeId> {
        self.overloads.is_empty().then_some(self.implementation)?
    }
}

/// How a class object is called (see [`Solver::constructor`])
#[derive(Clone, Debug)]
pub(crate) enum Constructor {
    /// Its class-level `(call)`, which is passed the class object first. `None`
    /// if it's overloaded.
    Call(Option<TypeId>),
    /// Instantiation, running `(init)`: functions from `(init)`'s arguments to
    /// the instance, quantified over the class's binders followed by
    /// `(init)`'s own. `None` if a signature doesn't take a receiver.
    Init(Option<Signature>),
    Dynamic,
}

impl Solver<'_> {
    /// How a class object is called. `(init)` is looked up on the class applied to
    /// its rigids, and the constructor's type abstracts them again.
    pub(crate) fn constructor(&self, class: DeclId) -> Constructor {
        if let Some(constructor) = self.constructors.borrow().get(&class) {
            return constructor.clone();
        }
        let constructor = self.construct(class);
        (self.constructors.borrow_mut()).insert(class, constructor.clone());
        constructor
    }

    fn construct(&self, class: DeclId) -> Constructor {
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
        // A side query, its class's rigids assumed
        let mut solver = Solver::new(db);
        solver.scope = self.scope.clone();
        solver.assume(class);
        #[cfg(feature = "debug")]
        {
            solver.names = self.names.clone();
        }
        let key = |name| MemberKey {
            name: db.intern_symbol(name),
            special: true,
            private: false,
        };
        match solver.member(solver.closed(object), key("call")) {
            Ok(Lookup::Found(found)) if found.scope == Scope::Class => {
                let signature = match &found.kind {
                    FoundKind::Method(Signatures {
                        overloads,
                        implementation: Some(signature),
                    }) if overloads.is_empty() => solver.reify(*signature).ok(),
                    _ => None,
                };
                return Constructor::Call(signature);
            }
            Ok(Lookup::Found(_) | Lookup::Missing) => {}
            Ok(Lookup::Dynamic | Lookup::Fallback { .. }) | Err(_) => return Constructor::Dynamic,
        }
        // A function quantified over the class's binders, with its rigids
        // abstracted
        let abstracted = |ty| Some(db.merge_groups(&binders, db.abstract_rigids(ty, class).ok()?));
        let signatures = match solver.member(solver.closed(instance), key("init")) {
            Ok(Lookup::Found(found)) => match found.kind {
                FoundKind::Method(signatures) => signatures,
                _ => return Constructor::Init(None),
            },
            Ok(Lookup::Missing) => {
                let constructor = db.intern(Type::Function(Function {
                    params: db.intern(Type::Schema(Vec::new().into())),
                    result: instance,
                    input: None,
                    output: None,
                }));
                return Constructor::Init(abstracted(constructor).map(|constructor| Signature {
                    overloads: Vec::new(),
                    implementation: Some(constructor),
                }));
            }
            Ok(Lookup::Dynamic | Lookup::Fallback { .. }) | Err(_) => {
                return Constructor::Dynamic;
            }
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
        let overloads: Option<Vec<TypeId>> =
            signatures.overloads.into_iter().map(constructor).collect();
        let implementation = match signatures.implementation {
            Some(signature) => constructor(signature).map(Some),
            None => Some(None),
        };
        Constructor::Init(
            overloads
                .zip(implementation)
                .map(|(overloads, implementation)| Signature {
                    overloads,
                    implementation,
                }),
        )
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

    /// Relate a nominal value to a function type, `expected` or quantified as
    /// `view` is, through the signatures it's called with
    pub(super) fn callable(
        &self,
        nominal: Nominal,
        actual: Term,
        view: TypeView,
        expected: Term,
        obligation: ObligationId,
    ) -> Result<(), Issue> {
        if let Type::Quantified { binders, body } = self.db.ty(view.ty) {
            return self.skolemization(view, binders, *body, actual, obligation);
        }
        let Some(signatures) = self.call_signatures(nominal)? else {
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
            UNFIT.into(),
        )
    }

    /// The signatures a nominal value is called with as a function, without
    /// their receivers. `None` if they're dynamic.
    fn call_signatures(&self, nominal: Nominal) -> Result<Option<Signatures>, Issue> {
        let key = MemberKey {
            name: self.db.intern_symbol("call"),
            special: true,
            private: false,
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
            match self.object_member(class.clone(), key)? {
                Lookup::Found(found) if found.scope == Scope::Class => {
                    return self.bound(&found.kind);
                }
                Lookup::Found(_) | Lookup::Missing => {}
                Lookup::Dynamic | Lookup::Fallback { .. } => return Ok(None),
            }
            let signatures = match self.constructor(class.declaration) {
                Constructor::Init(Some(signatures)) => signatures,
                Constructor::Init(None) => {
                    return Err(Residual::Unsupported("an `(init)` without a receiver").into());
                }
                Constructor::Call(_) | Constructor::Dynamic => return Ok(None),
            };
            // Applied to the class's arguments, as a method is
            let count = match self.db.ty(self.db.declaration(class.declaration).ty) {
                Type::Quantified { binders, .. } => binders.len(),
                _ => 0,
            };
            let applied = |ty| self.view(self.db.split(ty, count), class.environment);
            return Ok(Some(Signatures {
                overloads: signatures.overloads.into_iter().map(applied).collect(),
                implementation: signatures.implementation.map(applied),
            }));
        }
        let func = (self.db.intrinsic(Intrinsic::Func))
            .ok_or(Residual::MissingIntrinsic(Intrinsic::Func))?;
        let &Type::Decl(func) = self.db.ty(func) else {
            return Err(Residual::MissingIntrinsic(Intrinsic::Func).into());
        };
        let Some(reached) = self.ancestor(nominal.clone(), func, &mut HashSet::new(), 0)? else {
            return Err(Residual::Unsupported("a class that doesn't reach `Func`").into());
        };
        // Arguments given to `Func` describe the function its `(call)` conforms to;
        // without them, its own `(call)` says
        let mut bare = true;
        for &argument in &reached.arguments {
            bare &= self.is_unknown(&self.head(argument)?);
        }
        let receiver = match bare {
            true => nominal,
            false => reached,
        };
        match self.instance_member(receiver, key)? {
            Lookup::Found(found) => self.bound(&found.kind),
            Lookup::Missing => Err(Residual::Unsupported("a `Func` without `(call)`").into()),
            Lookup::Dynamic | Lookup::Fallback { .. } => Ok(None),
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
