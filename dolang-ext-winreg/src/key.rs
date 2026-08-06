//! `winreg.Key` — an open registry key, and the `winreg.open` entry point
//! that bootstraps one from a predefined root.

use dolang::runtime::{
    Args, Error, Object, Result, Slot, State, Strand, call, method,
    object::{FlagsTypeExt, TypeBuilder},
    unpack,
    vm::Builder,
};
use dolang_ext_shell::{ErrorExt, ResultExt};
use dolang_vfs_winreg::{PredefinedRoot, View};
use dolang_winterop::{
    DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
    SACL_SECURITY_INFORMATION,
};

use crate::{
    access_mask::AccessMask,
    convert,
    global::Global,
    subkeys::{SubKeys, SubKeysAnnex},
    value_entry::{ValueEntry, ValueEntryAnnex},
    values::{Values, ValuesAnnex},
};

pub(crate) struct Key(Option<dolang_vfs_winreg::Key>);

fn root_from_sym<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    slot: Slot<'v, '_>,
) -> Result<'v, 's, PredefinedRoot> {
    let sym = slot
        .as_sym(strand.vm())
        .ok_or_else(|| Error::type_error(strand, "root: expected a symbol"))?;
    let syms = &global.syms;
    if sym == syms.classes_root {
        Ok(PredefinedRoot::ClassesRoot)
    } else if sym == syms.current_user {
        Ok(PredefinedRoot::CurrentUser)
    } else if sym == syms.local_machine {
        Ok(PredefinedRoot::LocalMachine)
    } else if sym == syms.users {
        Ok(PredefinedRoot::Users)
    } else if sym == syms.current_config {
        Ok(PredefinedRoot::CurrentConfig)
    } else {
        Err(Error::value(
            strand,
            "root: expected :CLASSES_ROOT:, :CURRENT_USER:, :LOCAL_MACHINE:, :USERS:, or :CURRENT_CONFIG:",
        ))
    }
}

fn view_from_sym<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    slot: Option<Slot<'v, '_>>,
) -> Result<'v, 's, View> {
    let Some(slot) = slot else {
        return Ok(View::Native);
    };
    let sym = slot
        .as_sym(strand.vm())
        .ok_or_else(|| Error::type_error(strand, "view: expected a symbol"))?;
    let syms = &global.syms;
    if sym == syms.native {
        Ok(View::Native)
    } else if sym == syms.wow32 {
        Ok(View::Wow32)
    } else if sym == syms.wow64 {
        Ok(View::Wow64)
    } else {
        Err(Error::value(
            strand,
            "view: expected :NATIVE:, :WOW32:, or :WOW64:",
        ))
    }
}

/// Parses the `access:` keyword argument, accepting an existing
/// `winreg.AccessMask` instance, a bare symbol, or an iterable of symbols
/// whose bits are OR'd together (`access: [:READ:, :WRITE_DAC:]`) — needed
/// because a registry key is opened once and reused for every later
/// operation on it, so a caller that wants (say) to later modify a key's
/// DACL must request that access up front. Defaults to `:READ:` when
/// omitted.
async fn access_from_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    slot: Option<Slot<'v, '_>>,
) -> Result<'v, 's, AccessMask> {
    match slot {
        Some(slot) => global.types.access_mask.coerce(strand, &slot).await,
        None => Ok(AccessMask::READ),
    }
}

fn kind_from_sym<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    slot: Option<Slot<'v, '_>>,
) -> Result<'v, 's, Option<convert::Kind>> {
    let Some(slot) = slot else {
        return Ok(None);
    };
    let sym = slot
        .as_sym(strand.vm())
        .ok_or_else(|| Error::type_error(strand, "kind: expected a symbol"))?;
    let syms = &global.syms;
    let kind = if sym == syms.sz {
        convert::Kind::Sz
    } else if sym == syms.expand_sz {
        convert::Kind::ExpandSz
    } else if sym == syms.multi_sz {
        convert::Kind::MultiSz
    } else if sym == syms.dword {
        convert::Kind::Dword
    } else if sym == syms.dword_big_endian {
        convert::Kind::DwordBigEndian
    } else if sym == syms.qword {
        convert::Kind::Qword
    } else if sym == syms.binary {
        convert::Kind::Binary
    } else if sym == syms.none {
        convert::Kind::None
    } else {
        return Err(Error::value(
            strand,
            "kind: expected :SZ:, :EXPAND_SZ:, :MULTI_SZ:, :DWORD:, :DWORD_BIG_ENDIAN:, :QWORD:, :BINARY:, or :NONE:",
        ));
    };
    Ok(Some(kind))
}

