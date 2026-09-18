use super::unix::{posix_id, spec_id};
use super::*;

/// NFSv4 ACE permission mask (`security.nfs4.Mask`), a local newtype over
/// [`dolang_vfs::security::Nfs4AceMask`] so [`FlagLike`] can be implemented
/// here.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Nfs4AceMask(pub VfsNfs4AceMask);
flags_ops!(Nfs4AceMask);
impl FlagLike for Nfs4AceMask {
    const ZERO: Self = Self(VfsNfs4AceMask::empty());
    const MODULE: &'static str = "security.nfs4";
    const NAME: &'static str = "Mask";
    const BITS: &'static [(&'static str, Self)] = &[
        ("READ_DATA", Self(VfsNfs4AceMask::READ_DATA)),
        ("WRITE_DATA", Self(VfsNfs4AceMask::WRITE_DATA)),
        ("APPEND_DATA", Self(VfsNfs4AceMask::APPEND_DATA)),
        ("READ_NAMED_ATTRS", Self(VfsNfs4AceMask::READ_NAMED_ATTRS)),
        ("WRITE_NAMED_ATTRS", Self(VfsNfs4AceMask::WRITE_NAMED_ATTRS)),
        ("EXECUTE", Self(VfsNfs4AceMask::EXECUTE)),
        ("DELETE_CHILD", Self(VfsNfs4AceMask::DELETE_CHILD)),
        ("READ_ATTRIBUTES", Self(VfsNfs4AceMask::READ_ATTRIBUTES)),
        ("WRITE_ATTRIBUTES", Self(VfsNfs4AceMask::WRITE_ATTRIBUTES)),
        ("DELETE", Self(VfsNfs4AceMask::DELETE)),
        ("READ_ACL", Self(VfsNfs4AceMask::READ_ACL)),
        ("WRITE_ACL", Self(VfsNfs4AceMask::WRITE_ACL)),
        ("WRITE_OWNER", Self(VfsNfs4AceMask::WRITE_OWNER)),
        ("SYNCHRONIZE", Self(VfsNfs4AceMask::SYNCHRONIZE)),
    ];

    fn rank(self) -> usize {
        self.0.bits().count_ones() as usize
    }
}

/// NFSv4 ACE inheritance/audit flags (`security.nfs4.Flags`), a local
/// newtype over [`dolang_vfs::security::Nfs4AceFlags`] so [`FlagLike`] can be
/// implemented here.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Nfs4AceFlags(pub VfsNfs4AceFlags);
flags_ops!(Nfs4AceFlags);
impl FlagLike for Nfs4AceFlags {
    const ZERO: Self = Self(VfsNfs4AceFlags::empty());
    const MODULE: &'static str = "security.nfs4";
    const NAME: &'static str = "Flags";
    const BITS: &'static [(&'static str, Self)] = &[
        ("FILE_INHERIT", Self(VfsNfs4AceFlags::FILE_INHERIT)),
        (
            "DIRECTORY_INHERIT",
            Self(VfsNfs4AceFlags::DIRECTORY_INHERIT),
        ),
        (
            "NO_PROPAGATE_INHERIT",
            Self(VfsNfs4AceFlags::NO_PROPAGATE_INHERIT),
        ),
        ("INHERIT_ONLY", Self(VfsNfs4AceFlags::INHERIT_ONLY)),
        (
            "SUCCESSFUL_ACCESS",
            Self(VfsNfs4AceFlags::SUCCESSFUL_ACCESS),
        ),
        ("FAILED_ACCESS", Self(VfsNfs4AceFlags::FAILED_ACCESS)),
        ("INHERITED", Self(VfsNfs4AceFlags::INHERITED)),
    ];

    fn rank(self) -> usize {
        self.0.bits().count_ones() as usize
    }
}

pub(crate) struct Nfs4AclObject;

struct Nfs4AclAces;

impl<'v> ArrayLike<'v> for Nfs4AclAces {
    type Object = Nfs4AclObject;

    const MODULE: &'v str = "security.nfs4";
    const NAME: &'v str = "AclAces";

    fn len(&self, this: Instance<'v, '_, Nfs4AclObject>, _strand: &mut Strand<'v, '_>) -> usize {
        this.annex().entries().len()
    }

