use super::*;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SecInfo(pub WinSecInfo);
flags_ops!(SecInfo);
impl FlagLike for SecInfo {
    const ZERO: Self = Self(WinSecInfo::empty());
    const MODULE: &'static str = "security.windows";
    const NAME: &'static str = "SecInfo";
    const BITS: &'static [(&'static str, Self)] = &[
        ("OWNER", Self(WinSecInfo::OWNER)),
        ("GROUP", Self(WinSecInfo::GROUP)),
        ("DACL", Self(WinSecInfo::DACL)),
        ("SACL", Self(WinSecInfo::SACL)),
        ("ALL", Self(WinSecInfo::ALL)),
    ];
    fn rank(self) -> usize {
        self.0.bits().count_ones() as usize
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TokenGroupAttributes(pub WinTokenGroupAttributes);
flags_ops!(TokenGroupAttributes);
impl FlagLike for TokenGroupAttributes {
    const ZERO: Self = Self(WinTokenGroupAttributes::empty());
    const MODULE: &'static str = "security.windows";
    const NAME: &'static str = "TokenGroupAttributes";
    const BITS: &'static [(&'static str, Self)] = &[
        ("MANDATORY", Self(WinTokenGroupAttributes::MANDATORY)),
        (
            "ENABLED_BY_DEFAULT",
            Self(WinTokenGroupAttributes::ENABLED_BY_DEFAULT),
        ),
        ("ENABLED", Self(WinTokenGroupAttributes::ENABLED)),
        ("OWNER", Self(WinTokenGroupAttributes::OWNER)),
        (
            "USE_FOR_DENY_ONLY",
            Self(WinTokenGroupAttributes::USE_FOR_DENY_ONLY),
        ),
        ("INTEGRITY", Self(WinTokenGroupAttributes::INTEGRITY)),
        (
            "INTEGRITY_ENABLED",
            Self(WinTokenGroupAttributes::INTEGRITY_ENABLED),
        ),
        ("RESOURCE", Self(WinTokenGroupAttributes::RESOURCE)),
        ("LOGON_ID", Self(WinTokenGroupAttributes::LOGON_ID)),
    ];
    fn rank(self) -> usize {
        self.0.bits().count_ones() as usize
    }
}

/// Generic Windows `ACCESS_MASK` bits (`security.windows.AccessMask`), a
/// local newtype over [`dolang_winterop::security::AccessMask`]'s bit values so
/// [`FlagLike`] can be implemented here (both the trait and
/// `dolang_winterop::security::AccessMask` are foreign to this crate).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct AccessMask(pub WinAccessMask);

impl AccessMask {
    pub const DELETE: AccessMask = AccessMask(WinAccessMask::DELETE);
    pub const READ_CONTROL: AccessMask = AccessMask(WinAccessMask::READ_CONTROL);
    pub const WRITE_DAC: AccessMask = AccessMask(WinAccessMask::WRITE_DAC);
    pub const WRITE_OWNER: AccessMask = AccessMask(WinAccessMask::WRITE_OWNER);
    pub const SYNCHRONIZE: AccessMask = AccessMask(WinAccessMask::SYNCHRONIZE);
    pub const STANDARD_RIGHTS_REQUIRED: AccessMask =
        AccessMask(WinAccessMask::STANDARD_RIGHTS_REQUIRED);
    pub const STANDARD_RIGHTS_ALL: AccessMask = AccessMask(WinAccessMask::STANDARD_RIGHTS_ALL);
    pub const ACCESS_SYSTEM_SECURITY: AccessMask =
        AccessMask(WinAccessMask::ACCESS_SYSTEM_SECURITY);
    pub const MAXIMUM_ALLOWED: AccessMask = AccessMask(WinAccessMask::MAXIMUM_ALLOWED);
    pub const GENERIC_ALL: AccessMask = AccessMask(WinAccessMask::GENERIC_ALL);
    pub const GENERIC_EXECUTE: AccessMask = AccessMask(WinAccessMask::GENERIC_EXECUTE);
    pub const GENERIC_WRITE: AccessMask = AccessMask(WinAccessMask::GENERIC_WRITE);
    pub const GENERIC_READ: AccessMask = AccessMask(WinAccessMask::GENERIC_READ);
}

impl BitOr for AccessMask {
    type Output = AccessMask;
    fn bitor(self, rhs: AccessMask) -> AccessMask {
        AccessMask(self.0 | rhs.0)
    }
}

impl BitAnd for AccessMask {
    type Output = AccessMask;
    fn bitand(self, rhs: AccessMask) -> AccessMask {
        AccessMask(self.0 & rhs.0)
    }
}

impl BitXor for AccessMask {
    type Output = AccessMask;
    fn bitxor(self, rhs: AccessMask) -> AccessMask {
        AccessMask(self.0 ^ rhs.0)
    }
}

impl Not for AccessMask {
    type Output = AccessMask;
    fn not(self) -> AccessMask {
        AccessMask(!self.0)
    }
}

impl FlagLike for AccessMask {
    const ZERO: AccessMask = AccessMask(WinAccessMask::empty());
    const MODULE: &'static str = "security.windows";
    const NAME: &'static str = "AccessMask";
    const BITS: &'static [(&'static str, AccessMask)] = &[
        ("DELETE", AccessMask::DELETE),
        ("READ_CONTROL", AccessMask::READ_CONTROL),
        ("WRITE_DAC", AccessMask::WRITE_DAC),
        ("WRITE_OWNER", AccessMask::WRITE_OWNER),
        ("SYNCHRONIZE", AccessMask::SYNCHRONIZE),
        (
            "STANDARD_RIGHTS_REQUIRED",
            AccessMask::STANDARD_RIGHTS_REQUIRED,
        ),
        ("STANDARD_RIGHTS_ALL", AccessMask::STANDARD_RIGHTS_ALL),
        ("ACCESS_SYSTEM_SECURITY", AccessMask::ACCESS_SYSTEM_SECURITY),
        ("MAXIMUM_ALLOWED", AccessMask::MAXIMUM_ALLOWED),
        ("GENERIC_READ", AccessMask::GENERIC_READ),
        ("GENERIC_WRITE", AccessMask::GENERIC_WRITE),
        ("GENERIC_EXECUTE", AccessMask::GENERIC_EXECUTE),
        ("GENERIC_ALL", AccessMask::GENERIC_ALL),
    ];

    fn rank(self) -> usize {
        self.0.bits().count_ones() as usize
    }

    fn build<'v, 'a>(
        builder: TypeBuilder<'v, 'a, Flags<Self>>,
    ) -> TypeBuilder<'v, 'a, Flags<Self>> {
        builder
            .get("specific_rights", |this, strand, out| {
                Output::set(strand, out, this.flags().0.specific_rights());
                Ok(())
            })
            .get("standard_rights", |this, strand, out| {
                let ty = this.ty(strand.vm());
                ty.create_flags(strand, Self(this.flags().0.standard_rights()), out);
                Ok(())
            })
            .get("generic_rights", |this, strand, out| {
                let ty = this.ty(strand.vm());
                ty.create_flags(strand, Self(this.flags().0.generic_rights()), out);
                Ok(())
            })
            .get("int", |this, strand, out| {
                Output::set(strand, out, this.flags().0.bits());
                Ok(())
            })
            .type_method("from_int", async move |this, strand, args, out| {
                let ([value], []) = unpack!(strand, args, 1, 0)?;
                let value = ace_u32(strand, &value, &SpecPath::root("value"))?;
                this.create_flags(strand, Self(WinAccessMask::from_bits_retain(value)), out);
                Ok(())
            })
    }
}

/// ACE header flags (`security.windows.AceFlags`).
///
/// `TRUST_PROTECTED_FILTER` is deliberately absent: it is the same bit as
/// `SUCCESSFUL_ACCESS` (0x40), and an exact alias would make name lookup
/// ambiguous. It remains reachable through the `trust_protected_filter`
/// field on an access-filter [`Ace`].
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct AceFlags(pub WinAceFlags);
flags_ops!(AceFlags);
impl FlagLike for AceFlags {
    const ZERO: Self = Self(WinAceFlags::empty());
    const MODULE: &'static str = "security.windows";
    const NAME: &'static str = "AceFlags";
    const BITS: &'static [(&'static str, Self)] = &[
        ("OBJECT_INHERIT", Self(WinAceFlags::OBJECT_INHERIT)),
        ("CONTAINER_INHERIT", Self(WinAceFlags::CONTAINER_INHERIT)),
        (
            "NO_PROPAGATE_INHERIT",
            Self(WinAceFlags::NO_PROPAGATE_INHERIT),
        ),
        ("INHERIT_ONLY", Self(WinAceFlags::INHERIT_ONLY)),
        ("INHERITED", Self(WinAceFlags::INHERITED)),
        ("CRITICAL", Self(WinAceFlags::CRITICAL)),
        ("SUCCESSFUL_ACCESS", Self(WinAceFlags::SUCCESSFUL_ACCESS)),
        ("FAILED_ACCESS", Self(WinAceFlags::FAILED_ACCESS)),
    ];

    fn rank(self) -> usize {
        self.0.bits().count_ones() as usize
    }

    fn build<'v, 'a>(
        builder: TypeBuilder<'v, 'a, Flags<Self>>,
    ) -> TypeBuilder<'v, 'a, Flags<Self>> {
        builder
            .get("int", |this, strand, out| {
                Output::set(strand, out, this.flags().0.bits());
                Ok(())
            })
            .type_method("from_int", async move |this, strand, args, out| {
                let ([value], []) = unpack!(strand, args, 1, 0)?;
                let value = ace_u8(strand, &value, &SpecPath::root("value"))?;
                this.create_flags(strand, Self(WinAceFlags::from_bits_retain(value)), out);
                Ok(())
            })
    }
}

/// Security descriptor control flags (`security.windows.SecDescControl`).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SecDescControl(WinSecDescControl);

impl BitOr for SecDescControl {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitAnd for SecDescControl {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

impl BitXor for SecDescControl {
    type Output = Self;

    fn bitxor(self, rhs: Self) -> Self {
        Self(self.0 ^ rhs.0)
    }
}

impl Not for SecDescControl {
    type Output = Self;

    fn not(self) -> Self {
        Self(!self.0)
    }
}

impl FlagLike for SecDescControl {
    const ZERO: Self = Self(WinSecDescControl::empty());
    const MODULE: &'static str = "security.windows";
    const NAME: &'static str = "SecDescControl";
    const BITS: &'static [(&'static str, Self)] = &[
        ("OWNER_DEFAULTED", Self(WinSecDescControl::OWNER_DEFAULTED)),
        ("GROUP_DEFAULTED", Self(WinSecDescControl::GROUP_DEFAULTED)),
        ("DACL_PRESENT", Self(WinSecDescControl::DACL_PRESENT)),
        ("DACL_DEFAULTED", Self(WinSecDescControl::DACL_DEFAULTED)),
        ("SACL_PRESENT", Self(WinSecDescControl::SACL_PRESENT)),
        ("SACL_DEFAULTED", Self(WinSecDescControl::SACL_DEFAULTED)),
        (
            "DACL_AUTO_INHERIT_REQUIRED",
            Self(WinSecDescControl::DACL_AUTO_INHERIT_REQUIRED),
        ),
        (
            "SACL_AUTO_INHERIT_REQUIRED",
            Self(WinSecDescControl::SACL_AUTO_INHERIT_REQUIRED),
        ),
        (
            "DACL_AUTO_INHERITED",
            Self(WinSecDescControl::DACL_AUTO_INHERITED),
        ),
        (
            "SACL_AUTO_INHERITED",
            Self(WinSecDescControl::SACL_AUTO_INHERITED),
        ),
        ("DACL_PROTECTED", Self(WinSecDescControl::DACL_PROTECTED)),
        ("SACL_PROTECTED", Self(WinSecDescControl::SACL_PROTECTED)),
        (
            "RM_CONTROL_VALID",
            Self(WinSecDescControl::RM_CONTROL_VALID),
        ),
        ("SELF_RELATIVE", Self(WinSecDescControl::SELF_RELATIVE)),
    ];

    fn rank(self) -> usize {
        self.0.bits().count_ones() as usize
    }
}

pub(crate) struct Sid;

pub(crate) fn create_sid<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, WindowsSecurityGlobal<'v>>,
    sid: VfsSid,
    out: &mut Slot<'v, '_>,
) {
    global
        .types
        .sid
        .create_with_annex(strand, Sid, sid, &mut *out);
    global
        .types
        .sid
        .cast(&*out)
        .unwrap()
        .enter_sync(strand, |strand, this| {
            let annex = this.annex();
            let sub_authorities = annex.sub_authorities();
            Output::set(
                strand,
                Mut::slot_mut::<0>(&mut this.borrow_mut_unwrap()),
                AsTuple::new(sub_authorities.iter().copied()),
            );
        });
}

