//! Signatures: each def and method signature completed with what its omissions
//! default to, the type of each field, and the declarations of `std` the checker
//! treats specially.
//!
//! An omitted annotation on a def is dynamic, whatever the def's visibility. An
//! omitted ambient channel is an unbounded implicit binder of the signature,
//! following its written binders, in either mode. An instance method's
//! unannotated receiver is its class applied to the class's own binders, and a
//! class method's is `Type` of that; an annotated instance method receiver
//! specializes an overload, and is checked, once the database is sealed.
//!
//! A strict unit must annotate its parameters, fields and exported variables
//! instead. Its omitted
//! return types are `nil`, so its signatures come from its declarations alone,
//! with nothing dynamic.

use super::{
    Ambient, BadNominee, BinderRef, DeclNode, Designated, Diag, KindOf, MisdeclaredIntrinsic,
    MissingAnnotation, MissingIntrinsic, PIPES, ParamTy, Referent, RestSlot, Sig, Slot, Tables,
    UnitDiag,
    surface::{BinderKind, Class, Decorator, Member, MemberScope, Method, ParamKind, Signature},
};
use crate::diag::Span;
use crate::typeck::r#type::{DeclId, DeclKind, Intrinsic, Kind, Scope, UnitId, UnitSpan};

/// Complete every def and method signature, record every field's type, and find
/// the designated declarations of `std`.
pub(crate) fn signatures(tables: &mut Tables<'_>, diags: &mut Vec<UnitDiag>) {
    let decls: Vec<_> = tables.decl_ids().collect();
    for decl in decls {
        match &tables.decl(decl).node {
            DeclNode::Defs(_) | DeclNode::Methods(_) => {
                for sig in 0..tables.sig_count(decl) {
                    let completed = complete(tables, decl, sig);
                    for ambient in [completed.input, completed.output] {
                        if let Ambient::Implicit(binder) = ambient {
                            tables.facts_mut(binder.decl).binder_kinds.insert(
                                binder,
                                KindOf {
                                    kind: Kind::Type,
                                    flexible: false,
                                },
                            );
                        }
                    }
                    tables.facts_mut(decl).sigs.insert((decl, sig), completed);
                    missing_params(tables, decl, sig, diags);
                }
            }
            DeclNode::Class(class) => {
                let unit = tables.decl(decl).unit;
                let strict = tables.info(unit).strict;
                let mut fields = Vec::new();
                for member in &class.members {
                    let Member::Field(field) = member else {
                        continue;
                    };
                    let slot = field.annot.map_or(Slot::Unknown, Slot::Annot);
                    for name in &field.names {
                        fields.push(((decl, name.span), slot));
                        if strict && field.annot.is_none() {
                            diags.push((
                                unit,
                                Diag::new(MissingAnnotation {
                                    span: name.span,
                                    what: "field",
                                }),
                            ));
                        }
                    }
                }
                tables.facts_mut(decl).fields.extend(fields);
            }
            DeclNode::Alias(_) | DeclNode::Closure(_) => {}
        }
        designate(tables, decl, diags);
    }
    missing_intrinsics(tables, diags);
    missing_values(tables, diags);
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
    match &tables.decl(decl).node {
        DeclNode::Defs(defs) => &defs[sig].sig,
        DeclNode::Methods(methods) => &methods[sig].sig,
        _ => unreachable!("only a def or method has a signature"),
    }
}

/// The ambient channels of a def or method signature, as a function type written
/// within it without its own takes them: the channel written on the signature, or
/// for one omitted, the implicit binder standing for it.
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

/// The ambient channels of a function type written in a site whose function types
/// take the channels of `ambient`, a def or method signature, or else `Value`:
/// `Written` for each channel in `written`
pub(crate) fn func_channels(
    tables: &Tables<'_>,
    ambient: Option<(DeclId, usize)>,
    written: [bool; 2],
) -> [Ambient; 2] {
    let channels = match ambient {
        Some((decl, sig)) => channels(tables, decl, sig),
        None => [Ambient::Value; 2],
    };
    [0, 1].map(|index| match written[index] {
        true => Ambient::Written,
        false => channels[index],
    })
}

