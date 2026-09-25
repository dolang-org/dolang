//! Signatures: each def and method signature completed with what its omissions
//! default to, the type of each field, and the declarations of `std` the checker
//! treats specially.
//!
//! An omitted annotation on a def is dynamic, whatever the def's visibility. An
//! omitted ambient channel is an implicit binder of the signature, following its
//! written binders, and bounded by nothing. A method's unannotated receiver is its
//! class applied to the class's own binders.

use super::{
    Ambient, BinderRef, DeclNode, Designated, KindOf, MisdeclaredIntrinsic, ParamTy, RestSlot, Sig,
    Slot, Tables, UnitDiag,
};
use crate::{
    Mode,
    ast::{ClassMember, Expr, Function, Method, Param},
    source,
    typeck::r#type::{DeclId, DeclKind, Intrinsic, Kind, Scope, UnitId},
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

/// Designate a top-level declaration of `std` that the checker treats specially.
/// A declaration of another module with the same name is only a lookalike.
fn designate(tables: &mut Tables<'_>, decl: DeclId, diags: &mut Vec<UnitDiag>) {
    let owner = &tables.decls[decl.index()];
    let Mode::Module { name: "std" } = tables.units[owner.unit.index()].compiler.mode else {
        return;
    };
    let Some(name) = owner.name.filter(|_| owner.outer.is_none()) else {
        return;
    };
    let (designated, expected) = match tables.text(owner.unit, name) {
        "Value" => (Designated::Value, DeclKind::Class),
        "Phantom" => (Designated::Phantom, DeclKind::OpaqueAlias),
        "Union" => (
            Designated::Intrinsic(Intrinsic::Union),
            DeclKind::OpaqueAlias,
        ),
        "Func" => (Designated::Intrinsic(Intrinsic::Func), DeclKind::Class),
        "Int" => (Designated::Intrinsic(Intrinsic::Int), DeclKind::Class),
        "Bool" => (Designated::Intrinsic(Intrinsic::Bool), DeclKind::Class),
        "Sym" => (Designated::Intrinsic(Intrinsic::Sym), DeclKind::Class),
        "Nil" => (Designated::Intrinsic(Intrinsic::Nil), DeclKind::Class),
        "Str" => (Designated::Intrinsic(Intrinsic::Str), DeclKind::Class),
        "Iter" => (Designated::Intrinsic(Intrinsic::Iter), DeclKind::Class),
        "Sink" => (Designated::Intrinsic(Intrinsic::Sink), DeclKind::Class),
        _ => return,
    };
    if owner.kind == expected {
        tables.designated.insert(decl, designated);
    } else {
        let expected = match expected {
            DeclKind::OpaqueAlias => "an opaque alias",
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
