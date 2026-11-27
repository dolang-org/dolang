//! A declaration's signature, in parts, as the source writes it.
use dolang_compile::{Kind, NodeId, Unit, diag::Span};

use crate::unit::{as_written, one_line};

/// A declaration's signature, in parts. Types are as written.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize),
    serde(tag = "form", rename_all = "lowercase")
)]
pub enum Signature {
    Class {
        #[cfg_attr(feature = "serde", serde(rename = "pub"))]
        is_pub: bool,
        /// A protocol
        #[cfg_attr(feature = "serde", serde(rename = "typeOnly"))]
        type_only: bool,
        name: String,
        binders: Vec<String>,
        /// `final` and `mixable`
        decorators: Vec<String>,
        supers: Vec<String>,
    },
    Def {
        #[cfg_attr(feature = "serde", serde(rename = "pub"))]
        is_pub: bool,
        /// An `@def` overload
        #[cfg_attr(feature = "serde", serde(rename = "typeOnly"))]
        type_only: bool,
        name: String,
        binders: Vec<String>,
        /// A method's `getter`, `setter`, `class` or `static`
        decorators: Vec<String>,
        params: Vec<Param>,
        /// Whether it has a body, which a type-only declaration doesn't
        body: bool,
        #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
        returns: Option<String>,
    },
    Field {
        #[cfg_attr(feature = "serde", serde(rename = "pub"))]
        is_pub: bool,
        name: String,
        #[cfg_attr(
            feature = "serde",
            serde(rename = "type", skip_serializing_if = "Option::is_none")
        )]
        ty: Option<String>,
    },
    Let {
        #[cfg_attr(feature = "serde", serde(rename = "pub"))]
        is_pub: bool,
        name: String,
        #[cfg_attr(
            feature = "serde",
            serde(rename = "type", skip_serializing_if = "Option::is_none")
        )]
        ty: Option<String>,
    },
    /// A type alias, `type` absent for an opaque one, written `...`
    Alias {
        #[cfg_attr(feature = "serde", serde(rename = "pub"))]
        is_pub: bool,
        name: String,
        binders: Vec<String>,
        #[cfg_attr(
            feature = "serde",
            serde(rename = "type", skip_serializing_if = "Option::is_none")
        )]
        ty: Option<String>,
    },
    Param {
        param: Param,
    },
}

/// A parameter of a signature
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct Param {
    /// The parameter as written up to its annotation, sigils included, such
    /// as `:host`, `foo: local` or `...args`
    pub head: String,
    /// Its annotated type, as written
    #[cfg_attr(
        feature = "serde",
        serde(rename = "type", skip_serializing_if = "Option::is_none")
    )]
    pub ty: Option<String>,
    /// Whether it has a default
    pub default: bool,
}

/// A parameter of a signature, from its declaration
pub fn parameter(unit: &Unit<'_>, id: NodeId) -> Option<Param> {
    let source = unit.source();
    let node = unit.node(id)?;
    let span = node.span()?;
    let default = match node.kind() {
        Kind::PositionalParam { default, .. } | Kind::KeyParam { default, .. } => default,
        Kind::RestParam { .. } | Kind::SelfParam { .. } => None,
        // An implicit is written as its sigil and type with no `@` between
        Kind::ImplicitParam { .. } => {
            return Some(Param {
                head: one_line(unit, span),
                ty: None,
                default: false,
            });
        }
        _ => return None,
    };
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
pub fn signature(unit: &Unit<'_>, id: NodeId) -> Option<Signature> {
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
            // A getter or setter is used as a field, not called, and a class
            // or static method lives on the type object
            let decorators = match kind {
                Kind::Method { .. } => designated(&["getter", "setter", "class", "static"]),
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
        // A vertical type keeps its lines
        Kind::Alias { is_pub, .. } => Signature::Alias {
            is_pub,
            name,
            binders: binders(),
            ty: spans(|kind| matches!(kind, Kind::Type { .. }))
                .first()
                .zip(node.span())
                .map(|(&span, decl)| as_written(unit, decl, span)),
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

/// The header of the class a declaration is a member of, as `class C[T]`
pub fn class_header(unit: &Unit<'_>, id: NodeId) -> Option<String> {
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
