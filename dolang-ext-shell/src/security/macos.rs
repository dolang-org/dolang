use super::*;

/// macOS extended ACE permission mask (`security.macos.Mask`), a local
/// newtype over [`dolang_vfs::security::MacosAceMask`] so [`FlagLike`] can be
/// implemented here.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct MacosAceMask(pub VfsMacosAceMask);
flags_ops!(MacosAceMask);
impl FlagLike for MacosAceMask {
    const ZERO: Self = Self(VfsMacosAceMask::empty());
    const MODULE: &'static str = "security.macos";
    const NAME: &'static str = "Mask";
    const BITS: &'static [(&'static str, Self)] = &[
        ("READ_DATA", Self(VfsMacosAceMask::READ_DATA)),
        ("WRITE_DATA", Self(VfsMacosAceMask::WRITE_DATA)),
        ("EXECUTE", Self(VfsMacosAceMask::EXECUTE)),
        ("DELETE", Self(VfsMacosAceMask::DELETE)),
        ("APPEND_DATA", Self(VfsMacosAceMask::APPEND_DATA)),
        ("DELETE_CHILD", Self(VfsMacosAceMask::DELETE_CHILD)),
        ("READ_ATTRIBUTES", Self(VfsMacosAceMask::READ_ATTRIBUTES)),
        ("WRITE_ATTRIBUTES", Self(VfsMacosAceMask::WRITE_ATTRIBUTES)),
        (
            "READ_EXTATTRIBUTES",
            Self(VfsMacosAceMask::READ_EXTATTRIBUTES),
        ),
        (
            "WRITE_EXTATTRIBUTES",
            Self(VfsMacosAceMask::WRITE_EXTATTRIBUTES),
        ),
        ("READ_SECURITY", Self(VfsMacosAceMask::READ_SECURITY)),
        ("WRITE_SECURITY", Self(VfsMacosAceMask::WRITE_SECURITY)),
        ("CHANGE_OWNER", Self(VfsMacosAceMask::CHANGE_OWNER)),
        ("SYNCHRONIZE", Self(VfsMacosAceMask::SYNCHRONIZE)),
    ];

    fn rank(self) -> usize {
        self.0.bits().count_ones() as usize
    }
}

/// macOS extended ACE inheritance flags (`security.macos.Flags`), a local
/// newtype over [`dolang_vfs::security::MacosAceFlags`] so [`FlagLike`] can
/// be implemented here.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct MacosAceFlags(pub VfsMacosAceFlags);
flags_ops!(MacosAceFlags);
impl FlagLike for MacosAceFlags {
    const ZERO: Self = Self(VfsMacosAceFlags::empty());
    const MODULE: &'static str = "security.macos";
    const NAME: &'static str = "Flags";
    const BITS: &'static [(&'static str, Self)] = &[
        ("FILE_INHERIT", Self(VfsMacosAceFlags::FILE_INHERIT)),
        (
            "DIRECTORY_INHERIT",
            Self(VfsMacosAceFlags::DIRECTORY_INHERIT),
        ),
        ("LIMIT_INHERIT", Self(VfsMacosAceFlags::LIMIT_INHERIT)),
        ("ONLY_INHERIT", Self(VfsMacosAceFlags::ONLY_INHERIT)),
        ("INHERITED", Self(VfsMacosAceFlags::INHERITED)),
    ];

    fn rank(self) -> usize {
        self.0.bits().count_ones() as usize
    }
}

pub(crate) struct MacosAclObject;

struct MacosAclAces;

impl<'v> ArrayLike<'v> for MacosAclAces {
    type Object = MacosAclObject;

    const MODULE: &'v str = "security.macos";
    const NAME: &'v str = "AclAces";

    fn len(&self, this: Instance<'v, '_, MacosAclObject>, _strand: &mut Strand<'v, '_>) -> usize {
        this.annex().entries().len()
    }

    fn get<'a, 's>(
        &self,
        this: Instance<'v, '_, MacosAclObject>,
        strand: &'a mut Strand<'v, 's>,
        index: usize,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ace = this.annex().entries()[index];
        strand
            .state::<MacosSecurityGlobal<'v>>()
            .types
            .macos_ace
            .create_with_annex(strand, MacosAceObject, ace, out);
        Ok(())
    }
}

