//! Kinds: whether each binder and alias stands for a type or a schema, and a check
//! that every type expression is used as its kind allows.
//!
//! Only declarations determine kinds, never uses. A variadic binder is a schema and
//! a keyword binder a type; any other binder takes the kind of its bound, and an
//! alias the kind of its body. Those equations are solved with union-find, so alias
//! chains and cycles across units need no ordering. A kind no declaration
//! determines is a type.

use std::collections::HashMap;

use super::{
    Ambient, BinderRef, Head, KindMismatch, KindOf, NotAType, NotGeneric, PatternWithoutPack,
    Referent, Role, Site, Tables, TooManyTypeArgs, UnknownTypeKeyword,
};
use crate::{
    RestKind,
    ast::{
        AliasBody, Binder, BinderKind, ClassSuper, TypeArg, TypeArgKind, TypeExpr, TypeKey,
        TypeParam, TypeParamKind, implicits, visit::Node,
    },
    source::{self, Diagnose, Span},
    typeck::{
        elab::{DeclNode, UnitDiag, sig},
        r#type::{DeclId, DeclKind, Kind, UnitId, UnitSpan},
    },
};

/// Infer the kind of every binder and alias, then check every type expression
/// against the kind its use requires.
pub(crate) fn kinds(tables: &mut Tables<'_>, diags: &mut Vec<UnitDiag>) {
    let mut infer = Infer::default();
    for index in 0..tables.decls.len() {
        let decl = DeclId::from_index(index);
        for sig in 0..tables.sig_count(decl) {
            for (slot, binder) in tables.binders(decl, sig).iter().enumerate() {
                let fixed = match binder.kind {
                    BinderKind::Rest { .. } => Some(Kind::Schema),
                    BinderKind::Key { .. } => Some(Kind::Type),
                    BinderKind::Pos => None,
                };
                infer.var(Var::Binder(BinderRef { decl, sig, slot }), fixed);
            }
        }
        match tables.decls[index].kind {
            DeclKind::Alias => infer.var(Var::Alias(decl), None),
            DeclKind::OpaqueAlias => infer.var(Var::Alias(decl), Some(Kind::Type)),
            _ => {}
        }
    }

    // A positional binder takes its bound's kind, and an alias its body's. A variadic
    // binder's bound is either kind, bounding each item or the whole pack.
    for index in 0..tables.decls.len() {
        let decl = DeclId::from_index(index);
        let unit = tables.decls[index].unit;
        for sig in 0..tables.sig_count(decl) {
            for (slot, binder) in tables.binders(decl, sig).iter().enumerate() {
                if let (BinderKind::Pos, Some(bound)) = (&binder.kind, &binder.bound) {
                    let term = infer.synth(tables, unit, &bound.ty);
                    infer.equate(Var::Binder(BinderRef { decl, sig, slot }), term);
                }
            }
        }
        if let DeclNode::Alias(alias) = tables.decls[index].node
            && let AliasBody::Type(body) = &alias.body
        {
            // An alias on a cycle has an erroneous head, already diagnosed
            let term = match tables.aliases.get(&decl) {
                Some(Head::Error) => Term::Flexible,
                _ => infer.synth(tables, unit, body),
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
            Var::Binder(binder) => tables.binder_kinds.insert(binder, kind),
            Var::Alias(decl) => tables.alias_kinds.insert(decl, kind),
        };
    }

    let mut check = Check {
        tables: &*tables,
        unit: UnitId::from_index(0),
        ambient: None,
        packs: None,
        diags,
        func_ambients: HashMap::new(),
    };
    for site in &check.tables.sites {
        check.site(site);
    }
    for index in 0..check.tables.decls.len() {
        let decl = &check.tables.decls[index];
        if let DeclNode::Class(class) = decl.node {
            check.unit = decl.unit;
            check.ambient = None;
            for super_ref in &class.super_refs {
                check.supertype(super_ref);
            }
        }
    }
    let func_ambients = check.func_ambients;
    tables.func_ambients = func_ambients;
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
    vars: HashMap<Var, usize>,
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

    fn synth(&self, tables: &Tables<'_>, unit: UnitId, ty: &TypeExpr) -> Term {
        match ty {
            TypeExpr::Group { ty, .. } => self.synth(tables, unit, ty),
            TypeExpr::Name { head, .. } => {
                match tables.referents.get(&UnitSpan { unit, span: *head }) {
                    Some(Referent::Decl(decl)) => match tables.decls[decl.index()].kind {
                        DeclKind::Class | DeclKind::Protocol => Term::Known(Kind::Type),
                        DeclKind::Alias | DeclKind::OpaqueAlias => {
                            Term::Var(self.vars[&Var::Alias(*decl)])
                        }
                        DeclKind::Function | DeclKind::Closure | DeclKind::Annotation => {
                            Term::Flexible
                        }
                    },
                    Some(Referent::Binder(binder)) => Term::Var(self.vars[&Var::Binder(*binder)]),
                    Some(
                        Referent::External { .. }
                        | Referent::Module(_)
                        | Referent::Value(_)
                        | Referent::Error,
                    )
                    | None => Term::Flexible,
                }
            }
            // Applying a schema is an error, reported where it is checked
            TypeExpr::App { .. }
            | TypeExpr::Union { .. }
            | TypeExpr::Func { .. }
            | TypeExpr::Const { .. } => Term::Known(Kind::Type),
            TypeExpr::Schema { .. } => Term::Known(Kind::Schema),
            TypeExpr::Error => Term::Flexible,
        }
    }
}

/// What a type name names, for checking
enum Named<'u> {
    /// A binder or declaration of a known kind, where it was declared
    Kind(KindOf, UnitId, Span),
    /// A declaration that takes type arguments: its binders
    Generic(KindOf, UnitId, Span, DeclId, &'u [Binder]),
    /// A value, function or module
    NotAType,
    /// An external or erroneous name
    Unknown,
}

struct Check<'a, 't, 'u> {
    tables: &'t Tables<'u>,
    unit: UnitId,
    /// The signature whose channels function types without their own take
    ambient: Option<(DeclId, usize)>,
    /// Within a rest binding's `@...` pattern, the packs it names so far
    packs: Option<usize>,
    diags: &'a mut Vec<UnitDiag>,
    func_ambients: HashMap<UnitSpan, [Ambient; 2]>,
}