pub(crate) fn as_windows_sid<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
) -> Option<VfsSid> {
    let global = strand.try_state::<WindowsSecurityGlobal<'v>>()?;
    let sid = global.types.sid.cast(value)?;
    sid.enter_sync(strand, |_strand, sid| Some(sid.annex().clone()))
}

pub(crate) fn windows_sid<'v>(strand: &mut Strand<'v, '_>, sid: VfsSid, out: &mut Slot<'v, '_>) {
    let global = strand.force_state::<WindowsSecurityGlobal<'v>>();
    create_sid(strand, global, sid, out);
}

/// Do spellings of the SIDs that are the same on every Windows installation.
///
/// Names follow the Windows account or group they identify. A SID that is
/// relative to a domain or machine cannot appear here: resolving one needs a
/// lookup, and symbols name constants only.
const WELL_KNOWN_SIDS: &[(&str, WellKnownSid)] = &[
    ("NULL", WellKnownSid::Null),
    ("EVERYONE", WellKnownSid::Everyone),
    ("LOCAL", WellKnownSid::Local),
    ("CONSOLE_LOGON", WellKnownSid::ConsoleLogon),
    ("CREATOR_OWNER", WellKnownSid::CreatorOwner),
    ("CREATOR_GROUP", WellKnownSid::CreatorGroup),
    ("OWNER_RIGHTS", WellKnownSid::OwnerRights),
    ("DIALUP", WellKnownSid::Dialup),
    ("NETWORK", WellKnownSid::Network),
    ("BATCH", WellKnownSid::Batch),
    ("INTERACTIVE", WellKnownSid::Interactive),
    ("SERVICE", WellKnownSid::Service),
    ("ANONYMOUS", WellKnownSid::Anonymous),
    ("PRINCIPAL_SELF", WellKnownSid::PrincipalSelf),
    ("AUTHENTICATED_USERS", WellKnownSid::AuthenticatedUsers),
    ("RESTRICTED_CODE", WellKnownSid::RestrictedCode),
    (
        "REMOTE_INTERACTIVE_LOGON",
        WellKnownSid::RemoteInteractiveLogon,
    ),
    ("THIS_ORGANIZATION", WellKnownSid::ThisOrganization),
    ("LOCAL_SYSTEM", WellKnownSid::LocalSystem),
    ("LOCAL_SERVICE", WellKnownSid::LocalService),
    ("NETWORK_SERVICE", WellKnownSid::NetworkService),
    ("LOCAL_ACCOUNT", WellKnownSid::LocalAccount),
    (
        "LOCAL_ACCOUNT_ADMINISTRATOR",
        WellKnownSid::LocalAccountAdministrator,
    ),
    (
        "BUILTIN_ADMINISTRATORS",
        WellKnownSid::BuiltinAdministrators,
    ),
    ("BUILTIN_USERS", WellKnownSid::BuiltinUsers),
    ("BUILTIN_GUESTS", WellKnownSid::BuiltinGuests),
    ("BUILTIN_POWER_USERS", WellKnownSid::BuiltinPowerUsers),
    (
        "BUILTIN_BACKUP_OPERATORS",
        WellKnownSid::BuiltinBackupOperators,
    ),
    (
        "BUILTIN_REMOTE_DESKTOP_USERS",
        WellKnownSid::BuiltinRemoteDesktopUsers,
    ),
    (
        "BUILTIN_REMOTE_MANAGEMENT_USERS",
        WellKnownSid::BuiltinRemoteManagementUsers,
    ),
    (
        "ALL_APPLICATION_PACKAGES",
        WellKnownSid::AllApplicationPackages,
    ),
    (
        "ALL_RESTRICTED_APPLICATION_PACKAGES",
        WellKnownSid::AllRestrictedApplicationPackages,
    ),
    ("UNTRUSTED_LABEL", WellKnownSid::UntrustedLabel),
    ("LOW_LABEL", WellKnownSid::LowLabel),
    ("MEDIUM_LABEL", WellKnownSid::MediumLabel),
    ("MEDIUM_PLUS_LABEL", WellKnownSid::MediumPlusLabel),
    ("HIGH_LABEL", WellKnownSid::HighLabel),
    ("SYSTEM_LABEL", WellKnownSid::SystemLabel),
];

/// [`WELL_KNOWN_SIDS`] with its names interned, so a symbol is matched by
/// identity rather than by spelling.
pub(crate) struct WellKnownSids<'v>(Box<[(Sym<'v, 'v>, WellKnownSid)]>);

impl<'v> WellKnownSids<'v> {
    pub(crate) fn new(builder: &mut Register<'v>) -> Self {
        let mut entries: Box<[_]> = WELL_KNOWN_SIDS
            .iter()
            .map(|(name, well_known)| (builder.sym(name), *well_known))
            .collect();
        entries.sort_unstable_by_key(|(sym, _)| *sym);
        Self(entries)
    }

    /// The SID `sym` names, or `None` if it names no well-known SID.
    pub(crate) fn get(&self, sym: Sym<'v, '_>) -> Option<VfsSid> {
        let index = self
            .0
            .binary_search_by(|(candidate, _)| Sym::cmp(candidate, &sym))
            .ok()?;
        Some(VfsSid::from(self.0[index].1))
    }
}

/// Resolves the well-known SID a symbol names.
fn sid_from_sym<'v, 's>(
    strand: &mut Strand<'v, 's>,
    sym: Sym<'v, '_>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, VfsSid> {
    let global = strand.state::<WindowsSecurityGlobal<'v>>();
    match global.syms.well_known_sids.get(sym) {
        Some(sid) => Ok(sid),
        None => {
            let name = sym.as_str(strand.vm());
            Err(Error::value(
                strand,
                format!("{path}: `{name}` does not name a well-known SID"),
            ))
        }
    }
}

fn sid_from_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, VfsSid> {
    if let Some(sym) = value.as_sym(strand.vm()) {
        sid_from_sym(strand, sym, path)
    } else if let Some(value) = value.as_str(strand) {
        value
            .to_string()
            .parse::<VfsSid>()
            .map_err(|error| Error::value(strand, format!("{path}: {error}")))
    } else if let Some(value) = value.as_bin(strand) {
        let bytes = value.to_vec();
        VfsSid::from_bytes(&bytes).map_err(|error| Error::value(strand, format!("{path}: {error}")))
    } else {
        Err(Error::type_error(
            strand,
            format!("{path}: expected Str, Bin, or Sym"),
        ))
    }
}

impl<'v> Object<'v> for Sid {
    const NAME: &'v str = "Sid";
    const MODULE: &'v str = "security.windows";
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
        let sid = sid_from_value(strand, &value, &SpecPath::root("Sid"))?;
        this.create_with_annex(strand, Sid, sid, &mut out);
        this.cast(&out).unwrap().enter_sync(strand, |strand, this| {
            let annex = this.annex();
            Output::set(
                strand,
                Mut::slot_mut::<0>(&mut this.borrow_mut_unwrap()),
                AsTuple::new(annex.sub_authorities().iter().copied()),
            );
        });
        Ok(())
    }

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let null = builder.sym("NULL");
        let world = builder.sym("WORLD");
        let local = builder.sym("LOCAL");
        let creator = builder.sym("CREATOR");
        let non_unique = builder.sym("NON_UNIQUE");
        let nt = builder.sym("NT");
        let resource_manager = builder.sym("RESOURCE_MANAGER");
        let app_package = builder.sym("APP_PACKAGE");
        let mandatory_label = builder.sym("MANDATORY_LABEL");
        let scoped_policy = builder.sym("SCOPED_POLICY");
        let authentication = builder.sym("AUTHENTICATION");
        let process_trust = builder.sym("PROCESS_TRUST");

        builder
            .get("revision", |this, strand, out| {
                Output::set(strand, out, this.annex().revision() as u8);
                Ok(())
            })
            .get("sub_authority_count", |this, strand, out| {
                Output::set(strand, out, this.annex().sub_authorities().len());
                Ok(())
            })
            .get("identifier_authority", move |this, strand, out| {
                match this.annex().identifier_authority() {
                    SidIdentifierAuthority::Null => Output::set(strand, out, null),
                    SidIdentifierAuthority::World => Output::set(strand, out, world),
                    SidIdentifierAuthority::Local => Output::set(strand, out, local),
                    SidIdentifierAuthority::Creator => Output::set(strand, out, creator),
                    SidIdentifierAuthority::NonUnique => Output::set(strand, out, non_unique),
                    SidIdentifierAuthority::Nt => Output::set(strand, out, nt),
                    SidIdentifierAuthority::ResourceManager => {
                        Output::set(strand, out, resource_manager)
                    }
                    SidIdentifierAuthority::AppPackage => Output::set(strand, out, app_package),
                    SidIdentifierAuthority::MandatoryLabel => {
                        Output::set(strand, out, mandatory_label)
                    }
                    SidIdentifierAuthority::ScopedPolicy => Output::set(strand, out, scoped_policy),
                    SidIdentifierAuthority::Authentication => {
                        Output::set(strand, out, authentication)
                    }
                    SidIdentifierAuthority::ProcessTrust => Output::set(strand, out, process_trust),
                    SidIdentifierAuthority::Unknown(value) => Output::set(strand, out, value),
                    authority => Output::set(strand, out, u64::from(authority)),
                }
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
                let global = strand.state::<WindowsSecurityGlobal<'v>>();
                if global.local.get(strand).target().os().family() != OperatingSystemFamily::Windows
                {
                    return Err(Error::not_supported(strand));
                }
                let vfs = global.local.get(strand).vfs();
                let name = error::io_result(strand, vfs.sid_name(&sid).await)?;
                create_sid_name(strand, global, name, &mut out);
                Ok(())
            })
    }

    /// A SID is its value: two of them naming the same principal are equal
    /// however each was obtained, and neither carries anything else to
    /// distinguish it.
    fn eq<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        let global = strand.state::<WindowsSecurityGlobal<'v>>();
        let Some(other) = global.types.sid.cast(other) else {
            return Err(Error::not_supported(strand));
        };
        Ok(other.enter_sync(strand, |_strand, other| *this.annex() == *other.annex()))
    }

    fn hash<'a, 's>(
        this: Instance<'v, 'a, Self>,
        _strand: &'a mut Strand<'v, 's>,
        hasher: &mut impl Hasher,
    ) -> Result<'v, 's, ()> {
        this.annex().hash(hasher);
        Ok(())
    }

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "{}", this.annex().as_ref())
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(
            strand,
            w,
            "<security.windows.Sid {}>",
            this.annex().as_ref()
        )
    }
}

pub(crate) enum AclComponent {
    Dacl,
    Sacl,
}

pub(crate) enum AclAnnex {
    Component(AclComponent),
    Owned(VfsAclBuf),
}

pub(crate) struct Acl;

fn create_acl<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, WindowsSecurityGlobal<'v>>,
    descriptor: Instance<'v, '_, SecDesc>,
    component: AclComponent,
    out: &mut Slot<'v, '_>,
) {
    global
        .types
        .acl
        .create_with_annex(strand, Acl, AclAnnex::Component(component), &mut *out);
    global
        .types
        .acl
        .cast(&*out)
        .unwrap()
        .enter_sync(strand, |strand, acl| {
            Output::set(
                strand,
                Mut::slot_mut::<0>(&mut acl.borrow_mut_unwrap()),
                descriptor,
            );
        });
}

fn with_acl<'v, 's, T>(
    this: Instance<'v, '_, Acl>,
    strand: &mut Strand<'v, 's>,
    f: impl FnOnce(&VfsAcl) -> T,
) -> Result<'v, 's, T> {
    if let AclAnnex::Owned(acl) = &*this.annex() {
        return Ok(f(acl));
    }
    let global = strand.state::<WindowsSecurityGlobal<'v>>();
    let borrow = this.borrow(strand)?;
    let descriptor = global
        .types
        .sec_desc
        .cast(Ref::slot::<0>(&borrow))
        .expect("Acl root is a SecDesc");
    let value = descriptor.enter_sync(strand, |_strand, descriptor| {
        let descriptor = descriptor.annex();
        let acl = match &*this.annex() {
            AclAnnex::Component(AclComponent::Dacl) => descriptor.dacl(),
            AclAnnex::Component(AclComponent::Sacl) => descriptor.sacl(),
            AclAnnex::Owned(_) => unreachable!(),
        }
        .expect("Acl component is non-null");
        f(acl)
    });
    Ok(value)
}

