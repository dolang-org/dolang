//! Kinds: whether each binder and alias stands for a type or a schema, and a check
//! that every type expression is used as its kind allows.
//!
//! Only declarations determine kinds, never uses. A variadic binder is a schema;
//! any other binder takes the kind of its bound, and an alias the kind of its body. Those equations are solved with union-find, so alias
//! chains and cycles across units need no ordering. A kind no declaration
//! determines is a type. An opaque alias is a type, unless it's designated as an
//! intrinsic schema, and an application has its alias's kind.

use std::mem;

use rustc_hash::FxHashMap;

use super::{
    BinderRef, Designated, Head, KindMismatch, KindOf, NotAType, NotGeneric, PatternWithoutPack,
    Referent, Role, SchemaUse, ShorthandItem, Site, Tables, TooManyTypeArgs, UnfilledSchema,
    UnknownTypeKeyword,
    surface::{Binder, BinderKind, Ref, Super, Symbol, TypeExpr, TypeKey, TypeParam, TypeQuant},
};
use crate::{
    RestKind,
    diag::Span,
    typeck::{
        elab::{DeclNode, Diag, UnitDiag, sig},
        report::Report,
        r#type::{DeclId, DeclKind, Intrinsic, Kind, UnitId},
    },
};

/// Infer the kind of every binder and alias, then check every type expression
/// against the kind its use requires.
pub(crate) fn kinds(tables: &mut Tables<'_>, diags: &mut Vec<UnitDiag>) {
    let mut infer = Infer::default();
    for decl in tables.decl_ids() {
        for sig in 0..tables.sig_count(decl) {
            for (slot, binder) in tables.binders(decl, sig).iter().enumerate() {
                let fixed = match binder.kind {
                    BinderKind::Rest(_) => Some(Kind::Schema),
                    BinderKind::Pos | BinderKind::Key => None,
                };
                infer.var(Var::Binder(BinderRef { decl, sig, slot }), fixed);
            }
        }
        match tables.decl(decl).kind {
            DeclKind::Alias => infer.var(Var::Alias(decl), None),
            DeclKind::OpaqueAlias => {
                let kind = match sig::designation(tables, decl) {
                    Some((
                        Designated::Intrinsic(
                            Intrinsic::Deletable
                            | Intrinsic::Clearable
                            | Intrinsic::Fields
                            | Intrinsic::MutFields,
                        ),
                        _,
                    )) => Kind::Schema,
                    _ => Kind::Type,
                };
                infer.var(Var::Alias(decl), Some(kind))
            }
            _ => {}
        }
    }

    // A positional or key binder takes its bound's kind, and an alias its body's. A
    // variadic binder's bound is either kind, bounding each item or the whole pack.
    for decl in tables.decl_ids() {
        for sig in 0..tables.sig_count(decl) {
            for (slot, binder) in tables.binders(decl, sig).iter().enumerate() {
                if let (BinderKind::Pos | BinderKind::Key, Some(bound)) =
                    (binder.kind, binder.bound)
                {
                    let term = infer.synth(tables, tables.site_ty(bound));
                    infer.equate(Var::Binder(BinderRef { decl, sig, slot }), term);
                }
            }
        }
        if let DeclNode::Alias(alias) = &tables.decl(decl).node
            && let Some(body) = alias.body
        {
            // An alias on a cycle has an erroneous head, already diagnosed
            let term = match tables.alias(decl) {
                Some(Head::Error) => Term::Flexible,
                _ => infer.synth(tables, tables.site_ty(body)),
            };
            infer.equate(Var::Alias(decl), term);
        }
    }

    let vars: Vec<_> = infer
        .vars
        .iter()
        .map(|(&var, &index)| (var, index))
        .collect();
    for (var, index) in vars {
        let value = infer.value(index);
        let kind = KindOf {
            kind: value.kind.unwrap_or(Kind::Type),
            flexible: value.kind.is_none() && value.flexible,
        };
        match var {
            Var::Binder(binder) => tables
                .facts_mut(binder.decl)
                .binder_kinds
                .insert(binder, kind),
            Var::Alias(decl) => tables.facts_mut(decl).alias_kinds.insert(decl, kind),
        };
    }

    let mut check = Check {
        tables: &*tables,
        unit: UnitId::from_index(0),
        packs: None,
        item: false,
        repeat: None,
        diags,
    };
    for (_, site) in check.tables.sites() {
        check.site(site);
    }
    for id in check.tables.decl_ids() {
        let decl = check.tables.decl(id);
        if let DeclNode::Class(class) = &decl.node {
            check.unit = decl.unit;
            for super_ref in &class.supers {
                check.supertype(super_ref);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Var {
    Binder(BinderRef),
    Alias(DeclId),
}

#[derive(Clone, Copy, Default)]
struct Value {
    kind: Option<Kind>,
    /// A name with no known kind reached this variable
    flexible: bool,
}

/// What a type expression's kind is, in terms of the variables
enum Term {
    Known(Kind),
    Var(usize),
    /// An external or erroneous name, which constrains nothing
    Flexible,
}

#[derive(Default)]
struct Infer {
    vars: FxHashMap<Var, usize>,
    parent: Vec<usize>,
    values: Vec<Value>,
}

impl Infer {
    fn var(&mut self, var: Var, kind: Option<Kind>) {
        let index = self.parent.len();
        self.parent.push(index);
        self.values.push(Value {
            kind,
            flexible: false,
        });
        self.vars.insert(var, index);
    }

    fn root(&mut self, mut index: usize) -> usize {
        while self.parent[index] != index {
            self.parent[index] = self.parent[self.parent[index]];
            index = self.parent[index];
        }
        index
    }

    fn value(&mut self, index: usize) -> Value {
        let root = self.root(index);
        self.values[root]
    }

    fn equate(&mut self, var: Var, term: Term) {
        let root = self.root(self.vars[&var]);
        match term {
            Term::Known(kind) => {
                let value = &mut self.values[root];
                value.kind = value.kind.or(Some(kind));
            }
            Term::Flexible => self.values[root].flexible = true,
            Term::Var(other) => {
                let other = self.root(other);
                if other != root {
                    let (a, b) = (self.values[root], self.values[other]);
                    self.parent[other] = root;
                    self.values[root] = Value {
                        kind: a.kind.or(b.kind),
                        flexible: a.flexible || b.flexible,
                    };
                }
            }
        }
    }

    fn synth(&self, tables: &Tables<'_>, ty: &TypeExpr) -> Term {
        match ty {
            TypeExpr::Group { ty, .. } => self.synth(tables, ty),
            TypeExpr::Name(head) => match tables.linked(head.link) {
                Some(Referent::Decl(decl)) => match tables.decl(*decl).kind {
                    DeclKind::Class | DeclKind::Protocol => Term::Known(Kind::Type),
                    DeclKind::Alias | DeclKind::OpaqueAlias => {
                        Term::Var(self.vars[&Var::Alias(*decl)])
                    }
                    DeclKind::Function
                    | DeclKind::Closure
                    | DeclKind::Quantifier
                    | DeclKind::Annotation => Term::Flexible,
                },
                Some(Referent::Binder(binder)) => Term::Var(self.vars[&Var::Binder(*binder)]),
                Some(Referent::Module(_) | Referent::Value(_) | Referent::Error) | None => {
                    Term::Flexible
                }
            },
            // An application of an alias has its kind. Applying anything else
            // that isn't a class is an error, reported where it is checked.
            TypeExpr::App { base, .. } => match base.ungrouped() {
                TypeExpr::Name(head)
                    if matches!(
                        tables.linked(head.link),
                        Some(Referent::Decl(decl)) if matches!(
                            tables.decl(*decl).kind,
                            DeclKind::Alias | DeclKind::OpaqueAlias
                        )
                    ) =>
                {
                    self.synth(tables, base)
                }
                _ => Term::Known(Kind::Type),
            },
            TypeExpr::Tuple { .. }
            | TypeExpr::Pack { .. }
            | TypeExpr::Union { .. }
            | TypeExpr::Func { .. }
            | TypeExpr::Quantified { .. }
            | TypeExpr::Const { .. } => Term::Known(Kind::Type),
            TypeExpr::Schema { .. } => Term::Known(Kind::Schema),
            TypeExpr::Error { .. } => Term::Flexible,
        }
    }
}

/// What a type name names, for checking
enum Named<'t> {
    /// A binder or declaration of a known kind, where it was declared
    Kind(KindOf, UnitId, Span),
    /// A declaration that takes type arguments: its binders
    Generic(KindOf, UnitId, Span, DeclId, &'t [Binder]),
    /// A value, function or module
    NotAType,
    /// An external or erroneous name
    Unknown,
}

struct Check<'a, 't, 'u> {
    tables: &'t Tables<'u>,
    unit: UnitId,
    /// Within a rest binding's `@...` pattern, the packs it names so far
    packs: Option<usize>,
    /// Whether the type being checked is an item of a variadic class
    item: bool,
    /// The span of the operand of the `...` among type arguments being checked,
    /// which a type there was likely meant to repeat
    repeat: Option<(Span, String)>,
    diags: &'a mut Vec<UnitDiag>,
}

impl<'t> Check<'_, 't, '_> {
    fn diag(&mut self, info: impl Report + Send + 'static) {
        self.diags.push((self.unit, Diag::new(info)));
    }

    fn site(&mut self, site: &'t Site) {
        self.unit = site.unit;
        match site.role {
            Role::Type => self.check(&site.ty, Some(Kind::Type)),
            Role::Rest | Role::Alias(_) => self.check(&site.ty, None),
            Role::Pattern => {
                self.packs = Some(0);
                self.check(&site.ty, Some(Kind::Type));
                if self.packs.take() == Some(0) {
                    self.diag(PatternWithoutPack(site.ty.span()));
                }
            }
            Role::Bound(binder) | Role::Default(binder) => {
                let expected = match self.tables.binders(binder.decl, binder.sig)[binder.slot].kind
                {
                    // A variadic binder's bound may bound each item or the whole pack
                    BinderKind::Rest(_) if matches!(site.role, Role::Bound(_)) => None,
                    _ => Some(
                        self.tables
                            .binder_kind(binder)
                            .expect("every binder has a kind")
                            .kind,
                    ),
                };
                self.check(&site.ty, expected)
            }
        }
    }

    /// Check a supertype of a class, which is a type, and the arguments it takes.
    fn supertype(&mut self, super_ref: &'t Super) {
        let span = super_ref.head.span;
        let named = self.named(&super_ref.head);
        self.name(&super_ref.head, &named, Some(Kind::Type));
        if !super_ref.args.is_empty() {
            let bracket = super_ref.bracket_span.unwrap_or(span);
            self.args(
                &named,
                &super_ref.args,
                self.tables.info(self.unit).join(span, bracket),
            );
        }
    }

    fn named(&self, head: &Ref) -> Named<'t> {
        let tables = self.tables;
        let Some(referent) = tables.linked(head.link) else {
            return Named::Unknown;
        };
        match referent {
            Referent::Decl(decl) => {
                let owner = tables.decl(*decl);
                let name = owner.name_span().expect("a type declaration is named");
                let kind = match owner.kind {
                    DeclKind::Class | DeclKind::Protocol => KindOf {
                        kind: Kind::Type,
                        flexible: false,
                    },
                    DeclKind::Alias | DeclKind::OpaqueAlias => {
                        tables.alias_kind(*decl).expect("every alias has a kind")
                    }
                    DeclKind::Function
                    | DeclKind::Closure
                    | DeclKind::Quantifier
                    | DeclKind::Annotation => {
                        return Named::NotAType;
                    }
                };
                Named::Generic(kind, owner.unit, name, *decl, tables.binders(*decl, 0))
            }
            Referent::Binder(binder) => {
                let unit = tables.decl(binder.decl).unit;
                let name = tables.binders(binder.decl, binder.sig)[binder.slot].name;
                Named::Kind(
                    tables
                        .binder_kind(*binder)
                        .expect("every binder has a kind"),
                    unit,
                    name.span,
                )
            }
            Referent::Module(_) | Referent::Value(_) => Named::NotAType,
            Referent::Error => Named::Unknown,
        }
    }

    /// Check a name used where `expected` is required.
    fn name(&mut self, head: &Ref, named: &Named<'t>, expected: Option<Kind>) {
        let span = head.span;
        match *named {
            Named::Kind(kind, unit, declared) | Named::Generic(kind, unit, declared, ..) => {
                if kind.flexible {
                    return;
                }
                // A pattern expands over the packs it names in place of types
                if let Some(packs) = &mut self.packs
                    && kind.kind == Kind::Schema
                    && expected == Some(Kind::Type)
                {
                    *packs += 1;
                    return;
                }
                let declared = (unit == self.unit).then_some(declared);
                // Only an alias declares a schema with a name to suggest
                if expected == Some(Kind::Type)
                    && kind.kind == Kind::Schema
                    && let Named::Generic(..) = named
                {
                    let alias = self.tables.quote(self.unit, *head);
                    let schema = Some(match self.item {
                        true => SchemaUse::Expand(alias),
                        false => SchemaUse::Dict(alias),
                    });
                    self.diag(KindMismatch {
                        span,
                        expected: Kind::Type,
                        declared,
                        schema,
                    });
                    return;
                }
                self.expect(span, expected, kind.kind, declared);
            }
            Named::NotAType => {
                let name = self.tables.quote(self.unit, *head);
                self.diag(NotAType { span, name });
            }
            Named::Unknown => {}
        }
    }

    fn expect(&mut self, span: Span, expected: Option<Kind>, found: Kind, declared: Option<Span>) {
        if let Some(expected) = expected
            && expected != found
        {
            // A type included as a schema was likely meant to be repeated
            let schema = match &self.repeat {
                Some((at, ty)) if expected == Kind::Schema && *at == span => {
                    Some(SchemaUse::Repeat(ty.clone()))
                }
                _ => None,
            };
            self.diag(KindMismatch {
                span,
                expected,
                declared,
                schema,
            });
        }
    }

    fn check(&mut self, ty: &'t TypeExpr, expected: Option<Kind>) {
        let item = mem::take(&mut self.item);
        match ty {
            TypeExpr::Group { ty, .. } => {
                self.item = item;
                self.check(ty, expected)
            }
            TypeExpr::Name(head) => {
                let named = self.named(head);
                self.item = item;
                self.name(head, &named, expected);
                self.item = false;
            }
            TypeExpr::App { base, args, .. } => {
                let base = base.ungrouped();
                let found = match base {
                    TypeExpr::Name(head) => {
                        let named = self.named(head);
                        if let Named::NotAType = named {
                            self.name(head, &named, None);
                        }
                        self.args(&named, args, ty.span());
                        // An application of an alias has its kind. One of an
                        // alias without binders is already diagnosed.
                        match named {
                            Named::Generic(kind, _, _, decl, binders)
                                if matches!(
                                    self.tables.decl(decl).kind,
                                    DeclKind::Alias | DeclKind::OpaqueAlias
                                ) =>
                            {
                                (!kind.flexible && !binders.is_empty()).then_some(kind.kind)
                            }
                            _ => Some(Kind::Type),
                        }
                    }
                    _ => {
                        self.check(base, None);
                        self.args(&Named::Unknown, args, ty.span());
                        Some(Kind::Type)
                    }
                };
                if let Some(found) = found {
                    self.expect(ty.span(), expected, found, None);
                }
            }
            TypeExpr::Schema {
                span,
                params,
                vertical,
            } => {
                self.items(params);
                if expected == Some(Kind::Type) {
                    let written = match vertical {
                        true => Some("$".to_owned()),
                        false => self
                            .tables
                            .info(self.unit)
                            .source
                            .map(|source| source.text(*span).to_owned()),
                    };
                    let written = written.unwrap_or_else(|| "{...}".to_owned());
                    let schema = match (item, vertical) {
                        (true, _) => SchemaUse::Expand(written),
                        (false, true) => SchemaUse::Vertical,
                        (false, false) => SchemaUse::Dict(written),
                    };
                    self.diag(KindMismatch {
                        span: *span,
                        expected: Kind::Type,
                        declared: None,
                        schema: Some(schema),
                    });
                }
            }
            TypeExpr::Tuple { params, .. } | TypeExpr::Pack { params, .. } => {
                self.items(params);
                self.expect(ty.span(), expected, Kind::Type, None);
            }
            TypeExpr::Union { members, .. } => {
                for member in members {
                    self.check(member, Some(Kind::Type));
                }
                self.expect(ty.span(), expected, Kind::Type, None);
            }
            TypeExpr::Func {
                params,
                input,
                output,
                ret,
                ..
            } => {
                self.items(params);
                for ty in [input, output].into_iter().flatten() {
                    self.check(ty, Some(Kind::Type));
                }
                self.check(ret, Some(Kind::Type));
                self.expect(ty.span(), expected, Kind::Type, None);
            }
            TypeExpr::Quantified { func, .. } => self.check(func, expected),
            TypeExpr::Const { .. } => self.expect(ty.span(), expected, Kind::Type, None),
            TypeExpr::Error { .. } => {}
        }
    }

    /// Check the items of a schema or parameter list.
    fn items(&mut self, params: &'t [TypeParam]) {
        for param in params {
            match param {
                TypeParam::Pos {
                    quant: Some(TypeQuant::Include),
                    ty,
                } => self.check(ty, Some(Kind::Schema)),
                TypeParam::Pos { ty, .. } | TypeParam::AnyKey(_, Some(ty)) => {
                    self.check(ty, Some(Kind::Type))
                }
                TypeParam::Key { key, ty, .. } => {
                    if let TypeKey::Type(key) = key {
                        self.check(key, Some(Kind::Type));
                    }
                    self.check(ty, Some(Kind::Type));
                }
                TypeParam::AnyPos(_) | TypeParam::AnyKey(_, None) | TypeParam::Open(_) => {}
            }
        }
    }

    /// Check type arguments applied to what `named` names, matching each to the
    /// binder it fills.
    fn args(&mut self, named: &Named<'t>, args: &'t [TypeParam], span: Span) {
        match *named {
            Named::Generic(kind, .., binders) if !kind.flexible && !binders.is_empty() => {
                return self.match_args(named, args);
            }
            // A binder stands for a type, not for something that takes arguments
            Named::Generic(kind, ..) | Named::Kind(kind, ..) if !kind.flexible => {
                self.diag(NotGeneric {
                    span,
                    schema: kind.kind == Kind::Schema,
                });
            }
            _ => {}
        }
        for arg in args {
            self.arg(arg, None);
        }
    }

    /// Check a type argument. A plain one's type is checked against `expected`, and
    /// any other is checked as a schema item.
    fn arg(&mut self, arg: &'t TypeParam, expected: Option<Kind>) {
        match arg {
            _ if let Some(ty) = arg.plain() => self.check(ty, expected),
            // `...T` once repeated a type among type arguments
            TypeParam::Pos {
                quant: Some(TypeQuant::Include),
                ty,
            } => {
                let ty = ty.ungrouped();
                let written = self.tables.print(self.unit, ty);
                let outer = self.repeat.replace((ty.span(), written));
                self.check(ty, Some(Kind::Schema));
                self.repeat = outer;
            }
            _ => self.items(std::slice::from_ref(arg)),
        }
    }

    fn match_args(&mut self, named: &Named<'t>, args: &'t [TypeParam]) {
        let Named::Generic(_, _, _, decl, _) = *named else {
            unreachable!("only a declaration has binders")
        };
        let tables = self.tables;
        let mut reported = false;
        for (arg, fill) in args.iter().zip(tables.fill(self.unit, decl, args)) {
            let expected = match fill {
                Fill::Binder(slot) => Some(
                    tables
                        .binder_kind(BinderRef { decl, sig: 0, slot })
                        .expect("every binder has a kind")
                        .kind,
                ),
                Fill::Item(_) => Some(Kind::Type),
                Fill::Expand(_) | Fill::Unknown => None,
                Fill::Excess => {
                    if !reported {
                        reported = true;
                        self.diag(TooManyTypeArgs(arg.span()));
                    }
                    None
                }
                Fill::Unfit => {
                    let declared = tables.decl(decl);
                    self.diag(ShorthandItem {
                        span: arg.span(),
                        name: declared
                            .name
                            .map_or("?", |name| tables.name(declared.unit, name))
                            .to_owned(),
                    });
                    None
                }
                Fill::Included(slot) => {
                    let declared = tables.decl(decl);
                    let binder = &tables.binders(decl, 0)[slot];
                    self.diag(UnfilledSchema {
                        span: arg.span(),
                        name: declared
                            .name
                            .map_or("?", |name| tables.name(declared.unit, name))
                            .to_owned(),
                        binder: tables.name(declared.unit, binder.name).to_owned(),
                    });
                    None
                }
                Fill::UnknownKeyword => {
                    let TypeParam::Key {
                        key: TypeKey::Sym(name),
                        ..
                    } = arg
                    else {
                        unreachable!("only a keyword argument names a keyword")
                    };
                    self.diag(UnknownTypeKeyword {
                        span: name.span,
                        name: tables.name(self.unit, *name).to_owned(),
                    });
                    None
                }
            };
            // A positional schema among items was likely meant to be expanded
            self.item =
                matches!(fill, Fill::Item(_)) && matches!(arg, TypeParam::Pos { quant: None, .. });
            self.arg(arg, expected);
        }
    }
}

/// What a type argument fills of the binders of the declaration it is applied to
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fill {
    /// The whole binder in this slot
    Binder(usize),
    /// A type among the items of the schema binder in this slot
    Item(usize),
    /// An item only a variadic binder takes, such as `*T` or `...S`, into the
    /// variadic binder in this slot if it is known to reach only that binder
    Expand(Option<usize>),
    /// A positional argument after an item whose placement is unknown, which fills
    /// a binder not known
    Unknown,
    /// A positional argument beyond the binders
    Excess,
    /// A keyword argument no binder takes
    UnknownKeyword,
    /// A quantified item, such as `*T`, given for a schema binder that takes
    /// [shorthand](Tables::shorthand) types
    Unfit,
    /// An inclusion given to a declaration whose required schema binder in this
    /// slot no argument fills, and which its items can't
    Included(usize),
}

impl Tables<'_> {
    /// The slot of a declaration's only positional binder, when it's a schema and
    /// every other binder is a keyword binder. Such a declaration takes `Foo[T]`
    /// for `Foo[{*T}]` and `Foo[K, V]` for `Foo[{*(K): V}]`.
    pub(crate) fn shorthand(&self, decl: DeclId) -> Option<usize> {
        let binders = self.binders(decl, 0);
        let mut positional = binders
            .iter()
            .enumerate()
            .filter(|(_, binder)| binder.kind != BinderKind::Key);
        let (slot, binder) = positional.next()?;
        let kind = self
            .binder_kind(BinderRef { decl, sig: 0, slot })
            .expect("every binder has a kind");
        (positional.next().is_none()
            && binder.kind == BinderKind::Pos
            && kind.kind == Kind::Schema
            && !kind.flexible)
            .then_some(slot)
    }

    /// Match the type arguments applied, in `unit`, to a declaration to the binders
    /// they fill: positional arguments in order and then to a variadic binder, keyword
    /// arguments by name, with the [shorthand](Self::shorthand) for a schema. Any
    /// other item, such as `*T` or `...S`, fills only a variadic binder, and a
    /// quantified one never a schema the shorthand stands for.
    pub(crate) fn fill(&self, unit: UnitId, decl: DeclId, args: &[TypeParam]) -> Vec<Fill> {
        let mut fills = self.placed(unit, decl, args);
        // An inclusion's items are never schemas, so a declaration takes none while
        // a required schema binder isn't filled by an argument of its own: one by
        // key, or by position before any inclusion
        let required = self
            .binders(decl, 0)
            .iter()
            .enumerate()
            .position(|(slot, binder)| {
                !matches!(binder.kind, BinderKind::Rest(_))
                    && binder.default.is_none()
                    && !fills.contains(&Fill::Binder(slot))
                    && self
                        .binder_kind(BinderRef { decl, sig: 0, slot })
                        .is_some_and(|kind| kind.kind == Kind::Schema && !kind.flexible)
            });
        if let Some(slot) = required {
            for (arg, fill) in args.iter().zip(&mut fills) {
                if let TypeParam::Pos {
                    quant: Some(TypeQuant::Include),
                    ..
                } = arg
                {
                    *fill = Fill::Included(slot);
                }
            }
        }
        fills
    }

    /// Place type arguments without regard to whether an inclusion can fill the
    /// declaration's binders
    fn placed(&self, unit: UnitId, decl: DeclId, args: &[TypeParam]) -> Vec<Fill> {
        let binders = self.binders(decl, 0);
        let owner = self.decl(decl).unit;
        let keyword = |name: Symbol| {
            let text = self.name(unit, name);
            binders.iter().position(|binder| {
                binder.kind == BinderKind::Key && self.name(owner, binder.name) == text
            })
        };
        // A keyword argument names the binder it fills
        let named = |arg: &TypeParam| match arg {
            TypeParam::Key {
                quant: None,
                key: TypeKey::Sym(name),
                ..
            } => Some(*name),
            _ => None,
        };
        if let Some(slot) = self.shorthand(decl) {
            let mut written = args.iter().filter(|arg| named(arg).is_none());
            if let (Some(TypeParam::Pos { quant: None, ty }), None) =
                (written.next(), written.next())
                && self.kind_of(ty) == Some(Kind::Schema)
            {
                return args
                    .iter()
                    .map(|arg| match named(arg) {
                        Some(name) => keyword(name).map_or(Fill::UnknownKeyword, Fill::Binder),
                        None => Fill::Binder(slot),
                    })
                    .collect();
            }
            let mut positional = 0;
            return args
                .iter()
                .map(|arg| match (arg, named(arg)) {
                    (_, Some(name)) => keyword(name).map_or(Fill::UnknownKeyword, Fill::Binder),
                    (TypeParam::Pos { quant: None, .. }, _) => {
                        positional += 1;
                        match positional {
                            ..=2 => Fill::Item(slot),
                            _ => Fill::Excess,
                        }
                    }
                    // An inclusion reaches no binder known before its schema is
                    (
                        TypeParam::Pos {
                            quant: Some(TypeQuant::Include),
                            ..
                        },
                        _,
                    ) => Fill::Expand(None),
                    // The shorthand stands for a schema, so other items are inapt
                    _ => Fill::Unfit,
                })
                .collect();
        }

        let mut positional = binders
            .iter()
            .enumerate()
            .filter(|(_, binder)| matches!(binder.kind, BinderKind::Pos))
            .map(|(slot, _)| slot);
        let rest = |accepts: &dyn Fn(RestKind) -> bool| {
            binders
                .iter()
                .position(|binder| matches!(binder.kind, BinderKind::Rest(kind) if accepts(kind)))
        };
        let positional_rest = rest(&|kind| matches!(kind, RestKind::Pos | RestKind::Mixed));
        let keyed_rest = rest(&|kind| matches!(kind, RestKind::Key | RestKind::Mixed));
        // Items of either kind reach a variadic binder only when it's the only one
        let rests = binders
            .iter()
            .filter(|binder| matches!(binder.kind, BinderKind::Rest(_)))
            .count();
        let any_rest = rest(&|_| true).filter(|_| rests == 1);
        // After an item of unknown arity before the positional binders are filled,
        // or one no binder is known to take, which binders later positional
        // arguments fill is unknown
        let mut unknown = false;
        args.iter()
            .map(|arg| match (arg, named(arg)) {
                (_, Some(name)) => keyword(name)
                    .map(Fill::Binder)
                    .or(keyed_rest.map(Fill::Item))
                    .unwrap_or(Fill::UnknownKeyword),
                (TypeParam::Pos { quant: None, .. }, _) => match positional.next() {
                    _ if unknown => Fill::Unknown,
                    Some(slot) => Fill::Binder(slot),
                    None => positional_rest.map_or(Fill::Excess, Fill::Item),
                },
                // A keyed item reaches only a keyed variadic binder
                (TypeParam::Key { .. } | TypeParam::AnyKey(..), _) => {
                    keyed_rest.map_or(Fill::Excess, |slot| Fill::Expand(Some(slot)))
                }
                // Any other may stand for any number of positional items, so it
                // reaches a variadic binder only once every positional binder is
                // filled
                _ => {
                    let target = match arg {
                        TypeParam::Pos {
                            quant: Some(TypeQuant::Include),
                            ..
                        }
                        | TypeParam::Open(_) => any_rest,
                        _ => positional_rest,
                    };
                    if unknown || positional.clone().next().is_some() {
                        unknown = true;
                        Fill::Expand(None)
                    } else if let Some(slot) = target {
                        Fill::Expand(Some(slot))
                    } else if rests > 1 {
                        // Which of several variadic binders it reaches is unknown
                        unknown = true;
                        Fill::Expand(None)
                    } else {
                        Fill::Excess
                    }
                }
            })
            .collect()
    }

    /// The kind of a type expression of `unit`, once kinds are inferred. `None` when
    /// no declaration determines it, as for an external name.
    pub(crate) fn kind_of(&self, ty: &TypeExpr) -> Option<Kind> {
        let known = |kind: &KindOf| (!kind.flexible).then_some(kind.kind);
        match ty {
            TypeExpr::Group { ty, .. } => self.kind_of(ty),
            TypeExpr::Name(head) => match self.linked(head.link)? {
                Referent::Decl(decl) => match self.decl(*decl).kind {
                    DeclKind::Class | DeclKind::Protocol => Some(Kind::Type),
                    DeclKind::Alias | DeclKind::OpaqueAlias => {
                        known(&self.alias_kind(*decl).expect("every alias has a kind"))
                    }
                    DeclKind::Function
                    | DeclKind::Closure
                    | DeclKind::Quantifier
                    | DeclKind::Annotation => None,
                },
                Referent::Binder(binder) => {
                    known(&self.binder_kind(*binder).expect("every binder has a kind"))
                }
                _ => None,
            },
            // An application of an alias has its kind
            TypeExpr::App { base, .. } => match base.ungrouped() {
                name @ TypeExpr::Name(head)
                    if matches!(
                        self.linked(head.link),
                        Some(Referent::Decl(decl)) if matches!(
                            self.decl(*decl).kind,
                            DeclKind::Alias | DeclKind::OpaqueAlias
                        )
                    ) =>
                {
                    self.kind_of(name)
                }
                _ => Some(Kind::Type),
            },
            TypeExpr::Tuple { .. }
            | TypeExpr::Pack { .. }
            | TypeExpr::Union { .. }
            | TypeExpr::Func { .. }
            | TypeExpr::Quantified { .. }
            | TypeExpr::Const { .. } => Some(Kind::Type),
            TypeExpr::Schema { .. } => Some(Kind::Schema),
            TypeExpr::Error { .. } => None,
        }
    }
}