impl<'v> Object<'v> for MacosAclObject {
    const NAME: &'v str = "Acl";
    const MODULE: &'v str = "security.macos";
    type Annex = VfsMacosAcl;
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([mut iterable], []) = unpack!(strand, args, 1, 0)?;
        let global = strand.state::<MacosSecurityGlobal<'v>>();
        iterable.iter(strand, &mut out).await?;
        let mut entries = Vec::new();
        while out.next(strand, &mut iterable).await? {
            let ace = global.types.macos_ace.cast(&iterable).ok_or_else(|| {
                Error::type_error(strand, "Acl: iterable must contain security.macos.Ace")
            })?;
            entries.push(ace.enter_sync(strand, |_strand, ace| *ace.annex()));
        }
        let acl = VfsMacosAcl::new(entries);
        this.create_with_annex(strand, MacosAclObject, acl, out);
        Ok(())
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.get("aces", |this, strand, out| {
            Output::set(strand, out, ArrayView::new(this, MacosAclAces));
            Ok(())
        })
    }
}

pub(crate) fn create_macos_acl<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, MacosSecurityGlobal<'v>>,
    acl: Option<VfsMacosAcl>,
    out: &mut Slot<'v, '_>,
) {
    match acl {
        Some(acl) => {
            global
                .types
                .macos_acl
                .create_with_annex(strand, MacosAclObject, acl, out);
        }
        None => Output::set(strand, out, Nil),
    }
}

pub(crate) struct MacosAceObject;

async fn macos_ace_mask<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, MacosSecurityGlobal<'v>>,
    mask: Option<&Value<'v>>,
) -> Result<'v, 's, VfsMacosAceMask> {
    let Some(value) = mask else {
        return Err(Error::type_error(
            strand,
            "mask: expected security.macos.Mask",
        ));
    };
    global
        .types
        .macos_ace_mask
        .cast_flags(value)
        .map(|value| value.0)
        .ok_or_else(|| Error::type_error(strand, "mask: expected security.macos.Mask"))
}

async fn macos_ace_flags<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, MacosSecurityGlobal<'v>>,
    flags: Option<&Value<'v>>,
) -> Result<'v, 's, VfsMacosAceFlags> {
    let Some(value) = flags else {
        return Ok(VfsMacosAceFlags::empty());
    };
    global
        .types
        .macos_ace_flags
        .cast_flags(value)
        .map(|value| value.0)
        .ok_or_else(|| Error::type_error(strand, "flags: expected security.macos.Flags"))
}

impl<'v> Object<'v> for MacosAceObject {
    const NAME: &'v str = "Ace";
    const MODULE: &'v str = "security.macos";
    type Annex = VfsMacosAce;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let mask_sym = builder.sym("mask");
        let flags_sym = builder.sym("flags");
        let allow = builder.sym("ALLOW");
        let deny = builder.sym("DENY");

        macro_rules! constructor {
            ($builder:expr, $name:literal, $ace_type:expr) => {
                $builder.type_method($name, async move |this, strand, args, out| {
                    let ([principal], [mask, flags]) =
                        unpack!(strand, args, 1, 0, mask_sym = None, flags_sym = None)?;
                    let global = strand.state::<MacosSecurityGlobal<'v>>();
                    let qualifier =
                        dolang_ext_uuid::cast_uuid(strand, &principal).ok_or_else(|| {
                            Error::type_error(strand, "principal: expected uuid.Uuid")
                        })?;
                    let mask = macos_ace_mask(strand, global, mask.as_deref()).await?;
                    let flags = macos_ace_flags(strand, global, flags.as_deref()).await?;
                    this.create_with_annex(
                        strand,
                        MacosAceObject,
                        VfsMacosAce::new($ace_type, qualifier, mask, flags),
                        out,
                    );
                    Ok(())
                })
            };
        }

