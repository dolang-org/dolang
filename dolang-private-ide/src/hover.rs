//! What hover shows at a span of a unit, as data for the caller to render.
use dolang_compile::{
    Kind, NodeId, Unit, UnitId,
    diag::Span,
    typeck::{self, Role, Style},
};

use crate::{
    lookup::{self, Target},
    signature::{Signature, class_header, signature},
    unit::{self, Reference, normalize_doc, overloads, reference_at},
};

/// What a check concluded about a unit, and how to find the others
pub struct Checked<'a, R> {
    pub analysis: &'a typeck::Analysis,
    /// The unit in the check
    pub unit: UnitId,
    /// The unit's module name in the check
    pub name: &'a str,
    /// Finds a module's unit by name, as [`lookup::item`] takes it
    pub resolve: R,
}

/// What hover shows at a span
#[derive(Debug)]
pub struct Hover<D> {
    /// The reference there, or else the checked node's span
    pub span: Span,
    /// What the check concluded there, empty before a check
    pub types: Vec<TypeLine>,
    /// How the caller described the declarations named there
    pub declarations: Vec<D>,
    /// Whether every declaration the check found was described
    pub complete: bool,
}

/// What a line of what the check concluded is about
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize),
    serde(rename_all = "lowercase")
)]
pub enum LineRole {
    /// A variable read or bound
    Variable,
    /// A call's signature
    Callee,
    /// Any other expression
    Value,
}

/// A line of what the check concluded
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct TypeLine {
    pub role: LineRole,
    /// The variable, the member or item as `m.C.member` or `m.item`, or the
    /// reference whose value it is
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub name: Option<String>,
    /// The type, parenthesized where an annotation needs it to be
    #[cfg_attr(feature = "serde", serde(rename = "type"))]
    pub ty: String,
    /// The overload a call chose, from 0
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub overload: Option<u32>,
}

/// What a declaration's signature says
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize),
    serde(rename_all = "lowercase")
)]
pub enum Shape {
    /// A function, method, class or annotated field, whose signature says
    /// what its checked type would
    Signature,
    /// A variable, parameter or unannotated field, whose checked type says
    /// all its signature does
    Value,
    Other,
}

/// A declaration hover describes
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize),
    serde(tag = "kind", rename_all = "lowercase")
)]
pub enum Declaration {
    /// One in a unit
    Local {
        shape: Shape,
        /// The header of the class it's a member of, as `class C[T]`
        #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
        class: Option<String>,
        /// The module it's in, when that isn't the one hovered
        #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
        module: Option<String>,
        /// Its doc comment, as Markdown
        doc: String,
        signature: Signature,
    },
    /// A module with a unit, and the comment opening its source
    Module { name: String, doc: String },
    /// An item of a module with no unit, `item` empty for the module itself
    /// and dotted as `Class.member` for a class member
    Bundled {
        module: String,
        item: String,
        /// The overload a call chose, from 0, among the item's
        #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
        overload: Option<u32>,
    },
}

/// The innermost node the check judged containing `at`
pub fn node_at<'a, R>(
    unit: &Unit<'_>,
    checked: &Checked<'a, R>,
    at: Span,
) -> Option<typeck::Node<'a>> {
    let (start, end) = (at.start(), unit.end(at));
    let node = checked.analysis.at(checked.unit, start)?;
    node.ancestors()
        .find(|node| node.span().start() <= start && end <= node.end())
}

