use std::hash::{Hash, Hasher};

use dolang::runtime::object::fmt;

use dolang::{
    compile::Compiler,
    runtime::{
        Args, Error, Instance, Object, Output, Result, Slot, State, Strand, Type, Value, method,
        object::{ArrayLike, ArrayView, Mut, Ref, TypeBuilder},
        unpack,
        value::{AsTuple, Empty, Nil, View},
        vm::Builder,
    },
};
use dolang_shell_vfs::{
    Ace as VfsAce, AceType as VfsAceType, Acl as VfsAcl, Guid as VfsGuid, OperatingSystemFamily,
    SecDesc as VfsSecDesc, SecurityInfo, Sid as VfsSid, SidName as VfsSidName, SidNameUse,
    TokenGroup as VfsTokenGroup, UnixSecurityInfo, Vfs as _, WindowsTokenInfo,
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
        w: &mut dyn dolang::runtime::Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "{}", this.annex().as_ref())
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn dolang::runtime::Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "<security.Sid {}>", this.annex().as_ref())
    }
}

pub(crate) struct Guid;

fn create_guid<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    guid: VfsGuid,
    out: &mut Slot<'v, '_>,
) {
    global.types.guid.create_with_annex(strand, Guid, guid, out);
}

impl<'v> Object<'v> for Guid {
    const NAME: &'v str = "Guid";
    const MODULE: &'v str = "security";
    type Annex = VfsGuid;
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([value], []) = unpack!(strand, args, 1, 0)?;
        let guid = if let Some(value) = value.as_str(strand) {
            value
                .to_string()
                .parse::<VfsGuid>()
                .map_err(|error| Error::value(strand, error.to_string()))?
        } else if let Some(value) = value.as_bin(strand) {
            VfsGuid::from_bytes(&value.to_vec())
                .map_err(|error| Error::value(strand, error.to_string()))?
        } else {
            return Err(Error::type_error(strand, "Guid: expected str or bin"));
        };
        this.create_with_annex(strand, Guid, guid, out);
        Ok(())
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.method("to_bin", async move |this, strand, args, out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            Output::set(strand, out, this.annex().as_bytes().as_slice());
            Ok(())
        })
    }

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn dolang::runtime::Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "{}", &*this.annex())
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn dolang::runtime::Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "<security.Guid {}>", &*this.annex())
    }

    fn eq<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        let global = strand.state::<Global<'v>>();
        let Some(other) = global.types.guid.downcast(other) else {
            return Err(Error::not_supported(strand));
        };
        Ok(*this.annex() == *other.annex())
    }

    fn hash<'a, 's>(
        this: Instance<'v, 'a, Self>,
        _strand: &'a mut Strand<'v, 's>,
        hasher: &mut impl Hasher,
    ) -> Result<'v, 's, ()> {
        (*this.annex()).hash(hasher);
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub(crate) enum AclComponent {
    Dacl,
    Sacl,
}

pub(crate) struct Acl;

fn create_acl<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    descriptor: Instance<'v, '_, SecDesc>,
    component: AclComponent,
    out: &mut Slot<'v, '_>,
) {
    global
        .types
        .acl
        .create_with_annex(strand, Acl, component, &mut *out);
    let acl = global.types.acl.downcast(&*out).unwrap();
    Output::set(
        strand,
        Mut::slot_mut::<0>(&mut acl.borrow_mut_unwrap()),
        descriptor,
    );
}

