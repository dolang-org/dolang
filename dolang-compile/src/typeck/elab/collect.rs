//! Collect every declaration of the checked units, and resolve their type names to
//! what they refer to, chasing imports across units.
//!
//! Each unit is walked with the frames type resolution pushed, binder groups and
//! lexical scopes, so a name's [`TypeRes`] nominates its target directly. Names that
//! go through an import are fixed up once every unit's exports are known.

use std::{borrow::Cow, collections::hash_map::Entry as MapEntry, iter};

use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

use super::{
    AliasCycle, Bases, BinderRef, Decl, DeclAst, DeclNode, Diag, Head, ImportCycle, MissingExport,
    MissingModule, PIPES, Referent, Role, Shard, Site, Tables, Target, UnitDiag, UnitInfo,
    ids::Rebase,
    interface::PrivateInInterface,
    local,
    surface::{
        self, Collection, ConstLit, Interned, LinkId, ModuleId, Name, Ref, SiteId, StrId, Symbol,
        TypeExpr,
    },
};
use crate::{
    Mode, PreludeImport, Unit,
    ast::{
        self, AliasBody, Annot, Arg, ArrayElem, Binders, Block, Class, ClassMember, Const,
        DictElem, Expr, ExprBody, FieldInit, For, Function, GetVariant, Ident, If, ImportElement,
        LValue, Method, PatBind, PatIdent, PatItem, Pattern, PrimStmt, Res, SpecialMethod, Stmt,
        TypeDecl, TypeEntry, TypeRes, Var, visit::Node,
    },
    resolvety::names_type_in,
    source::{File, Span},
    typeck::{
        r#type::{Database, DeclId, DeclKind, UnitId, UnitSpan},
        typelib::wire,
    },
};

/// What collection learns of one unit: its surface, and how the type names written
/// in it resolve as far as the unit itself can tell.
///
/// A harvest is local until it is linked: its unit is unit 0, and its declarations
/// and sites are numbered from 0.
#[derive(Clone)]
pub(crate) struct Harvest<'u> {
    pub(crate) info: UnitInfo<'u>,
    /// The unit's text, by [`StrId`]: each string once. Most borrow the unit's
    /// source; a constant type's string is folded from it.
    pub(crate) strings: Vec<Cow<'u, str>>,
    pub(crate) decls: Vec<Decl<'u>>,
    pub(crate) sites: Vec<Site>,
    /// What the unit's names refer to as far as the unit can tell, each once, by
    /// [`LinkId`]
    pub(crate) links: Vec<Link>,
    /// The link of each name the unit writes, by the span of its head. Empty for a
    /// typelib, which has no syntax to look a name up in.
    pub(crate) refs: FxHashMap<Span, LinkId>,
    /// The unit's exports, each name once, in the order they are declared. Empty for
    /// a unit that is not a module.
    pub(crate) exports: Vec<Export>,
    /// Each exported variable, in the order of the spans of their names
    pub(crate) values: Vec<Value>,
    /// The modules the unit imports, and the prelude modules it uses, by
    /// [`ModuleId`]
    pub(crate) modules: Vec<Module>,
}

/// Something a unit's names refer to, as far as the unit can tell, shared by every
/// name that resolves to it
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Link {
    /// A harvest's own unit, which a typelib leaves implicit
    #[serde(skip, default = "local")]
    pub(crate) unit: UnitId,
    pub(crate) base: Target,
    /// The item a dotted name takes from the module `base` names
    pub(crate) item: Option<StrId>,
    /// The head of the first name referring to it, which a typelib omits
    #[serde(skip, default = "wire::invalid")]
    pub(crate) span: Span,
}

/// A name a module exports
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Export {
    pub(crate) name: StrId,
    /// The name it is bound by
    #[serde(with = "wire::span")]
    pub(crate) span: Span,
    pub(crate) target: Target,
}

/// A variable a module exports
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Value {
    /// Its name's
    #[serde(with = "wire::span")]
    pub(crate) span: Span,
    /// Its annotation, if it has one
    pub(crate) annot: Option<SiteId>,
}

/// The export of `exports`, sorted by name, that `name` names
pub(crate) fn find_export<'e>(
    exports: &'e [Export],
    strings: &[Cow<'_, str>],
    name: &str,
) -> Option<&'e Export> {
    exports
        .binary_search_by(|export| (*strings[export.name.index()]).cmp(name))
        .ok()
        .map(|index| &exports[index])
}

/// A module a unit imports
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Module {
    pub(crate) name: StrId,
    /// The module's name in the first import statement naming it, or the start of
    /// the unit for a module only the prelude imports
    #[serde(with = "wire::span")]
    pub(crate) span: Span,
}

/// Collect the surface of a unit, and resolve its type names as far as it can.
pub(crate) fn harvest<'u>(unit: &'u Unit<'u>) -> Harvest<'u> {
    let mut decls = Vec::new();
    let mut sites = Vec::new();
    let mut walk = Walk {
        unit: local(),
        file: &unit.file,
        prelude: &unit.prelude,
        decls: &mut decls,
        sites: &mut sites,
        strings: Vec::new(),
        interned: FxHashMap::default(),
        links: Vec::new(),
        linked: FxHashMap::default(),
        refs: FxHashMap::default(),
        modules: Vec::new(),
        module_ids: FxHashMap::default(),
        owner: None,
        sig: None,
        declared: FxHashMap::default(),
    };
    let root = &unit.ast.0;
    walk.function(None, root);
    let (exports, values) = match unit.mode {
        Mode::Module { .. } => walk.exports(&root.body.stmts),
        Mode::Script | Mode::Repl => (Vec::new(), Vec::new()),
    };
    // The prelude imports no statement writes, so a module it names is in the table
    // only when the unit uses it, and is reported at the start of the unit. A type
    // name using it already put it there.
    for import in unit.prelude.iter() {
        let (module, used) = match import {
            PreludeImport::Items { module, items } => (
                module,
                items
                    .iter()
                    .any(|item| (!item.type_only && !item.unused) || item.type_used),
            ),
            PreludeImport::ModuleAsIs { module, unused, .. }
            | PreludeImport::ModuleRenamed { module, unused, .. } => (module, !unused),
        };
        if used {
            walk.module(module, None);
        }
    }
    let Walk {
        strings,
        links,
        refs,
        modules,
        ..
    } = walk;
    let modules = modules
        .into_iter()
        .map(|(name, span)| Module {
            name,
            span: span.unwrap_or_default(),
        })
        .collect();
    Harvest {
        info: UnitInfo {
            module: match &unit.mode {
                Mode::Module { name } => Some(&**name),
                Mode::Script | Mode::Repl => None,
            },
            path: unit.file.path(),
            lines: Vec::new(),
            source: Some(unit),
            strict: unit.strict,
        },
        strings,
        decls,
        sites,
        links,
        refs,
        exports,
        values,
        modules,
    }
}

