use std::fmt::{self};

use super::{BlockId, FuncId, Graph, Inst, InstInfo, ScopeId, Term, TermInfo};
use crate::Compiler;

#[cfg(feature = "debug")]
use super::Block;

#[cfg(feature = "debug")]
use dolang_bytecode::BUILTINS;

#[cfg(feature = "debug")]
use dot_writer::{Attributes, Color, DotWriter, Shape, Style};

#[cfg(feature = "debug")]
impl Inst {
    fn resolve_local<'a>(
        &self,
        compiler: &'a Compiler,
        graph: &Graph,
        scope_id: ScopeId,
        idx: usize,
    ) -> &'a str {
        let scope = graph.scope(scope_id);
        if scope.local_offset > idx {
            let parent = scope.parent.unwrap();
            drop(scope);
            self.resolve_local(compiler, graph, parent, idx)
        } else {
            let var = scope
                .vars
                .iter()
                .filter(|v| v.is_emitted(&compiler.origintab) && !v.captured)
                .nth(idx - scope.local_offset)
                .unwrap();
            if var.sym.index() == usize::MAX {
                "<synthetic>"
            } else {
                &compiler.bintab[compiler.symtab[var.sym]]
            }
        }
    }

    fn resolve_upvar<'a>(
        &self,
        compiler: &'a Compiler,
        graph: &Graph,
        scope_id: ScopeId,
        idx: usize,
        mut depth: usize,
    ) -> &'a str {
        let scope = graph.scope(scope_id);
        if scope.is_nl_guard {
            depth -= 1;
        }
        if !scope.has_upvars() {
            let parent = scope.parent.unwrap();
            drop(scope);
            self.resolve_upvar(compiler, graph, parent, idx, depth)
        } else if depth == 0 {
            let var = scope.vars.iter().filter(|v| v.captured).nth(idx).unwrap();
            if var.sym.index() == usize::MAX {
                "<synthetic>"
            } else {
                &compiler.bintab[compiler.symtab[var.sym]]
            }
        } else {
            let parent = scope.parent.unwrap();
            drop(scope);
            self.resolve_upvar(compiler, graph, parent, idx, depth - 1)
        }
    }

    fn dump(
        &self,
        compiler: &Compiler,
        graph: &Graph,
        scope_id: ScopeId,
        w: &mut impl fmt::Write,
    ) -> fmt::Result {
        use InstInfo::*;

        match &self.0 {
            Nop => write!(w, "nop"),
            Pop => write!(w, "pop"),
            Dup => write!(w, "dup"),
            Swap(i, j) => write!(w, "swap {} {}", i, j),
            Add => write!(w, "add"),
            Sub => write!(w, "sub"),
            Mul => write!(w, "mul"),
            Div => write!(w, "div"),
            Ediv => write!(w, "ediv"),
            Mod => write!(w, "mod"),
            Eq => write!(w, "eq"),
            Ne => write!(w, "ne"),
            Gt => write!(w, "gt"),
            Lt => write!(w, "lt"),
            Gte => write!(w, "gte"),
            Lte => write!(w, "lte"),
            Neg => write!(w, "neg"),
            Not => write!(w, "not"),
            BitNot => write!(w, "bnot"),
            BitAnd => write!(w, "band"),
            BitOr => write!(w, "bor"),
            BitXor => write!(w, "bxor"),
            LoadConst(id) => {
                write!(w, "ldc ")?;
                compiler.consttab[*id].dump(compiler, w)
            }
            Call(id) => {
                write!(w, "call ")?;
                compiler.packtab[*id].dump(compiler, w)
            }
            MethodCall(sym, sig) => {
                write!(w, "mcll {} ", &compiler.bintab[compiler.symtab[*sym]],)?;
                compiler.packtab[*sig].dump(compiler, w)
            }
            Builtin(idx, sig) => {
                write!(w, "bltn {} ", BUILTINS[*idx])?;
                compiler.packtab[*sig].dump(compiler, w)
            }
            LoadLocal(idx) => write!(
                w,
                "ldl #{} ({})",
                idx,
                self.resolve_local(compiler, graph, scope_id, *idx)
            ),
            StoreLocal(idx) => write!(
                w,
                "stl #{} ({})",
                idx,
                self.resolve_local(compiler, graph, scope_id, *idx)
            ),
            LoadUpvar(idx, depth) => write!(
                w,
                "ldu #{}🠅{} ({})",
                idx,
                depth,
                self.resolve_upvar(compiler, graph, scope_id, *idx, *depth)
            ),
            StoreUpvar(idx, depth) => write!(
                w,
                "stu #{}🠅{} ({})",
                idx,
                depth,
                self.resolve_upvar(compiler, graph, scope_id, *idx, *depth)
            ),
            Get(sym) => write!(w, "get {}", &compiler.bintab[compiler.symtab[*sym]]),
            Set(sym) => write!(w, "set {}", &compiler.bintab[compiler.symtab[*sym]]),
            Index => write!(w, "indx"),
            Assign => write!(w, "assn"),
            PushUpvars(count) => write!(w, "pshu {}", count),
            PopUpvars => write!(w, "popu"),
            Close(id) => write!(w, "cls #{}", id.0),
            Reify(id) => {
                write!(w, "rfy ")?;
                compiler.packtab[*id].dump(compiler, w)
            }
            Next => write!(w, "next"),
            Unpack(id) => {
                write!(w, "unpk ")?;
                compiler.unpacktab[*id].dump(compiler, w)
            }
            NlGuard(id) => write!(w, "nlgd #{}", id.0),
        }
    }
}

