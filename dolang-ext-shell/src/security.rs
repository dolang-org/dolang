use std::fmt;

use dolang::{
    compile::Compiler,
    runtime::{
        Args, Error, Instance, Object, Output, Result, Slot, State, Strand, Type, Value,
        error::ResultExt as _,
        method,
        object::{ArrayLike, ArrayView, Mut, Ref, TypeBuilder},
        unpack,
        value::{AsTuple, Empty, Nil, View},
        vm::Builder,
    },
};
use dolang_shell_vfs::{
    SecurityInfo, Sid as VfsSid, TokenGroup as VfsTokenGroup, UnixSecurityInfo, WindowsTokenInfo,
};

use crate::{error, global::Global};

const SE_GROUP_MANDATORY: u32 = 0x0000_0001;
const SE_GROUP_ENABLED_BY_DEFAULT: u32 = 0x0000_0002;
const SE_GROUP_ENABLED: u32 = 0x0000_0004;
const SE_GROUP_OWNER: u32 = 0x0000_0008;
const SE_GROUP_USE_FOR_DENY_ONLY: u32 = 0x0000_0010;
const SE_GROUP_INTEGRITY: u32 = 0x0000_0020;
const SE_GROUP_INTEGRITY_ENABLED: u32 = 0x0000_0040;
const SE_GROUP_RESOURCE: u32 = 0x2000_0000;
const SE_GROUP_LOGON_ID: u32 = 0xC000_0000;

pub(crate) fn configure_compiler<'a>(_compiler: &mut Compiler<'a>) {}

pub(crate) struct UnixInfo;

impl<'v> Object<'v> for UnixInfo {
    const NAME: &'v str = "UnixInfo";
    const MODULE: &'v str = "security";
    const SLOTS: usize = 1;
    type Annex = UnixSecurityInfo;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("uid", |this, strand, out| {
                Output::set(strand, out, this.annex().uid);
                Ok(())
            })
            .get("gid", |this, strand, out| {
                Output::set(strand, out, this.annex().gid);
                Ok(())
            })
            .get("euid", |this, strand, out| {
                Output::set(strand, out, this.annex().euid);
                Ok(())
            })
            .get("egid", |this, strand, out| {
                Output::set(strand, out, this.annex().egid);
                Ok(())
            })
            .get("group_ids", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                Output::set(strand, out, Ref::slot::<0>(&borrow));
                Ok(())
            })
    }
}

pub(crate) struct Sid;

fn create_sid<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    sid: VfsSid,
    out: &mut Slot<'v, '_>,
) {
    global
        .types
        .sid
        .create_with_annex(strand, Sid, sid, &mut *out);
    let this = global.types.sid.downcast(&*out).unwrap();
    let annex = this.annex();
    let sub_authorities = annex.sub_authorities();
    Output::set(
        strand,
        Mut::slot_mut::<0>(&mut this.borrow_mut_unwrap()),
        AsTuple::new(sub_authorities.iter().copied()),
    );
}

fn sid_from_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
) -> Result<'v, 's, VfsSid> {
    if let Some(value) = value.as_str(strand) {
        value
            .to_string()
            .parse::<VfsSid>()
            .map_err(|error| Error::value(strand, error.to_string()))
    } else if let Some(value) = value.as_bin(strand) {
        let bytes = value.to_vec();
        VfsSid::from_bytes(&bytes).map_err(|error| Error::value(strand, error.to_string()))
    } else {
        Err(Error::type_error(strand, "Sid: expected str or bin"))
    }
}