/// Link the units' harvests, by [`UnitId`], into the tables, allocating every
/// declaration in `db`, and resolve the type names that go through imports. The
/// units are linked in `order`, which fixes the order of declarations and
/// diagnostics. `pipes` nominates the types `strand`'s pipe placeholders stand for,
/// by module and item.
pub(crate) fn link<'u>(
    db: &mut Database,
    harvests: Vec<Harvest<'u>>,
    order: &[UnitId],
    pipes: [(&'u str, &'u str); 2],
) -> (Tables<'u>, Vec<UnitDiag>) {
    let count = harvests.len();
    let mut harvests: Vec<_> = harvests.into_iter().map(Some).collect();
    let mut shards: Vec<Option<Shard<'u>>> = (0..count).map(|_| None).collect();
    let mut bases = Vec::with_capacity(count);
    let mut links = vec![Vec::new(); count];
    let mut tables = vec![Vec::new(); count];
    let mut next = Bases {
        unit: local(),
        decls: 0,
        sites: 0,
        links: 0,
    };
    let mut leaks = Vec::new();
    for &id in order {
        let mut harvest = harvests[id.index()]
            .take()
            .expect("each unit is linked once");
        // A typelib leaves out private declarations, so only a unit with source can
        // name one in its interface
        if harvest.info.source.is_some() {
            let (_, found) = harvest.interface();
            leaks.extend(
                found
                    .iter()
                    .map(|leak| (id, Diag::new(PrivateInInterface::new(&harvest, leak)))),
            );
        }
        harvest.visit_ids(&mut Rebase {
            unit: id,
            decls: next.decls,
            sites: next.sites,
            links: next.links,
        });
        bases.push(Bases { unit: id, ..next });
        for _ in 0..harvest.decls.len() {
            db.allocate();
        }
        // The database declarations of a function's signatures other than the one
        // its own ID holds follow the unit's declarations
        let mut sig_decls = FxHashMap::default();
        let mut sig_owners = Vec::new();
        for (index, decl) in harvest.decls.iter().enumerate() {
            let decl_id = DeclId::from_index(next.decls + index);
            let primary = decl.node.primary_sig().unwrap_or(0);
            for sig in 0..decl.node.sig_count() {
                let declaration = if sig == primary {
                    decl_id
                } else {
                    sig_owners.push((decl_id, sig));
                    db.allocate()
                };
                sig_decls.insert((decl_id, sig), declaration);
            }
        }
        next.decls += harvest.decls.len() + sig_owners.len();
        next.sites += harvest.sites.len();
        next.links += harvest.links.len();
        // By name, to be found by name
        let mut sorted = harvest.exports;
        sorted
            .sort_by(|a, b| harvest.strings[a.name.index()].cmp(&harvest.strings[b.name.index()]));
        links[id.index()] = harvest.links;
        tables[id.index()] = harvest.modules;
        shards[id.index()] = Some(Shard {
            info: harvest.info,
            strings: harvest.strings,
            decls: harvest.decls,
            sites: harvest.sites,
            referents: Vec::new(),
            refs: harvest.refs,
            modules: Vec::new(),
            exports: sorted,
            values: harvest
                .values
                .into_iter()
                .map(|value| (value.span, value.annot))
                .collect(),
            sig_decls,
            sig_owners,
        });
    }
    let mut shards: Vec<Shard<'u>> = shards
        .into_iter()
        .map(|shard| shard.expect("every unit is linked"))
        .collect();

    let provided: FxHashMap<&str, UnitId> = shards
        .iter()
        .enumerate()
        .filter_map(|(index, shard)| Some((shard.info.module?, UnitId::from_index(index))))
        .collect();
    let mut diags = Vec::new();
    // A module a unit imports is reported at its import, whether or not anything
    // names it
    let resolved: Vec<_> = tables
        .iter()
        .enumerate()
        .map(|(index, modules)| {
            let unit = UnitId::from_index(index);
            let mut order: Vec<_> = (0..modules.len()).collect();
            order.sort_by_key(|&id| (modules[id].span.start, modules[id].span.end, id));
            let mut resolved = vec![None; modules.len()];
            for id in order {
                let Module { name, span } = modules[id];
                let name = &*shards[index].strings[name.index()];
                resolved[id] = provided.get(name).copied();
                if resolved[id].is_none() {
                    let module = name.into();
                    diags.push((unit, Diag::new(MissingModule { span, module })));
                }
            }
            resolved
        })
        .collect();
    let imports = tables;
    for (shard, resolved) in shards.iter_mut().zip(resolved) {
        shard.modules = resolved;
    }
    let mut tables = Tables::new(shards, bases);
    let mut fixup = Fixup {
        tables: &tables,
        imports: &imports,
        memo: FxHashMap::default(),
        stack: Vec::new(),
        diags,
    };
    // Each unit's links are resolved in the order the units were linked
    let referents: Vec<Vec<Referent>> = {
        let mut referents = vec![Vec::new(); count];
        for &id in order {
            referents[id.index()] = links[id.index()]
                .iter()
                .map(|link| fixup.link(link))
                .collect();
        }
        referents
    };
    // Each nominee is resolved as if its placeholder imported it. A placeholder
    // that isn't an opaque alias is diagnosed when designated.
    let mut nominees = FxHashMap::default();
    if let Some(&strand) = provided.get("strand") {
        for (placeholder, (module, item)) in PIPES.into_iter().zip(pipes) {
            if let Some(&Export {
                span,
                target: Target::Local(Referent::Decl(decl)),
                ..
            }) = fixup.tables.export(strand, placeholder)
                && fixup.tables.decl(decl).kind == DeclKind::OpaqueAlias
                // A nominee whose module isn't checked leaves its placeholder
                // dynamic; no unit imports it
                && let Some(&unit) = provided.get(module)
            {
                let site = UnitSpan { unit: strand, span };
                let nominee = fixup.export(unit, item, site);
                nominees.insert(placeholder, nominee);
            }
        }
    }
    let mut diags = fixup.diags;
    diags.extend(leaks);
    tables.nominees = nominees;
    for (shard, referents) in tables.shards.iter_mut().zip(referents) {
        shard.referents = referents;
    }

    let mut aliases = Aliases {
        tables: &tables,
        heads: FxHashMap::default(),
        diags: &mut diags,
    };
    // Every alias is followed in declaration order, so which one a cycle is reported
    // at does not depend on which alias names it first
    for id in tables.decl_ids() {
        if tables.decl(id).kind == DeclKind::Alias {
            aliases.head(id);
        }
    }
    let heads = aliases.heads;
    for (id, head) in heads {
        let head = head.expect("every alias is finished");
        tables.facts_mut(id).aliases.insert(id, head);
    }
    (tables, diags)
}

/// A name a lexical scope binds
#[derive(Clone)]
enum Entry<'u> {
    Target(Target),
    /// An item the prelude imports, whose module is in the module table only once a
    /// name refers to it
    Prelude {
        module: &'u str,
        item: &'u str,
    },
    /// Modules imported under one name. Modules are not nested, so `security.unix`
    /// and `security.nfs4` share the head they bind, and a dotted type name picks
    /// one by its whole path.
    Modules(Vec<ModuleImport<'u>>),
}

#[derive(Clone, Copy)]
struct ModuleImport<'u> {
    /// What a dotted type name spells to reach the module: its own path, or the name
    /// a renamed import binds
    spelled: &'u str,
    module: &'u str,
}

/// A frame as type resolution pushed one
struct Frame<'f, 'u> {
    outer: Option<&'f Frame<'f, 'u>>,
    kind: FrameKind<'u>,
}

enum FrameKind<'u> {
    /// The binders of one signature of a declaration
    Binders {
        decl: DeclId,
        sig: usize,
        binders: &'u [ast::Binder],
    },
    /// A lexical scope, with what each of its variables and type-only declarations
    /// names
    Scope {
        vars: &'u [Var],
        entries: Vec<Entry<'u>>,
        types: Vec<(Span, Entry<'u>)>,
        prelude: Vec<Entry<'u>>,
    },
}

impl<'u> Frame<'_, 'u> {
    /// What a type name resolved to `res` names, where `name` is the name's head.
    fn entry(&self, file: &File<'_>, res: TypeRes, name: &str) -> Option<Entry<'u>> {
        let mut frame = self;
        for _ in 0..res.depth {
            let Some(outer) = frame.outer else {
                debug_assert!(false, "{res:?} reaches past the outermost frame");
                return None;
            };
            frame = outer;
        }
        let found = match (&frame.kind, res.entry) {
            (FrameKind::Scope { prelude, .. }, TypeEntry::Prelude(index)) => {
                prelude.get(index).map(|entry| (None, entry.clone()))
            }
            (FrameKind::Binders { decl, sig, binders }, TypeEntry::Binder(slot)) => {
                binders.get(slot).map(|binder| {
                    let referent = Referent::Binder(BinderRef {
                        decl: *decl,
                        sig: *sig,
                        slot,
                    });
                    (
                        Some(binder.ident.span),
                        Entry::Target(Target::Local(referent)),
                    )
                })
            }
            (FrameKind::Scope { vars, entries, .. }, TypeEntry::Var(index)) => vars
                .get(index)
                .zip(entries.get(index))
                .map(|(var, entry)| (var.origin.name(), entry.clone())),
            (FrameKind::Scope { types, .. }, TypeEntry::Type(index)) => types
                .get(index)
                .map(|(span, entry)| (Some(*span), entry.clone())),
            _ => None,
        };
        let Some((span, entry)) = found else {
            debug_assert!(false, "{res:?} names no entry of its frame");
            return None;
        };
        // A prelude binding has no source name to compare
        if let Some(span) = span {
            debug_assert_eq!(file.str(span), name, "{res:?} names another entry");
        }
        Some(entry)
    }

    /// What the variable `res` resolves to names. Only lexical scopes count toward a
    /// variable's depth.
    fn value_entry(&self, res: Res) -> Option<Entry<'u>> {
        let mut depth = res.depth;
        let mut frame = self;
        loop {
            if let FrameKind::Scope { entries, .. } = &frame.kind {
                if depth == 0 {
                    return entries.get(res.index).cloned();
                }
                depth -= 1;
            }
            frame = frame.outer?;
        }
    }
}

/// What a block's statements declare, gathered as its scope is entered
struct BlockDecls<'u> {
    entries: Vec<Entry<'u>>,
    types: Vec<(Span, Entry<'u>)>,
    /// The function each name's defs share
    defs: FxHashMap<&'u str, DeclId>,
}

impl<'u> BlockDecls<'u> {
    fn var(&mut self, ident: &Ident, target: Target) {
        if let Some(res) = ident.res {
            self.var_res(res, Entry::Target(target));
        }
    }

    fn var_res(&mut self, res: Res, bound: Entry<'u>) {
        if res.depth == 0
            && let Some(entry) = self.entries.get_mut(res.index)
        {
            *entry = bound;
        }
    }

    fn module(&mut self, res: Option<Res>, import: ModuleImport<'u>) {
        let Some(Res {
            index, depth: 0, ..
        }) = res
        else {
            return;
        };
        let Some(entry) = self.entries.get_mut(index) else {
            return;
        };
        match entry {
            Entry::Modules(modules) => modules.push(import),
            Entry::Target(_) | Entry::Prelude { .. } => *entry = Entry::Modules(vec![import]),
        }
    }
}

