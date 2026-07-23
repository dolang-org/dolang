use dolang::runtime::{
    Sym, Type,
    vm::{Builder, Stateful},
};

use crate::tar::{TarEntry, TarEntryWriter, TarReader, TarWriter};

pub(crate) struct Types<'v> {
    pub(crate) reader: Type<'v, TarReader>,
    pub(crate) entry: Type<'v, TarEntry>,
    pub(crate) writer: Type<'v, TarWriter>,
    pub(crate) entry_writer: Type<'v, TarEntryWriter>,
}

pub(crate) struct Syms<'v> {
    pub(crate) none: Sym<'v, 'v>,
    pub(crate) gzip: Sym<'v, 'v>,
    pub(crate) zstd: Sym<'v, 'v>,
    pub(crate) file: Sym<'v, 'v>,
    pub(crate) hardlink: Sym<'v, 'v>,
    pub(crate) symlink: Sym<'v, 'v>,
    pub(crate) char_device: Sym<'v, 'v>,
    pub(crate) block_device: Sym<'v, 'v>,
    pub(crate) dir: Sym<'v, 'v>,
    pub(crate) fifo: Sym<'v, 'v>,
    pub(crate) contiguous: Sym<'v, 'v>,
    pub(crate) unknown: Sym<'v, 'v>,
}

pub(crate) struct Global<'v> {
    pub(crate) types: Types<'v>,
    pub(crate) syms: Syms<'v>,
}

pub struct Tag;

impl<'v> Stateful<'v> for Global<'v> {
    type Tag = Tag;
}

impl<'v> Global<'v> {
    pub(crate) fn new(builder: &mut Builder<'v>) -> Self {
        Self {
            types: Types {
                reader: builder.register_type(),
                entry: builder.register_type(),
                writer: builder.register_type(),
                entry_writer: builder.register_type(),
            },
            syms: Syms {
                none: builder.sym("NONE"),
                gzip: builder.sym("GZIP"),
                zstd: builder.sym("ZSTD"),
                file: builder.sym("FILE"),
                hardlink: builder.sym("HARDLINK"),
                symlink: builder.sym("SYMLINK"),
                char_device: builder.sym("CHAR_DEVICE"),
                block_device: builder.sym("BLOCK_DEVICE"),
                dir: builder.sym("DIR"),
                fifo: builder.sym("FIFO"),
                contiguous: builder.sym("CONTIGUOUS"),
                unknown: builder.sym("UNKNOWN"),
            },
        }
    }
}