/// What hover shows of `unit` at `at`: a position when it's empty, else the
/// innermost node containing it. `refs` are the unit's
/// [`references`](crate::references), and `render` describes a declaration
/// as the caller shows it, which is nothing for one it has no docs for.
pub fn hover<'u, M, R, D>(
    unit: &Unit<'_>,
    refs: &[Reference],
    checked: Option<&Checked<'_, R>>,
    at: Span,
    mut render: impl FnMut(Declaration) -> Vec<D>,
) -> Option<Hover<D>>
where
    R: Fn(&str) -> Option<(&'u Unit<'u>, M)>,
{
    let reference = reference_at(unit, refs, at);
    let same = |a: Span, b: Span| a.start() == b.start() && unit.end(a) == unit.end(b);
    // A module has no node of its own, so the check's is the path through it,
    // which says nothing about the module
    let module = reference.is_some_and(|reference| {
        let kind = unit.node(reference.id).map(|node| node.kind());
        matches!(
            kind,
            Some(Kind::ImportModule { .. } | Kind::PreludeModule { .. })
        )
    });
    let node = checked.and_then(|checked| node_at(unit, checked, at));
    // An item a path through a module names as a type has no token referring
    // to it, so the check's judgment is what names it
    let type_name = node
        .filter(|node| {
            reference.is_none()
                && node
                    .judgments()
                    .any(|judged| judged.role() == Role::Reference)
        })
        .map(|node| node.span());
    let types = node.filter(|_| !module).and_then(|node| {
        let span = node.span();
        let named = reference
            .filter(|reference| same(reference.span, span))
            .map(|_| unit.text(span));
        Some((span, type_lines(node, named)?))
    });
    let (targets, chosen) = match checked {
        Some(checked) => targets(unit, checked, node, reference.map(|reference| reference.id)),
        None => (Vec::new(), None),
    };
    // What a check found has its own docs, each union alternative's in turn;
    // any other reference's are its declaration's
    let own = checked.map(|checked| checked.name);
    let found: Vec<Vec<D>> = targets
        .iter()
        .map(|target| {
            let described = describe(target, chosen, own);
            described.into_iter().flat_map(&mut render).collect()
        })
        .collect();
    let (declarations, complete): (Vec<D>, _) = match found.iter().any(|docs| !docs.is_empty()) {
        true => {
            let complete = found.iter().all(|docs| !docs.is_empty());
            (found.into_iter().flatten().collect(), complete)
        }
        false => {
            let declared = reference.map(|reference| declared(unit, reference.id));
            let declared = declared.unwrap_or_default();
            (declared.into_iter().flat_map(&mut render).collect(), true)
        }
    };
    let reference = reference.map(|reference| reference.span).or(type_name);
    let (span, types) = match types {
        None if declarations.is_empty() => return None,
        None => (reference?, Vec::new()),
        Some((span, lines)) => (reference.unwrap_or(span), lines),
    };
    Some(Hover {
        span,
        types,
        declarations,
        complete,
    })
}

/// What the check concluded at `node`, a line each, from the first of:
///
/// - the variable it reads or binds;
/// - the signatures a call reached, by its member's name for a method;
/// - the type of the member a member use reached, by its name in its class;
/// - the type of the item a path through a module names, by its module;
/// - its value, by `named` where given.
fn type_lines(node: typeck::Node<'_>, named: Option<&str>) -> Option<Vec<TypeLine>> {
    let find = |role| node.judgments().find(|judged| judged.role() == role);
    let variable = node
        .judgments()
        .find(|judged| matches!(judged.role(), Role::Read | Role::Bind));
    if let Some(judged) = variable {
        return Some(vec![TypeLine {
            role: LineRole::Variable,
            name: Some(judged.name()?.to_owned()),
            ty: judged.annotation(Style::Reader),
            overload: None,
        }]);
    }
    // A member by the class declaring it, as `m.Box.item`; a union receiver's
    // reach one in each alternative's class
    let members: Vec<_> = node
        .judgments()
        .filter(|judged| judged.role() == Role::Member)
        .filter_map(|judged| {
            let name = judged.name()?;
            Some(match judged.declaration() {
                Some(class) => format!("{}.{}.{name}", class.module(), class.name()),
                None => name.to_owned(),
            })
        })
        .collect();
    // An item by the module exporting it, as `m.f`
    let item = find(Role::Item)
        .and_then(|judged| judged.declaration())
        .map(|item| format!("{}.{}", item.module(), item.name()));
    let member = (!members.is_empty()).then(|| members.join(" | ")).or(item);
    let callees: Vec<_> = node
        .judgments()
        .filter(|judged| judged.role() == Role::Callee)
        .map(|judged| TypeLine {
            role: LineRole::Callee,
            name: member.clone(),
            ty: judged.annotation(Style::Reader),
            overload: judged.overload(),
        })
        .collect();
    if !callees.is_empty() {
        return Some(callees);
    }
    let value = find(Role::Value)?;
    Some(vec![TypeLine {
        role: LineRole::Value,
        name: member.or(named.map(str::to_owned)),
        ty: value.annotation(Style::Reader),
        overload: None,
    }])
}

