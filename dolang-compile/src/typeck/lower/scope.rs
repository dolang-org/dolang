//! Lexical frames, which mirror the resolver's scopes so that a [`Res`] can be
//! decoded, and the declarations lowering finds by node.

use std::{ptr, rc::Rc};

use super::{Ctx, Lower, Scope};
use crate::{
    Mode, PreludeImport,
    ast::{self, Class, Def, Function, Ident, ImportElement, Method, Res, Stmt},
    typeck::{
        cfg::{FuncId, Origin, VarId},
        r#type::TypeId,
        r#type::UnitSpan,
    },
};

/// A declaration node, by address. The node's type is kept, since a node and its
/// first field can share an address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum DeclKey {
    Class(*const Class),
    Def(*const Def),
    Method(*const Method),
    /// A lambda or field initializer
    Closure(*const Function),
}

impl DeclKey {
    pub(super) fn class(class: &Class) -> Self {
        Self::Class(ptr::from_ref(class))
    }

    pub(super) fn def(def: &Def) -> Self {
        Self::Def(ptr::from_ref(def))
    }

    pub(super) fn method(method: &Method) -> Self {
        Self::Method(ptr::from_ref(method))
    }

    pub(super) fn closure(func: &Function) -> Self {
        Self::Closure(ptr::from_ref(func))
    }
}

/// A resolver scope: a function body, a statement block, a comprehension body or a
/// try part
pub(super) struct Frame<'u> {
    parent: Option<Rc<Frame<'u>>>,
    entries: Vec<Entry<'u>>,
    /// For a function's outermost frame, the context the function was created in
    pub(super) origin: Option<Ctx<'u>>,
}

impl<'u> Frame<'u> {
    pub(super) fn parent(&self) -> Option<&Frame<'u>> {
        self.parent.as_deref()
    }

    /// The variables it declares
    pub(super) fn vars(&self) -> impl Iterator<Item = VarId> {
        self.entries.iter().filter_map(|entry| match entry {
            Entry::Var(var) | Entry::Item { var: Some(var), .. } => Some(*var),
            Entry::Modules(_) | Entry::Item { var: None, .. } => None,
        })
    }

    fn entry(&self, res: Res) -> Option<&Entry<'u>> {
        let mut frame = self;
        for _ in 0..res.depth {
            frame = frame.parent.as_deref()?;
        }
        frame.entries.get(res.index)
    }
}

/// What a variable of a scope stands for
#[derive(Clone, Debug)]
pub(super) enum Entry<'u> {
    Var(VarId),
    /// Modules imported under one name, each with how a path spells it
    Modules(Vec<Spelled<'u>>),
    /// An item imported from a module. An import statement binds it to a variable
    /// it assigns; the prelude binds it to none.
    Item {
        module: &'u str,
        item: &'u str,
        var: Option<VarId>,
    },
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Spelled<'u> {
    pub(super) spelled: &'u str,
    pub(super) module: &'u str,
}

impl<'u> Lower<'_, 'u> {
    /// Enter a scope of `func`, allocating its variables. Its statements' module
    /// imports bind names that aren't variables, as does the prelude in the root
    /// scope.
    pub(super) fn frame(
        &self,
        func: FuncId,
        parent: Option<Rc<Frame<'u>>>,
        vars: &'u [ast::Var],
        stmts: &'u [Stmt],
        origin: Option<Ctx<'u>>,
    ) -> Rc<Frame<'u>> {
        let mut entries: Vec<Option<Entry<'u>>> = vec![None; vars.len()];
        for stmt in stmts {
            self.imports(func, stmt, &mut entries);
        }
        if parent.is_none() {
            self.prelude(&mut entries);
        }
        let unit = self.tables.units[self.unit.index()]
            .source
            .expect("only a unit with source is lowered");
        let module = parent.is_none() && matches!(unit.mode, Mode::Module { .. });
        let entries = entries
            .into_iter()
            .zip(vars)
            .map(|(entry, var)| {
                entry.unwrap_or_else(|| {
                    let origin = match var.origin {
                        ast::Origin::Source(span)
                        | ast::Origin::SelfParam(span)
                        | ast::Origin::Import(span) => Origin::Source(span),
                        _ => Origin::Synthetic,
                    };
                    let id = self.graph.alloc_var(func, origin, None);
                    if module && var.exported {
                        let mut data = self.graph.var_mut(id);
                        data.exported = true;
                        data.interprocedural = true;
                        data.volatile = true;
                    }
                    Entry::Var(id)
                })
            })
            .collect();
        Rc::new(Frame {
            parent,
            entries,
            origin,
        })
    }

    fn imports(&self, func: FuncId, stmt: &'u Stmt, entries: &mut [Option<Entry<'u>>]) {
        let import = match stmt {
            Stmt::NlGuard(guard) => return self.imports(func, &guard.body, entries),
            Stmt::Import(import) => import,
            _ => return,
        };
        for element in &import.elements {
            match element {
                ImportElement::ModuleAsIs {
                    module,
                    bind,
                    type_only: None,
                    ..
                } => {
                    let module = self.text(*module);
                    module_entry(
                        entries,
                        bind.res,
                        Spelled {
                            spelled: module,
                            module,
                        },
                    );
                }
                ImportElement::ModuleRenamed {
                    module,
                    bind,
                    type_only: None,
                    ..
                } => module_entry(
                    entries,
                    bind.res,
                    Spelled {
                        spelled: self.text(bind.span),
                        module: self.text(*module),
                    },
                ),
                ImportElement::Items { module, items } => {
                    // Never exported, even by a public import: an importer reaches
                    // the item through the module that exports it
                    for item in items.iter().filter(|item| !item.is_type_only()) {
                        let bind = item.bind();
                        let Some(Res {
                            index, depth: 0, ..
                        }) = bind.res
                        else {
                            continue;
                        };
                        let Some(slot) = entries.get_mut(index) else {
                            continue;
                        };
                        let var = self.graph.alloc_var(func, Origin::Source(bind.span), None);
                        *slot = Some(Entry::Item {
                            module: self.text(*module),
                            item: self.text(item.item()),
                            var: Some(var),
                        });
                    }
                }
                ImportElement::ModuleAsIs { .. } | ImportElement::ModuleRenamed { .. } => {}
            }
        }
    }

    fn prelude(&self, entries: &mut [Option<Entry<'u>>]) {
        let unit = self.tables.units[self.unit.index()]
            .source
            .expect("only a unit with source is lowered");
        for import in &unit.prelude {
            match import {
                PreludeImport::Items { module, items } => {
                    for item in items {
                        set_entry(
                            entries,
                            item.res,
                            Entry::Item {
                                module,
                                item: &item.item,
                                var: None,
                            },
                        );
                    }
                }
                PreludeImport::ModuleAsIs { module, res, .. } => module_entry(
                    entries,
                    *res,
                    Spelled {
                        spelled: module,
                        module,
                    },
                ),
                PreludeImport::ModuleRenamed {
                    module, bind, res, ..
                } => module_entry(
                    entries,
                    *res,
                    Spelled {
                        spelled: bind,
                        module,
                    },
                ),
            }
        }
    }
}