        let builder = constructor!(builder, "allow", VfsMacosAceType::Allow);
        let builder = constructor!(builder, "deny", VfsMacosAceType::Deny);

        builder
            .get("type", move |this, strand, out| {
                let value = match this.annex().ace_type() {
                    VfsMacosAceType::Allow => allow,
                    VfsMacosAceType::Deny => deny,
                };
                Output::set(strand, out, value);
                Ok(())
            })
            .get("principal", move |this, strand, out| {
                dolang_ext_uuid::create_uuid(strand, this.annex().qualifier(), out);
                Ok(())
            })
            .get("mask", |this, strand, out| {
                let mask = strand
                    .state::<MacosSecurityGlobal<'v>>()
                    .types
                    .macos_ace_mask;
                mask.create_flags(strand, MacosAceMask(this.annex().mask()), out);
                Ok(())
            })
            .get("flags", |this, strand, out| {
                let flags = strand
                    .state::<MacosSecurityGlobal<'v>>()
                    .types
                    .macos_ace_flags;
                flags.create_flags(strand, MacosAceFlags(this.annex().flags()), out);
                Ok(())
            })
    }
}

async fn coerce_mask<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, MacosSecurityGlobal<'v>>,
    value: &Value<'v>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, VfsMacosAceMask> {
    global
        .types
        .macos_ace_mask
        .coerce(strand, value)
        .await
        .map(|v| v.0)
        .map_err(|_| Error::type_error(strand, format!("{path}: expected security.macos.Mask")))
}

async fn coerce_flags<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, MacosSecurityGlobal<'v>>,
    value: Option<&Value<'v>>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, VfsMacosAceFlags> {
    match value {
        Some(value) => global
            .types
            .macos_ace_flags
            .coerce(strand, value)
            .await
            .map(|v| v.0)
            .map_err(|_| {
                Error::type_error(strand, format!("{path}: expected security.macos.Flags"))
            }),
        None => Ok(VfsMacosAceFlags::empty()),
    }
}

async fn coerce_ace<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, MacosSecurityGlobal<'v>>,
    value: &Value<'v>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, VfsMacosAce> {
    if let Some(ace) = global.types.macos_ace.cast(value) {
        return Ok(ace.enter_sync(strand, |_strand, ace| *ace.annex()));
    }
    let dict = value.as_dict(strand.vm()).ok_or_else(|| {
        Error::type_error(
            strand,
            format!("{path}: expected security.macos.Ace or Dict"),
        )
    })?;
    let mut ace_type = None;
    let mut principal = None;
    let mut mask = None;
    let mut flags = None;
    let mut pairs = dict.pairs();
    strand
        .with_slots(async |strand, [mut key, mut entry]| {
            while pairs.next(strand, &mut key, &mut entry)? {
                let sym = key
                    .as_sym(strand.vm())
                    .ok_or_else(|| Error::value(strand, format!("{path}: keys must be symbols")))?;
                match sym.as_str(strand.vm()) {
                    name @ ("allow" | "deny") => {
                        if ace_type.is_some() {
                            return Err(Error::value(
                                strand,
                                format!("{path}: multiple keys name conflicting ACE types"),
                            ));
                        }
                        ace_type = Some(if name == "allow" {
                            VfsMacosAceType::Allow
                        } else {
                            VfsMacosAceType::Deny
                        });
                        principal =
                            Some(dolang_ext_uuid::value_to_uuid(strand, &entry).map_err(|_| {
                                Error::type_error(
                                    strand,
                                    format!("{}: expected uuid.Uuid, Str, or Bin", path.key(name)),
                                )
                            })?);
                    }
                    "mask" => {
                        if mask.is_some() {
                            return Err(Error::value(
                                strand,
                                format!("{path}: duplicate key `mask`"),
                            ));
                        }
                        mask = Some(coerce_mask(strand, global, &entry, &path.key("mask")).await?);
                    }
                    "flags" => {
                        if flags.is_some() {
                            return Err(Error::value(
                                strand,
                                format!("{path}: duplicate key `flags`"),
                            ));
                        }
                        flags = Some(
                            coerce_flags(strand, global, Some(&entry), &path.key("flags")).await?,
                        );
                    }
                    name => {
                        return Err(Error::value(
                            strand,
                            format!("{path}: unknown key `{name}`"),
                        ));
                    }
                }
            }
            Ok::<_, Error<'v, 's>>(())
        })
        .await?;
    let ace_type =
        ace_type.ok_or_else(|| Error::value(strand, format!("{path}: expected allow or deny")))?;
    let principal = principal.expect("ACE type and principal are set together");
    let mask =
        mask.ok_or_else(|| Error::value(strand, format!("{}: required", path.key("mask"))))?;
    Ok(VfsMacosAce::new(
        ace_type,
        principal,
        mask,
        flags.unwrap_or_default(),
    ))
}