struct AclAces;

impl<'v> ArrayLike<'v> for AclAces {
    type Object = Acl;

    const MODULE: &'v str = "security.windows";
    const NAME: &'v str = "AclAces";

    fn len(&self, this: Instance<'v, '_, Acl>, strand: &mut Strand<'v, '_>) -> usize {
        with_acl(this, strand, |acl| usize::from(acl.ace_count())).unwrap()
    }

    fn get<'a, 's>(
        &self,
        this: Instance<'v, '_, Acl>,
        strand: &'a mut Strand<'v, 's>,
        index: usize,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = strand.state::<WindowsSecurityGlobal<'v>>();
        global
            .types
            .ace
            .create_with_annex(strand, Ace, AceAnnex::InAcl(index), &mut out);
        global
            .types
            .ace
            .cast(&out)
            .unwrap()
            .enter_sync(strand, |strand, ace| {
                Output::set(
                    strand,
                    Mut::slot_mut::<0>(&mut ace.borrow_mut_unwrap()),
                    this,
                );
            });
        Ok(())
    }
}

impl<'v> Object<'v> for Acl {
    const NAME: &'v str = "Acl";
    const MODULE: &'v str = "security.windows";
    const SLOTS: usize = 1;
    type Annex = AclAnnex;
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = strand.state::<WindowsSecurityGlobal<'v>>();
        let revision_sym = global.syms.revision;
        let ([mut iterable], [revision]) = unpack!(strand, args, 1, 0, revision_sym = None)?;
        let revision = revision
            .map(|value| acl_revision(strand, global, &value, &SpecPath::root("revision")))
            .transpose()?;

        iterable.iter(strand, &mut out).await?;
        let mut aces = Vec::new();
        while out.next(strand, &mut iterable).await? {
            let ace = global.types.ace.cast(&iterable).ok_or_else(|| {
                Error::type_error(strand, "Acl: iterable must contain security.windows.Ace")
            })?;
            let value = ace.enter_sync(strand, |strand, ace| {
                with_ace(ace, strand, VfsAce::to_owned)
            })?;
            aces.push(value);
        }
        let acl = VfsAclBuf::from_aces(&aces, revision)
            .map_err(|error| Error::value(strand, error.to_string()))?;
        this.create_with_annex(strand, Acl, AclAnnex::Owned(acl), out);
        Ok(())
    }

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let basic = builder.sym("BASIC");
        let directory_service = builder.sym("DIRECTORY_SERVICE");
        builder
            .get("revision", move |this, strand, out| {
                let revision = with_acl(this, strand, |acl| acl.revision())?;
                match revision {
                    AclRevision::Basic => Output::set(strand, out, basic),
                    AclRevision::DirectoryService => Output::set(strand, out, directory_service),
                    AclRevision::Unknown(value) => Output::set(strand, out, value),
                }
                Ok(())
            })
            .get("size", |this, strand, out| {
                let size = with_acl(this, strand, |acl| acl.size())?;
                Output::set(strand, out, size);
                Ok(())
            })
            .get("aces", |this, strand, out| {
                Output::set(strand, out, ArrayView::new(this, AclAces));
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

pub(crate) enum AceAnnex {
    InAcl(usize),
    Owned(VfsAceBuf),
}

pub(crate) struct Ace;

fn ace_u32<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, u32> {
    let value = value
        .to_i64(strand)
        .map_err(|_| Error::type_error(strand, format!("{path}: expected Int")))?;
    u32::try_from(value).map_err(|_| Error::value(strand, format!("{path}: out of range")))
}

fn ace_u8<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, u8> {
    let value = ace_u32(strand, value, path)?;
    u8::try_from(value).map_err(|_| Error::value(strand, format!("{path}: out of range")))
}

/// Resolves an access mask argument.
///
/// Accepts this module's [`AccessMask`], any flags type that names it as a
/// nominal supertype (a domain-specific mask such as `winreg.AccessMask`,
/// read through the `int` field that contract requires), a symbol or
/// iterable of symbols naming generic rights, or a raw integer.
async fn ace_mask<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, WindowsSecurityGlobal<'v>>,
    value: &Value<'v>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, WinAccessMask> {
    if let Some(mask) = global.types.access_mask.cast_flags(value) {
        return Ok(mask.0);
    }
    if value.is_instance_of(strand, global.types.access_mask) {
        return strand.with_slots_sync(|strand, [mut bits]| {
            value.get(strand, global.syms.int, &mut bits)?;
            Ok(WinAccessMask::from_bits_retain(ace_u32(
                strand, &bits, path,
            )?))
        });
    }
    if value.as_int(strand).is_some() {
        return Ok(WinAccessMask::from_bits_retain(ace_u32(
            strand, value, path,
        )?));
    }
    let mask = global
        .types
        .access_mask
        .coerce(strand, value)
        .await
        .map_err(|_| {
            Error::type_error(
                strand,
                format!(
                    "{path}: expected security.windows.AccessMask (or a subtype), Int, Sym, or \
                     iterable of Sym"
                ),
            )
        })?;
    Ok(mask.0)
}

/// Resolves an ACE header flags argument.
///
/// Accepts [`AceFlags`], a symbol or iterable of symbols naming its bits, or
/// a raw integer.
async fn ace_flags<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, WinAceFlags> {
    let ace_flags = strand.state::<WindowsSecurityGlobal<'v>>().types.ace_flags;
    if let Some(flags) = ace_flags.cast_flags(value) {
        return Ok(flags.0);
    }
    if value.is_int(strand) {
        return Ok(WinAceFlags::from_bits_retain(ace_u8(strand, value, path)?));
    }
    ace_flags
        .coerce(strand, value)
        .await
        .map(|flags| flags.0)
        .map_err(|_| {
            Error::type_error(
                strand,
                format!("{path}: expected security.windows.AceFlags, Int, Sym, or iterable of Sym"),
            )
        })
}

fn ace_bool<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, bool> {
    value
        .as_bool(strand)
        .ok_or_else(|| Error::type_error(strand, format!("{path}: expected Bool")))
}

fn with_ace<'v, 's, T>(
    this: Instance<'v, '_, Ace>,
    strand: &mut Strand<'v, 's>,
    f: impl FnOnce(&VfsAce) -> T,
) -> Result<'v, 's, T> {
    if let AceAnnex::Owned(ace) = &*this.annex() {
        return Ok(f(ace));
    }
    let global = strand.state::<WindowsSecurityGlobal<'v>>();
    let borrow = this.borrow(strand)?;
    let acl = global
        .types
        .acl
        .cast(Ref::slot::<0>(&borrow))
        .expect("Ace root is an Acl");
    let index = match &*this.annex() {
        AceAnnex::InAcl(index) => *index,
        AceAnnex::Owned(_) => unreachable!(),
    };
    acl.enter_sync(strand, |strand, acl| {
        with_acl(acl, strand, |acl| {
            let ace = acl
                .aces()
                .nth(index)
                .expect("Ace array index was normalized");
            f(ace)
        })
    })
}

impl<'v> Object<'v> for Ace {
    const NAME: &'v str = "Ace";
    const MODULE: &'v str = "security.windows";
    const SLOTS: usize = 1;
    type Annex = AceAnnex;
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = strand.state::<WindowsSecurityGlobal<'v>>();
        let allow_sym = global.syms.allow;
        let deny_sym = global.syms.deny;
        let audit_sym = global.syms.audit;
        let mask_sym = global.syms.mask;
        let flags_sym = global.syms.flags;
        let object_type_sym = global.syms.object_type;
        let inherited_object_type_sym = global.syms.inherited_object_type;
        let callback_sym = global.syms.callback;
        let application_data_sym = global.syms.application_data;
        let successful_sym = global.syms.successful;
        let failed_sym = global.syms.failed;
        let (
            [mask_value],
            [
                allow_value,
                deny_value,
                audit_value,
                flags_value,
                object_type_value,
                inherited_value,
                callback_value,
                application_value,
                successful_value,
                failed_value,
            ],
        ) = unpack!(
            strand,
            args,
            0,
            0,
            mask_sym,
            allow_sym = None,
            deny_sym = None,
            audit_sym = None,
            flags_sym = None,
            object_type_sym = None,
            inherited_object_type_sym = None,
            callback_sym = None,
            application_data_sym = None,
            successful_sym = None,
            failed_sym = None
        )?;

        enum Trustee {
            Allow(VfsSid),
            Deny(VfsSid),
            Audit(VfsSid),
        }
        let trustee = match (
            allow_value.as_deref(),
            deny_value.as_deref(),
            audit_value.as_deref(),
        ) {
            (Some(value), None, None) => Trustee::Allow(
                global
                    .types
                    .sid
                    .cast(value)
                    .ok_or_else(|| {
                        Error::type_error(strand, "Ace.allow: expected security.windows.Sid")
                    })?
                    .enter_sync(strand, |_strand, sid| (*sid.annex()).clone()),
            ),
            (None, Some(value), None) => Trustee::Deny(
                global
                    .types
                    .sid
                    .cast(value)
                    .ok_or_else(|| {
                        Error::type_error(strand, "Ace.deny: expected security.windows.Sid")
                    })?
                    .enter_sync(strand, |_strand, sid| (*sid.annex()).clone()),
            ),
            (None, None, Some(value)) => Trustee::Audit(
                global
                    .types
                    .sid
                    .cast(value)
                    .ok_or_else(|| {
                        Error::type_error(strand, "Ace.audit: expected security.windows.Sid")
                    })?
                    .enter_sync(strand, |_strand, sid| (*sid.annex()).clone()),
            ),
            (None, None, None) => {
                return Err(Error::value(
                    strand,
                    "Ace: expected one of allow, deny, audit",
                ));
            }
            _ => {
                return Err(Error::value(
                    strand,
                    "Ace: multiple arguments name conflicting ACE types",
                ));
            }
        };

        let path = SpecPath::root("Ace");
        let mask = global
            .types
            .access_mask
            .cast_flags(&mask_value)
            .map(|mask| mask.0)
            .ok_or_else(|| {
                Error::type_error(strand, "Ace.mask: expected security.windows.AccessMask")
            })?;
        let mut options = AceBuildOptions::new();
        if let Some(value) = flags_value.as_deref() {
            let flags = global.types.ace_flags.cast_flags(value).ok_or_else(|| {
                Error::type_error(strand, "Ace.flags: expected security.windows.AceFlags")
            })?;
            options = options.flags(flags.0);
        }
        if let Some(value) = object_type_value.as_deref() {
            let guid = dolang_ext_uuid::downcast_guid(strand, value)
                .ok_or_else(|| Error::type_error(strand, "Ace.object_type: expected uuid.Guid"))?;
            options = options.object_type(guid);
        }
        if let Some(value) = inherited_value.as_deref() {
            let guid = dolang_ext_uuid::downcast_guid(strand, value).ok_or_else(|| {
                Error::type_error(strand, "Ace.inherited_object_type: expected uuid.Guid")
            })?;
            options = options.inherited_object_type(guid);
        }
        if callback_value
            .as_deref()
            .map(|value| ace_bool(strand, value, &path.key("callback")))
            .transpose()?
            .unwrap_or(false)
        {
            options = options.callback();
        }
        if let Some(value) = application_value.as_deref() {
            let data = value
                .as_bin(strand)
                .ok_or_else(|| Error::type_error(strand, "Ace.application_data: expected Bin"))?;
            options = options.application_data(data.to_vec());
        }
        let successful = successful_value
            .as_deref()
            .map(|value| ace_bool(strand, value, &path.key("successful")))
            .transpose()?;
        let failed = failed_value
            .as_deref()
            .map(|value| ace_bool(strand, value, &path.key("failed")))
            .transpose()?;
        let ace = match trustee {
            Trustee::Allow(_) | Trustee::Deny(_) if successful.is_some() || failed.is_some() => {
                return Err(Error::value(
                    strand,
                    "Ace: successful and failed apply only to audit",
                ));
            }
            Trustee::Allow(sid) => VfsAceBuf::allow(&sid, mask, options),
            Trustee::Deny(sid) => VfsAceBuf::deny(&sid, mask, options),
            Trustee::Audit(_) if successful.is_none() && failed.is_none() => {
                return Err(Error::value(
                    strand,
                    "Ace: audit requires successful or failed",
                ));
            }
            Trustee::Audit(sid) => VfsAceBuf::audit(
                &sid,
                mask,
                successful.unwrap_or(false),
                failed.unwrap_or(false),
                options,
            ),
        }
        .map_err(|error| Error::value(strand, format!("Ace: {error}")))?;
        this.create_with_annex(strand, Ace, AceAnnex::Owned(ace), out);
        Ok(())
    }

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
                    _ => unknown,
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
                let flags = strand.state::<WindowsSecurityGlobal<'v>>().types.ace_flags;
                flags.create_flags(strand, AceFlags(value), out);
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
                let global = strand.state::<WindowsSecurityGlobal<'v>>();
                global
                    .types
                    .access_mask
                    .create_flags(strand, AccessMask(value), out);
                Ok(())
            })
            .get("sid", move |this, strand, mut out| {
                let Some(value) = with_ace(this, strand, |ace| ace.sid())? else {
                    return Err(Error::field(strand, sid_field));
                };
                let global = strand.state::<WindowsSecurityGlobal<'v>>();
                create_sid(strand, global, value, &mut out);
                Ok(())
            })
            .get("object_flags", move |this, strand, out| {
                let Some(value) = with_ace(this, strand, |ace| ace.object_flags())? else {
                    return Err(Error::field(strand, object_flags_field));
                };
                Output::set(strand, out, value.bits());
                Ok(())
            })
            .get("object_type", move |this, strand, out| {
                let (flags, value) =
                    with_ace(this, strand, |ace| (ace.object_flags(), ace.object_type()))?;
                if flags.is_none() {
                    return Err(Error::field(strand, object_type_field));
                }
                if let Some(value) = value {
                    dolang_ext_uuid::create_guid(strand, value, out);
                } else {
                    Output::set(strand, out, Nil);
                }
                Ok(())
            })
            .get("inherited_object_type", move |this, strand, out| {
                let (flags, value) = with_ace(this, strand, |ace| {
                    (ace.object_flags(), ace.inherited_object_type())
                })?;
                if flags.is_none() {
                    return Err(Error::field(strand, inherited_object_type_field));
                }
                if let Some(value) = value {
                    dolang_ext_uuid::create_guid(strand, value, out);
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
                Output::set(strand, out, flags.contains(WinAceFlags::SUCCESSFUL_ACCESS));
                Ok(())
            })
            .get("failed_access", move |this, strand, out| {
                let (kind, flags) = with_ace(this, strand, |ace| (ace.ace_type(), ace.flags()))?;
                if !ace_is_audit(kind) {
                    return Err(Error::field(strand, failed_access_field));
                }
                Output::set(strand, out, flags.contains(WinAceFlags::FAILED_ACCESS));
                Ok(())
            })
            .get("trust_protected_filter", move |this, strand, out| {
                let (kind, flags) = with_ace(this, strand, |ace| (ace.ace_type(), ace.flags()))?;
                if kind != VfsAceType::SystemAccessFilter {
                    return Err(Error::field(strand, trust_protected_filter_field));
                }
                Output::set(
                    strand,
                    out,
                    flags.contains(WinAceFlags::TRUST_PROTECTED_FILTER),
                );
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
    Output::set(
        strand,
        out,
        flags.contains(WinAceFlags::from_bits_retain(flag)),
    );
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

#[derive(Default)]
struct SecDescComponents {
    owner: NullableComponent<VfsSid>,
    group: NullableComponent<VfsSid>,
    dacl: NullableComponent<VfsAclBuf>,
    sacl: NullableComponent<VfsAclBuf>,
    owner_defaulted: Option<bool>,
    group_defaulted: Option<bool>,
    dacl_present: Option<bool>,
    dacl_defaulted: Option<bool>,
    dacl_auto_inherit_required: Option<bool>,
    dacl_auto_inherited: Option<bool>,
    dacl_protected: Option<bool>,
    sacl_present: Option<bool>,
    sacl_defaulted: Option<bool>,
    sacl_auto_inherit_required: Option<bool>,
    sacl_auto_inherited: Option<bool>,
    sacl_protected: Option<bool>,
    rm_control: NullableComponent<u8>,
}

#[derive(Default)]
enum NullableComponent<T> {
    #[default]
    Unspecified,
    Set(T),
    Clear,
}

impl<T> NullableComponent<T> {
    fn from_option(value: Option<T>) -> Self {
        match value {
            Some(value) => Self::Set(value),
            None => Self::Clear,
        }
    }

    fn is_unspecified(&self) -> bool {
        matches!(self, Self::Unspecified)
    }
}

impl SecDescComponents {
    fn is_empty(&self) -> bool {
        self.owner.is_unspecified()
            && self.group.is_unspecified()
            && self.dacl.is_unspecified()
            && self.sacl.is_unspecified()
            && self.owner_defaulted.is_none()
            && self.group_defaulted.is_none()
            && self.dacl_present.is_none()
            && self.dacl_defaulted.is_none()
            && self.dacl_auto_inherit_required.is_none()
            && self.dacl_auto_inherited.is_none()
            && self.dacl_protected.is_none()
            && self.sacl_present.is_none()
            && self.sacl_defaulted.is_none()
            && self.sacl_auto_inherit_required.is_none()
            && self.sacl_auto_inherited.is_none()
            && self.sacl_protected.is_none()
            && self.rm_control.is_unspecified()
    }

    fn into_update(self) -> VfsSecDescUpdate {
        let mut update = VfsSecDescUpdate::new();
        match self.owner {
            NullableComponent::Unspecified => {}
            NullableComponent::Set(value) => update = update.owner(Some(value)),
            NullableComponent::Clear => update = update.owner(None),
        }
        match self.group {
            NullableComponent::Unspecified => {}
            NullableComponent::Set(value) => update = update.group(Some(value)),
            NullableComponent::Clear => update = update.group(None),
        }
        match self.dacl {
            NullableComponent::Unspecified => {}
            NullableComponent::Set(value) => update = update.dacl(Some(value)),
            NullableComponent::Clear => update = update.dacl(None),
        }
        match self.sacl {
            NullableComponent::Unspecified => {}
            NullableComponent::Set(value) => update = update.sacl(Some(value)),
            NullableComponent::Clear => update = update.sacl(None),
        }
        if let Some(value) = self.owner_defaulted {
            update = update.owner_defaulted(value);
        }
        if let Some(value) = self.group_defaulted {
            update = update.group_defaulted(value);
        }
        if let Some(value) = self.dacl_present {
            update = update.dacl_present(value);
        }
        if let Some(value) = self.dacl_defaulted {
            update = update.dacl_defaulted(value);
        }
        if let Some(value) = self.dacl_auto_inherit_required {
            update = update.dacl_auto_inherit_required(value);
        }
        if let Some(value) = self.dacl_auto_inherited {
            update = update.dacl_auto_inherited(value);
        }
        if let Some(value) = self.dacl_protected {
            update = update.dacl_protected(value);
        }
        if let Some(value) = self.sacl_present {
            update = update.sacl_present(value);
        }
        if let Some(value) = self.sacl_defaulted {
            update = update.sacl_defaulted(value);
        }
        if let Some(value) = self.sacl_auto_inherit_required {
            update = update.sacl_auto_inherit_required(value);
        }
        if let Some(value) = self.sacl_auto_inherited {
            update = update.sacl_auto_inherited(value);
        }
        if let Some(value) = self.sacl_protected {
            update = update.sacl_protected(value);
        }
        match self.rm_control {
            NullableComponent::Unspecified => {}
            NullableComponent::Set(value) => update = update.rm_control(Some(value)),
            NullableComponent::Clear => update = update.rm_control(None),
        }
        update
    }
}

fn downcast_sid_component<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, WindowsSecurityGlobal<'v>>,
    value: Option<&Value<'v>>,
    name: &'static str,
) -> Result<'v, 's, NullableComponent<VfsSid>> {
    let Some(value) = value else {
        return Ok(NullableComponent::Unspecified);
    };
    if value.is_nil() {
        return Ok(NullableComponent::Clear);
    }
    global
        .types
        .sid
        .cast(value)
        .map(|value| {
            value.enter_sync(strand, |_strand, value| {
                NullableComponent::Set((*value.annex()).clone())
            })
        })
        .ok_or_else(|| {
            Error::type_error(
                strand,
                format!("{name}: expected security.windows.Sid or nil"),
            )
        })
}

fn downcast_acl_component<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, WindowsSecurityGlobal<'v>>,
    value: Option<&Value<'v>>,
    name: &'static str,
) -> Result<'v, 's, NullableComponent<VfsAclBuf>> {
    let Some(value) = value else {
        return Ok(NullableComponent::Unspecified);
    };
    if value.is_nil() {
        return Ok(NullableComponent::Clear);
    }
    let value = global.types.acl.cast(value).ok_or_else(|| {
        Error::type_error(
            strand,
            format!("{name}: expected security.windows.Acl or nil"),
        )
    })?;
    value.enter_sync(strand, |strand, value| {
        with_acl(value, strand, |acl| NullableComponent::Set(acl.to_owned()))
    })
}

fn parse_bool_component<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: Option<&Value<'v>>,
    name: &'static str,
) -> Result<'v, 's, Option<bool>> {
    value
        .map(|value| ace_bool(strand, value, &SpecPath::root(name)))
        .transpose()
}

