use dolang::runtime::{
    Sym, Type,
    object::{FlagLike, Flags},
    vm::{Builder, Stateful},
};

use crate::{
    access_mask::AccessMask, key::Key, subkeys::SubKeys, value_entry::ValueEntry, values::Values,
};

pub(crate) struct Types<'v> {
    pub(crate) key: Type<'v, Key>,
    pub(crate) value_entry: Type<'v, ValueEntry>,
    pub(crate) values: Type<'v, Values>,
    pub(crate) subkeys: Type<'v, SubKeys>,
    pub(crate) access_mask: Type<'v, Flags<AccessMask>>,
}

/// Symbols for the `:UPPER_CASE:` constants this extension accepts in place
/// of dedicated enum types: predefined roots, 32/64-bit view selection, and
/// (on `set`) an explicit `REG_*` kind override.
pub(crate) struct Syms<'v> {
    // Predefined roots
    pub(crate) classes_root: Sym<'v, 'v>,
    pub(crate) current_user: Sym<'v, 'v>,
    pub(crate) local_machine: Sym<'v, 'v>,
    pub(crate) users: Sym<'v, 'v>,
    pub(crate) current_config: Sym<'v, 'v>,
    // Views
    pub(crate) native: Sym<'v, 'v>,
    pub(crate) wow32: Sym<'v, 'v>,
    pub(crate) wow64: Sym<'v, 'v>,
    // Value kinds
    pub(crate) sz: Sym<'v, 'v>,
    pub(crate) expand_sz: Sym<'v, 'v>,
    pub(crate) multi_sz: Sym<'v, 'v>,
    pub(crate) dword: Sym<'v, 'v>,
    pub(crate) dword_big_endian: Sym<'v, 'v>,
    pub(crate) qword: Sym<'v, 'v>,
    pub(crate) binary: Sym<'v, 'v>,
    pub(crate) none: Sym<'v, 'v>,
    // Method/keyword names
    pub(crate) close: Sym<'v, 'v>,
    pub(crate) view: Sym<'v, 'v>,
    pub(crate) access: Sym<'v, 'v>,
    pub(crate) kind: Sym<'v, 'v>,
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
        let key = builder.register_type::<Key>();
        let value_entry = builder.register_type::<ValueEntry>();
        let values = builder.register_type::<Values>();
        let subkeys = builder.register_type::<SubKeys>();
        let access_mask = AccessMask::register_type(builder);

        Self {
            types: Types {
                key,
                value_entry,
                values,
                subkeys,
                access_mask,
            },
            syms: Syms {
                classes_root: builder.sym("CLASSES_ROOT"),
                current_user: builder.sym("CURRENT_USER"),
                local_machine: builder.sym("LOCAL_MACHINE"),
                users: builder.sym("USERS"),
                current_config: builder.sym("CURRENT_CONFIG"),
                native: builder.sym("NATIVE"),
                wow32: builder.sym("WOW32"),
                wow64: builder.sym("WOW64"),
                sz: builder.sym("SZ"),
                expand_sz: builder.sym("EXPAND_SZ"),
                multi_sz: builder.sym("MULTI_SZ"),
                dword: builder.sym("DWORD"),
                dword_big_endian: builder.sym("DWORD_BIG_ENDIAN"),
                qword: builder.sym("QWORD"),
                binary: builder.sym("BINARY"),
                none: builder.sym("NONE"),
                close: builder.sym("close"),
                view: builder.sym("view"),
                access: builder.sym("access"),
                kind: builder.sym("kind"),
            },
        }
    }
}