fn with_acl<'v, 's, T>(
    this: Instance<'v, '_, Acl>,
    strand: &mut Strand<'v, 's>,
    f: impl FnOnce(VfsAcl<'_>) -> T,
) -> Result<'v, 's, T> {
    let global = strand.state::<Global<'v>>();
    let borrow = this.borrow(strand)?;
    let descriptor = global
        .types
        .sec_desc
        .downcast(Ref::slot::<0>(&borrow))
        .expect("Acl root is a SecDesc");
    let descriptor = descriptor.annex();
    let acl = match *this.annex() {
        AclComponent::Dacl => descriptor.dacl(),
        AclComponent::Sacl => descriptor.sacl(),
    }
    .expect("Acl component is non-null");
    Ok(f(acl))
}

struct AclAces;

impl<'v> ArrayLike<'v> for AclAces {
    type Object = Acl;

    const MODULE: &'v str = "security";
    const NAME: &'v str = "AclAces";

    fn len(this: Instance<'v, '_, Acl>, strand: &mut Strand<'v, '_>) -> usize {
        with_acl(this, strand, |acl| usize::from(acl.ace_count())).unwrap()
    }

    fn get<'a, 's>(
        this: Instance<'v, '_, Acl>,
        strand: &'a mut Strand<'v, 's>,
        index: usize,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = strand.state::<Global<'v>>();
        global.types.ace.create_with_annex(
            strand,
            Ace,
            AceAnnex {
                component: *this.annex(),
                index,
            },
            &mut out,
        );
        let ace = global.types.ace.downcast(&out).unwrap();
        let borrow = this.borrow(strand)?;
        Output::set(
            strand,
            Mut::slot_mut::<0>(&mut ace.borrow_mut_unwrap()),
            Ref::slot::<0>(&borrow),
        );
        Ok(())
    }
}

impl<'v> Object<'v> for Acl {
    const NAME: &'v str = "Acl";
    const MODULE: &'v str = "security";
    const SLOTS: usize = 1;
    type Annex = AclComponent;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("revision", |this, strand, out| {
                let revision = with_acl(this, strand, |acl| acl.revision())?;
                Output::set(strand, out, revision);
                Ok(())
            })
            .get("size", |this, strand, out| {
                let size = with_acl(this, strand, |acl| acl.size())?;
                Output::set(strand, out, size);
                Ok(())
            })
            .get("ace_count", |this, strand, out| {
                let count = with_acl(this, strand, |acl| acl.ace_count())?;
                Output::set(strand, out, count);
                Ok(())
            })
            .get("aces", |this, strand, out| {
                Output::set(strand, out, ArrayView::<AclAces>::new(this));
                Ok(())
            })
            .method("to_bin", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let bytes = with_acl(this, strand, |acl| acl.as_bytes().to_vec())?;
                Output::set(strand, out, bytes.as_slice());
                Ok(())
            })
    }
}

#[derive(Clone, Copy)]
pub(crate) struct AceAnnex {
    component: AclComponent,
    index: usize,
}

pub(crate) struct Ace;