impl<'u> Check<'_, '_, 'u> {
    fn diag(&mut self, info: impl Diagnose + 'static) {
        self.diags.push((self.unit, source::Diag::new(info)));
    }

    fn site(&mut self, site: &Site<'u>) {
        self.unit = site.unit;
        self.ambient = site.ambient;
        match site.role {
            Role::Type => self.check(site.ty, Some(Kind::Type)),
            Role::Rest | Role::Alias(_) => self.check(site.ty, None),
            Role::Pattern => {
                self.packs = Some(0);
                self.check(site.ty, Some(Kind::Type));
                if self.packs.take() == Some(0) {
                    self.diag(PatternWithoutPack(site.ty.span()));
                }
            }
            Role::Bound(binder) | Role::Default(binder) => {
                let expected = match self.tables.binders(binder.decl, binder.sig)[binder.slot].kind
                {
                    // A variadic binder's bound may bound each item or the whole pack
                    BinderKind::Rest { .. } if matches!(site.role, Role::Bound(_)) => None,
                    _ => Some(self.tables.binder_kinds[&binder].kind),
                };
                self.check(site.ty, expected)
            }
        }
    }

    /// Check a supertype of a class, which is a type, and the arguments it takes.
    fn supertype(&mut self, super_ref: &'u ClassSuper) {
        let span = super_ref
            .fields
            .last()
            .map_or(super_ref.ident.span, |field| super_ref.ident.span | field);
        let named = self.named(super_ref.ident.span);
        self.name(span, &named, Some(Kind::Type));
        if !super_ref.args.is_empty() {
            let bracket = super_ref.bracket_span.unwrap_or(span);
            self.args(&named, &super_ref.args, span | bracket);
        }
    }

