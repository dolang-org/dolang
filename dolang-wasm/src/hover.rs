//! What hover shows at a position, as data for the page to render.
//!
//! An adaptation of the LSP's hover (`dolang-lsp/src/backend.rs` and
//! `lookup.rs`) to the playground's one unit: every other module is a bundled
//! typelib, so a declaration found in one is left as a key into the doc index
//! the page loads. Signatures are returned in parts, since how one is laid out
//! is up to the page.
use std::ops::Range;

use dolang::compile::{
    Kind, NodeId, Span, Token, Unit, UnitId,
    typeck::{self, Role, Style},
};
use dolang_util::utf16::Offsets;
use serde::Serialize;

/// The most re-exports a lookup follows, which ends a cycle of them
const MAX_HOPS: usize = 16;

/// What a check concluded about the unit
pub(crate) struct Checked {
    pub(crate) unit: UnitId,
    /// The unit's module name in the check
    pub(crate) name: String,
    pub(crate) analysis: typeck::Analysis,
}

/// A token naming a declaration, by byte offsets
pub(crate) struct Reference {
    start: usize,
    end: usize,
    id: NodeId,
}

/// What hover shows at a position. Offsets are UTF-16.
#[derive(Debug, Serialize)]
pub(crate) struct Hover {
    /// The reference under the position, or else the checked node's span
    pub from: usize,
    pub to: usize,
    /// What the check concluded there, empty before a check
    pub types: Vec<TypeLine>,
    /// The declarations named there
    pub declarations: Vec<Declaration>,
    /// Whether every declaration in the source found by the check was
    /// described. A bundled key the page has no docs for isn't either.
    pub complete: bool,
}

/// A line of what the check concluded
#[derive(Debug, Serialize)]
pub(crate) struct TypeLine {
    /// `variable` for a variable read or bound, `callee` for a call's
    /// signature, or `value` for any other expression
    pub role: &'static str,
    /// The variable, the member or item as `m.C.member` or `m.item`, or the
    /// reference whose value it is
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The type, parenthesized where an annotation needs it to be
    #[serde(rename = "type")]
    pub ty: String,
    /// The overload a call chose, from 0
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overload: Option<u32>,
}

/// A declaration hover describes
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub(crate) enum Declaration {
    /// One in the source
    Local {
        /// `signature` for a function, method, class or annotated field,
        /// whose signature says what its checked type would; `value` for a
        /// variable, parameter or unannotated field, whose checked type says
        /// all its signature does; else `other`
        shape: &'static str,
        /// The header of the class it's a member of, as `class C[T]`
        #[serde(skip_serializing_if = "Option::is_none")]
        class: Option<String>,
        /// Its doc comment, as Markdown
        doc: String,
        signature: Signature,
    },
    /// An item of a bundled module, `item` empty for the module itself and
    /// dotted as `Class.member` for a class member
    Bundled {
        module: String,
        item: String,
        /// The overload a call chose, from 0, among the item's
        #[serde(skip_serializing_if = "Option::is_none")]
        overload: Option<u32>,
    },
}

/// A declaration's signature, in parts. Types are as written.
#[derive(Debug, Serialize)]
#[serde(tag = "form", rename_all = "lowercase")]
pub(crate) enum Signature {
    Class {
        #[serde(rename = "pub")]
        is_pub: bool,
        /// A protocol
        #[serde(rename = "typeOnly")]
        type_only: bool,
        name: String,
        binders: Vec<String>,
        /// `final` and `mixable`
        decorators: Vec<String>,
        supers: Vec<String>,
    },
    Def {
        #[serde(rename = "pub")]
        is_pub: bool,
        /// An `@def` overload
        #[serde(rename = "typeOnly")]
        type_only: bool,
        name: String,
        binders: Vec<String>,
        /// A method's `getter` or `setter`
        decorators: Vec<String>,
        params: Vec<Param>,
        /// Whether it has a body, which a type-only declaration doesn't
        body: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        returns: Option<String>,
    },
    Field {
        #[serde(rename = "pub")]
        is_pub: bool,
        name: String,
        #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
        ty: Option<String>,
    },
    Let {
        #[serde(rename = "pub")]
        is_pub: bool,
        name: String,
        #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
        ty: Option<String>,
    },
    Param {
        param: Param,
    },
}