/// Whether a declaration is of a strict unit
fn is_strict(tables: &Tables<'_>, decl: DeclId) -> bool {
    tables.info(tables.decl(decl).unit).strict
}

fn complete(tables: &Tables<'_>, decl: DeclId, sig: usize) -> Sig {
    let func = function(tables, decl, sig);
    let receiver = match &tables.decl(decl).node {
        DeclNode::Methods(methods) => match method_scope(&methods[sig]) {
            Scope::Static => None,
            scope => Some(scope),
        },
        _ => None,
    };
    let omitted = match is_strict(tables, decl) {
        false => Slot::Unknown,
        true => Slot::Nil,
    };
    let params = params(tables, func, receiver);
    let [input, output] = channels(tables, decl, sig).map(|ambient| match ambient {
        Ambient::Of(..) => Ambient::Written,
        ambient => ambient,
    });
    Sig {
        params,
        receiver,
        input,
        output,
        ret: func.ret.map_or(omitted, Slot::Annot),
    }
}

/// Diagnose the parameters a strict unit's def or method signature leaves
/// unannotated. A method's receiver is typed by its class, and so is exempt.
fn missing_params(tables: &Tables<'_>, decl: DeclId, sig: usize, diags: &mut Vec<UnitDiag>) {
    if !is_strict(tables, decl) {
        return;
    }
    let unit = tables.decl(decl).unit;
    let (name, receiver) = match &tables.decl(decl).node {
        DeclNode::Defs(defs) => (defs[sig].name, false),
        DeclNode::Methods(methods) => (
            methods[sig].name,
            method_scope(&methods[sig]) != Scope::Static,
        ),
        _ => unreachable!("only a def or method has a signature"),
    };
    let func = function(tables, decl, sig);
    for (index, param) in func.params.iter().enumerate() {
        if param.annot.is_some() || (receiver && index == 0) {
            continue;
        }
        let what = match param.kind {
            ParamKind::Rest { .. } => "rest parameter",
            _ => "parameter",
        };
        diags.push((
            unit,
            Diag::new(MissingAnnotation {
                span: param.name.unwrap_or(name).span,
                what,
            }),
        ));
    }
}

/// Diagnose the exported variables a strict unit checked from source leaves
/// unannotated
fn missing_values(tables: &Tables<'_>, diags: &mut Vec<UnitDiag>) {
    let mut missing: Vec<UnitSpan> = tables
        .shards
        .iter()
        .enumerate()
        .filter(|(_, shard)| shard.info.strict && shard.info.source.is_some())
        .flat_map(|(index, shard)| {
            let unit = UnitId::from_index(index);
            shard
                .values
                .iter()
                .filter(|value| value.annot.is_none())
                .map(move |value| UnitSpan {
                    unit,
                    span: value.span,
                })
        })
        .collect();
    missing.sort_by_key(|value| (value.unit, value.span.start()));
    for value in missing {
        diags.push((
            value.unit,
            Diag::new(MissingAnnotation {
                span: value.span,
                what: "exported variable",
            }),
        ));
    }
}

/// The parameters of a def, method or closure, each with the type it has, or the
/// default for an omitted annotation. The receiver of a method of scope `receiver`
/// is its class, or for a class method `Type` of it, unless annotated otherwise.
pub(crate) fn params(
    tables: &Tables<'_>,
    func: &Signature,
    receiver: Option<Scope>,
) -> Vec<ParamTy> {
    let own = match receiver {
        Some(Scope::Class) => Slot::ObjectType,
        _ => Slot::SelfType,
    };
    func.params
        .iter()
        .enumerate()
        .map(|(index, param)| match param.kind {
            ParamKind::Pos if index == 0 && receiver.is_some() => {
                ParamTy::Single(param.annot.map_or(own, Slot::Annot))
            }
            ParamKind::Pos | ParamKind::Key { .. } | ParamKind::ConstKey { .. } => {
                ParamTy::Single(param.annot.map_or(Slot::Unknown, Slot::Annot))
            }
            ParamKind::Rest { kind, pattern } => ParamTy::Rest(match (param.annot, pattern) {
                (None, _) => RestSlot::Items(kind, Slot::Unknown),
                (Some(annot), true) => RestSlot::Pattern(annot),
                (Some(annot), false) => match tables.kind_of(tables.site_ty(annot)) {
                    Some(Kind::Schema) => RestSlot::Pack(annot),
                    _ => RestSlot::Items(kind, Slot::Annot(annot)),
                },
            }),
        })
        .collect()
}