impl<'v> Object<'v> for Sid {
    const NAME: &'v str = "Sid";
    const MODULE: &'v str = "security";
    const SLOTS: usize = 1;
    type Annex = VfsSid;
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([value], []) = unpack!(strand, args, 1, 0)?;
        let sid = sid_from_value(strand, &value)?;
        this.create_with_annex(strand, Sid, sid, &mut out);
        let this = this.downcast(&out).unwrap();
        let annex = this.annex();
        Output::set(
            strand,
            Mut::slot_mut::<0>(&mut this.borrow_mut_unwrap()),
            AsTuple::new(annex.sub_authorities().iter().copied()),
        );
        Ok(())
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("revision", |this, strand, out| {
                Output::set(strand, out, this.annex().revision());
                Ok(())
            })
            .get("sub_authority_count", |this, strand, out| {
                Output::set(strand, out, this.annex().sub_authorities().len());
                Ok(())
            })
            .get("identifier_authority", |this, strand, out| {
                Output::set(strand, out, this.annex().identifier_authority());
                Ok(())
            })
            .get("sub_authorities", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                Output::set(strand, out, Ref::slot::<0>(&borrow));
                Ok(())
            })
            .method("to_bin", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let bytes = this.annex().to_bytes();
                Output::set(strand, out, bytes.as_slice());
                Ok(())
            })
    }

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "{}", this.annex().as_ref()).into_do(strand)
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<security.Sid {}>", this.annex().as_ref()).into_do(strand)
    }
}

pub(crate) struct TokenGroup;

impl<'v> Object<'v> for TokenGroup {
    const NAME: &'v str = "TokenGroup";
    const MODULE: &'v str = "security";
    const SLOTS: usize = 1;
    type Annex = VfsTokenGroup;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        fn flag<'v, 's>(
            this: Instance<'v, '_, TokenGroup>,
            strand: &mut Strand<'v, 's>,
            out: impl Output<'v>,
            mask: u32,
        ) -> Result<'v, 's, ()> {
            Output::set(strand, out, this.annex().attributes & mask != 0);
            Ok(())
        }

        builder
            .get("sid", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                Output::set(strand, out, Ref::slot::<0>(&borrow));
                Ok(())
            })
            .get("attributes", |this, strand, out| {
                Output::set(strand, out, this.annex().attributes);
                Ok(())
            })
            .get("mandatory", |this, strand, out| {
                flag(this, strand, out, SE_GROUP_MANDATORY)
            })
            .get("enabled_by_default", |this, strand, out| {
                flag(this, strand, out, SE_GROUP_ENABLED_BY_DEFAULT)
            })
            .get("enabled", |this, strand, out| {
                flag(this, strand, out, SE_GROUP_ENABLED)
            })
            .get("owner", |this, strand, out| {
                flag(this, strand, out, SE_GROUP_OWNER)
            })
            .get("use_for_deny_only", |this, strand, out| {
                flag(this, strand, out, SE_GROUP_USE_FOR_DENY_ONLY)
            })
            .get("integrity", |this, strand, out| {
                flag(this, strand, out, SE_GROUP_INTEGRITY)
            })
            .get("integrity_enabled", |this, strand, out| {
                flag(this, strand, out, SE_GROUP_INTEGRITY_ENABLED)
            })
            .get("resource", |this, strand, out| {
                flag(this, strand, out, SE_GROUP_RESOURCE)
            })
            .get("logon_id", |this, strand, out| {
                Output::set(
                    strand,
                    out,
                    this.annex().attributes & SE_GROUP_LOGON_ID == SE_GROUP_LOGON_ID,
                );
                Ok(())
            })
    }
}

pub(crate) struct TokenInfo;

struct TokenGroups;

impl<'v> ArrayLike<'v> for TokenGroups {
    type Object = TokenInfo;

    const MODULE: &'v str = "security";
    const NAME: &'v str = "TokenGroups";

    fn len(this: Instance<'v, '_, Self::Object>, _strand: &mut Strand<'v, '_>) -> usize {
        this.annex().groups.len()
    }

    fn get<'a, 's>(
        this: Instance<'v, '_, Self::Object>,
        strand: &'a mut Strand<'v, 's>,
        index: usize,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let token_group = this
            .annex()
            .groups
            .get(index)
            .expect("array view index was normalized")
            .clone();
        let global = strand.state::<Global<'v>>();
        strand.with_slots_sync(|strand, [mut sid]| {
            create_sid(strand, global, token_group.sid.clone(), &mut sid);
            global
                .types
                .token_group
                .create_with_annex(strand, TokenGroup, token_group, &mut out);
            let group = global.types.token_group.downcast(&out).unwrap();
            Output::set(
                strand,
                Mut::slot_mut::<0>(&mut group.borrow_mut_unwrap()),
                &sid,
            );
            Ok(())
        })
    }
}

