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
    OperatingSystemFamily, SecDesc as VfsSecDesc, SecurityInfo, Sid as VfsSid,
    SidName as VfsSidName, SidNameUse, TokenGroup as VfsTokenGroup, UnixSecurityInfo, Vfs as _,
    WindowsTokenInfo,
};

use crate::{error, global::Global, util};

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
            .method("lookup", async move |this, strand, args, mut out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let sid = this.annex().clone();
                let global = strand.state::<Global<'v>>();
                if global.local.get(strand).target().operating_system.family()
                    != OperatingSystemFamily::Windows
                {
                    return Err(Error::not_supported(strand));
                }
                let vfs = global.local.get(strand).vfs();
                let name = error::io_result(strand, vfs.sid_name(&sid).await)?;
                create_sid_name(strand, global, name, &mut out);
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

pub(crate) struct SecDesc;

impl<'v> Object<'v> for SecDesc {
    const NAME: &'v str = "SecDesc";
    const MODULE: &'v str = "security";
    type Annex = VfsSecDesc;
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([value], []) = unpack!(strand, args, 1, 0)?;
        let Some(value) = value.as_bin(strand) else {
            return Err(Error::type_error(strand, "SecDesc: expected bin"));
        };
        let bytes = value.to_vec();
        let descriptor = VfsSecDesc::from_bytes(&bytes)
            .map_err(|error| Error::value(strand, error.to_string()))?;
        this.create_with_annex(strand, SecDesc, descriptor, out);
        Ok(())
    }

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        fn control_field<'v, 's>(
            _this: Instance<'v, '_, SecDesc>,
            strand: &mut Strand<'v, 's>,
            out: impl Output<'v>,
            field: dolang::runtime::Sym<'v, '_>,
            loaded: bool,
            value: bool,
        ) -> Result<'v, 's, ()> {
            if !loaded {
                return Err(Error::field(strand, field));
            }
            Output::set(strand, out, value);
            Ok(())
        }

        let rm_control = builder.sym("rm_control");
        let owner = builder.sym("owner");
        let group = builder.sym("group");
        let owner_defaulted = builder.sym("owner_defaulted");
        let group_defaulted = builder.sym("group_defaulted");
        let dacl_present = builder.sym("dacl_present");
        let dacl_defaulted = builder.sym("dacl_defaulted");
        let dacl_auto_inherit_required = builder.sym("dacl_auto_inherit_required");
        let dacl_auto_inherited = builder.sym("dacl_auto_inherited");
        let dacl_protected = builder.sym("dacl_protected");
        let sacl_present = builder.sym("sacl_present");
        let sacl_defaulted = builder.sym("sacl_defaulted");
        let sacl_auto_inherit_required = builder.sym("sacl_auto_inherit_required");
        let sacl_auto_inherited = builder.sym("sacl_auto_inherited");
        let sacl_protected = builder.sym("sacl_protected");

        builder
            .get("revision", |this, strand, out| {
                Output::set(strand, out, this.annex().revision());
                Ok(())
            })
            .get("control", |this, strand, out| {
                Output::set(strand, out, this.annex().control());
                Ok(())
            })
            .get("mask", |this, strand, out| {
                Output::set(strand, out, this.annex().mask());
                Ok(())
            })
            .get("rm_control_valid", |this, strand, out| {
                Output::set(strand, out, this.annex().rm_control_valid());
                Ok(())
            })
            .get("rm_control", move |this, strand, out| {
                util::option_field(strand, this.annex().rm_control(), rm_control, out)
            })
            .get("owner", move |this, strand, mut out| {
                let descriptor = this.annex();
                let Some(value) = descriptor.owner().filter(|_| descriptor.owner_loaded()) else {
                    return Err(Error::field(strand, owner));
                };
                let global = strand.state::<Global<'v>>();
                create_sid(strand, global, value.clone(), &mut out);
                Ok(())
            })
            .get("group", move |this, strand, mut out| {
                let descriptor = this.annex();
                let Some(value) = descriptor.group().filter(|_| descriptor.group_loaded()) else {
                    return Err(Error::field(strand, group));
                };
                let global = strand.state::<Global<'v>>();
                create_sid(strand, global, value.clone(), &mut out);
                Ok(())
            })
            .get("owner_defaulted", move |this, strand, out| {
                control_field(
                    this,
                    strand,
                    out,
                    owner_defaulted,
                    this.annex().owner_loaded(),
                    this.annex().owner_defaulted(),
                )
            })
            .get("group_defaulted", move |this, strand, out| {
                control_field(
                    this,
                    strand,
                    out,
                    group_defaulted,
                    this.annex().group_loaded(),
                    this.annex().group_defaulted(),
                )
            })
            .get("dacl_present", move |this, strand, out| {
                control_field(
                    this,
                    strand,
                    out,
                    dacl_present,
                    this.annex().dacl_loaded(),
                    this.annex().dacl_present(),
                )
            })
            .get("dacl_defaulted", move |this, strand, out| {
                control_field(
                    this,
                    strand,
                    out,
                    dacl_defaulted,
                    this.annex().dacl_loaded(),
                    this.annex().dacl_defaulted(),
                )
            })
            .get("dacl_auto_inherit_required", move |this, strand, out| {
                control_field(
                    this,
                    strand,
                    out,
                    dacl_auto_inherit_required,
                    this.annex().dacl_loaded(),
                    this.annex().dacl_auto_inherit_required(),
                )
            })
            .get("dacl_auto_inherited", move |this, strand, out| {
                control_field(
                    this,
                    strand,
                    out,
                    dacl_auto_inherited,
                    this.annex().dacl_loaded(),
                    this.annex().dacl_auto_inherited(),
                )
            })
            .get("dacl_protected", move |this, strand, out| {
                control_field(
                    this,
                    strand,
                    out,
                    dacl_protected,
                    this.annex().dacl_loaded(),
                    this.annex().dacl_protected(),
                )
            })
            .get("sacl_present", move |this, strand, out| {
                control_field(
                    this,
                    strand,
                    out,
                    sacl_present,
                    this.annex().sacl_loaded(),
                    this.annex().sacl_present(),
                )
            })
            .get("sacl_defaulted", move |this, strand, out| {
                control_field(
                    this,
                    strand,
                    out,
                    sacl_defaulted,
                    this.annex().sacl_loaded(),
                    this.annex().sacl_defaulted(),
                )
            })
            .get("sacl_auto_inherit_required", move |this, strand, out| {
                control_field(
                    this,
                    strand,
                    out,
                    sacl_auto_inherit_required,
                    this.annex().sacl_loaded(),
                    this.annex().sacl_auto_inherit_required(),
                )
            })
            .get("sacl_auto_inherited", move |this, strand, out| {
                control_field(
                    this,
                    strand,
                    out,
                    sacl_auto_inherited,
                    this.annex().sacl_loaded(),
                    this.annex().sacl_auto_inherited(),
                )
            })
            .get("sacl_protected", move |this, strand, out| {
                control_field(
                    this,
                    strand,
                    out,
                    sacl_protected,
                    this.annex().sacl_loaded(),
                    this.annex().sacl_protected(),
                )
            })
            .method("to_bin", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let bytes = this.annex().to_bytes();
                Output::set(strand, out, bytes.as_slice());
                Ok(())
            })
    }
}

