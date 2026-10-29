//! Rendering for the `typeck.flow` trace

use super::{
    Flow, State,
    state::{CtxId, Link},
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
        let source = self.tables.info(self.unit).source;
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
            FuncKind::DefaultInit(class) => {
                format!("default (init) of {}", self.tables.qualified(class))
            }
        };
        let mut out = format!("b{} of f{} {name}", block.index(), func.index());
        let tags = self.contexts.tags(ctx);
        if !tags.is_empty() {
            let tags: Vec<String> = tags
                .iter()
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
    /// `^` if it may have escaped at the block's entry, with its link as
    /// `@g<group>.<row>`, then the stack and each group's columns
    pub(super) fn render_state(&self, block: BlockId, state: &State) -> String {
        let data = self.ir.block(block);
        let bottom = self.db.bottom();
        let mut parts: Vec<String> = self.layout.vars[data.func.index()]
            .iter()
            .zip(&state.vars)
            .enumerate()
            .map(|(index, (&var, slot))| {
                let escaped = data.escaped.get(index).is_some_and(|bit| *bit);
                (var, slot.fact(), escaped, slot.link)
            })
            .filter(|(_, fact, _, _)| fact.ty != bottom || fact.unassigned)
            .map(|(var, fact, escaped, link)| {
                let ty = match (fact.unassigned, fact.ty == bottom) {
                    (false, _) => self.show(fact.ty),
                    (true, true) => "unassigned".to_owned(),
                    (true, false) => format!("{} | unassigned", self.show(fact.ty)),
                };
                let mark = if escaped { "^" } else { "" };
                format!("{}{mark}{}: {ty}", self.var_name(var), show_link(link))
            })
            .collect();
        if !state.stack.is_empty() {
            let stack: Vec<String> = state
                .stack
                .iter()
                .map(|operand| format!("{}{}", self.show(operand.ty), show_link(operand.link)))
                .collect();
            parts.push(format!("stack [{}]", stack.join(", ")));
        }
        for (index, group) in state.groups.iter().enumerate() {
            if group.columns.is_empty() {
                continue;
            }
            let columns: Vec<String> = group
                .columns
                .iter()
                .map(|column| {
                    let entries: Vec<String> = column.iter().map(|&ty| self.show(ty)).collect();
                    format!("[{}]", entries.join(", "))
                })
                .collect();
            parts.push(format!("g{index} {}", columns.join(" ")));
        }
        format!("{{{}}}", parts.join(", "))
    }
}

fn show_link(link: Option<Link>) -> String {
    match link {
        Some(link) => format!("@g{}.{}", link.group.index(), link.row),
        None => String::new(),
    }
}
