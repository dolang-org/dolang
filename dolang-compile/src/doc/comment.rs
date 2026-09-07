//! Doc comment blocks, recovered from the comment spans the lexer collected.
//!
//! Comments are the one part of a declaration the grammar does not model: they
//! attach by adjacency rather than by syntax.  Resolving that adjacency here
//! rather than in each consumer is what lets a consumer ask a node for its
//! documentation instead of rescanning the source for it.

use crate::source::{File, Offset, Span};

/// The doc comment blocks of a file, in source order.
pub(crate) struct Blocks(Vec<Span>);

impl Blocks {
    /// Coalesce collected comments into the blocks that can document something.
    ///
    /// A comment with code before it on its line annotates that code, not what
    /// follows, so it is dropped; the rest join into a block per run of
    /// consecutive lines.  A blank line ends a block, which is what lets a
    /// declaration be preceded by unrelated commentary.
    ///
    /// `comments` must be in source order, as the lexer reports them.
    pub(crate) fn new(file: &File<'_>, comments: &[Span]) -> Self {
        let mut blocks: Vec<Span> = Vec::new();
        for comment in comments {
            // The lexer's span runs to the line terminator, so it can carry a
            // `\r` that is no part of what was written.
            let text = file.str(*comment);
            let span = Span {
                start: comment.start,
                end: comment.start + text.trim_end().len() as Offset,
            };
            if !own_line(file, span.start) || shebang(file, span) {
                continue;
            }
            match blocks.last_mut() {
                Some(last) if adjacent(file, last.end, span.start) => *last = *last | span,
                _ => blocks.push(span),
            }
        }
        Self(blocks)
    }

    /// The block documenting a construct that starts at `anchor`, if any.
    ///
    /// The construct claims the block on the line directly above it.  Nothing
    /// arbitrates between constructs: a block sits above exactly one line, and
    /// a nested construct starts after its parent's first line, so the
    /// outermost one is the only one whose text the block precedes.
    pub(crate) fn attached(&self, file: &File<'_>, anchor: Offset) -> Option<Span> {
        if anchor as usize > file.content().len() {
            return None;
        }
        let index = self.0.partition_point(|block| block.end <= anchor);
        let block = *self.0.get(index.checked_sub(1)?)?;
        adjacent(file, block.end, anchor).then_some(block)
    }
}

/// Whether a `#!` interpreter line opens the file.
///
/// It addresses the shell rather than the reader, so it documents nothing —
/// and without this it would document whatever declaration came first.
fn shebang(file: &File<'_>, span: Span) -> bool {
    span.start == 0 && file.slice(span).starts_with(b"#!")
}

/// Whether only whitespace precedes `offset` on its line.
fn own_line(file: &File<'_>, offset: Offset) -> bool {
    file.content()[..offset as usize]
        .iter()
        .rev()
        .take_while(|byte| **byte != b'\n')
        .all(u8::is_ascii_whitespace)
}

/// Whether `to` starts on the line directly below the one `from` ends on.
fn adjacent(file: &File<'_>, from: Offset, to: Offset) -> bool {
    let between = file.slice(Span {
        start: from,
        end: to,
    });
    between.iter().all(u8::is_ascii_whitespace)
        && between.iter().filter(|byte| **byte == b'\n').count() == 1
}
