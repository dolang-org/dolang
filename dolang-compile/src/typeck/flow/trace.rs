//! Rendering for the `typeck.flow` trace

use super::{Flow, State, state::CtxId};
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
        let file = &source
            .expect("flow analyzes a unit from source")
            .compiler
            .file;
        self.ir.var_name(var, |span| file.str(span))
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

    /// A block's state: what each variable may hold, as a judgment shows it, then
    /// the stack
    pub(super) fn render_state(&self, block: BlockId, state: &State) -> String {
        let func = self.ir.func(self.ir.block(block).func);
        let bottom = self.db.bottom();
        let mut parts: Vec<String> = (func.vars.iter().zip(&state.vars))
            .filter(|(_, fact)| fact.ty != bottom || fact.unassigned)
            .map(|(&var, fact)| {
                let ty = match (fact.unassigned, fact.ty == bottom) {
                    (false, _) => self.show(fact.ty),
                    (true, true) => "unassigned".to_owned(),
                    (true, false) => format!("{} | unassigned", self.show(fact.ty)),
                };
                format!("{}: {ty}", self.var_name(var))
            })
            .collect();
        if !state.stack.is_empty() {
            let stack: Vec<String> = state.stack.iter().map(|&ty| self.show(ty)).collect();
            parts.push(format!("stack [{}]", stack.join(", ")));
        }
        format!("{{{}}}", parts.join(", "))
    }
}