/// Builds a `dolang_vfs` security-information mask from the
/// `owner:`/`group:`/`dacl:`/`sacl:` boolean keyword arguments of
/// `Key.sec_desc()`, mirroring `dolang-ext-shell`'s
/// `fs::sec_desc_mask` (same defaults: owner/group/dacl default `true`,
/// sacl defaults `false`, since SACL access needs an explicit opt-in).
fn sec_desc_mask<'v, 's>(
    strand: &mut Strand<'v, 's>,
    owner: Option<Slot<'v, '_>>,
    group: Option<Slot<'v, '_>>,
    dacl: Option<Slot<'v, '_>>,
    sacl: Option<Slot<'v, '_>>,
) -> Result<'v, 's, u32> {
    fn selected<'v, 's>(
        strand: &mut Strand<'v, 's>,
        value: Option<Slot<'v, '_>>,
        default: bool,
    ) -> Result<'v, 's, bool> {
        value
            .map(|value| {
                value.as_bool(strand).ok_or_else(|| {
                    Error::type_error(strand, "security descriptor component: expected Bool")
                })
            })
            .transpose()
            .map(|value| value.unwrap_or(default))
    }
    let mut mask = 0;
    if selected(strand, owner, true)? {
        mask |= OWNER_SECURITY_INFORMATION;
    }
    if selected(strand, group, true)? {
        mask |= GROUP_SECURITY_INFORMATION;
    }
    if selected(strand, dacl, true)? {
        mask |= DACL_SECURITY_INFORMATION;
    }
    if selected(strand, sacl, false)? {
        mask |= SACL_SECURITY_INFORMATION;
    }
    Ok(mask)
}

fn expect_str<'v, 's>(
    strand: &mut Strand<'v, 's>,
    slot: Slot<'v, '_>,
    what: &str,
) -> Result<'v, 's, String> {
    slot.as_str(strand)
        .map(|s| s.to_string())
        .ok_or_else(|| Error::type_error(strand, format!("{what}: expected Str")))
}

/// Wraps a freshly opened/created [`dolang_vfs_winreg::Key`] into a
/// `winreg.Key`. With a trailing block, the block is called with the handle
/// and the key is auto-closed afterward; without one, the handle is
/// returned directly and the caller owns it.
async fn finish_open<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    inner: dolang_vfs_winreg::Key,
    block: Option<Slot<'v, '_>>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    if let Some(block) = block {
        strand
            .with_slots(async move |strand, [mut handle, mut tmp]| {
                global
                    .types
                    .key
                    .create(strand, Key(Some(inner)), &mut handle);
                let result = call!(strand, block, out, &handle).await;
                let _ = method!(strand, &handle, global.syms.close, &mut tmp).await;
                result
            })
            .await
    } else {
        global.types.key.create(strand, Key(Some(inner)), out);
        Ok(())
    }
}

/// `winreg.open` — bootstraps a [`Key`] from a predefined root.
async fn open<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    args: Args<'v, '_>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let view_sym = global.syms.view;
    let access_sym = global.syms.access;
    let ([root], [block, view, access]) =
        unpack!(strand, args, 1, 1, view_sym = None, access_sym = None)?;
    let root = root_from_sym(strand, global, root)?;
    let view = view_from_sym(strand, global, view)?;
    let access = access_from_value(strand, global, access).await?;
    let vfs = dolang_ext_shell::vfs(strand);
    let inner = dolang_vfs_winreg::Key::open_root(&vfs, root, view, access.into())
        .await
        .into_sys(strand)?;
    finish_open(strand, global, inner, block, out).await
}