#[cfg(feature = "debug")]
impl Term {
    fn dump(&self, _compiler: &Compiler, w: &mut impl fmt::Write) -> fmt::Result {
        use TermInfo::*;

        match &self.0 {
            Ret => write!(w, "ret"),
            Branch(id) => write!(w, "br #{}", id.0),
            If(tid, fid) => write!(w, "if #{}, #{}", tid.0, fid.0),
            NlBranch(depth, indicator) => write!(w, "nlbr {}:{}", depth, indicator),
        }
    }
}

#[cfg(feature = "debug")]
impl Block {
    fn dot(
        &self,
        compiler: &Compiler,
        graph: &Graph,
        scope: &mut dot_writer::Scope,
        block_id: BlockId,
    ) -> std::io::Result<()> {
        use std::{fmt::Write, io};
        let node_id = format!("block_{}", block_id.0);

        // Create label for the node
        let mut label = String::new();

        write!(&mut label, "bb #{}\\l", block_id.0).map_err(io::Error::other)?;

        // Add all instructions with literal newlines
        for inst in self.insts.iter() {
            let mut tmp = String::new();
            inst.dump(compiler, graph, self.scope, &mut tmp)
                .map_err(io::Error::other)?;
            label.push_str("  ");
            label.extend(tmp.escape_debug());
            label.push_str("\\l");
        }

        // Add terminator
        let mut tmp = String::new();
        self.term
            .dump(compiler, &mut tmp)
            .map_err(io::Error::other)?;
        label.push_str("  ");
        label.push_str(&tmp.replace('"', "\\\""));
        label.push_str("\\l");

        // Create node with proper font and alignment
        scope
            .node_named(&node_id)
            .set_label(&label)
            .set_font("monospace") // Use font API properly
            .set_shape(Shape::Rectangle)
            .set_color(Color::Black)
            .set_style(Style::Filled)
            .set_fill_color(Color::White)
            .set("margin", "0.2", false);

        // // Add edge from block to its scope
        let scope_node = format!("scope_{}", self.scope.0);
        scope
            .edge(&node_id, &scope_node)
            .attributes()
            .set_style(Style::Dotted);

        Ok(())
    }