/// Coerces a SID: a [`Sid`], its canonical string or native packet, or a
/// symbol naming a well-known SID.
fn coerce_sid<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, WindowsSecurityGlobal<'v>>,
    value: &Value<'v>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, VfsSid> {
    if let Some(sid) = global.types.sid.cast(value) {
        return Ok(sid.enter_sync(strand, |_strand, sid| (*sid.annex()).clone()));
    }
    sid_from_value(strand, value, path)
}

/// Coerces the SID spellings that cannot mean anything else: a [`Sid`], its
/// native packet, or a symbol naming a well-known SID.
///
/// Yields `Ok(None)` for everything else, including a `Str`. Callers that give
/// a string its own meaning — an account name to look up, say — use this
/// instead of [`coerce_sid`] and handle the string themselves.
fn coerce_sid_non_str<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, WindowsSecurityGlobal<'v>>,
    value: &Value<'v>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, Option<VfsSid>> {
    if let Some(sid) = global.types.sid.cast(value) {
        Ok(Some(
            sid.enter_sync(strand, |_strand, sid| (*sid.annex()).clone()),
        ))
    } else if let Some(sym) = value.as_sym(strand.vm()) {
        sid_from_sym(strand, sym, path).map(Some)
    } else if let Some(value) = value.as_bin(strand) {
        let bytes = value.to_vec();
        VfsSid::from_bytes(&bytes)
            .map(Some)
            .map_err(|error| Error::value(strand, format!("{path}: {error}")))
    } else {
        Ok(None)
    }
}

/// Coerces an [`Ace`] or a declarative ACE spec into a native ACE.
///
/// A spec names its trustee under exactly one of `allow:`, `deny:`, or
/// `audit:`; the remaining keys are the same options the rigid constructors
/// take.
#[derive(Default)]
struct AceComponents {
    allow: Option<VfsSid>,
    deny: Option<VfsSid>,
    audit: Option<VfsSid>,
    mask: Option<WinAccessMask>,
    flags: Option<WinAceFlags>,
    object_type: Option<Guid>,
    inherited_object_type: Option<Guid>,
    callback: Option<bool>,
    application_data: Option<Vec<u8>>,
    successful: Option<bool>,
    failed: Option<bool>,
}