pub(super) async fn coerce_acl<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, MacosSecurityGlobal<'v>>,
    value: &Value<'v>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, VfsMacosAcl> {
    if let Some(acl) = global.types.macos_acl.cast(value) {
        return Ok(acl.enter_sync(strand, |_strand, acl| acl.annex().clone()));
    }
    let mut entries = Vec::new();
    strand
        .with_slots(async |strand, [mut iter, mut item]| {
            value.iter(strand, &mut iter).await?;
            let mut index = 0;
            while iter.next(strand, &mut item).await? {
                entries.push(coerce_ace(strand, global, &item, &path.index(index)).await?);
                index += 1;
            }
            Ok::<_, Error<'v, 's>>(())
        })
        .await?;
    Ok(VfsMacosAcl::new(entries))
}

async fn ace_from_args<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, MacosSecurityGlobal<'v>>,
    args: Args<'v, '_>,
) -> Result<'v, 's, VfsMacosAce> {
    let mask_sym = global.syms.mask;
    let allow_sym = global.syms.allow;
    let deny_sym = global.syms.deny;
    let flags_sym = global.syms.flags;
    let ([mask], [allow, deny, flags]) = unpack!(
        strand,
        args,
        0,
        0,
        mask_sym,
        allow_sym = None,
        deny_sym = None,
        flags_sym = None
    )?;
    let (ace_type, principal) = match (allow, deny) {
        (Some(value), None) => (VfsMacosAceType::Allow, value),
        (None, Some(value)) => (VfsMacosAceType::Deny, value),
        (None, None) => return Err(Error::value(strand, "ace: expected allow or deny")),
        _ => {
            return Err(Error::value(
                strand,
                "ace: multiple keys name conflicting ACE types",
            ));
        }
    };
    let principal = dolang_ext_uuid::value_to_uuid(strand, &principal)?;
    let mask = coerce_mask(strand, global, &mask, &SpecPath::root("ace.mask")).await?;
    let flags = coerce_flags(
        strand,
        global,
        flags.as_deref(),
        &SpecPath::root("ace.flags"),
    )
    .await?;
    Ok(VfsMacosAce::new(ace_type, principal, mask, flags))
}

/// Resolves a `security.unix.user_name`/`group_name` argument that may be a
/// `uuid.Uuid` (macOS only) as well as the usual numeric uid/gid.
pub(super) async fn resolve_uid_or_gid_arg<'v, 's>(
    strand: &mut Strand<'v, 's>,
    local: LocalKey<'v, Local>,
    value: &Value<'v>,
    want: VfsPrincipalIdKind,
) -> Result<'v, 's, u32> {
    let Some(uuid) = dolang_ext_uuid::cast_uuid(strand, value) else {
        return value.to_u32(strand);
    };
    if local.get(strand).target().os() != OperatingSystem::Macos {
        return Err(Error::not_supported(strand));
    }
    let vfs = local.get(strand).vfs();
    let id = error::io_result(
        strand,
        vfs.resolve_principal_id(VfsPrincipalId::Uuid(uuid), want)
            .await,
    )?;
    Ok(match id {
        VfsPrincipalId::Uid(uid) => uid,
        VfsPrincipalId::Gid(gid) => gid,
        VfsPrincipalId::Uuid(_) => {
            unreachable!("resolve_principal_id(Uuid, Uid|Gid) returned a Uuid")
        }
        _ => return Err(Error::not_supported(strand)),
    })
}