pub(crate) struct SidName;

fn create_sid_name<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    name: VfsSidName,
    out: &mut Slot<'v, '_>,
) {
    global
        .types
        .sid_name
        .create_with_annex(strand, SidName, name, out);
}

impl<'v> Object<'v> for SidName {
    const NAME: &'v str = "SidName";
    const MODULE: &'v str = "security";
    type Annex = VfsSidName;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let user = builder.sym("USER");
        let group = builder.sym("GROUP");
        let domain = builder.sym("DOMAIN");
        let alias = builder.sym("ALIAS");
        let well_known_group = builder.sym("WELL_KNOWN_GROUP");
        let deleted_account = builder.sym("DELETED_ACCOUNT");
        let invalid = builder.sym("INVALID");
        let unknown = builder.sym("UNKNOWN");
        let computer = builder.sym("COMPUTER");
        let label = builder.sym("LABEL");
        let logon_session = builder.sym("LOGON_SESSION");
        builder
            .get("sid", |this, strand, mut out| {
                let global = strand.state::<Global<'v>>();
                create_sid(strand, global, this.annex().sid.clone(), &mut out);
                Ok(())
            })
            .get("name", |this, strand, out| {
                Output::set(strand, out, this.annex().name.as_str());
                Ok(())
            })
            .get("domain", |this, strand, out| {
                Output::set(strand, out, this.annex().domain.as_str());
                Ok(())
            })
            .get("qualified_name", |this, strand, out| {
                if this.annex().domain.is_empty() {
                    Output::set(strand, out, this.annex().name.as_str());
                } else {
                    let name = format!("{}\\{}", this.annex().domain, this.annex().name);
                    Output::set(strand, out, name.as_str());
                }
                Ok(())
            })
            .get("kind", move |this, strand, out| {
                let kind = match this.annex().kind {
                    SidNameUse::User => user,
                    SidNameUse::Group => group,
                    SidNameUse::Domain => domain,
                    SidNameUse::Alias => alias,
                    SidNameUse::WellKnownGroup => well_known_group,
                    SidNameUse::DeletedAccount => deleted_account,
                    SidNameUse::Invalid => invalid,
                    SidNameUse::Unknown => unknown,
                    SidNameUse::Computer => computer,
                    SidNameUse::Label => label,
                    SidNameUse::LogonSession => logon_session,
                };
                Output::set(strand, out, kind);
                Ok(())
            })
            .type_method("lookup", async move |_this, strand, args, mut out| {
                let ([value], []) = unpack!(strand, args, 1, 0)?;
                let global = strand.state::<Global<'v>>();
                if global.local.get(strand).target().operating_system.family()
                    != OperatingSystemFamily::Windows
                {
                    return Err(Error::not_supported(strand));
                }
                let vfs = global.local.get(strand).vfs();
                let name = if let Some(sid) = global.types.sid.downcast(&value) {
                    let sid = sid.annex().clone();
                    error::io_result(strand, vfs.sid_name(&sid).await)?
                } else if let Some(value) = value.as_str(strand) {
                    let value = value.to_string();
                    error::io_result(strand, vfs.account_name(&value).await)?
                } else {
                    return Err(Error::type_error(
                        strand,
                        "SidName.lookup: expected Sid or str",
                    ));
                };
                create_sid_name(strand, global, name, &mut out);
                Ok(())
            })
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
        .function("user_name", async move |strand, args, out| {
            let ([], [uid]) = unpack!(strand, args, 0, 1)?;
            let family = global.local.get(strand).target().operating_system.family();
            let vfs = global.local.get(strand).vfs();
            let name = match (family, uid) {
                (OperatingSystemFamily::Unix, Some(uid)) => {
                    let uid = uid.to_u32(strand)?;
                    error::io_result(strand, vfs.user_name(uid).await)?
                }
                (OperatingSystemFamily::Unix, None) => {
                    let SecurityInfo::Unix(info) = security_info(strand, global)? else {
                        unreachable!("Unix target returned Windows security information")
                    };
                    error::io_result(strand, vfs.user_name(info.uid).await)?
                }
                (OperatingSystemFamily::Windows, Some(_)) => {
                    return Err(Error::not_supported(strand));
                }
                (OperatingSystemFamily::Windows, None) => {
                    let SecurityInfo::Windows(info) = security_info(strand, global)? else {
                        unreachable!("Windows target returned Unix security information")
                    };
                    error::io_result(strand, vfs.sid_name(&info.user_sid).await)?.name
                }
            };
            Output::set(strand, out, name.as_str());
            Ok(())
        })
        .function("user_id", async move |strand, args, out| {
            let ([name], []) = unpack!(strand, args, 1, 0)?;
            if global.local.get(strand).target().operating_system.family()
                != OperatingSystemFamily::Unix
            {
                return Err(Error::not_supported(strand));
            }
            let name = name
                .as_str(strand)
                .ok_or_else(|| Error::type_error(strand, "user_id: expected str"))?
                .to_string();
            let vfs = global.local.get(strand).vfs();
            let uid = error::io_result(strand, vfs.user_id(&name).await)?;
            Output::set(strand, out, uid);
            Ok(())
        })
        .function("group_name", async move |strand, args, out| {
            let ([gid], []) = unpack!(strand, args, 1, 0)?;
            if global.local.get(strand).target().operating_system.family()
                != OperatingSystemFamily::Unix
            {
                return Err(Error::not_supported(strand));
            }
            let gid = gid.to_u32(strand)?;
            let vfs = global.local.get(strand).vfs();
            let name = error::io_result(strand, vfs.group_name(gid).await)?;
            Output::set(strand, out, name.as_str());
            Ok(())
        })
        .function("group_id", async move |strand, args, out| {
            let ([name], []) = unpack!(strand, args, 1, 0)?;
            if global.local.get(strand).target().operating_system.family()
                != OperatingSystemFamily::Unix
            {
                return Err(Error::not_supported(strand));
            }
            let name = name
                .as_str(strand)
                .ok_or_else(|| Error::type_error(strand, "group_id: expected str"))?
                .to_string();
            let vfs = global.local.get(strand).vfs();
            let gid = error::io_result(strand, vfs.group_id(&name).await)?;
            Output::set(strand, out, gid);
            Ok(())
        })
        .value("UnixInfo", global.types.unix_info)
        .value("SecDesc", global.types.sec_desc)
        .value("Sid", global.types.sid)
        .value("SidName", global.types.sid_name)
        .value("TokenGroup", global.types.token_group)
        .value("TokenInfo", global.types.token_info)
        .commit();
}
