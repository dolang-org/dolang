//! Reduction of calls.
//!
//! A call relates its arguments to its callee's parameters, and its callee's
//! result to its own. A callee that isn't a function is called in place of
//! something that is: a quantified function's body under fresh variables, an
//! overload or callable signature chosen by trials, each member of a union, or
//! a rigid's or literal's bound. A callee that is still a variable leaves the
//! call waiting for its solution: a call never bounds its callee.

use super::*;

impl Solver<'_> {
    /// Reduce a call, deriving its relations to what it calls
    pub(super) fn call(&self, obligation: ObligationId, call: Call) -> Result<(), Issue> {
        let head = self.head(call.callee)?;
        // A dynamic callee takes anything and gives the dynamic type, and a
        // callee without values is never called
        if self.is_unknown(&head) {
            return Ok(());
        }
        let through = |callee: Term, step: Step| {
            self.derive_call(obligation, Call { callee, ..call }, step);
            Ok(())
        };
        let view = match head {
            Head::Infer(_) => return Err(Residual::Inference.into()),
            Head::Skolem(id) => {
                return match self.abstract_bound(Abstract::Skolem(id)) {
                    Some((bound, step)) => through(bound, step),
                    None => Err(Issue::Contradiction(Contradiction::Rigid)),
                };
            }
            Head::Nominal(nominal) => return self.call_nominal(obligation, call, nominal),
            Head::Structural(view) => view,
        };
        if let Type::Unsupported { .. } = self.db.ty(view.ty) {
            return Err(UNREPRESENTED.into());
        }
        if view.ty == self.db.bottom() {
            return Ok(());
        }
        if view.ty == self.db.top() {
            return Err(Issue::Contradiction(Contradiction::Outside));
        }
        if self.rigid(view.ty)?.is_some() {
            return match self.abstract_bound(Abstract::Rigid(view.ty)) {
                Some((bound, step)) => through(bound, step),
                None => Err(Issue::Contradiction(Contradiction::Rigid)),
            };
        }
        match self.db.ty(view.ty) {
            Type::Function(function) => {
                self.call_function(obligation, call, view, function);
                Ok(())
            }
            Type::Quantified { binders, body } => {
                let others = [
                    Some(call.arguments),
                    Some(call.result),
                    call.input,
                    call.output,
                ];
                let others: Vec<Term> = others.into_iter().flatten().collect();
                let environment = self.instantiate_binders(view, binders, &others, obligation)?;
                through(self.view(*body, environment), Step::Instantiation)
            }
            Type::Overloaded { overloads, .. } => {
                // A lone overload is always chosen, so what doesn't fit it is
                // diagnosed as for any function
                if let &[overload] = &overloads[..] {
                    return through(view.child(overload), Step::Overload(0));
                }
                let calls: Vec<Call> = (overloads.iter())
                    .map(|&ty| Call {
                        callee: view.child(ty),
                        ..call
                    })
                    .collect();
                // Only what's passed chooses, as its twin says it
                let selections = (calls.iter())
                    .map(|&call| Call {
                        arguments: self.twin(call.arguments),
                        result: self.closed(self.db.top()),
                        ..call
                    })
                    .collect();
                let none = Issue::Contradiction(Contradiction::NoOverload);
                self.choose_call(obligation, calls, Some(selections), Step::Overload, none)
            }
            // Each member may be the callee
            Type::Union(members) => {
                self.conflicting(members)?;
                let mut derived = Vec::new();
                for (index, &member) in members.iter().enumerate() {
                    let (callee, step) = match member {
                        UnionMember::Type(ty) => (view.child(ty), Step::UnionMember(index)),
                        _ if self.rigid(member.id())?.is_some() => {
                            let Some(&(bound, _)) = self.rigid_bounds(member.id()).first() else {
                                return Err(Residual::Unsupported(
                                    "a projection of a rigid without a bound",
                                )
                                .into());
                            };
                            // The key may refer to the view's binders
                            let projected =
                                self.db.intern(Type::Union(vec![member.with(bound)].into()));
                            (view.child(projected), Step::RigidBound)
                        }
                        _ => return Err(Residual::Unsupported("an unevaluated projection").into()),
                    };
                    derived.push((callee, step));
                }
                for (callee, step) in derived {
                    through(callee, step)?;
                }
                Ok(())
            }
            // A literal is called as its class
            Type::Literal(literal) => {
                let intrinsic = literal.intrinsic();
                let backing =
                    (self.db.intrinsic(intrinsic)).ok_or(Residual::MissingIntrinsic(intrinsic))?;
                through(self.closed(backing), Step::IntrinsicBacking(intrinsic))
            }
            _ => Err(Residual::Unsupported("these structural types").into()),
        }
    }

    /// Derive a call's arguments below a function's parameters and its result
    /// below the call's. The ambient channels are implicit arguments; an omitted
    /// one is gradual.
    fn call_function(
        &self,
        obligation: ObligationId,
        call: Call,
        view: TypeView,
        function: &Function,
    ) {
        self.derive(
            obligation,
            call.arguments,
            view.child(function.params),
            Step::Arguments,
        );
        self.derive(
            obligation,
            view.child(function.result),
            call.result,
            Step::Return,
        );
        // A result that selects by a key is exposed, so a key its schema doesn't
        // admit is reported though nothing uses the result
        if let Type::Union(members) = self.db.ty(function.result)
            && members.iter().any(|member| member.key().is_some())
        {
            self.derive(
                obligation,
                self.closed(self.db.bottom()),
                view.child(function.result),
                Step::Return,
            );
        }
        let channel = |term: Option<Term>| term.unwrap_or_else(|| self.closed(self.db.unknown()));
        let own = |ty: Option<TypeId>| ty.map(|ty| view.child(ty));
        if call.input.is_some() || function.input.is_some() {
            let accepted = channel(own(function.input));
            self.derive(obligation, channel(call.input), accepted, Step::Input);
        }
        if call.output.is_some() || function.output.is_some() {
            let accepted = channel(own(function.output));
            self.derive(obligation, channel(call.output), accepted, Step::Output);
        }
    }

    /// Call a nominal value through the signatures it's called with, chosen by
    /// trials when it has several
    fn call_nominal(
        &self,
        obligation: ObligationId,
        call: Call,
        nominal: Nominal,
    ) -> Result<(), Issue> {
        let Some(signatures) = self.call_signatures(nominal)? else {
            return Ok(());
        };
        let signatures = match signatures.overloads.is_empty() {
            true => signatures.implementation.into_iter().collect(),
            false => signatures.overloads,
        };
        let calls = (signatures.into_iter())
            .map(|callee| Call { callee, ..call })
            .collect();
        let none = Issue::Contradiction(Contradiction::NoOverload);
        self.choose_call(obligation, calls, None, Step::Callable, none)
    }
}