/// The declarations what's at `node` stands for, and the overload a call of
/// it chose: a module, as its root; an item imported from a module, as the
/// reference `reference` names it; a function declared here, when a call
/// chose one of its overloads; the members a member use reached, one for
/// each class of a union receiver; or the item a path through a module or a
/// type names.
pub fn targets<'u, M, R>(
    unit: &Unit<'_>,
    checked: &Checked<'_, R>,
    node: Option<typeck::Node<'_>>,
    reference: Option<NodeId>,
) -> (Vec<Target<'u, M>>, Option<u32>)
where
    R: Fn(&str) -> Option<(&'u Unit<'u>, M)>,
{
    let resolve = &checked.resolve;
    let chosen = node.and_then(unit::chosen);
    let found = match reference {
        Some(id) => {
            let root = |module: &str| {
                let (unit, found) = resolve(module)?;
                Some(Target::Source {
                    unit,
                    module: found,
                    name: module.to_owned(),
                    id: unit.root()?,
                })
            };
            let target = match unit.node(id).map(|node| node.kind()) {
                Some(Kind::ImportModule { module, .. }) => root(unit.text(module)),
                Some(Kind::PreludeModule { module, .. }) => root(module),
                Some(Kind::ImportItem { module, item, .. }) => {
                    lookup::item(resolve, unit.text(module), unit.text(item))
                }
                Some(Kind::PreludeItem { module, item, .. }) => lookup::item(resolve, module, item),
                Some(Kind::Function { .. }) if chosen.is_some() => {
                    resolve(checked.name).map(|(unit, found)| Target::Source {
                        unit,
                        module: found,
                        name: checked.name.to_owned(),
                        id,
                    })
                }
                _ => None,
            };
            target.into_iter().collect()
        }
        None => node
            .map(|node| node_targets(resolve, node))
            .unwrap_or_default(),
    };
    // A union receiver's calls each chose their own
    let chosen = chosen.filter(|_| found.len() == 1);
    (found, chosen)
}

/// What the analysis `node` names: the members a member use reached, or else
/// the item a path through a module names, or the declaration a type names
pub fn node_targets<'u, M>(
    resolve: impl Fn(&str) -> Option<(&'u Unit<'u>, M)>,
    node: typeck::Node<'_>,
) -> Vec<Target<'u, M>> {
    let members: Vec<_> = node
        .judgments()
        .filter(|judged| judged.role() == Role::Member)
        .filter_map(|judged| {
            let class = judged.declaration()?;
            lookup::member(&resolve, class.module(), class.name(), judged.name()?)
        })
        .collect();
    if !members.is_empty() {
        return members;
    }
    let item = node
        .judgments()
        .find(|judged| matches!(judged.role(), Role::Item | Role::Reference));
    let item = item.and_then(|judged| judged.declaration());
    item.and_then(|item| lookup::item(&resolve, item.module(), item.name()))
        .into_iter()
        .collect()
}