struct Walk<'c, 'u> {
    unit: UnitId,
    file: &'u File<'u>,
    prelude: &'u [PreludeImport],
    decls: &'c mut Vec<Decl<'u>>,
    sites: &'c mut Vec<Site>,
    /// The unit's string table
    strings: Vec<Cow<'u, str>>,
    /// Each string of the unit's string table, by its text
    interned: FxHashMap<Cow<'u, str>, StrId>,
    links: Vec<Link>,
    /// Each link, by its target
    linked: FxHashMap<(Target, Option<StrId>), LinkId>,
    /// The link of each name, by its head
    refs: FxHashMap<Span, LinkId>,
    /// The module table: each module's name, at the first import statement naming
    /// it, if one does
    modules: Vec<(StrId, Option<Span>)>,
    /// Each module of the module table, by its name
    module_ids: FxHashMap<&'u str, ModuleId>,
    /// The declaration being walked, and which of its signatures, which encloses any
    /// found within it
    owner: Option<(DeclId, usize)>,
    /// The def or method signature whose ambient channels the types being walked
    /// share, absent outside a def's signature
    sig: Option<(DeclId, usize)>,
    /// The declarations of the unit's blocks, by the span of the name each declares,
    /// with the signature each def is among its function's
    declared: FxHashMap<Span, (DeclId, usize)>,
}

