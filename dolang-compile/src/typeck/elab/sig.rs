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
    Ambient, BadNominee, BinderRef, DeclNode, Designated, Diag, KindOf, MisdeclaredIntrinsic,
    PIPES, ParamTy, Referent, RestSlot, Sig, Slot, Tables, UnitDiag,
    surface::{BinderKind, Decorator, Member, Method, ParamKind, Signature},
};
use crate::typeck::r#type::{DeclId, DeclKind, Intrinsic, Kind, Scope, UnitId, UnitSpan};

/// Complete every def and method signature, record every field's type, and find
/// the designated declarations of `std`.
pub(crate) fn signatures(tables: &mut Tables<'_>, diags: &mut Vec<UnitDiag>) {
    for index in 0..tables.decls.len() {
        let decl = DeclId::from_index(index);
        match &tables.decls[index].node {
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
                let mut fields = Vec::new();
                for member in &class.members {
                    let Member::Field(field) = member else {
                        continue;
                    };
                    let slot = field.annot.map_or(Slot::Unknown, Slot::Annot);
                    for name in &field.names {
                        fields.push(((decl, name.span), slot));
                    }
                }
                tables.fields.extend(fields);
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

/// The signature of a def or method
pub(crate) fn function<'t>(tables: &'t Tables<'_>, decl: DeclId, sig: usize) -> &'t Signature {
    match &tables.decls[decl.index()].node {
        DeclNode::Defs(defs) => &defs[sig].sig,
        DeclNode::Methods(methods) => &methods[sig].sig,
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

fn complete(tables: &Tables<'_>, decl: DeclId, sig: usize) -> Sig {
    let unit = tables.decls[decl.index()].unit;
    let func = function(tables, decl, sig);
    let receiver = match &tables.decls[decl.index()].node {
        DeclNode::Methods(methods) => method_scope(tables, unit, &methods[sig]) == Scope::Instance,
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
        ret: func.ret.map_or(Slot::Unknown, Slot::Annot),
    }
}

/// The parameters of a def, method or closure, each with the type it has, or the
/// default for an omitted annotation. An instance method's `receiver` is its class
/// unless annotated otherwise.
pub(crate) fn params(
    tables: &Tables<'_>,
    unit: UnitId,
    func: &Signature,
    receiver: bool,
) -> Vec<ParamTy> {
    func.params
        .iter()
        .enumerate()
        .map(|(index, param)| match param.kind {
            ParamKind::Pos if index == 0 && receiver => {
                ParamTy::Single(param.annot.map_or(Slot::SelfType, Slot::Annot))
            }
            ParamKind::Pos | ParamKind::Key { .. } | ParamKind::ConstKey { .. } => {
                ParamTy::Single(param.annot.map_or(Slot::Unknown, Slot::Annot))
            }
            ParamKind::Rest { kind, pattern } => ParamTy::Rest(match (param.annot, pattern) {
                (None, _) => RestSlot::Items(kind, Slot::Unknown),
                (Some(annot), true) => RestSlot::Pattern(annot),
                (Some(annot), false) => match tables.kind_of(unit, tables.site_ty(annot)) {
                    Some(Kind::Schema) => RestSlot::Pack(annot),
                    _ => RestSlot::Items(kind, Slot::Annot(annot)),
                },
            }),
        })
        .collect()
}

/// The scope of a method, from its decorators
pub(crate) fn method_scope(tables: &Tables<'_>, unit: UnitId, method: &Method) -> Scope {
    let mut scope = Scope::Instance;
    for decorator in &method.decorators {
        if let Decorator::Ident(name) = decorator {
            match tables.name(unit, *name) {
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
        let designated = match decorator {
            Decorator::Ident(name) => match tables.name(unit, *name) {
                "class" | "static" => continue,
                _ => match tables.referents.get(&UnitSpan {
                    unit,
                    span: name.span,
                }) {
                    Some(Referent::Decl(decl)) => tables.designated.get(decl).copied(),
                    _ => None,
                },
            },
            Decorator::Other => None,
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
    let Some(module) = tables.units[owner.unit.index()].module else {
        return;
    };
    let Some(name) = owner.name.filter(|_| owner.outer.is_none()) else {
        return;
    };
    let (designated, expected) = match (module, tables.name(owner.unit, name)) {
        ("strand", "PipeSender") => (Designated::PipeSender, DeclKind::OpaqueAlias),
        ("strand", "PipeReceiver") => (Designated::PipeReceiver, DeclKind::OpaqueAlias),
        ("std", "Value") => (Designated::Value, DeclKind::Class),
        ("std", "Never") => (Designated::Never, DeclKind::Alias),
        ("std", "Phantom") => (Designated::Phantom, DeclKind::OpaqueAlias),
        ("std", "Union") => (
            Designated::Intrinsic(Intrinsic::Union),
            DeclKind::OpaqueAlias,
        ),
        ("std", "Keys") => (
            Designated::Intrinsic(Intrinsic::Keys),
            DeclKind::OpaqueAlias,
        ),
        ("std", "Values") => (
            Designated::Intrinsic(Intrinsic::Values),
            DeclKind::OpaqueAlias,
        ),
        ("std", "Entries") => (
            Designated::Intrinsic(Intrinsic::Entries),
            DeclKind::OpaqueAlias,
        ),
        ("std", "IndexItem") => (
            Designated::Intrinsic(Intrinsic::IndexItem),
            DeclKind::OpaqueAlias,
        ),
        ("std", "AssignItem") => (
            Designated::Intrinsic(Intrinsic::AssignItem),
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
        ("std", "BaseDict") => (Designated::BaseDict, DeclKind::Class),
        ("std", "Tuple") => (Designated::Intrinsic(Intrinsic::Tuple), DeclKind::Class),
        ("std", "Record") => (Designated::Record, DeclKind::Class),
        ("std", "Range") => (Designated::Range, DeclKind::Class),
        ("std", "BaseIterable") => (Designated::BaseIterable, DeclKind::Protocol),
        ("std", "Spread") => (Designated::Spread, DeclKind::Protocol),
        ("std", "Unpack") => (Designated::Unpack, DeclKind::Protocol),
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
            DeclKind::Protocol => "a protocol",
            _ => "a class",
        };
        diags.push((
            owner.unit,
            Diag::new(MisdeclaredIntrinsic {
                span: name.span,
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
    let span = owner
        .name_span()
        .expect("a designated declaration is named");
    let described = match (tables.units[target.unit.index()].module, target.name) {
        (Some(module), Some(item)) => {
            format!("{module}.{}", tables.name(target.unit, item))
        }
        (_, Some(item)) => tables.name(target.unit, item).to_owned(),
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
        Diag::new(BadNominee {
            span,
            nominee: described,
            placeholder: name,
            declared: (target.unit == owner.unit)
                .then(|| target.name_span())
                .flatten(),
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
            binder.default.is_some() || matches!(binder.kind, BinderKind::Rest(_))
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