    fn get<'a, 's>(
        &self,
        this: Instance<'v, '_, Nfs4AclObject>,
        strand: &'a mut Strand<'v, 's>,
        index: usize,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ace = this.annex().entries()[index];
        strand
            .state::<Nfs4SecurityGlobal<'v>>()
            .types
            .nfs4_ace
            .create_with_annex(strand, Nfs4AceObject, ace, out);
        Ok(())
    }
}

impl<'v> Object<'v> for Nfs4AclObject {
    const NAME: &'v str = "Acl";
    const MODULE: &'v str = "security.nfs4";
    type Annex = VfsNfs4Acl;
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([mut iterable], []) = unpack!(strand, args, 1, 0)?;
        let global = strand.state::<Nfs4SecurityGlobal<'v>>();
        iterable.iter(strand, &mut out).await?;
        let mut entries = Vec::new();
        while out.next(strand, &mut iterable).await? {
            let ace = global.types.nfs4_ace.cast(&iterable).ok_or_else(|| {
                Error::type_error(strand, "Acl: iterable must contain security.nfs4.Ace")
            })?;
            entries.push(ace.enter_sync(strand, |_strand, ace| *ace.annex()));
        }
        let acl = VfsNfs4Acl::new(entries);
        this.create_with_annex(strand, Nfs4AclObject, acl, out);
        Ok(())
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.get("aces", |this, strand, out| {
            Output::set(strand, out, ArrayView::new(this, Nfs4AclAces));
            Ok(())
        })
    }
}

pub(crate) fn create_nfs4_acl<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, Nfs4SecurityGlobal<'v>>,
    acl: Option<VfsNfs4Acl>,
    out: &mut Slot<'v, '_>,
) {
    match acl {
        Some(acl) => {
            global
                .types
                .nfs4_acl
                .create_with_annex(strand, Nfs4AclObject, acl, out);
        }
        None => Output::set(strand, out, Nil),
    }
}

pub(crate) struct Nfs4AceObject;

async fn nfs4_ace_mask<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Nfs4SecurityGlobal<'v>>,
    mask: Option<&Value<'v>>,
) -> Result<'v, 's, VfsNfs4AceMask> {
    let Some(value) = mask else {
        return Err(Error::type_error(
            strand,
            "mask: expected security.nfs4.Mask",
        ));
    };
    global
        .types
        .nfs4_ace_mask
        .cast_flags(value)
        .map(|value| value.0)
        .ok_or_else(|| Error::type_error(strand, "mask: expected security.nfs4.Mask"))
}

async fn nfs4_ace_flags<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Nfs4SecurityGlobal<'v>>,
    flags: Option<&Value<'v>>,
) -> Result<'v, 's, VfsNfs4AceFlags> {
    let Some(value) = flags else {
        return Ok(VfsNfs4AceFlags::empty());
    };
    global
        .types
        .nfs4_ace_flags
        .cast_flags(value)
        .map(|value| value.0)
        .ok_or_else(|| Error::type_error(strand, "flags: expected security.nfs4.Flags"))
}

async fn coerce_mask<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Nfs4SecurityGlobal<'v>>,
    value: &Value<'v>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, VfsNfs4AceMask> {
    global
        .types
        .nfs4_ace_mask
        .coerce(strand, value)
        .await
        .map(|v| v.0)
        .map_err(|_| Error::type_error(strand, format!("{path}: expected security.nfs4.Mask")))
}

async fn coerce_flags<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Nfs4SecurityGlobal<'v>>,
    value: Option<&Value<'v>>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, VfsNfs4AceFlags> {
    match value {
        Some(value) => global
            .types
            .nfs4_ace_flags
            .coerce(strand, value)
            .await
            .map(|v| v.0)
            .map_err(|_| {
                Error::type_error(strand, format!("{path}: expected security.nfs4.Flags"))
            }),
        None => Ok(VfsNfs4AceFlags::empty()),
    }
}

