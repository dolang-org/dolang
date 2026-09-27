//! Signatures: each def and method signature completed with what its omissions
//! default to, the type of each field, and the declarations of `std` the checker
//! treats specially.
//!
//! An omitted annotation on a def is dynamic, whatever the def's visibility. An
//! omitted ambient channel is an implicit binder of the signature, following its
//! written binders; population bounds it. A method's unannotated receiver is its
//! class applied to the class's own binders; an annotated one specializes the
//! method once the database is sealed.

use super::{
    Ambient, BadNominee, BinderRef, DeclNode, Designated, KindOf, MisdeclaredIntrinsic, PIPES,
    ParamTy, Referent, RestSlot, Sig, Slot, Tables, UnitDiag,
};
use crate::{
    Mode,
    ast::{BinderKind, ClassMember, Expr, Function, Method, Param},
    source,
    typeck::r#type::{DeclId, DeclKind, Intrinsic, Kind, Scope, UnitId, UnitSpan},
};

/// Complete every def and method signature, record every field's type, and find
/// the designated declarations of `std`.
pub(crate) fn signatures(tables: &mut Tables<'_>, diags: &mut Vec<UnitDiag>) {
    for index in 0..tables.decls.len() {
        let decl = DeclId::from_index(index);
        match tables.decls[index].node {
            DeclNode::Defs(_) | DeclNode::Methods(_) => {
                for sig in 0..tables.sig_count(decl) {
                    let completed = complete(tables, decl, sig);
                    for ambient in [completed.input, completed.output] {
                        if let Ambient::Implicit(binder) = ambient {
                            tables.binder_kinds.insert(
                                binder,
                                KindOf {
                                    kind: Kind::Type,
                                    flexible: false,
                                },
                            );
                        }
                    }
                    tables.sigs.insert((decl, sig), completed);
                }
            }
            DeclNode::Class(class) => {
                for member in &class.body.members {
                    let ClassMember::Field(field) = member else {
                        continue;
                    };
                    let slot = field
                        .ty
                        .as_ref()
                        .map_or(Slot::Unknown, |annot| Slot::Annot(&annot.ty));
                    for name in &field.fields {
                        tables.fields.insert((decl, name.ident.span), slot);
                    }
                }
            }
            DeclNode::Alias(_) | DeclNode::Closure(_) => {}
        }
        designate(tables, decl, diags);
    }
    let mut placeholders: Vec<_> = tables
        .designated
        .iter()
        .filter_map(|(&decl, designated)| match designated {
            Designated::PipeSender => Some((decl, PIPES[0])),
            Designated::PipeReceiver => Some((decl, PIPES[1])),
            _ => None,
        })
        .collect();
    placeholders.sort();
    for (placeholder, name) in placeholders {
        let nominee = nominate(tables, placeholder, name, diags);
        tables.pipes.insert(placeholder, nominee);
    }
}

/// The function of a def or method signature
pub(crate) fn function<'u>(tables: &Tables<'u>, decl: DeclId, sig: usize) -> &'u Function {
    match tables.decls[decl.index()].node {
        DeclNode::Defs(ref defs) => &defs[sig].func,
        DeclNode::Methods(ref methods) => &methods[sig].func,
        _ => unreachable!("only a def or method has a signature"),
    }
}

/// The ambient channels of a def or method signature, as a function type written
/// within it without its own takes them: the channel written on the signature, or
/// the implicit binder standing for one omitted.
pub(crate) fn channels(tables: &Tables<'_>, decl: DeclId, sig: usize) -> [Ambient; 2] {
    let func = function(tables, decl, sig);
    let mut slot = tables.binders(decl, sig).len();
    [func.input.is_some(), func.output.is_some()].map(|written| {
        if written {
            Ambient::Of(decl, sig)
        } else {
            let binder = BinderRef { decl, sig, slot };
            slot += 1;
            Ambient::Implicit(binder)
        }
    })
}

fn complete<'u>(tables: &Tables<'u>, decl: DeclId, sig: usize) -> Sig<'u> {
    let unit = tables.decls[decl.index()].unit;
    let func = function(tables, decl, sig);
    let receiver = match tables.decls[decl.index()].node {
        DeclNode::Methods(ref methods) => {
            method_scope(tables, unit, methods[sig]) == Scope::Instance
        }
        _ => false,
    };
    let params = params(tables, unit, func, receiver);
    let [input, output] = channels(tables, decl, sig).map(|ambient| match ambient {
        Ambient::Of(..) => Ambient::Written,
        ambient => ambient,
    });
    Sig {
        params,
        receiver,
        input,
        output,
        ret: func
            .ret
            .as_ref()
            .map_or(Slot::Unknown, |ret| Slot::Annot(&ret.ty)),
    }
}