async fn ace_components_from_spec<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, WindowsSecurityGlobal<'v>>,
    dict: Dict<'v, '_>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, AceComponents> {
    let mut pairs = dict.pairs();
    let mut components = AceComponents::default();
    strand
        .with_slots(async |strand, [mut key, mut entry]| {
            while pairs.next(strand, &mut key, &mut entry)? {
                let Some(sym) = key.as_sym(strand.vm()) else {
                    return Err(Error::value(
                        strand,
                        format!("{path}: keys must be symbols"),
                    ));
                };
                if sym == global.syms.allow {
                    if components.allow.is_some() {
                        return Err(Error::value(
                            strand,
                            format!("{path}: duplicate key `allow`"),
                        ));
                    }
                    components.allow =
                        Some(coerce_sid(strand, global, &entry, &path.key("allow"))?);
                } else if sym == global.syms.deny {
                    if components.deny.is_some() {
                        return Err(Error::value(
                            strand,
                            format!("{path}: duplicate key `deny`"),
                        ));
                    }
                    components.deny = Some(coerce_sid(strand, global, &entry, &path.key("deny"))?);
                } else if sym == global.syms.audit {
                    if components.audit.is_some() {
                        return Err(Error::value(
                            strand,
                            format!("{path}: duplicate key `audit`"),
                        ));
                    }
                    components.audit =
                        Some(coerce_sid(strand, global, &entry, &path.key("audit"))?);
                } else if sym == global.syms.mask {
                    if components.mask.is_some() {
                        return Err(Error::value(
                            strand,
                            format!("{path}: duplicate key `mask`"),
                        ));
                    }
                    components.mask =
                        Some(ace_mask(strand, global, &entry, &path.key("mask")).await?);
                } else if sym == global.syms.flags {
                    if components.flags.is_some() {
                        return Err(Error::value(
                            strand,
                            format!("{path}: duplicate key `flags`"),
                        ));
                    }
                    components.flags = Some(ace_flags(strand, &entry, &path.key("flags")).await?);
                } else if sym == global.syms.object_type {
                    if components.object_type.is_some() {
                        return Err(Error::value(
                            strand,
                            format!("{path}: duplicate key `object_type`"),
                        ));
                    }
                    components.object_type = Some(dolang_ext_uuid::value_to_guid(strand, &entry)?);
                } else if sym == global.syms.inherited_object_type {
                    if components.inherited_object_type.is_some() {
                        return Err(Error::value(
                            strand,
                            format!("{path}: duplicate key `inherited_object_type`"),
                        ));
                    }
                    components.inherited_object_type =
                        Some(dolang_ext_uuid::value_to_guid(strand, &entry)?);
                } else if sym == global.syms.callback {
                    if components.callback.is_some() {
                        return Err(Error::value(
                            strand,
                            format!("{path}: duplicate key `callback`"),
                        ));
                    }
                    components.callback = Some(ace_bool(strand, &entry, &path.key("callback"))?);
                } else if sym == global.syms.application_data {
                    if components.application_data.is_some() {
                        return Err(Error::value(
                            strand,
                            format!("{path}: duplicate key `application_data`"),
                        ));
                    }
                    components.application_data = Some(
                        entry
                            .as_bin(strand)
                            .map(|value| value.to_vec())
                            .ok_or_else(|| {
                                Error::type_error(
                                    strand,
                                    format!("{}: expected Bin", path.key("application_data")),
                                )
                            })?,
                    );
                } else if sym == global.syms.successful {
                    if components.successful.is_some() {
                        return Err(Error::value(
                            strand,
                            format!("{path}: duplicate key `successful`"),
                        ));
                    }
                    components.successful =
                        Some(ace_bool(strand, &entry, &path.key("successful"))?);
                } else if sym == global.syms.failed {
                    if components.failed.is_some() {
                        return Err(Error::value(
                            strand,
                            format!("{path}: duplicate key `failed`"),
                        ));
                    }
                    components.failed = Some(ace_bool(strand, &entry, &path.key("failed"))?);
                } else {
                    return Err(Error::value(
                        strand,
                        format!("{path}: unknown key `{}`", sym.as_str(strand.vm())),
                    ));
                }
            }
            Ok::<_, Error<'v, 's>>(())
        })
        .await?;
    Ok(components)
}

async fn coerce_ace<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, WindowsSecurityGlobal<'v>>,
    value: &Value<'v>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, VfsAceBuf> {
    if let Some(ace) = global.types.ace.cast(value) {
        return ace.enter_sync(strand, |strand, ace| {
            with_ace(ace, strand, VfsAce::to_owned)
        });
    }
    let Some(dict) = value.as_dict(strand.vm()) else {
        return Err(Error::type_error(
            strand,
            format!("{path}: expected a dictionary"),
        ));
    };
    let components = ace_components_from_spec(strand, global, dict, path).await?;

    ace_from_components(strand, components, path)
}

fn ace_from_components<'v, 's>(
    strand: &mut Strand<'v, 's>,
    components: AceComponents,
    path: &SpecPath<'_>,
) -> Result<'v, 's, VfsAceBuf> {
    enum Trustee {
        Allow(VfsSid),
        Deny(VfsSid),
        Audit(VfsSid),
    }
    let trustee = match (components.allow, components.deny, components.audit) {
        (Some(sid), None, None) => Trustee::Allow(sid),
        (None, Some(sid), None) => Trustee::Deny(sid),
        (None, None, Some(sid)) => Trustee::Audit(sid),
        (None, None, None) => {
            return Err(Error::value(
                strand,
                format!("{path}: expected one of allow, deny, audit"),
            ));
        }
        _ => {
            return Err(Error::value(
                strand,
                format!("{path}: multiple keys name conflicting ACE types"),
            ));
        }
    };

    let Some(mask) = components.mask else {
        return Err(Error::value(strand, format!("{path}: missing key `mask`")));
    };
    let mut options = AceBuildOptions::new();
    if let Some(flags) = components.flags {
        options = options.flags(flags);
    }
    if let Some(object_type) = components.object_type {
        options = options.object_type(object_type);
    }
    if let Some(inherited_object_type) = components.inherited_object_type {
        options = options.inherited_object_type(inherited_object_type);
    }
    if components.callback.unwrap_or(false) {
        options = options.callback();
    }
    if let Some(application_data) = components.application_data {
        options = options.application_data(application_data);
    }
    let successful = components.successful;
    let failed = components.failed;
    let ace = match trustee {
        Trustee::Allow(_) | Trustee::Deny(_) if successful.is_some() || failed.is_some() => {
            return Err(Error::value(
                strand,
                format!("{path}: successful and failed apply only to audit"),
            ));
        }
        Trustee::Allow(sid) => VfsAceBuf::allow(&sid, mask, options),
        Trustee::Deny(sid) => VfsAceBuf::deny(&sid, mask, options),
        Trustee::Audit(_) if successful.is_none() && failed.is_none() => {
            return Err(Error::value(
                strand,
                format!("{path}: audit requires successful or failed"),
            ));
        }
        Trustee::Audit(sid) => VfsAceBuf::audit(
            &sid,
            mask,
            successful.unwrap_or(false),
            failed.unwrap_or(false),
            options,
        ),
    };
    ace.map_err(|error| Error::value(strand, format!("{path}: {error}")))
}

/// Reads an ACE from the lowercase `ace` function's named arguments.
async fn ace_from_args<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, WindowsSecurityGlobal<'v>>,
    args: Args<'v, '_>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, VfsAceBuf> {
    let allow_sym = global.syms.allow;
    let deny_sym = global.syms.deny;
    let audit_sym = global.syms.audit;
    let mask_sym = global.syms.mask;
    let flags_sym = global.syms.flags;
    let object_type_sym = global.syms.object_type;
    let inherited_object_type_sym = global.syms.inherited_object_type;
    let callback_sym = global.syms.callback;
    let application_data_sym = global.syms.application_data;
    let successful_sym = global.syms.successful;
    let failed_sym = global.syms.failed;
    let (
        [mask],
        [
            allow,
            deny,
            audit,
            flags,
            object_type,
            inherited_object_type,
            callback,
            application_data,
            successful,
            failed,
        ],
    ) = unpack!(
        strand,
        args,
        0,
        0,
        mask_sym,
        allow_sym = None,
        deny_sym = None,
        audit_sym = None,
        flags_sym = None,
        object_type_sym = None,
        inherited_object_type_sym = None,
        callback_sym = None,
        application_data_sym = None,
        successful_sym = None,
        failed_sym = None
    )?;

    let components = AceComponents {
        allow: allow
            .as_deref()
            .map(|value| coerce_sid(strand, global, value, &path.key("allow")))
            .transpose()?,
        deny: deny
            .as_deref()
            .map(|value| coerce_sid(strand, global, value, &path.key("deny")))
            .transpose()?,
        audit: audit
            .as_deref()
            .map(|value| coerce_sid(strand, global, value, &path.key("audit")))
            .transpose()?,
        mask: Some(ace_mask(strand, global, &mask, &path.key("mask")).await?),
        flags: match flags.as_deref() {
            Some(value) => Some(ace_flags(strand, value, &path.key("flags")).await?),
            None => None,
        },
        object_type: object_type
            .as_deref()
            .map(|value| dolang_ext_uuid::value_to_guid(strand, value))
            .transpose()?,
        inherited_object_type: inherited_object_type
            .as_deref()
            .map(|value| dolang_ext_uuid::value_to_guid(strand, value))
            .transpose()?,
        callback: callback
            .as_deref()
            .map(|value| ace_bool(strand, value, &path.key("callback")))
            .transpose()?,
        application_data: application_data
            .as_deref()
            .map(|value| {
                value
                    .as_bin(strand)
                    .map(|value| value.to_vec())
                    .ok_or_else(|| {
                        Error::type_error(
                            strand,
                            format!("{}: expected Bin", path.key("application_data")),
                        )
                    })
            })
            .transpose()?,
        successful: successful
            .as_deref()
            .map(|value| ace_bool(strand, value, &path.key("successful")))
            .transpose()?,
        failed: failed
            .as_deref()
            .map(|value| ace_bool(strand, value, &path.key("failed")))
            .transpose()?,
    };
    ace_from_components(strand, components, path)
}

/// Coerces an [`Acl`], a sequence of ACE specs, or a dict of ACE entries
/// with named options, into a native ACL.
struct AclComponents {
    aces: Vec<VfsAceBuf>,
    revision: Option<AclRevision>,
}

async fn acl_components_from_spec<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, WindowsSecurityGlobal<'v>>,
    dict: Dict<'v, '_>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, AclComponents> {
    let mut components = AclComponents {
        aces: Vec::new(),
        revision: None,
    };
    let mut index = 0_usize;
    let mut pairs = dict.pairs();
    strand
        .with_slots(async |strand, [mut key, mut entry]| {
            while pairs.next(strand, &mut key, &mut entry)? {
                if let Some(sym) = key.as_sym(strand.vm()) {
                    if sym != global.syms.revision {
                        return Err(Error::value(
                            strand,
                            format!(
                                "{path}: unknown key `{}`; expected revision",
                                sym.as_str(strand.vm())
                            ),
                        ));
                    }
                    if components.revision.is_some() {
                        return Err(Error::value(
                            strand,
                            format!("{path}: duplicate key `revision`"),
                        ));
                    }
                    components.revision =
                        Some(acl_revision(strand, global, &entry, &path.key("revision"))?);
                } else if let Some(found) = key.as_int(strand) {
                    if found != i128::try_from(index).expect("entry count fits in i128") {
                        return Err(Error::value(
                            strand,
                            format!("{path}: expected entry {index}, found {found}"),
                        ));
                    }
                    components
                        .aces
                        .push(coerce_ace(strand, global, &entry, &path.index(index)).await?);
                    index += 1;
                } else {
                    return Err(Error::value(
                        strand,
                        format!("{path}: keys must be symbols or entry indices"),
                    ));
                }
            }
            Ok::<_, Error<'v, 's>>(())
        })
        .await?;
    Ok(components)
}

async fn coerce_acl<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, WindowsSecurityGlobal<'v>>,
    value: &Value<'v>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, VfsAclBuf> {
    if let Some(acl) = global.types.acl.cast(value) {
        return acl.enter_sync(strand, |strand, acl| {
            with_acl(acl, strand, |acl| acl.to_owned())
        });
    }

    if let Some(dict) = value.as_dict(strand.vm()) {
        let components = acl_components_from_spec(strand, global, dict, path).await?;
        return VfsAclBuf::from_aces(&components.aces, components.revision)
            .map_err(|error| Error::value(strand, format!("{path}: {error}")));
    }

    let mut aces = Vec::new();
    strand
        .with_slots(async |strand, [mut iter, mut item]| {
            value.iter(strand, &mut iter).await?;
            let mut index = 0;
            while iter.next(strand, &mut item).await? {
                aces.push(coerce_ace(strand, global, &item, &path.index(index)).await?);
                index += 1;
            }
            Ok::<_, Error<'v, 's>>(())
        })
        .await?;
    VfsAclBuf::from_aces(&aces, None)
        .map_err(|error| Error::value(strand, format!("{path}: {error}")))
}

/// Reads an ACL from the lowercase `acl` function's mixed arguments.
///
/// Positional arguments are ACE values or specs, while `revision:` is an
/// optional keyword argument.
async fn acl_from_args<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, WindowsSecurityGlobal<'v>>,
    args: Args<'v, '_>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, VfsAclBuf> {
    let revision_sym = global.syms.revision;
    let ([], [revision], entries) = unpack!(strand, args, 0, 0, revision_sym = None, ...)?;
    let revision = revision
        .as_deref()
        .map(|value| acl_revision(strand, global, value, &path.key("revision")))
        .transpose()?;

    let mut aces = Vec::new();
    for (index, entry) in entries.enumerate() {
        let entry = match entry {
            Arg::Pos(entry) => entry,
            Arg::Key(key, _) => {
                return Err(Error::value(
                    strand,
                    format!("{path}: unknown key `{}`", key.as_str(strand.vm())),
                ));
            }
        };
        aces.push(coerce_ace(strand, global, &entry, &path.index(index)).await?);
    }
    VfsAclBuf::from_aces(&aces, revision)
        .map_err(|error| Error::value(strand, format!("{path}: {error}")))
}