async fn coerce_principal<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, VfsNfs4AceQualifier> {
    if let Some(sym) = value.as_sym(strand.vm()) {
        return match sym.as_str(strand.vm()) {
            "OWNER" => Ok(VfsNfs4AceQualifier::Owner),
            "OWNING_GROUP" => Ok(VfsNfs4AceQualifier::OwningGroup),
            "EVERYONE" => Ok(VfsNfs4AceQualifier::Everyone),
            _ => Err(Error::value(
                strand,
                format!("{path}: expected OWNER, OWNING_GROUP, EVERYONE, user, or group"),
            )),
        };
    }
    let dict = value
        .as_dict(strand.vm())
        .ok_or_else(|| Error::type_error(strand, format!("{path}: expected principal")))?;
    let mut principal = None;
    let mut pairs = dict.pairs();
    strand
        .with_slots(async |strand, [mut key, mut entry]| {
            while pairs.next(strand, &mut key, &mut entry)? {
                let sym = key.as_sym(strand.vm()).ok_or_else(|| {
                    Error::value(strand, format!("{path}: principal keys must be symbols"))
                })?;
                if principal.is_some() {
                    return Err(Error::value(
                        strand,
                        format!("{path}: multiple principal keys"),
                    ));
                }
                principal = Some(match sym.as_str(strand.vm()) {
                    "user" => {
                        VfsNfs4AceQualifier::User(spec_id(strand, &entry, &path.key("user"))?)
                    }
                    "group" => {
                        VfsNfs4AceQualifier::Group(spec_id(strand, &entry, &path.key("group"))?)
                    }
                    name => {
                        return Err(Error::value(
                            strand,
                            format!("{path}: unknown key `{name}`"),
                        ));
                    }
                });
            }
            Ok::<_, Error<'v, 's>>(())
        })
        .await?;
    principal.ok_or_else(|| Error::value(strand, format!("{path}: expected user or group")))
}

fn nfs4_ace_type<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: Option<&Value<'v>>,
    allow: Sym<'v, 'v>,
    deny: Sym<'v, 'v>,
    audit: Sym<'v, 'v>,
    alarm: Sym<'v, 'v>,
) -> Result<'v, 's, VfsNfs4AceType> {
    let Some(value) = value else {
        return Err(Error::type_error(
            strand,
            "type: expected :ALLOW:, :DENY:, :AUDIT:, or :ALARM:",
        ));
    };
    let Some(sym) = value.as_sym(strand) else {
        return Err(Error::type_error(
            strand,
            "type: expected :ALLOW:, :DENY:, :AUDIT:, or :ALARM:",
        ));
    };
    if sym == allow {
        Ok(VfsNfs4AceType::Allow)
    } else if sym == deny {
        Ok(VfsNfs4AceType::Deny)
    } else if sym == audit {
        Ok(VfsNfs4AceType::Audit)
    } else if sym == alarm {
        Ok(VfsNfs4AceType::Alarm)
    } else {
        Err(Error::value(
            strand,
            "type: expected :ALLOW:, :DENY:, :AUDIT:, or :ALARM:",
        ))
    }
}

impl<'v> Object<'v> for Nfs4AceObject {
    const NAME: &'v str = "Ace";
    const MODULE: &'v str = "security.nfs4";
    type Annex = VfsNfs4Ace;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let type_sym = builder.sym("type");
        let mask_sym = builder.sym("mask");
        let flags_sym = builder.sym("flags");
        let id_field = builder.sym("id");
        let allow = builder.sym("ALLOW");
        let deny = builder.sym("DENY");
        let audit = builder.sym("AUDIT");
        let alarm = builder.sym("ALARM");
        let owner = builder.sym("OWNER");
        let owning_group = builder.sym("OWNING_GROUP");
        let everyone = builder.sym("EVERYONE");
        let user = builder.sym("USER");
        let group = builder.sym("GROUP");

        macro_rules! constructor {
            ($builder:expr, $name:literal, $qualifier:expr) => {
                $builder.type_method($name, async move |this, strand, args, out| {
                    let ([], [ace_type, mask, flags]) = unpack!(
                        strand,
                        args,
                        0,
                        0,
                        type_sym = None,
                        mask_sym = None,
                        flags_sym = None
                    )?;
                    let global = strand.state::<Nfs4SecurityGlobal<'v>>();
                    let ace_type =
                        nfs4_ace_type(strand, ace_type.as_deref(), allow, deny, audit, alarm)?;
                    let mask = nfs4_ace_mask(strand, global, mask.as_deref()).await?;
                    let flags = nfs4_ace_flags(strand, global, flags.as_deref()).await?;
                    this.create_with_annex(
                        strand,
                        Nfs4AceObject,
                        VfsNfs4Ace::new(ace_type, $qualifier, mask, flags),
                        out,
                    );
                    Ok(())
                })
            };
        }

