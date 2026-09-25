//! Collect every declaration of the checked units, and resolve their type names to
//! what they refer to, chasing imports across units.
//!
//! Each unit is walked with the frames type resolution pushed, binder groups and
//! lexical scopes, so a name's [`TypeRes`] nominates its target directly. Names that
//! go through an import are fixed up once every unit's exports are known.

use std::{
    collections::{HashMap, hash_map::Entry as MapEntry},
    iter,
};

use super::{
    AliasCycle, BinderRef, Decl, DeclNode, Head, ImportCycle, MissingExport, ModuleRef, Referent,
    Role, Site, Tables, Target,
};
use crate::{
    Mode, PreludeImport, Unit,
    ast::{
        AliasBody, Annot, Arg, ArrayElem, Binder, Binders, Block, Class, ClassMember, DictElem,
        Expr, ExprBody, FieldInit, For, Function, Ident, If, ImportElement, LValue, Param,
        PatIdent, Pattern, PrimStmt, Res, Stmt, TypeDecl, TypeEntry, TypeExpr, TypeRes, Var,
        implicits,
    },
    resolvety::names_type_in,
    source::{self, Diagnose, File, Span},
    typeck::r#type::{Database, DeclId, DeclKind, UnitId, UnitSpan},
};

/// A diagnostic, with the unit whose source it points into
pub(crate) type UnitDiag = (UnitId, source::Diag);

/// Collect the declarations of `units`, allocating each in `db`, and resolve every
/// type name in them. The units are walked in `order`, which fixes the order of
/// declarations and diagnostics.
pub(crate) fn collect<'u>(
    db: &mut Database,
    units: &[&'u Unit<'u>],
    order: &[UnitId],
) -> (Tables<'u>, Vec<UnitDiag>) {
    let mut decls = Vec::new();
    let mut pending = Vec::new();
    let mut sites = Vec::new();
    let mut exports = vec![HashMap::new(); units.len()];
    for &id in order {
        let unit = units[id.index()];
        let mut walk = Walk {
            unit: id,
            file: &unit.compiler.file,
            prelude: &unit.compiler.prelude,
            db: &mut *db,
            decls: &mut decls,
            pending: &mut pending,
            sites: &mut sites,
            owner: None,
            sig: None,
            declared: HashMap::new(),
        };
        let root = &unit.ast.0;
        walk.function(None, root);
        if let Mode::Module { .. } = unit.compiler.mode {
            exports[id.index()] = walk.exports(&root.body.stmts);
        }
    }

    let mut fixup = Fixup {
        units,
        modules: units
            .iter()
            .enumerate()
            .filter_map(|(index, unit)| match unit.compiler.mode {
                Mode::Module { name } => Some((name, UnitId::from_index(index))),
                Mode::Script | Mode::Repl => None,
            })
            .collect(),
        exports: &exports,
        memo: HashMap::new(),
        stack: Vec::new(),
        diags: Vec::new(),
    };
    let referents: HashMap<_, _> = pending
        .into_iter()
        .map(|pending| (pending.head, fixup.pending(&pending)))
        .collect();
    let mut diags = fixup.diags;

    let mut aliases = Aliases {
        decls: &decls,
        referents: &referents,
        heads: HashMap::new(),
        diags: &mut diags,
    };
    // Every alias is followed in declaration order, so which one a cycle is reported
    // at does not depend on which alias names it first
    for (index, decl) in decls.iter().enumerate() {
        if decl.kind == DeclKind::Alias {
            aliases.head(DeclId::from_index(index));
        }
    }
    let aliases = aliases
        .heads
        .into_iter()
        .map(|(id, head)| (id, head.expect("every alias is finished")))
        .collect();

    let tables = Tables {
        units: units.to_vec(),
        decls,
        referents,
        aliases,
        exports,
        sites,
        binder_kinds: HashMap::new(),
        alias_kinds: HashMap::new(),
        sigs: HashMap::new(),
        fields: HashMap::new(),
        func_ambients: HashMap::new(),
        designated: HashMap::new(),
        variance: HashMap::new(),
        captured: HashMap::new(),
    };
    (tables, diags)
}