/// Parses an ACL revision option.
fn acl_revision<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, WindowsSecurityGlobal<'v>>,
    value: &Value<'v>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, AclRevision> {
    if let Some(sym) = value.as_sym(strand.vm()) {
        return if sym == global.syms.basic {
            Ok(AclRevision::Basic)
        } else if sym == global.syms.directory_service {
            Ok(AclRevision::DirectoryService)
        } else {
            Err(Error::value(
                strand,
                format!("{path}: expected BASIC or DIRECTORY_SERVICE"),
            ))
        };
    }
    let value = ace_u8(strand, value, path)?;
    Ok(AclRevision::from(value))
}

/// An empty descriptor, the base every set of component options builds on.
fn empty_sec_desc() -> VfsSecDesc {
    VfsSecDesc::new(
        WinSecInfo::empty(),
        0,
        WinSecDescControl::empty(),
        None,
        None,
        None,
        None,
    )
    .expect("empty security descriptor is valid")
}

fn coerce_sid_option<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, WindowsSecurityGlobal<'v>>,
    value: &Value<'v>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, Option<VfsSid>> {
    if value.is_nil() {
        return Ok(None);
    }
    coerce_sid(strand, global, value, path).map(Some)
}

async fn coerce_acl_option<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, WindowsSecurityGlobal<'v>>,
    value: &Value<'v>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, Option<VfsAclBuf>> {
    if value.is_nil() {
        return Ok(None);
    }
    coerce_acl(strand, global, value, path).await.map(Some)
}

async fn sec_desc_components_from_spec<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, WindowsSecurityGlobal<'v>>,
    dict: Dict<'v, '_>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, SecDescComponents> {
    let mut pairs = dict.pairs();
    let mut components = SecDescComponents::default();
    strand
        .with_slots(async |strand, [mut key, mut entry]| {
            while pairs.next(strand, &mut key, &mut entry)? {
                let Some(sym) = key.as_sym(strand.vm()) else {
                    return Err(Error::value(
                        strand,
                        format!("{path}: keys must be symbols"),
                    ));
                };
                if sym == global.syms.owner {
                    if !components.owner.is_unspecified() {
                        return Err(Error::value(
                            strand,
                            format!("{path}: duplicate key `owner`"),
                        ));
                    }
                    components.owner = NullableComponent::from_option(coerce_sid_option(
                        strand,
                        global,
                        &entry,
                        &path.key("owner"),
                    )?);
                } else if sym == global.syms.group {
                    if !components.group.is_unspecified() {
                        return Err(Error::value(
                            strand,
                            format!("{path}: duplicate key `group`"),
                        ));
                    }
                    components.group = NullableComponent::from_option(coerce_sid_option(
                        strand,
                        global,
                        &entry,
                        &path.key("group"),
                    )?);
                } else if sym == global.syms.dacl {
                    if !components.dacl.is_unspecified() {
                        return Err(Error::value(
                            strand,
                            format!("{path}: duplicate key `dacl`"),
                        ));
                    }
                    components.dacl = NullableComponent::from_option(
                        coerce_acl_option(strand, global, &entry, &path.key("dacl")).await?,
                    );
                } else if sym == global.syms.sacl {
                    if !components.sacl.is_unspecified() {
                        return Err(Error::value(
                            strand,
                            format!("{path}: duplicate key `sacl`"),
                        ));
                    }
                    components.sacl = NullableComponent::from_option(
                        coerce_acl_option(strand, global, &entry, &path.key("sacl")).await?,
                    );
                } else if sym == global.syms.owner_defaulted {
                    if components.owner_defaulted.is_some() {
                        return Err(Error::value(
                            strand,
                            format!("{path}: duplicate key `owner_defaulted`"),
                        ));
                    }
                    components.owner_defaulted =
                        Some(ace_bool(strand, &entry, &path.key("owner_defaulted"))?);
                } else if sym == global.syms.group_defaulted {
                    if components.group_defaulted.is_some() {
                        return Err(Error::value(
                            strand,
                            format!("{path}: duplicate key `group_defaulted`"),
                        ));
                    }
                    components.group_defaulted =
                        Some(ace_bool(strand, &entry, &path.key("group_defaulted"))?);
                } else if sym == global.syms.dacl_present {
                    if components.dacl_present.is_some() {
                        return Err(Error::value(
                            strand,
                            format!("{path}: duplicate key `dacl_present`"),
                        ));
                    }
                    components.dacl_present =
                        Some(ace_bool(strand, &entry, &path.key("dacl_present"))?);
                } else if sym == global.syms.dacl_defaulted {
                    if components.dacl_defaulted.is_some() {
                        return Err(Error::value(
                            strand,
                            format!("{path}: duplicate key `dacl_defaulted`"),
                        ));
                    }
                    components.dacl_defaulted =
                        Some(ace_bool(strand, &entry, &path.key("dacl_defaulted"))?);
                } else if sym == global.syms.dacl_auto_inherit_required {
                    if components.dacl_auto_inherit_required.is_some() {
                        return Err(Error::value(
                            strand,
                            format!("{path}: duplicate key `dacl_auto_inherit_required`"),
                        ));
                    }
                    components.dacl_auto_inherit_required = Some(ace_bool(
                        strand,
                        &entry,
                        &path.key("dacl_auto_inherit_required"),
                    )?);
                } else if sym == global.syms.dacl_auto_inherited {
                    if components.dacl_auto_inherited.is_some() {
                        return Err(Error::value(
                            strand,
                            format!("{path}: duplicate key `dacl_auto_inherited`"),
                        ));
                    }
                    components.dacl_auto_inherited =
                        Some(ace_bool(strand, &entry, &path.key("dacl_auto_inherited"))?);
                } else if sym == global.syms.dacl_protected {
                    if components.dacl_protected.is_some() {
                        return Err(Error::value(
                            strand,
                            format!("{path}: duplicate key `dacl_protected`"),
                        ));
                    }
                    components.dacl_protected =
                        Some(ace_bool(strand, &entry, &path.key("dacl_protected"))?);
                } else if sym == global.syms.sacl_present {
                    if components.sacl_present.is_some() {
                        return Err(Error::value(
                            strand,
                            format!("{path}: duplicate key `sacl_present`"),
                        ));
                    }
                    components.sacl_present =
                        Some(ace_bool(strand, &entry, &path.key("sacl_present"))?);
                } else if sym == global.syms.sacl_defaulted {
                    if components.sacl_defaulted.is_some() {
                        return Err(Error::value(
                            strand,
                            format!("{path}: duplicate key `sacl_defaulted`"),
                        ));
                    }
                    components.sacl_defaulted =
                        Some(ace_bool(strand, &entry, &path.key("sacl_defaulted"))?);
                } else if sym == global.syms.sacl_auto_inherit_required {
                    if components.sacl_auto_inherit_required.is_some() {
                        return Err(Error::value(
                            strand,
                            format!("{path}: duplicate key `sacl_auto_inherit_required`"),
                        ));
                    }
                    components.sacl_auto_inherit_required = Some(ace_bool(
                        strand,
                        &entry,
                        &path.key("sacl_auto_inherit_required"),
                    )?);
                } else if sym == global.syms.sacl_auto_inherited {
                    if components.sacl_auto_inherited.is_some() {
                        return Err(Error::value(
                            strand,
                            format!("{path}: duplicate key `sacl_auto_inherited`"),
                        ));
                    }
                    components.sacl_auto_inherited =
                        Some(ace_bool(strand, &entry, &path.key("sacl_auto_inherited"))?);
                } else if sym == global.syms.sacl_protected {
                    if components.sacl_protected.is_some() {
                        return Err(Error::value(
                            strand,
                            format!("{path}: duplicate key `sacl_protected`"),
                        ));
                    }
                    components.sacl_protected =
                        Some(ace_bool(strand, &entry, &path.key("sacl_protected"))?);
                } else if sym == global.syms.rm_control {
                    if !components.rm_control.is_unspecified() {
                        return Err(Error::value(
                            strand,
                            format!("{path}: duplicate key `rm_control`"),
                        ));
                    }
                    components.rm_control = if entry.is_nil() {
                        NullableComponent::Clear
                    } else {
                        NullableComponent::Set(ace_u8(strand, &entry, &path.key("rm_control"))?)
                    };
                } else {
                    return Err(Error::value(
                        strand,
                        format!("{path}: unknown key `{}`", sym.as_str(strand.vm())),
                    ));
                }
            }
            Ok::<_, Error<'v, 's>>(())
        })
        .await?;
    Ok(components)
}

/// Coerces a [`SecDesc`], a self-relative packet, or a declarative
/// descriptor spec into a native security descriptor.
async fn coerce_sec_desc<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, WindowsSecurityGlobal<'v>>,
    value: &Value<'v>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, VfsSecDesc> {
    if let Some(descriptor) = global.types.sec_desc.cast(value) {
        return Ok(descriptor.enter_sync(strand, |_strand, descriptor| descriptor.annex().clone()));
    }
    if let Some(packet) = value.as_bin(strand) {
        let bytes = packet.to_vec();
        return VfsSecDesc::from_bytes(&bytes)
            .map_err(|error| Error::value(strand, format!("{path}: {error}")));
    }

    let Some(dict) = value.as_dict(strand.vm()) else {
        return Err(Error::type_error(
            strand,
            format!("{path}: expected a dictionary"),
        ));
    };
    let components = sec_desc_components_from_spec(strand, global, dict, path).await?;
    empty_sec_desc()
        .with(components.into_update())
        .map_err(|error| Error::value(strand, format!("{path}: {error}")))
}

pub(crate) async fn sec_desc_from_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, WindowsSecurityGlobal<'v>>,
    value: &Value<'v>,
    name: &str,
) -> Result<'v, 's, VfsSecDesc> {
    coerce_sec_desc(strand, global, value, &SpecPath::root(name)).await
}

pub(crate) fn create_sec_desc<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, WindowsSecurityGlobal<'v>>,
    sec_desc: VfsSecDesc,
    out: &mut Slot<'v, '_>,
) {
    global
        .types
        .sec_desc
        .create_with_annex(strand, SecDesc, sec_desc, out);
}