/// A parameter of a signature
#[derive(Debug, Serialize)]
pub(crate) struct Param {
    /// The parameter as written up to its annotation, sigils included, such
    /// as `:host`, `foo: local` or `...args`
    pub head: String,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub ty: Option<String>,
    /// Whether it has a default
    pub default: bool,
}

/// The tokens of `unit` naming a declaration, sorted by start. A prelude
/// binding has no declaration in the source, but names a bundled item.
pub(crate) fn references(unit: &Unit<'_>) -> Vec<Reference> {
    let mut refs = Vec::new();
    unit.tokens(&mut |token, span: Span, node: Option<NodeId>, _| {
        let Range { start, end } = unit.byte_range(span);
        let punctuation = matches!(token, Token::Delim | Token::Sigil);
        let Some(id) = node.filter(|_| start != end && !punctuation) else {
            return;
        };
        let Some(node) = unit.node(id) else {
            return;
        };
        let named = node.definition().is_some()
            || matches!(
                node.kind(),
                Kind::PreludeModule { .. } | Kind::PreludeItem { .. }
            );
        if named {
            refs.push(Reference { start, end, id });
        }
    });
    refs.sort_by_key(|reference| reference.start);
    refs
}

/// What hover shows of `unit` at the UTF-16 offsets `from` to `to`: a
/// position when they're equal, else the innermost node containing the range.
pub(crate) fn hover(
    unit: &Unit<'_>,
    offsets: &Offsets,
    refs: &[Reference],
    checked: Option<&Checked>,
    from: usize,
    to: usize,
) -> Option<Hover> {
    let (start, end) = (offsets.byte(from), offsets.byte(to));
    if end < start {
        return None;
    }
    let source = unit.source();
    // The token under the position naming a declaration, or for a range, the
    // one it covers exactly
    let reference = refs
        .get(
            refs.partition_point(|reference| reference.start <= start)
                .checked_sub(1)?,
        )
        .filter(|reference| match start == end {
            true => start < reference.end,
            false => (reference.start, reference.end) == (start, end),
        });
    // A module has no node of its own, so the check's is the path through it,
    // which says nothing about the module
    let module = reference.is_some_and(|reference| {
        let kind = unit.node(reference.id).map(|node| node.kind());
        matches!(
            kind,
            Some(Kind::ImportModule { .. } | Kind::PreludeModule { .. })
        )
    });
    let node = checked.and_then(|checked| {
        let start = u32::try_from(start).ok()?;
        let end = u32::try_from(end).ok()?;
        let node = checked.analysis.at(checked.unit, start)?;
        node.ancestors()
            .find(|node| node.span().start <= start && end <= node.span().end)
    });
    let types = node.filter(|_| !module).and_then(|node| {
        let span = node.span();
        let spans = reference.map(|reference| (reference.start, reference.end));
        let named = (spans == Some((span.start as usize, span.end as usize)))
            .then(|| source.get(span.start as usize..span.end as usize))
            .flatten();
        Some((span, type_lines(node, named)?))
    });
    let (targets, chosen) = match checked {
        Some(checked) => targets(unit, checked, node, reference.map(|reference| reference.id)),
        None => (Vec::new(), None),
    };
    // What a check found has its own docs, each union alternative's in turn;
    // any other reference's are its declaration's
    let found: Vec<_> = targets
        .iter()
        .map(|target| describe(unit, target, chosen))
        .collect();
    let (declarations, complete) = match found.iter().any(|docs| !docs.is_empty()) {
        true => {
            let complete = found.iter().all(|docs| !docs.is_empty());
            (found.into_iter().flatten().collect(), complete)
        }
        false => {
            let declared = reference.map(|reference| declared(unit, reference.id));
            (declared.unwrap_or_default(), true)
        }
    };
    let reference = reference.map(|reference| (reference.start, reference.end));
    let ((start, end), types) = match types {
        None if declarations.is_empty() => return None,
        None => (reference?, Vec::new()),
        Some((span, lines)) => (
            reference.unwrap_or((span.start as usize, span.end as usize)),
            lines,
        ),
    };
    Some(Hover {
        from: offsets.utf16(start),
        to: offsets.utf16(end),
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
            role: "variable",
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
            role: "callee",
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
        role: "value",
        name: member.or(named.map(str::to_owned)),
        ty: value.annotation(Style::Reader),
        overload: None,
    }])
}

