//! What the Do language server and the playground both ask of a compiled unit:
//! the tokens naming declarations, how to highlight a token, a declaration's
//! signature, where a name a check reached is declared, and what hover shows.
//!
//! It does no I/O and builds for wasm. Positions are the compiler's
//! [`Span`](dolang_compile::diag::Span)s, which each caller converts to its
//! own encoding, and modules other than a caller's own are found through a
//! resolver it gives, so the server can look in a check's modules and the
//! playground in its one unit.

mod classify;
mod hover;
mod lookup;
mod signature;
mod unit;

pub use classify::{Highlight, classify};
pub use hover::{
    Checked, Declaration, Hover, LineRole, Shape, TypeLine, declared, describe, hover, node_at,
    node_targets, overloaded, targets,
};
pub use lookup::{Target, item, member};
pub use signature::{Param, Signature, class_header, parameter, signature};
pub use unit::{
    Reference, as_written, chosen, normalize_doc, one_line, overload, overloads, reference_at,
    references,
};