fn with_ace<'v, 's, T>(
    this: Instance<'v, '_, Ace>,
    strand: &mut Strand<'v, 's>,
    f: impl FnOnce(VfsAce<'_>) -> T,
) -> Result<'v, 's, T> {
    let global = strand.state::<Global<'v>>();
    let borrow = this.borrow(strand)?;
    let descriptor = global
        .types
        .sec_desc
        .downcast(Ref::slot::<0>(&borrow))
        .expect("Ace root is a SecDesc");
    let descriptor = descriptor.annex();
    let acl = match this.annex().component {
        AclComponent::Dacl => descriptor.dacl(),
        AclComponent::Sacl => descriptor.sacl(),
    }
    .expect("Ace ACL component is non-null");
    let ace = acl
        .aces()
        .nth(this.annex().index)
        .expect("Ace array index was normalized");
    Ok(f(ace))
}

impl<'v> Object<'v> for Ace {
    const NAME: &'v str = "Ace";
    const MODULE: &'v str = "security";
    const SLOTS: usize = 1;
    type Annex = AceAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let mask_field = builder.sym("mask");
        let sid_field = builder.sym("sid");
        let object_flags_field = builder.sym("object_flags");
        let object_type_field = builder.sym("object_type");
        let inherited_object_type_field = builder.sym("inherited_object_type");
        let application_data_field = builder.sym("application_data");
        let successful_access_field = builder.sym("successful_access");
        let failed_access_field = builder.sym("failed_access");
        let trust_protected_filter_field = builder.sym("trust_protected_filter");

        let access_allowed = builder.sym("ACCESS_ALLOWED");
        let access_denied = builder.sym("ACCESS_DENIED");
        let system_audit = builder.sym("SYSTEM_AUDIT");
        let system_alarm = builder.sym("SYSTEM_ALARM");
        let access_allowed_compound = builder.sym("ACCESS_ALLOWED_COMPOUND");
        let access_allowed_object = builder.sym("ACCESS_ALLOWED_OBJECT");
        let access_denied_object = builder.sym("ACCESS_DENIED_OBJECT");
        let system_audit_object = builder.sym("SYSTEM_AUDIT_OBJECT");
        let system_alarm_object = builder.sym("SYSTEM_ALARM_OBJECT");
        let access_allowed_callback = builder.sym("ACCESS_ALLOWED_CALLBACK");
        let access_denied_callback = builder.sym("ACCESS_DENIED_CALLBACK");
        let access_allowed_callback_object = builder.sym("ACCESS_ALLOWED_CALLBACK_OBJECT");
        let access_denied_callback_object = builder.sym("ACCESS_DENIED_CALLBACK_OBJECT");
        let system_audit_callback = builder.sym("SYSTEM_AUDIT_CALLBACK");
        let system_alarm_callback = builder.sym("SYSTEM_ALARM_CALLBACK");
        let system_audit_callback_object = builder.sym("SYSTEM_AUDIT_CALLBACK_OBJECT");
        let system_alarm_callback_object = builder.sym("SYSTEM_ALARM_CALLBACK_OBJECT");
        let system_mandatory_label = builder.sym("SYSTEM_MANDATORY_LABEL");
        let system_resource_attribute = builder.sym("SYSTEM_RESOURCE_ATTRIBUTE");
        let system_scoped_policy_id = builder.sym("SYSTEM_SCOPED_POLICY_ID");
        let system_process_trust_label = builder.sym("SYSTEM_PROCESS_TRUST_LABEL");
        let system_access_filter = builder.sym("SYSTEM_ACCESS_FILTER");
        let unknown = builder.sym("UNKNOWN");

        builder
            .get("type", move |this, strand, out| {
                let ace_type = with_ace(this, strand, |ace| ace.ace_type())?;
                let value = match ace_type {
                    VfsAceType::AccessAllowed => access_allowed,
                    VfsAceType::AccessDenied => access_denied,
                    VfsAceType::SystemAudit => system_audit,
                    VfsAceType::SystemAlarm => system_alarm,
                    VfsAceType::AccessAllowedCompound => access_allowed_compound,
                    VfsAceType::AccessAllowedObject => access_allowed_object,
                    VfsAceType::AccessDeniedObject => access_denied_object,
                    VfsAceType::SystemAuditObject => system_audit_object,
                    VfsAceType::SystemAlarmObject => system_alarm_object,
                    VfsAceType::AccessAllowedCallback => access_allowed_callback,
                    VfsAceType::AccessDeniedCallback => access_denied_callback,
                    VfsAceType::AccessAllowedCallbackObject => access_allowed_callback_object,
                    VfsAceType::AccessDeniedCallbackObject => access_denied_callback_object,
                    VfsAceType::SystemAuditCallback => system_audit_callback,
                    VfsAceType::SystemAlarmCallback => system_alarm_callback,
                    VfsAceType::SystemAuditCallbackObject => system_audit_callback_object,
                    VfsAceType::SystemAlarmCallbackObject => system_alarm_callback_object,
                    VfsAceType::SystemMandatoryLabel => system_mandatory_label,
                    VfsAceType::SystemResourceAttribute => system_resource_attribute,
                    VfsAceType::SystemScopedPolicyId => system_scoped_policy_id,
                    VfsAceType::SystemProcessTrustLabel => system_process_trust_label,
                    VfsAceType::SystemAccessFilter => system_access_filter,
                    VfsAceType::Unknown(_) => unknown,
                };
                Output::set(strand, out, value);
                Ok(())
            })
            .get("type_code", |this, strand, out| {
                let value = with_ace(this, strand, |ace| ace.type_code())?;
                Output::set(strand, out, value);
                Ok(())
            })
            .get("flags", |this, strand, out| {
                let value = with_ace(this, strand, |ace| ace.flags())?;
                Output::set(strand, out, value);
                Ok(())
            })
            .get("size", |this, strand, out| {
                let value = with_ace(this, strand, |ace| ace.size())?;
                Output::set(strand, out, value);
                Ok(())
            })
            .get("mask", move |this, strand, out| {
                let Some(value) = with_ace(this, strand, |ace| ace.mask())? else {
                    return Err(Error::field(strand, mask_field));
                };
                Output::set(strand, out, value);
                Ok(())
            })
            .get("sid", move |this, strand, mut out| {
                let Some(value) = with_ace(this, strand, |ace| ace.sid())? else {
                    return Err(Error::field(strand, sid_field));
                };
                let global = strand.state::<Global<'v>>();
                create_sid(strand, global, value, &mut out);
                Ok(())
            })
            .get("object_flags", move |this, strand, out| {
                let Some(value) = with_ace(this, strand, |ace| ace.object_flags())? else {
                    return Err(Error::field(strand, object_flags_field));
                };
                Output::set(strand, out, value);
                Ok(())
            })
            .get("object_type", move |this, strand, mut out| {
                let (flags, value) =
                    with_ace(this, strand, |ace| (ace.object_flags(), ace.object_type()))?;
                if flags.is_none() {
                    return Err(Error::field(strand, object_type_field));
                }
                if let Some(value) = value {
                    let global = strand.state::<Global<'v>>();
                    create_guid(strand, global, value, &mut out);
                } else {
                    Output::set(strand, out, Nil);
                }
                Ok(())
            })
            .get("inherited_object_type", move |this, strand, mut out| {
                let (flags, value) = with_ace(this, strand, |ace| {
                    (ace.object_flags(), ace.inherited_object_type())
                })?;
                if flags.is_none() {
                    return Err(Error::field(strand, inherited_object_type_field));
                }
                if let Some(value) = value {
                    let global = strand.state::<Global<'v>>();
                    create_guid(strand, global, value, &mut out);
                } else {
                    Output::set(strand, out, Nil);
                }
                Ok(())
            })
            .get("application_data", move |this, strand, out| {
                let Some(value) = with_ace(this, strand, |ace| {
                    ace.application_data().map(<[u8]>::to_vec)
                })?
                else {
                    return Err(Error::field(strand, application_data_field));
                };
                Output::set(strand, out, value.as_slice());
                Ok(())
            })
            .get("object_inherit", |this, strand, out| {
                ace_flag(this, strand, out, 0x01)
            })
            .get("container_inherit", |this, strand, out| {
                ace_flag(this, strand, out, 0x02)
            })
            .get("no_propagate_inherit", |this, strand, out| {
                ace_flag(this, strand, out, 0x04)
            })
            .get("inherit_only", |this, strand, out| {
                ace_flag(this, strand, out, 0x08)
            })
            .get("inherited", |this, strand, out| {
                ace_flag(this, strand, out, 0x10)
            })
            .get("critical", |this, strand, out| {
                ace_flag(this, strand, out, 0x20)
            })
            .get("successful_access", move |this, strand, out| {
                let (kind, flags) = with_ace(this, strand, |ace| (ace.ace_type(), ace.flags()))?;
                if !ace_is_audit(kind) {
                    return Err(Error::field(strand, successful_access_field));
                }
                Output::set(strand, out, flags & 0x40 != 0);
                Ok(())
            })
            .get("failed_access", move |this, strand, out| {
                let (kind, flags) = with_ace(this, strand, |ace| (ace.ace_type(), ace.flags()))?;
                if !ace_is_audit(kind) {
                    return Err(Error::field(strand, failed_access_field));
                }
                Output::set(strand, out, flags & 0x80 != 0);
                Ok(())
            })
            .get("trust_protected_filter", move |this, strand, out| {
                let (kind, flags) = with_ace(this, strand, |ace| (ace.ace_type(), ace.flags()))?;
                if kind != VfsAceType::SystemAccessFilter {
                    return Err(Error::field(strand, trust_protected_filter_field));
                }
                Output::set(strand, out, flags & 0x40 != 0);
                Ok(())
            })
            .method("to_bin", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let bytes = with_ace(this, strand, |ace| ace.as_bytes().to_vec())?;
                Output::set(strand, out, bytes.as_slice());
                Ok(())
            })
    }
}