/// Where a lookup ended
enum Target {
    /// A declaration in the source
    Source(NodeId),
    /// An item of a bundled module, by module and name
    Bundled(String, String),
}

/// The declarations what's at `node` stands for, and the overload a call of
/// it chose: an item imported from a bundled module, as the reference `id`
/// names it; a function declared here, when a call chose one of its
/// overloads; the members a member use reached, one for each class of a
/// union receiver; or the item a path through a module names.
fn targets(
    unit: &Unit<'_>,
    checked: &Checked,
    node: Option<typeck::Node<'_>>,
    reference: Option<NodeId>,
) -> (Vec<Target>, Option<u32>) {
    let own = &checked.name[..];
    let chosen = node.and_then(chosen);
    let found = match reference {
        Some(id) => {
            let target = match unit.node(id).map(|node| node.kind()) {
                Some(Kind::ImportItem { module, item, .. }) => {
                    item_target(unit, own, unit.text(module), unit.text(item))
                }
                Some(Kind::PreludeItem { module, item, .. }) => {
                    item_target(unit, own, module, item)
                }
                Some(Kind::Function { .. }) if chosen.is_some() => Some(Target::Source(id)),
                _ => None,
            };
            target.into_iter().collect()
        }
        None => node
            .map(|node| node_targets(unit, own, node))
            .unwrap_or_default(),
    };
    // A union receiver's calls each chose their own
    let chosen = chosen.filter(|_| found.len() == 1);
    (found, chosen)
}

/// What the analysis `node` names: the members a member use reached, or else
/// the item a path through a module names
fn node_targets(unit: &Unit<'_>, own: &str, node: typeck::Node<'_>) -> Vec<Target> {
    let members: Vec<_> = node
        .judgments()
        .filter(|judged| judged.role() == Role::Member)
        .filter_map(|judged| {
            let class = judged.declaration()?;
            member_target(unit, own, class.module(), class.name(), judged.name()?)
        })
        .collect();
    if !members.is_empty() {
        return members;
    }
    let item = node.judgments().find(|judged| judged.role() == Role::Item);
    let item = item.and_then(|judged| judged.declaration());
    item.and_then(|item| item_target(unit, own, item.module(), item.name()))
        .into_iter()
        .collect()
}

/// The overload chosen by the call of what `node` names. A method call is
/// judged at its member, and a function call at the whole call, which starts
/// where its callee does. A union receiver's call reaches several.
fn chosen(node: typeck::Node<'_>) -> Option<u32> {
    let start = node.span().start;
    let callees = node
        .ancestors()
        .take_while(|ancestor| ancestor.span().start == start)
        .map(|ancestor| {
            let callees = ancestor.judgments();
            callees
                .filter(|judged| judged.role() == Role::Callee)
                .collect::<Vec<_>>()
        })
        .find(|callees| !callees.is_empty())?;
    match callees[..] {
        [callee] => callee.overload(),
        _ => None,
    }
}