/// How hover describes a target, or the overload `chosen` of it. A
/// declaration in a module other than `own` is noted with its module.
pub fn describe<M>(
    target: &Target<'_, M>,
    chosen: Option<u32>,
    own: Option<&str>,
) -> Vec<Declaration> {
    match *target {
        Target::Source {
            unit, ref name, id, ..
        } => {
            // A module is documented by the comment opening its source
            if unit.root() == Some(id) {
                let doc = unit.node(id).and_then(|node| node.doc());
                return vec![Declaration::Module {
                    name: name.clone(),
                    doc: doc
                        .map(|span| normalize_doc(unit.text(span)))
                        .unwrap_or_default(),
                }];
            }
            let module = Some(&name[..]).filter(|&name| Some(name) != own);
            overloaded(unit, id, chosen, module)
        }
        Target::Bundled {
            ref module,
            ref item,
        } => vec![Declaration::Bundled {
            module: module.clone(),
            item: item.clone(),
            overload: chosen,
        }],
    }
}

/// How hover describes a declaration in a unit, noted as in `module` when
/// given, or for an overloaded function or method, the overload `chosen` of
/// it, or else each overload in turn: the overloads say all the checker does
/// of its interface. An overload with no doc comment of its own has its
/// implementation's.
pub fn overloaded(
    unit: &Unit<'_>,
    implementation: NodeId,
    chosen: Option<u32>,
    module: Option<&str>,
) -> Vec<Declaration> {
    let ids = match chosen {
        Some(index) => vec![unit::overload(unit, implementation, index)],
        None => {
            let overloads = overloads(unit, implementation);
            match overloads.is_empty() {
                true => vec![implementation],
                false => overloads,
            }
        }
    };
    let span = unit.node(implementation).and_then(|node| node.doc());
    ids.into_iter()
        .filter_map(|id| {
            let mut described = local(unit, id, module)?;
            if let Declaration::Local { doc, .. } = &mut described
                && doc.is_empty()
                && let Some(span) = span
            {
                *doc = normalize_doc(unit.text(span));
            }
            Some(described)
        })
        .collect()
}

/// How hover describes the declaration `id` a reference names, without a
/// check: an import or prelude binding names a bundled item, and anything
/// else is described from the source.
pub fn declared(unit: &Unit<'_>, id: NodeId) -> Vec<Declaration> {
    let bundled = |module: &str, item: &str| Declaration::Bundled {
        module: module.to_owned(),
        item: item.to_owned(),
        overload: None,
    };
    match unit.node(id).map(|node| node.kind()) {
        Some(Kind::ImportModule { module, .. }) => vec![bundled(unit.text(module), "")],
        Some(Kind::ImportItem { module, item, .. }) => {
            vec![bundled(unit.text(module), unit.text(item))]
        }
        Some(Kind::PreludeModule { module, .. }) => vec![bundled(module, "")],
        Some(Kind::PreludeItem { module, item, .. }) => vec![bundled(module, item)],
        _ => overloaded(unit, id, None, None),
    }
}

/// A declaration in a unit, noted as in `module` when given: its signature,
/// the class it's in, and its doc comment
fn local(unit: &Unit<'_>, id: NodeId, module: Option<&str>) -> Option<Declaration> {
    let signature = signature(unit, id)?;
    let node = unit.node(id)?;
    let shape = match (node.kind(), &signature) {
        (
            Kind::Function { .. }
            | Kind::Method { .. }
            | Kind::SpecialMethod { .. }
            | Kind::Class { .. },
            _,
        )
        | (Kind::Field { .. }, Signature::Field { ty: Some(_), .. }) => Shape::Signature,
        (
            Kind::Bind { .. }
            | Kind::PositionalParam { .. }
            | Kind::KeyParam { .. }
            | Kind::RestParam { .. }
            | Kind::SelfParam { .. }
            | Kind::Field { .. },
            _,
        ) => Shape::Value,
        _ => Shape::Other,
    };
    Some(Declaration::Local {
        shape,
        class: class_header(unit, id),
        module: module.map(str::to_owned),
        doc: node
            .doc()
            .map(|span| normalize_doc(unit.text(span)))
            .unwrap_or_default(),
        signature,
    })
}
