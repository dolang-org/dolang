use super::macos::resolve_uid_or_gid_arg;
use super::*;

/// Unix read/write/execute permission bits (`security.unix.Permission`), a
/// local newtype over [`dolang_vfs::security::Permission`] so [`FlagLike`]
/// can be implemented here. Shared by [`Mode`]'s owner/group/other
/// projections and by [`PosixAceObject`]'s permission set.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Permission(pub VfsPermission);
flags_ops!(Permission);
impl FlagLike for Permission {
    const ZERO: Self = Self(VfsPermission::empty());
    const MODULE: &'static str = "security.unix";
    const NAME: &'static str = "Permission";
    const BITS: &'static [(&'static str, Self)] = &[
        ("READ", Self(VfsPermission::READ)),
        ("WRITE", Self(VfsPermission::WRITE)),
        ("EXECUTE", Self(VfsPermission::EXECUTE)),
    ];

    fn rank(self) -> usize {
        self.0.bits().count_ones() as usize
    }
}

pub(crate) struct Identity;

impl<'v> Object<'v> for Identity {
    const NAME: &'v str = "Identity";
    const MODULE: &'v str = "security.unix";
    const SLOTS: usize = 1;
    type Annex = UnixSecurityInfo;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("uid", |this, strand, out| {
                Output::set(strand, out, this.annex().uid());
                Ok(())
            })
            .get("gid", |this, strand, out| {
                Output::set(strand, out, this.annex().gid());
                Ok(())
            })
            .get("euid", |this, strand, out| {
                Output::set(strand, out, this.annex().effective_uid());
                Ok(())
            })
            .get("egid", |this, strand, out| {
                Output::set(strand, out, this.annex().effective_gid());
                Ok(())
            })
            .get("groups", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                Output::set(strand, out, Ref::slot::<0>(&borrow));
                Ok(())
            })
    }
}

/// Builds a `security.unix.Identity` around `info`.
///
/// The credentials themselves live in the annex, but `groups` projects a
/// tuple, which has to be constructed and rooted in slot 0 rather than built on
/// demand from a getter.
pub(crate) fn create_identity<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, UnixSecurityGlobal<'v>>,
    info: &UnixSecurityInfo,
    out: &mut Slot<'v, '_>,
) {
    strand.with_slots_sync(|strand, [mut groups]| {
        Output::set(
            strand,
            &mut groups,
            AsTuple::new(info.groups().iter().copied()),
        );
        global
            .types
            .unix_identity
            .create_with_annex(strand, Identity, info.clone(), &mut *out);
        global
            .types
            .unix_identity
            .cast(out)
            .unwrap()
            .enter_sync(strand, |strand, this| {
                Output::set(
                    strand,
                    Mut::slot_mut::<0>(&mut this.borrow_mut_unwrap()),
                    &groups,
                );
            });
    });
}

pub(crate) struct PosixAclObject;

struct PosixAclAces;

impl<'v> ArrayLike<'v> for PosixAclAces {
    type Object = PosixAclObject;

    const MODULE: &'v str = "security.unix";
    const NAME: &'v str = "AclAces";

    fn len(&self, this: Instance<'v, '_, PosixAclObject>, _strand: &mut Strand<'v, '_>) -> usize {
        this.annex().entries().len()
    }

    fn get<'a, 's>(
        &self,
        this: Instance<'v, '_, PosixAclObject>,
        strand: &'a mut Strand<'v, 's>,
        index: usize,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ace = this.annex().entries()[index];
        strand
            .state::<UnixSecurityGlobal<'v>>()
            .types
            .posix_ace
            .create_with_annex(strand, PosixAceObject, ace, out);
        Ok(())
    }
}