/// Reads a descriptor from an API's own arguments.
///
/// Accepts a positional descriptor — a [`SecDesc`], a packet, or a spec —
/// and the descriptor's component options as keyword arguments. Given both,
/// the options amend the positional descriptor, exactly as
/// [`SecDesc::with`](SecDesc) would.
pub(crate) async fn sec_desc_from_args<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, WindowsSecurityGlobal<'v>>,
    args: Args<'v, '_>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, VfsSecDesc> {
    let owner = global.syms.owner;
    let group = global.syms.group;
    let dacl = global.syms.dacl;
    let sacl = global.syms.sacl;
    let owner_defaulted = global.syms.owner_defaulted;
    let group_defaulted = global.syms.group_defaulted;
    let dacl_present = global.syms.dacl_present;
    let dacl_defaulted = global.syms.dacl_defaulted;
    let dacl_auto_inherit_required = global.syms.dacl_auto_inherit_required;
    let dacl_auto_inherited = global.syms.dacl_auto_inherited;
    let dacl_protected = global.syms.dacl_protected;
    let sacl_present = global.syms.sacl_present;
    let sacl_defaulted = global.syms.sacl_defaulted;
    let sacl_auto_inherit_required = global.syms.sacl_auto_inherit_required;
    let sacl_auto_inherited = global.syms.sacl_auto_inherited;
    let sacl_protected = global.syms.sacl_protected;
    let rm_control = global.syms.rm_control;
    let (
        [],
        [
            value,
            owner_value,
            group_value,
            dacl_value,
            sacl_value,
            owner_defaulted_value,
            group_defaulted_value,
            dacl_present_value,
            dacl_defaulted_value,
            dacl_auto_inherit_required_value,
            dacl_auto_inherited_value,
            dacl_protected_value,
            sacl_present_value,
            sacl_defaulted_value,
            sacl_auto_inherit_required_value,
            sacl_auto_inherited_value,
            sacl_protected_value,
            rm_control_value,
        ],
    ) = unpack!(
        strand,
        args,
        0,
        1,
        owner = None,
        group = None,
        dacl = None,
        sacl = None,
        owner_defaulted = None,
        group_defaulted = None,
        dacl_present = None,
        dacl_defaulted = None,
        dacl_auto_inherit_required = None,
        dacl_auto_inherited = None,
        dacl_protected = None,
        sacl_present = None,
        sacl_defaulted = None,
        sacl_auto_inherit_required = None,
        sacl_auto_inherited = None,
        sacl_protected = None,
        rm_control = None
    )?;
    let owner = match owner_value.as_deref() {
        Some(value) => NullableComponent::from_option(coerce_sid_option(
            strand,
            global,
            value,
            &path.key("owner"),
        )?),
        None => NullableComponent::Unspecified,
    };
    let group = match group_value.as_deref() {
        Some(value) => NullableComponent::from_option(coerce_sid_option(
            strand,
            global,
            value,
            &path.key("group"),
        )?),
        None => NullableComponent::Unspecified,
    };
    let dacl = match dacl_value.as_deref() {
        Some(value) => NullableComponent::from_option(
            coerce_acl_option(strand, global, value, &path.key("dacl")).await?,
        ),
        None => NullableComponent::Unspecified,
    };
    let sacl = match sacl_value.as_deref() {
        Some(value) => NullableComponent::from_option(
            coerce_acl_option(strand, global, value, &path.key("sacl")).await?,
        ),
        None => NullableComponent::Unspecified,
    };
    let rm_control = match rm_control_value.as_deref() {
        Some(value) if value.is_nil() => NullableComponent::Clear,
        Some(value) => NullableComponent::Set(ace_u8(strand, value, &path.key("rm_control"))?),
        None => NullableComponent::Unspecified,
    };
    let components = SecDescComponents {
        owner,
        group,
        dacl,
        sacl,
        owner_defaulted: owner_defaulted_value
            .as_deref()
            .map(|value| ace_bool(strand, value, &path.key("owner_defaulted")))
            .transpose()?,
        group_defaulted: group_defaulted_value
            .as_deref()
            .map(|value| ace_bool(strand, value, &path.key("group_defaulted")))
            .transpose()?,
        dacl_present: dacl_present_value
            .as_deref()
            .map(|value| ace_bool(strand, value, &path.key("dacl_present")))
            .transpose()?,
        dacl_defaulted: dacl_defaulted_value
            .as_deref()
            .map(|value| ace_bool(strand, value, &path.key("dacl_defaulted")))
            .transpose()?,
        dacl_auto_inherit_required: dacl_auto_inherit_required_value
            .as_deref()
            .map(|value| ace_bool(strand, value, &path.key("dacl_auto_inherit_required")))
            .transpose()?,
        dacl_auto_inherited: dacl_auto_inherited_value
            .as_deref()
            .map(|value| ace_bool(strand, value, &path.key("dacl_auto_inherited")))
            .transpose()?,
        dacl_protected: dacl_protected_value
            .as_deref()
            .map(|value| ace_bool(strand, value, &path.key("dacl_protected")))
            .transpose()?,
        sacl_present: sacl_present_value
            .as_deref()
            .map(|value| ace_bool(strand, value, &path.key("sacl_present")))
            .transpose()?,
        sacl_defaulted: sacl_defaulted_value
            .as_deref()
            .map(|value| ace_bool(strand, value, &path.key("sacl_defaulted")))
            .transpose()?,
        sacl_auto_inherit_required: sacl_auto_inherit_required_value
            .as_deref()
            .map(|value| ace_bool(strand, value, &path.key("sacl_auto_inherit_required")))
            .transpose()?,
        sacl_auto_inherited: sacl_auto_inherited_value
            .as_deref()
            .map(|value| ace_bool(strand, value, &path.key("sacl_auto_inherited")))
            .transpose()?,
        sacl_protected: sacl_protected_value
            .as_deref()
            .map(|value| ace_bool(strand, value, &path.key("sacl_protected")))
            .transpose()?,
        rm_control,
    };

    let base = match value.as_deref() {
        Some(value) => coerce_sec_desc(strand, global, value, path).await?,
        None if !components.is_empty() => empty_sec_desc(),
        None => {
            return Err(Error::value(
                strand,
                format!("{path}: expected a security descriptor or component options"),
            ));
        }
    };
    if components.is_empty() {
        return Ok(base);
    }
    base.with(components.into_update())
        .map_err(|error| Error::value(strand, format!("{path}: {error}")))
}

