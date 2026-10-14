//! Rendering for the `typeck.flow` trace

use super::{
    Flow, State,
    state::{CtxId, May},
};
use crate::typeck::{
    cfg::{BlockId, FuncKind, Tag, VarId},
    r#type::TypeId,
};

impl Flow<'_, '_> {
    /// A type as diagnostics show it
    pub(super) fn show(&self, ty: TypeId) -> String {
        self.tables.render_type(self.db, ty)
    }

    pub(super) fn var_name(&self, var: VarId) -> String {
        let source = self.tables.units[self.unit.index()].source;
        let file = &source.expect("flow analyzes a unit from source").file;
        self.ir.var_name(var, self.db, |span| file.str(span))
    }

    /// A block in a context: the block, the `finally` tags it was entered with,
    /// and its function
    pub(super) fn place(&self, block: BlockId, ctx: CtxId) -> String {
        let func = self.ir.block(block).func;
        let name = match self.ir.func(func).kind {
            FuncKind::Module(_) => "<module>".to_owned(),
            FuncKind::Decl(decl) => self.tables.qualified(decl),
        };
        let mut out = format!("b{} of f{} {name}", block.index(), func.index());
        let tags = self.contexts.tags(ctx);
        if !tags.is_empty() {
            let tags: Vec<String> = (tags.iter())
                .map(|tag| match tag {
                    Tag::Goto(target) => format!("goto b{}", target.index()),
                    Tag::Rethrow => "rethrow".to_owned(),
                })
                .collect();
            out.push_str(&format!(" [{}]", tags.join(", ")));
        }
        out
    }

    /// A block's state: what each variable may hold, as a judgment shows it, marked
    /// `^` once it may have escaped and `~` once a cached capture thaws, then the
    /// stack
    pub(super) fn render_state(&self, block: BlockId, state: &State) -> String {
        let func = self.ir.block(block).func;
        let bottom = self.db.bottom();
        let mut parts: Vec<String> = (self.funcs[func.index()].vars.iter().zip(&state.vars))
            .map(|(&var, slot)| (var, slot.fact(), slot.may))
            .filter(|(_, fact, _)| fact.ty != bottom || fact.unassigned)
            .map(|(var, fact, may)| {
                let ty = match (fact.unassigned, fact.ty == bottom) {
                    (false, _) => self.show(fact.ty),
                    (true, true) => "unassigned".to_owned(),
                    (true, false) => format!("{} | unassigned", self.show(fact.ty)),
                };
                let mark = match (may.contains(May::ESCAPED), may.contains(May::THAWED)) {
                    (true, _) => "^",
                    (false, true) => "~",
                    (false, false) => "",
                };
                format!("{}{mark}: {ty}", self.var_name(var))
            })
            .collect();
        if !state.stack.is_empty() {
            let stack: Vec<String> = state.stack.iter().map(|&ty| self.show(ty)).collect();
            parts.push(format!("stack [{}]", stack.join(", ")));
        }
        format!("{{{}}}", parts.join(", "))
    }
}