impl<'v> Object<'v> for Key {
    const NAME: &'v str = "Key";
    const MODULE: &'v str = "winreg";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let owner = builder.sym("owner");
        let group = builder.sym("group");
        let dacl = builder.sym("dacl");
        let sacl = builder.sym("sacl");
        let all = builder.sym("all");
        let ignore = builder.sym("ignore");
        builder
            .method("open", async move |this, strand, args, out| {
                let global = strand.state::<Global<'v>>();
                let view_sym = global.syms.view;
                let access_sym = global.syms.access;
                let ([subpath], [block, view, access]) =
                    unpack!(strand, args, 1, 1, view_sym = None, access_sym = None)?;
                let subpath = expect_str(strand, subpath, "subpath")?;
                let view = view_from_sym(strand, global, view)?;
                let access = access_from_value(strand, global, access).await?;
                let inner = {
                    let borrow = this.borrow(strand)?;
                    let key = borrow
                        .0
                        .as_ref()
                        .ok_or_else(|| Error::state_error(strand, "key is closed"))?;
                    key.open(&subpath, view, access.into())
                        .await
                        .into_sys(strand)?
                };
                finish_open(strand, global, inner, block, out).await
            })
            .method("create", async move |this, strand, args, out| {
                let global = strand.state::<Global<'v>>();
                let view_sym = global.syms.view;
                let access_sym = global.syms.access;
                let ([subpath], [block, view, access]) =
                    unpack!(strand, args, 1, 1, view_sym = None, access_sym = None)?;
                let subpath = expect_str(strand, subpath, "subpath")?;
                let view = view_from_sym(strand, global, view)?;
                let access = access_from_value(strand, global, access).await?;
                let inner = {
                    let borrow = this.borrow(strand)?;
                    let key = borrow
                        .0
                        .as_ref()
                        .ok_or_else(|| Error::state_error(strand, "key is closed"))?;
                    key.create(&subpath, view, access.into())
                        .await
                        .into_sys(strand)?
                };
                finish_open(strand, global, inner, block, out).await
            })
            .method("delete", async move |this, strand, args, _out| {
                let global = strand.state::<Global<'v>>();
                let view_sym = global.syms.view;
                let ([subpath], [view, all, ignore]) = unpack!(
                    strand,
                    args,
                    1,
                    0,
                    view_sym = None,
                    all = None,
                    ignore = None
                )?;
                let subpath = expect_str(strand, subpath, "subpath")?;
                let view = view_from_sym(strand, global, view)?;
                let all = all
                    .map(|value| {
                        value
                            .as_bool(strand)
                            .ok_or_else(|| Error::type_error(strand, "all: expected Bool"))
                    })
                    .transpose()?
                    .unwrap_or(false);
                let ignore = ignore
                    .map(|value| {
                        value
                            .as_bool(strand)
                            .ok_or_else(|| Error::type_error(strand, "ignore: expected Bool"))
                    })
                    .transpose()?
                    .unwrap_or(false);
                let borrow = this.borrow(strand)?;
                let key = borrow
                    .0
                    .as_ref()
                    .ok_or_else(|| Error::state_error(strand, "key is closed"))?;
                key.delete(&subpath, view, all, ignore)
                    .await
                    .into_sys(strand)
            })
            .method("close", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let inner = this.borrow_mut(strand)?.0.take();
                match inner {
                    Some(inner) => inner.close().await.into_sys(strand),
                    None => Ok(()),
                }
            })
            .method("subkeys", async move |this, strand, args, out| {
                let global = strand.state::<Global<'v>>();
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let names = {
                    let borrow = this.borrow(strand)?;
                    let key = borrow
                        .0
                        .as_ref()
                        .ok_or_else(|| Error::state_error(strand, "key is closed"))?;
                    key.subkeys().await.into_sys(strand)?
                };
                global.types.subkeys.create_with_annex(
                    strand,
                    SubKeys::new(),
                    SubKeysAnnex { names },
                    out,
                );
                Ok(())
            })
            .method("values", async move |this, strand, args, out| {
                let global = strand.state::<Global<'v>>();
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let entries = {
                    let borrow = this.borrow(strand)?;
                    let key = borrow
                        .0
                        .as_ref()
                        .ok_or_else(|| Error::state_error(strand, "key is closed"))?;
                    key.values().await.into_sys(strand)?
                };
                global.types.values.create_with_annex(
                    strand,
                    Values::new(),
                    ValuesAnnex { global, entries },
                    out,
                );
                Ok(())
            })
            .method("get", async move |this, strand, args, out| {
                let ([name], []) = unpack!(strand, args, 1, 0)?;
                let name = expect_str(strand, name, "name")?;
                let value = {
                    let borrow = this.borrow(strand)?;
                    let key = borrow
                        .0
                        .as_ref()
                        .ok_or_else(|| Error::state_error(strand, "key is closed"))?;
                    key.get_value(Some(&name)).await.into_sys(strand)?
                };
                let Some(value) = value else {
                    let error = dolang_vfs::Error::new(
                        dolang_vfs::ErrorKind::NotFound,
                        format!("no such value: {name}"),
                    );
                    return Err(error.into_sys(strand));
                };
                convert::to_do(strand, &value, out)
            })
            .method("get_value", async move |this, strand, args, out| {
                let global = strand.state::<Global<'v>>();
                let ([name], []) = unpack!(strand, args, 1, 0)?;
                let name = expect_str(strand, name, "name")?;
                let value = {
                    let borrow = this.borrow(strand)?;
                    let key = borrow
                        .0
                        .as_ref()
                        .ok_or_else(|| Error::state_error(strand, "key is closed"))?;
                    key.get_value(Some(&name)).await.into_sys(strand)?
                };
                if let Some(value) = value {
                    global.types.value_entry.create_with_annex(
                        strand,
                        ValueEntry,
                        ValueEntryAnnex {
                            global,
                            name,
                            value,
                        },
                        out,
                    );
                }
                Ok(())
            })
            .method("set", async move |this, strand, args, _out| {
                let global = strand.state::<Global<'v>>();
                let kind_sym = global.syms.kind;
                let ([name, value], [kind]) = unpack!(strand, args, 2, 0, kind_sym = None)?;
                let name = expect_str(strand, name, "name")?;
                let kind = kind_from_sym(strand, global, kind)?;
                let borrow = this.borrow(strand)?;
                let key = borrow
                    .0
                    .as_ref()
                    .ok_or_else(|| Error::state_error(strand, "key is closed"))?;
                let existing = key.get_value(Some(&name)).await.into_sys(strand)?;
                let registry_value =
                    convert::from_do(strand, existing.as_ref(), kind, value).await?;
                key.set_value(Some(&name), registry_value)
                    .await
                    .into_sys(strand)
            })
            .method("delete_value", async move |this, strand, args, _out| {
                let ([name], []) = unpack!(strand, args, 1, 0)?;
                let name = expect_str(strand, name, "name")?;
                let borrow = this.borrow(strand)?;
                let key = borrow
                    .0
                    .as_ref()
                    .ok_or_else(|| Error::state_error(strand, "key is closed"))?;
                key.delete_value(Some(&name)).await.into_sys(strand)
            })
            .method("sec_desc", async move |this, strand, args, out| {
                let ([], [owner, group, dacl, sacl]) = unpack!(
                    strand,
                    args,
                    0,
                    0,
                    owner = None,
                    group = None,
                    dacl = None,
                    sacl = None
                )?;
                let mask = sec_desc_mask(strand, owner, group, dacl, sacl)?;
                let descriptor = {
                    let borrow = this.borrow(strand)?;
                    let key = borrow
                        .0
                        .as_ref()
                        .ok_or_else(|| Error::state_error(strand, "key is closed"))?;
                    key.sec_desc(mask).await.into_sys(strand)?
                };
                dolang_ext_shell::create_sec_desc(strand, descriptor, out);
                Ok(())
            })
            .method("set_sec_desc", async move |this, strand, args, _out| {
                let ([descriptor], []) = unpack!(strand, args, 1, 0)?;
                let descriptor = dolang_ext_shell::sec_desc_from_value(strand, &descriptor)?;
                let borrow = this.borrow(strand)?;
                let key = borrow
                    .0
                    .as_ref()
                    .ok_or_else(|| Error::state_error(strand, "key is closed"))?;
                key.set_sec_desc(&descriptor).await.into_sys(strand)
            })
    }
}

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    builder
        .module("winreg")
        .value("AccessMask", global.types.access_mask)
        .value("Key", global.types.key)
        .value("Value", global.types.value_entry)
        .function("open", async move |strand, args, out| {
            open(strand, global, args, out).await
        })
        .commit();
}
