//! A graphviz rendering of a graph: a cluster per function, labelled with its
//! dump header, and a node per block, labelled with its dump.

use std::io;

use dot_writer::{Attributes, Color, DotWriter, Scope, Shape, Style};

use super::{BlockId, Ir, Tag, Terminal, dump::Dump};
use crate::{source::Span, typeck::r#type::Database};

impl Ir {
    /// Write the graph in DOT, naming variables by the source text of their spans
    pub(crate) fn dot<'s>(
        &self,
        db: &Database,
        text: impl Fn(Span) -> &'s str,
        w: &mut impl io::Write,
    ) -> io::Result<()> {
        let dump = Dump::new(self, db, &text);
        let mut writer = DotWriter::from(w);
        writer.set_pretty_print(true);
        let mut digraph = writer.digraph();
        let mut blocks = vec![Vec::new(); self.funcs().count()];
        for (block_id, block) in self.blocks() {
            blocks[block.func.index()].push(block_id);
        }
        for (func_id, _) in self.funcs() {
            let mut header = String::new();
            dump.func(&mut header, func_id).map_err(io::Error::other)?;
            let mut cluster = digraph.cluster();
            cluster
                .set_label(&escape(&header))
                .set_style(Style::Filled)
                .set("fillcolor", "cornsilk", false)
                .set_font("monospace")
                .set_color(Color::Black);
            for &block_id in &blocks[func_id.index()] {
                let mut text = String::new();
                dump.block(&mut text, block_id).map_err(io::Error::other)?;
                let label: String = text.lines().map(|line| escape(line) + "\\l").collect();
                cluster
                    .node_named(node(block_id))
                    .set_label(&label)
                    .set_font("monospace")
                    .set_shape(Shape::Rectangle)
                    .set_style(Style::Filled)
                    .set_fill_color(Color::White)
                    .set_color(Color::Black);
            }
        }
        for (block_id, block) in self.blocks() {
            edges(self, &mut digraph, block_id, &block.terminal);
            if let Some(handler) = block.handler {
                digraph
                    .edge(node(block_id), node(handler))
                    .attributes()
                    .set_style(Style::Dotted)
                    .set_color(Color::Red);
            }
        }
        Ok(())
    }
}

/// The edges of a block's terminal, labelled with the role of each successor
fn edges(ir: &Ir, digraph: &mut Scope, block: BlockId, terminal: &Terminal) {
    let mut edge = |to: BlockId, label: &str, style: Option<Style>| {
        let mut attributes = digraph.edge(node(block), node(to)).attributes();
        if !label.is_empty() {
            attributes.set_label(label);
        }
        if let Some(style) = style {
            attributes.set_style(style);
        }
    };
    match terminal {
        Terminal::If { then, else_, .. } | Terminal::Unpack { then, else_, .. } => {
            edge(*then, "then", None);
            edge(*else_, "else", None);
        }
        Terminal::Next { body, exit, .. } => {
            edge(*body, "body", None);
            edge(*exit, "exit", None);
        }
        Terminal::Catch { clauses, otherwise } => {
            for (index, &(_, clause)) in clauses.iter().enumerate() {
                edge(clause, &index.to_string(), None);
            }
            edge(*otherwise, "otherwise", None);
        }
        Terminal::Leave { entry, tag } => {
            edge(*entry, "leave", None);
            if let Tag::Goto(to) = tag {
                edge(*to, "tag", Some(Style::Dashed));
            }
        }
        Terminal::Guard { next, targets } => {
            edge(*next, "", None);
            for &target in targets {
                edge(target, "escape", Some(Style::Dashed));
            }
        }
        Terminal::ReturnFrom { func, .. } => {
            edge(ir.func(*func).exit, "return", Some(Style::Dashed));
        }
        Terminal::Branch(_)
        | Terminal::Return
        | Terminal::Throw(_)
        | Terminal::EndFinally
        | Terminal::Escape
        | Terminal::Unreachable => {
            for to in terminal.successors() {
                edge(to, "", None);
            }
        }
    }
}

fn node(block: BlockId) -> String {
    format!("b{}", block.index())
}

/// A line escaped for a quoted DOT label
fn escape(line: &str) -> String {
    line.escape_debug().collect()
}
