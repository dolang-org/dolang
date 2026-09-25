//! Population: every declaration lifted and interned into the type database, ready
//! to seal.
//!
//! Each declaration signature is closed over one flat binder group: the outer
//! binders it is lifted over, its written binders, then its implicit ambient binders.
//! A reference to a declaration passes the binders it is lifted over as leading
//! arguments, and places type arguments by the binders they fill. An omitted type
//! argument takes its binder's default, which may refer to the binders before it; a
//! later binder stands for the dynamic type there.
//!
//! Every site with an error, diagnosed here or before, is interned as the dynamic
//! type or schema of the kind its position requires, so the database keeps every
//! structural invariant. Nothing here validates the database.

use std::{
    collections::{HashMap, HashSet},
    fmt::{self, Write},
};

use super::{
    Ambient, BinderRef, DeclNode, Designated, Head, ParamTy, Referent, RestSlot, Role, Slot,
    Tables, UnitDiag, sig,
};
use crate::{
    Compiler, RestKind,
    ast::{
        AliasBody, BinderKind, Class, ClassMember, Const, MemberScope, Param, SpecialMethod,
        TypeArg, TypeArgKind, TypeExpr, TypeKey, TypeParam, TypeParamKind, TypeQuant, visit::Node,
    },
    diag::Severity,
    source::{self, Diagnose, Span},
    typeck::r#type::{
        Argument, Binder, BinderOrigin, BinderSource, Binding, BoundRef, Database, DeclId,
        DeclKind, DeclSource, Declaration, Element, Function, Intrinsic, Kind, Literal, Member,
        MemberKey, Multiplicity, Rest, SchemaItem, Scope, Type, TypeId, UnionMember, UnitId,
        UnitSpan, Variance,
    },
};

/// The most slots a binder group may have
const MAX_BINDERS: usize = u16::MAX as usize + 1;
/// The deepest a type expression may nest
const MAX_TYPE_DEPTH: usize = 256;

/// Lift and intern every declaration into `db`, which is left ready to seal.
pub(crate) fn populate(db: &mut Database, tables: &mut Tables<'_>, diags: &mut Vec<UnitDiag>) {
    let mut sig_decls = HashMap::new();
    let mut overloads = Vec::new();
    for index in 0..tables.decls.len() {
        let id = DeclId::from_index(index);
        let count = tables.sig_count(id);
        let primary = match &tables.decls[index].node {
            DeclNode::Defs(defs) => defs.iter().position(|def| !def.is_type_only()),
            DeclNode::Methods(methods) => methods
                .iter()
                .position(|method| method.at_span.is_none() && !method.type_only),
            _ => None,
        }
        .unwrap_or(0);
        let mut all = Vec::new();
        for sig in 0..count {
            let decl = if sig == primary { id } else { db.allocate() };
            sig_decls.insert((id, sig), decl);
            all.push(decl);
        }
        if count > 1 {
            overloads.push((id, all));
        }
    }
    for (id, all) in overloads {
        db.set_overloads(id, all);
    }

    let mut groups = HashMap::new();
    let mut broken = HashSet::new();
    for index in 0..tables.decls.len() {
        let id = DeclId::from_index(index);
        for sig in 0..tables.sig_count(id) {
            let mut group = tables.lifted[&id].clone();
            let written = tables.binders(id, sig).len();
            group.extend((0..written).map(|slot| BinderRef {
                decl: id,
                sig,
                slot,
            }));
            if let Some(completed) = tables.sigs.get(&(id, sig)) {
                for ambient in [completed.input, completed.output] {
                    if let Ambient::Implicit(binder) = ambient {
                        group.push(binder);
                    }
                }
            }
            if group.len() > MAX_BINDERS {
                let decl = &tables.decls[index];
                diags.push((
                    decl.unit,
                    source::Diag::new(TooManyBinders(sig_name(tables, id, sig))),
                ));
                broken.insert((id, sig));
            }
            groups.insert((id, sig), group);
        }
    }

    for (&decl, designated) in &tables.designated {
        if let Designated::Intrinsic(intrinsic) = designated {
            let ty = db.intern(Type::Decl(decl));
            db.set_intrinsic(*intrinsic, ty);
        }
    }

    let mut populate = Populate {
        tables: &*tables,
        db: &*db,
        groups: &groups,
        broken: &broken,
        diags: Vec::new(),
        reported: HashSet::new(),
        defaults: HashMap::new(),
        expanding: Vec::new(),
    };
    let mut site_types = HashMap::new();
    for site in &tables.sites {
        let scope = populate.scope(site.group(), site.unit);
        let ty = match site.role {
            Role::Type => populate.intern(scope, site.ty, Kind::Type, 0),
            Role::Rest => {
                let kind = tables.kind_of(site.unit, site.ty).unwrap_or(Kind::Type);
                populate.intern(scope, site.ty, kind, 0)
            }
            Role::Pattern => populate.db.unknown_schema(),
            Role::Bound(binder) => populate.bound(scope, binder, site.ty),
            Role::Default(binder) => populate.intern(scope, site.ty, populate.kind(binder), 0),
            Role::Alias(decl) => populate.intern(scope, site.ty, tables.alias_kinds[&decl].kind, 0),
        };
        site_types.insert(
            UnitSpan {
                unit: site.unit,
                span: site.ty.span(),
            },
            ty,
        );
    }
    let mut declarations = Vec::new();
    for index in 0..tables.decls.len() {
        populate.decl(DeclId::from_index(index), &sig_decls, &mut declarations);
    }
    populate.inheritance_cycles();
    diags.append(&mut populate.diags);

    for (id, declaration) in declarations {
        db.populate(id, declaration);
    }
    tables.sig_decls = sig_decls;
    tables.groups = groups;
    tables.site_types = site_types;
}

