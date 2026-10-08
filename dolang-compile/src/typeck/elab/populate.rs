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
    collections::{HashMap, HashSet, hash_map::Entry as MapEntry},
    fmt::{self, Write},
};

use super::{
    Ambient, BinderRef, DeclNode, Designated, Diag, Head, ParamTy, Referent, RestSlot, Role, Slot,
    Tables, UnitDiag,
    sig::{self, Form},
    surface::{
        BinderKind, Class, ConstLit, Member as SourceMember, MemberScope, Name, ParamKind,
        Signature, TypeArg, TypeArgKind, TypeExpr, TypeKey, TypeParam, TypeParamKind, TypeQuant,
    },
};
use crate::{
    RestKind,
    ast::SpecialMethod,
    diag::{NoteKind, Severity},
    source::Span,
    typeck::report::Report,
    typeck::r#type::{
        Argument, Binder, BinderOrigin, BinderSource, Binding, BoundRef, Database, DeclId,
        DeclKind, DeclSource, Declaration, Element, Function, Intrinsic, Kind, Literal, Member,
        MemberKey, Multiplicity, Rest, SchemaItem, Scope, Supertype, Type, TypeId, UnionMember,
        UnitId, UnitSpan, Variance,
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
            DeclNode::Defs(defs) => defs.iter().position(|def| !def.type_only),
            // A protocol member is type-only, but not an overload signature
            DeclNode::Methods(methods) => methods.iter().position(|method| !method.overload),
            _ => None,
        };
        // Without an implementation, which resolution reports, the first overload
        // holds the function's own ID
        let mut signatures = Vec::new();
        for sig in 0..count {
            let decl = if sig == primary.unwrap_or(0) {
                id
            } else {
                db.allocate()
            };
            sig_decls.insert((id, sig), decl);
            if Some(sig) != primary {
                signatures.push(decl);
            }
        }
        if count > 1 {
            overloads.push((id, signatures));
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
                    Diag::new(TooManyBinders(sig_span(tables, id, sig))),
                ));
                broken.insert((id, sig));
            }
            groups.insert((id, sig), group);
        }
    }

    for (&decl, designated) in &tables.designated {
        let intrinsic = match *designated {
            Designated::Intrinsic(intrinsic) => intrinsic,
            // Its name is top, but its members are every value's
            Designated::Value => Intrinsic::Value,
            _ => continue,
        };
        let ty = db.intern(Type::Decl(decl));
        db.set_intrinsic(intrinsic, ty);
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
        expr_types: HashMap::new(),
        pattern: None,
        supertype: false,
    };
    let mut site_types = HashMap::new();
    for site in &tables.sites {
        let scope = populate.scope(site.group(), site.unit);
        let ty = match site.role {
            Role::Type => populate.intern(scope, &site.ty, Kind::Type, 0),
            Role::Rest => {
                let kind = tables.kind_of(site.unit, &site.ty).unwrap_or(Kind::Type);
                populate.intern(scope, &site.ty, kind, 0)
            }
            Role::Pattern => populate.pattern(scope, &site.ty),
            Role::Bound(binder) => populate.bound(scope, binder, &site.ty),
            Role::Default(binder) => populate.intern(scope, &site.ty, populate.kind(binder), 0),
            Role::Alias(decl) => {
                populate.intern(scope, &site.ty, tables.alias_kinds[&decl].kind, 0)
            }
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

    let expr_types = std::mem::take(&mut populate.expr_types);
    for (id, declaration) in declarations {
        db.populate(id, declaration);
    }
    tables.expr_types = expr_types;
    tables.sig_decls = sig_decls;
    tables.groups = groups;
    tables.site_types = site_types;
}

/// The name of signature `sig` of a named declaration
fn sig_name(tables: &Tables<'_>, decl: DeclId, sig: usize) -> Name {
    match &tables.decls[decl.index()].node {
        DeclNode::Defs(defs) => defs[sig].name,
        DeclNode::Methods(methods) => methods[sig].name,
        DeclNode::Closure(_) => unreachable!("a closure is not named"),
        DeclNode::Class(_) | DeclNode::Alias(_) => tables.decls[decl.index()]
            .name
            .expect("a type declaration is named"),
    }
}

/// The span of the name of signature `sig` of a declaration, or where a closure
/// begins
fn sig_span(tables: &Tables<'_>, decl: DeclId, sig: usize) -> Span {
    match &tables.decls[decl.index()].node {
        DeclNode::Closure(closure) => closure.span,
        _ => sig_name(tables, decl, sig).span,
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
    /// Each written application and function type, by span
    expr_types: HashMap<UnitSpan, TypeId>,
    /// Within a rest binding's `@...` pattern, the packs it names so far. The
    /// pattern is interpreted in a group of its own, one item of each pack.
    pattern: Option<Vec<TypeId>>,
    /// Whether the reference being interned is a supertype, which stays nominal
    /// where a type position would intern it as a structural type
    supertype: bool,
}

impl<'t, 'u> Populate<'t, 'u> {
    fn report(&mut self, unit: UnitId, info: impl Report + 'static) {
        if self.reported.insert((unit, info.span())) {
            self.diags.push((unit, Diag::new(info)));
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

    fn symbol(&self, unit: UnitId, name: Name) -> TypeId {
        let sym = self.db.intern_symbol(self.tables.name(unit, name));
        self.db.intern(Type::Literal(Literal::Sym(sym)))
    }

    /// The literal a constant of `unit` stands for
    fn literal(&self, unit: UnitId, value: &ConstLit) -> Literal {
        match value {
            ConstLit::Str(value) => Literal::Str(value.clone()),
            ConstLit::Int(value) => Literal::Int(*value),
            ConstLit::Bool(value) => Literal::Bool(*value),
            ConstLit::Nil => Literal::Nil,
            ConstLit::Sym(name) => {
                Literal::Sym(self.db.intern_symbol(self.tables.name(unit, *name)))
            }
        }
    }

    /// The type of a symbol key
    fn sym(&self) -> TypeId {
        self.db
            .intrinsic(Intrinsic::Sym)
            .unwrap_or_else(|| self.db.unknown())
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

    /// A reference to a binder in `group`, from outside a pattern's own group if
    /// one is being interned
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
            reference: BoundRef::new(usize::from(self.pattern.is_some()), slot),
            kind,
        })
    }

    /// A rest binding's `@...` pattern mapped over the packs it names, or the
    /// dynamic schema if it names none, which is already diagnosed
    fn pattern(&mut self, group: Group<'_>, ty: &TypeExpr) -> TypeId {
        let outer = self.pattern.replace(Vec::new());
        let pattern = self.intern(group, ty, Kind::Type, 0);
        let packs = std::mem::replace(&mut self.pattern, outer).expect("in a pattern");
        if packs.is_empty() || packs.len() > MAX_BINDERS {
            return self.db.unknown_schema();
        }
        self.db.intern(Type::Map {
            packs: packs.into(),
            pattern,
        })
    }

    /// In a pattern, a reference to the current item of the pack a schema name
    /// stands for where a type is expected, as kind checking counts it
    fn pack(
        &mut self,
        group: Group<'_>,
        head: &Name,
        fields: &[Name],
        span: Span,
        depth: usize,
    ) -> Option<TypeId> {
        let tables = self.tables;
        self.pattern.as_ref()?;
        let kind = match tables.referents.get(&UnitSpan {
            unit: group.unit,
            span: head.span,
        })? {
            Referent::Binder(binder) => tables.binder_kinds[binder],
            Referent::Decl(decl) => match tables.decls[decl.index()].kind {
                DeclKind::Alias | DeclKind::OpaqueAlias => tables.alias_kinds[decl],
                _ => return None,
            },
            _ => return None,
        };
        if kind.flexible || kind.kind != Kind::Schema {
            return None;
        }
        // The pack itself is interpreted outside the pattern's group
        let outer = self.pattern.take();
        let pack = self.reference(group, head, fields, span, None, Kind::Schema, depth);
        self.pattern = outer;
        let packs = self.pattern.as_mut().expect("in a pattern");
        let slot = packs
            .iter()
            .position(|&found| found == pack)
            .unwrap_or_else(|| {
                packs.push(pack);
                packs.len() - 1
            });
        Some(self.db.intern(Type::Bound {
            reference: BoundRef::new(0, slot),
            kind: Kind::Type,
        }))
    }

    /// Intern a type expression as `expected`, or the dynamic type or schema when it
    /// is not one.
    fn intern(&mut self, group: Group<'_>, ty: &TypeExpr, expected: Kind, depth: usize) -> TypeId {
        let id = self.intern_node(group, ty, expected, depth);
        // Well-formedness checks these where they are written, in the group they are
        // written in, not where a def's channels are taken by a function type. A
        // pattern's interior is not checked.
        if let TypeExpr::App { .. }
        | TypeExpr::Tuple { .. }
        | TypeExpr::Record { .. }
        | TypeExpr::Func { .. } = ty
            && self.expanding.is_empty()
            && self.pattern.is_none()
        {
            self.expr_types.insert(
                UnitSpan {
                    unit: group.unit,
                    span: ty.span(),
                },
                id,
            );
        }
        id
    }

    fn intern_node(
        &mut self,
        group: Group<'_>,
        ty: &TypeExpr,
        expected: Kind,
        depth: usize,
    ) -> TypeId {
        if depth > MAX_TYPE_DEPTH {
            self.report(group.unit, TypeTooDeep(ty.span()));
            return self.unknown(expected);
        }
        let depth = depth + 1;
        match ty {
            TypeExpr::Group { ty, .. } => self.intern(group, ty, expected, depth),
            TypeExpr::Name { head, fields, .. } => {
                let span = fields
                    .last()
                    .map_or(head.span, |field| head.span | field.span);
                self.reference(group, head, fields, span, None, expected, depth)
            }
            TypeExpr::App { base, args, .. } => match base.ungrouped() {
                TypeExpr::Name { head, fields, .. } => {
                    self.reference(group, head, fields, ty.span(), Some(args), expected, depth)
                }
                _ => self.unknown(expected),
            },
            TypeExpr::Schema { params, .. } if expected == Kind::Schema => {
                let items = self.items(group, params, false, depth);
                self.schema(items)
            }
            TypeExpr::Tuple { params, .. } if expected == Kind::Type => {
                self.keyed_inclusions(group.unit, params);
                let items = self.items(group, params, false, depth);
                let tuple = Designated::Intrinsic(Intrinsic::Tuple);
                self.collection(tuple, self.schema(items))
            }
            TypeExpr::Record { params, .. } if expected == Kind::Type => {
                let items = self.items(group, params, false, depth);
                self.collection(Designated::Record, self.schema(items))
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
                let items = self.items(group, params, true, depth);
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
                        Some(implicit) => self.intern(group, implicit, Kind::Type, depth),
                        None => {
                            let ambient =
                                ambients.map_or(Ambient::Value, |ambients| ambients[index]);
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
            TypeExpr::Const {
                value: Some(value), ..
            } if expected == Kind::Type => {
                let literal = self.literal(group.unit, value);
                self.db.intern(Type::Literal(literal))
            }
            _ => self.unknown(expected),
        }
    }

    /// A designated class applied to `arg` for its only binder, or `Unknown` if the
    /// class isn't checked
    fn collection(&self, role: Designated, arg: TypeId) -> TypeId {
        let Some(decl) = self.tables.designated_decl(role) else {
            return self.unknown(Kind::Type);
        };
        self.db.intern(Type::Apply {
            base: self.db.intern(Type::Decl(decl)),
            args: vec![Argument::Positional(arg)].into(),
            kind: Kind::Type,
        })
    }

    /// Report each item of a tuple form that may admit keyed items. An inclusion
    /// never makes a record, so what it includes can't change the form's meaning.
    fn keyed_inclusions(&mut self, unit: UnitId, params: &[TypeParam]) {
        for param in params {
            match &param.kind {
                Some(TypeParamKind::Open(span)) => {
                    self.report(
                        unit,
                        KeyedInclusion {
                            span: *span,
                            open: true,
                        },
                    );
                }
                Some(TypeParamKind::Include { ty }) if self.may_key(unit, ty, &mut Vec::new()) => {
                    self.report(
                        unit,
                        KeyedInclusion {
                            span: ty.span(),
                            open: false,
                        },
                    );
                }
                _ => {}
            }
        }
    }

    /// Whether a schema of `unit` may admit keyed items. What it is when no
    /// declaration says, as for an external name, constrains nothing, so it may not.
    fn may_key(&self, unit: UnitId, ty: &TypeExpr, aliases: &mut Vec<DeclId>) -> bool {
        let tables = self.tables;
        match ty {
            TypeExpr::Group { ty, .. } => self.may_key(unit, ty, aliases),
            // An application's arguments only fill the alias's binders
            TypeExpr::App { base, .. } => self.may_key(unit, base, aliases),
            TypeExpr::Schema { params, .. } => {
                params
                    .iter()
                    .any(|param| match (&param.quant, &param.kind) {
                        (Some(TypeQuant::StarStar), _)
                        | (_, Some(TypeParamKind::Key { .. } | TypeParamKind::Open(_))) => true,
                        (_, Some(TypeParamKind::Include { ty })) => self.may_key(unit, ty, aliases),
                        _ => false,
                    })
            }
            TypeExpr::Name { head, .. } => match tables.referents.get(&UnitSpan {
                unit,
                span: head.span,
            }) {
                Some(&Referent::Decl(decl)) => {
                    let DeclNode::Alias(alias) = &tables.decls[decl.index()].node else {
                        return false;
                    };
                    let Some(body) = alias.body.filter(|_| !aliases.contains(&decl)) else {
                        return false;
                    };
                    aliases.push(decl);
                    let owner = tables.decls[decl.index()].unit;
                    let may = self.may_key(owner, tables.site_ty(body), aliases);
                    aliases.pop();
                    may
                }
                Some(&Referent::Binder(binder)) => {
                    let written = &tables.binders(binder.decl, binder.sig)[binder.slot];
                    let owner = tables.decls[binder.decl.index()].unit;
                    // A bound of a variadic binder may bound each item instead
                    let bound = written
                        .bound
                        .map(|bound| tables.site_ty(bound))
                        .filter(|bound| tables.kind_of(owner, bound) == Some(Kind::Schema));
                    match (written.kind, bound) {
                        (BinderKind::Key | BinderKind::Rest(RestKind::Pos), _) => false,
                        (BinderKind::Rest(RestKind::Key), _) => true,
                        (_, Some(bound)) => self.may_key(owner, bound, aliases),
                        (_, None) => true,
                    }
                }
                _ => false,
            },
            _ => false,
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
                let Some(implicit) = [func.input, func.output][index] else {
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
                let ty = self.intern(owner, self.tables.site_ty(implicit), Kind::Type, depth);
                self.expanding.pop();
                ty
            }
            Ambient::Value => self.db.top(),
            Ambient::Written => self.db.unknown(),
        }
    }

    /// The items of a schema or parameter list. A bare `**` or `...` admits any
    /// keyed item in a schema, but only a named one in a parameter list.
    fn items(
        &mut self,
        group: Group<'_>,
        params: &[TypeParam],
        parameters: bool,
        depth: usize,
    ) -> Vec<SchemaItem> {
        let top = self.db.top();
        let sym = self.sym();
        let any = match parameters {
            true => sym,
            false => top,
        };
        let mut items = Vec::new();
        for param in params {
            let (multiplicity, keyed) = match param.quant {
                None => (Multiplicity::Required, false),
                Some(TypeQuant::Opt) => (Multiplicity::Optional, false),
                Some(TypeQuant::Star) => (Multiplicity::Repeated, false),
                Some(TypeQuant::StarStar) => (Multiplicity::Repeated, true),
            };
            let element = |value| match keyed {
                true => Element::Keyed { key: sym, value },
                false => Element::Positional(value),
            };
            match &param.kind {
                None if keyed => items.push(Self::item(
                    multiplicity,
                    Element::Keyed {
                        key: any,
                        value: top,
                    },
                )),
                None => items.push(Self::item(multiplicity, element(top))),
                Some(TypeParamKind::Pos(ty)) => {
                    let ty = self.intern(group, ty, Kind::Type, depth);
                    items.push(Self::item(multiplicity, element(ty)));
                }
                Some(TypeParamKind::Key { key, ty }) => {
                    let key = match key {
                        TypeKey::Sym(name) => self.symbol(group.unit, *name),
                        TypeKey::Type(key) => self.intern(group, key, Kind::Type, depth),
                    };
                    let value = self.intern(group, ty, Kind::Type, depth);
                    items.push(Self::item(multiplicity, Element::Keyed { key, value }));
                }
                // Only a schema's items are included, as kind checking requires
                Some(TypeParamKind::Include { ty }) => {
                    let ty = self.intern(group, ty, Kind::Schema, depth);
                    items.push(Self::item(multiplicity, Element::Include(ty)));
                }
                Some(TypeParamKind::Open(_)) => {
                    items.push(Self::item(Multiplicity::Repeated, Element::Positional(top)));
                    items.push(Self::item(
                        Multiplicity::Repeated,
                        Element::Keyed {
                            key: any,
                            value: top,
                        },
                    ));
                }
            }
        }
        items
    }

    /// A name, spanning `span`, with the type arguments applied to it if any
    #[allow(clippy::too_many_arguments)]
    fn reference(
        &mut self,
        group: Group<'_>,
        head: &Name,
        fields: &[Name],
        span: Span,
        args: Option<&[TypeArg]>,
        expected: Kind,
        depth: usize,
    ) -> TypeId {
        let supertype = std::mem::take(&mut self.supertype);
        if expected == Kind::Type
            && args.is_none()
            && let Some(item) = self.pack(group, head, fields, span, depth)
        {
            return item;
        }
        let tables = self.tables;
        let referent = tables.referents.get(&UnitSpan {
            unit: group.unit,
            span: head.span,
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
                match (tables.designated.get(&decl), args) {
                    (Some(Designated::Value), None) => return self.db.top(),
                    (Some(Designated::Never), None) => return self.db.bottom(),
                    _ => {}
                }
                // Bare `Func` in a type is any function
                if let (Some(Designated::Intrinsic(Intrinsic::Func)), None) =
                    (tables.designated.get(&decl), args)
                    && !supertype
                {
                    return self.db.gradual_function();
                }
                self.apply(group, decl, args, (head, fields, span), depth)
            }
            _ => self.unknown(expected),
        }
    }

    /// A type declaration applied to type arguments, or named without any, by the
    /// name it is written with and the span of the whole
    fn apply(
        &mut self,
        group: Group<'_>,
        decl: DeclId,
        args: Option<&[TypeArg]>,
        (head, fields, span): (&Name, &[Name], Span),
        depth: usize,
    ) -> TypeId {
        let tables = self.tables;
        let result = match tables.decls[decl.index()].kind {
            DeclKind::Class | DeclKind::Protocol => Kind::Type,
            _ => tables.alias_kinds[&decl].kind,
        };
        let base = self.db.intern(Type::Decl(decl));
        let full: Vec<_> = tables.lifted[&decl]
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
                    written.default.is_none()
                        && !matches!(written.kind, BinderKind::Rest(_))
                        && !matches!(
                            tables.designated.get(&decl),
                            Some(Designated::Fmt | Designated::FmtValue)
                        )
                }) {
                    let name = tables.dotted(group.unit, *head, fields);
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
                    TypeArgKind::Key { name, ty } => Argument::Keyword(
                        self.db.intern_symbol(tables.name(group.unit, *name)),
                        self.intern(group, ty, kind_of(ty), depth),
                    ),
                    TypeArgKind::Expand { ty } => {
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
        let short = tables.shorthand(decl);
        for (arg, fill) in args.iter().zip(fills) {
            match fill {
                super::Fill::Binder(slot) => {
                    given[slot] =
                        Some(self.intern(group, arg.ty(), self.kind(binder(slot)), depth));
                }
                super::Fill::Item(slot) => {
                    let item = match &arg.kind {
                        TypeArgKind::Pos(ty) if short.is_some() => {
                            shorthand.push(self.intern(group, ty, Kind::Type, depth));
                            items[slot].get_or_insert_default();
                            continue;
                        }
                        TypeArgKind::Pos(ty) => Self::item(
                            Multiplicity::Required,
                            Element::Positional(self.intern(group, ty, Kind::Type, depth)),
                        ),
                        TypeArgKind::Key { name, ty } => Self::item(
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
        let short = short.unwrap_or(0);
        match shorthand[..] {
            [] => {}
            [ty] => items[short].get_or_insert_default().insert(
                0,
                Self::item(Multiplicity::Repeated, Element::Positional(ty)),
            ),
            [key, value, ..] => items[short].get_or_insert_default().insert(
                0,
                Self::item(Multiplicity::Repeated, Element::Keyed { key, value }),
            ),
        }

        self.complete(group, decl, full, given, items, span)
    }

    /// Complete an application of a type declaration from the arguments given for
    /// its binders, after those it is lifted over. Omitted arguments take defaults,
    /// which see the arguments before them.
    fn complete(
        &mut self,
        group: Group<'_>,
        decl: DeclId,
        mut full: Vec<TypeId>,
        given: Vec<Option<TypeId>>,
        mut items: Vec<Option<Vec<SchemaItem>>>,
        span: Span,
    ) -> TypeId {
        let tables = self.tables;
        let result = match tables.decls[decl.index()].kind {
            DeclKind::Class | DeclKind::Protocol => Kind::Type,
            _ => tables.alias_kinds[&decl].kind,
        };
        let written = tables.binders(decl, 0);
        let count = written.len();
        let binder = |slot| BinderRef { decl, sig: 0, slot };
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
            } else if let BinderKind::Rest(_) = written[slot].kind {
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
                .map(|&slot| format!("`{}`", tables.name(owner, written[slot].name)))
                .collect::<Vec<_>>()
                .join(", ");
            if missing.len() > 3 {
                let _ = write!(names, ", and {} more", missing.len() - 3);
            }
            self.report(group.unit, MissingTypeArgs { span, names });
        }
        let base = self.db.intern(Type::Decl(decl));
        if full.is_empty() {
            return base;
        }
        let args: Vec<_> = full.into_iter().map(Argument::Positional).collect();
        self.db.intern(Type::Apply {
            base,
            args: args.into(),
            kind: result,
        })
    }

    /// The body of a pipe placeholder: its nominee applied to the placeholder's
    /// binders in order, or `Unknown` without one
    fn pipe(&mut self, group: Group<'_>, placeholder: DeclId) -> TypeId {
        let Some(nominee) = self.tables.pipes[&placeholder] else {
            return self.unknown(Kind::Type);
        };
        let mut given = vec![None; self.tables.binders(nominee, 0).len()];
        for (slot, filled) in sig::positional(self.tables, nominee)
            .into_iter()
            .enumerate()
            .take(self.tables.binders(placeholder, 0).len())
        {
            let binder = BinderRef {
                decl: placeholder,
                sig: 0,
                slot,
            };
            given[filled] = Some(self.binder(group, binder));
        }
        let items = vec![None; given.len()];
        let span = self.tables.decls[placeholder.index()]
            .name_span()
            .expect("a designated declaration is named");
        self.complete(group, nominee, Vec::new(), given, items, span)
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
        let Some(default) = written.default else {
            return matches!(
                tables.designated.get(&binder.decl),
                Some(Designated::Fmt | Designated::FmtValue)
            )
            .then(|| self.unknown(self.kind(binder)));
        };
        let default = tables.site_ty(default);
        match self.defaults.get(&binder) {
            Some(Some(ty)) => return Some(*ty),
            // A default that refers to its own binder through an application
            Some(None) => return Some(self.unknown(self.kind(binder))),
            None => {}
        }
        self.defaults.insert(binder, None);
        let unit = tables.decls[binder.decl.index()].unit;
        let group = self.scope(Some((binder.decl, binder.sig)), unit);
        let pattern = self.pattern.take();
        let ty = self.intern(group, default, self.kind(binder), 0);
        self.pattern = pattern;
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
            BinderKind::Rest(rest) if self.tables.kind_of(unit, ty) != Some(Kind::Schema) => {
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
                    let name = self.db.intern_symbol(tables.name(owner, written.name));
                    // A lifted binder keeps its binding, so a rest binder keeps its
                    // shape as a bound; it is still always passed positionally
                    let binding = match written.kind {
                        BinderKind::Pos => Binding::Positional,
                        BinderKind::Key => Binding::Keyword(name),
                        BinderKind::Rest(kind) => Binding::Rest(match kind {
                            RestKind::Mixed => Rest::All,
                            RestKind::Pos => Rest::Positional,
                            RestKind::Key => Rest::Keyed,
                        }),
                    };
                    (name, written.name.span, binding)
                }
                None => {
                    let sigil = match tables.sigs[&(binder.decl, binder.sig)].input {
                        Ambient::Implicit(input) if input == binder => "<",
                        _ => ">",
                    };
                    (
                        self.db.intern_symbol(sigil),
                        sig_span(tables, binder.decl, binder.sig),
                        Binding::Implicit,
                    )
                }
            };
            let bound = match written {
                Some(written) => written
                    .bound
                    .map(|bound| self.bound(group, binder, tables.site_ty(bound))),
                // An omitted channel is unbounded; the body sees it by its unit's
                // mode
                None => None,
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
                    .and_then(|written| written.bound)
                    .map(|bound| span(tables.site_ty(bound).span())),
                default: written
                    .and_then(|written| written.default)
                    .filter(|_| origin == BinderOrigin::Written)
                    .map(|default| span(tables.site_ty(default).span())),
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
                .intern_symbol(tables.name(decl.unit, sig_name(tables, key.0, key.1)))
        });
        Declaration {
            source: DeclSource {
                kind,
                result_kind,
                name,
                span: UnitSpan {
                    unit: decl.unit,
                    span: sig_span(tables, key.0, key.1),
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
        match &decl.node {
            DeclNode::Class(class) => {
                let body = self.db.intern(Type::Decl(id));
                let mut declaration = self.declaration((id, 0), decl.kind, Kind::Type, body);
                let mut supertypes = Vec::new();
                for super_ref in &class.supers {
                    let head = super_ref.head.span;
                    let span = super_ref.span();
                    let args = (!super_ref.args.is_empty()).then_some(&super_ref.args[..]);
                    let ty = match tables.referents.get(&UnitSpan {
                        unit: decl.unit,
                        span: head,
                    }) {
                        Some(Referent::Decl(_) | Referent::External { .. }) => {
                            self.supertype = true;
                            self.reference(
                                group,
                                &super_ref.head,
                                &super_ref.fields,
                                span,
                                args,
                                Kind::Type,
                                0,
                            )
                        }
                        _ => continue,
                    };
                    // Checked for well-formedness by its name, as it has no type
                    // expression of its own
                    self.expr_types.insert(
                        UnitSpan {
                            unit: decl.unit,
                            span,
                        },
                        ty,
                    );
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
                        // A protocol's supertypes are all claims
                        let runtime = decl.kind == DeclKind::Class && !super_ref.type_only;
                        supertypes.push(Supertype { ty, runtime });
                    }
                }
                declaration.supertypes = supertypes.into();
                declaration.members = self.members(id, group, class).into();
                out.push((id, declaration));
            }
            DeclNode::Alias(alias) => {
                let kind = tables.alias_kinds[&id].kind;
                let body = match alias.body {
                    None if tables.pipes.contains_key(&id) => self.pipe(group, id),
                    None => self.db.intern(Type::Decl(id)),
                    // An alias on or reaching a cycle, already diagnosed
                    Some(_) if tables.aliases.get(&id) == Some(&Head::Error) => self.unknown(kind),
                    Some(body) => self.intern(group, tables.site_ty(body), kind, 0),
                };
                // A pipe placeholder is transparent, standing for its nominee
                let decl_kind = match tables.pipes.contains_key(&id) {
                    true => DeclKind::Alias,
                    false => decl.kind,
                };
                out.push((id, self.declaration((id, 0), decl_kind, kind, body)));
            }
            DeclNode::Defs(_) | DeclNode::Methods(_) => {
                for sig in 0..tables.sig_count(id) {
                    let group = self.scope(Some((id, sig)), decl.unit);
                    let completed = &tables.sigs[&(id, sig)];
                    let func = sig::function(tables, id, sig);
                    let params = self.params(id, group, func, &completed.params);
                    let [input, output] = [
                        (0, completed.input, func.input),
                        (1, completed.output, func.output),
                    ]
                    .map(|(index, ambient, written)| {
                        match (ambient, written) {
                            (Ambient::Written, Some(implicit)) => {
                                self.intern(group, tables.site_ty(implicit), Kind::Type, 0)
                            }
                            (ambient, _) => self.ambient(group, ambient, index, 0),
                        }
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
            DeclNode::Closure(closure) => {
                let func = &closure.sig;
                let params = sig::params(tables, decl.unit, func, false);
                let params = self.params(id, group, func, &params);
                let [input, output, result] =
                    [func.input, func.output, func.ret].map(|written| match written {
                        Some(written) => self.intern(group, tables.site_ty(written), Kind::Type, 0),
                        None => self.db.unknown(),
                    });
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
    fn slot(&mut self, id: DeclId, group: Group<'_>, slot: &Slot) -> TypeId {
        match *slot {
            Slot::Annot(ty) => self.intern(group, self.tables.site_ty(ty), Kind::Type, 0),
            Slot::Unknown => self.db.unknown(),
            Slot::Nil => self.db.intern(Type::Literal(Literal::Nil)),
            // The class, applied to its own binders, which lead a method's group
            Slot::SelfType => {
                let (class, _) = self.tables.decls[id.index()]
                    .outer
                    .expect("a method is in a class");
                if group.broken || self.broken.contains(&(class, 0)) {
                    return self.db.unknown();
                }
                // Every value is a `Value`'s receiver
                if self.tables.designated.get(&class) == Some(&Designated::Value) {
                    return self.db.top();
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
    fn params(
        &mut self,
        id: DeclId,
        group: Group<'_>,
        func: &Signature,
        tys: &[ParamTy],
    ) -> TypeId {
        let sym = self.sym();
        let mut items = Vec::new();
        for (param, ty) in func.params.iter().zip(tys) {
            let multiplicity = match (&param.kind, param.default) {
                (ParamKind::Rest { .. }, _) => Multiplicity::Repeated,
                (_, true) => Multiplicity::Optional,
                (_, false) => Multiplicity::Required,
            };
            match (&param.kind, ty) {
                (ParamKind::Pos, ParamTy::Single(slot)) => {
                    let ty = self.slot(id, group, slot);
                    items.push(Self::item(multiplicity, Element::Positional(ty)));
                }
                (ParamKind::Key { key }, ParamTy::Single(slot)) => {
                    let key = self.symbol(group.unit, *key);
                    let value = self.slot(id, group, slot);
                    items.push(Self::item(multiplicity, Element::Keyed { key, value }));
                }
                (ParamKind::ConstKey { key }, ParamTy::Single(slot)) => {
                    let key = match key {
                        Some(key) => {
                            let literal = self.literal(group.unit, key);
                            self.db.intern(Type::Literal(literal))
                        }
                        None => self.db.unknown(),
                    };
                    let value = self.slot(id, group, slot);
                    items.push(Self::item(multiplicity, Element::Keyed { key, value }));
                }
                (ParamKind::Rest { .. }, ParamTy::Rest(rest)) => match rest {
                    // No call gives a `do` block's rest its items, so a strict
                    // unit's admits any
                    RestSlot::Items(kind, Slot::Unknown)
                        if self.tables.units[group.unit.index()].strict
                            && matches!(
                                self.tables.decls[id.index()].node,
                                DeclNode::Closure(_)
                            ) =>
                    {
                        items.extend(rest_items(*kind, self.db.top(), sym));
                    }
                    RestSlot::Items(kind, slot) => {
                        let ty = self.slot(id, group, slot);
                        items.extend(rest_items(*kind, ty, sym));
                    }
                    RestSlot::Pack(ty) => {
                        let ty = self.intern(group, self.tables.site_ty(*ty), Kind::Schema, 0);
                        items.push(Self::item(Multiplicity::Required, Element::Include(ty)));
                    }
                    RestSlot::Pattern(ty) => {
                        let ty = self.pattern(group, self.tables.site_ty(*ty));
                        items.push(Self::item(Multiplicity::Required, Element::Include(ty)));
                    }
                },
                _ => unreachable!("a parameter's type matches its kind"),
            }
        }
        self.schema(items)
    }

    /// A class's members, in source order. The first member of a key in a
    /// namespace wins, except that a getter and a setter make one property.
    fn members(&mut self, id: DeclId, group: Group<'_>, class: &Class) -> Vec<(MemberKey, Member)> {
        let tables = self.tables;
        let unit = group.unit;
        // A function's overload signatures share the scope, visibility and form of
        // its implementation
        let mut methods = HashMap::new();
        for (index, decl) in tables.decls.iter().enumerate() {
            if decl.outer == Some((id, 0))
                && let DeclNode::Methods(ref found) = decl.node
            {
                let primary = found
                    .iter()
                    .find(|method| !method.overload)
                    .unwrap_or(&found[0]);
                methods.insert(primary.name.span, (DeclId::from_index(index), primary));
            }
        }
        // Instance members and type-object members are separate namespaces
        let mut index = HashMap::new();
        let mut members = Vec::new();
        for member in &class.members {
            match *member {
                SourceMember::Field(ref field) => {
                    let scope = match field.scope {
                        MemberScope::Instance => Scope::Instance,
                        MemberScope::Class => Scope::Class,
                        MemberScope::Static => Scope::Static,
                    };
                    for &name in &field.names {
                        let key = MemberKey {
                            name: self.db.intern_symbol(tables.name(unit, name)),
                            special: false,
                            private: !field.public,
                        };
                        let MapEntry::Vacant(entry) = index.entry((key, scope == Scope::Instance))
                        else {
                            continue;
                        };
                        entry.insert(members.len());
                        let slot = tables.fields[&(id, name.span)];
                        let ty = self.slot(id, group, &slot);
                        members.push((
                            key,
                            Member::Field {
                                ty,
                                scope,
                                public: field.public,
                            },
                        ));
                    }
                }
                SourceMember::Method { decl, sig } => {
                    let method = tables.method(decl, sig);
                    let Some(&(decl, primary)) = methods.get(&method.name.span) else {
                        // An overload signature, with its implementation's function
                        continue;
                    };
                    let key = MemberKey {
                        name: self.db.intern_symbol(tables.name(unit, primary.name)),
                        special: primary.special.is_some(),
                        private: !primary.public && primary.special.is_none(),
                    };
                    let scope = sig::method_scope(tables, unit, primary);
                    // A special method other than `(init)` is reached by the runtime
                    // from anywhere
                    let public = primary.public
                        || matches!(primary.special, Some(special)
                            if !matches!(special, SpecialMethod::Init));
                    let form = sig::method_form(tables, unit, primary);
                    match index.entry((key, scope == Scope::Instance)) {
                        MapEntry::Occupied(entry) => {
                            let (_, found) = &mut members[*entry.get()];
                            match (form, found) {
                                (
                                    Form::Getter,
                                    Member::Property {
                                        getter: slot @ None,
                                        ..
                                    },
                                )
                                | (
                                    Form::Setter,
                                    Member::Property {
                                        setter: slot @ None,
                                        ..
                                    },
                                ) => *slot = Some(decl),
                                _ => {}
                            }
                        }
                        MapEntry::Vacant(entry) => {
                            entry.insert(members.len());
                            let member = match form {
                                Form::Plain => Member::Method {
                                    decl,
                                    scope,
                                    public,
                                },
                                Form::Getter | Form::Setter => Member::Property {
                                    getter: (form == Form::Getter).then_some(decl),
                                    setter: (form == Form::Setter).then_some(decl),
                                    scope,
                                    public,
                                },
                                Form::Unknown => Member::Decorated {
                                    decl,
                                    scope,
                                    public,
                                },
                            };
                            members.push((key, member));
                        }
                    }
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
            let DeclNode::Class(class) = &decl.node else {
                continue;
            };
            for super_ref in &class.supers {
                let head = super_ref.head.span;
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
                    edges[index].push((target, super_ref.span()));
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

impl Report for MissingTypeArgs {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
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

impl Report for BareGeneric {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
        write!(w, "`{}` needs type arguments", self.name)
    }

    fn span(&self) -> Span {
        self.span
    }
}

/// An item of a tuple form that may admit keyed items: a bare `...` when `open`,
/// and otherwise an inclusion
struct KeyedInclusion {
    span: Span,
    open: bool,
}

impl Report for KeyedInclusion {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
        match self.open {
            true => write!(w, "`...` admits keyed items, which a tuple type can't hold"),
            false => write!(
                w,
                "included schema may have keyed items, which a tuple type can't hold"
            ),
        }
    }

    fn span(&self) -> Span {
        self.span
    }

    fn notes(&self) -> Vec<(NoteKind, String)> {
        let note = match self.open {
            true => "`(*)` is a tuple of any positional items",
            false => {
                "a keyed item makes a record; to include keyed items without one, write `Record[...{...}]`"
            }
        };
        vec![(NoteKind::Help, note.to_owned())]
    }
}

struct InheritanceCycle(Span);

impl Report for InheritanceCycle {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
        write!(w, "class inherits from itself")
    }

    fn span(&self) -> Span {
        self.0
    }
}

struct TooManyBinders(Span);

impl Report for TooManyBinders {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
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

impl Report for TypeTooDeep {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
        write!(w, "type is nested more than {MAX_TYPE_DEPTH} deep")
    }

    fn span(&self) -> Span {
        self.0
    }
}