impl<'v> Object<'v> for SecDesc {
    const NAME: &'v str = "SecDesc";
    const MODULE: &'v str = "security.windows";
    type Annex = VfsSecDesc;
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = strand.state::<WindowsSecurityGlobal<'v>>();
        let owner = global.syms.owner;
        let group = global.syms.group;
        let dacl = global.syms.dacl;
        let sacl = global.syms.sacl;
        let owner_defaulted = global.syms.owner_defaulted;
        let group_defaulted = global.syms.group_defaulted;
        let dacl_present = global.syms.dacl_present;
        let dacl_defaulted = global.syms.dacl_defaulted;
        let dacl_auto_inherit_required = global.syms.dacl_auto_inherit_required;
        let dacl_auto_inherited = global.syms.dacl_auto_inherited;
        let dacl_protected = global.syms.dacl_protected;
        let sacl_present = global.syms.sacl_present;
        let sacl_defaulted = global.syms.sacl_defaulted;
        let sacl_auto_inherit_required = global.syms.sacl_auto_inherit_required;
        let sacl_auto_inherited = global.syms.sacl_auto_inherited;
        let sacl_protected = global.syms.sacl_protected;
        let rm_control = global.syms.rm_control;
        let (
            [],
            [
                value,
                owner_value,
                group_value,
                dacl_value,
                sacl_value,
                owner_defaulted_value,
                group_defaulted_value,
                dacl_present_value,
                dacl_defaulted_value,
                dacl_auto_inherit_required_value,
                dacl_auto_inherited_value,
                dacl_protected_value,
                sacl_present_value,
                sacl_defaulted_value,
                sacl_auto_inherit_required_value,
                sacl_auto_inherited_value,
                sacl_protected_value,
                rm_control_value,
            ],
        ) = unpack!(
            strand,
            args,
            0,
            1,
            owner = None,
            group = None,
            dacl = None,
            sacl = None,
            owner_defaulted = None,
            group_defaulted = None,
            dacl_present = None,
            dacl_defaulted = None,
            dacl_auto_inherit_required = None,
            dacl_auto_inherited = None,
            dacl_protected = None,
            sacl_present = None,
            sacl_defaulted = None,
            sacl_auto_inherit_required = None,
            sacl_auto_inherited = None,
            sacl_protected = None,
            rm_control = None
        )?;
        let components_supplied = owner_value.is_some()
            || group_value.is_some()
            || dacl_value.is_some()
            || sacl_value.is_some()
            || owner_defaulted_value.is_some()
            || group_defaulted_value.is_some()
            || dacl_present_value.is_some()
            || dacl_defaulted_value.is_some()
            || dacl_auto_inherit_required_value.is_some()
            || dacl_auto_inherited_value.is_some()
            || dacl_protected_value.is_some()
            || sacl_present_value.is_some()
            || sacl_defaulted_value.is_some()
            || sacl_auto_inherit_required_value.is_some()
            || sacl_auto_inherited_value.is_some()
            || sacl_protected_value.is_some()
            || rm_control_value.is_some();
        let descriptor = if let Some(value) = value {
            if components_supplied {
                return Err(Error::value(
                    strand,
                    "SecDesc: packet form does not accept component options",
                ));
            }
            let value = value
                .as_bin(strand)
                .ok_or_else(|| Error::type_error(strand, "SecDesc: expected Bin"))?;
            VfsSecDesc::from_bytes(&value.to_vec())
                .map_err(|error| Error::value(strand, error.to_string()))?
        } else {
            let rm_control = match rm_control_value.as_deref() {
                Some(value) if value.is_nil() => NullableComponent::Clear,
                Some(value) => {
                    NullableComponent::Set(ace_u8(strand, value, &SpecPath::root("rm_control"))?)
                }
                None => NullableComponent::Unspecified,
            };
            let components = SecDescComponents {
                owner: downcast_sid_component(strand, global, owner_value.as_deref(), "owner")?,
                group: downcast_sid_component(strand, global, group_value.as_deref(), "group")?,
                dacl: downcast_acl_component(strand, global, dacl_value.as_deref(), "dacl")?,
                sacl: downcast_acl_component(strand, global, sacl_value.as_deref(), "sacl")?,
                owner_defaulted: parse_bool_component(
                    strand,
                    owner_defaulted_value.as_deref(),
                    "owner_defaulted",
                )?,
                group_defaulted: parse_bool_component(
                    strand,
                    group_defaulted_value.as_deref(),
                    "group_defaulted",
                )?,
                dacl_present: parse_bool_component(
                    strand,
                    dacl_present_value.as_deref(),
                    "dacl_present",
                )?,
                dacl_defaulted: parse_bool_component(
                    strand,
                    dacl_defaulted_value.as_deref(),
                    "dacl_defaulted",
                )?,
                dacl_auto_inherit_required: parse_bool_component(
                    strand,
                    dacl_auto_inherit_required_value.as_deref(),
                    "dacl_auto_inherit_required",
                )?,
                dacl_auto_inherited: parse_bool_component(
                    strand,
                    dacl_auto_inherited_value.as_deref(),
                    "dacl_auto_inherited",
                )?,
                dacl_protected: parse_bool_component(
                    strand,
                    dacl_protected_value.as_deref(),
                    "dacl_protected",
                )?,
                sacl_present: parse_bool_component(
                    strand,
                    sacl_present_value.as_deref(),
                    "sacl_present",
                )?,
                sacl_defaulted: parse_bool_component(
                    strand,
                    sacl_defaulted_value.as_deref(),
                    "sacl_defaulted",
                )?,
                sacl_auto_inherit_required: parse_bool_component(
                    strand,
                    sacl_auto_inherit_required_value.as_deref(),
                    "sacl_auto_inherit_required",
                )?,
                sacl_auto_inherited: parse_bool_component(
                    strand,
                    sacl_auto_inherited_value.as_deref(),
                    "sacl_auto_inherited",
                )?,
                sacl_protected: parse_bool_component(
                    strand,
                    sacl_protected_value.as_deref(),
                    "sacl_protected",
                )?,
                rm_control,
            };
            empty_sec_desc()
                .with(components.into_update())
                .map_err(|error| Error::value(strand, error.to_string()))?
        };
        this.create_with_annex(strand, SecDesc, descriptor, out);
        Ok(())
    }

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        fn control_field<'v, 's>(
            _this: Instance<'v, '_, SecDesc>,
            strand: &mut Strand<'v, 's>,
            out: impl Output<'v>,
            field: Sym<'v, '_>,
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
                Output::set(strand, out, this.annex().revision() as u8);
                Ok(())
            })
            .get("control", |this, strand, out| {
                let global = strand.state::<WindowsSecurityGlobal<'v>>();
                global.types.sec_desc_control.create_flags(
                    strand,
                    SecDescControl(this.annex().control()),
                    out,
                );
                Ok(())
            })
            .get("mask", |this, strand, out| {
                let global = strand.state::<WindowsSecurityGlobal<'v>>();
                global
                    .types
                    .sec_info
                    .create_flags(strand, SecInfo(this.annex().mask()), out);
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
                let global = strand.state::<WindowsSecurityGlobal<'v>>();
                create_sid(strand, global, value.clone(), &mut out);
                Ok(())
            })
            .get("group", move |this, strand, mut out| {
                let descriptor = this.annex();
                let Some(value) = descriptor.group().filter(|_| descriptor.group_loaded()) else {
                    return Err(Error::field(strand, group));
                };
                let global = strand.state::<WindowsSecurityGlobal<'v>>();
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
                    let global = strand.state::<WindowsSecurityGlobal<'v>>();
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
                    let global = strand.state::<WindowsSecurityGlobal<'v>>();
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
            .method("with", async move |this, strand, args, out| {
                let (
                    [],
                    [
                        owner_value,
                        group_value,
                        dacl_value,
                        sacl_value,
                        owner_defaulted_value,
                        group_defaulted_value,
                        dacl_present_value,
                        dacl_defaulted_value,
                        dacl_auto_inherit_required_value,
                        dacl_auto_inherited_value,
                        dacl_protected_value,
                        sacl_present_value,
                        sacl_defaulted_value,
                        sacl_auto_inherit_required_value,
                        sacl_auto_inherited_value,
                        sacl_protected_value,
                        rm_control_value,
                    ],
                ) = unpack!(
                    strand,
                    args,
                    0,
                    0,
                    owner = None,
                    group = None,
                    dacl = None,
                    sacl = None,
                    owner_defaulted = None,
                    group_defaulted = None,
                    dacl_present = None,
                    dacl_defaulted = None,
                    dacl_auto_inherit_required = None,
                    dacl_auto_inherited = None,
                    dacl_protected = None,
                    sacl_present = None,
                    sacl_defaulted = None,
                    sacl_auto_inherit_required = None,
                    sacl_auto_inherited = None,
                    sacl_protected = None,
                    rm_control = None
                )?;
                let global = strand.state::<WindowsSecurityGlobal<'v>>();
                let rm_control = match rm_control_value.as_deref() {
                    Some(value) if value.is_nil() => NullableComponent::Clear,
                    Some(value) => NullableComponent::Set(ace_u8(
                        strand,
                        value,
                        &SpecPath::root("rm_control"),
                    )?),
                    None => NullableComponent::Unspecified,
                };
                let components = SecDescComponents {
                    owner: downcast_sid_component(strand, global, owner_value.as_deref(), "owner")?,
                    group: downcast_sid_component(strand, global, group_value.as_deref(), "group")?,
                    dacl: downcast_acl_component(strand, global, dacl_value.as_deref(), "dacl")?,
                    sacl: downcast_acl_component(strand, global, sacl_value.as_deref(), "sacl")?,
                    owner_defaulted: parse_bool_component(
                        strand,
                        owner_defaulted_value.as_deref(),
                        "owner_defaulted",
                    )?,
                    group_defaulted: parse_bool_component(
                        strand,
                        group_defaulted_value.as_deref(),
                        "group_defaulted",
                    )?,
                    dacl_present: parse_bool_component(
                        strand,
                        dacl_present_value.as_deref(),
                        "dacl_present",
                    )?,
                    dacl_defaulted: parse_bool_component(
                        strand,
                        dacl_defaulted_value.as_deref(),
                        "dacl_defaulted",
                    )?,
                    dacl_auto_inherit_required: parse_bool_component(
                        strand,
                        dacl_auto_inherit_required_value.as_deref(),
                        "dacl_auto_inherit_required",
                    )?,
                    dacl_auto_inherited: parse_bool_component(
                        strand,
                        dacl_auto_inherited_value.as_deref(),
                        "dacl_auto_inherited",
                    )?,
                    dacl_protected: parse_bool_component(
                        strand,
                        dacl_protected_value.as_deref(),
                        "dacl_protected",
                    )?,
                    sacl_present: parse_bool_component(
                        strand,
                        sacl_present_value.as_deref(),
                        "sacl_present",
                    )?,
                    sacl_defaulted: parse_bool_component(
                        strand,
                        sacl_defaulted_value.as_deref(),
                        "sacl_defaulted",
                    )?,
                    sacl_auto_inherit_required: parse_bool_component(
                        strand,
                        sacl_auto_inherit_required_value.as_deref(),
                        "sacl_auto_inherit_required",
                    )?,
                    sacl_auto_inherited: parse_bool_component(
                        strand,
                        sacl_auto_inherited_value.as_deref(),
                        "sacl_auto_inherited",
                    )?,
                    sacl_protected: parse_bool_component(
                        strand,
                        sacl_protected_value.as_deref(),
                        "sacl_protected",
                    )?,
                    rm_control,
                };
                let descriptor = this
                    .annex()
                    .with(components.into_update())
                    .map_err(|error| Error::value(strand, error.to_string()))?;
                global
                    .types
                    .sec_desc
                    .create_with_annex(strand, SecDesc, descriptor, out);
                Ok(())
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

pub(crate) fn create_sid_name<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, WindowsSecurityGlobal<'v>>,
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
    const MODULE: &'v str = "security.windows";
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
                let global = strand.state::<WindowsSecurityGlobal<'v>>();
                create_sid(strand, global, this.annex().sid().clone(), &mut out);
                Ok(())
            })
            .get("name", |this, strand, out| {
                Output::set(strand, out, this.annex().name());
                Ok(())
            })
            .get("domain", |this, strand, out| {
                Output::set(strand, out, this.annex().domain());
                Ok(())
            })
            .get("qualified_name", |this, strand, out| {
                if this.annex().domain().is_empty() {
                    Output::set(strand, out, this.annex().name());
                } else {
                    let name = format!("{}\\{}", this.annex().domain(), this.annex().name());
                    Output::set(strand, out, name.as_str());
                }
                Ok(())
            })
            .get("kind", move |this, strand, out| {
                let kind = match this.annex().kind() {
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
                    _ => unknown,
                };
                Output::set(strand, out, kind);
                Ok(())
            })
            .type_method("lookup", async move |_this, strand, args, mut out| {
                let ([value], []) = unpack!(strand, args, 1, 0)?;
                let global = strand.state::<WindowsSecurityGlobal<'v>>();
                if global.local.get(strand).target().os().family() != OperatingSystemFamily::Windows
                {
                    return Err(Error::not_supported(strand));
                }
                let vfs = global.local.get(strand).vfs();
                // A `Str` is the account name to resolve, so only the
                // unambiguous SID spellings are coerced here.
                let path = SpecPath::root("SidName.lookup");
                let sid = coerce_sid_non_str(strand, global, &value, &path)?;
                let name = if let Some(sid) = sid {
                    error::io_result(strand, vfs.sid_name(&sid).await)?
                } else if let Some(value) = value.as_str(strand) {
                    let value = value.to_string();
                    error::io_result(strand, vfs.account_name(&value).await)?
                } else {
                    return Err(Error::type_error(
                        strand,
                        "SidName.lookup: expected Sid, Str, Sym, or Bin",
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
    const MODULE: &'v str = "security.windows";
    const SLOTS: usize = 1;
    type Annex = VfsTokenGroup;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        fn flag<'v, 's>(
            this: Instance<'v, '_, TokenGroup>,
            strand: &mut Strand<'v, 's>,
            out: impl Output<'v>,
            mask: WinTokenGroupAttributes,
        ) -> Result<'v, 's, ()> {
            Output::set(strand, out, this.annex().attributes().contains(mask));
            Ok(())
        }

        builder
            .get("sid", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                Output::set(strand, out, Ref::slot::<0>(&borrow));
                Ok(())
            })
            .get("attributes", |this, strand, out| {
                let global = strand.state::<WindowsSecurityGlobal<'v>>();
                global.types.token_group_attributes.create_flags(
                    strand,
                    TokenGroupAttributes(this.annex().attributes()),
                    out,
                );
                Ok(())
            })
            .get("mandatory", |this, strand, out| {
                flag(this, strand, out, WinTokenGroupAttributes::MANDATORY)
            })
            .get("enabled_by_default", |this, strand, out| {
                flag(
                    this,
                    strand,
                    out,
                    WinTokenGroupAttributes::ENABLED_BY_DEFAULT,
                )
            })
            .get("enabled", |this, strand, out| {
                flag(this, strand, out, WinTokenGroupAttributes::ENABLED)
            })
            .get("owner", |this, strand, out| {
                flag(this, strand, out, WinTokenGroupAttributes::OWNER)
            })
            .get("use_for_deny_only", |this, strand, out| {
                flag(
                    this,
                    strand,
                    out,
                    WinTokenGroupAttributes::USE_FOR_DENY_ONLY,
                )
            })
            .get("integrity", |this, strand, out| {
                flag(this, strand, out, WinTokenGroupAttributes::INTEGRITY)
            })
            .get("integrity_enabled", |this, strand, out| {
                flag(
                    this,
                    strand,
                    out,
                    WinTokenGroupAttributes::INTEGRITY_ENABLED,
                )
            })
            .get("resource", |this, strand, out| {
                flag(this, strand, out, WinTokenGroupAttributes::RESOURCE)
            })
            .get("logon_id", |this, strand, out| {
                Output::set(
                    strand,
                    out,
                    this.annex()
                        .attributes()
                        .contains(WinTokenGroupAttributes::LOGON_ID),
                );
                Ok(())
            })
    }
}

pub(crate) struct TokenInfo;

struct TokenGroups;

impl<'v> ArrayLike<'v> for TokenGroups {
    type Object = TokenInfo;

    const MODULE: &'v str = "security.windows";
    const NAME: &'v str = "TokenGroups";

    fn len(&self, this: Instance<'v, '_, Self::Object>, _strand: &mut Strand<'v, '_>) -> usize {
        this.annex().groups().len()
    }

    fn get<'a, 's>(
        &self,
        this: Instance<'v, '_, Self::Object>,
        strand: &'a mut Strand<'v, 's>,
        index: usize,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let token_group = this
            .annex()
            .groups()
            .get(index)
            .expect("array view index was normalized")
            .clone();
        let global = strand.state::<WindowsSecurityGlobal<'v>>();
        strand.with_slots_sync(|strand, [mut sid]| {
            create_sid(strand, global, token_group.sid().clone(), &mut sid);
            global
                .types
                .token_group
                .create_with_annex(strand, TokenGroup, token_group, &mut out);
            global
                .types
                .token_group
                .cast(&out)
                .unwrap()
                .enter_sync(strand, |strand, group| {
                    Output::set(
                        strand,
                        Mut::slot_mut::<0>(&mut group.borrow_mut_unwrap()),
                        &sid,
                    );
                });
            Ok(())
        })
    }
}

impl<'v> Object<'v> for TokenInfo {
    const NAME: &'v str = "TokenInfo";
    const MODULE: &'v str = "security.windows";
    const SLOTS: usize = 4;
    type Annex = WindowsTokenInfo;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("is_elevated", |this, strand, out| {
                Output::set(strand, out, this.annex().is_elevated());
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
                Output::set(strand, out, ArrayView::new(this, TokenGroups));
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

/// Builds a `security.windows.TokenInfo` around `info`.
///
/// The three mandatory SIDs and the optional logon SID become `Sid` objects
/// rooted in slots 0-3; `groups` is projected from the annex on demand, so it
/// needs no slot of its own.
pub(crate) fn create_token_info<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, WindowsSecurityGlobal<'v>>,
    info: &WindowsTokenInfo,
    out: &mut Slot<'v, '_>,
) {
    strand.with_slots_sync(|strand, [mut sid]| {
        global
            .types
            .token_info
            .create_with_annex(strand, TokenInfo, info.clone(), &mut *out);
        global
            .types
            .token_info
            .cast(out)
            .unwrap()
            .enter_sync(strand, |strand, this| {
                for (index, value) in [
                    (0, info.user_sid().clone()),
                    (1, info.owner_sid().clone()),
                    (2, info.primary_group_sid().clone()),
                ] {
                    create_sid(strand, global, value, &mut sid);
                    let mut borrow = this.borrow_mut_unwrap();
                    match index {
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
            });
    });
}

pub(crate) fn configure_vm<'v>(
    builder: &mut Register<'v>,
    global: State<'v, WindowsSecurityGlobal<'v>>,
) {
    builder
        .module("security.windows")
        .value("AccessMask", global.types.access_mask)
        .value("AceFlags", global.types.ace_flags)
        .value("SecDescControl", global.types.sec_desc_control)
        .value("SecInfo", global.types.sec_info)
        .value("TokenGroupAttributes", global.types.token_group_attributes)
        .value("Acl", global.types.acl)
        .value("Ace", global.types.ace)
        .value("SecDesc", global.types.sec_desc)
        .value("Sid", global.types.sid)
        .value("SidName", global.types.sid_name)
        .value("TokenGroup", global.types.token_group)
        .value("TokenInfo", global.types.token_info)
        .function_with_slots("ace", async move |strand, args, mut out, [mut slot]| {
            let ace = ace_from_args(strand, global, args, &SpecPath::root("ace")).await?;
            global
                .types
                .ace
                .create_with_annex(strand, Ace, AceAnnex::Owned(ace), &mut slot);
            Output::set(strand, &mut out, &slot);
            Ok(())
        })
        .function_with_slots("acl", async move |strand, args, mut out, [mut slot]| {
            let acl = acl_from_args(strand, global, args, &SpecPath::root("acl")).await?;
            global
                .types
                .acl
                .create_with_annex(strand, Acl, AclAnnex::Owned(acl), &mut slot);
            Output::set(strand, &mut out, &slot);
            Ok(())
        })
        .function_with_slots(
            "sec_desc",
            async move |strand, args, mut out, [mut slot]| {
                let descriptor =
                    sec_desc_from_args(strand, global, args, &SpecPath::root("sec_desc")).await?;
                create_sec_desc(strand, global, descriptor, &mut slot);
                Output::set(strand, &mut out, &slot);
                Ok(())
            },
        )
        .function("token_info", async move |strand, args, mut out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            let security = security_info(strand, global.local)?;
            let Some(info) = security.windows() else {
                return Err(Error::not_supported(strand));
            };
            create_token_info(strand, global, info, &mut out);
            Ok(())
        })
        .commit();
}