pub(crate) fn configure_vm<'v>(
    builder: &mut Register<'v>,
    global: State<'v, MacosSecurityGlobal<'v>>,
) {
    let macos_uid_sym = builder.sym("UID");
    let macos_gid_sym = builder.sym("GID");
    builder
        .module("security.macos")
        .value("Acl", global.types.macos_acl)
        .value("Ace", global.types.macos_ace)
        .value("Mask", global.types.macos_ace_mask)
        .value("Flags", global.types.macos_ace_flags)
        .function("ace", async move |strand, args, out| {
            let ace = ace_from_args(strand, global, args).await?;
            global
                .types
                .macos_ace
                .create_with_annex(strand, MacosAceObject, ace, out);
            Ok(())
        })
        .function("acl", async move |strand, args, out| {
            let ([], [], entries) = unpack!(strand, args, 0, 0, *)?;
            let mut aces = Vec::new();
            for (index, value) in entries.enumerate() {
                aces.push(
                    coerce_ace(strand, global, &value, &SpecPath::root("acl").index(index)).await?,
                );
            }
            global.types.macos_acl.create_with_annex(
                strand,
                MacosAclObject,
                VfsMacosAcl::new(aces),
                out,
            );
            Ok(())
        })
        .function("uuid_for_uid", async move |strand, args, out| {
            let ([uid], []) = unpack!(strand, args, 1, 0)?;
            if global.local.get(strand).target().os() != OperatingSystem::Macos {
                return Err(Error::not_supported(strand));
            }
            let uid = uid.to_u32(strand)?;
            let vfs = global.local.get(strand).vfs();
            let id = error::io_result(
                strand,
                vfs.resolve_principal_id(VfsPrincipalId::Uid(uid), VfsPrincipalIdKind::Uuid)
                    .await,
            )?;
            let VfsPrincipalId::Uuid(uuid) = id else {
                unreachable!("resolve_principal_id(_, Uuid) returned a non-Uuid PrincipalId")
            };
            dolang_ext_uuid::create_uuid(strand, uuid, out);
            Ok(())
        })
        .function("uuid_for_gid", async move |strand, args, out| {
            let ([gid], []) = unpack!(strand, args, 1, 0)?;
            if global.local.get(strand).target().os() != OperatingSystem::Macos {
                return Err(Error::not_supported(strand));
            }
            let gid = gid.to_u32(strand)?;
            let vfs = global.local.get(strand).vfs();
            let id = error::io_result(
                strand,
                vfs.resolve_principal_id(VfsPrincipalId::Gid(gid), VfsPrincipalIdKind::Uuid)
                    .await,
            )?;
            let VfsPrincipalId::Uuid(uuid) = id else {
                unreachable!("resolve_principal_id(_, Uuid) returned a non-Uuid PrincipalId")
            };
            dolang_ext_uuid::create_uuid(strand, uuid, out);
            Ok(())
        })
        .function("id_for_uuid", async move |strand, args, out| {
            let ([uuid], []) = unpack!(strand, args, 1, 0)?;
            if global.local.get(strand).target().os() != OperatingSystem::Macos {
                return Err(Error::not_supported(strand));
            }
            let uuid = dolang_ext_uuid::value_to_uuid(strand, &uuid)?;
            let vfs = global.local.get(strand).vfs();
            let id = error::io_result(
                strand,
                vfs.resolve_principal_id(VfsPrincipalId::Uuid(uuid), VfsPrincipalIdKind::Uid)
                    .await,
            )?;
            let (kind, id) = match id {
                VfsPrincipalId::Uid(uid) => (macos_uid_sym, uid),
                VfsPrincipalId::Gid(gid) => (macos_gid_sym, gid),
                VfsPrincipalId::Uuid(_) => {
                    unreachable!("resolve_principal_id(_, Uid|Gid) returned a Uuid")
                }
                _ => return Err(Error::not_supported(strand)),
            };
            Output::set(strand, out, (kind, id));
            Ok(())
        })
        .commit();
}
