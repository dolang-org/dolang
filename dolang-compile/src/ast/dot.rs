use std::{fmt::Write, io, ops::ControlFlow};

use dot_writer::{Attributes, Color, NodeId, Shape, Style};

use super::{Node, NodeKind, Token, Visit};
use crate::{Compiler, origin, source::Span};

/// A visitor that generates graphviz DOT output for the AST
pub struct DotVisitor<'a, 'b> {
    digraph: &'b mut dot_writer::Scope<'a, 'a>,
    compiler: &'a Compiler<'a>,
    parent: Option<NodeId>,
    compact: bool,
}

impl<'a, 'b> DotVisitor<'a, 'b> {
    pub(crate) fn new(
        digraph: &'b mut dot_writer::Scope<'a, 'a>,
        compiler: &'a Compiler<'a>,
    ) -> Self {
        Self {
            digraph,
            compiler,
            parent: None,
            compact: true,
        }
    }

    /// Create a node label from the node kind and optional span
    fn make_label(&self, kind: NodeKind, span: Span) -> String {
        let mut label = format!("{}", kind);

        // Add span information if enabled
        if span != Span::INVALID {
            let coords = self.compiler.file.coord_span(span);
            write!(
                &mut label,
                "\\nL{}:{}–L{}:{}",
                coords.start.line, coords.start.column, coords.end.line, coords.end.column
            )
            .unwrap();
        }

        label
    }

    /// Create a token label
    fn make_token_label(&self, token: Token, span: Span) -> String {
        let mut label = format!("{:?}", token);

        if span != Span::INVALID {
            let text = self.compiler.file.str(span);
            if !text.is_empty() {
                let escaped = text.escape_debug();
                write!(&mut label, "\\n{}", escaped).unwrap();
            }

            let coords = self.compiler.file.coord_span(span);
            write!(
                &mut label,
                "\\nL{}:{}–L{}:{}",
                coords.start.line, coords.start.column, coords.end.line, coords.end.column
            )
            .unwrap();
        }

        label
    }
}

impl<'a, 'b> Visit for DotVisitor<'a, 'b> {
    type Break = io::Error;

    fn node<T: Node + ?Sized>(&mut self, node: &T) -> ControlFlow<Self::Break> {
        let span = node.span();
        let kind = node.kind();

        let label = self.make_label(kind, span);

        let mut node_builder = self.digraph.node_auto();
        node_builder
            .set_label(&label)
            .set_shape(Shape::Rectangle)
            .set_style(Style::Unfilled)
            .set_color(Color::Black)
            .set_font("monospace")
            .set_font_size(10.0);

        let node_id = node_builder.id();
        drop(node_builder);

        // Add edge from parent if there is one
        if let Some(parent_id) = &self.parent {
            self.digraph.edge(parent_id, node_id.clone());
        }

        node.accept(&mut DotVisitor {
            digraph: self.digraph,
            compiler: self.compiler,
            parent: Some(node_id),
            compact: self.compact,
        })
    }

    fn token(
        &mut self,
        token: Token,
        span: Span,
        _origin: Option<origin::Id>,
    ) -> ControlFlow<Self::Break> {
        let label = self.make_token_label(token, span);

        if self.compact && matches!(token, Token::Delim | Token::Keyword | Token::StringDelim) {
            return ControlFlow::Continue(());
        }

        let mut node = self.digraph.node_auto();
        node.set_label(&label)
            .set_style(Style::Dashed)
            .set_color(Color::Black)
            .set_font("monospace")
            .set_font_size(9.0);

        let node_id = node.id();

        drop(node);

        // Add edge from parent
        if let Some(parent_id) = &self.parent {
            self.digraph
                .edge(parent_id, &node_id)
                .attributes()
                .set_style(Style::Dashed);
        }

        ControlFlow::Continue(())
    }
}