/// A name a lexical scope binds
#[derive(Clone)]
enum Entry<'u> {
    Target(Target<'u>),
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
        binders: &'u [Binder],
    },
    /// A lexical scope, with what each of its variables and type-only declarations
    /// names
    Scope {
        vars: &'u [Var],
        entries: Vec<Entry<'u>>,
        types: Vec<(Span, Entry<'u>)>,
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

/// A type name whose referent is found once every unit is collected
struct Pending<'u> {
    head: UnitSpan,
    base: Target<'u>,
    /// The item a dotted name takes from the module `base` names
    item: Option<&'u str>,
}

/// What a block's statements declare, gathered as its scope is entered
struct BlockDecls<'u> {
    entries: Vec<Entry<'u>>,
    types: Vec<(Span, Entry<'u>)>,
    /// The function each name's defs share
    defs: HashMap<&'u str, DeclId>,
}

impl<'u> BlockDecls<'u> {
    fn var(&mut self, ident: &Ident, target: Target<'u>) {
        if let Some(res) = ident.res {
            self.var_res(res, target);
        }
    }

    fn var_res(&mut self, res: Res, target: Target<'u>) {
        if res.depth == 0
            && let Some(entry) = self.entries.get_mut(res.index)
        {
            *entry = Entry::Target(target);
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
            Entry::Target(_) => *entry = Entry::Modules(vec![import]),
        }
    }
}

struct Walk<'c, 'u> {
    unit: UnitId,
    file: &'u File<'u>,
    prelude: &'u [PreludeImport],
    db: &'c mut Database,
    decls: &'c mut Vec<Decl<'u>>,
    pending: &'c mut Vec<Pending<'u>>,
    sites: &'c mut Vec<Site<'u>>,
    /// The declaration being walked, and which of its signatures, which encloses any
    /// found within it
    owner: Option<(DeclId, usize)>,
    /// The def or method signature whose ambient channels the types being walked
    /// share, absent outside any def or within a class or alias declared in one
    sig: Option<(DeclId, usize)>,
    /// The declarations of the unit's blocks, by the span of the name each declares,
    /// with the signature each def is among its function's
    declared: HashMap<Span, (DeclId, usize)>,
}

impl<'u> Walk<'_, 'u> {
    fn allocate(&mut self, kind: DeclKind, name: Option<Span>, node: DeclNode<'u>) -> DeclId {
        let id = self.db.allocate();
        debug_assert_eq!(id.index(), self.decls.len());
        self.decls.push(Decl {
            unit: self.unit,
            kind,
            name,
            node,
            outer: self.owner,
        });
        id
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
            defs: HashMap::new(),
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
                                Target::Import {
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
                let id = self.allocate(kind, Some(class.ident.span), DeclNode::Class(class));
                self.declared.insert(class.ident.span, (id, 0));
                block.var(&class.ident, Target::Local(Referent::Decl(id)));
            }
            Stmt::TypeAlias(alias) => {
                let kind = match alias.body {
                    AliasBody::Type(_) => DeclKind::Alias,
                    AliasBody::Opaque(_) => DeclKind::OpaqueAlias,
                };
                let id = self.allocate(kind, Some(alias.ident.span), DeclNode::Alias(alias));
                self.declared.insert(alias.ident.span, (id, 0));
            }
            // A def's overloads and implementation are one function
            Stmt::Def(def) => {
                let (id, sig) = match block.defs.entry(file.str(def.ident.span)) {
                    MapEntry::Occupied(entry) => {
                        let id = *entry.get();
                        let DeclNode::Defs(defs) = &mut self.decls[id.index()].node else {
                            unreachable!("a def groups with defs")
                        };
                        defs.push(def);
                        (id, defs.len() - 1)
                    }
                    MapEntry::Vacant(entry) => {
                        let id = self.allocate(
                            DeclKind::Function,
                            Some(def.ident.span),
                            DeclNode::Defs(vec![def]),
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
                                block.var(
                                    item.bind(),
                                    Target::Import {
                                        module: file.str(*module),
                                        item: file.str(item.item()),
                                    },
                                );
                            }
                        }
                        ImportElement::ModuleAsIs { .. } | ImportElement::ModuleRenamed { .. } => {}
                    }
                }
            }
            _ => {}
        }
        let declared = &self.declared;
        stmt.type_decls(&mut |decl| {
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
                TypeDecl::ItemAsIs { module, bind, .. } => Entry::Target(Target::Import {
                    module: file.str(module),
                    item: file.str(bind.span),
                }),
                TypeDecl::ItemRenamed { module, item, .. } => Entry::Target(Target::Import {
                    module: file.str(module),
                    item: file.str(item),
                }),
                TypeDecl::Alias(_) | TypeDecl::Protocol(_) => {
                    Entry::Target(Target::Local(Referent::Decl(declared[&decl.name()].0)))
                }
            };
            block.types.push((decl.name(), entry));
        });
    }

    /// Record what a type name refers to, given the entry its head resolved to.
    fn refer(&mut self, head: Span, entry: Entry<'u>, fields: &'u [Span]) {
        let file = self.file;
        let item = fields.last().map(|field| file.str(*field));
        let base = match entry {
            Entry::Target(target) => target,
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
                    Some(module) => Target::Module(module.module),
                    // A prelude module constrains no dotted path, so a path may name
                    // a module that does not exist
                    None => Target::Local(Referent::Error),
                }
            }
        };
        self.pending.push(Pending {
            head: UnitSpan {
                unit: self.unit,
                span: head,
            },
            base,
            item,
        });
    }

    fn name(
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

    fn ty(&mut self, frame: &Frame<'_, 'u>, ty: &'u TypeExpr, role: Role) {
        self.sites.push(Site {
            unit: self.unit,
            ty,
            role,
            ambient: self.sig,
        });
        ty.names(&mut |head, res, fields| self.name(frame, head, res, fields));
    }

    fn annot(&mut self, frame: &Frame<'_, 'u>, annot: &'u Option<Box<Annot>>, role: Role) {
        if let Some(annot) = annot {
            self.ty(frame, &annot.ty, role);
        }
    }

    fn function(&mut self, outer: Option<&Frame<'_, 'u>>, func: &'u Function) {
        let frame = self.scope(outer, &func.body.vars, &func.body.stmts);
        for param in &func.params {
            self.param(&frame, param);
        }
        for implicit in implicits(&func.input, &func.output) {
            self.ty(&frame, &implicit.ty, Role::Type);
        }
        if let Some(ret) = &func.ret {
            self.ty(&frame, &ret.ty, Role::Type);
        }
        for stmt in &func.body.stmts {
            self.stmt(&frame, stmt);
        }
    }

    /// Enter the binder group of one signature of a declaration.
    fn binders<'f>(
        &mut self,
        frame: &'f Frame<'f, 'u>,
        decl: DeclId,
        sig: usize,
        binders: Option<&'u Binders>,
    ) -> Frame<'f, 'u> {
        let binders = binders.map_or(&[][..], |binders| &binders.binders);
        let group = Frame {
            outer: Some(frame),
            kind: FrameKind::Binders { decl, sig, binders },
        };
        for (slot, binder) in binders.iter().enumerate() {
            let binder_ref = BinderRef { decl, sig, slot };
            if let Some(bound) = &binder.bound {
                self.ty(&group, &bound.ty, Role::Bound(binder_ref));
            }
            if let Some(default) = &binder.default {
                self.ty(&group, &default.ty, Role::Default(binder_ref));
            }
        }
        group
    }

    /// Walk a def or method, one signature `sig` of the function `decl`.
    fn def(
        &mut self,
        frame: &Frame<'_, 'u>,
        decl: DeclId,
        sig: usize,
        binders: Option<&'u Binders>,
        func: &'u Function,
    ) {
        let outer = self.sig.replace((decl, sig));
        let group = self.binders(frame, decl, sig, binders);
        let owner = self.owner.replace((decl, sig));
        self.function(Some(&group), func);
        self.owner = owner;
        self.sig = outer;
    }

    fn closure(&mut self, frame: &Frame<'_, 'u>, func: &'u Function) {
        let id = self.allocate(DeclKind::Closure, None, DeclNode::Closure(func));
        let owner = self.owner.replace((id, 0));
        self.function(Some(frame), func);
        self.owner = owner;
    }

    fn param(&mut self, frame: &Frame<'_, 'u>, param: &'u Param) {
        match param {
            Param::Pos { ty, default, .. } | Param::Key { ty, default, .. } => {
                if let Some(default) = default {
                    self.expr(frame, &default.expr);
                }
                self.annot(frame, ty, Role::Type);
            }
            Param::ConstKey {
                key_expr,
                ty,
                default,
                ..
            } => {
                self.expr(frame, key_expr);
                if let Some(default) = default {
                    self.expr(frame, &default.expr);
                }
                self.annot(frame, ty, Role::Type);
            }
            Param::Rest {
                ty,
                type_ellipsis_span,
                ..
            } => {
                let role = match type_ellipsis_span {
                    Some(_) => Role::Pattern,
                    None => Role::Rest,
                };
                self.annot(frame, ty, role);
            }
        }
    }

    fn pattern(&mut self, frame: &Frame<'_, 'u>, pattern: &'u Pattern) {
        match pattern {
            Pattern::Ident(PatIdent { ty, .. }) => self.annot(frame, ty, Role::Type),
            Pattern::Unpack(params) => {
                for param in params {
                    self.param(frame, param);
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
                self.pattern(frame, &node.bind);
            }
            Stmt::Bind(node) => {
                self.expr(frame, &node.expr);
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
                let group = self.binders(frame, id, 0, alias.binders.as_deref());
                if let AliasBody::Type(ty) = &alias.body {
                    self.ty(&group, ty, Role::Alias(id));
                }
                self.sig = outer;
            }
            Stmt::Def(def) => {
                for decorator in &def.decorators {
                    self.expr(frame, &decorator.expr);
                }
                let (id, sig) = self.declared[&def.ident.span];
                self.def(frame, id, sig, def.binders.as_deref(), &def.func);
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
        for decorator in &class.decorators {
            self.expr(frame, &decorator.expr);
        }
        let id = self.declared(class.ident.span);
        let outer = self.sig.take();
        let group = self.binders(frame, id, 0, class.binders.as_deref());
        let owner = self.owner.replace((id, 0));
        for super_ref in &class.super_refs {
            if super_ref.type_only {
                self.name(
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
            for arg in &super_ref.args {
                // Checked with the supertype, whose binders they fill
                arg.ty()
                    .names(&mut |head, res, fields| self.name(&group, head, res, fields));
            }
        }
        // The methods of a name are one function, as are the overloads of a def
        let file = self.file;
        let mut methods: HashMap<_, DeclId> = HashMap::new();
        let mut sigs = Vec::new();
        for member in &class.body.members {
            let ClassMember::Method(method) = member else {
                continue;
            };
            let key = (method.special.is_some(), file.str(method.name_span));
            sigs.push(match methods.entry(key) {
                MapEntry::Occupied(entry) => {
                    let id = *entry.get();
                    let DeclNode::Methods(methods) = &mut self.decls[id.index()].node else {
                        unreachable!("a method groups with methods")
                    };
                    methods.push(method);
                    (id, methods.len() - 1)
                }
                MapEntry::Vacant(entry) => {
                    let id = self.allocate(
                        DeclKind::Function,
                        Some(method.name_span),
                        DeclNode::Methods(vec![method]),
                    );
                    entry.insert(id);
                    (id, 0)
                }
            });
        }
        let mut sigs = sigs.into_iter();
        for member in &class.body.members {
            match member {
                ClassMember::Method(method) => {
                    let (id, sig) = sigs.next().expect("a signature for each method");
                    for decorator in &method.decorators {
                        self.expr(&group, &decorator.expr);
                    }
                    self.def(&group, id, sig, method.binders.as_deref(), &method.func);
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
                    self.annot(&group, &field.ty, Role::Type);
                }
            }
        }
        self.owner = owner;
        self.sig = outer;
    }

    fn prim(&mut self, frame: &Frame<'_, 'u>, prim: &'u PrimStmt) {
        match prim {
            PrimStmt::Expr(expr) => self.expr(frame, expr),
            PrimStmt::If(node) => self.if_body(frame, node),
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
            Expr::Binary { exprs, .. } | Expr::Index { exprs, .. } => {
                for expr in exprs.iter() {
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

    /// The names a module's root block exports, with the name each is bound by.
    fn exports(&self, stmts: &'u [Stmt]) -> HashMap<&'u str, (Span, Target<'u>)> {
        let mut exports = HashMap::new();
        for stmt in stmts {
            self.export(stmt, &mut exports);
        }
        exports
    }

    fn export(&self, stmt: &'u Stmt, exports: &mut HashMap<&'u str, (Span, Target<'u>)>) {
        let file = self.file;
        let mut add = |span: Span, target| {
            exports.insert(file.str(span), (span, target));
        };
        let decl = |span| Target::Local(Referent::Decl(self.declared(span)));
        match stmt {
            Stmt::NlGuard(guard) => self.export(&guard.body, exports),
            // Overloads are exported with their implementation, whose function they are
            Stmt::Def(def) if def.pub_span.is_some() => add(def.ident.span, decl(def.ident.span)),
            Stmt::Class(class) if class.pub_span.is_some() => {
                add(class.ident.span, decl(class.ident.span))
            }
            Stmt::TypeAlias(alias) if alias.pub_span.is_some() => {
                add(alias.ident.span, decl(alias.ident.span))
            }
            Stmt::Let(node) if node.pub_span.is_some() => {
                let mut value = |ident: &Ident| {
                    let value = Referent::Value(UnitSpan {
                        unit: self.unit,
                        span: ident.span,
                    });
                    add(ident.span, Target::Local(value));
                };
                match &node.bind {
                    Pattern::Ident(PatIdent { ident, .. }) => value(ident),
                    Pattern::Unpack(params) => {
                        for param in params {
                            match param {
                                Param::Pos { ident, .. }
                                | Param::Key { ident, .. }
                                | Param::ConstKey { ident, .. }
                                | Param::Rest {
                                    ident: Some(ident), ..
                                } => value(ident),
                                Param::Rest { ident: None, .. } => {}
                            }
                        }
                    }
                }
            }
            // A public import re-exports what it binds
            Stmt::Import(import) if import.pub_span.is_some() => {
                for element in &import.elements {
                    match element {
                        ImportElement::ModuleAsIs { module, bind, .. }
                        | ImportElement::ModuleRenamed { module, bind, .. } => {
                            add(bind.span, Target::Module(file.str(*module)))
                        }
                        ImportElement::Items { module, items } => {
                            for item in items {
                                add(
                                    item.bind().span,
                                    Target::Import {
                                        module: file.str(*module),
                                        item: file.str(item.item()),
                                    },
                                );
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Resolution of names that go through an import, once every unit is collected
struct Fixup<'a, 'u> {
    units: &'a [&'u Unit<'u>],
    modules: HashMap<&'u str, UnitId>,
    exports: &'a [HashMap<&'u str, (Span, Target<'u>)>],
    memo: HashMap<(UnitId, &'u str), Referent>,
    /// The exports being resolved, outermost first
    stack: Vec<(UnitId, &'u str)>,
    diags: Vec<UnitDiag>,
}

impl<'u> Fixup<'_, 'u> {
    fn diag(&mut self, unit: UnitId, info: impl Diagnose + 'static) {
        self.diags.push((unit, source::Diag::new(info)));
    }

    fn module_name(&self, unit: UnitId) -> &'u str {
        match self.units[unit.index()].compiler.mode {
            Mode::Module { name } => name,
            Mode::Script | Mode::Repl => unreachable!("only modules export"),
        }
    }

    fn pending(&mut self, pending: &Pending<'u>) -> Referent {
        let base = self.target(&pending.base, pending.head);
        let Some(item) = pending.item else {
            return base;
        };
        match base {
            Referent::Module(ModuleRef::Unit(unit)) => self.export(unit, item, pending.head),
            Referent::Module(ModuleRef::External(module)) => Referent::External {
                module,
                item: item.into(),
            },
            // Only a module has items, which resolving the name has warned about
            _ => Referent::Error,
        }
    }

    /// Resolve a target, found at `site`.
    fn target(&mut self, target: &Target<'u>, site: UnitSpan) -> Referent {
        match target {
            Target::Local(referent) => referent.clone(),
            Target::Module(module) => Referent::Module(match self.modules.get(module) {
                Some(unit) => ModuleRef::Unit(*unit),
                None => ModuleRef::External((*module).into()),
            }),
            Target::Import { module, item } => match self.modules.get(module) {
                Some(unit) => self.export(*unit, item, site),
                None => Referent::External {
                    module: (*module).into(),
                    item: (*item).into(),
                },
            },
        }
    }

    /// Resolve the export `item` of `unit`, imported at `site`.
    fn export(&mut self, unit: UnitId, item: &'u str, site: UnitSpan) -> Referent {
        if let Some(referent) = self.memo.get(&(unit, item)) {
            return referent.clone();
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
        let Some((span, target)) = self.exports[unit.index()].get(item) else {
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
        let (span, target) = (*span, target.clone());
        self.stack.push((unit, item));
        let referent = self.target(&target, UnitSpan { unit, span });
        self.stack.pop();
        self.memo.insert((unit, item), referent.clone());
        referent
    }
}

/// The heads of transparent aliases, found by following each alias's chain
struct Aliases<'a, 'u> {
    decls: &'a [Decl<'u>],
    referents: &'a HashMap<UnitSpan, Referent>,
    /// Each alias's head, or `None` while its chain is being followed
    heads: HashMap<DeclId, Option<Head>>,
    diags: &'a mut Vec<UnitDiag>,
}

impl Aliases<'_, '_> {
    fn head(&mut self, id: DeclId) -> Head {
        let decl = &self.decls[id.index()];
        match self.heads.get(&id) {
            Some(Some(head)) => return head.clone(),
            // Every alias on the cycle takes the error this returns, so it is
            // reported once
            Some(None) => {
                let span = decl.name.expect("an alias is named");
                self.diags
                    .push((decl.unit, source::Diag::new(AliasCycle(span))));
                return Head::Error;
            }
            None => {}
        }
        self.heads.insert(id, None);
        let DeclNode::Alias(alias) = decl.node else {
            unreachable!("only an alias has a head")
        };
        let head = match &alias.body {
            AliasBody::Type(ty) => self.ty(decl.unit, ty),
            AliasBody::Opaque(_) => unreachable!("an opaque alias is not transparent"),
        };
        self.heads.insert(id, Some(head.clone()));
        head
    }

    fn ty(&mut self, unit: UnitId, ty: &TypeExpr) -> Head {
        match ty {
            TypeExpr::Group { ty, .. } => self.ty(unit, ty),
            TypeExpr::App { base, .. } => self.ty(unit, base),
            TypeExpr::Name { head, .. } => {
                match self.referents.get(&UnitSpan { unit, span: *head }) {
                    Some(Referent::Decl(decl))
                        if self.decls[decl.index()].kind == DeclKind::Alias =>
                    {
                        self.head(*decl)
                    }
                    Some(Referent::Decl(decl)) => Head::Decl(*decl),
                    Some(Referent::Binder(binder)) => Head::Binder(*binder),
                    Some(Referent::External { module, item }) => Head::External {
                        module: module.clone(),
                        item: item.clone(),
                    },
                    Some(Referent::Module(_) | Referent::Value(_) | Referent::Error) | None => {
                        Head::Error
                    }
                }
            }
            TypeExpr::Const { .. }
            | TypeExpr::Union { .. }
            | TypeExpr::Func { .. }
            | TypeExpr::Schema { .. } => Head::Structural,
            TypeExpr::Error => Head::Error,
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