/// The name of signature `sig` of a declaration, or where a closure begins
fn sig_name(tables: &Tables<'_>, decl: DeclId, sig: usize) -> Span {
    match &tables.decls[decl.index()].node {
        DeclNode::Defs(defs) => defs[sig].ident.span,
        DeclNode::Methods(methods) => methods[sig].name_span,
        DeclNode::Closure(func) => func.span(),
        DeclNode::Class(_) | DeclNode::Alias(_) => tables.decls[decl.index()]
            .name
            .expect("a type declaration is named"),
    }
}

/// Where types are interpreted: a unit, and the binder group of a declaration
/// signature, if any
#[derive(Clone, Copy)]
struct Group<'g> {
    unit: UnitId,
    binders: &'g [BinderRef],
    /// The group is too large to intern, so its binders stand for the dynamic type
    broken: bool,
}

struct Populate<'t, 'u> {
    tables: &'t Tables<'u>,
    db: &'t Database,
    groups: &'t HashMap<(DeclId, usize), Vec<BinderRef>>,
    broken: &'t HashSet<(DeclId, usize)>,
    diags: Vec<UnitDiag>,
    /// The spans already diagnosed, since a type may be interned more than once
    reported: HashSet<(UnitId, Span)>,
    /// Each binder default, interned in its declaration's group, or `None` while it is
    defaults: HashMap<BinderRef, Option<TypeId>>,
    /// The written channels being interned in place of a function type's own
    expanding: Vec<(DeclId, usize, usize)>,
}

impl<'t, 'u> Populate<'t, 'u> {
    fn report(&mut self, unit: UnitId, info: impl Diagnose + 'static) {
        if self.reported.insert((unit, info.span())) {
            self.diags.push((unit, source::Diag::new(info)));
        }
    }