        let builder = constructor!(builder, "owner", VfsNfs4AceQualifier::Owner);
        let builder = constructor!(builder, "owning_group", VfsNfs4AceQualifier::OwningGroup);
        let builder = constructor!(builder, "everyone", VfsNfs4AceQualifier::Everyone);

        builder
            .type_method("user", async move |this, strand, args, out| {
                let ([id], [ace_type, mask, flags]) = unpack!(
                    strand,
                    args,
                    1,
                    0,
                    type_sym = None,
                    mask_sym = None,
                    flags_sym = None
                )?;
                let global = strand.state::<Nfs4SecurityGlobal<'v>>();
                let ace_type =
                    nfs4_ace_type(strand, ace_type.as_deref(), allow, deny, audit, alarm)?;
                let mask = nfs4_ace_mask(strand, global, mask.as_deref()).await?;
                let flags = nfs4_ace_flags(strand, global, flags.as_deref()).await?;
                let id = posix_id(strand, &id, "uid")?;
                this.create_with_annex(
                    strand,
                    Nfs4AceObject,
                    VfsNfs4Ace::new(ace_type, VfsNfs4AceQualifier::User(id), mask, flags),
                    out,
                );
                Ok(())
            })
            .type_method("group", async move |this, strand, args, out| {
                let ([id], [ace_type, mask, flags]) = unpack!(
                    strand,
                    args,
                    1,
                    0,
                    type_sym = None,
                    mask_sym = None,
                    flags_sym = None
                )?;
                let global = strand.state::<Nfs4SecurityGlobal<'v>>();
                let ace_type =
                    nfs4_ace_type(strand, ace_type.as_deref(), allow, deny, audit, alarm)?;
                let mask = nfs4_ace_mask(strand, global, mask.as_deref()).await?;
                let flags = nfs4_ace_flags(strand, global, flags.as_deref()).await?;
                let id = posix_id(strand, &id, "gid")?;
                this.create_with_annex(
                    strand,
                    Nfs4AceObject,
                    VfsNfs4Ace::new(ace_type, VfsNfs4AceQualifier::Group(id), mask, flags),
                    out,
                );
                Ok(())
            })
            .get("type", move |this, strand, out| {
                let value = match this.annex().ace_type() {
                    VfsNfs4AceType::Allow => allow,
                    VfsNfs4AceType::Deny => deny,
                    VfsNfs4AceType::Audit => audit,
                    VfsNfs4AceType::Alarm => alarm,
                };
                Output::set(strand, out, value);
                Ok(())
            })
            .get("principal", move |this, strand, out| {
                let value = match this.annex().qualifier() {
                    VfsNfs4AceQualifier::Owner => owner,
                    VfsNfs4AceQualifier::OwningGroup => owning_group,
                    VfsNfs4AceQualifier::Everyone => everyone,
                    VfsNfs4AceQualifier::User(_) => user,
                    VfsNfs4AceQualifier::Group(_) => group,
                };
                Output::set(strand, out, value);
                Ok(())
            })
            .get("id", move |this, strand, out| {
                let id = match this.annex().qualifier() {
                    VfsNfs4AceQualifier::User(id) | VfsNfs4AceQualifier::Group(id) => id,
                    _ => return Err(Error::field(strand, id_field)),
                };
                Output::set(strand, out, id);
                Ok(())
            })
            .get("mask", |this, strand, out| {
                let mask = strand.state::<Nfs4SecurityGlobal<'v>>().types.nfs4_ace_mask;
                mask.create_flags(strand, Nfs4AceMask(this.annex().mask()), out);
                Ok(())
            })
            .get("flags", |this, strand, out| {
                let flags = strand
                    .state::<Nfs4SecurityGlobal<'v>>()
                    .types
                    .nfs4_ace_flags;
                flags.create_flags(strand, Nfs4AceFlags(this.annex().flags()), out);
                Ok(())
            })
    }
}