impl<'v> Object<'v> for PosixAclObject {
    const NAME: &'v str = "Acl";
    const MODULE: &'v str = "security.unix";
    type Annex = VfsPosixAcl;
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([mut iterable], []) = unpack!(strand, args, 1, 0)?;
        let global = strand.state::<UnixSecurityGlobal<'v>>();
        iterable.iter(strand, &mut out).await?;
        let mut entries = Vec::new();
        while out.next(strand, &mut iterable).await? {
            let ace = global.types.posix_ace.cast(&iterable).ok_or_else(|| {
                Error::type_error(strand, "Acl: iterable must contain security.unix.Ace")
            })?;
            entries.push(ace.enter_sync(strand, |_strand, ace| *ace.annex()));
        }
        let acl =
            VfsPosixAcl::new(entries).map_err(|error| Error::value(strand, error.to_string()))?;
        this.create_with_annex(strand, PosixAclObject, acl, out);
        Ok(())
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.get("aces", |this, strand, out| {
            Output::set(strand, out, ArrayView::new(this, PosixAclAces));
            Ok(())
        })
    }
}

pub(crate) fn create_posix_acl<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, UnixSecurityGlobal<'v>>,
    acl: Option<VfsPosixAcl>,
    out: &mut Slot<'v, '_>,
) {
    match acl {
        Some(acl) => {
            global
                .types
                .posix_acl
                .create_with_annex(strand, PosixAclObject, acl, out);
        }
        None => Output::set(strand, out, Nil),
    }
}

pub(crate) struct PosixAceObject;

async fn posix_permissions<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, UnixSecurityGlobal<'v>>,
    permissions: Option<&Value<'v>>,
) -> Result<'v, 's, VfsPermission> {
    let Some(value) = permissions else {
        return Ok(VfsPermission::empty());
    };
    global
        .types
        .permission
        .cast_flags(value)
        .map(|value| value.0)
        .ok_or_else(|| Error::type_error(strand, "expected security.unix.Permission"))
}

async fn coerce_permissions<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, UnixSecurityGlobal<'v>>,
    value: &Value<'v>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, VfsPermission> {
    global
        .types
        .permission
        .coerce(strand, value)
        .await
        .map(|value| value.0)
        .map_err(|_| {
            Error::type_error(strand, format!("{path}: expected security.unix.Permission"))
        })
}

pub(super) fn posix_id<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    name: &'static str,
) -> Result<'v, 's, u32> {
    let value = value
        .to_i64(strand)
        .map_err(|_| Error::type_error(strand, format!("{name}: expected Int")))?;
    u32::try_from(value).map_err(|_| Error::value(strand, format!("{name}: out of range")))
}

pub(super) fn spec_id<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, u32> {
    let value = value
        .to_i64(strand)
        .map_err(|_| Error::type_error(strand, format!("{path}: expected Int")))?;
    u32::try_from(value).map_err(|_| Error::value(strand, format!("{path}: out of range")))
}

impl<'v> Object<'v> for PosixAceObject {
    const NAME: &'v str = "Ace";
    const MODULE: &'v str = "security.unix";
    type Annex = VfsPosixAce;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let id_field = builder.sym("id");
        let user_obj = builder.sym("USER_OBJ");
        let user = builder.sym("USER");
        let group_obj = builder.sym("GROUP_OBJ");
        let group = builder.sym("GROUP");
        let mask = builder.sym("MASK");
        let other = builder.sym("OTHER");

        macro_rules! constructor {
            ($builder:expr, $name:literal, $qualifier:expr) => {
                $builder.type_method($name, async move |this, strand, args, out| {
                    let ([], [permissions]) = unpack!(strand, args, 0, 1)?;
                    let global = strand.state::<UnixSecurityGlobal<'v>>();
                    let permissions =
                        posix_permissions(strand, global, permissions.as_deref()).await?;
                    this.create_with_annex(
                        strand,
                        PosixAceObject,
                        VfsPosixAce::new($qualifier, permissions),
                        out,
                    );
                    Ok(())
                })
            };
        }

        let builder = constructor!(builder, "user_obj", VfsPosixAclQualifier::UserObj);
        let builder = constructor!(builder, "group_obj", VfsPosixAclQualifier::GroupObj);
        let builder = constructor!(builder, "mask", VfsPosixAclQualifier::Mask);
        let builder = constructor!(builder, "other", VfsPosixAclQualifier::Other);