impl<'u> Walk<'_, 'u> {
    /// Allocate a declaration. Its surface is filled in as it is walked.
    fn allocate(
        &mut self,
        kind: DeclKind,
        name: Option<Span>,
        node: DeclNode,
        ast: DeclAst<'u>,
    ) -> DeclId {
        let id = DeclId::from_index(self.decls.len());
        let name = name.map(|span| self.name(span));
        self.decls.push(Decl {
            unit: self.unit,
            kind,
            name,
            node,
            outer: self.owner,
            ast: Some(ast),
        });
        id
    }

    fn node(&mut self, id: DeclId) -> &mut DeclNode {
        &mut self.decls[id.index()].node
    }

    /// Intern a string in the unit's string table.
    fn intern(&mut self, text: impl Into<Cow<'u, str>>) -> StrId {
        *self.interned.entry(text.into()).or_insert_with_key(|text| {
            self.strings.push(text.clone());
            StrId::from_index(self.strings.len() - 1)
        })
    }

    fn name(&mut self, span: Span) -> Name {
        Name {
            span,
            text: self.intern(self.file.str(span)),
        }
    }

    fn symbol(&mut self, span: Span) -> Symbol {
        self.name(span).into()
    }

    /// A name referring to something, which [`Self::refer`] has linked if it can,
    /// by its head and the fields dotted onto it
    fn reference(&mut self, head: Span, fields: &[Span]) -> Ref {
        Ref {
            span: fields.last().map_or(head, |field| head | field),
            link: self.refs.get(&head).copied(),
        }
    }

    /// The module table's entry for a module, written at `span` by an import
    /// statement
    fn module(&mut self, module: &'u str, span: Option<Span>) -> ModuleId {
        let id = match self.module_ids.get(module) {
            Some(&id) => id,
            None => {
                let name = self.intern(module);
                self.modules.push((name, None));
                let id = ModuleId::from_index(self.modules.len() - 1);
                self.module_ids.insert(module, id);
                id
            }
        };
        let written = &mut self.modules[id.index()].1;
        *written = written.or(span);
        id
    }

    /// An item of a module, imported at `span` by an import statement
    fn import(&mut self, module: &'u str, item: &'u str, span: Option<Span>) -> Target {
        Target::Import {
            module: self.module(module, span),
            item: self.intern(item),
        }
    }

    /// A type expression's surface
    fn surface(&mut self, ty: &'u ast::TypeExpr) -> TypeExpr {
        let span = ty.span();
        match ty {
            ast::TypeExpr::Name { head, fields, .. } => {
                TypeExpr::Name(self.reference(*head, fields))
            }
            ast::TypeExpr::Const { expr } => TypeExpr::Const {
                span,
                value: expr.fold(self.file).and_then(|value| self.const_lit(value)),
            },
            ast::TypeExpr::App { base, args, .. } => TypeExpr::App {
                span,
                base: Box::new(self.surface(base)),
                args: args.iter().map(|arg| self.surface_arg(arg)).collect(),
                interned: Interned::default(),
            },
            ast::TypeExpr::Schema {
                params, brace_span, ..
            } => TypeExpr::Schema {
                span,
                params: self.surface_params(params),
                tuple: brace_span.is_none() && !keyed(params),
                collection: Collection::default(),
            },
            ast::TypeExpr::Group { ty, .. } => TypeExpr::Group {
                span,
                ty: Box::new(self.surface(ty)),
            },
            ast::TypeExpr::Parens { params, .. } => {
                let keyed = keyed(params);
                let params = self.surface_params(params);
                match keyed {
                    true => TypeExpr::Record { span, params },
                    false => TypeExpr::Tuple { span, params },
                }
            }
            ast::TypeExpr::Array { elem, .. } => TypeExpr::Array {
                span,
                elem: Box::new(self.surface(elem)),
            },
            ast::TypeExpr::Union { members, .. } => TypeExpr::Union {
                span,
                members: members.iter().map(|member| self.surface(member)).collect(),
            },
            ast::TypeExpr::Func {
                params,
                input,
                output,
                arrow_span,
                ret,
                ..
            } => TypeExpr::Func {
                span,
                params: self.surface_params(params),
                input: input
                    .as_ref()
                    .map(|implicit| Box::new(self.surface(&implicit.ty))),
                output: output
                    .as_ref()
                    .map(|implicit| Box::new(self.surface(&implicit.ty))),
                arrow_span: *arrow_span,
                ret: Box::new(self.surface(ret)),
                interned: Interned::default(),
            },
            ast::TypeExpr::Error => TypeExpr::Error { span },
        }
    }

    fn surface_arg(&mut self, arg: &'u ast::TypeArg) -> surface::TypeArg {
        use ast::TypeArgKind as A;
        use surface::TypeArgKind;

        surface::TypeArg {
            kind: match &arg.kind {
                A::Pos(ty) => TypeArgKind::Pos(self.surface(ty)),
                A::Key { name, ty, .. } => TypeArgKind::Key {
                    name: self.symbol(*name),
                    ty: self.surface(ty),
                },
                A::Expand { ty, .. } => TypeArgKind::Expand {
                    ty: self.surface(ty),
                },
            },
        }
    }

    fn surface_params(&mut self, params: &'u [ast::TypeParam]) -> Vec<surface::TypeParam> {
        use ast::{TypeKey as K, TypeParamKind as P, TypeQuant as Q};
        use surface::{TypeKey, TypeParam, TypeQuant};

        let quant = |quant: &Option<Q>| {
            quant.as_ref().map(|quant| match quant {
                Q::Opt(_) => TypeQuant::Opt,
                Q::Star(_) => TypeQuant::Star,
                Q::Include(_) => TypeQuant::Include,
            })
        };
        params
            .iter()
            .map(|param| match &param.kind {
                P::Pos { quant: q, ty } => TypeParam::Pos {
                    quant: quant(q),
                    ty: self.surface(ty),
                },
                P::Key {
                    quant: q, key, ty, ..
                } => TypeParam::Key {
                    quant: quant(q),
                    key: match key {
                        K::Sym(span) => TypeKey::Sym(self.symbol(*span)),
                        K::Type(ty) => TypeKey::Type(Box::new(self.surface(ty))),
                    },
                    ty: self.surface(ty),
                },
                P::AnyPos { .. } => TypeParam::AnyPos,
                P::AnyKey { ty, .. } => TypeParam::AnyKey(ty.as_ref().map(|ty| self.surface(ty))),
                P::Open { ellipsis_span } => TypeParam::Open(*ellipsis_span),
            })
            .collect()
    }

    /// The literal a constant stands for as a type, if it can
    fn const_lit(&mut self, value: Const) -> Option<ConstLit> {
        Some(match value {
            Const::Str(value) => ConstLit::Str(self.intern(value)),
            Const::Int(value) => ConstLit::Int(value),
            Const::Bool(value) => ConstLit::Bool(value),
            Const::Nil => ConstLit::Nil,
            Const::Sym(span) => ConstLit::Sym(self.symbol(span)),
            Const::Bin(_) | Const::F64(_) | Const::Error => return None,
        })
    }

    fn declared(&self, name: Span) -> DeclId {
        self.declared[&name].0
    }

    /// Enter a lexical scope, declaring what its elements declare.
    fn scope<'f, T: Element>(
        &mut self,
        outer: Option<&'f Frame<'f, 'u>>,
        vars: &'u [Var],
        elems: &'u [T],
    ) -> Frame<'f, 'u> {
        let unit = self.unit;
        let mut block = BlockDecls {
            entries: vars
                .iter()
                .map(|var| {
                    Entry::Target(Target::Local(match var.origin.name() {
                        Some(span) => Referent::Value(UnitSpan { unit, span }),
                        None => Referent::Error,
                    }))
                })
                .collect(),
            types: Vec::new(),
            defs: FxHashMap::default(),
        };
        // Only the root scope has no outer frame, and the prelude binds into it
        if outer.is_none() {
            self.prelude(&mut block);
        }
        for elem in elems {
            elem.predeclare(self, &mut block);
        }
        Frame {
            outer,
            kind: FrameKind::Scope {
                vars,
                entries: block.entries,
                types: block.types,
                prelude: if outer.is_none() {
                    crate::prelude_items(self.prelude)
                        .map(|(module, item)| Entry::Prelude {
                            module,
                            item: &item.item,
                        })
                        .collect()
                } else {
                    Vec::new()
                },
            },
        }
    }

    fn prelude(&self, block: &mut BlockDecls<'u>) {
        for import in self.prelude {
            match import {
                PreludeImport::Items { module, items } => {
                    for item in items {
                        if let Some(res) = item.res {
                            block.var_res(
                                res,
                                Entry::Prelude {
                                    module,
                                    item: &item.item,
                                },
                            );
                        }
                    }
                }
                PreludeImport::ModuleAsIs { module, res, .. } => block.module(
                    *res,
                    ModuleImport {
                        spelled: module,
                        module,
                    },
                ),
                PreludeImport::ModuleRenamed {
                    module, bind, res, ..
                } => block.module(
                    *res,
                    ModuleImport {
                        spelled: bind,
                        module,
                    },
                ),
            }
        }
    }

    /// Declare what a statement declares in its block.
    fn predeclare(&mut self, stmt: &'u Stmt, block: &mut BlockDecls<'u>) {
        let file = self.file;
        match stmt {
            // The guarded statement declares its own type-only names
            Stmt::NlGuard(guard) => return self.predeclare(&guard.body, block),
            Stmt::Class(class) => {
                let kind = if class.is_protocol() {
                    DeclKind::Protocol
                } else {
                    DeclKind::Class
                };
                let node = DeclNode::Class(surface::Class {
                    binders: Vec::new(),
                    supers: Vec::new(),
                    members: Vec::new(),
                    decorators: Vec::new(),
                });
                let id = self.allocate(kind, Some(class.ident.span), node, DeclAst::Class(class));
                self.declared.insert(class.ident.span, (id, 0));
                block.var(&class.ident, Target::Local(Referent::Decl(id)));
            }
            Stmt::TypeAlias(alias) => {
                let kind = match alias.body {
                    AliasBody::Type(_) => DeclKind::Alias,
                    AliasBody::Opaque(_) => DeclKind::OpaqueAlias,
                };
                let node = DeclNode::Alias(surface::Alias {
                    binders: Vec::new(),
                    body: None,
                });
                let id = self.allocate(kind, Some(alias.ident.span), node, DeclAst::Alias(alias));
                self.declared.insert(alias.ident.span, (id, 0));
            }
            // A def's overloads and implementation are one function
            Stmt::Def(def) => {
                let (id, sig) = match block.defs.entry(file.str(def.ident.span)) {
                    MapEntry::Occupied(entry) => {
                        let id = *entry.get();
                        let Some(DeclAst::Defs(defs)) = &mut self.decls[id.index()].ast else {
                            unreachable!("a def groups with defs")
                        };
                        defs.push(def);
                        (id, defs.len() - 1)
                    }
                    MapEntry::Vacant(entry) => {
                        let id = self.allocate(
                            DeclKind::Function,
                            Some(def.ident.span),
                            DeclNode::Defs(Vec::new()),
                            DeclAst::Defs(vec![def]),
                        );
                        entry.insert(id);
                        (id, 0)
                    }
                };
                self.declared.insert(def.ident.span, (id, sig));
                if !def.is_type_only() {
                    block.var(&def.ident, Target::Local(Referent::Decl(id)));
                }
            }
            Stmt::Import(import) => {
                for element in &import.elements {
                    let (ImportElement::ModuleAsIs { module, .. }
                    | ImportElement::ModuleRenamed { module, .. }
                    | ImportElement::Items { module, .. }) = element;
                    self.module(file.str(*module), Some(*module));
                    match element {
                        ImportElement::ModuleAsIs {
                            module,
                            bind,
                            type_only: None,
                            ..
                        } => block.module(
                            bind.res,
                            ModuleImport {
                                spelled: file.str(*module),
                                module: file.str(*module),
                            },
                        ),
                        ImportElement::ModuleRenamed {
                            module,
                            bind,
                            type_only: None,
                            ..
                        } => block.module(
                            bind.res,
                            ModuleImport {
                                spelled: file.str(bind.span),
                                module: file.str(*module),
                            },
                        ),
                        ImportElement::Items { module, items } => {
                            for item in items.iter().filter(|item| !item.is_type_only()) {
                                let target =
                                    self.import(file.str(*module), file.str(item.item()), None);
                                block.var(item.bind(), target);
                            }
                        }
                        ImportElement::ModuleAsIs { .. } | ImportElement::ModuleRenamed { .. } => {}
                    }
                }
            }
            _ => {}
        }
        let mut decls = Vec::new();
        stmt.type_decls(&mut |decl| decls.push(decl));
        for decl in decls {
            let entry = match decl {
                TypeDecl::ModuleAsIs { module, .. } => Entry::Modules(vec![ModuleImport {
                    spelled: file.str(module),
                    module: file.str(module),
                }]),
                TypeDecl::ModuleRenamed { module, bind, .. } => {
                    Entry::Modules(vec![ModuleImport {
                        spelled: file.str(bind.span),
                        module: file.str(module),
                    }])
                }
                TypeDecl::ItemAsIs { module, bind, .. } => {
                    Entry::Target(self.import(file.str(module), file.str(bind.span), None))
                }
                TypeDecl::ItemRenamed { module, item, .. } => {
                    Entry::Target(self.import(file.str(module), file.str(item), None))
                }
                TypeDecl::Alias(_) | TypeDecl::Protocol(_) => {
                    Entry::Target(Target::Local(Referent::Decl(self.declared(decl.name()))))
                }
            };
            block.types.push((decl.name(), entry));
        }
    }

    /// Record what a type name refers to, given the entry its head resolved to.
    fn refer(&mut self, head: Span, entry: Entry<'u>, fields: &[Span]) {
        let file = self.file;
        let item = fields.last().map(|field| file.str(*field));
        let base = match entry {
            Entry::Target(target) => target,
            Entry::Prelude { module, item } => self.import(module, item, None),
            Entry::Modules(modules) => {
                let path: Vec<_> = iter::once(head)
                    .chain(fields.iter().copied())
                    .map(|span| file.str(span))
                    .collect();
                let module = match item {
                    None => modules.first(),
                    Some(_) => modules
                        .iter()
                        .find(|module| names_type_in(module.spelled, &path)),
                };
                match module {
                    Some(module) => Target::Module(self.module(module.module, None)),
                    // A prelude module constrains no dotted path, so a path may name
                    // a module that does not exist
                    None => Target::Local(Referent::Error),
                }
            }
        };
        let item = item.map(|item| self.intern(item));
        let link = match self.linked.entry((base, item)) {
            MapEntry::Occupied(entry) => *entry.get(),
            MapEntry::Vacant(entry) => {
                self.links.push(Link {
                    unit: self.unit,
                    base,
                    item,
                    span: head,
                });
                *entry.insert(LinkId::from_index(self.links.len() - 1))
            }
        };
        self.refs.insert(head, link);
    }

    fn name_ref(
        &mut self,
        frame: &Frame<'_, 'u>,
        head: Span,
        res: Option<TypeRes>,
        fields: &'u [Span],
    ) {
        // An unresolved name was diagnosed when it was resolved
        let Some(res) = res else { return };
        if let Some(entry) = frame.entry(self.file, res, self.file.str(head)) {
            self.refer(head, entry, fields);
        }
    }

    /// Record a site, resolving the names in its type.
    fn ty(&mut self, frame: &Frame<'_, 'u>, ty: &'u ast::TypeExpr, role: Role) -> SiteId {
        // The names are linked before the surface reads their links
        ty.names(&mut |head, res, fields| self.name_ref(frame, head, res, fields));
        let id = SiteId::from_index(self.sites.len());
        let surface = self.surface(ty);
        self.sites.push(Site {
            unit: self.unit,
            ty: surface,
            role,
            ambient: self.sig,
            owner: self.owner,
            interned: Interned::default(),
        });
        id
    }

    fn annot(
        &mut self,
        frame: &Frame<'_, 'u>,
        annot: &'u Option<Box<Annot>>,
        role: Role,
    ) -> Option<SiteId> {
        annot.as_ref().map(|annot| self.ty(frame, &annot.ty, role))
    }

    /// Walk a function, returning its signature.
    fn function(
        &mut self,
        outer: Option<&Frame<'_, 'u>>,
        func: &'u Function,
    ) -> surface::Signature {
        let frame = self.scope(outer, &func.body.vars, &func.body.stmts);
        let params = func
            .params
            .iter()
            .map(|param| self.param(&frame, param))
            .collect();
        let [input, output] = [&func.input, &func.output].map(|implicit| {
            implicit
                .as_ref()
                .map(|implicit| self.ty(&frame, &implicit.ty, Role::Type))
        });
        let ret = func
            .ret
            .as_ref()
            .map(|ret| self.ty(&frame, &ret.ty, Role::Type));
        // A def's channels are shared only by the types of its signature
        let sig = self.sig.take();
        for stmt in &func.body.stmts {
            self.stmt(&frame, stmt);
        }
        self.sig = sig;
        surface::Signature {
            params,
            input,
            output,
            ret,
        }
    }

    /// Enter the binder group of one signature of a declaration.
    fn binders<'f>(
        &mut self,
        frame: &'f Frame<'f, 'u>,
        decl: DeclId,
        sig: usize,
        binders: Option<&'u Binders>,
    ) -> (Frame<'f, 'u>, Vec<surface::Binder>) {
        let binders = binders.map_or(&[][..], |binders| &binders.binders);
        let group = Frame {
            outer: Some(frame),
            kind: FrameKind::Binders { decl, sig, binders },
        };
        let mut surfaces = Vec::new();
        for (slot, binder) in binders.iter().enumerate() {
            let binder_ref = BinderRef { decl, sig, slot };
            let bound = binder
                .bound
                .as_ref()
                .map(|bound| self.ty(&group, &bound.ty, Role::Bound(binder_ref)));
            let default = binder
                .default
                .as_ref()
                .map(|default| self.ty(&group, &default.ty, Role::Default(binder_ref)));
            surfaces.push(surface::Binder {
                kind: match binder.kind {
                    ast::BinderKind::Pos => surface::BinderKind::Pos,
                    ast::BinderKind::Key { .. } => surface::BinderKind::Key,
                    ast::BinderKind::Rest { kind, .. } => surface::BinderKind::Rest(kind),
                },
                name: self.name(binder.ident.span),
                bound,
                default,
            });
        }
        (group, surfaces)
    }

    /// Walk a def or method, one signature `sig` of the function `decl`, returning
    /// its binders and signature.
    fn def(
        &mut self,
        frame: &Frame<'_, 'u>,
        decl: DeclId,
        sig: usize,
        binders: Option<&'u Binders>,
        func: &'u Function,
    ) -> (Vec<surface::Binder>, surface::Signature) {
        let outer = self.sig.replace((decl, sig));
        let (group, binders) = self.binders(frame, decl, sig, binders);
        let owner = self.owner.replace((decl, sig));
        let signature = self.function(Some(&group), func);
        self.owner = owner;
        self.sig = outer;
        (binders, signature)
    }

    fn closure(&mut self, frame: &Frame<'_, 'u>, func: &'u Function) {
        let span = func.span();
        let empty = surface::Signature {
            params: Vec::new(),
            input: None,
            output: None,
            ret: None,
        };
        let id = self.allocate(
            DeclKind::Closure,
            None,
            DeclNode::Closure(surface::Closure { span, sig: empty }),
            DeclAst::Closure(func),
        );
        let owner = self.owner.replace((id, 0));
        let sig = self.function(Some(frame), func);
        self.owner = owner;
        *self.node(id) = DeclNode::Closure(surface::Closure { span, sig });
    }

    /// Walk a parameter's default, which like a body is outside its def's signature.
    fn default(&mut self, frame: &Frame<'_, 'u>, expr: &'u Expr) {
        let sig = self.sig.take();
        self.expr(frame, expr);
        self.sig = sig;
    }

    /// The name an item binds, walking a sub-pattern, which binds none of its own
    fn bind(&mut self, frame: &Frame<'_, 'u>, bind: &'u PatBind) -> Option<Name> {
        match bind {
            PatBind::Ident(ident) => Some(self.name(ident.span)),
            PatBind::Nested { pattern, .. } => {
                self.pattern(frame, pattern);
                None
            }
        }
    }

    /// Walk a parameter, of a signature or a pattern, returning its surface.
    fn param(&mut self, frame: &Frame<'_, 'u>, param: &'u PatItem) -> surface::Param {
        use surface::ParamKind;

        match param {
            PatItem::Pos { bind, ty, default } => {
                if let Some(default) = default {
                    self.default(frame, &default.expr);
                }
                surface::Param {
                    kind: ParamKind::Pos,
                    name: self.bind(frame, bind),
                    // `?` makes a sub-pattern optional, as a default does a name
                    default: default.is_some() || bind.optional().is_some(),
                    annot: self.annot(frame, ty, Role::Type),
                }
            }
            PatItem::Key {
                key_span,
                bind,
                ty,
                default,
                ..
            } => {
                if let Some(default) = default {
                    self.default(frame, &default.expr);
                }
                surface::Param {
                    kind: ParamKind::Key {
                        key: self.name(*key_span),
                    },
                    name: self.bind(frame, bind),
                    default: default.is_some() || bind.optional().is_some(),
                    annot: self.annot(frame, ty, Role::Type),
                }
            }
            PatItem::ConstKey {
                key_expr,
                key_const,
                bind,
                ty,
                default,
                ..
            } => {
                self.expr(frame, key_expr);
                if let Some(default) = default {
                    self.default(frame, &default.expr);
                }
                let key = match key_const {
                    Const::Str(value) => Some(ConstLit::Str(self.intern(value.as_str()))),
                    Const::Int(value) => Some(ConstLit::Int(*value)),
                    Const::Bool(value) => Some(ConstLit::Bool(*value)),
                    Const::Nil => Some(ConstLit::Nil),
                    Const::Sym(span) => Some(ConstLit::Sym(self.symbol(*span))),
                    Const::Bin(_) | Const::F64(_) | Const::Error => None,
                };
                surface::Param {
                    kind: ParamKind::ConstKey { key },
                    name: self.bind(frame, bind),
                    default: default.is_some() || bind.optional().is_some(),
                    annot: self.annot(frame, ty, Role::Type),
                }
            }
            PatItem::Rest {
                kind,
                ident,
                ty,
                type_ellipsis_span,
                ..
            } => {
                let role = match type_ellipsis_span {
                    Some(_) => Role::Pattern,
                    None => Role::Rest,
                };
                surface::Param {
                    kind: ParamKind::Rest {
                        kind: *kind,
                        pattern: type_ellipsis_span.is_some(),
                    },
                    name: ident.as_ref().map(|ident| self.name(ident.span)),
                    default: false,
                    annot: self.annot(frame, ty, role),
                }
            }
        }
    }

    fn pattern(&mut self, frame: &Frame<'_, 'u>, pattern: &'u Pattern) {
        match pattern {
            Pattern::Constant { .. } => {}
            Pattern::TypeTest(test) => {
                if let Some(res) = test.class.ident.res
                    && let Some(entry) = frame.value_entry(res)
                {
                    self.refer(test.class.ident.span, entry, &test.class.fields);
                }
                self.pattern(frame, &test.pattern);
            }
            Pattern::Ident(PatIdent { ty, .. }) => {
                self.annot(frame, ty, Role::Type);
            }
            Pattern::Unpack(params) => {
                for param in params {
                    self.param(frame, param);
                }
            }
            Pattern::Alt(alt) => {
                for pattern in &alt.alts {
                    self.pattern(frame, pattern);
                }
            }
        }
    }

    fn stmt(&mut self, frame: &Frame<'_, 'u>, stmt: &'u Stmt) {
        match stmt {
            Stmt::NlGuard(guard) => self.stmt(frame, &guard.body),
            Stmt::Prim(prim) => self.prim(frame, prim),
            Stmt::Let(node) => {
                self.prim(frame, &node.rhs);
                if let Some((body, _)) = &node.else_branch {
                    self.branch(frame, &**body, None);
                }
                self.pattern(frame, &node.bind);
            }
            Stmt::Bind(node) => {
                self.expr(frame, &node.expr);
                if let Some((body, _)) = &node.else_branch {
                    self.branch(frame, &**body, None);
                }
                self.pattern(frame, &node.bind);
            }
            Stmt::Assign(node) => {
                self.lvalue(frame, &node.lhs);
                self.prim(frame, &node.rhs);
            }
            Stmt::Import(_) | Stmt::Break(..) | Stmt::Continue(..) => {}
            Stmt::TypeAlias(alias) => {
                let id = self.declared(alias.ident.span);
                let outer = self.sig.take();
                let (group, binders) = self.binders(frame, id, 0, alias.binders.as_deref());
                let body = match &alias.body {
                    AliasBody::Type(ty) => Some(self.ty(&group, ty, Role::Alias(id))),
                    AliasBody::Opaque(_) => None,
                };
                *self.node(id) = DeclNode::Alias(surface::Alias { binders, body });
                self.sig = outer;
            }
            Stmt::Def(def) => {
                for decorator in &def.decorators {
                    self.expr(frame, &decorator.expr);
                }
                let (id, sig) = self.declared[&def.ident.span];
                let (binders, signature) =
                    self.def(frame, id, sig, def.binders.as_deref(), &def.func);
                let name = self.name(def.ident.span);
                let DeclNode::Defs(defs) = self.node(id) else {
                    unreachable!("a def is collected into defs")
                };
                debug_assert_eq!(defs.len(), sig, "defs are walked in order");
                defs.push(surface::Def {
                    name,
                    binders,
                    sig: signature,
                    type_only: def.is_type_only(),
                });
            }
            Stmt::Class(class) => self.class(frame, class),
            Stmt::Return(ret) => {
                if let Some(expr) = &ret.expr {
                    self.expr(frame, expr);
                }
            }
            Stmt::Throw(node) => self.expr(frame, &node.expr),
            Stmt::While(node) => {
                self.expr(frame, &node.expr);
                self.branch(
                    frame,
                    &node.body,
                    node.bind.as_ref().map(|bind| &bind.pattern),
                );
            }
            Stmt::For(node) => {
                if let Some(expr) = &node.expr {
                    self.expr(frame, expr);
                }
                self.branch(frame, &node.body, Some(&node.bind));
            }
        }
    }

    fn class(&mut self, frame: &Frame<'_, 'u>, class: &'u Class) {
        let mut decorators = Vec::new();
        for decorator in &class.decorators {
            self.expr(frame, &decorator.expr);
            // Only std's `final` is recognized so far
            if let Expr::Ident(ident) = &decorator.expr
                && let Some(res) = ident.res
                && let Some(entry) = frame.value_entry(res)
            {
                self.refer(ident.span, entry, &[]);
            }
            decorators.push(match &decorator.expr {
                Expr::Ident(ident) => surface::Decorator::Ident(self.reference(ident.span, &[])),
                _ => surface::Decorator::Other,
            });
        }
        let id = self.declared(class.ident.span);
        let outer = self.sig.take();
        let (group, binders) = self.binders(frame, id, 0, class.binders.as_deref());
        let owner = self.owner.replace((id, 0));
        let mut supers = Vec::new();
        for super_ref in &class.super_refs {
            if super_ref.type_only {
                self.name_ref(
                    &group,
                    super_ref.ident.span,
                    super_ref.res,
                    &super_ref.fields,
                );
            } else if let Some(res) = super_ref.ident.res
                && let Some(entry) = group.value_entry(res)
            {
                // A supertype that exists at runtime is a value elaboration resolved
                self.refer(super_ref.ident.span, entry, &super_ref.fields);
            }
            let mut args = Vec::new();
            for arg in &super_ref.args {
                // Checked with the supertype, whose binders they fill
                arg.ty()
                    .names(&mut |head, res, fields| self.name_ref(&group, head, res, fields));
                args.push(self.surface_arg(arg));
            }
            supers.push(surface::Super {
                head: self.reference(super_ref.ident.span, &super_ref.fields),
                args,
                bracket_span: super_ref.bracket_span,
                type_only: super_ref.type_only,
                interned: Interned::default(),
            });
        }
        // A method's overload signatures are one function with its first
        // implementation, as a def's are. Each other implementation of the name is a
        // function of its own, as a getter and a setter are.
        let file = self.file;
        let mut grouped: FxHashMap<_, Vec<&'u Method>> = FxHashMap::default();
        let mut names = Vec::new();
        for member in &class.body.members {
            let ClassMember::Method(method) = member else {
                continue;
            };
            let key = (method.special.is_some(), file.str(method.name_span));
            grouped
                .entry(key)
                .or_insert_with(|| {
                    names.push(key);
                    Vec::new()
                })
                .push(method);
        }
        let mut sigs = FxHashMap::default();
        for key in names {
            let methods = &grouped[&key];
            let first = methods.iter().position(|method| method.at_span.is_none());
            let mut overloaded = Vec::new();
            let mut single = Vec::new();
            for (index, &method) in methods.iter().enumerate() {
                if method.at_span.is_some() || first.is_none_or(|first| first == index) {
                    overloaded.push(method);
                } else {
                    single.push(vec![method]);
                }
            }
            for methods in iter::once(overloaded).chain(single) {
                let id = self.allocate(
                    DeclKind::Function,
                    Some(methods[0].name_span),
                    DeclNode::Methods(Vec::new()),
                    DeclAst::Methods(methods.clone()),
                );
                for (sig, method) in methods.into_iter().enumerate() {
                    sigs.insert(method.name_span, (id, sig));
                }
            }
        }
        let mut members = Vec::new();
        for member in &class.body.members {
            match member {
                ClassMember::Method(method) => {
                    let (id, sig) = sigs[&method.name_span];
                    let mut decorators = Vec::new();
                    let mut scope = surface::MemberScope::Instance;
                    for decorator in &method.decorators {
                        self.expr(&group, &decorator.expr);
                        // `class` and `static` say the method's namespace, by syntax
                        if let Expr::Ident(ident) = &decorator.expr {
                            let namespace = match self.file.str(ident.span) {
                                "class" => Some(surface::MemberScope::Class),
                                "static" => Some(surface::MemberScope::Static),
                                _ => None,
                            };
                            if let Some(namespace) = namespace {
                                scope = namespace;
                                continue;
                            }
                        }
                        // Only std's `getter` and `setter` are recognized so far
                        if let Expr::Ident(ident) = &decorator.expr
                            && let Some(res) = ident.res
                            && let Some(entry) = group.value_entry(res)
                        {
                            self.refer(ident.span, entry, &[]);
                        }
                        decorators.push(match &decorator.expr {
                            Expr::Ident(ident) => {
                                surface::Decorator::Ident(self.reference(ident.span, &[]))
                            }
                            _ => surface::Decorator::Other,
                        });
                    }
                    let (binders, signature) =
                        self.def(&group, id, sig, method.binders.as_deref(), &method.func);
                    let name = self.name(method.name_span);
                    let DeclNode::Methods(methods) = self.node(id) else {
                        unreachable!("a method is collected into methods")
                    };
                    debug_assert_eq!(methods.len(), sig, "methods are walked in order");
                    methods.push(surface::Method {
                        name,
                        binders,
                        sig: signature,
                        overload: method.at_span.is_some(),
                        special: method.special,
                        public: method.pub_span.is_some(),
                        scope,
                        decorators,
                    });
                    members.push(surface::Member::Method { decl: id, sig });
                }
                ClassMember::Field(field) => {
                    for decorator in &field.decorators {
                        self.expr(&group, &decorator.expr);
                    }
                    match &field.init {
                        FieldInit::None => {}
                        FieldInit::Expr(expr) | FieldInit::Const(expr, _) => {
                            self.expr(&group, expr)
                        }
                        FieldInit::Thunk(func) => self.closure(&group, func),
                    }
                    let annot = self.annot(&group, &field.ty, Role::Type);
                    members.push(surface::Member::Field(surface::Field {
                        names: field
                            .fields
                            .iter()
                            .map(|name| self.name(name.ident.span))
                            .collect(),
                        annot,
                        public: field.pub_span.is_some(),
                        scope: match field.scope {
                            ast::MemberScope::Instance => surface::MemberScope::Instance,
                            ast::MemberScope::Class => surface::MemberScope::Class,
                            ast::MemberScope::Static => surface::MemberScope::Static,
                        },
                    }));
                }
            }
        }
        *self.node(id) = DeclNode::Class(surface::Class {
            binders,
            supers,
            members,
            decorators,
        });
        self.owner = owner;
        self.sig = outer;
    }

    fn prim(&mut self, frame: &Frame<'_, 'u>, prim: &'u PrimStmt) {
        match prim {
            PrimStmt::Expr(expr) => self.expr(frame, expr),
            PrimStmt::If(node) => self.if_body(frame, node),
            PrimStmt::Match(node) => {
                self.expr(frame, &node.scrutinee);
                for arm in &node.arms {
                    let inner = self.scope(Some(frame), &arm.body.vars, &arm.body.stmts);
                    self.pattern(&inner, &arm.pattern);
                    if let Some(guard) = &arm.guard {
                        self.expr(&inner, &guard.expr);
                        if let Some(bind) = &guard.bind {
                            self.pattern(&inner, &bind.pattern);
                        }
                    }
                    for stmt in &arm.body.stmts {
                        stmt.walk(self, &inner);
                    }
                }
                if let Some((body, _)) = &node.else_branch {
                    self.branch(frame, body, None);
                }
            }
            PrimStmt::Try(node) => {
                self.function(Some(frame), &node.body);
                for handler in &node.handlers {
                    if let Some(expr) = &handler.class_expr {
                        self.expr(frame, expr);
                    }
                    self.function(Some(frame), &handler.func);
                }
                if let Some((func, _)) = &node.finally {
                    self.function(Some(frame), func);
                }
            }
        }
    }

    fn branch<T: Body>(
        &mut self,
        frame: &Frame<'_, 'u>,
        body: &'u T,
        pattern: Option<&'u Pattern>,
    ) {
        let (vars, elems) = body.parts();
        let inner = self.scope(Some(frame), vars, elems);
        if let Some(pattern) = pattern {
            self.pattern(&inner, pattern);
        }
        for elem in elems {
            elem.walk(self, &inner);
        }
    }

    fn if_body<T: Body>(&mut self, frame: &Frame<'_, 'u>, node: &'u If<T>) {
        for branch in iter::once(&node.tbranch).chain(node.elif_branches.iter().map(|(b, _)| b)) {
            self.expr(frame, &branch.expr);
            self.branch(
                frame,
                &branch.body,
                branch.bind.as_ref().map(|bind| &bind.pattern),
            );
        }
        if let Some((body, _)) = &node.else_branch {
            self.branch(frame, body, None);
        }
    }

    fn for_elem<T: Element>(&mut self, frame: &Frame<'_, 'u>, node: &'u For<ExprBody<T>>) {
        if let Some(expr) = &node.expr {
            self.expr(frame, expr);
        }
        self.branch(frame, &node.body, Some(&node.bind));
    }

    fn lvalue(&mut self, frame: &Frame<'_, 'u>, value: &'u LValue) {
        match value {
            LValue::Ident(_) => {}
            LValue::Field { object, .. } | LValue::PrivateField { object, .. } => {
                self.expr(frame, object)
            }
            LValue::Index { exprs, .. } => {
                for expr in exprs.iter() {
                    self.expr(frame, expr);
                }
            }
        }
    }

    fn expr(&mut self, frame: &Frame<'_, 'u>, expr: &'u Expr) {
        match expr {
            Expr::Group { expr, .. } | Expr::Unary { expr, .. } => self.expr(frame, expr),
            Expr::Cast { expr, annot, .. } => {
                self.expr(frame, expr);
                self.ty(frame, &annot.ty, Role::Type);
            }
            Expr::Binary { exprs, .. } | Expr::Index { exprs, .. } => {
                for expr in exprs.iter() {
                    self.expr(frame, expr);
                }
            }
            Expr::Logical { exprs, .. } => {
                for expr in exprs {
                    self.expr(frame, expr);
                }
            }
            Expr::Range { exprs, .. } => {
                for expr in exprs.iter().flatten() {
                    self.expr(frame, expr);
                }
            }
            Expr::Lambda { func, .. } => self.closure(frame, func),
            Expr::Call { arg0, args, .. } => {
                self.expr(frame, arg0);
                // What a class's `(init)` is called on, which lowering compares with
                // the superclasses of an `(init)` making the call
                if let Expr::Get {
                    object,
                    field:
                        GetVariant::SpecialMethod {
                            method: SpecialMethod::Init,
                            ..
                        },
                    ..
                } = &**arg0
                    && let Some((head, fields)) = path(object)
                    && let Some(res) = head.res
                    && let Some(entry) = frame.value_entry(res)
                {
                    self.refer(head.span, entry, &fields);
                }
                for arg in args {
                    arg.walk(self, frame);
                }
            }
            Expr::Get { object, .. } => self.expr(frame, object),
            Expr::Array { elems, .. } | Expr::Tuple { elems, .. } => {
                for elem in elems {
                    elem.walk(self, frame);
                }
            }
            Expr::Record { args, .. } => {
                for arg in args {
                    arg.walk(self, frame);
                }
            }
            Expr::Dict { elems, .. } => {
                for elem in elems {
                    elem.walk(self, frame);
                }
            }
            Expr::Concat { exprs, .. }
            | Expr::FmtSeq { exprs, .. }
            | Expr::BinConcat { exprs, .. } => {
                for expr in exprs.iter() {
                    self.expr(frame, expr);
                }
            }
            Expr::Fmt { value, spec, .. } => {
                self.expr(frame, value);
                for expr in [&spec.width, &spec.precision].into_iter().flatten() {
                    self.expr(frame, expr);
                }
            }
            Expr::FmtParam { spec, .. } => {
                for expr in [&spec.width, &spec.precision].into_iter().flatten() {
                    self.expr(frame, expr);
                }
            }
            Expr::Ident(_)
            | Expr::Escape(..)
            | Expr::EscapeByte(..)
            | Expr::Stub(_)
            | Expr::Literal(_)
            | Expr::Int(..)
            | Expr::VerbatimInt(..)
            | Expr::F64(..)
            | Expr::VerbatimF64(..)
            | Expr::Bool(..)
            | Expr::Nil(_)
            | Expr::Sym(_)
            | Expr::Error => {}
        }
    }

    /// The names a module's root block exports, with the name each is bound by, and
    /// the exported variables, with the site of each one's annotation.
    fn exports(&mut self, stmts: &'u [Stmt]) -> (Vec<Export>, Vec<Value>) {
        let mut exports = Vec::new();
        let mut annots = FxHashMap::default();
        for stmt in stmts {
            self.export(stmt, &mut exports, &mut annots);
        }
        // The root block's annotations, by the span of the type each writes
        let sites: FxHashMap<Span, SiteId> = self
            .sites
            .iter()
            .enumerate()
            .filter(|(_, site)| site.owner.is_none() && site.role == Role::Type)
            .map(|(index, site)| (site.ty.span(), SiteId::from_index(index)))
            .collect();
        let mut values: Vec<_> = exports
            .iter()
            .filter_map(|export| match export.target {
                Target::Local(Referent::Value(value)) => Some(value.span),
                _ => None,
            })
            .map(|span| {
                let annot = annots.get(&span).and_then(|ty| sites.get(ty).copied());
                Value { span, annot }
            })
            .collect();
        values.sort_by_key(|value| (value.span.start, value.span.end));
        (exports, values)
    }

    /// Add a statement's exports. `annots` collects the span of each exported
    /// variable's annotation, by the span of its name.
    fn export(
        &mut self,
        stmt: &'u Stmt,
        exports: &mut Vec<Export>,
        annots: &mut FxHashMap<Span, Span>,
    ) {
        let file = self.file;
        // A name exported again is exported as it was last bound
        let add = |walk: &mut Self, exports: &mut Vec<Export>, span: Span, target| {
            let export = Export {
                name: walk.intern(file.str(span)),
                span,
                target,
            };
            match exports.iter_mut().find(|other| other.name == export.name) {
                Some(other) => *other = export,
                None => exports.push(export),
            }
        };
        let decl = |walk: &mut Self, exports: &mut Vec<Export>, span| {
            let target = Target::Local(Referent::Decl(walk.declared(span)));
            add(walk, exports, span, target)
        };
        match stmt {
            Stmt::NlGuard(guard) => self.export(&guard.body, exports, annots),
            // Overloads are exported with their implementation, whose function they are
            Stmt::Def(def) if def.pub_span.is_some() => decl(self, exports, def.ident.span),
            Stmt::Class(class) if class.pub_span.is_some() => decl(self, exports, class.ident.span),
            Stmt::TypeAlias(alias) if alias.pub_span.is_some() => {
                decl(self, exports, alias.ident.span)
            }
            Stmt::Let(node) if node.pub_span.is_some() => {
                let mut value = |ident: &Ident, annot: Option<&Annot>| {
                    let value = Referent::Value(UnitSpan {
                        unit: self.unit,
                        span: ident.span,
                    });
                    add(self, exports, ident.span, Target::Local(value));
                    if let Some(annot) = annot {
                        annots.insert(ident.span, annot.ty.span());
                    }
                };
                pattern_names(&node.bind, &mut value);
            }
            // A public import re-exports what it binds
            Stmt::Import(import) if import.pub_span.is_some() => {
                for element in &import.elements {
                    match element {
                        ImportElement::ModuleAsIs { module, bind, .. }
                        | ImportElement::ModuleRenamed { module, bind, .. } => {
                            let target = Target::Module(self.module(file.str(*module), None));
                            add(self, exports, bind.span, target)
                        }
                        ImportElement::Items { module, items } => {
                            for item in items {
                                let target =
                                    self.import(file.str(*module), file.str(item.item()), None);
                                add(self, exports, item.bind().span, target);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Whether items written as data are keyed: whether one is explicitly keyed, which
/// makes parenthesized items a record and vertical ones a dict. An inclusion isn't,
/// so the form's meaning never depends on what it includes.
fn keyed(params: &[ast::TypeParam]) -> bool {
    params.iter().any(|param| {
        matches!(
            param.kind,
            ast::TypeParamKind::Key { .. } | ast::TypeParamKind::AnyKey { .. }
        )
    })
}

/// Call `f` on each name a pattern binds, at any level, with its annotation if it
/// has one that types it alone
fn pattern_names(pattern: &Pattern, f: &mut impl FnMut(&Ident, Option<&Annot>)) {
    let params = match pattern {
        Pattern::Constant { .. } => return,
        Pattern::TypeTest(test) => return pattern_names(&test.pattern, f),
        Pattern::Ident(PatIdent { ident, ty, .. }) => return f(ident, ty.as_deref()),
        Pattern::Unpack(params) => params,
        // Every alternative binds the same names
        Pattern::Alt(alt) => return pattern_names(&alt.alts[0], f),
    };
    for param in params {
        match param {
            PatItem::Pos { bind, ty, .. }
            | PatItem::Key { bind, ty, .. }
            | PatItem::ConstKey { bind, ty, .. } => match bind {
                PatBind::Ident(ident) => f(ident, ty.as_deref()),
                PatBind::Nested { pattern, .. } => pattern_names(pattern, f),
            },
            PatItem::Rest {
                ident: Some(ident), ..
            } => f(ident, None),
            PatItem::Rest { ident: None, .. } => {}
        }
    }
}

/// Resolution of names that go through an import, once every unit is collected
struct Fixup<'a, 'u> {
    /// The tables, whose links are not yet resolved
    tables: &'a Tables<'u>,
    /// Each unit's module table
    imports: &'a [Vec<Module>],
    memo: FxHashMap<(UnitId, &'a str), Referent>,
    /// The exports being resolved, outermost first
    stack: Vec<(UnitId, &'a str)>,
    diags: Vec<UnitDiag>,
}

impl<'a, 'u> Fixup<'a, 'u> {
    fn diag(&mut self, unit: UnitId, info: impl super::Report + Send + 'static) {
        self.diags.push((unit, Diag::new(info)));
    }

    fn module_name(&self, unit: UnitId) -> &'u str {
        self.tables.info(unit).module.expect("only modules export")
    }

    fn str(&self, unit: UnitId, id: StrId) -> &'a str {
        self.tables.str(unit, id)
    }

    fn link(&mut self, link: &Link) -> Referent {
        let base = self.target(link.unit, link.base, link.span);
        let Some(item) = link.item else {
            return base;
        };
        match base {
            Referent::Module(unit) => {
                let site = self.site(link.unit, link.base, link.span);
                self.export(unit, self.str(link.unit, item), site)
            }
            // Only a module has items, which resolving the name has warned about,
            // and a missing module is reported with its unit's module table
            _ => Referent::Error,
        }
    }

    /// Where a target of `unit`, named at `span`, is reported as unresolved: at
    /// `span`, or in a unit without source, which keeps no reference's span, at the
    /// import of the module the target goes through.
    fn site(&self, unit: UnitId, target: Target, span: Span) -> UnitSpan {
        let span = match target {
            Target::Import { module, .. } | Target::Module(module)
                if self.tables.info(unit).source.is_none() =>
            {
                self.imports[unit.index()][module.index()].span
            }
            _ => span,
        };
        UnitSpan { unit, span }
    }

    /// Resolve a target of `unit`, named at `span`. A module no unit provides is
    /// reported with the unit's module table.
    fn target(&mut self, unit: UnitId, target: Target, span: Span) -> Referent {
        match target {
            Target::Local(referent) => referent,
            Target::Module(module) => match self.tables.shard(unit).modules[module.index()] {
                Some(found) => Referent::Module(found),
                None => Referent::Error,
            },
            Target::Import { module, item } => {
                match self.tables.shard(unit).modules[module.index()] {
                    Some(found) => {
                        let site = self.site(unit, target, span);
                        self.export(found, self.str(unit, item), site)
                    }
                    None => Referent::Error,
                }
            }
        }
    }

    /// Resolve the export `item` of `unit`, imported at `site`.
    fn export(&mut self, unit: UnitId, item: &'a str, site: UnitSpan) -> Referent {
        if let Some(&referent) = self.memo.get(&(unit, item)) {
            return referent;
        }
        if let Some(start) = self.stack.iter().position(|&key| key == (unit, item)) {
            let chain = self.stack[start..]
                .iter()
                .map(|&(unit, item)| format!("`{}.{item}`", self.module_name(unit)))
                .collect::<Vec<_>>()
                .join(", ");
            self.diag(
                site.unit,
                ImportCycle {
                    span: site.span,
                    chain,
                },
            );
            return Referent::Error;
        }
        let Some(&Export { span, target, .. }) = self.tables.export(unit, item) else {
            let module = self.module_name(unit).into();
            self.diag(
                site.unit,
                MissingExport {
                    span: site.span,
                    module,
                    item: item.into(),
                },
            );
            return Referent::Error;
        };
        self.stack.push((unit, item));
        let referent = self.target(unit, target, span);
        self.stack.pop();
        self.memo.insert((unit, item), referent);
        referent
    }
}

/// The heads of transparent aliases, found by following each alias's chain
struct Aliases<'a, 'u> {
    tables: &'a Tables<'u>,
    /// Each alias's head, or `None` while its chain is being followed
    heads: FxHashMap<DeclId, Option<Head>>,
    diags: &'a mut Vec<UnitDiag>,
}

impl Aliases<'_, '_> {
    fn head(&mut self, id: DeclId) -> Head {
        let decl = self.tables.decl(id);
        match self.heads.get(&id) {
            Some(Some(head)) => return head.clone(),
            // Every alias on the cycle takes the error this returns, so it is
            // reported once
            Some(None) => {
                let span = decl.name_span().expect("an alias is named");
                self.diags.push((decl.unit, Diag::new(AliasCycle(span))));
                return Head::Error;
            }
            None => {}
        }
        self.heads.insert(id, None);
        let DeclNode::Alias(alias) = &decl.node else {
            unreachable!("only an alias has a head")
        };
        let body = alias.body.expect("an opaque alias is not transparent");
        let head = self.ty(&self.tables.site(body).ty);
        self.heads.insert(id, Some(head.clone()));
        head
    }

    fn ty(&mut self, ty: &TypeExpr) -> Head {
        match ty {
            TypeExpr::Group { ty, .. } => self.ty(ty),
            TypeExpr::App { base, .. } => self.ty(base),
            TypeExpr::Name(head) => match self.tables.linked(head.link) {
                Some(Referent::Decl(decl)) if self.tables.decl(*decl).kind == DeclKind::Alias => {
                    self.head(*decl)
                }
                Some(Referent::Decl(decl)) => Head::Decl(*decl),
                Some(Referent::Binder(binder)) => Head::Binder(*binder),
                Some(Referent::Module(_) | Referent::Value(_) | Referent::Error) | None => {
                    Head::Error
                }
            },
            TypeExpr::Const { .. }
            | TypeExpr::Union { .. }
            | TypeExpr::Func { .. }
            | TypeExpr::Schema { .. }
            | TypeExpr::Tuple { .. }
            | TypeExpr::Record { .. }
            | TypeExpr::Array { .. } => Head::Structural,
            TypeExpr::Error { .. } => Head::Error,
        }
    }
}

/// The body of a construct that opens a scope, whether statements or the elements of
/// a literal
trait Body {
    type Element: Element;
    fn parts(&self) -> (&[Var], &[Self::Element]);
}

impl Body for Block {
    type Element = Stmt;
    fn parts(&self) -> (&[Var], &[Stmt]) {
        (&self.vars, &self.stmts)
    }
}

impl<T: Element> Body for ExprBody<T> {
    type Element = T;
    fn parts(&self) -> (&[Var], &[T]) {
        (&self.vars, &self.elems)
    }
}

trait Element {
    fn walk<'u>(&'u self, walk: &mut Walk<'_, 'u>, frame: &Frame<'_, 'u>);

    /// Declare what the element declares in its enclosing block.
    fn predeclare<'u>(&'u self, _walk: &mut Walk<'_, 'u>, _block: &mut BlockDecls<'u>) {}
}

impl Element for Stmt {
    fn walk<'u>(&'u self, walk: &mut Walk<'_, 'u>, frame: &Frame<'_, 'u>) {
        walk.stmt(frame, self);
    }

    fn predeclare<'u>(&'u self, walk: &mut Walk<'_, 'u>, block: &mut BlockDecls<'u>) {
        walk.predeclare(self, block);
    }
}

impl Element for Arg {
    fn walk<'u>(&'u self, walk: &mut Walk<'_, 'u>, frame: &Frame<'_, 'u>) {
        match self {
            Self::Pos(node) => walk.expr(frame, &node.expr),
            Self::Key(node) => walk.expr(frame, &node.expr),
            Self::Expand(node) => walk.expr(frame, &node.expr),
            Self::DynamicKey(node) => {
                walk.expr(frame, &node.key);
                walk.expr(frame, &node.value);
            }
            Self::For(node) => walk.for_elem(frame, node),
            Self::If(node) => walk.if_body(frame, node),
        }
    }
}

impl Element for ArrayElem {
    fn walk<'u>(&'u self, walk: &mut Walk<'_, 'u>, frame: &Frame<'_, 'u>) {
        match self {
            Self::Single(node) => walk.expr(frame, &node.expr),
            Self::Expand(node) => walk.expr(frame, &node.expr),
            Self::For(node) => walk.for_elem(frame, node),
            Self::If(node) => walk.if_body(frame, node),
        }
    }
}

impl Element for DictElem {
    fn walk<'u>(&'u self, walk: &mut Walk<'_, 'u>, frame: &Frame<'_, 'u>) {
        match self {
            Self::Single(node) => walk.expr(frame, &node.expr),
            Self::Key(node) => walk.expr(frame, &node.expr),
            Self::Pair(node) => {
                walk.expr(frame, &node.key);
                walk.expr(frame, &node.value);
            }
            Self::Expand(node) => walk.expr(frame, &node.expr),
            Self::For(node) => walk.for_elem(frame, node),
            Self::If(node) => walk.if_body(frame, node),
        }
    }
}

/// A name and the fields after it, as in `a.b.c`
fn path(mut expr: &Expr) -> Option<(&Ident, Vec<Span>)> {
    let mut fields = Vec::new();
    let head = loop {
        match expr {
            Expr::Get {
                object,
                field: GetVariant::Normal(span),
                ..
            } => {
                fields.push(*span);
                expr = object;
            }
            Expr::Group { expr: inner, .. } => expr = inner,
            Expr::Ident(ident) => break ident,
            _ => return None,
        }
    };
    fields.reverse();
    Some((head, fields))
}
