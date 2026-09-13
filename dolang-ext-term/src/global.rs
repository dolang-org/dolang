use std::cell::{Cell, RefCell};

use dolang::runtime::{
    Sym, Type,
    strand::{LocalKey, LocalRootKey},
    value::Root,
    vm::{Builder, Stateful},
};

use crate::{
    console::{Console, DefaultOutput, SinkConsole, SubConsole},
    geometry::Geometry,
    local::Local,
    term::{StyleKeys, StyleObject, Text},
};

pub(crate) struct Types<'v> {
    pub(crate) console: Type<'v, Console>,
    pub(crate) sink_console: Type<'v, SinkConsole>,
    pub(crate) sub_console: Type<'v, SubConsole>,
    pub(crate) default: Type<'v, DefaultOutput>,
    pub(crate) geometry: Type<'v, Geometry>,
    pub(crate) text: Type<'v, Text>,
    pub(crate) style: Type<'v, StyleObject>,
}

pub(crate) struct Syms<'v> {
    /// `FmtValue.value`, the value a format specification is bound to
    pub(crate) value: Sym<'v, 'v>,
    /// `Fmt.len`, the number of segments in a sequence
    pub(crate) len: Sym<'v, 'v>,
    /// `FmtParam.name`, the parameter an unbound position names
    pub(crate) name: Sym<'v, 'v>,
    pub(crate) chunk: Sym<'v, 'v>,
    pub(crate) line: Sym<'v, 'v>,
    pub(crate) mode: Sym<'v, 'v>,
    pub(crate) write: Sym<'v, 'v>,
    pub(crate) flush: Sym<'v, 'v>,
    pub(crate) line_ending: Sym<'v, 'v>,
    pub(crate) can_style: Sym<'v, 'v>,
    pub(crate) is_tty: Sym<'v, 'v>,
    pub(crate) geometry: Sym<'v, 'v>,
}

/// The host console, as installed by [`crate::install_console`].
pub(crate) struct Host<'v> {
    /// The console itself, or `nil` until one is installed.
    pub(crate) console: RefCell<Root<'v>>,
    /// The console's `line_ending`, supplied on installation.
    pub(crate) line_ending: RefCell<Root<'v>>,
    /// The console's `can_style`, supplied on installation.
    pub(crate) can_style: Cell<bool>,
}

pub(crate) struct Global<'v> {
    pub(crate) types: Types<'v>,
    pub(crate) syms: Syms<'v>,
    /// The symbols naming `term`'s style options.
    pub(crate) style_keys: StyleKeys<'v>,
    pub(crate) local: LocalKey<'v, Local>,
    /// The console installed by an enclosing `term.capture`, or `nil` for none.
    ///
    /// A strand-local root rather than a `Local` field because it holds a GC
    /// value; it is duplicated into derived strands at spawn, so a capture
    /// covers whatever the block spawns.
    pub(crate) capture: LocalRootKey<'v>,
    /// The installed capture's `line_ending`, read when it was installed.
    ///
    /// A root for the same reason as [`Self::capture`], and duplicated into
    /// derived strands alongside it.
    pub(crate) capture_line_ending: LocalRootKey<'v>,
    pub(crate) host: Host<'v>,
}

pub struct Tag;

impl<'v> Stateful<'v> for Global<'v> {
    type Tag = Tag;
}

impl<'v> Global<'v> {
    pub(crate) fn new(builder: &mut Builder<'v>) -> Self {
        let console = builder.register_type::<Console>();
        let sink_console = builder
            .build_type::<SinkConsole>((), ())
            .nominal_supertype(console)
            .build();
        let sub_console = builder
            .build_type::<SubConsole>((), ())
            .nominal_supertype(console)
            .build();
        let default = builder
            .build_type::<DefaultOutput>((), ())
            .nominal_supertype(console)
            .build();

        Self {
            types: Types {
                console,
                sink_console,
                sub_console,
                default,
                geometry: builder.register_type(),
                text: builder.register_type(),
                style: builder.register_type(),
            },
            style_keys: crate::term::style_keys(builder),
            syms: Syms {
                value: builder.sym("value"),
                len: builder.sym("len"),
                name: builder.sym("name"),
                chunk: builder.sym("CHUNK"),
                line: builder.sym("LINE"),
                mode: builder.sym("mode"),
                write: builder.sym("write"),
                flush: builder.sym("flush"),
                line_ending: builder.sym("line_ending"),
                can_style: builder.sym("can_style"),
                is_tty: builder.sym("is_tty"),
                geometry: builder.sym("geometry"),
            },
            local: builder.local(),
            capture: builder.local_root(),
            capture_line_ending: builder.local_root(),
            host: Host {
                console: RefCell::new(Root::new(builder)),
                line_ending: RefCell::new(Root::new(builder)),
                can_style: Cell::new(false),
            },
        }
    }
}