/// The item `module` exports as `name`, following re-exports. The source is
/// the module `own`; any other is bundled. An overloaded function is found as
/// its implementation.
fn item_target(unit: &Unit<'_>, own: &str, module: &str, name: &str) -> Option<Target> {
    let (mut module, mut name) = (module.to_owned(), name.to_owned());
    for _ in 0..MAX_HOPS {
        if module != own {
            return Some(Target::Bundled(module, name));
        }
        let named = |span: &_| unit.text(*span) == name;
        let mut signature = None;
        let mut reexport = None;
        for id in unit.node(unit.root()?)?.children() {
            match unit.node(id)?.kind() {
                Kind::Class {
                    name: span,
                    is_pub: true,
                    ..
                }
                | Kind::Bind {
                    name: span,
                    is_pub: true,
                }
                | Kind::Alias {
                    name: span,
                    is_pub: true,
                    ..
                }
                | Kind::Function {
                    name: span,
                    is_pub: true,
                    type_only: false,
                } if named(&span) => return Some(Target::Source(id)),
                Kind::Function {
                    name: span,
                    is_pub: true,
                    type_only: true,
                } if named(&span) => signature = Some(id),
                Kind::ImportItem {
                    module,
                    item,
                    name: span,
                    is_pub: true,
                    ..
                } if named(&span) => reexport = Some((module, item)),
                _ => {}
            }
        }
        if let Some(id) = signature {
            return Some(Target::Source(id));
        }
        let (next, item) = reexport?;
        (module, name) = (unit.text(next).to_owned(), unit.text(item).to_owned());
    }
    None
}

/// The member `name` of the class `module` declares as `class`. The source
/// is the module `own`; any other is bundled. A special method's name keeps
/// its parentheses, and an overloaded method is found as its implementation.
fn member_target(
    unit: &Unit<'_>,
    own: &str,
    module: &str,
    class: &str,
    name: &str,
) -> Option<Target> {
    if module != own {
        return Some(Target::Bundled(
            module.to_owned(),
            format!("{class}.{name}"),
        ));
    }
    let named = |id: NodeId, name: &str| {
        let node = unit.node(id);
        let span = node.and_then(|node| node.definition());
        span.is_some_and(|span| unit.text(span) == name)
    };
    let class = unit.node(unit.root()?)?.children().find(|&id| {
        let is_class = matches!(
            unit.node(id).map(|node| node.kind()),
            Some(Kind::Class { .. })
        );
        is_class && named(id, class)
    })?;
    let id = unit.node(class)?.children().find(|&id| {
        let member = matches!(
            unit.node(id).map(|node| node.kind()),
            Some(Kind::Method { .. } | Kind::SpecialMethod { .. } | Kind::Field { .. })
        );
        member && named(id, name) && !is_overload(unit, id)
    })?;
    Some(Target::Source(id))
}

/// The `index`th `@def` overload of the function or method `id`, in source
/// order, or `id` itself when it has no such overload
fn overload(unit: &Unit<'_>, id: NodeId, index: u32) -> NodeId {
    let chosen = || {
        let node = unit.node(id)?;
        let name = unit.text(node.definition()?);
        let mut overloads: Vec<_> = unit
            .node(node.parent()?)?
            .children()
            .filter(|&other| is_overload(unit, other))
            .filter_map(|other| {
                let node = unit.node(other)?;
                let named = unit.text(node.definition()?) == name;
                named.then_some((node.span()?.start(), other))
            })
            .collect();
        overloads.sort_unstable_by_key(|&(start, _)| start);
        Some(overloads.get(usize::try_from(index).ok()?)?.1)
    };
    chosen().unwrap_or(id)
}

