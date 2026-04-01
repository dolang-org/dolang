use dolang_util::intern;

pub(crate) struct Tag;

pub(crate) type Id = intern::Id<Tag>;
pub(crate) type Table = intern::Table<intern::StrId, Tag>;