fn set_entry<'u>(entries: &mut [Option<Entry<'u>>], res: Option<Res>, entry: Entry<'u>) {
    if let Some(Res {
        index, depth: 0, ..
    }) = res
        && let Some(slot) = entries.get_mut(index)
    {
        *slot = Some(entry);
    }
}

fn module_entry<'u>(entries: &mut [Option<Entry<'u>>], res: Option<Res>, module: Spelled<'u>) {
    let Some(Res {
        index, depth: 0, ..
    }) = res
    else {
        return;
    };
    match entries.get_mut(index) {
        Some(Some(Entry::Modules(modules))) => modules.push(module),
        Some(slot) => *slot = Some(Entry::Modules(vec![module])),
        None => {}
    }
}

impl<'u> Scope<'_, '_, 'u> {
    /// What a resolved name stands for in the current frame
    pub(super) fn entry(&self, res: Option<Res>) -> Option<Entry<'u>> {
        self.ctx.frame.entry(res?).cloned()
    }

    /// The variable a name resolves to, recording it as a capture if another
    /// function owns it
    pub(super) fn var(&self, ident: &Ident) -> Option<VarId> {
        match self.entry(ident.res)? {
            Entry::Var(var) | Entry::Item { var: Some(var), .. } => {
                self.capture(var);
                Some(var)
            }
            Entry::Modules(_) | Entry::Item { var: None, .. } => None,
        }
    }

    pub(super) fn capture(&self, var: VarId) {
        let graph = self.graph();
        if graph.var(var).owner == self.ctx.func {
            return;
        }
        graph.var_mut(var).interprocedural = true;
        let mut func = graph.func_mut(self.ctx.func);
        if !func.captures.contains(&var) {
            func.captures.push(var);
        }
    }

    /// Record an assignment to a variable after it was bound. One by a function
    /// other than its owner makes it volatile.
    pub(super) fn assigned(&self, var: VarId) {
        let graph = self.graph();
        if graph.var(var).owner != self.ctx.func {
            graph.var_mut(var).volatile = true;
        }
    }

    /// Give a variable the type its annotation was interned as
    pub(super) fn annotate(&self, var: VarId, annot: Option<&ast::Annot>) {
        if let Some(ty) = annot.and_then(|annot| self.annotation(annot)) {
            self.graph().var_mut(var).annotation = Some(ty);
        }
    }

    /// An annotation's type, with its group's binders as the rigids its body is
    /// checked under
    pub(super) fn annotation(&self, annot: &ast::Annot) -> Option<TypeId> {
        use crate::ast::visit::Node;

        let span = annot.ty.span();
        let lower = self.lower;
        let ty = *lower.tables.site_types.get(&UnitSpan {
            unit: lower.unit,
            span,
        })?;
        Some(match lower.site_groups.get(&span) {
            Some(&Some(group)) => lower
                .db
                .substitute(ty, &lower.tables.group_rigids(lower.db, group)),
            _ => ty,
        })
    }
}