        builder
            .type_method("user", async move |this, strand, args, out| {
                let ([id], [permissions]) = unpack!(strand, args, 1, 1)?;
                let global = strand.state::<UnixSecurityGlobal<'v>>();
                let permissions = posix_permissions(strand, global, permissions.as_deref()).await?;
                let id = posix_id(strand, &id, "uid")?;
                this.create_with_annex(
                    strand,
                    PosixAceObject,
                    VfsPosixAce::new(VfsPosixAclQualifier::User(id), permissions),
                    out,
                );
                Ok(())
            })
            .type_method("group", async move |this, strand, args, out| {
                let ([id], [permissions]) = unpack!(strand, args, 1, 1)?;
                let global = strand.state::<UnixSecurityGlobal<'v>>();
                let permissions = posix_permissions(strand, global, permissions.as_deref()).await?;
                let id = posix_id(strand, &id, "gid")?;
                this.create_with_annex(
                    strand,
                    PosixAceObject,
                    VfsPosixAce::new(VfsPosixAclQualifier::Group(id), permissions),
                    out,
                );
                Ok(())
            })
            .get("type", move |this, strand, out| {
                let value = match this.annex().qualifier() {
                    VfsPosixAclQualifier::UserObj => user_obj,
                    VfsPosixAclQualifier::User(_) => user,
                    VfsPosixAclQualifier::GroupObj => group_obj,
                    VfsPosixAclQualifier::Group(_) => group,
                    VfsPosixAclQualifier::Mask => mask,
                    VfsPosixAclQualifier::Other => other,
                };
                Output::set(strand, out, value);
                Ok(())
            })
            .get("id", move |this, strand, out| {
                let id = match this.annex().qualifier() {
                    VfsPosixAclQualifier::User(id) | VfsPosixAclQualifier::Group(id) => id,
                    _ => return Err(Error::field(strand, id_field)),
                };
                Output::set(strand, out, id);
                Ok(())
            })
            .get("permissions", |this, strand, out| {
                let permission = strand.state::<UnixSecurityGlobal<'v>>().types.permission;
                permission.create_flags(strand, Permission(this.annex().permissions()), out);
                Ok(())
            })
    }
}

