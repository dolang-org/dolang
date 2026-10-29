//! Kinds: whether each binder and alias stands for a type or a schema, and a check
//! that every type expression is used as its kind allows.
//!
//! Only declarations determine kinds, never uses. A variadic binder is a schema and
//! a keyword binder a type; any other binder takes the kind of its bound, and an
//! alias the kind of its body. Those equations are solved with union-find, so alias
//! chains and cycles across units need no ordering. A kind no declaration
//! determines is a type. An opaque alias is a type, unless it's designated as an
//! intrinsic schema, and an application has its alias's kind.

use rustc_hash::FxHashMap;

use super::{
    BinderRef, Designated, Head, KindMismatch, KindOf, NotAType, NotGeneric, PatternWithoutPack,
    Referent, Role, Site, Tables, TooManyTypeArgs, UnknownTypeKeyword,
    surface::{
        Binder, BinderKind, Ref, Super, TypeArg, TypeArgKind, TypeExpr, TypeKey, TypeParam,
        TypeQuant,
    },
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
                    BinderKind::Key => Some(Kind::Type),
                    BinderKind::Pos => None,
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

    // A positional binder takes its bound's kind, and an alias its body's. A variadic
    // binder's bound is either kind, bounding each item or the whole pack.
    for decl in tables.decl_ids() {
        for sig in 0..tables.sig_count(decl) {
            for (slot, binder) in tables.binders(decl, sig).iter().enumerate() {
                if let (BinderKind::Pos, Some(bound)) = (binder.kind, binder.bound) {
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
                    DeclKind::Function | DeclKind::Closure | DeclKind::Annotation => Term::Flexible,
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
            | TypeExpr::Record { .. }
            | TypeExpr::Array { .. }
            | TypeExpr::Union { .. }
            | TypeExpr::Func { .. }
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
                    DeclKind::Function | DeclKind::Closure | DeclKind::Annotation => {
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
                // Only an alias declares a schema with a name a dict could take
                if expected == Some(Kind::Type)
                    && kind.kind == Kind::Schema
                    && let Named::Generic(..) = named
                {
                    let alias = Some(self.tables.quote(self.unit, *head));
                    self.diag(KindMismatch {
                        span,
                        expected: Kind::Type,
                        declared,
                        alias,
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
            self.diag(KindMismatch {
                span,
                expected,
                declared,
                alias: None,
            });
        }
    }

    fn check(&mut self, ty: &'t TypeExpr, expected: Option<Kind>) {
        match ty {
            TypeExpr::Group { ty, .. } => self.check(ty, expected),
            TypeExpr::Name(head) => {
                let named = self.named(head);
                self.name(head, &named, expected);
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
                params,
                tuple,
                collection,
                ..
            } => {
                self.items(params);
                // Where a type is required, a schema is a collection, as data is
                if expected == Some(Kind::Type) {
                    collection.set(*tuple);
                }
            }
            TypeExpr::Tuple { params, .. } | TypeExpr::Record { params, .. } => {
                self.items(params);
                self.expect(ty.span(), expected, Kind::Type, None);
            }
            TypeExpr::Array { elem, .. } => {
                self.check(elem, Some(Kind::Type));
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
                TypeParam::Pos { ty, .. } | TypeParam::AnyKey(Some(ty)) => {
                    self.check(ty, Some(Kind::Type))
                }
                TypeParam::Key { key, ty, .. } => {
                    if let TypeKey::Type(key) = key {
                        self.check(key, Some(Kind::Type));
                    }
                    self.check(ty, Some(Kind::Type));
                }
                TypeParam::AnyPos | TypeParam::AnyKey(None) | TypeParam::Open(_) => {}
            }
        }
    }

    /// Check type arguments applied to what `named` names, matching each to the
    /// binder it fills.
    fn args(&mut self, named: &Named<'t>, args: &'t [TypeArg], span: Span) {
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
            self.check(arg.ty(), None);
        }
    }

    fn match_args(&mut self, named: &Named<'t>, args: &'t [TypeArg]) {
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
                        self.diag(TooManyTypeArgs(arg.ty().span()));
                    }
                    None
                }
                Fill::UnknownKeyword => {
                    let TypeArgKind::Key { name, .. } = &arg.kind else {
                        unreachable!("only a keyword argument names a keyword")
                    };
                    self.diag(UnknownTypeKeyword {
                        span: name.span,
                        name: tables.name(self.unit, *name).to_owned(),
                    });
                    None
                }
            };
            self.check(arg.ty(), expected);
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
    /// An expansion `...X`, into the variadic binder in this slot if it is known to
    /// reach only that binder
    Expand(Option<usize>),
    /// A positional argument after an expansion, which fills a binder not known
    Unknown,
    /// A positional argument beyond the binders
    Excess,
    /// A keyword argument no binder takes
    UnknownKeyword,
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
    /// arguments by name, with the [shorthand](Self::shorthand) for a schema.
    pub(crate) fn fill(&self, unit: UnitId, decl: DeclId, args: &[TypeArg]) -> Vec<Fill> {
        let binders = self.binders(decl, 0);
        let owner = self.decl(decl).unit;
        let keyword = |name| {
            let text = self.name(unit, name);
            binders.iter().position(|binder| {
                binder.kind == BinderKind::Key && self.name(owner, binder.name) == text
            })
        };

        if let Some(slot) = self.shorthand(decl) {
            let mut written = args
                .iter()
                .filter(|arg| !matches!(arg.kind, TypeArgKind::Key { .. }));
            if let (
                Some(TypeArg {
                    kind: TypeArgKind::Pos(ty),
                    ..
                }),
                None,
            ) = (written.next(), written.next())
                && self.kind_of(ty) == Some(Kind::Schema)
            {
                return args
                    .iter()
                    .map(|arg| match arg.kind {
                        TypeArgKind::Key { name, .. } => {
                            keyword(name).map_or(Fill::UnknownKeyword, Fill::Binder)
                        }
                        _ => Fill::Binder(slot),
                    })
                    .collect();
            }
            let mut positional = 0;
            return args
                .iter()
                .map(|arg| match arg.kind {
                    TypeArgKind::Pos(_) => {
                        positional += 1;
                        match positional {
                            ..=2 => Fill::Item(slot),
                            _ => Fill::Excess,
                        }
                    }
                    TypeArgKind::Key { name, .. } => {
                        keyword(name).map_or(Fill::UnknownKeyword, Fill::Binder)
                    }
                    TypeArgKind::Expand { .. } => Fill::Expand(Some(slot)),
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
        let any_rest = rest(&|_| true);
        // After an expansion, which binders later positional arguments fill is unknown
        let mut expanded = false;
        args.iter()
            .map(|arg| match &arg.kind {
                TypeArgKind::Pos(_) => match positional.next() {
                    _ if expanded => Fill::Unknown,
                    Some(slot) => Fill::Binder(slot),
                    None => positional_rest.map_or(Fill::Excess, Fill::Item),
                },
                TypeArgKind::Key { name, .. } => keyword(*name)
                    .map(Fill::Binder)
                    .or(keyed_rest.map(Fill::Item))
                    .unwrap_or(Fill::UnknownKeyword),
                // A type expands as a pack of any number of it. It reaches only the
                // variadic binder once every positional binder is filled.
                TypeArgKind::Expand { .. } => {
                    let only_rest = !expanded && positional.clone().next().is_none();
                    expanded = true;
                    Fill::Expand(any_rest.filter(|_| only_rest))
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
                    DeclKind::Function | DeclKind::Closure | DeclKind::Annotation => None,
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
            | TypeExpr::Record { .. }
            | TypeExpr::Array { .. }
            | TypeExpr::Union { .. }
            | TypeExpr::Func { .. }
            | TypeExpr::Const { .. } => Some(Kind::Type),
            TypeExpr::Schema { .. } => Some(Kind::Schema),
            TypeExpr::Error { .. } => None,
        }
    }
}
