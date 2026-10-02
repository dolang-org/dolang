use super::*;
use crate::typeck::r#type::{
    BinderOrigin, BinderSource, BoundRef, DeclKind, DeclSource, Declaration, Rest, SchemaItem,
    Supertype, UnionMember,
};
use dolang_util::alias;

mod basics;
mod functions;
mod inference;
mod instantiation;
mod lattice;
mod members;
mod narrow;
mod nominal;
mod projections;
mod rigids;
mod schemas;
mod skolems;
mod unknown;
mod unpack;

fn literal(db: &Database, n: i128) -> TypeId {
    db.intern(Type::Literal(Literal::Int(n)))
}

/// An `Int` literal a term gave
fn fresh(db: &Database, n: i128) -> TypeId {
    db.intern(Type::Fresh(Literal::Int(n)))
}

fn reference(db: &Database, depth: usize, slot: usize) -> TypeId {
    db.intern(Type::Bound {
        reference: BoundRef::new(depth, slot),
        kind: Kind::Type,
    })
}

fn binder(variance: Variance) -> Binder {
    Binder {
        kind: Kind::Type,
        binding: Binding::Positional,
        bound: None,
        default: None,
        variance,
    }
}

fn quantified(db: &Database, binders: Vec<Binder>, body: TypeId) -> TypeId {
    db.intern(Type::Quantified {
        binders: binders.into(),
        body,
    })
}

fn apply(db: &Database, base: TypeId, args: &[TypeId]) -> TypeId {
    db.intern(Type::Apply {
        base,
        args: args.iter().copied().map(Argument::Positional).collect(),
        kind: Kind::Type,
    })
}

fn schema(db: &Database, params: &[TypeId]) -> TypeId {
    db.intern(Type::Schema(
        params
            .iter()
            .map(|&ty| SchemaItem {
                multiplicity: Multiplicity::Required,
                element: Element::Positional(ty),
            })
            .collect(),
    ))
}

fn function(db: &Database, params: &[TypeId], result: TypeId) -> TypeId {
    db.intern(Type::Function(Function {
        params: schema(db, params),
        result,
        input: None,
        output: None,
    }))
}

fn reserve(db: &mut Database, kind: DeclKind, name: &str) -> (DeclId, TypeId, DeclSource) {
    let id = db.allocate();
    let ty = db.intern(Type::Decl(id));
    let source = DeclSource {
        kind,
        result_kind: Kind::Type,
        name: Some(db.intern_symbol(name)),
        span: UnitSpan {
            unit: db.allocate_unit(),
            span: (0u32..1).into(),
        },
    };
    (id, ty, source)
}

fn populate(
    db: &mut Database,
    id: DeclId,
    source: DeclSource,
    ty: TypeId,
    supertypes: Vec<TypeId>,
) {
    let binders = match db.ty(ty) {
        Type::Quantified { binders, .. } => (0..binders.len())
            .map(|_| BinderSource {
                name: source.name.unwrap(),
                span: source.span,
                bound: None,
                default: None,
                origin: BinderOrigin::Written,
            })
            .collect(),
        _ => Default::default(),
    };
    db.populate(
        id,
        Declaration {
            source,
            ty,
            binders,
            supertypes: inherited(supertypes),
            members: Default::default(),
        },
    );
}

/// Supertypes the runtime inherits from
fn inherited(supertypes: Vec<TypeId>) -> alias::Box<[Supertype]> {
    supertypes
        .into_iter()
        .map(|ty| Supertype { ty, runtime: true })
        .collect::<Vec<_>>()
        .into()
}

fn nominal(db: &mut Database, name: &str, binders: Vec<Binder>, supers: Vec<TypeId>) -> TypeId {
    let (id, ty, source) = reserve(db, DeclKind::Class, name);
    let body = quantified(db, binders, ty);
    populate(db, id, source, body, supers);
    ty
}

fn alias(db: &mut Database, name: &str, ty: TypeId) -> TypeId {
    let (id, result, source) = reserve(db, DeclKind::Alias, name);
    populate(db, id, source, ty, vec![]);
    result
}

fn check(db: &Database, a: TypeId, b: TypeId) -> Outcome {
    let mut solver = Solver::new(db);
    solver.constrain(solver.closed(a), solver.closed(b), Provenance::default());
    solver.solve().remove(0)
}

fn has(outcome: &Outcome, issue: Issue) -> bool {
    outcome.diagnostics.iter().any(|d| d.issue == issue)
}

fn variable_id(term: Term) -> InferVarId {
    let Term::Infer(id) = term else {
        panic!("expected inference variable")
    };
    id
}

/// A generic function declaration over `binders`
fn generic(db: &mut Database, binders: Vec<Binder>, body: TypeId) -> DeclId {
    let (id, _, source) = reserve(db, DeclKind::Function, "f");
    let ty = quantified(db, binders, body);
    populate(db, id, source, ty, vec![]);
    id
}

fn bounded(kind: Kind, binding: Binding, bound: Option<TypeId>) -> Binder {
    Binder {
        kind,
        binding,
        bound,
        default: None,
        variance: Variance::Invariant,
    }
}

/// Relate two types interpreted in `decl`'s group, while checking `decl`
fn under(db: &Database, decl: DeclId, a: TypeId, b: TypeId) -> Outcome {
    let mut s = Solver::new(db);
    let env = s.rigid_environment(decl);
    s.constrain(s.view(a, env), s.view(b, env), Provenance::default());
    s.solve().remove(0)
}

fn item(multiplicity: Multiplicity, element: Element) -> SchemaItem {
    SchemaItem {
        multiplicity,
        element,
    }
}

fn items(db: &Database, items: Vec<SchemaItem>) -> TypeId {
    db.intern(Type::Schema(items.into()))
}

/// `Int` with its literal backing registered
fn int(db: &mut Database) -> TypeId {
    let int = nominal(db, "Int", vec![], vec![]);
    db.set_intrinsic(Intrinsic::Int, int);
    int
}

fn positional(multiplicity: Multiplicity, ty: TypeId) -> SchemaItem {
    item(multiplicity, Element::Positional(ty))
}

fn keyed(multiplicity: Multiplicity, key: TypeId, value: TypeId) -> SchemaItem {
    item(multiplicity, Element::Keyed { key, value })
}

fn include(multiplicity: Multiplicity, schema: TypeId) -> SchemaItem {
    item(multiplicity, Element::Include(schema))
}

fn contradiction(outcome: &Outcome, contradiction: Contradiction) -> bool {
    outcome.status == Status::Contradicted && has(outcome, Issue::Contradiction(contradiction))
}

/// Default every variable whose lower bounds are solved, repeatedly, solving
/// between rounds, as a flow driver would
fn default_all(s: &mut Solver<'_>) -> Vec<Outcome> {
    loop {
        let unsolved: Vec<_> = s.unresolved().collect();
        let progress = unsolved
            .into_iter()
            .filter(|&id| s.default(id).is_ok())
            .count();
        let outcomes = s.solve();
        if progress == 0 {
            return outcomes;
        }
    }
}