async fn coerce_ace<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, UnixSecurityGlobal<'v>>,
    value: &Value<'v>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, VfsPosixAce> {
    if let Some(ace) = global.types.posix_ace.cast(value) {
        return Ok(ace.enter_sync(strand, |_strand, ace| *ace.annex()));
    }
    let dict = value.as_dict(strand.vm()).ok_or_else(|| {
        Error::type_error(
            strand,
            format!("{path}: expected security.unix.Ace or Dict"),
        )
    })?;
    let mut qualifier = None;
    let mut permissions = None;
    let mut separate_permissions = false;
    let mut pairs = dict.pairs();
    strand
        .with_slots(async |strand, [mut key, mut entry]| {
            while pairs.next(strand, &mut key, &mut entry)? {
                let sym = key
                    .as_sym(strand.vm())
                    .ok_or_else(|| Error::value(strand, format!("{path}: keys must be symbols")))?;
                let name = sym.as_str(strand.vm());
                match name {
                    "user_obj" | "group_obj" | "mask" | "other" => {
                        if qualifier.is_some() {
                            return Err(Error::value(
                                strand,
                                format!("{path}: multiple keys name conflicting qualifiers"),
                            ));
                        }
                        qualifier = Some(match name {
                            "user_obj" => VfsPosixAclQualifier::UserObj,
                            "group_obj" => VfsPosixAclQualifier::GroupObj,
                            "mask" => VfsPosixAclQualifier::Mask,
                            _ => VfsPosixAclQualifier::Other,
                        });
                        if separate_permissions {
                            return Err(Error::value(
                                strand,
                                format!("{path}: permissions conflicts with qualifier value"),
                            ));
                        }
                        permissions = Some(
                            coerce_permissions(strand, global, &entry, &path.key(name)).await?,
                        );
                    }
                    "user" | "group" => {
                        if qualifier.is_some() {
                            return Err(Error::value(
                                strand,
                                format!("{path}: multiple keys name conflicting qualifiers"),
                            ));
                        }
                        let id = spec_id(strand, &entry, &path.key(name))?;
                        qualifier = Some(if name == "user" {
                            VfsPosixAclQualifier::User(id)
                        } else {
                            VfsPosixAclQualifier::Group(id)
                        });
                    }
                    "permissions" => {
                        if permissions.is_some() {
                            return Err(Error::value(
                                strand,
                                format!("{path}: duplicate key `permissions`"),
                            ));
                        }
                        permissions = Some(
                            coerce_permissions(strand, global, &entry, &path.key("permissions"))
                                .await?,
                        );
                        separate_permissions = true;
                    }
                    _ => {
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
    let qualifier =
        qualifier.ok_or_else(|| Error::value(strand, format!("{path}: expected one qualifier")))?;
    if !matches!(
        qualifier,
        VfsPosixAclQualifier::User(_) | VfsPosixAclQualifier::Group(_)
    ) && permissions.is_none()
    {
        return Err(Error::value(
            strand,
            format!("{path}: qualifier requires permissions"),
        ));
    }
    Ok(VfsPosixAce::new(qualifier, permissions.unwrap_or_default()))
}

pub(super) async fn coerce_acl<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, UnixSecurityGlobal<'v>>,
    value: &Value<'v>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, VfsPosixAcl> {
    if let Some(acl) = global.types.posix_acl.cast(value) {
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
    VfsPosixAcl::new(entries).map_err(|error| Error::value(strand, format!("{path}: {error}")))
}

async fn ace_from_args<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, UnixSecurityGlobal<'v>>,
    args: Args<'v, '_>,
) -> Result<'v, 's, VfsPosixAce> {
    let user_obj = global.syms.user_obj;
    let group_obj = global.syms.group_obj;
    let mask = global.syms.mask;
    let other = global.syms.other;
    let user = global.syms.user;
    let group = global.syms.group;
    let permissions = global.syms.permissions;
    let ([], [user_obj, group_obj, mask, other, user, group, permissions]) = unpack!(
        strand,
        args,
        0,
        0,
        user_obj = None,
        group_obj = None,
        mask = None,
        other = None,
        user = None,
        group = None,
        permissions = None
    )?;
    let mut found = Vec::new();
    for (qualifier, value) in [
        (VfsPosixAclQualifier::UserObj, user_obj),
        (VfsPosixAclQualifier::GroupObj, group_obj),
        (VfsPosixAclQualifier::Mask, mask),
        (VfsPosixAclQualifier::Other, other),
    ] {
        if let Some(value) = value {
            found.push((
                qualifier,
                Some(coerce_permissions(strand, global, &value, &SpecPath::root("ace")).await?),
            ));
        }
    }
    if let Some(value) = user {
        found.push((
            VfsPosixAclQualifier::User(spec_id(strand, &value, &SpecPath::root("ace.user"))?),
            None,
        ));
    }
    if let Some(value) = group {
        found.push((
            VfsPosixAclQualifier::Group(spec_id(strand, &value, &SpecPath::root("ace.group"))?),
            None,
        ));
    }
    if found.len() != 1 {
        return Err(Error::value(
            strand,
            if found.is_empty() {
                "ace: expected one qualifier"
            } else {
                "ace: multiple keys name conflicting qualifiers"
            },
        ));
    }
    let (qualifier, inline_permissions) = found.pop().unwrap();
    if inline_permissions.is_some() && permissions.is_some() {
        return Err(Error::value(
            strand,
            "ace: permissions conflicts with qualifier value",
        ));
    }
    let permissions = match inline_permissions {
        Some(value) => value,
        None => match permissions.as_deref() {
            Some(value) => {
                coerce_permissions(strand, global, value, &SpecPath::root("ace.permissions"))
                    .await?
            }
            None => VfsPermission::empty(),
        },
    };
    Ok(VfsPosixAce::new(qualifier, permissions))
}

pub(crate) fn configure_vm<'v>(
    builder: &mut Register<'v>,
    global: State<'v, UnixSecurityGlobal<'v>>,
) {
    builder
        .module("security.unix")
        .value("Identity", global.types.unix_identity)
        .value("Acl", global.types.posix_acl)
        .value("Ace", global.types.posix_ace)
        .value("Permission", global.types.permission)
        .function("ace", async move |strand, args, out| {
            let ace = ace_from_args(strand, global, args).await?;
            global
                .types
                .posix_ace
                .create_with_annex(strand, PosixAceObject, ace, out);
            Ok(())
        })
        .function("acl", async move |strand, args, out| {
            let ([], [], entries) = unpack!(strand, args, 0, 0, ...)?;
            let mut aces = Vec::new();
            for (index, entry) in entries.enumerate() {
                let value = match entry {
                    Arg::Pos(value) => value,
                    Arg::Key(key, _) => return Err(Error::unexpected_key(strand, key)),
                };
                aces.push(
                    coerce_ace(strand, global, &value, &SpecPath::root("acl").index(index)).await?,
                );
            }
            let acl = VfsPosixAcl::new(aces)
                .map_err(|error| Error::value(strand, format!("acl: {error}")))?;
            global
                .types
                .posix_acl
                .create_with_annex(strand, PosixAclObject, acl, out);
            Ok(())
        })
        .function("id", async move |strand, args, mut out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            let security = security_info(strand, global.local)?;
            let Some(info) = security.unix() else {
                return Err(Error::not_supported(strand));
            };
            create_identity(strand, global, info, &mut out);
            Ok(())
        })
        .function("user_name", async move |strand, args, out| {
            let ([uid], []) = unpack!(strand, args, 1, 0)?;
            if global.local.get(strand).target().os().family() != OperatingSystemFamily::Unix {
                return Err(Error::not_supported(strand));
            }
            let vfs = global.local.get(strand).vfs();
            let uid =
                resolve_uid_or_gid_arg(strand, global.local, &uid, VfsPrincipalIdKind::Uid).await?;
            let name = error::io_result(strand, vfs.user_name(uid).await)?;
            Output::set(strand, out, name.as_str());
            Ok(())
        })
        .function("user_id", async move |strand, args, out| {
            let ([name], []) = unpack!(strand, args, 1, 0)?;
            if global.local.get(strand).target().os().family() != OperatingSystemFamily::Unix {
                return Err(Error::not_supported(strand));
            }
            let name = name
                .as_str(strand)
                .ok_or_else(|| Error::type_error(strand, "user_id: expected Str"))?
                .to_string();
            let vfs = global.local.get(strand).vfs();
            let uid = error::io_result(strand, vfs.user_id(&name).await)?;
            Output::set(strand, out, uid);
            Ok(())
        })
        .function("group_name", async move |strand, args, out| {
            let ([gid], []) = unpack!(strand, args, 1, 0)?;
            if global.local.get(strand).target().os().family() != OperatingSystemFamily::Unix {
                return Err(Error::not_supported(strand));
            }
            let vfs = global.local.get(strand).vfs();
            let gid =
                resolve_uid_or_gid_arg(strand, global.local, &gid, VfsPrincipalIdKind::Gid).await?;
            let name = error::io_result(strand, vfs.group_name(gid).await)?;
            Output::set(strand, out, name.as_str());
            Ok(())
        })
        .function("group_id", async move |strand, args, out| {
            let ([name], []) = unpack!(strand, args, 1, 0)?;
            if global.local.get(strand).target().os().family() != OperatingSystemFamily::Unix {
                return Err(Error::not_supported(strand));
            }
            let name = name
                .as_str(strand)
                .ok_or_else(|| Error::type_error(strand, "group_id: expected Str"))?
                .to_string();
            let vfs = global.local.get(strand).vfs();
            let gid = error::io_result(strand, vfs.group_id(&name).await)?;
            Output::set(strand, out, gid);
            Ok(())
        })
        .commit();
}