    fn scope(&self, key: Option<(DeclId, usize)>, unit: UnitId) -> Group<'t> {
        match key {
            Some(key) => Group {
                unit,
                binders: &self.groups[&key],
                broken: self.broken.contains(&key),
            },
            None => Group {
                unit,
                binders: &[],
                broken: false,
            },
        }
    }

    fn kind(&self, binder: BinderRef) -> Kind {
        self.tables.binder_kinds[&binder].kind
    }

    fn unknown(&self, kind: Kind) -> TypeId {
        self.db.unknown_of(kind)
    }

    fn symbol(&self, unit: UnitId, span: Span) -> TypeId {
        let sym = self.db.intern_symbol(self.tables.text(unit, span));
        self.db.intern(Type::Literal(Literal::Sym(sym)))
    }

    /// The type of a symbol key
    fn sym(&self) -> TypeId {
        self.db
            .intrinsic(Intrinsic::Sym)
            .unwrap_or_else(|| self.db.unknown())
    }

    /// The default bound of an omitted ambient channel, `Iter[Unknown]` or
    /// `Sink[Unknown]`, when `std` designates one with a single type binder
    fn ambient_bound(&mut self, intrinsic: Intrinsic) -> Option<TypeId> {
        let base = self.db.intrinsic(intrinsic)?;
        let Type::Decl(decl) = *self.db.ty(base) else {
            return None;
        };
        let tables = self.tables;
        let binder = BinderRef {
            decl,
            sig: 0,
            slot: 0,
        };
        let single = matches!(tables.binders(decl, 0), [written] if matches!(written.kind, BinderKind::Pos))
            && tables.lifted[&decl].is_empty()
            && self.kind(binder) == Kind::Type;
        single.then(|| {
            self.db.intern(Type::Apply {
                base,
                args: vec![Argument::Positional(self.db.unknown())].into(),
                kind: Kind::Type,
            })
        })
    }

    fn schema(&self, items: Vec<SchemaItem>) -> TypeId {
        self.db.intern(Type::Schema(items.into()))
    }

    fn item(multiplicity: Multiplicity, element: Element) -> SchemaItem {
        SchemaItem {
            multiplicity,
            element,
        }
    }

    /// A reference to a binder in `group`
    fn binder(&self, group: Group<'_>, binder: BinderRef) -> TypeId {
        let kind = self.kind(binder);
        if group.broken {
            return self.unknown(kind);
        }
        let slot = group
            .binders
            .iter()
            .position(|found| *found == binder)
            .expect("a binder named outside the declarations lifted over it");
        self.db.intern(Type::Bound {
            reference: BoundRef::new(0, slot),
            kind,
        })
    }

    /// Intern a type expression as `expected`, or the dynamic type or schema when it
    /// is not one.
    fn intern(&mut self, group: Group<'_>, ty: &TypeExpr, expected: Kind, depth: usize) -> TypeId {
        if depth > MAX_TYPE_DEPTH {
            self.report(group.unit, TypeTooDeep(ty.span()));
            return self.unknown(expected);
        }
        let depth = depth + 1;
        match ty {
            TypeExpr::Group { ty, .. } => self.intern(group, ty, expected, depth),
            TypeExpr::Name { head, fields, .. } => {
                let span = fields.last().map_or(*head, |field| *head | field);
                self.reference(group, *head, span, None, expected, depth)
            }
            TypeExpr::App { base, args, .. } => {
                let mut base = &**base;
                while let TypeExpr::Group { ty, .. } = base {
                    base = ty;
                }
                match base {
                    TypeExpr::Name { head, .. } => {
                        self.reference(group, *head, ty.span(), Some(args), expected, depth)
                    }
                    _ => self.unknown(expected),
                }
            }
            TypeExpr::Schema { params, .. } if expected == Kind::Schema => {
                let items = self.items(group, params, depth);
                self.schema(items)
            }
            TypeExpr::Union { members, .. } if expected == Kind::Type => {
                let members: Vec<_> = members
                    .iter()
                    .map(|member| UnionMember::Type(self.intern(group, member, Kind::Type, depth)))
                    .collect();
                self.db.intern(Type::Union(members.into()))
            }
            TypeExpr::Func {
                params,
                input,
                output,
                arrow_span,
                ret,
                ..
            } if expected == Kind::Type => {
                let items = self.items(group, params, depth);
                let params = self.schema(items);
                let ambients = self
                    .tables
                    .func_ambients
                    .get(&UnitSpan {
                        unit: group.unit,
                        span: *arrow_span,
                    })
                    .copied();
                let [input, output] =
                    [(0, input), (1, output)].map(|(index, written)| match written {
                        Some(implicit) => self.intern(group, &implicit.ty, Kind::Type, depth),
                        None => {
                            let ambient =
                                ambients.map_or(Ambient::Unknown, |ambients| ambients[index]);
                            self.ambient(group, ambient, index, depth)
                        }
                    });
                let result = self.intern(group, ret, Kind::Type, depth);
                self.db.intern(Type::Function(Function {
                    params,
                    result,
                    input: Some(input),
                    output: Some(output),
                }))
            }
            TypeExpr::Const { expr } if expected == Kind::Type => {
                let file = &self.tables.units[group.unit.index()].compiler.file;
                let literal = match expr.fold(file) {
                    Some(Const::Str(value)) => Literal::Str(value.into()),
                    Some(Const::Int(value)) => Literal::Int(value),
                    Some(Const::Bool(value)) => Literal::Bool(value),
                    Some(Const::Nil) => Literal::Nil,
                    Some(Const::Sym(span)) => {
                        Literal::Sym(self.db.intern_symbol(self.tables.text(group.unit, span)))
                    }
                    _ => return self.unknown(expected),
                };
                self.db.intern(Type::Literal(literal))
            }
            _ => self.unknown(expected),
        }
    }

    /// The ambient channel a function type without its own takes
    fn ambient(
        &mut self,
        group: Group<'_>,
        ambient: Ambient,
        index: usize,
        depth: usize,
    ) -> TypeId {
        match ambient {
            Ambient::Implicit(binder) => self.binder(group, binder),
            Ambient::Of(decl, sig) => {
                let func = sig::function(self.tables, decl, sig);
                let Some(implicit) = [&func.input, &func.output][index] else {
                    return self.db.unknown();
                };
                // A channel containing a function type that takes that channel would
                // be infinite
                if self.expanding.contains(&(decl, sig, index)) {
                    return self.db.unknown();
                }
                self.expanding.push((decl, sig, index));
                let owner = Group {
                    unit: self.tables.decls[decl.index()].unit,
                    ..group
                };
                let ty = self.intern(owner, &implicit.ty, Kind::Type, depth);
                self.expanding.pop();
                ty
            }
            Ambient::Written | Ambient::Unknown => self.db.unknown(),
        }
    }

    /// The items of a schema or parameter list
    fn items(&mut self, group: Group<'_>, params: &[TypeParam], depth: usize) -> Vec<SchemaItem> {
        let top = self.db.top();
        let sym = self.sym();
        let mut items = Vec::new();
        for param in params {
            let (multiplicity, keyed) = match param.quant {
                None => (Multiplicity::Required, false),
                Some(TypeQuant::Opt(_)) => (Multiplicity::Optional, false),
                Some(TypeQuant::Star(_)) => (Multiplicity::Repeated, false),
                Some(TypeQuant::StarStar(_)) => (Multiplicity::Repeated, true),
            };
            let element = |value| match keyed {
                true => Element::Keyed { key: sym, value },
                false => Element::Positional(value),
            };
            match &param.kind {
                None => items.push(Self::item(multiplicity, element(top))),
                Some(TypeParamKind::Pos(ty)) => {
                    let ty = self.intern(group, ty, Kind::Type, depth);
                    items.push(Self::item(multiplicity, element(ty)));
                }
                Some(TypeParamKind::Key { key, ty, .. }) => {
                    let key = match key {
                        TypeKey::Sym(span) => self.symbol(group.unit, *span),
                        TypeKey::Type(key) => self.intern(group, key, Kind::Type, depth),
                    };
                    let value = self.intern(group, ty, Kind::Type, depth);
                    items.push(Self::item(multiplicity, Element::Keyed { key, value }));
                }
                // Only a schema's items are included, as kind checking requires
                Some(TypeParamKind::Include { ty, .. }) => {
                    let ty = self.intern(group, ty, Kind::Schema, depth);
                    items.push(Self::item(multiplicity, Element::Include(ty)));
                }
                Some(TypeParamKind::Open { .. }) => {
                    items.push(Self::item(Multiplicity::Repeated, Element::Positional(top)));
                    items.push(Self::item(
                        Multiplicity::Repeated,
                        Element::Keyed {
                            key: sym,
                            value: top,
                        },
                    ));
                }
            }
        }
        items
    }

    /// A name, with the type arguments applied to it if any
    fn reference(
        &mut self,
        group: Group<'_>,
        head: Span,
        span: Span,
        args: Option<&[TypeArg]>,
        expected: Kind,
        depth: usize,
    ) -> TypeId {
        let tables = self.tables;
        let referent = tables.referents.get(&UnitSpan {
            unit: group.unit,
            span: head,
        });
        match referent {
            // A binder takes no arguments, which is already diagnosed
            Some(&Referent::Binder(binder)) if args.is_none() && self.kind(binder) == expected => {
                self.binder(group, binder)
            }
            Some(&Referent::Decl(decl)) => {
                let result = match tables.decls[decl.index()].kind {
                    DeclKind::Class | DeclKind::Protocol => Kind::Type,
                    DeclKind::Alias | DeclKind::OpaqueAlias => tables.alias_kinds[&decl].kind,
                    DeclKind::Function | DeclKind::Closure | DeclKind::Annotation => {
                        return self.unknown(expected);
                    }
                };
                if result != expected || self.broken.contains(&(decl, 0)) || group.broken {
                    return self.unknown(expected);
                }
                if tables.designated.get(&decl) == Some(&Designated::Value) && args.is_none() {
                    return self.db.top();
                }
                self.apply(group, decl, args, span, depth)
            }
            _ => self.unknown(expected),
        }
    }

    /// A type declaration applied to type arguments, or named without any
    fn apply(
        &mut self,
        group: Group<'_>,
        decl: DeclId,
        args: Option<&[TypeArg]>,
        span: Span,
        depth: usize,
    ) -> TypeId {
        let tables = self.tables;
        let result = match tables.decls[decl.index()].kind {
            DeclKind::Class | DeclKind::Protocol => Kind::Type,
            _ => tables.alias_kinds[&decl].kind,
        };
        let base = self.db.intern(Type::Decl(decl));
        let mut full: Vec<_> = tables.lifted[&decl]
            .iter()
            .map(|&binder| self.binder(group, binder))
            .collect();
        let written = tables.binders(decl, 0);
        let binder = |slot| BinderRef { decl, sig: 0, slot };
        let application = |db: &Database, full: Vec<Argument>| match full.is_empty() {
            true => base,
            false => db.intern(Type::Apply {
                base,
                args: full.into(),
                kind: result,
            }),
        };
        let positional = |full: Vec<TypeId>| full.into_iter().map(Argument::Positional).collect();

        if written.is_empty() {
            // Arguments to what takes none are already diagnosed
            if args.is_some_and(|args| !args.is_empty()) {
                return self.unknown(result);
            }
            return application(self.db, positional(full));
        }
        let args = match args {
            Some(args) => args,
            None => {
                if written.iter().any(|written| {
                    written.default.is_none() && !matches!(written.kind, BinderKind::Rest { .. })
                }) {
                    let name = tables.text(group.unit, span).to_owned();
                    self.report(group.unit, BareGeneric { span, name });
                    return self.unknown(result);
                }
                &[]
            }
        };
        let fills = tables.fill(group.unit, decl, args);

        // Where the arguments after an expansion go is not known until its pack is,
        // so they stay as written
        if fills
            .iter()
            .any(|fill| matches!(fill, super::Fill::Unknown | super::Fill::Expand(None)))
        {
            let mut out = positional(full);
            for arg in args {
                let kind_of = |ty: &TypeExpr| tables.kind_of(group.unit, ty).unwrap_or(Kind::Type);
                out.push(match &arg.kind {
                    TypeArgKind::Pos(ty) => {
                        Argument::Positional(self.intern(group, ty, kind_of(ty), depth))
                    }
                    TypeArgKind::Key { name, ty, .. } => Argument::Keyword(
                        self.db.intern_symbol(tables.text(group.unit, *name)),
                        self.intern(group, ty, kind_of(ty), depth),
                    ),
                    TypeArgKind::Expand { ty, .. } => {
                        Argument::Expand(match self.expansion(group, ty, depth) {
                            SchemaItem {
                                element: Element::Include(schema),
                                ..
                            } => schema,
                            item => self.schema(vec![item]),
                        })
                    }
                });
            }
            return application(self.db, out);
        }

        let count = written.len();
        let mut given: Vec<Option<TypeId>> = vec![None; count];
        let mut items: Vec<Option<Vec<SchemaItem>>> = vec![None; count];
        // `Foo[T]` for `Foo[{*T}]`, or `Foo[K, V]` for `Foo[{*(K): V}]`
        let mut shorthand = Vec::new();
        let short = count == 1
            && matches!(written[0].kind, BinderKind::Pos)
            && self.kind(binder(0)) == Kind::Schema;
        for (arg, fill) in args.iter().zip(fills) {
            match fill {
                super::Fill::Binder(slot) => {
                    given[slot] =
                        Some(self.intern(group, arg.ty(), self.kind(binder(slot)), depth));
                }
                super::Fill::Item(slot) => {
                    let item = match &arg.kind {
                        TypeArgKind::Pos(ty) if short => {
                            shorthand.push(self.intern(group, ty, Kind::Type, depth));
                            items[slot].get_or_insert_default();
                            continue;
                        }
                        TypeArgKind::Pos(ty) => Self::item(
                            Multiplicity::Required,
                            Element::Positional(self.intern(group, ty, Kind::Type, depth)),
                        ),
                        TypeArgKind::Key { name, ty, .. } => Self::item(
                            Multiplicity::Required,
                            Element::Keyed {
                                key: self.symbol(group.unit, *name),
                                value: self.intern(group, ty, Kind::Type, depth),
                            },
                        ),
                        TypeArgKind::Expand { .. } => unreachable!("an expansion fills a pack"),
                    };
                    items[slot].get_or_insert_default().push(item);
                }
                super::Fill::Expand(Some(slot)) => {
                    let item = self.expansion(group, arg.ty(), depth);
                    items[slot].get_or_insert_default().push(item);
                }
                // Already diagnosed
                super::Fill::Excess | super::Fill::UnknownKeyword => {}
                super::Fill::Unknown | super::Fill::Expand(None) => unreachable!(),
            }
        }
        match shorthand[..] {
            [] => {}
            [ty] => items[0].get_or_insert_default().insert(
                0,
                Self::item(Multiplicity::Repeated, Element::Positional(ty)),
            ),
            [key, value, ..] => items[0].get_or_insert_default().insert(
                0,
                Self::item(Multiplicity::Repeated, Element::Keyed { key, value }),
            ),
        }

        // Omitted arguments take defaults, which see the arguments before them
        let lifted = full.len();
        let mut placeholder = full.clone();
        placeholder.extend((0..count).map(|slot| self.unknown(self.kind(binder(slot)))));
        let mut missing = Vec::new();
        for slot in 0..count {
            let kind = self.kind(binder(slot));
            let ty = if let Some(ty) = given[slot] {
                ty
            } else if let Some(items) = items[slot].take() {
                self.schema(items)
            } else if let Some(default) = self.default(binder(slot)) {
                self.db.substitute(default, &placeholder)
            } else if let BinderKind::Rest { .. } = written[slot].kind {
                self.schema(Vec::new())
            } else {
                missing.push(slot);
                self.unknown(kind)
            };
            placeholder[lifted + slot] = ty;
            full.push(ty);
        }
        if !missing.is_empty() {
            let owner = tables.decls[decl.index()].unit;
            let mut names = missing
                .iter()
                .take(3)
                .map(|&slot| format!("`{}`", tables.text(owner, written[slot].ident.span)))
                .collect::<Vec<_>>()
                .join(", ");
            if missing.len() > 3 {
                let _ = write!(names, ", and {} more", missing.len() - 3);
            }
            self.report(group.unit, MissingTypeArgs { span, names });
        }
        application(self.db, positional(full))
    }

    /// An expansion `...X` among type arguments: the items of a schema, or any
    /// number of a type
    fn expansion(&mut self, group: Group<'_>, ty: &TypeExpr, depth: usize) -> SchemaItem {
        if self.tables.kind_of(group.unit, ty) == Some(Kind::Schema) {
            let ty = self.intern(group, ty, Kind::Schema, depth);
            Self::item(Multiplicity::Required, Element::Include(ty))
        } else {
            let ty = self.intern(group, ty, Kind::Type, depth);
            Self::item(Multiplicity::Repeated, Element::Positional(ty))
        }
    }

    /// A binder's default, interned in its declaration's group
    fn default(&mut self, binder: BinderRef) -> Option<TypeId> {
        let tables = self.tables;
        let written = &tables.binders(binder.decl, binder.sig)[binder.slot];
        let default = written.default.as_ref()?;
        match self.defaults.get(&binder) {
            Some(Some(ty)) => return Some(*ty),
            // A default that refers to its own binder through an application
            Some(None) => return Some(self.unknown(self.kind(binder))),
            None => {}
        }
        self.defaults.insert(binder, None);
        let unit = tables.decls[binder.decl.index()].unit;
        let group = self.scope(Some((binder.decl, binder.sig)), unit);
        let ty = self.intern(group, &default.ty, self.kind(binder), 0);
        self.defaults.insert(binder, Some(ty));
        Some(ty)
    }

    /// A binder's bound. A variadic binder's bound may bound each item rather than
    /// the whole pack.
    fn bound(&mut self, group: Group<'_>, binder: BinderRef, ty: &TypeExpr) -> TypeId {
        let kind = self.kind(binder);
        let written = &self.tables.binders(binder.decl, binder.sig)[binder.slot];
        let unit = self.tables.decls[binder.decl.index()].unit;
        let group = Group { unit, ..group };
        match written.kind {
            BinderKind::Rest { kind: rest, .. }
                if self.tables.kind_of(unit, ty) != Some(Kind::Schema) =>
            {
                let item = self.intern(group, ty, Kind::Type, 0);
                let items = rest_items(rest, item, self.sym());
                self.schema(items)
            }
            _ => self.intern(group, ty, kind, 0),
        }
    }

    /// The binders of a declaration signature's group, and their metadata
    fn binders(&mut self, key: (DeclId, usize)) -> (Vec<Binder>, Vec<BinderSource>) {
        let tables = self.tables;
        if self.broken.contains(&key) {
            return (Vec::new(), Vec::new());
        }
        let unit = tables.decls[key.0.index()].unit;
        let group = self.scope(Some(key), unit);
        let mut binders = Vec::new();
        let mut sources = Vec::new();
        for &binder in group.binders {
            let owner = tables.decls[binder.decl.index()].unit;
            let span = |span| UnitSpan { unit: owner, span };
            let kind = self.kind(binder);
            let written = tables.binders(binder.decl, binder.sig).get(binder.slot);
            let origin = match written {
                _ if binder.decl != key.0 => BinderOrigin::Lifted,
                Some(_) => BinderOrigin::Written,
                None => BinderOrigin::Implicit,
            };
            let (name, name_span, binding) = match written {
                Some(written) => {
                    let name = self
                        .db
                        .intern_symbol(tables.text(owner, written.ident.span));
                    let binding = match (origin, &written.kind) {
                        (BinderOrigin::Lifted, _) | (_, BinderKind::Pos) => Binding::Positional,
                        (_, BinderKind::Key { .. }) => Binding::Keyword(name),
                        (_, BinderKind::Rest { kind, .. }) => Binding::Rest(match kind {
                            RestKind::Mixed => Rest::All,
                            RestKind::Pos => Rest::Positional,
                            RestKind::Key => Rest::Keyed,
                        }),
                    };
                    (name, written.ident.span, binding)
                }
                None => {
                    let sigil = match tables.sigs[&(binder.decl, binder.sig)].input {
                        Ambient::Implicit(input) if input == binder => "<",
                        _ => ">",
                    };
                    (
                        self.db.intern_symbol(sigil),
                        sig_name(tables, binder.decl, binder.sig),
                        Binding::Implicit,
                    )
                }
            };
            let bound = match written {
                Some(written) => written
                    .bound
                    .as_ref()
                    .map(|bound| self.bound(group, binder, &bound.ty)),
                // An omitted channel is gradual: its elements are `Unknown`
                None => self.ambient_bound(match self.db.symbol(name) {
                    "<" => Intrinsic::Iter,
                    _ => Intrinsic::Sink,
                }),
            };
            // A lifted binder is always passed, so it needs no default
            let default = match origin {
                BinderOrigin::Written => self.default(binder),
                _ => None,
            };
            let variance = match origin {
                BinderOrigin::Lifted => tables
                    .captured
                    .get(&(key.0, binder))
                    .copied()
                    .unwrap_or(Variance::Invariant),
                _ => tables.variance[&binder],
            };
            binders.push(Binder {
                kind,
                binding,
                bound,
                default,
                variance,
            });
            sources.push(BinderSource {
                name,
                span: span(name_span),
                bound: written
                    .and_then(|written| written.bound.as_ref())
                    .map(|bound| span(bound.ty.span())),
                default: written
                    .and_then(|written| written.default.as_ref())
                    .filter(|_| origin == BinderOrigin::Written)
                    .map(|default| span(default.ty.span())),
                origin,
            });
        }
        (binders, sources)
    }

    /// Close a body over a declaration signature's group
    fn declaration(
        &mut self,
        key: (DeclId, usize),
        kind: DeclKind,
        result_kind: Kind,
        body: TypeId,
    ) -> Declaration {
        let tables = self.tables;
        let decl = &tables.decls[key.0.index()];
        let (binders, sources) = self.binders(key);
        let name = decl.name.map(|_| {
            self.db
                .intern_symbol(tables.text(decl.unit, sig_name(tables, key.0, key.1)))
        });
        Declaration {
            source: DeclSource {
                kind,
                result_kind,
                name,
                span: UnitSpan {
                    unit: decl.unit,
                    span: sig_name(tables, key.0, key.1),
                },
            },
            ty: self.db.intern(Type::Quantified {
                binders: binders.into(),
                body,
            }),
            binders: sources.into(),
            supertypes: Default::default(),
            members: Default::default(),
        }
    }

    fn decl(
        &mut self,
        id: DeclId,
        sig_decls: &HashMap<(DeclId, usize), DeclId>,
        out: &mut Vec<(DeclId, Declaration)>,
    ) {
        let tables = self.tables;
        let decl = &tables.decls[id.index()];
        let group = self.scope(Some((id, 0)), decl.unit);
        match decl.node {
            DeclNode::Class(class) => {
                let body = self.db.intern(Type::Decl(id));
                let mut declaration = self.declaration((id, 0), decl.kind, Kind::Type, body);
                let mut supertypes = Vec::new();
                for super_ref in &class.super_refs {
                    let head = super_ref.ident.span;
                    let span = super_ref.fields.last().map_or(head, |field| head | field);
                    let args = (!super_ref.args.is_empty()).then_some(&super_ref.args[..]);
                    let ty = match tables.referents.get(&UnitSpan {
                        unit: decl.unit,
                        span: head,
                    }) {
                        Some(Referent::Decl(_) | Referent::External { .. }) => {
                            self.reference(group, head, span, args, Kind::Type, 0)
                        }
                        _ => continue,
                    };
                    // Every type is a subtype of top, and an erroneous supertype is
                    // already diagnosed
                    let erroneous = ty == self.db.unknown()
                        && !matches!(
                            tables.referents.get(&UnitSpan {
                                unit: decl.unit,
                                span: head
                            }),
                            Some(Referent::External { .. })
                        );
                    if ty != self.db.top() && !erroneous {
                        supertypes.push(ty);
                    }
                }
                declaration.supertypes = supertypes.into();
                declaration.members = self.members(id, group, class).into();
                out.push((id, declaration));
            }
            DeclNode::Alias(alias) => {
                let kind = tables.alias_kinds[&id].kind;
                let body = match &alias.body {
                    AliasBody::Opaque(_) => self.db.intern(Type::Decl(id)),
                    // An alias on or reaching a cycle, already diagnosed
                    AliasBody::Type(_) if tables.aliases.get(&id) == Some(&Head::Error) => {
                        self.unknown(kind)
                    }
                    AliasBody::Type(body) => self.intern(group, body, kind, 0),
                };
                out.push((id, self.declaration((id, 0), decl.kind, kind, body)));
            }
            DeclNode::Defs(_) | DeclNode::Methods(_) => {
                for sig in 0..tables.sig_count(id) {
                    let group = self.scope(Some((id, sig)), decl.unit);
                    let completed = &tables.sigs[&(id, sig)];
                    let params = self.params(id, group, &completed.params);
                    let func = sig::function(tables, id, sig);
                    let [input, output] = [
                        (completed.input, &func.input),
                        (completed.output, &func.output),
                    ]
                    .map(|(ambient, written)| match (ambient, written) {
                        (Ambient::Written, Some(implicit)) => {
                            self.intern(group, &implicit.ty, Kind::Type, 0)
                        }
                        (ambient, _) => self.ambient(group, ambient, 0, 0),
                    });
                    let result = self.slot(id, group, &completed.ret);
                    let body = self.db.intern(Type::Function(Function {
                        params,
                        result,
                        input: Some(input),
                        output: Some(output),
                    }));
                    let declaration =
                        self.declaration((id, sig), DeclKind::Function, Kind::Type, body);
                    out.push((sig_decls[&(id, sig)], declaration));
                }
            }
            DeclNode::Closure(func) => {
                let params = sig::params(tables, decl.unit, func, false);
                let params = self.params(id, group, &params);
                let [input, output] = [&func.input, &func.output].map(|written| match written {
                    Some(implicit) => self.intern(group, &implicit.ty, Kind::Type, 0),
                    None => self.db.unknown(),
                });
                let result = match &func.ret {
                    Some(ret) => self.intern(group, &ret.ty, Kind::Type, 0),
                    None => self.db.unknown(),
                };
                let body = self.db.intern(Type::Function(Function {
                    params,
                    result,
                    input: Some(input),
                    output: Some(output),
                }));
                out.push((
                    id,
                    self.declaration((id, 0), DeclKind::Closure, Kind::Type, body),
                ));
            }
        }
    }

    /// The type of a parameter, a return type, or a field
    fn slot(&mut self, id: DeclId, group: Group<'_>, slot: &Slot<'_>) -> TypeId {
        match slot {
            Slot::Annot(ty) => self.intern(group, ty, Kind::Type, 0),
            Slot::Unknown => self.db.unknown(),
            // The class, applied to its own binders, which lead a method's group
            Slot::SelfType => {
                let (class, _) = self.tables.decls[id.index()]
                    .outer
                    .expect("a method is in a class");
                if group.broken || self.broken.contains(&(class, 0)) {
                    return self.db.unknown();
                }
                let count = self.groups[&(class, 0)].len();
                let base = self.db.intern(Type::Decl(class));
                if count == 0 {
                    return base;
                }
                let args: Vec<_> = group.binders[..count]
                    .iter()
                    .map(|&binder| Argument::Positional(self.binder(group, binder)))
                    .collect();
                self.db.intern(Type::Apply {
                    base,
                    args: args.into(),
                    kind: Kind::Type,
                })
            }
        }
    }

    /// The parameter schema of a def, method or closure
    fn params(&mut self, id: DeclId, group: Group<'_>, params: &[(&Param, ParamTy<'_>)]) -> TypeId {
        let sym = self.sym();
        let mut items = Vec::new();
        for (param, ty) in params {
            let multiplicity = match param {
                Param::Pos { default, .. }
                | Param::Key { default, .. }
                | Param::ConstKey { default, .. } => match default {
                    Some(_) => Multiplicity::Optional,
                    None => Multiplicity::Required,
                },
                Param::Rest { .. } => Multiplicity::Repeated,
            };
            match (param, ty) {
                (Param::Pos { .. }, ParamTy::Single(slot)) => {
                    let ty = self.slot(id, group, slot);
                    items.push(Self::item(multiplicity, Element::Positional(ty)));
                }
                (Param::Key { key_span, .. }, ParamTy::Single(slot)) => {
                    let key = self.symbol(group.unit, *key_span);
                    let value = self.slot(id, group, slot);
                    items.push(Self::item(multiplicity, Element::Keyed { key, value }));
                }
                (Param::ConstKey { key_const, .. }, ParamTy::Single(slot)) => {
                    let key = match key_const {
                        Const::Str(value) => Some(Literal::Str(value.as_str().into())),
                        Const::Int(value) => Some(Literal::Int(*value)),
                        Const::Bool(value) => Some(Literal::Bool(*value)),
                        Const::Nil => Some(Literal::Nil),
                        Const::Sym(span) => Some(Literal::Sym(
                            self.db.intern_symbol(self.tables.text(group.unit, *span)),
                        )),
                        Const::Bin(_) | Const::F64(_) | Const::Error => None,
                    };
                    let key =
                        key.map_or(self.db.unknown(), |key| self.db.intern(Type::Literal(key)));
                    let value = self.slot(id, group, slot);
                    items.push(Self::item(multiplicity, Element::Keyed { key, value }));
                }
                (Param::Rest { .. }, ParamTy::Rest(rest)) => match rest {
                    RestSlot::Items(kind, slot) => {
                        let ty = self.slot(id, group, slot);
                        items.extend(rest_items(*kind, ty, sym));
                    }
                    RestSlot::Pack(ty) => {
                        let ty = self.intern(group, ty, Kind::Schema, 0);
                        items.push(Self::item(Multiplicity::Required, Element::Include(ty)));
                    }
                    // Mapping a pattern over packs has no representation yet
                    RestSlot::Pattern(_) => items.push(Self::item(
                        Multiplicity::Required,
                        Element::Include(self.db.unknown_schema()),
                    )),
                },
                _ => unreachable!("a parameter's type matches its kind"),
            }
        }
        self.schema(items)
    }

    /// A class's members, in source order. The first of a name wins.
    fn members(&mut self, id: DeclId, group: Group<'_>, class: &Class) -> Vec<(MemberKey, Member)> {
        let tables = self.tables;
        let unit = group.unit;
        // A method's overloads share the scope and visibility of its implementation
        let mut methods = HashMap::new();
        for (index, decl) in tables.decls.iter().enumerate() {
            if decl.outer == Some((id, 0))
                && let DeclNode::Methods(ref found) = decl.node
            {
                let primary = found
                    .iter()
                    .find(|method| method.at_span.is_none() && !method.type_only)
                    .unwrap_or(&found[0]);
                let key = (
                    primary.special.is_some(),
                    tables.text(unit, primary.name_span),
                );
                methods.insert(key, (DeclId::from_index(index), *primary));
            }
        }
        let mut seen = HashSet::new();
        let mut members = Vec::new();
        for member in &class.body.members {
            match member {
                ClassMember::Field(field) => {
                    for name in &field.fields {
                        let key = MemberKey {
                            name: self.db.intern_symbol(tables.text(unit, name.ident.span)),
                            special: false,
                        };
                        if !seen.insert(key) {
                            continue;
                        }
                        let slot = tables.fields[&(id, name.ident.span)];
                        let ty = self.slot(id, group, &slot);
                        members.push((
                            key,
                            Member::Field {
                                ty,
                                scope: match field.scope {
                                    MemberScope::Instance => Scope::Instance,
                                    MemberScope::Class => Scope::Class,
                                    MemberScope::Static => Scope::Static,
                                },
                                public: field.pub_span.is_some(),
                            },
                        ));
                    }
                }
                ClassMember::Method(method) => {
                    let text = tables.text(unit, method.name_span);
                    let key = MemberKey {
                        name: self.db.intern_symbol(text),
                        special: method.special.is_some(),
                    };
                    if !seen.insert(key) {
                        continue;
                    }
                    let (decl, primary) = methods[&(method.special.is_some(), text)];
                    members.push((
                        key,
                        Member::Method {
                            decl,
                            scope: sig::method_scope(tables, unit, primary),
                            // A special method other than `(init)` is reached by the
                            // runtime from anywhere
                            public: primary.pub_span.is_some()
                                || matches!(primary.special, Some(special)
                                    if !matches!(special, SpecialMethod::Init)),
                        },
                    ));
                }
            }
        }
        members
    }

    /// Diagnose each cycle of supertypes among classes and protocols once, where
    /// it closes, in declaration order.
    fn inheritance_cycles(&mut self) {
        let tables = self.tables;
        let count = tables.decls.len();
        let mut edges: Vec<Vec<(DeclId, Span)>> = vec![Vec::new(); count];
        for (index, decl) in tables.decls.iter().enumerate() {
            let DeclNode::Class(class) = decl.node else {
                continue;
            };
            for super_ref in &class.super_refs {
                let head = super_ref.ident.span;
                let target = match tables.referents.get(&UnitSpan {
                    unit: decl.unit,
                    span: head,
                }) {
                    Some(Referent::Decl(target)) => match tables.decls[target.index()].kind {
                        DeclKind::Class | DeclKind::Protocol => Some(*target),
                        DeclKind::Alias => match tables.aliases.get(target) {
                            Some(Head::Decl(found))
                                if matches!(
                                    tables.decls[found.index()].kind,
                                    DeclKind::Class | DeclKind::Protocol
                                ) =>
                            {
                                Some(*found)
                            }
                            _ => None,
                        },
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(target) = target {
                    let span = super_ref.fields.last().map_or(head, |field| head | field);
                    edges[index].push((target, span));
                }
            }
        }
        #[derive(Clone, Copy, PartialEq)]
        enum Color {
            White,
            Gray,
            Black,
        }
        let mut color = vec![Color::White; count];
        for root in 0..count {
            if color[root] != Color::White {
                continue;
            }
            // Iterative depth-first search, with each node's next edge
            let mut stack = vec![(root, 0)];
            color[root] = Color::Gray;
            while let Some((node, edge)) = stack.last_mut() {
                let node = *node;
                let Some(&(target, span)) = edges[node].get(*edge) else {
                    color[node] = Color::Black;
                    stack.pop();
                    continue;
                };
                *edge += 1;
                match color[target.index()] {
                    Color::White => {
                        color[target.index()] = Color::Gray;
                        stack.push((target.index(), 0));
                    }
                    Color::Gray => {
                        let unit = tables.decls[node].unit;
                        self.report(unit, InheritanceCycle(span));
                    }
                    Color::Black => {}
                }
            }
        }
    }
}

/// The items of a rest parameter or variadic bound of `item`s: `{*T}`, `{**T}` or
/// both
fn rest_items(kind: RestKind, item: TypeId, sym: TypeId) -> Vec<SchemaItem> {
    let positional = SchemaItem {
        multiplicity: Multiplicity::Repeated,
        element: Element::Positional(item),
    };
    let keyed = SchemaItem {
        multiplicity: Multiplicity::Repeated,
        element: Element::Keyed {
            key: sym,
            value: item,
        },
    };
    match kind {
        RestKind::Pos => vec![positional],
        RestKind::Key => vec![keyed],
        RestKind::Mixed => vec![positional, keyed],
    }
}

/// A generic declaration applied to too few type arguments
struct MissingTypeArgs {
    span: Span,
    names: String,
}

impl Diagnose for MissingTypeArgs {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "missing type arguments for {}", self.names)
    }

    fn span(&self) -> Span {
        self.span
    }
}

/// A generic declaration named without the type arguments it needs
struct BareGeneric {
    span: Span,
    name: String,
}

impl Diagnose for BareGeneric {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "`{}` needs type arguments", self.name)
    }

    fn span(&self) -> Span {
        self.span
    }
}

struct InheritanceCycle(Span);

impl Diagnose for InheritanceCycle {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "class inherits from itself")
    }

    fn span(&self) -> Span {
        self.0
    }
}

struct TooManyBinders(Span);

impl Diagnose for TooManyBinders {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(
            w,
            "declaration has more than {MAX_BINDERS} binders, counting those it captures"
        )
    }

    fn span(&self) -> Span {
        self.0
    }
}

struct TypeTooDeep(Span);

impl Diagnose for TypeTooDeep {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "type is nested more than {MAX_TYPE_DEPTH} deep")
    }

    fn span(&self) -> Span {
        self.0
    }
}
