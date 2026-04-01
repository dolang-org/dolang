use dolang_util::intern;

use crate::source::Span;

/// Origin of a resolved name
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub enum Origin {
    ImportItem {
        module: Span,
        item: Span,
        name: Span,
    },
    ImportModule {
        module: Span,
        name: Span,
    },
    PreludeModule {
        module: String,
        name: String,
    },
    PreludeItem {
        module: String,
        item: String,
        name: String,
    },
    Class {
        span: Span,
    },
    Def {
        span: Span,
        class: Option<Span>,
    },
    Bind {
        span: Span,
        class: Option<Span>,
    },
    Param {
        span: Span,
    },
    SelfParam {
        span: Span,
    },
    Synthetic,
    Repl,
}

pub(crate) struct Tag;

pub(crate) type Id = intern::Id<Tag>;
pub(crate) type Table = intern::Table<Origin, Tag>;
