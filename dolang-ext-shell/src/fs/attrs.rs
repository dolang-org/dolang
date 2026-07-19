use dolang_shell_vfs::{Metadata, MetadataFamily, UnixMetadataPlatform};

pub(crate) mod windows {
    pub(crate) const READONLY: u32 = 0x0000_0001;
    pub(crate) const HIDDEN: u32 = 0x0000_0002;
    pub(crate) const SYSTEM: u32 = 0x0000_0004;
    pub(crate) const ARCHIVE: u32 = 0x0000_0020;
    pub(crate) const TEMPORARY: u32 = 0x0000_0100;
    pub(crate) const REPARSE_POINT: u32 = 0x0000_0400;
    pub(crate) const COMPRESSED: u32 = 0x0000_0800;
    pub(crate) const OFFLINE: u32 = 0x0000_1000;
    pub(crate) const NOT_CONTENT_INDEXED: u32 = 0x0000_2000;
    pub(crate) const ENCRYPTED: u32 = 0x0000_4000;
}

pub(crate) mod linux {
    pub(crate) const SECURE_DELETE: u32 = 0x0000_0001;
    pub(crate) const UNDELETE: u32 = 0x0000_0002;
    pub(crate) const COMPRESSED: u32 = 0x0000_0004;
    pub(crate) const SYNC: u32 = 0x0000_0008;
    pub(crate) const IMMUTABLE: u32 = 0x0000_0010;
    pub(crate) const APPEND_ONLY: u32 = 0x0000_0020;
    pub(crate) const NO_DUMP: u32 = 0x0000_0040;
    pub(crate) const NO_ATIME: u32 = 0x0000_0080;
    pub(crate) const NO_COMPRESS: u32 = 0x0000_0400;
    pub(crate) const DATA_JOURNALING: u32 = 0x0000_4000;
    pub(crate) const NO_TAIL_MERGE: u32 = 0x0000_8000;
    pub(crate) const DIR_SYNC: u32 = 0x0001_0000;
    pub(crate) const TOP_DIR: u32 = 0x0002_0000;
    pub(crate) const EXTENT_FORMAT: u32 = 0x0008_0000;
    pub(crate) const NO_COPY_ON_WRITE: u32 = 0x0080_0000;
    pub(crate) const DIRECT_ACCESS: u32 = 0x0200_0000;
    pub(crate) const PROJECT_INHERIT: u32 = 0x2000_0000;
    pub(crate) const CASEFOLD: u32 = 0x4000_0000;
}

pub(crate) mod macos {
    pub(crate) const NO_DUMP: u32 = 0x0000_0001;
    pub(crate) const IMMUTABLE: u32 = 0x0000_0002;
    pub(crate) const APPEND_ONLY: u32 = 0x0000_0004;
    pub(crate) const OPAQUE: u32 = 0x0000_0008;
    pub(crate) const COMPRESSED: u32 = 0x0000_0020;
    pub(crate) const HIDDEN: u32 = 0x0000_8000;
}

pub(crate) enum Flag {
    Inapplicable,
    Unavailable,
    Value(bool),
}

pub(crate) fn flag(metadata: &Metadata, windows: u32, linux: u32, macos: u32) -> Flag {
    let (value, mask) = match &metadata.family {
        MetadataFamily::Windows(metadata) => (Some(metadata.attrs), windows),
        MetadataFamily::Unix(metadata) => match metadata.platform {
            UnixMetadataPlatform::Linux { attrs } => (attrs, linux),
            UnixMetadataPlatform::Macos { attrs } => (Some(attrs), macos),
        },
    };
    if mask == 0 {
        Flag::Inapplicable
    } else if let Some(value) = value {
        Flag::Value(value & mask != 0)
    } else {
        Flag::Unavailable
    }
}