/// The scope of a method
pub(crate) fn method_scope(method: &Method) -> Scope {
    match method.scope {
        MemberScope::Instance => Scope::Instance,
        MemberScope::Class => Scope::Class,
        MemberScope::Static => Scope::Static,
    }
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

/// The form of a method, from its decorators besides `class` and `static`, whose
/// meaning is fixed by syntax. Any other decorator may replace the method with any
/// value; until decorator applications are evaluated, std's `getter` and `setter`
/// are recognized by what their names resolve to, and anything else leaves a value
/// of unknown type.
pub(crate) fn method_form(tables: &Tables<'_>, method: &Method) -> Form {
    let mut form = Form::Plain;
    for decorator in &method.decorators {
        let designated = decorator_designation(tables, decorator);
        form = match (form, designated) {
            (Form::Plain, Some(Designated::Getter)) => Form::Getter,
            (Form::Plain, Some(Designated::Setter)) => Form::Setter,
            _ => return Form::Unknown,
        };
    }
    form
}

/// The designation of the declaration a decorator names, if any
fn decorator_designation(tables: &Tables<'_>, decorator: &Decorator) -> Option<Designated> {
    let Decorator::Ident(name) = decorator else {
        return None;
    };
    match tables.linked(name.link) {
        Some(Referent::Decl(decl)) => tables.designated.get(decl).copied(),
        _ => None,
    }
}

/// Whether a class's decorators are all std's `final` and `mixable`, each of
/// which returns the class it is given after marking it, and `mark` is one of
/// them. Without decorators, a class has no mark.
pub(crate) fn decorated(tables: &Tables<'_>, class: &Class, mark: Designated) -> bool {
    let designations = class
        .decorators
        .iter()
        .map(|decorator| decorator_designation(tables, decorator))
        .collect::<Vec<_>>();
    designations.contains(&Some(mark))
        && designations
            .iter()
            .all(|designation| matches!(designation, Some(Designated::Final | Designated::Mixable)))
}

/// Designate a top-level declaration of `std` or `strand` that the checker treats
/// specially. A declaration of another module with the same name is only a
/// lookalike.
fn designate(tables: &mut Tables<'_>, decl: DeclId, diags: &mut Vec<UnitDiag>) {
    let Some((designated, expected)) = designation(tables, decl) else {
        return;
    };
    let owner = tables.decl(decl);
    let name = owner.name.expect("a designated declaration is named");
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

/// The declarations of `std` behind each [`Intrinsic`], which the solver can't
/// do without
const INTRINSICS: [&str; 20] = [
    "Value",
    "Union",
    "Keys",
    "Values",
    "Entries",
    "IndexItem",
    "AssignItem",
    "InsertItem",
    "Deletable",
    "Clearable",
    "Fields",
    "MutFields",
    "Tuple",
    "Func",
    "Int",
    "Bool",
    "Sym",
    "Nil",
    "Str",
    "Type",
];

/// Report once each intrinsic a `std` checked from source doesn't declare, so
/// the checks that need it can go unreported. One declared as the wrong kind is
/// already reported by [`designate`]. Without `std` checked from source, nothing
/// is reported: a typelib's was reported when it was checked, and with no `std`
/// at all, everything it would declare is dynamic.
fn missing_intrinsics(tables: &Tables<'_>, diags: &mut Vec<UnitDiag>) {
    for (index, shard) in tables.shards.iter().enumerate() {
        if shard.info.module != Some("std") || shard.info.source.is_none() {
            continue;
        }
        let unit = UnitId::from_index(index);
        let declared: Vec<&str> = shard
            .decls
            .iter()
            .filter(|decl| decl.outer.is_none())
            .filter_map(|decl| decl.name.map(|name| tables.name(unit, name)))
            .collect();
        for name in INTRINSICS {
            if !declared.contains(&name) {
                diags.push((
                    unit,
                    Diag::new(MissingIntrinsic {
                        span: Span::new(0, 0, 0),
                        name,
                    }),
                ));
            }
        }
    }
}

/// What a top-level declaration of `std` or `strand` is designated as by its
/// name, and the kind of declaration it must be, before [`signatures`] records
/// it
pub(super) fn designation(tables: &Tables<'_>, decl: DeclId) -> Option<(Designated, DeclKind)> {
    let owner = tables.decl(decl);
    let module = tables.info(owner.unit).module?;
    let name = owner.name.filter(|_| owner.outer.is_none())?;
    Some(match (module, tables.name(owner.unit, name)) {
        ("strand", "PipeSender") => (Designated::PipeSender, DeclKind::OpaqueAlias),
        ("strand", "PipeReceiver") => (Designated::PipeReceiver, DeclKind::OpaqueAlias),
        ("std", "Value") => (Designated::Value, DeclKind::Class),
        ("std", "Never") => (Designated::Never, DeclKind::Alias),
        ("std", "Unknown") => (Designated::Unknown, DeclKind::OpaqueAlias),
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
        ("std", "InsertItem") => (
            Designated::Intrinsic(Intrinsic::InsertItem),
            DeclKind::OpaqueAlias,
        ),
        ("std", "Deletable") => (
            Designated::Intrinsic(Intrinsic::Deletable),
            DeclKind::OpaqueAlias,
        ),
        ("std", "Clearable") => (
            Designated::Intrinsic(Intrinsic::Clearable),
            DeclKind::OpaqueAlias,
        ),
        ("std", "Fields") => (
            Designated::Intrinsic(Intrinsic::Fields),
            DeclKind::OpaqueAlias,
        ),
        ("std", "MutFields") => (
            Designated::Intrinsic(Intrinsic::MutFields),
            DeclKind::OpaqueAlias,
        ),
        ("std", "Func") => (Designated::Intrinsic(Intrinsic::Func), DeclKind::Class),
        ("std", "Int") => (Designated::Intrinsic(Intrinsic::Int), DeclKind::Class),
        ("std", "Bool") => (Designated::Intrinsic(Intrinsic::Bool), DeclKind::Class),
        ("std", "Sym") => (Designated::Intrinsic(Intrinsic::Sym), DeclKind::Class),
        ("std", "Nil") => (Designated::Intrinsic(Intrinsic::Nil), DeclKind::Class),
        ("std", "Str") => (Designated::Intrinsic(Intrinsic::Str), DeclKind::Class),
        ("std", "Type") => (Designated::Intrinsic(Intrinsic::Type), DeclKind::Class),
        ("std", "Fmt") => (Designated::Fmt, DeclKind::Class),
        ("std", "FmtValue") => (Designated::FmtValue, DeclKind::Class),
        ("std", "FmtParam") => (Designated::FmtParam, DeclKind::Class),
        ("std", "Float") => (Designated::Float, DeclKind::Class),
        ("std", "Bin") => (Designated::Bin, DeclKind::Class),
        ("std", "Array") => (Designated::Array, DeclKind::Class),
        ("std", "Dict") => (Designated::Dict, DeclKind::Class),
        ("std", "Tuple") => (Designated::Intrinsic(Intrinsic::Tuple), DeclKind::Class),
        ("std", "Record") => (Designated::Record, DeclKind::Class),
        ("std", "Range") => (Designated::Range, DeclKind::Class),
        ("std", "Spread") => (Designated::Spread, DeclKind::Protocol),
        ("std", "Unpack") => (Designated::Unpack, DeclKind::Protocol),
        ("std", "getter") => (Designated::Getter, DeclKind::Function),
        ("std", "setter") => (Designated::Setter, DeclKind::Function),
        ("std", "final") => (Designated::Final, DeclKind::Function),
        ("std", "mixable") => (Designated::Mixable, DeclKind::Function),
        _ => return None,
    })
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
    let owner = tables.decl(placeholder);
    let target = tables.decl(nominee);
    let span = owner
        .name_span()
        .expect("a designated declaration is named");
    let described = match (tables.info(target.unit).module, target.name) {
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
                && tables
                    .binder_kind(BinderRef {
                        decl: nominee,
                        sig: 0,
                        slot,
                    })
                    .expect("every binder has a kind")
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