fn ace_flag<'v, 's>(
    this: Instance<'v, '_, Ace>,
    strand: &mut Strand<'v, 's>,
    out: impl Output<'v>,
    flag: u8,
) -> Result<'v, 's, ()> {
    let flags = with_ace(this, strand, |ace| ace.flags())?;
    Output::set(strand, out, flags & flag != 0);
    Ok(())
}

const fn ace_is_audit(kind: VfsAceType) -> bool {
    matches!(
        kind,
        VfsAceType::SystemAudit
            | VfsAceType::SystemAlarm
            | VfsAceType::SystemAuditObject
            | VfsAceType::SystemAlarmObject
            | VfsAceType::SystemAuditCallback
            | VfsAceType::SystemAlarmCallback
            | VfsAceType::SystemAuditCallbackObject
            | VfsAceType::SystemAlarmCallbackObject
    )
}

pub(crate) struct SecDesc;

pub(crate) fn create_sec_desc<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    sec_desc: VfsSecDesc,
    out: &mut Slot<'v, '_>,
) {
    global
        .types
        .sec_desc
        .create_with_annex(strand, SecDesc, sec_desc, out);
}

pub(crate) fn sec_desc_from_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: &Value<'v>,
) -> Result<'v, 's, VfsSecDesc> {
    global
        .types
        .sec_desc
        .downcast(value)
        .map(|value| value.annex().clone())
        .ok_or_else(|| Error::type_error(strand, "expected security.SecDesc"))
}

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
        let dacl = builder.sym("dacl");
        let sacl = builder.sym("sacl");
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
            .get("dacl", move |this, strand, mut out| {
                let descriptor = this.annex();
                if !descriptor.dacl_loaded() || !descriptor.dacl_present() {
                    return Err(Error::field(strand, dacl));
                }
                if descriptor.dacl().is_none() {
                    Output::set(strand, out, Nil);
                } else {
                    let global = strand.state::<Global<'v>>();
                    create_acl(strand, global, this, AclComponent::Dacl, &mut out);
                }
                Ok(())
            })
            .get("sacl", move |this, strand, mut out| {
                let descriptor = this.annex();
                if !descriptor.sacl_loaded() || !descriptor.sacl_present() {
                    return Err(Error::field(strand, sacl));
                }
                if descriptor.sacl().is_none() {
                    Output::set(strand, out, Nil);
                } else {
                    let global = strand.state::<Global<'v>>();
                    create_acl(strand, global, this, AclComponent::Sacl, &mut out);
                }
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
        .value("Guid", global.types.guid)
        .value("Acl", global.types.acl)
        .value("Ace", global.types.ace)
        .value("SecDesc", global.types.sec_desc)
        .value("Sid", global.types.sid)
        .value("SidName", global.types.sid_name)
        .value("TokenGroup", global.types.token_group)
        .value("TokenInfo", global.types.token_info)
        .commit();
}