/// The parameters of a def, method or closure, each with the type it has, or the
/// default for an omitted annotation. An instance method's `receiver` is its class
/// unless annotated otherwise.
pub(crate) fn params<'u>(
    tables: &Tables<'u>,
    unit: UnitId,
    func: &'u Function,
    receiver: bool,
) -> Vec<(&'u Param, ParamTy<'u>)> {
    func.params
        .iter()
        .enumerate()
        .map(|(index, param)| {
            let ty = match param {
                Param::Pos { ty, .. } if index == 0 && receiver => ParamTy::Single(
                    ty.as_ref()
                        .map_or(Slot::SelfType, |annot| Slot::Annot(&annot.ty)),
                ),
                Param::Pos { ty, .. } | Param::Key { ty, .. } | Param::ConstKey { ty, .. } => {
                    ParamTy::Single(
                        ty.as_ref()
                            .map_or(Slot::Unknown, |annot| Slot::Annot(&annot.ty)),
                    )
                }
                Param::Rest {
                    kind,
                    ty,
                    type_ellipsis_span,
                    ..
                } => ParamTy::Rest(match (ty, type_ellipsis_span) {
                    (None, _) => RestSlot::Items(*kind, Slot::Unknown),
                    (Some(annot), Some(_)) => RestSlot::Pattern(&annot.ty),
                    (Some(annot), None) => match tables.kind_of(unit, &annot.ty) {
                        Some(Kind::Schema) => RestSlot::Pack(&annot.ty),
                        _ => RestSlot::Items(*kind, Slot::Annot(&annot.ty)),
                    },
                }),
            };
            (param, ty)
        })
        .collect()
}

/// The scope of a method, from its decorators
pub(crate) fn method_scope(tables: &Tables<'_>, unit: UnitId, method: &Method) -> Scope {
    let mut scope = Scope::Instance;
    for decorator in &method.decorators {
        if let Expr::Ident(ident) = &decorator.expr {
            match tables.text(unit, ident.span) {
                "class" => scope = Scope::Class,
                "static" => scope = Scope::Static,
                _ => {}
            }
        }
    }
    scope
}

/// What a method's decorators make of it
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Form {
    Plain,
    Getter,
    Setter,
    /// A value of unknown type
    Unknown,
}

/// The form of a method, from its decorators. Only `class` and `static` have a
/// meaning fixed by syntax. Any other decorator may replace the method with any
/// value; until decorator applications are evaluated, std's `getter` and `setter`
/// are recognized by what their names resolve to, and anything else leaves a value
/// of unknown type.
pub(crate) fn method_form(tables: &Tables<'_>, unit: UnitId, method: &Method) -> Form {
    let mut form = Form::Plain;
    for decorator in &method.decorators {
        let designated = match &decorator.expr {
            Expr::Ident(ident) => match tables.text(unit, ident.span) {
                "class" | "static" => continue,
                _ => match tables.referents.get(&UnitSpan {
                    unit,
                    span: ident.span,
                }) {
                    Some(Referent::Decl(decl)) => tables.designated.get(decl).copied(),
                    _ => None,
                },
            },
            _ => None,
        };
        form = match (form, designated) {
            (Form::Plain, Some(Designated::Getter)) => Form::Getter,
            (Form::Plain, Some(Designated::Setter)) => Form::Setter,
            _ => return Form::Unknown,
        };
    }
    form
}