async fn coerce_ace<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Nfs4SecurityGlobal<'v>>,
    value: &Value<'v>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, VfsNfs4Ace> {
    if let Some(ace) = global.types.nfs4_ace.cast(value) {
        return Ok(ace.enter_sync(strand, |_strand, ace| *ace.annex()));
    }
    let dict = value.as_dict(strand.vm()).ok_or_else(|| {
        Error::type_error(
            strand,
            format!("{path}: expected security.nfs4.Ace or Dict"),
        )
    })?;
    let mut ace_type = None;
    let mut qualifier = None;
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
                    name @ ("allow" | "deny" | "audit" | "alarm") => {
                        if ace_type.is_some() {
                            return Err(Error::value(
                                strand,
                                format!("{path}: multiple keys name conflicting ACE types"),
                            ));
                        }
                        ace_type = Some(match name {
                            "allow" => VfsNfs4AceType::Allow,
                            "deny" => VfsNfs4AceType::Deny,
                            "audit" => VfsNfs4AceType::Audit,
                            _ => VfsNfs4AceType::Alarm,
                        });
                        qualifier = Some(coerce_principal(strand, &entry, &path.key(name)).await?);
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
    let ace_type = ace_type.ok_or_else(|| {
        Error::value(
            strand,
            format!("{path}: expected one of allow, deny, audit, or alarm"),
        )
    })?;
    let qualifier = qualifier.expect("ACE type and qualifier are set together");
    let mask =
        mask.ok_or_else(|| Error::value(strand, format!("{}: required", path.key("mask"))))?;
    Ok(VfsNfs4Ace::new(
        ace_type,
        qualifier,
        mask,
        flags.unwrap_or_default(),
    ))
}

pub(super) async fn coerce_acl<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Nfs4SecurityGlobal<'v>>,
    value: &Value<'v>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, VfsNfs4Acl> {
    if let Some(acl) = global.types.nfs4_acl.cast(value) {
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
    Ok(VfsNfs4Acl::new(entries))
}

async fn ace_from_args<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Nfs4SecurityGlobal<'v>>,
    args: Args<'v, '_>,
) -> Result<'v, 's, VfsNfs4Ace> {
    let mask_sym = global.syms.mask;
    let allow_sym = global.syms.allow;
    let deny_sym = global.syms.deny;
    let audit_sym = global.syms.audit;
    let alarm_sym = global.syms.alarm;
    let flags_sym = global.syms.flags;
    let ([mask], [allow, deny, audit, alarm, flags]) = unpack!(
        strand,
        args,
        0,
        0,
        mask_sym,
        allow_sym = None,
        deny_sym = None,
        audit_sym = None,
        alarm_sym = None,
        flags_sym = None
    )?;
    let mut found = Vec::new();
    for (ty, value) in [
        (VfsNfs4AceType::Allow, allow),
        (VfsNfs4AceType::Deny, deny),
        (VfsNfs4AceType::Audit, audit),
        (VfsNfs4AceType::Alarm, alarm),
    ] {
        if let Some(value) = value {
            found.push((ty, value));
        }
    }
    if found.len() != 1 {
        return Err(Error::value(
            strand,
            if found.is_empty() {
                "ace: expected one of allow, deny, audit, or alarm"
            } else {
                "ace: multiple keys name conflicting ACE types"
            },
        ));
    }
    let (ace_type, principal) = found.pop().unwrap();
    let qualifier = coerce_principal(strand, &principal, &SpecPath::root("ace.principal")).await?;
    let mask = coerce_mask(strand, global, &mask, &SpecPath::root("ace.mask")).await?;
    let flags = coerce_flags(
        strand,
        global,
        flags.as_deref(),
        &SpecPath::root("ace.flags"),
    )
    .await?;
    Ok(VfsNfs4Ace::new(ace_type, qualifier, mask, flags))
}

pub(crate) fn configure_vm<'v>(
    builder: &mut Register<'v>,
    global: State<'v, Nfs4SecurityGlobal<'v>>,
) {
    builder
        .module("security.nfs4")
        .value("Acl", global.types.nfs4_acl)
        .value("Ace", global.types.nfs4_ace)
        .value("Mask", global.types.nfs4_ace_mask)
        .value("Flags", global.types.nfs4_ace_flags)
        .function("ace", async move |strand, args, out| {
            let ace = ace_from_args(strand, global, args).await?;
            global
                .types
                .nfs4_ace
                .create_with_annex(strand, Nfs4AceObject, ace, out);
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
            global.types.nfs4_acl.create_with_annex(
                strand,
                Nfs4AclObject,
                VfsNfs4Acl::new(aces),
                out,
            );
            Ok(())
        })
        .commit();
}