/// Whether a declaration is an `@def` overload signature
fn is_overload(unit: &Unit<'_>, id: NodeId) -> bool {
    let Some(node) = unit.node(id) else {
        return false;
    };
    match node.kind() {
        Kind::Function { type_only, .. } => type_only,
        // A protocol's own member is type-only too, but is written without `@`
        Kind::Method {
            type_only: true, ..
        }
        | Kind::SpecialMethod {
            type_only: true, ..
        } => {
            let (Some(span), Some(name)) = (node.span(), node.definition()) else {
                return false;
            };
            let start = unit.byte_range(span).start;
            let before = unit.source().get(start..unit.byte_range(name).start);
            let def = before.and_then(|before| before.trim_end().strip_suffix("def"));
            def.is_some_and(|def| def.trim_end().ends_with('@'))
        }
        _ => false,
    }
}

/// How hover describes a target, or the overload `chosen` of it. An overload
/// with no doc comment of its own has its implementation's.
fn describe(unit: &Unit<'_>, target: &Target, chosen: Option<u32>) -> Vec<Declaration> {
    match *target {
        Target::Source(implementation) => {
            let id = match chosen {
                Some(index) => overload(unit, implementation, index),
                None => implementation,
            };
            let Some(mut described) = local(unit, id) else {
                return Vec::new();
            };
            if let Declaration::Local { doc, .. } = &mut described
                && doc.is_empty()
                && let Some(span) = unit.node(implementation).and_then(|node| node.doc())
            {
                *doc = normalize_doc(unit, span);
            }
            vec![described]
        }
        Target::Bundled(ref module, ref item) => vec![Declaration::Bundled {
            module: module.clone(),
            item: item.clone(),
            overload: chosen,
        }],
    }
}

/// How hover describes the declaration `id` a reference names, without a
/// check: an import or prelude binding names a bundled item, and anything
/// else is described from the source.
fn declared(unit: &Unit<'_>, id: NodeId) -> Vec<Declaration> {
    let bundled = |module: &str, item: &str| Declaration::Bundled {
        module: module.to_owned(),
        item: item.to_owned(),
        overload: None,
    };
    let declaration = match unit.node(id).map(|node| node.kind()) {
        Some(Kind::ImportModule { module, .. }) => Some(bundled(unit.text(module), "")),
        Some(Kind::ImportItem { module, item, .. }) => {
            Some(bundled(unit.text(module), unit.text(item)))
        }
        Some(Kind::PreludeModule { module, .. }) => Some(bundled(module, "")),
        Some(Kind::PreludeItem { module, item, .. }) => Some(bundled(module, item)),
        _ => local(unit, id),
    };
    declaration.into_iter().collect()
}

/// A declaration in the source: its signature, the classes it's in, and its
/// doc comment
fn local(unit: &Unit<'_>, id: NodeId) -> Option<Declaration> {
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
        | (Kind::Field { .. }, Signature::Field { ty: Some(_), .. }) => "signature",
        (
            Kind::Bind { .. }
            | Kind::PositionalParam { .. }
            | Kind::KeyParam { .. }
            | Kind::RestParam { .. }
            | Kind::SelfParam { .. }
            | Kind::Field { .. },
            _,
        ) => "value",
        _ => "other",
    };
    Some(Declaration::Local {
        shape,
        class: class_header(unit, id),
        doc: node
            .doc()
            .map(|span| normalize_doc(unit, span))
            .unwrap_or_default(),
        signature,
    })
}

/// The header of the class a declaration is a member of, as `class C[T]`
fn class_header(unit: &Unit<'_>, id: NodeId) -> Option<String> {
    let parent = unit.node(unit.node(id)?.parent()?)?;
    let Kind::Class {
        name, type_only, ..
    } = parent.kind()
    else {
        return None;
    };
    let mut binders: Vec<_> = parent
        .children()
        .filter_map(|child| unit.node(child))
        .filter(|child| matches!(child.kind(), Kind::Binder { .. }))
        .filter_map(|child| child.span())
        .collect();
    binders.sort();
    let binders: Vec<_> = binders.iter().map(|span| one_line(unit, *span)).collect();
    let binders = if binders.is_empty() {
        String::new()
    } else {
        format!("[{}]", binders.join(", "))
    };
    let at = if type_only { "@" } else { "" };
    Some(format!("{at}class {}{binders}", unit.text(name)))
}