/// Designate a top-level declaration of `std` or `strand` that the checker treats
/// specially. A declaration of another module with the same name is only a
/// lookalike.
fn designate(tables: &mut Tables<'_>, decl: DeclId, diags: &mut Vec<UnitDiag>) {
    let owner = &tables.decls[decl.index()];
    let Mode::Module { name: module } = tables.units[owner.unit.index()].compiler.mode else {
        return;
    };
    let Some(name) = owner.name.filter(|_| owner.outer.is_none()) else {
        return;
    };
    let (designated, expected) = match (module, tables.text(owner.unit, name)) {
        ("strand", "PipeSender") => (Designated::PipeSender, DeclKind::OpaqueAlias),
        ("strand", "PipeReceiver") => (Designated::PipeReceiver, DeclKind::OpaqueAlias),
        ("std", "Value") => (Designated::Value, DeclKind::Class),
        ("std", "Phantom") => (Designated::Phantom, DeclKind::OpaqueAlias),
        ("std", "Union") => (
            Designated::Intrinsic(Intrinsic::Union),
            DeclKind::OpaqueAlias,
        ),
        ("std", "Func") => (Designated::Intrinsic(Intrinsic::Func), DeclKind::Class),
        ("std", "Int") => (Designated::Intrinsic(Intrinsic::Int), DeclKind::Class),
        ("std", "Bool") => (Designated::Intrinsic(Intrinsic::Bool), DeclKind::Class),
        ("std", "Sym") => (Designated::Intrinsic(Intrinsic::Sym), DeclKind::Class),
        ("std", "Nil") => (Designated::Intrinsic(Intrinsic::Nil), DeclKind::Class),
        ("std", "Str") => (Designated::Intrinsic(Intrinsic::Str), DeclKind::Class),
        ("std", "Iter") => (Designated::Intrinsic(Intrinsic::Iter), DeclKind::Class),
        ("std", "Sink") => (Designated::Intrinsic(Intrinsic::Sink), DeclKind::Class),
        ("std", "Type") => (Designated::Intrinsic(Intrinsic::Type), DeclKind::Class),
        ("std", "Fmt") => (Designated::Fmt, DeclKind::Class),
        ("std", "FmtValue") => (Designated::FmtValue, DeclKind::Class),
        ("std", "FmtParam") => (Designated::FmtParam, DeclKind::Class),
        ("std", "Float") => (Designated::Float, DeclKind::Class),
        ("std", "Bin") => (Designated::Bin, DeclKind::Class),
        ("std", "Array") => (Designated::Array, DeclKind::Class),
        ("std", "Dict") => (Designated::Dict, DeclKind::Class),
        ("std", "Tuple") => (Designated::Tuple, DeclKind::Class),
        ("std", "Record") => (Designated::Record, DeclKind::Class),
        ("std", "Range") => (Designated::Range, DeclKind::Class),
        ("std", "getter") => (Designated::Getter, DeclKind::Function),
        ("std", "setter") => (Designated::Setter, DeclKind::Function),
        _ => return,
    };
    if owner.kind == expected {
        tables.designated.insert(decl, designated);
    } else {
        let expected = match expected {
            DeclKind::OpaqueAlias => "an opaque alias",
            DeclKind::Function => "a def",
            _ => "a class",
        };
        diags.push((
            owner.unit,
            source::Diag::new(MisdeclaredIntrinsic {
                span: name,
                expected,
            }),
        ));
    }
}

/// The type a designated pipe placeholder stands for: its nominee, when that is a
/// class the placeholder's type arguments can be passed to positionally. A nominee
/// that isn't checked is `None`, as is one diagnosed here.
fn nominate(
    tables: &Tables<'_>,
    placeholder: DeclId,
    name: &'static str,
    diags: &mut Vec<UnitDiag>,
) -> Option<DeclId> {
    // An external nominee is unknown, and an erroneous one already diagnosed
    let Some(&Referent::Decl(nominee)) = tables.nominees.get(name) else {
        return None;
    };
    let owner = &tables.decls[placeholder.index()];
    let target = &tables.decls[nominee.index()];
    let span = owner.name.expect("a designated declaration is named");
    let described = match (
        &tables.units[target.unit.index()].compiler.mode,
        target.name,
    ) {
        (Mode::Module { name: module }, Some(item)) => {
            format!("{module}.{}", tables.text(target.unit, item))
        }
        (_, Some(item)) => tables.text(target.unit, item).to_owned(),
        (_, None) => unreachable!("an export is named"),
    };
    let reason = match target.kind {
        // A class can't reach the placeholder again, so the alias can't cycle
        DeclKind::Class | DeclKind::Protocol => {
            (!passes(tables, placeholder, nominee)).then_some(false)
        }
        _ => Some(true),
    };
    let Some(not_class) = reason else {
        return Some(nominee);
    };
    diags.push((
        owner.unit,
        source::Diag::new(BadNominee {
            span,
            nominee: described,
            placeholder: name,
            declared: (target.unit == owner.unit).then_some(target.name).flatten(),
            not_class,
        }),
    ));
    None
}

/// Whether a placeholder's type arguments can be passed to `nominee` in order,
/// each filling a positional binder of kind `Type` with no bound, while every
/// binder left over has a default or is a rest
fn passes(tables: &Tables<'_>, placeholder: DeclId, nominee: DeclId) -> bool {
    let given = tables.binders(placeholder, 0);
    if given
        .iter()
        .any(|binder| !matches!(binder.kind, BinderKind::Pos))
    {
        return false;
    }
    let slots = positional(tables, nominee);
    if slots.len() < given.len() {
        return false;
    }
    let written = tables.binders(nominee, 0);
    let filled = &slots[..given.len()];
    written.iter().enumerate().all(|(slot, binder)| {
        if filled.contains(&slot) {
            binder.bound.is_none()
                && tables.binder_kinds[&BinderRef {
                    decl: nominee,
                    sig: 0,
                    slot,
                }]
                    .kind
                    == Kind::Type
        } else {
            binder.default.is_some() || matches!(binder.kind, BinderKind::Rest { .. })
        }
    })
}

/// The slots of a declaration's positional binders, which a pipe placeholder's type
/// arguments fill in order
pub(crate) fn positional(tables: &Tables<'_>, decl: DeclId) -> Vec<usize> {
    tables
        .binders(decl, 0)
        .iter()
        .enumerate()
        .filter(|(_, binder)| matches!(binder.kind, BinderKind::Pos))
        .map(|(slot, _)| slot)
        .collect()
}