    fn named(&self, head: Span) -> Named<'u> {
        let tables = self.tables;
        let Some(referent) = tables.referents.get(&UnitSpan {
            unit: self.unit,
            span: head,
        }) else {
            return Named::Unknown;
        };
        match referent {
            Referent::Decl(decl) => {
                let owner = &tables.decls[decl.index()];
                let name = owner.name.expect("a type declaration is named");
                let kind = match owner.kind {
                    DeclKind::Class | DeclKind::Protocol => KindOf {
                        kind: Kind::Type,
                        flexible: false,
                    },
                    DeclKind::Alias | DeclKind::OpaqueAlias => tables.alias_kinds[decl],
                    DeclKind::Function | DeclKind::Closure | DeclKind::Annotation => {
                        return Named::NotAType;
                    }
                };
                Named::Generic(kind, owner.unit, name, *decl, tables.binders(*decl, 0))
            }
            Referent::Binder(binder) => {
                let unit = tables.decls[binder.decl.index()].unit;
                let ident = &tables.binders(binder.decl, binder.sig)[binder.slot].ident;
                Named::Kind(tables.binder_kinds[binder], unit, ident.span)
            }
            Referent::Module(_) | Referent::Value(_) => Named::NotAType,
            Referent::External { .. } | Referent::Error => Named::Unknown,
        }
    }

    /// Check a name used where `expected` is required.
    fn name(&mut self, span: Span, named: &Named<'u>, expected: Option<Kind>) {
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
                self.expect(span, expected, kind.kind, declared);
            }
            Named::NotAType => {
                let name = self.tables.text(self.unit, span).to_owned();
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
            });
        }
    }

    fn check(&mut self, ty: &'u TypeExpr, expected: Option<Kind>) {
        match ty {
            TypeExpr::Group { ty, .. } => self.check(ty, expected),
            TypeExpr::Name { head, fields, .. } => {
                let span = fields.last().map_or(*head, |field| *head | field);
                let named = self.named(*head);
                self.name(span, &named, expected);
            }
            TypeExpr::App { base, args, .. } => {
                let mut base = &**base;
                while let TypeExpr::Group { ty, .. } = base {
                    base = ty;
                }
                match base {
                    TypeExpr::Name { head, .. } => {
                        let named = self.named(*head);
                        if let Named::NotAType = named {
                            self.name(base.span(), &named, None);
                        }
                        self.args(&named, args, ty.span());
                    }
                    _ => {
                        self.check(base, None);
                        self.args(&Named::Unknown, args, ty.span());
                    }
                }
                self.expect(ty.span(), expected, Kind::Type, None);
            }
            TypeExpr::Schema { params, .. } => {
                self.items(params);
                self.expect(ty.span(), expected, Kind::Schema, None);
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
                arrow_span,
                ret,
                ..
            } => {
                self.items(params);
                for implicit in implicits(input, output) {
                    self.check(&implicit.ty, Some(Kind::Type));
                }
                self.check(ret, Some(Kind::Type));
                if input.is_none() || output.is_none() {
                    let channels = match self.ambient {
                        Some((decl, sig)) => sig::channels(self.tables, decl, sig),
                        None => [Ambient::Unknown; 2],
                    };
                    let written = [input.is_some(), output.is_some()];
                    let ambients = [0, 1].map(|index| match written[index] {
                        true => Ambient::Written,
                        false => channels[index],
                    });
                    self.func_ambients.insert(
                        UnitSpan {
                            unit: self.unit,
                            span: *arrow_span,
                        },
                        ambients,
                    );
                }
                self.expect(ty.span(), expected, Kind::Type, None);
            }
            TypeExpr::Const { .. } => self.expect(ty.span(), expected, Kind::Type, None),
            TypeExpr::Error => {}
        }
    }

    /// Check the items of a schema or parameter list.
    fn items(&mut self, params: &'u [TypeParam]) {
        for param in params {
            match &param.kind {
                Some(TypeParamKind::Pos(ty)) => self.check(ty, Some(Kind::Type)),
                Some(TypeParamKind::Key { key, ty, .. }) => {
                    if let TypeKey::Type(key) = key {
                        self.check(key, Some(Kind::Type));
                    }
                    self.check(ty, Some(Kind::Type));
                }
                Some(TypeParamKind::Include { ty, .. }) => self.check(ty, Some(Kind::Schema)),
                Some(TypeParamKind::Open { .. }) | None => {}
            }
        }
    }

    /// Check type arguments applied to what `named` names, matching each to the
    /// binder it fills.
    fn args(&mut self, named: &Named<'u>, args: &'u [TypeArg], span: Span) {
        match *named {
            Named::Generic(kind, .., binders)
                if !kind.flexible && kind.kind == Kind::Type && !binders.is_empty() =>
            {
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

    fn match_args(&mut self, named: &Named<'u>, args: &'u [TypeArg]) {
        let Named::Generic(_, _, _, decl, _) = *named else {
            unreachable!("only a declaration has binders")
        };
        let tables = self.tables;
        let mut reported = false;
        for (arg, fill) in args.iter().zip(tables.fill(self.unit, decl, args)) {
            let expected = match fill {
                Fill::Binder(slot) => {
                    Some(tables.binder_kinds[&BinderRef { decl, sig: 0, slot }].kind)
                }
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
                    let TypeArgKind::Key { name, .. } = arg.kind else {
                        unreachable!("only a keyword argument names a keyword")
                    };
                    self.diag(UnknownTypeKeyword {
                        span: name,
                        name: tables.text(self.unit, name).to_owned(),
                    });
                    None
                }
            };
            self.check(arg.ty(), expected);
        }
    }

    /// The kind a type expression has, if a declaration determines it
    fn synth_kind(&self, ty: &TypeExpr) -> Option<Kind> {
        self.tables.kind_of(self.unit, ty)
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
    /// Match the type arguments applied, in `unit`, to a declaration to the binders
    /// they fill: positional arguments in order and then to a variadic binder, keyword
    /// arguments by name. A declaration whose only binder is a schema takes `Foo[T]`
    /// for `Foo[{*T}]` and `Foo[K, V]` for `Foo[{*(K): V}]`.
    pub(crate) fn fill(&self, unit: UnitId, decl: DeclId, args: &[TypeArg]) -> Vec<Fill> {
        let binders = self.binders(decl, 0);
        let kind_of = |slot: usize| self.binder_kinds[&BinderRef { decl, sig: 0, slot }];

        if let [binder] = binders
            && matches!(binder.kind, BinderKind::Pos)
            && kind_of(0).kind == Kind::Schema
            && !kind_of(0).flexible
        {
            if let [
                TypeArg {
                    kind: TypeArgKind::Pos(ty),
                    ..
                },
            ] = args
                && self.kind_of(unit, ty) == Some(Kind::Schema)
            {
                return vec![Fill::Binder(0)];
            }
            let mut positional = 0;
            return args
                .iter()
                .map(|arg| match arg.kind {
                    TypeArgKind::Pos(_) => {
                        positional += 1;
                        match positional {
                            ..=2 => Fill::Item(0),
                            _ => Fill::Excess,
                        }
                    }
                    TypeArgKind::Key { .. } => Fill::UnknownKeyword,
                    TypeArgKind::Expand { .. } => Fill::Expand(Some(0)),
                })
                .collect();
        }

        let mut positional = binders
            .iter()
            .enumerate()
            .filter(|(_, binder)| matches!(binder.kind, BinderKind::Pos))
            .map(|(slot, _)| slot);
        let rest = |accepts: &dyn Fn(RestKind) -> bool| {
            binders.iter().position(
                |binder| matches!(binder.kind, BinderKind::Rest { kind, .. } if accepts(kind)),
            )
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
                TypeArgKind::Key { name, .. } => {
                    let text = self.text(unit, *name);
                    let owner = self.decls[decl.index()].unit;
                    binders
                        .iter()
                        .position(|binder| {
                            matches!(binder.kind, BinderKind::Key { .. })
                                && self.text(owner, binder.ident.span) == text
                        })
                        .map(Fill::Binder)
                        .or(keyed_rest.map(Fill::Item))
                        .unwrap_or(Fill::UnknownKeyword)
                }
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
    pub(crate) fn kind_of(&self, unit: UnitId, ty: &TypeExpr) -> Option<Kind> {
        let known = |kind: &KindOf| (!kind.flexible).then_some(kind.kind);
        match ty {
            TypeExpr::Group { ty, .. } => self.kind_of(unit, ty),
            TypeExpr::Name { head, .. } => {
                match self.referents.get(&UnitSpan { unit, span: *head })? {
                    Referent::Decl(decl) => match self.decls[decl.index()].kind {
                        DeclKind::Class | DeclKind::Protocol => Some(Kind::Type),
                        DeclKind::Alias | DeclKind::OpaqueAlias => known(&self.alias_kinds[decl]),
                        DeclKind::Function | DeclKind::Closure | DeclKind::Annotation => None,
                    },
                    Referent::Binder(binder) => known(&self.binder_kinds[binder]),
                    _ => None,
                }
            }
            TypeExpr::App { .. }
            | TypeExpr::Union { .. }
            | TypeExpr::Func { .. }
            | TypeExpr::Const { .. } => Some(Kind::Type),
            TypeExpr::Schema { .. } => Some(Kind::Schema),
            TypeExpr::Error => None,
        }
    }
}