impl<'v> Object<'v> for TokenInfo {
    const NAME: &'v str = "TokenInfo";
    const MODULE: &'v str = "security";
    const SLOTS: usize = 4;
    type Annex = WindowsTokenInfo;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("is_elevated", |this, strand, out| {
                Output::set(strand, out, this.annex().is_elevated);
                Ok(())
            })
            .get("user_sid", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                Output::set(strand, out, Ref::slot::<0>(&borrow));
                Ok(())
            })
            .get("owner_sid", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                Output::set(strand, out, Ref::slot::<1>(&borrow));
                Ok(())
            })
            .get("primary_group_sid", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                Output::set(strand, out, Ref::slot::<2>(&borrow));
                Ok(())
            })
            .get("groups", |this, strand, out| {
                Output::set(strand, out, ArrayView::<TokenGroups>::new(this));
                Ok(())
            })
            .get("logon_sid", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                let sid = Ref::slot::<3>(&borrow);
                if sid.is_nil() {
                    Output::set(strand, out, Nil);
                } else {
                    Output::set(strand, out, sid);
                }
                Ok(())
            })
    }
}

fn security_info<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
) -> Result<'v, 's, SecurityInfo> {
    if let Some(security) = global.local.get(strand).security() {
        return Ok(security);
    }
    let security = error::io_result(strand, SecurityInfo::current())?;
    global
        .local
        .get(strand)
        .replace_security(Some(security.clone()));
    Ok(security)
}

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    let tuple = builder.sym("tuple");

    builder
        .module("security")
        .function_with_slots(
            "unix_info",
            async move |strand, args, mut out, [mut std, mut group_ids, mut tmp]| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let SecurityInfo::Unix(info) = security_info(strand, global)? else {
                    return Err(Error::not_supported(strand));
                };

                strand.import("std", &mut std).await?;
                Output::set(strand, &mut group_ids, Empty::Array);
                {
                    let View::Array(group_ids_view) = group_ids.view(strand.vm()) else {
                        unreachable!("Empty::Array did not create an array")
                    };
                    for group_id in &info.group_ids {
                        group_ids_view.push(strand, *group_id)?;
                    }
                }
                method!(strand, &std, tuple, &mut tmp, &group_ids).await?;

                global
                    .types
                    .unix_info
                    .create_with_annex(strand, UnixInfo, info, &mut out);
                let this = global.types.unix_info.downcast(&out).unwrap();
                Output::set(
                    strand,
                    Mut::slot_mut::<0>(&mut this.borrow_mut_unwrap()),
                    &tmp,
                );
                Ok(())
            },
        )
        .function_with_slots(
            "token_info",
            async move |strand, args, mut out, [mut sid]| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let SecurityInfo::Windows(info) = security_info(strand, global)? else {
                    return Err(Error::not_supported(strand));
                };

                global.types.token_info.create_with_annex(
                    strand,
                    TokenInfo,
                    info.clone(),
                    &mut out,
                );
                let this = global.types.token_info.downcast(&out).unwrap();

                for (slot, value) in [
                    (0, info.user_sid.clone()),
                    (1, info.owner_sid.clone()),
                    (2, info.primary_group_sid.clone()),
                ] {
                    create_sid(strand, global, value, &mut sid);
                    let mut borrow = this.borrow_mut_unwrap();
                    match slot {
                        0 => Output::set(strand, Mut::slot_mut::<0>(&mut borrow), &sid),
                        1 => Output::set(strand, Mut::slot_mut::<1>(&mut borrow), &sid),
                        2 => Output::set(strand, Mut::slot_mut::<2>(&mut borrow), &sid),
                        _ => unreachable!(),
                    }
                }

                if let Some(logon_sid) = info.logon_sid().cloned() {
                    create_sid(strand, global, logon_sid, &mut sid);
                    Output::set(
                        strand,
                        Mut::slot_mut::<3>(&mut this.borrow_mut_unwrap()),
                        &sid,
                    );
                }
                Ok(())
            },
        )
        .value("UnixInfo", global.types.unix_info)
        .value("Sid", global.types.sid)
        .value("TokenGroup", global.types.token_group)
        .value("TokenInfo", global.types.token_info)
        .commit();
}