/// A doc comment without its comment markers, keeping its Markdown layout
fn normalize_doc(unit: &Unit<'_>, span: Span) -> String {
    unit.text(span)
        .split('\n')
        .map(|line| {
            let line = line.trim();
            let line = line.strip_prefix('#').unwrap_or(line);
            line.strip_prefix(' ').unwrap_or(line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Source text as one line
fn one_line(unit: &Unit<'_>, span: Span) -> String {
    unit.text(span)
        .lines()
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ")
}

/// A parameter of a signature, from its declaration
fn parameter(unit: &Unit<'_>, id: NodeId) -> Option<Param> {
    let source = unit.source();
    let node = unit.node(id)?;
    let default = match node.kind() {
        Kind::PositionalParam { default, .. } | Kind::KeyParam { default, .. } => default,
        Kind::RestParam { .. } | Kind::SelfParam { .. } => None,
        _ => return None,
    };
    let span = node.span()?;
    let ty = node
        .children()
        .filter_map(|child| unit.node(child))
        .find(|child| matches!(child.kind(), Kind::Type { .. }))
        .and_then(|child| child.span());
    // The parameter as written ends where its annotation or default starts
    let range = unit.byte_range(span);
    let end = [ty, default]
        .into_iter()
        .flatten()
        .map(|span| unit.byte_range(span).start)
        .min()
        .unwrap_or(range.end);
    let head = source.get(range.start..end)?.trim_end();
    let head = head.strip_suffix('=').unwrap_or(head).trim_end();
    let head = head.strip_suffix('@').unwrap_or(head).trim_end();
    Some(Param {
        head: head.lines().map(str::trim).collect::<Vec<_>>().join(" "),
        ty: ty.map(|span| one_line(unit, span)),
        default: default.is_some(),
    })
}

/// A declaration's signature, including any type it is annotated with
fn signature(unit: &Unit<'_>, id: NodeId) -> Option<Signature> {
    let node = unit.node(id)?;
    let kind = node.kind();
    let name = unit.text(node.definition()?).to_owned();
    // In source order; a prelude binding, having no span, sorts first
    let mut ids: Vec<_> = node.children().collect();
    ids.sort_by_key(|&id| {
        let span = unit.node(id).and_then(|child| child.span());
        span.map(|span| span.start())
    });
    let children: Vec<_> = ids.iter().filter_map(|&id| unit.node(id)).collect();
    let spans = |matches: fn(&Kind<'_>) -> bool| -> Vec<Span> {
        children
            .iter()
            .filter(|child| matches(&child.kind()))
            .filter_map(|child| child.span())
            .collect()
    };
    let binders = || {
        spans(|kind| matches!(kind, Kind::Binder { .. }))
            .iter()
            .map(|span| one_line(unit, *span))
            .collect()
    };
    let ty = || {
        spans(|kind| matches!(kind, Kind::Type { .. }))
            .first()
            .map(|span| one_line(unit, *span))
    };
    let params = || {
        ids.iter()
            .filter_map(|&child| parameter(unit, child))
            .collect::<Vec<_>>()
    };
    // Only the decorators the type checker designates say anything about the
    // declaration, and only when they are `std`'s
    let designated = |items: &[&str]| -> Vec<String> {
        children
            .iter()
            .filter_map(|child| {
                let Kind::Decorator {
                    target: Some(target),
                } = child.kind()
                else {
                    return None;
                };
                let item = match unit.node(target)?.kind() {
                    Kind::PreludeItem {
                        module: "std",
                        item,
                        ..
                    } => item,
                    Kind::ImportItem { module, item, .. } if unit.text(module) == "std" => {
                        unit.text(item)
                    }
                    _ => return None,
                };
                items.contains(&item).then(|| item.to_owned())
            })
            .collect()
    };
    let signature = match kind {
        Kind::Class {
            is_pub, type_only, ..
        } => Signature::Class {
            is_pub,
            type_only,
            name,
            binders: binders(),
            decorators: designated(&["final", "mixable"]),
            // A class's types are its superclasses
            supers: spans(|kind| matches!(kind, Kind::Type { .. }))
                .iter()
                .map(|span| unit.text(*span).to_owned())
                .collect(),
        },
        Kind::Function {
            is_pub,
            type_only: bodiless,
            ..
        }
        | Kind::Method {
            is_pub,
            type_only: bodiless,
            ..
        } => {
            // An overload is written with `@`, but a protocol's member is
            // type-only without one
            let protocol = node
                .parent()
                .and_then(|parent| unit.node(parent))
                .is_some_and(|parent| {
                    matches!(
                        parent.kind(),
                        Kind::Class {
                            type_only: true,
                            ..
                        }
                    )
                });
            let type_only = bodiless && !protocol;
            // A getter or setter is used as a field, not called
            let decorators = match kind {
                Kind::Method { .. } => designated(&["getter", "setter"]),
                _ => Vec::new(),
            };
            Signature::Def {
                is_pub: is_pub && !type_only,
                type_only,
                name,
                binders: binders(),
                decorators,
                params: params(),
                body: !bodiless,
                returns: ty(),
            }
        }
        Kind::SpecialMethod { type_only, .. } => Signature::Def {
            is_pub: false,
            type_only: false,
            name,
            binders: binders(),
            decorators: Vec::new(),
            params: params(),
            body: !type_only,
            returns: ty(),
        },
        Kind::Field { is_pub, .. } => Signature::Field {
            is_pub,
            name,
            ty: ty(),
        },
        Kind::Bind { is_pub, .. } => Signature::Let {
            is_pub,
            name,
            ty: ty(),
        },
        // A parameter is as written, but for its default
        Kind::PositionalParam { .. }
        | Kind::KeyParam { .. }
        | Kind::RestParam { .. }
        | Kind::SelfParam { .. } => Signature::Param {
            param: parameter(unit, id)?,
        },
        _ => return None,
    };
    Some(signature)
}

#[cfg(all(test, target_family = "wasm"))]
mod tests {
    use super::*;
    use crate::analysis::Analyzed;
    use wasm_bindgen_test::wasm_bindgen_test;

    /// The UTF-16 offset of the last `needle` in `source`
    fn last(source: &str, needle: &str) -> usize {
        source[..source.rfind(needle).unwrap()]
            .encode_utf16()
            .count()
    }

    fn checked(source: &str) -> Analyzed {
        let mut analyzed = Analyzed::new(source.to_owned());
        analyzed.check();
        analyzed
    }

    fn bundled(hover: &Hover) -> Vec<(&str, &str, Option<u32>)> {
        hover
            .declarations
            .iter()
            .map(|declaration| match declaration {
                Declaration::Bundled {
                    module,
                    item,
                    overload,
                } => (&module[..], &item[..], *overload),
                other => panic!("not bundled: {other:?}"),
            })
            .collect()
    }

    #[wasm_bindgen_test]
    fn variable_read_has_its_type_and_declaration() {
        // Characters before it are two and four bytes, and one and two code
        // units
        let source = "# é 😀\nlet count = 1\necho $count";
        let at = last(source, "count");
        let hover = checked(source).hover(at, at).unwrap();
        assert_eq!((hover.from, hover.to), (at, at + 5));
        let [line] = &hover.types[..] else {
            panic!("{:?}", hover.types)
        };
        assert_eq!(
            (line.role, line.name.as_deref()),
            ("variable", Some("count"))
        );
        assert_eq!(line.ty, "std.Int");
        let [
            Declaration::Local {
                shape: "value",
                signature: Signature::Let { name, .. },
                ..
            },
        ] = &hover.declarations[..]
        else {
            panic!("{:?}", hover.declarations)
        };
        assert_eq!(name, "count");
    }

    #[wasm_bindgen_test]
    fn unchecked_source_has_declarations_only() {
        let source = "let count = 1\necho $count";
        let at = last(source, "count");
        let hover = Analyzed::new(source.to_owned()).hover(at, at).unwrap();
        assert!(hover.types.is_empty());
        assert!(matches!(
            &hover.declarations[..],
            [Declaration::Local {
                signature: Signature::Let { .. },
                ..
            }]
        ));
    }

    #[wasm_bindgen_test]
    fn local_function_call_has_its_signature_and_doc() {
        // A comment opening the source documents the module instead
        let source =
            "let n = 2\n# Doubles a number.\ndef double x@Int -> Int\n  (x * 2)\ndouble $n";
        let at = last(source, "double");
        let hover = checked(source).hover(at, at).unwrap();
        // The callee's name is a read of the function
        let [line] = &hover.types[..] else {
            panic!("{:?}", hover.types)
        };
        // Parenthesized, as an annotation needs it to be
        assert_eq!(line.ty, "((std.Int) -> std.Int)");
        let [
            Declaration::Local {
                shape: "signature",
                doc,
                signature:
                    Signature::Def {
                        name,
                        params,
                        returns,
                        ..
                    },
                ..
            },
        ] = &hover.declarations[..]
        else {
            panic!("{:?}", hover.declarations)
        };
        assert_eq!(doc, "Doubles a number.");
        assert_eq!(name, "double");
        let [Param { head, ty, default }] = &params[..] else {
            panic!("{params:?}")
        };
        assert_eq!(
            (&head[..], ty.as_deref(), *default),
            ("x", Some("Int"), false)
        );
        assert_eq!(returns.as_deref(), Some("Int"));
    }

    #[wasm_bindgen_test]
    fn prelude_call_names_a_bundled_item() {
        let hover = checked("echo hi").hover(1, 1).unwrap();
        assert_eq!(bundled(&hover), [("term", "echo", None)]);
        assert_eq!((hover.from, hover.to), (0, 4));
    }

    #[wasm_bindgen_test]
    fn overloaded_call_names_the_chosen_overload() {
        let source = "int \"ff\" base: 16";
        let hover = checked(source).hover(0, 0).unwrap();
        assert_eq!(bundled(&hover), [("std", "int", Some(1))]);
    }

    #[wasm_bindgen_test]
    fn bundled_method_is_dotted_by_its_class() {
        let source = "let s = \"a\"\necho $s.upper()";
        let at = last(source, "upper");
        let hover = checked(source).hover(at, at).unwrap();
        assert_eq!(bundled(&hover), [("std", "Str.upper", None)]);
    }

    #[wasm_bindgen_test]
    fn nothing_describable_is_none() {
        let source = "let x = 1\n\n# note\n";
        let analyzed = checked(source);
        let blank = last(source, "\n\n") + 1;
        assert!(analyzed.hover(blank, blank).is_none());
        let comment = last(source, "note");
        assert!(analyzed.hover(comment, comment).is_none());
    }

    #[wasm_bindgen_test]
    fn ranges_take_the_innermost_node_containing_them() {
        let source = "let count = (1 + 2)\necho $count";
        let analyzed = checked(source);
        // Exactly a reference, as the position in it is
        let at = last(source, "count");
        let hover = analyzed.hover(at, at + 5).unwrap();
        assert_eq!((hover.from, hover.to), (at, at + 5));
        assert!(!hover.declarations.is_empty());
        // An expression
        let sum = last(source, "1 + 2");
        let hover = analyzed.hover(sum, sum + 5).unwrap();
        assert!(hover.from <= sum && sum + 5 <= hover.to);
        assert!(!hover.types.is_empty());
        assert!(hover.declarations.is_empty());
    }
}