    fn dot_edges(
        &self,
        _compiler: &Compiler,
        scope: &mut dot_writer::Scope,
        block_id: BlockId,
    ) -> std::io::Result<()> {
        let source_node = format!("block_{}", block_id.0);

        use TermInfo::*;
        match &self.term.0 {
            Ret | NlBranch(..) => {
                // No outgoing edges
            }
            Branch(target) => {
                let target_node = format!("block_{}", target.0);
                scope
                    .edge(&source_node, &target_node)
                    .attributes()
                    .set("tailport", "s", true)
                    .set("headport", "n", true);
            }
            If(then_target, else_target) => {
                let then_node = format!("block_{}", then_target.0);
                let else_node = format!("block_{}", else_target.0);

                scope
                    .edge(&source_node, &then_node)
                    .attributes()
                    .set("tailport", "s", true)
                    .set("headport", "n", true);
                scope
                    .edge(&source_node, &else_node)
                    .attributes()
                    .set("tailport", "s", true)
                    .set("headport", "n", true);
            }
        }

        Ok(())
    }
}

#[cfg(feature = "debug")]
impl super::Func {
    fn dot(
        &self,
        compiler: &Compiler,
        graph: &Graph,
        digraph: &mut dot_writer::Scope,
        func_id: FuncId,
    ) -> std::io::Result<()> {
        let mut cluster = digraph.cluster();

        // Set cluster label to function name or index
        let func_name = if let Some(span) = self.name {
            format!("<<b>{} (#{})</b>>", compiler.file.str(span), func_id.0)
        } else {
            format!("<<b>function #{}</b>>", func_id.0)
        };
        cluster
            .set("label", &func_name, false)
            .set_shape(Shape::Rectangle)
            .set_style(Style::Filled)
            .set("fillcolor", "cornsilk4", false)
            .set_font_color(Color::White)
            .set_font("Noto Sans")
            .set_font_size(16.0)
            .set_color(Color::Black);

        // Create scope nodes with content
        for scope_id in self.scopes.iter() {
            let scope = graph.scope(*scope_id);
            let scope_name = format!("scope_{}", scope_id.0);

            // Create label with scope content
            let mut label = format!("Scope #{}:\\n", scope_id.0);

            let used: Vec<_> = scope
                .vars
                .iter()
                .enumerate()
                .filter(|(_, v)| v.is_emitted(&compiler.origintab))
                .collect();

            // Add all variables
            for (j, local) in used.iter() {
                label.push_str(&format!("  {}: ", j));
                let name = if local.sym.index() == usize::MAX {
                    "<synthetic>"
                } else {
                    &compiler.bintab[compiler.symtab[local.sym]]
                };
                label.push_str(name);
                if local.captured {
                    label.push('🠇');
                }
                label.push_str("\\l");
            }

            cluster
                .node_named(scope_name)
                .set_label(&label)
                .set_font("monospace")
                .set_shape(Shape::Rectangle)
                .set_style(Style::Filled)
                .set_fill_color(Color::White)
                .set_color(Color::Black);

            for block_id in scope.blocks.iter() {
                let block = graph.block(*block_id);
                if block.inbound.is_empty() && block.insts.is_empty() {
                    // Runt block
                    continue;
                }
                block.dot(compiler, graph, &mut cluster, *block_id)?;
            }
        }

        // Add edges from child scopes to parent scopes
        for scope_id in self.scopes.iter() {
            let scope = graph.scope(*scope_id);

            if let Some(parent_scope) = scope.parent {
                let child_node = format!("scope_{}", scope_id.0);
                let parent_node = format!("scope_{}", parent_scope.0);
                cluster
                    .edge(&child_node, &parent_node)
                    .attributes()
                    .set_style(Style::Dotted);
            }
        }

        Ok(())
    }
}

#[cfg(feature = "debug")]
impl Graph {
    pub(crate) fn dot(
        &self,
        compiler: &Compiler,
        w: &mut impl std::io::Write,
    ) -> std::io::Result<()> {
        let mut writer = DotWriter::from(w);
        writer.set_pretty_print(true);
        let mut digraph = writer.digraph();

        // Add each function as a cluster containing scopes
        for func_id in self.iter_funcs() {
            let func = self.func(func_id);
            func.dot(compiler, self, &mut digraph, func_id)?;
        }

        for block_id in self.iter_blocks() {
            let block = self.block(block_id);
            block.dot_edges(compiler, &mut digraph, block_id)?;
        }

        Ok(())
    }
}
