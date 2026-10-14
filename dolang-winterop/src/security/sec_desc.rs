use std::{
    borrow::Borrow,
    error, fmt,
    hash::{Hash, Hasher},
    ops::Deref,
};

use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, SeqAccess, Visitor},
    ser::SerializeTuple,
};

use super::{access_mask::AccessMask, sid::Sid};
use crate::guid::Guid;

const REVISION: u8 = 1;

/// A component of a Windows security descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecDescComponent {
    /// The owner SID.
    Owner,
    /// The primary-group SID.
    Group,
    /// The discretionary ACL.
    Dacl,
    /// The system ACL.
    Sacl,
}

impl fmt::Display for SecDescComponent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Owner => f.write_str("owner"),
            Self::Group => f.write_str("group"),
            Self::Dacl => f.write_str("DACL"),
            Self::Sacl => f.write_str("SACL"),
        }
    }
}

/// A security descriptor's access-control-list component.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AclKind {
    /// The discretionary ACL.
    Dacl,
    /// The system ACL.
    Sacl,
}

impl AclKind {
    const fn component(self) -> SecDescComponent {
        match self {
            Self::Dacl => SecDescComponent::Dacl,
            Self::Sacl => SecDescComponent::Sacl,
        }
    }
}

impl fmt::Display for AclKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.component().fmt(f)
    }
}

bitflags::bitflags! {
    /// Security-descriptor components selected by a query or represented by a [`SecDesc`].
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub struct SecInfo: u32 {
        /// Selects the owner SID.
        const OWNER = 0x0000_0001;
        /// Selects the primary group SID.
        const GROUP = 0x0000_0002;
        /// Selects the discretionary ACL.
        const DACL = 0x0000_0004;
        /// Selects the system ACL.
        const SACL = 0x0000_0008;
        /// Selects every supported security-descriptor component.
        const ALL = Self::OWNER.bits() | Self::GROUP.bits() | Self::DACL.bits() | Self::SACL.bits();
    }
}

bitflags::bitflags! {
    /// Flags describing the state and storage of a security descriptor.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub struct SecDescControl: u16 {
        const OWNER_DEFAULTED = 0x0001;
        const GROUP_DEFAULTED = 0x0002;
        const DACL_PRESENT = 0x0004;
        const DACL_DEFAULTED = 0x0008;
        const SACL_PRESENT = 0x0010;
        const SACL_DEFAULTED = 0x0020;
        const DACL_AUTO_INHERIT_REQUIRED = 0x0100;
        const SACL_AUTO_INHERIT_REQUIRED = 0x0200;
        const DACL_AUTO_INHERITED = 0x0400;
        const SACL_AUTO_INHERITED = 0x0800;
        const DACL_PROTECTED = 0x1000;
        const SACL_PROTECTED = 0x2000;
        const RM_CONTROL_VALID = 0x4000;
        const SELF_RELATIVE = 0x8000;
    }
}

bitflags::bitflags! {
    /// Flags stored in an ACE header.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub struct AceFlags: u8 {
        const OBJECT_INHERIT = 0x01;
        const CONTAINER_INHERIT = 0x02;
        const NO_PROPAGATE_INHERIT = 0x04;
        const INHERIT_ONLY = 0x08;
        const INHERITED = 0x10;
        const CRITICAL = 0x20;
        const SUCCESSFUL_ACCESS = 0x40;
        const TRUST_PROTECTED_FILTER = 0x40;
        const FAILED_ACCESS = 0x80;
    }
}

bitflags::bitflags! {
    /// Object-specific fields present in an object ACE.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub struct ObjectAceFlags: u32 {
        const OBJECT_TYPE_PRESENT = 0x1;
        const INHERITED_OBJECT_TYPE_PRESENT = 0x2;
    }
}

/// Native ACL packet revision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AclRevision {
    Basic,
    DirectoryService,
    Unknown(u8),
}

impl From<u8> for AclRevision {
    fn from(value: u8) -> Self {
        match value {
            2 => Self::Basic,
            4 => Self::DirectoryService,
            value => Self::Unknown(value),
        }
    }
}

impl From<AclRevision> for u8 {
    fn from(value: AclRevision) -> Self {
        match value {
            AclRevision::Basic => 2,
            AclRevision::DirectoryService => 4,
            AclRevision::Unknown(value) => value,
        }
    }
}

impl Serialize for AclRevision {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8((*self).into())
    }
}

impl<'de> Deserialize<'de> for AclRevision {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(u8::deserialize(deserializer)?.into())
    }
}

const ACL_HEADER_LEN: usize = 8;
const ACE_HEADER_LEN: usize = 4;
const SELF_RELATIVE_HEADER_LEN: usize = 20;

/// Security descriptor format revision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum SecDescRevision {
    /// Revision 1, the format supported by current security descriptors.
    One = 1,
}

impl Serialize for SecDescRevision {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u8(*self as u8)
    }
}

impl<'de> Deserialize<'de> for SecDescRevision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match u8::deserialize(deserializer)? {
            REVISION => Ok(Self::One),
            revision => Err(de::Error::custom(SecDescError::Revision(revision))),
        }
    }
}

type Revision = SecDescRevision;

/// An immutable borrowed native Windows access-control list (ACL).
#[repr(transparent)]
pub struct Acl([u8]);

impl Acl {
    /// Parses and validates a complete native ACL packet.
    pub fn from_bytes(bytes: &[u8]) -> Result<&Self, AclError> {
        if bytes.len() < ACL_HEADER_LEN || !bytes.len().is_multiple_of(4) {
            return Err(AclError::Length(bytes.len()));
        }
        let declared = u16::from_le_bytes(bytes[2..4].try_into().unwrap());
        if usize::from(declared) != bytes.len() {
            return Err(AclError::Size(declared, bytes.len()));
        }
        let count = u16::from_le_bytes(bytes[4..6].try_into().unwrap());
        let mut offset = ACL_HEADER_LEN;
        for index in 0..usize::from(count) {
            let header = bytes
                .get(offset..offset + ACE_HEADER_LEN)
                .ok_or(AclError::AceCount(count, index))?;
            let size = usize::from(u16::from_le_bytes(header[2..4].try_into().unwrap()));
            let ace = bytes
                .get(offset..offset.saturating_add(size))
                .ok_or(AclError::Ace(index, AceError::Bounds(size)))?;
            Ace::from_bytes(ace).map_err(|error| AclError::Ace(index, error))?;
            offset += size;
        }
        // SAFETY: Acl is transparent over [u8], and the packet was validated above.
        Ok(unsafe { &*(bytes as *const [u8] as *const Self) })
    }

    /// Returns the exact native ACL packet.
    pub const fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Returns the ACL revision.
    pub const fn revision(&self) -> AclRevision {
        match self.0[0] {
            2 => AclRevision::Basic,
            4 => AclRevision::DirectoryService,
            value => AclRevision::Unknown(value),
        }
    }

    /// Returns the declared ACL size.
    pub fn size(&self) -> u16 {
        u16::from_le_bytes(self.0[2..4].try_into().unwrap())
    }

    /// Returns the number of ACEs declared by the ACL.
    pub fn ace_count(&self) -> u16 {
        u16::from_le_bytes(self.0[4..6].try_into().unwrap())
    }

    /// Iterates over the validated ACE packets.
    pub fn aces(&self) -> Aces<'_> {
        Aces {
            bytes: &self.0,
            offset: ACL_HEADER_LEN,
            remaining: usize::from(self.ace_count()),
        }
    }
}

impl fmt::Debug for Acl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Acl").field(&&self.0).finish()
    }
}

impl PartialEq for Acl {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for Acl {}

impl Hash for Acl {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl AsRef<Acl> for Acl {
    fn as_ref(&self) -> &Acl {
        self
    }
}

/// Iterator over the ACEs in an [`Acl`].
#[derive(Clone, Debug)]
pub struct Aces<'a> {
    bytes: &'a [u8],
    offset: usize,
    remaining: usize,
}

impl<'a> Iterator for Aces<'a> {
    type Item = &'a Ace;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let size = usize::from(u16::from_le_bytes(
            self.bytes[self.offset + 2..self.offset + 4]
                .try_into()
                .unwrap(),
        ));
        let bytes = &self.bytes[self.offset..self.offset + size];
        self.offset += size;
        self.remaining -= 1;
        // SAFETY: the containing ACL validated every ACE packet.
        Some(unsafe { &*(bytes as *const [u8] as *const Ace) })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for Aces<'_> {}

/// A classified native ACE type.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AceType {
    /// Grants access.
    AccessAllowed,
    /// Denies access.
    AccessDenied,
    /// Emits an audit event.
    SystemAudit,
    /// Raises a system alarm.
    SystemAlarm,
    /// A compound access-allowed ACE.
    AccessAllowedCompound,
    /// An object-specific access-allowed ACE.
    AccessAllowedObject,
    /// An object-specific access-denied ACE.
    AccessDeniedObject,
    /// An object-specific system-audit ACE.
    SystemAuditObject,
    /// An object-specific system-alarm ACE.
    SystemAlarmObject,
    /// A callback access-allowed ACE.
    AccessAllowedCallback,
    /// A callback access-denied ACE.
    AccessDeniedCallback,
    /// A callback object-specific access-allowed ACE.
    AccessAllowedCallbackObject,
    /// A callback object-specific access-denied ACE.
    AccessDeniedCallbackObject,
    /// A callback system-audit ACE.
    SystemAuditCallback,
    /// A callback system-alarm ACE.
    SystemAlarmCallback,
    /// A callback object-specific system-audit ACE.
    SystemAuditCallbackObject,
    /// A callback object-specific system-alarm ACE.
    SystemAlarmCallbackObject,
    /// A mandatory-integrity-label ACE.
    SystemMandatoryLabel,
    /// A resource-attribute ACE.
    SystemResourceAttribute,
    /// A scoped-policy-ID ACE.
    SystemScopedPolicyId,
    /// A process-trust-label ACE.
    SystemProcessTrustLabel,
    /// An access-filter ACE.
    SystemAccessFilter,
    /// An unrecognized native ACE type code.
    Unknown(u8),
}

impl From<u8> for AceType {
    fn from(code: u8) -> Self {
        match code {
            0 => Self::AccessAllowed,
            1 => Self::AccessDenied,
            2 => Self::SystemAudit,
            3 => Self::SystemAlarm,
            4 => Self::AccessAllowedCompound,
            5 => Self::AccessAllowedObject,
            6 => Self::AccessDeniedObject,
            7 => Self::SystemAuditObject,
            8 => Self::SystemAlarmObject,
            9 => Self::AccessAllowedCallback,
            10 => Self::AccessDeniedCallback,
            11 => Self::AccessAllowedCallbackObject,
            12 => Self::AccessDeniedCallbackObject,
            13 => Self::SystemAuditCallback,
            14 => Self::SystemAlarmCallback,
            15 => Self::SystemAuditCallbackObject,
            16 => Self::SystemAlarmCallbackObject,
            17 => Self::SystemMandatoryLabel,
            18 => Self::SystemResourceAttribute,
            19 => Self::SystemScopedPolicyId,
            20 => Self::SystemProcessTrustLabel,
            21 => Self::SystemAccessFilter,
            code => Self::Unknown(code),
        }
    }
}

impl From<AceType> for u8 {
    fn from(value: AceType) -> Self {
        match value {
            AceType::AccessAllowed => 0,
            AceType::AccessDenied => 1,
            AceType::SystemAudit => 2,
            AceType::SystemAlarm => 3,
            AceType::AccessAllowedCompound => 4,
            AceType::AccessAllowedObject => 5,
            AceType::AccessDeniedObject => 6,
            AceType::SystemAuditObject => 7,
            AceType::SystemAlarmObject => 8,
            AceType::AccessAllowedCallback => 9,
            AceType::AccessDeniedCallback => 10,
            AceType::AccessAllowedCallbackObject => 11,
            AceType::AccessDeniedCallbackObject => 12,
            AceType::SystemAuditCallback => 13,
            AceType::SystemAlarmCallback => 14,
            AceType::SystemAuditCallbackObject => 15,
            AceType::SystemAlarmCallbackObject => 16,
            AceType::SystemMandatoryLabel => 17,
            AceType::SystemResourceAttribute => 18,
            AceType::SystemScopedPolicyId => 19,
            AceType::SystemProcessTrustLabel => 20,
            AceType::SystemAccessFilter => 21,
            AceType::Unknown(code) => code,
        }
    }
}

impl Serialize for AceType {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8((*self).into())
    }
}

impl<'de> Deserialize<'de> for AceType {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(u8::deserialize(deserializer)?.into())
    }
}

/// An immutable borrowed native Windows access-control entry (ACE).
#[repr(transparent)]
pub struct Ace([u8]);

#[derive(Debug)]
struct AceBody {
    mask: AccessMask,
    sid: Sid,
    object_flags: Option<ObjectAceFlags>,
    object_type: Option<Guid>,
    inherited_object_type: Option<Guid>,
    application_data_at: usize,
}

impl Ace {
    /// Parses and validates one complete native ACE packet.
    pub fn from_bytes(bytes: &[u8]) -> Result<&Self, AceError> {
        if bytes.len() < ACE_HEADER_LEN {
            return Err(AceError::Length(bytes.len()));
        }
        let declared = u16::from_le_bytes(bytes[2..4].try_into().unwrap());
        if usize::from(declared) != bytes.len() {
            return Err(AceError::Size(declared, bytes.len()));
        }
        if !bytes.len().is_multiple_of(4) {
            return Err(AceError::Alignment(bytes.len()));
        }
        // SAFETY: Ace is transparent over [u8]. Validation below only reads it.
        let this = unsafe { &*(bytes as *const [u8] as *const Self) };
        if this.has_simple_body() {
            this.parse_simple_body()?;
        } else if this.has_object_body() {
            this.parse_object_body()?;
        }
        Ok(this)
    }

    /// Returns the exact native ACE packet.
    pub const fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Returns the native ACE type code.
    pub const fn type_code(&self) -> u8 {
        self.0[0]
    }

    /// Returns the classified ACE type.
    pub fn ace_type(&self) -> AceType {
        self.type_code().into()
    }

    /// Returns the native ACE flags byte.
    pub const fn flags(&self) -> AceFlags {
        AceFlags::from_bits_retain(self.0[1])
    }

    /// Returns the declared ACE size.
    pub fn size(&self) -> u16 {
        u16::from_le_bytes(self.0[2..4].try_into().unwrap())
    }

    /// Returns the access mask for ACE layouts that contain one.
    pub fn mask(&self) -> Option<AccessMask> {
        self.body().map(|body| body.mask)
    }

    /// Returns the trustee SID for ACE layouts that contain one.
    pub fn sid(&self) -> Option<Sid> {
        self.body().map(|body| body.sid)
    }

    /// Returns object-specific flags for object ACE layouts.
    pub fn object_flags(&self) -> Option<ObjectAceFlags> {
        self.parse_object_body()
            .ok()
            .map(|body| body.object_flags.unwrap())
    }

    /// Returns the optional object-type GUID for object ACE layouts.
    pub fn object_type(&self) -> Option<Guid> {
        self.parse_object_body().ok()?.object_type
    }

    /// Returns the optional inherited-object-type GUID for object ACE layouts.
    pub fn inherited_object_type(&self) -> Option<Guid> {
        self.parse_object_body().ok()?.inherited_object_type
    }

    /// Returns trailing application data for parsed SID-bearing layouts.
    pub fn application_data(&self) -> Option<&[u8]> {
        self.body().map(|body| &self.0[body.application_data_at..])
    }

    const fn has_simple_body(&self) -> bool {
        matches!(self.type_code(), 0..=3 | 9..=10 | 13..=14 | 17..=21)
    }

    const fn has_object_body(&self) -> bool {
        matches!(self.type_code(), 5..=8 | 11..=12 | 15..=16)
    }

    fn body(&self) -> Option<AceBody> {
        if self.has_simple_body() {
            self.parse_simple_body().ok()
        } else if self.has_object_body() {
            self.parse_object_body().ok()
        } else {
            None
        }
    }

    fn parse_simple_body(&self) -> Result<AceBody, AceError> {
        let mask = AccessMask::from_bits_retain(read_u32(&self.0, 4)?);
        let (sid, application_data_at) = parse_ace_sid(&self.0, 8)?;
        Ok(AceBody {
            mask,
            sid,
            object_flags: None,
            object_type: None,
            inherited_object_type: None,
            application_data_at,
        })
    }

    fn parse_object_body(&self) -> Result<AceBody, AceError> {
        let mask = AccessMask::from_bits_retain(read_u32(&self.0, 4)?);
        let object_flags = ObjectAceFlags::from_bits_retain(read_u32(&self.0, 8)?);
        let mut offset = 12;
        let object_type = if object_flags.contains(ObjectAceFlags::OBJECT_TYPE_PRESENT) {
            let value = Guid::from_bytes(self.0.get(offset..offset + 16).ok_or(AceError::Body)?)
                .map_err(|_| AceError::Body)?;
            offset += 16;
            Some(value)
        } else {
            None
        };
        let inherited_object_type = if object_flags
            .contains(ObjectAceFlags::INHERITED_OBJECT_TYPE_PRESENT)
        {
            let value = Guid::from_bytes(self.0.get(offset..offset + 16).ok_or(AceError::Body)?)
                .map_err(|_| AceError::Body)?;
            offset += 16;
            Some(value)
        } else {
            None
        };
        let (sid, application_data_at) = parse_ace_sid(&self.0, offset)?;
        Ok(AceBody {
            mask,
            sid,
            object_flags: Some(object_flags),
            object_type,
            inherited_object_type,
            application_data_at,
        })
    }
}

impl fmt::Debug for Ace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Ace").field(&&self.0).finish()
    }
}

impl PartialEq for Ace {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for Ace {}

impl Hash for Ace {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl AsRef<Ace> for Ace {
    fn as_ref(&self) -> &Ace {
        self
    }
}

/// Options shared by the supported ACE builders.
///
/// Construct options with [`new`](Self::new) and its fluent setters. The
/// fields are kept private so packet-layout choices remain explicit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AceBuildOptions {
    flags: AceFlags,
    object_type: Option<Guid>,
    inherited_object_type: Option<Guid>,
    callback: bool,
    object: bool,
    application_data: Vec<u8>,
}

impl AceBuildOptions {
    /// Creates options for a plain, non-callback ACE.
    #[allow(clippy::new_without_default)]
    pub const fn new() -> Self {
        Self {
            flags: AceFlags::empty(),
            object_type: None,
            inherited_object_type: None,
            callback: false,
            object: false,
            application_data: Vec::new(),
        }
    }

    /// Replaces the native ACE flags.
    pub fn flags(mut self, flags: AceFlags) -> Self {
        self.flags = flags;
        self
    }

    /// Adds an object-type GUID, selecting an object ACE layout.
    pub fn object_type(mut self, object_type: Guid) -> Self {
        self.object_type = Some(object_type);
        self
    }

    /// Adds an inherited-object-type GUID, selecting an object ACE layout.
    pub fn inherited_object_type(mut self, inherited_object_type: Guid) -> Self {
        self.inherited_object_type = Some(inherited_object_type);
        self
    }

    /// Selects an object ACE layout, even without object GUIDs.
    pub fn object(mut self) -> Self {
        self.object = true;
        self
    }

    /// Selects a callback ACE layout.
    pub fn callback(mut self) -> Self {
        self.callback = true;
        self
    }

    /// Appends opaque application data after the trustee SID.
    pub fn application_data(mut self, application_data: impl Into<Vec<u8>>) -> Self {
        self.application_data = application_data.into();
        self
    }
}

/// An owned, validated native Windows ACE packet.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AceBuf(Box<[u8]>);

impl AceBuf {
    /// Takes ownership of a raw packet after validating it.
    pub fn try_from_bytes(bytes: impl Into<Box<[u8]>>) -> Result<Self, AceError> {
        let bytes = bytes.into();
        Ace::from_bytes(&bytes)?;
        Ok(Self(bytes))
    }

    /// Builds an access-allowed ACE.
    pub fn allow(
        sid: &Sid,
        mask: AccessMask,
        options: AceBuildOptions,
    ) -> Result<Self, AceBuildError> {
        Self::build(AceFamily::Allow, sid, mask, false, false, options)
    }

    /// Builds an access-denied ACE.
    pub fn deny(
        sid: &Sid,
        mask: AccessMask,
        options: AceBuildOptions,
    ) -> Result<Self, AceBuildError> {
        Self::build(AceFamily::Deny, sid, mask, false, false, options)
    }

    /// Builds a system-audit ACE.
    pub fn audit(
        sid: &Sid,
        mask: AccessMask,
        successful: bool,
        failed: bool,
        options: AceBuildOptions,
    ) -> Result<Self, AceBuildError> {
        if !successful && !failed {
            return Err(AceBuildError::AuditOutcome);
        }
        if options
            .flags
            .intersects(AceFlags::SUCCESSFUL_ACCESS | AceFlags::FAILED_ACCESS)
        {
            return Err(AceBuildError::AuditFlags);
        }
        Self::build(AceFamily::Audit, sid, mask, successful, failed, options)
    }

    /// Builds a system-alarm ACE with the same outcome rules as an audit ACE.
    pub fn alarm(
        sid: &Sid,
        mask: AccessMask,
        successful: bool,
        failed: bool,
        options: AceBuildOptions,
    ) -> Result<Self, AceBuildError> {
        if !successful && !failed {
            return Err(AceBuildError::AuditOutcome);
        }
        if options
            .flags
            .intersects(AceFlags::SUCCESSFUL_ACCESS | AceFlags::FAILED_ACCESS)
        {
            return Err(AceBuildError::AuditFlags);
        }
        Self::build(AceFamily::Alarm, sid, mask, successful, failed, options)
    }

    fn build(
        family: AceFamily,
        sid: &Sid,
        mask: AccessMask,
        successful: bool,
        failed: bool,
        options: AceBuildOptions,
    ) -> Result<Self, AceBuildError> {
        let object = options.object
            || options.object_type.is_some()
            || options.inherited_object_type.is_some();
        let type_code = match (family, options.callback, object) {
            (AceFamily::Allow, false, false) => 0,
            (AceFamily::Deny, false, false) => 1,
            (AceFamily::Audit, false, false) => 2,
            (AceFamily::Alarm, false, false) => 3,
            (AceFamily::Allow, false, true) => 5,
            (AceFamily::Deny, false, true) => 6,
            (AceFamily::Audit, false, true) => 7,
            (AceFamily::Alarm, false, true) => 8,
            (AceFamily::Allow, true, false) => 9,
            (AceFamily::Deny, true, false) => 10,
            (AceFamily::Allow, true, true) => 11,
            (AceFamily::Deny, true, true) => 12,
            (AceFamily::Audit, true, false) => 13,
            (AceFamily::Alarm, true, false) => 14,
            (AceFamily::Audit, true, true) => 15,
            (AceFamily::Alarm, true, true) => 16,
        };
        let mut flags = options.flags;
        if successful {
            flags |= AceFlags::SUCCESSFUL_ACCESS;
        }
        if failed {
            flags |= AceFlags::FAILED_ACCESS;
        }

        let mut bytes = vec![type_code, flags.bits(), 0, 0];
        bytes.extend_from_slice(&mask.bits().to_le_bytes());
        if object {
            let mut object_flags = ObjectAceFlags::empty();
            object_flags.set(
                ObjectAceFlags::OBJECT_TYPE_PRESENT,
                options.object_type.is_some(),
            );
            object_flags.set(
                ObjectAceFlags::INHERITED_OBJECT_TYPE_PRESENT,
                options.inherited_object_type.is_some(),
            );
            bytes.extend_from_slice(&object_flags.bits().to_le_bytes());
            if let Some(value) = options.object_type {
                bytes.extend_from_slice(&value.to_bytes());
            }
            if let Some(value) = options.inherited_object_type {
                bytes.extend_from_slice(&value.to_bytes());
            }
        }
        bytes.extend_from_slice(&sid.to_bytes());
        bytes.extend_from_slice(&options.application_data);
        bytes.resize(bytes.len().next_multiple_of(4), 0);
        let size = u16::try_from(bytes.len()).map_err(|_| AceBuildError::Size(bytes.len()))?;
        bytes[2..4].copy_from_slice(&size.to_le_bytes());
        Ok(Self(bytes.into_boxed_slice()))
    }

    /// Returns the owned packet bytes.
    pub fn into_boxed_bytes(self) -> Box<[u8]> {
        self.0
    }
}

#[derive(Clone, Copy)]
enum AceFamily {
    Allow,
    Deny,
    Audit,
    Alarm,
}

impl Deref for AceBuf {
    type Target = Ace;

    fn deref(&self) -> &Self::Target {
        // The constructor invariant guarantees validation.
        unsafe { &*(&*self.0 as *const [u8] as *const Ace) }
    }
}

impl AsRef<Ace> for AceBuf {
    fn as_ref(&self) -> &Ace {
        self
    }
}

impl Borrow<Ace> for AceBuf {
    fn borrow(&self) -> &Ace {
        self
    }
}

impl ToOwned for Ace {
    type Owned = AceBuf;

    fn to_owned(&self) -> Self::Owned {
        AceBuf(self.0.into())
    }
}

impl Serialize for AceBuf {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for AceBuf {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let bytes = Box::<[u8]>::deserialize(deserializer)?;
        Self::try_from_bytes(bytes).map_err(de::Error::custom)
    }
}

/// An owned, validated native Windows ACL packet.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AclBuf(Box<[u8]>);

impl AclBuf {
    /// Takes ownership of a raw packet after validating it.
    pub fn try_from_bytes(bytes: impl Into<Box<[u8]>>) -> Result<Self, AclError> {
        let bytes = bytes.into();
        Acl::from_bytes(&bytes)?;
        Ok(Self(bytes))
    }

    /// Takes ownership of an ACL packet without validating it.
    ///
    /// # Safety
    ///
    /// `bytes` must contain a complete, structurally valid native ACL packet.
    /// Violating this invariant can make methods that view it as an [`Acl`]
    /// perform unchecked indexing.
    pub unsafe fn from_bytes_unchecked(bytes: impl Into<Box<[u8]>>) -> Self {
        Self(bytes.into())
    }

    /// Builds an ACL from already-validated ACE packets.
    pub fn from_aces<I, A>(aces: I, revision: Option<AclRevision>) -> Result<Self, AclBuildError>
    where
        I: IntoIterator<Item = A>,
        A: AsRef<Ace>,
    {
        let aces: Vec<A> = aces.into_iter().collect();
        let mut size = ACL_HEADER_LEN;
        let mut has_object = false;
        for ace in &aces {
            let ace = ace.as_ref();
            size = size
                .checked_add(ace.as_bytes().len())
                .ok_or(AclBuildError::Size(usize::MAX))?;
            has_object |= matches!(
                ace.ace_type(),
                AceType::AccessAllowedObject
                    | AceType::AccessDeniedObject
                    | AceType::SystemAuditObject
                    | AceType::SystemAlarmObject
                    | AceType::AccessAllowedCallbackObject
                    | AceType::AccessDeniedCallbackObject
                    | AceType::SystemAuditCallbackObject
                    | AceType::SystemAlarmCallbackObject
            );
        }
        let count = u16::try_from(aces.len()).map_err(|_| AclBuildError::Count(aces.len()))?;
        let size16 = u16::try_from(size).map_err(|_| AclBuildError::Size(size))?;
        let revision = revision.unwrap_or(if has_object {
            AclRevision::DirectoryService
        } else {
            AclRevision::Basic
        });
        if let AclRevision::Unknown(revision) = revision {
            return Err(AclBuildError::Revision(revision));
        }
        if revision == AclRevision::Basic && has_object {
            return Err(AclBuildError::ObjectRevision);
        }

        let mut bytes = Vec::with_capacity(size);
        bytes.extend_from_slice(&[revision.into(), 0]);
        bytes.extend_from_slice(&size16.to_le_bytes());
        bytes.extend_from_slice(&count.to_le_bytes());
        bytes.extend_from_slice(&[0, 0]);
        for ace in &aces {
            bytes.extend_from_slice(ace.as_ref().as_bytes());
        }
        Ok(Self(bytes.into_boxed_slice()))
    }

    /// Returns the owned packet bytes.
    pub fn into_boxed_bytes(self) -> Box<[u8]> {
        self.0
    }
}

impl Deref for AclBuf {
    type Target = Acl;

    fn deref(&self) -> &Self::Target {
        // The constructor invariant guarantees validation.
        unsafe { &*(&*self.0 as *const [u8] as *const Acl) }
    }
}

impl AsRef<Acl> for AclBuf {
    fn as_ref(&self) -> &Acl {
        self
    }
}

impl Borrow<Acl> for AclBuf {
    fn borrow(&self) -> &Acl {
        self
    }
}

impl ToOwned for Acl {
    type Owned = AclBuf;

    fn to_owned(&self) -> Self::Owned {
        AclBuf(self.0.into())
    }
}

impl Serialize for AclBuf {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for AclBuf {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let bytes = Box::<[u8]>::deserialize(deserializer)?;
        Self::try_from_bytes(bytes).map_err(de::Error::custom)
    }
}

/// Error returned when building an ACE.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AceBuildError {
    /// An audit ACE selected neither successful nor failed access.
    AuditOutcome,
    /// Audit outcome bits were supplied both explicitly and through `flags`.
    AuditFlags,
    /// The generated packet exceeds the 16-bit native size field.
    Size(usize),
}

impl fmt::Display for AceBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AuditOutcome => f.write_str("audit ACE requires a successful or failed outcome"),
            Self::AuditFlags => f.write_str("audit outcome bits must not be supplied in flags"),
            Self::Size(size) => write!(f, "ACE packet size {size} exceeds the native limit"),
        }
    }
}

impl error::Error for AceBuildError {}

/// Error returned when building an ACL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AclBuildError {
    /// The requested ACL revision is unsupported.
    Revision(u8),
    /// Revision 2 cannot encode an object ACE.
    ObjectRevision,
    /// The ACL contains too many ACEs for its 16-bit count field.
    Count(usize),
    /// The ACL packet exceeds its 16-bit size field.
    Size(usize),
}

impl fmt::Display for AclBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Revision(revision) => write!(f, "unsupported ACL revision {revision}"),
            Self::ObjectRevision => f.write_str("ACL revision 2 cannot contain object ACEs"),
            Self::Count(count) => write!(f, "ACL ACE count {count} exceeds the native limit"),
            Self::Size(size) => write!(f, "ACL packet size {size} exceeds the native limit"),
        }
    }
}

impl error::Error for AclBuildError {}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, AceError> {
    let bytes = bytes.get(offset..offset + 4).ok_or(AceError::Body)?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

fn parse_ace_sid(bytes: &[u8], offset: usize) -> Result<(Sid, usize), AceError> {
    let header = bytes.get(offset..offset + 8).ok_or(AceError::Sid)?;
    let length = 8 + usize::from(header[1]) * 4;
    let sid = bytes.get(offset..offset + length).ok_or(AceError::Sid)?;
    let sid = Sid::from_bytes(sid).map_err(|_| AceError::Sid)?;
    Ok((sid, offset + length))
}

/// Error returned when parsing an ACL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AclError {
    /// The packet is shorter than an ACL header or is not DWORD-aligned.
    Length(usize),
    /// The declared packet size differs from the supplied byte length.
    Size(u16, usize),
    /// Fewer ACE packets could be read than the header declares.
    AceCount(u16, usize),
    /// An ACE at the supplied index is malformed.
    Ace(usize, AceError),
}

impl fmt::Display for AclError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Length(length) => write!(f, "ACL packet has invalid length {length}"),
            Self::Size(declared, actual) => write!(
                f,
                "ACL packet declares size {declared}, but contains {actual} bytes"
            ),
            Self::AceCount(count, parsed) => write!(
                f,
                "ACL declares {count} ACEs, but only {parsed} can be traversed"
            ),
            Self::Ace(index, error) => write!(f, "ACE {index} is invalid: {error}"),
        }
    }
}

impl error::Error for AclError {}

/// Error returned when parsing an ACE.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AceError {
    /// The packet is shorter than an ACE header.
    Length(usize),
    /// The declared packet size differs from the supplied byte length.
    Size(u16, usize),
    /// The packet is not DWORD-aligned.
    Alignment(usize),
    /// An ACE declared inside an ACL extends beyond that ACL's bytes.
    Bounds(usize),
    /// A recognized ACE body is truncated or malformed.
    Body,
    /// The ACE's embedded SID is malformed.
    Sid,
}

impl fmt::Display for AceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Length(length) => write!(f, "ACE packet has invalid length {length}"),
            Self::Size(declared, actual) => write!(
                f,
                "ACE packet declares size {declared}, but contains {actual} bytes"
            ),
            Self::Alignment(length) => write!(f, "ACE packet length {length} is not aligned"),
            Self::Bounds(size) => write!(f, "ACE of size {size} exceeds its ACL"),
            Self::Body => f.write_str("ACE body is truncated"),
            Self::Sid => f.write_str("ACE contains an invalid SID"),
        }
    }
}

impl error::Error for AceError {}

/// A validated, portable representation of a self-relative Windows security descriptor.
///
/// A descriptor tracks which components were loaded through its
/// [`mask`](Self::mask), so partial security queries can be carried without
/// pretending that omitted components were absent on the target. Use
/// [`SecDescUpdate`] with [`with`](Self::with) to make a checked update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecDesc {
    mask: SecInfo,
    revision: Revision,
    rm_control: u8,
    control: SecDescControl,
    owner: Option<Sid>,
    group: Option<Sid>,
    dacl: Option<AclBuf>,
    sacl: Option<AclBuf>,
}

/// A builder for a functional update to a [`SecDesc`].
///
/// Component and control-flag methods select the values to replace.
/// Internally, control changes are stored as compact set/clear bitmasks;
/// [`SecDesc::with`] rejects changes that do not apply to loaded components.
#[derive(Clone, Debug)]
pub struct SecDescUpdate {
    owner: Option<Option<Sid>>,
    group: Option<Option<Sid>>,
    dacl: Option<Option<AclBuf>>,
    sacl: Option<Option<AclBuf>>,
    set_flags: SecDescControl,
    clear_flags: SecDescControl,
    rm_control: Option<Option<u8>>,
}

impl SecDescUpdate {
    /// Creates an update with no changes.
    #[allow(clippy::new_without_default)]
    pub const fn new() -> Self {
        Self {
            owner: None,
            group: None,
            dacl: None,
            sacl: None,
            set_flags: SecDescControl::empty(),
            clear_flags: SecDescControl::empty(),
            rm_control: None,
        }
    }

    /// Replaces the owner, or clears it with `None`.
    pub fn owner(mut self, owner: Option<Sid>) -> Self {
        self.owner = Some(owner);
        self
    }
    /// Replaces the primary group, or clears it with `None`.
    pub fn group(mut self, group: Option<Sid>) -> Self {
        self.group = Some(group);
        self
    }
    /// Replaces the DACL, or supplies a null DACL with `None`.
    pub fn dacl(mut self, dacl: Option<AclBuf>) -> Self {
        self.dacl = Some(dacl);
        self
    }
    /// Replaces the SACL, or supplies a null SACL with `None`.
    pub fn sacl(mut self, sacl: Option<AclBuf>) -> Self {
        self.sacl = Some(sacl);
        self
    }
    /// Sets the owner-defaulted control flag.
    pub fn owner_defaulted(self, value: bool) -> Self {
        self.control_flag(SecDescControl::OWNER_DEFAULTED.bits(), value)
    }
    /// Sets the group-defaulted control flag.
    pub fn group_defaulted(self, value: bool) -> Self {
        self.control_flag(SecDescControl::GROUP_DEFAULTED.bits(), value)
    }
    /// Sets whether a DACL is present.
    pub fn dacl_present(self, value: bool) -> Self {
        self.control_flag(SecDescControl::DACL_PRESENT.bits(), value)
    }
    /// Sets the DACL-defaulted control flag.
    pub fn dacl_defaulted(self, value: bool) -> Self {
        self.control_flag(SecDescControl::DACL_DEFAULTED.bits(), value)
    }
    /// Sets the DACL auto-inheritance-requested control flag.
    pub fn dacl_auto_inherit_required(self, value: bool) -> Self {
        self.control_flag(SecDescControl::DACL_AUTO_INHERIT_REQUIRED.bits(), value)
    }
    /// Sets the DACL auto-inherited control flag.
    pub fn dacl_auto_inherited(self, value: bool) -> Self {
        self.control_flag(SecDescControl::DACL_AUTO_INHERITED.bits(), value)
    }
    /// Sets the DACL-protected control flag.
    pub fn dacl_protected(self, value: bool) -> Self {
        self.control_flag(SecDescControl::DACL_PROTECTED.bits(), value)
    }
    /// Sets whether a SACL is present.
    pub fn sacl_present(self, value: bool) -> Self {
        self.control_flag(SecDescControl::SACL_PRESENT.bits(), value)
    }
    /// Sets the SACL-defaulted control flag.
    pub fn sacl_defaulted(self, value: bool) -> Self {
        self.control_flag(SecDescControl::SACL_DEFAULTED.bits(), value)
    }
    /// Sets the SACL auto-inheritance-requested control flag.
    pub fn sacl_auto_inherit_required(self, value: bool) -> Self {
        self.control_flag(SecDescControl::SACL_AUTO_INHERIT_REQUIRED.bits(), value)
    }
    /// Sets the SACL auto-inherited control flag.
    pub fn sacl_auto_inherited(self, value: bool) -> Self {
        self.control_flag(SecDescControl::SACL_AUTO_INHERITED.bits(), value)
    }
    /// Sets the SACL-protected control flag.
    pub fn sacl_protected(self, value: bool) -> Self {
        self.control_flag(SecDescControl::SACL_PROTECTED.bits(), value)
    }
    /// Sets the resource-manager control byte, or clears its validity with `None`.
    pub fn rm_control(mut self, rm_control: Option<u8>) -> Self {
        self.rm_control = Some(rm_control);
        self
    }

    fn control_flag(mut self, flag: u16, value: bool) -> Self {
        let flag = SecDescControl::from_bits_retain(flag);
        self.set_flags.remove(flag);
        self.clear_flags.remove(flag);
        if value {
            self.set_flags |= flag;
        } else {
            self.clear_flags |= flag;
        }
        self
    }
}

impl SecDesc {
    /// Creates a security descriptor from its structural components.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        mask: SecInfo,
        rm_control: u8,
        control: SecDescControl,
        owner: Option<Sid>,
        group: Option<Sid>,
        dacl: Option<AclBuf>,
        sacl: Option<AclBuf>,
    ) -> Result<Self, SecDescError> {
        if !mask.contains(SecInfo::OWNER) && owner.is_some() {
            return Err(SecDescError::OwnerNotLoaded);
        }
        if !mask.contains(SecInfo::GROUP) && group.is_some() {
            return Err(SecDescError::GroupNotLoaded);
        }
        validate_acl(
            AclKind::Dacl,
            mask.contains(SecInfo::DACL),
            control.contains(SecDescControl::DACL_PRESENT),
            dacl.as_ref(),
        )?;
        validate_acl(
            AclKind::Sacl,
            mask.contains(SecInfo::SACL),
            control.contains(SecDescControl::SACL_PRESENT),
            sacl.as_ref(),
        )?;

        Ok(Self {
            mask,
            revision: Revision::One,
            rm_control,
            control: control - SecDescControl::SELF_RELATIVE,
            owner,
            group,
            dacl,
            sacl,
        })
    }

    /// Parses a self-relative Windows security descriptor packet.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, SecDescError> {
        Self::from_bytes_with_mask(bytes, SecInfo::ALL)
    }

    /// Parses the selected components of a self-relative Windows security descriptor packet.
    pub fn from_bytes_with_mask(bytes: &[u8], mask: SecInfo) -> Result<Self, SecDescError> {
        if bytes.len() < SELF_RELATIVE_HEADER_LEN {
            return Err(SecDescError::PacketLength);
        }
        match bytes[0] {
            REVISION => {}
            revision => return Err(SecDescError::Revision(revision)),
        }
        let rm_control = bytes[1];
        let control = u16::from_le_bytes(bytes[2..4].try_into().unwrap());
        if control & SecDescControl::SELF_RELATIVE.bits() == 0 {
            return Err(SecDescError::NotSelfRelative);
        }

        let owner_offset = packet_offset(bytes, 4, SecDescComponent::Owner)?;
        let group_offset = packet_offset(bytes, 8, SecDescComponent::Group)?;
        let sacl_offset = packet_offset(bytes, 12, SecDescComponent::Sacl)?;
        let dacl_offset = packet_offset(bytes, 16, SecDescComponent::Dacl)?;
        if control & SecDescControl::SACL_PRESENT.bits() == 0 && sacl_offset != 0 {
            return Err(SecDescError::AclNotPresent(AclKind::Sacl));
        }
        if control & SecDescControl::DACL_PRESENT.bits() == 0 && dacl_offset != 0 {
            return Err(SecDescError::AclNotPresent(AclKind::Dacl));
        }
        let owner = mask
            .contains(SecInfo::OWNER)
            .then(|| parse_sid(bytes, owner_offset, SecDescComponent::Owner))
            .transpose()?
            .flatten();
        let group = mask
            .contains(SecInfo::GROUP)
            .then(|| parse_sid(bytes, group_offset, SecDescComponent::Group))
            .transpose()?
            .flatten();
        let sacl = mask
            .contains(SecInfo::SACL)
            .then(|| parse_acl(bytes, sacl_offset, AclKind::Sacl))
            .transpose()?
            .flatten();
        let dacl = mask
            .contains(SecInfo::DACL)
            .then(|| parse_acl(bytes, dacl_offset, AclKind::Dacl))
            .transpose()?
            .flatten();

        Self::new(
            mask,
            rm_control,
            SecDescControl::from_bits_retain(control),
            owner,
            group,
            dacl,
            sacl,
        )
    }

    /// Converts this descriptor to a canonical self-relative Windows packet.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = vec![0; SELF_RELATIVE_HEADER_LEN];
        bytes[0] = self.revision as u8;
        bytes[1] = self.rm_control;
        bytes[2..4].copy_from_slice(
            &(self.control | SecDescControl::SELF_RELATIVE)
                .bits()
                .to_le_bytes(),
        );

        let owner = self.owner.as_ref().map(Sid::to_bytes);
        let group = self.group.as_ref().map(Sid::to_bytes);
        append_component(&mut bytes, 4, owner.as_deref());
        append_component(&mut bytes, 8, group.as_deref());
        append_component(&mut bytes, 12, self.sacl.as_deref().map(Acl::as_bytes));
        append_component(&mut bytes, 16, self.dacl.as_deref().map(Acl::as_bytes));
        bytes
    }

    /// Returns the native SECURITY_INFORMATION mask associated with the descriptor.
    pub const fn mask(&self) -> SecInfo {
        self.mask
    }

    /// Returns the security descriptor revision.
    pub const fn revision(&self) -> Revision {
        self.revision
    }

    /// Returns the security descriptor control mask.
    pub const fn control(&self) -> SecDescControl {
        self.control
    }

    /// Returns the resource-manager control byte when it is valid.
    pub const fn rm_control(&self) -> Option<u8> {
        if self.rm_control_valid() {
            Some(self.rm_control)
        } else {
            None
        }
    }

    /// Returns whether the resource-manager control byte is valid.
    pub const fn rm_control_valid(&self) -> bool {
        self.control.contains(SecDescControl::RM_CONTROL_VALID)
    }

    /// Returns whether the owner component was loaded.
    pub const fn owner_loaded(&self) -> bool {
        self.mask.contains(SecInfo::OWNER)
    }

    /// Returns the owner SID, if present.
    pub const fn owner(&self) -> Option<&Sid> {
        self.owner.as_ref()
    }

    /// Returns whether the owner SID was supplied by a default mechanism.
    pub const fn owner_defaulted(&self) -> bool {
        self.control.contains(SecDescControl::OWNER_DEFAULTED)
    }

    /// Returns whether the group component was loaded.
    pub const fn group_loaded(&self) -> bool {
        self.mask.contains(SecInfo::GROUP)
    }

    /// Returns the primary group SID, if present.
    pub const fn group(&self) -> Option<&Sid> {
        self.group.as_ref()
    }

    /// Returns whether the group SID was supplied by a default mechanism.
    pub const fn group_defaulted(&self) -> bool {
        self.control.contains(SecDescControl::GROUP_DEFAULTED)
    }

    /// Returns whether the DACL component was loaded.
    pub const fn dacl_loaded(&self) -> bool {
        self.mask.contains(SecInfo::DACL)
    }

    /// Returns the DACL, if it is non-null.
    pub fn dacl(&self) -> Option<&Acl> {
        self.dacl.as_deref()
    }

    /// Returns whether the descriptor marks the DACL as present.
    pub const fn dacl_present(&self) -> bool {
        self.control.contains(SecDescControl::DACL_PRESENT)
    }

    /// Returns whether the DACL was supplied by a default mechanism.
    pub const fn dacl_defaulted(&self) -> bool {
        self.control.contains(SecDescControl::DACL_DEFAULTED)
    }

    /// Returns whether DACL inheritance computation was requested.
    pub const fn dacl_auto_inherit_required(&self) -> bool {
        self.control
            .contains(SecDescControl::DACL_AUTO_INHERIT_REQUIRED)
    }

    /// Returns whether the DACL was produced through inheritance.
    pub const fn dacl_auto_inherited(&self) -> bool {
        self.control.contains(SecDescControl::DACL_AUTO_INHERITED)
    }

    /// Returns whether the DACL is protected from inheritance.
    pub const fn dacl_protected(&self) -> bool {
        self.control.contains(SecDescControl::DACL_PROTECTED)
    }

    /// Returns whether the SACL component was loaded.
    pub const fn sacl_loaded(&self) -> bool {
        self.mask.contains(SecInfo::SACL)
    }

    /// Returns the SACL, if it is non-null.
    pub fn sacl(&self) -> Option<&Acl> {
        self.sacl.as_deref()
    }

    /// Returns whether the descriptor marks the SACL as present.
    pub const fn sacl_present(&self) -> bool {
        self.control.contains(SecDescControl::SACL_PRESENT)
    }

    /// Returns whether the SACL was supplied by a default mechanism.
    pub const fn sacl_defaulted(&self) -> bool {
        self.control.contains(SecDescControl::SACL_DEFAULTED)
    }

    /// Returns whether SACL inheritance computation was requested.
    pub const fn sacl_auto_inherit_required(&self) -> bool {
        self.control
            .contains(SecDescControl::SACL_AUTO_INHERIT_REQUIRED)
    }

    /// Returns whether the SACL was produced through inheritance.
    pub const fn sacl_auto_inherited(&self) -> bool {
        self.control.contains(SecDescControl::SACL_AUTO_INHERITED)
    }

    /// Returns whether the SACL is protected from inheritance.
    pub const fn sacl_protected(&self) -> bool {
        self.control.contains(SecDescControl::SACL_PROTECTED)
    }

    /// Returns a new descriptor with the supplied component and control updates.
    pub fn with(&self, update: SecDescUpdate) -> Result<Self, SecDescError> {
        let SecDescUpdate {
            owner: owner_update,
            group: group_update,
            dacl: dacl_update,
            sacl: sacl_update,
            set_flags,
            clear_flags,
            rm_control: rm_control_update,
        } = update;
        let mut mask = self.mask;
        let mut control = self.control.bits();
        let set_flags = set_flags.bits();
        let clear_flags = clear_flags.bits();

        let owner = match owner_update {
            Some(value) => {
                mask |= SecInfo::OWNER;
                value
            }
            None => self.owner.clone(),
        };
        let group = match group_update {
            Some(value) => {
                mask |= SecInfo::GROUP;
                value
            }
            None => self.group.clone(),
        };

        let (dacl, dacl_explicit) = match dacl_update {
            Some(value) => {
                mask |= SecInfo::DACL;
                set_control(&mut control, SecDescControl::DACL_PRESENT.bits(), true);
                (value, true)
            }
            None => (self.dacl.clone(), false),
        };
        let (sacl, sacl_explicit) = match sacl_update {
            Some(value) => {
                mask |= SecInfo::SACL;
                set_control(&mut control, SecDescControl::SACL_PRESENT.bits(), true);
                (value, true)
            }
            None => (self.sacl.clone(), false),
        };

        let dacl = apply_presence(
            AclKind::Dacl,
            &mut mask,
            &mut control,
            SecInfo::DACL,
            SecDescControl::DACL_PRESENT.bits(),
            flag_update(set_flags, clear_flags, SecDescControl::DACL_PRESENT.bits()),
            dacl_explicit,
            dacl,
        )?;
        let sacl = apply_presence(
            AclKind::Sacl,
            &mut mask,
            &mut control,
            SecInfo::SACL,
            SecDescControl::SACL_PRESENT.bits(),
            flag_update(set_flags, clear_flags, SecDescControl::SACL_PRESENT.bits()),
            sacl_explicit,
            sacl,
        )?;

        apply_component_flag(
            SecDescComponent::Owner,
            mask.contains(SecInfo::OWNER),
            &mut control,
            SecDescControl::OWNER_DEFAULTED.bits(),
            flag_update(
                set_flags,
                clear_flags,
                SecDescControl::OWNER_DEFAULTED.bits(),
            ),
        )?;
        apply_component_flag(
            SecDescComponent::Group,
            mask.contains(SecInfo::GROUP),
            &mut control,
            SecDescControl::GROUP_DEFAULTED.bits(),
            flag_update(
                set_flags,
                clear_flags,
                SecDescControl::GROUP_DEFAULTED.bits(),
            ),
        )?;
        for (name, loaded, flag, value) in [
            (
                SecDescComponent::Dacl,
                mask.contains(SecInfo::DACL),
                SecDescControl::DACL_DEFAULTED.bits(),
                flag_update(
                    set_flags,
                    clear_flags,
                    SecDescControl::DACL_DEFAULTED.bits(),
                ),
            ),
            (
                SecDescComponent::Dacl,
                mask.contains(SecInfo::DACL),
                SecDescControl::DACL_AUTO_INHERIT_REQUIRED.bits(),
                flag_update(
                    set_flags,
                    clear_flags,
                    SecDescControl::DACL_AUTO_INHERIT_REQUIRED.bits(),
                ),
            ),
            (
                SecDescComponent::Dacl,
                mask.contains(SecInfo::DACL),
                SecDescControl::DACL_AUTO_INHERITED.bits(),
                flag_update(
                    set_flags,
                    clear_flags,
                    SecDescControl::DACL_AUTO_INHERITED.bits(),
                ),
            ),
            (
                SecDescComponent::Dacl,
                mask.contains(SecInfo::DACL),
                SecDescControl::DACL_PROTECTED.bits(),
                flag_update(
                    set_flags,
                    clear_flags,
                    SecDescControl::DACL_PROTECTED.bits(),
                ),
            ),
            (
                SecDescComponent::Sacl,
                mask.contains(SecInfo::SACL),
                SecDescControl::SACL_DEFAULTED.bits(),
                flag_update(
                    set_flags,
                    clear_flags,
                    SecDescControl::SACL_DEFAULTED.bits(),
                ),
            ),
            (
                SecDescComponent::Sacl,
                mask.contains(SecInfo::SACL),
                SecDescControl::SACL_AUTO_INHERIT_REQUIRED.bits(),
                flag_update(
                    set_flags,
                    clear_flags,
                    SecDescControl::SACL_AUTO_INHERIT_REQUIRED.bits(),
                ),
            ),
            (
                SecDescComponent::Sacl,
                mask.contains(SecInfo::SACL),
                SecDescControl::SACL_AUTO_INHERITED.bits(),
                flag_update(
                    set_flags,
                    clear_flags,
                    SecDescControl::SACL_AUTO_INHERITED.bits(),
                ),
            ),
            (
                SecDescComponent::Sacl,
                mask.contains(SecInfo::SACL),
                SecDescControl::SACL_PROTECTED.bits(),
                flag_update(
                    set_flags,
                    clear_flags,
                    SecDescControl::SACL_PROTECTED.bits(),
                ),
            ),
        ] {
            apply_component_flag(name, loaded, &mut control, flag, value)?;
        }

        let rm_control = match rm_control_update {
            Some(Some(value)) => {
                set_control(&mut control, SecDescControl::RM_CONTROL_VALID.bits(), true);
                value
            }
            Some(None) => {
                set_control(&mut control, SecDescControl::RM_CONTROL_VALID.bits(), false);
                0
            }
            None => self.rm_control,
        };

        Ok(Self {
            mask,
            revision: self.revision,
            rm_control,
            control: SecDescControl::from_bits_retain(control),
            owner,
            group,
            dacl,
            sacl,
        })
    }
}

fn set_control(control: &mut u16, flag: u16, value: bool) {
    if value {
        *control |= flag;
    } else {
        *control &= !flag;
    }
}

fn flag_update(set_flags: u16, clear_flags: u16, flag: u16) -> Option<bool> {
    if set_flags & flag != 0 {
        Some(true)
    } else if clear_flags & flag != 0 {
        Some(false)
    } else {
        None
    }
}

fn apply_component_flag(
    component: SecDescComponent,
    loaded: bool,
    control: &mut u16,
    flag: u16,
    value: Option<bool>,
) -> Result<(), SecDescError> {
    if let Some(value) = value {
        if !loaded {
            return Err(SecDescError::ComponentNotLoaded(component));
        }
        set_control(control, flag, value);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn apply_presence(
    acl_kind: AclKind,
    mask: &mut SecInfo,
    control: &mut u16,
    mask_flag: SecInfo,
    present_flag: u16,
    requested: Option<bool>,
    explicit: bool,
    mut acl: Option<AclBuf>,
) -> Result<Option<AclBuf>, SecDescError> {
    if let Some(present) = requested {
        let was_loaded = mask.contains(mask_flag);
        if !present {
            if explicit {
                return Err(SecDescError::AclPresenceConflict(acl_kind));
            }
            acl = None;
            set_control(control, present_flag, false);
        } else {
            if !explicit && (!was_loaded || *control & present_flag == 0) {
                return Err(SecDescError::AclPresenceRequiresValue(acl_kind));
            }
            set_control(control, present_flag, true);
        }
        *mask |= mask_flag;
    }
    Ok(acl)
}

fn packet_offset(
    bytes: &[u8],
    at: usize,
    component: SecDescComponent,
) -> Result<usize, SecDescError> {
    let offset = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    usize::try_from(offset).map_err(|_| SecDescError::PacketOffset(component, offset))
}

fn validate_offset(
    bytes: &[u8],
    offset: usize,
    component: SecDescComponent,
) -> Result<(), SecDescError> {
    if offset < SELF_RELATIVE_HEADER_LEN || !offset.is_multiple_of(4) || offset >= bytes.len() {
        return Err(SecDescError::PacketOffset(
            component,
            u32::try_from(offset).unwrap_or(u32::MAX),
        ));
    }
    Ok(())
}

fn parse_sid(
    bytes: &[u8],
    offset: usize,
    component: SecDescComponent,
) -> Result<Option<Sid>, SecDescError> {
    if offset == 0 {
        return Ok(None);
    }
    validate_offset(bytes, offset, component)?;
    let header = bytes
        .get(offset..offset + 8)
        .ok_or(SecDescError::PacketComponent(component))?;
    let length = 8 + usize::from(header[1]) * 4;
    let sid = bytes
        .get(offset..offset + length)
        .ok_or(SecDescError::PacketComponent(component))?;
    Sid::from_bytes(sid)
        .map(Some)
        .map_err(|_| SecDescError::PacketComponent(component))
}

fn parse_acl(
    bytes: &[u8],
    offset: usize,
    acl_kind: AclKind,
) -> Result<Option<AclBuf>, SecDescError> {
    if offset == 0 {
        return Ok(None);
    }
    validate_offset(bytes, offset, acl_kind.component())?;
    let header = bytes
        .get(offset..offset + ACL_HEADER_LEN)
        .ok_or(SecDescError::PacketComponent(acl_kind.component()))?;
    let length = usize::from(u16::from_le_bytes(header[2..4].try_into().unwrap()));
    let acl = bytes
        .get(offset..offset + length)
        .ok_or(SecDescError::PacketComponent(acl_kind.component()))?;
    AclBuf::try_from_bytes(acl.to_vec().into_boxed_slice())
        .map(Some)
        .map_err(|error| SecDescError::Acl(acl_kind, error))
}

fn append_component(bytes: &mut Vec<u8>, offset_at: usize, component: Option<&[u8]>) {
    let Some(component) = component else {
        return;
    };
    let offset = u32::try_from(bytes.len()).expect("security descriptor exceeds 4 GiB");
    bytes[offset_at..offset_at + 4].copy_from_slice(&offset.to_le_bytes());
    bytes.extend_from_slice(component);
}

fn validate_acl(
    acl_kind: AclKind,
    loaded: bool,
    present: bool,
    acl: Option<&AclBuf>,
) -> Result<(), SecDescError> {
    let Some(_acl) = acl else {
        return Ok(());
    };
    if !loaded {
        return Err(SecDescError::AclNotLoaded(acl_kind));
    }
    if !present {
        return Err(SecDescError::AclNotPresent(acl_kind));
    }
    Ok(())
}

impl Serialize for SecDesc {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut tuple = serializer.serialize_tuple(8)?;
        tuple.serialize_element(&self.mask.bits())?;
        tuple.serialize_element(&(self.revision as u8))?;
        tuple.serialize_element(&self.rm_control)?;
        tuple.serialize_element(&self.control.bits())?;
        tuple.serialize_element(&self.owner)?;
        tuple.serialize_element(&self.group)?;
        tuple.serialize_element(&self.dacl)?;
        tuple.serialize_element(&self.sacl)?;
        tuple.end()
    }
}

impl<'de> Deserialize<'de> for SecDesc {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SecDescVisitor;

        impl<'de> Visitor<'de> for SecDescVisitor {
            type Value = SecDesc;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a structurally encoded Windows security descriptor")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mask = SecInfo::from_bits_retain(next(&mut seq, 0, &self)?);
                match next(&mut seq, 1, &self)? {
                    REVISION => {}
                    revision => return Err(de::Error::custom(SecDescError::Revision(revision))),
                }
                let rm_control = next(&mut seq, 2, &self)?;
                let control = SecDescControl::from_bits_retain(next(&mut seq, 3, &self)?);
                let owner = next(&mut seq, 4, &self)?;
                let group = next(&mut seq, 5, &self)?;
                let dacl = next(&mut seq, 6, &self)?;
                let sacl = next(&mut seq, 7, &self)?;
                SecDesc::new(mask, rm_control, control, owner, group, dacl, sacl)
                    .map_err(de::Error::custom)
            }
        }

        fn next<'de, A, T>(
            seq: &mut A,
            index: usize,
            visitor: &dyn de::Expected,
        ) -> Result<T, A::Error>
        where
            A: SeqAccess<'de>,
            T: Deserialize<'de>,
        {
            seq.next_element()?
                .ok_or_else(|| de::Error::invalid_length(index, visitor))
        }

        deserializer.deserialize_tuple(8, SecDescVisitor)
    }
}

/// Error returned when constructing or deserializing a security descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecDescError {
    /// The descriptor uses an unsupported revision.
    Revision(u8),
    /// An owner SID was supplied without loading the owner component.
    OwnerNotLoaded,
    /// A group SID was supplied without loading the group component.
    GroupNotLoaded,
    /// An ACL was supplied without loading its component.
    AclNotLoaded(AclKind),
    /// An ACL was supplied while its present bit is clear.
    AclNotPresent(AclKind),
    /// An update supplies an ACL while explicitly marking it absent.
    AclPresenceConflict(AclKind),
    /// An update marks an unloaded or absent ACL present without supplying one.
    AclPresenceRequiresValue(AclKind),
    /// An update changes a control bit for an unloaded component.
    ComponentNotLoaded(SecDescComponent),
    /// A supplied ACL packet is malformed.
    Acl(AclKind, AclError),
    /// The self-relative descriptor header is truncated.
    PacketLength,
    /// The descriptor is in absolute rather than self-relative form.
    NotSelfRelative,
    /// A component offset is invalid or misaligned.
    PacketOffset(SecDescComponent, u32),
    /// A component packet is truncated or malformed.
    PacketComponent(SecDescComponent),
}

impl fmt::Display for SecDescError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Revision(revision) => {
                write!(f, "unsupported security descriptor revision {revision}")
            }
            Self::OwnerNotLoaded => f.write_str("owner SID supplied when owner was not loaded"),
            Self::GroupNotLoaded => f.write_str("group SID supplied when group was not loaded"),
            Self::AclNotLoaded(name) => write!(f, "{name} supplied when it was not loaded"),
            Self::AclNotPresent(name) => {
                write!(f, "{name} supplied when its PRESENT control bit is clear")
            }
            Self::AclPresenceConflict(name) => {
                write!(f, "{name} cannot be supplied with presence false")
            }
            Self::AclPresenceRequiresValue(name) => {
                write!(
                    f,
                    "{name} presence true requires an existing or supplied ACL"
                )
            }
            Self::ComponentNotLoaded(name) => {
                write!(f, "cannot update control flags for unloaded {name}")
            }
            Self::Acl(name, error) => write!(f, "invalid {name}: {error}"),
            Self::PacketLength => f.write_str("security descriptor packet is too short"),
            Self::NotSelfRelative => f.write_str("security descriptor packet is not self-relative"),
            Self::PacketOffset(name, offset) => {
                write!(f, "security descriptor {name} has invalid offset {offset}")
            }
            Self::PacketComponent(name) => {
                write!(f, "security descriptor contains an invalid {name}")
            }
        }
    }
}

impl error::Error for SecDescError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(value: &str) -> Sid {
        value.parse().unwrap()
    }

    fn acl(size: u16) -> Vec<u8> {
        let mut value = vec![0; usize::from(size)];
        value[0] = 2;
        value[2..4].copy_from_slice(&size.to_le_bytes());
        value
    }

    fn valid_acl(size: u16) -> AclBuf {
        AclBuf::try_from_bytes(acl(size).into_boxed_slice()).unwrap()
    }

    fn ace(ace_type: u8, flags: u8, mask: u32, sid: &Sid, application: &[u8]) -> Vec<u8> {
        let mut value = vec![ace_type, flags, 0, 0];
        value.extend_from_slice(&mask.to_le_bytes());
        value.extend_from_slice(&sid.to_bytes());
        value.extend_from_slice(application);
        let size = u16::try_from(value.len()).unwrap();
        value[2..4].copy_from_slice(&size.to_le_bytes());
        value
    }

    fn acl_with_aces(aces: &[Vec<u8>], tail: &[u8]) -> Vec<u8> {
        let size = ACL_HEADER_LEN + aces.iter().map(Vec::len).sum::<usize>() + tail.len();
        let mut value = vec![2, 0];
        value.extend_from_slice(&u16::try_from(size).unwrap().to_le_bytes());
        value.extend_from_slice(&u16::try_from(aces.len()).unwrap().to_le_bytes());
        value.extend_from_slice(&[0, 0]);
        for ace in aces {
            value.extend_from_slice(ace);
        }
        value.extend_from_slice(tail);
        value
    }

    #[test]
    fn exposes_known_and_unknown_aces_without_losing_bytes() {
        let trustee = sid("S-1-5-32-544");
        let known = ace(0, 0x13, 0x1234_5678, &trustee, &[0xde, 0xad, 0xbe, 0xef]);
        let unknown = vec![0x7f, 0xa0, 8, 0, 0x11, 0x22, 0x33, 0x44];
        let bytes = acl_with_aces(&[known.clone(), unknown.clone()], &[0xaa, 0xbb, 0xcc, 0xdd]);
        let acl = Acl::from_bytes(&bytes).unwrap();

        assert_eq!(acl.revision(), AclRevision::Basic);
        assert_eq!(usize::from(acl.size()), bytes.len());
        assert_eq!(acl.ace_count(), 2);
        assert_eq!(acl.as_bytes(), bytes);

        let mut aces = acl.aces();
        let first = aces.next().unwrap();
        assert_eq!(first.ace_type(), AceType::AccessAllowed);
        assert_eq!(first.flags().bits(), 0x13);
        assert_eq!(first.mask().map(|mask| mask.bits()), Some(0x1234_5678));
        assert_eq!(first.sid(), Some(trustee));
        assert_eq!(
            first.application_data(),
            Some(&[0xde, 0xad, 0xbe, 0xef][..])
        );
        assert_eq!(first.as_bytes(), known);

        let second = aces.next().unwrap();
        assert_eq!(second.ace_type(), AceType::Unknown(0x7f));
        assert_eq!(second.mask(), None);
        assert_eq!(second.application_data(), None);
        assert_eq!(second.as_bytes(), unknown);
        assert_eq!(aces.next(), None);
    }

    #[test]
    fn parses_object_ace_guids_and_application_data() {
        let object_type: Guid = "00112233-4455-6677-8899-aabbccddeeff".parse().unwrap();
        let inherited_type: Guid = "ffeeddcc-bbaa-9988-7766-554433221100".parse().unwrap();
        let trustee = sid("S-1-1-0");
        for object_flags in 0..=3u32 {
            let mut bytes = vec![11, 0, 0, 0];
            bytes.extend_from_slice(&0x90ab_cdefu32.to_le_bytes());
            bytes.extend_from_slice(&object_flags.to_le_bytes());
            if object_flags & 1 != 0 {
                bytes.extend_from_slice(&object_type.to_bytes());
            }
            if object_flags & 2 != 0 {
                bytes.extend_from_slice(&inherited_type.to_bytes());
            }
            bytes.extend_from_slice(&trustee.to_bytes());
            bytes.extend_from_slice(&[1, 2, 3, 4]);
            let size = u16::try_from(bytes.len()).unwrap();
            bytes[2..4].copy_from_slice(&size.to_le_bytes());

            let ace = Ace::from_bytes(&bytes).unwrap();
            assert_eq!(ace.ace_type(), AceType::AccessAllowedCallbackObject);
            assert_eq!(
                ace.object_flags().map(|flags| flags.bits()),
                Some(object_flags)
            );
            assert_eq!(
                ace.object_type(),
                (object_flags & 1 != 0).then_some(object_type)
            );
            assert_eq!(
                ace.inherited_object_type(),
                (object_flags & 2 != 0).then_some(inherited_type)
            );
            assert_eq!(ace.sid(), Some(trustee.clone()));
            assert_eq!(ace.application_data(), Some(&[1, 2, 3, 4][..]));
        }
    }

    #[test]
    fn rejects_untraversable_or_malformed_aces() {
        let mut count_mismatch = acl(8);
        count_mismatch[4..6].copy_from_slice(&1u16.to_le_bytes());
        assert_eq!(
            Acl::from_bytes(&count_mismatch),
            Err(AclError::AceCount(1, 0))
        );

        let malformed = vec![0, 0, 8, 0, 0, 0, 0, 0];
        let bytes = acl_with_aces(&[malformed], &[]);
        assert_eq!(
            Acl::from_bytes(&bytes),
            Err(AclError::Ace(0, AceError::Sid))
        );

        let overrun = vec![0x7f, 0, 12, 0, 0, 0, 0, 0];
        let bytes = acl_with_aces(&[overrun], &[]);
        assert_eq!(
            Acl::from_bytes(&bytes),
            Err(AclError::Ace(0, AceError::Bounds(12)))
        );
    }

    #[test]
    fn represents_loaded_absent_null_and_non_null_components() {
        let unloaded = SecDesc::new(
            SecInfo::empty(),
            0,
            SecDescControl::empty(),
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(!unloaded.owner_loaded());
        assert!(!unloaded.dacl_loaded());

        let absent = SecDesc::new(
            SecInfo::OWNER | SecInfo::DACL,
            0,
            SecDescControl::empty(),
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(absent.owner_loaded());
        assert_eq!(absent.owner(), None);
        assert!(absent.dacl_loaded());
        assert!(!absent.dacl_present());

        let null = SecDesc::new(
            SecInfo::DACL,
            0,
            SecDescControl::DACL_PRESENT,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(null.dacl_present());
        assert_eq!(null.dacl(), None);

        let bytes = valid_acl(8);
        let present = SecDesc::new(
            SecInfo::DACL,
            0,
            SecDescControl::DACL_PRESENT,
            None,
            None,
            Some(bytes.clone()),
            None,
        )
        .unwrap();
        assert_eq!(present.dacl().map(Acl::as_bytes), Some(bytes.as_bytes()));
    }

    #[test]
    fn rejects_inconsistent_components() {
        assert_eq!(
            SecDesc::new(
                SecInfo::empty(),
                0,
                SecDescControl::empty(),
                Some(sid("S-1-5-18")),
                None,
                None,
                None,
            ),
            Err(SecDescError::OwnerNotLoaded)
        );
        assert_eq!(
            SecDesc::new(
                SecInfo::empty(),
                0,
                SecDescControl::empty(),
                None,
                Some(sid("S-1-5-18")),
                None,
                None,
            ),
            Err(SecDescError::GroupNotLoaded)
        );
        assert_eq!(
            SecDesc::new(
                SecInfo::empty(),
                0,
                SecDescControl::DACL_PRESENT,
                None,
                None,
                Some(valid_acl(8)),
                None
            ),
            Err(SecDescError::AclNotLoaded(AclKind::Dacl))
        );
        assert_eq!(
            SecDesc::new(
                SecInfo::DACL,
                0,
                SecDescControl::empty(),
                None,
                None,
                Some(valid_acl(8)),
                None,
            ),
            Err(SecDescError::AclNotPresent(AclKind::Dacl))
        );
    }

    #[test]
    fn validates_acl_packet_and_ace_boundaries() {
        assert_eq!(
            AclBuf::try_from_bytes(vec![0; 4].into_boxed_slice()),
            Err(AclError::Length(4))
        );

        let mut wrong_size = acl(8);
        wrong_size.extend_from_slice(&[0; 4]);
        assert_eq!(
            AclBuf::try_from_bytes(wrong_size.into_boxed_slice()),
            Err(AclError::Size(8, 12))
        );

        let mut opaque = acl(12);
        opaque[8..].copy_from_slice(&[0xff; 4]);
        let opaque = unsafe { AclBuf::from_bytes_unchecked(opaque.into_boxed_slice()) };
        let descriptor = SecDesc::new(
            SecInfo::DACL,
            0,
            SecDescControl::DACL_PRESENT,
            None,
            None,
            Some(opaque.clone()),
            None,
        )
        .unwrap();
        assert_eq!(
            descriptor.dacl().map(Acl::as_bytes),
            Some(opaque.as_bytes())
        );
    }

    #[test]
    fn projects_control_flags_and_normalizes_storage_form() {
        let control = SecDescControl::OWNER_DEFAULTED.bits()
            | SecDescControl::GROUP_DEFAULTED.bits()
            | SecDescControl::DACL_DEFAULTED.bits()
            | SecDescControl::SACL_DEFAULTED.bits()
            | SecDescControl::DACL_AUTO_INHERIT_REQUIRED.bits()
            | SecDescControl::SACL_AUTO_INHERIT_REQUIRED.bits()
            | SecDescControl::DACL_AUTO_INHERITED.bits()
            | SecDescControl::SACL_AUTO_INHERITED.bits()
            | SecDescControl::DACL_PROTECTED.bits()
            | SecDescControl::SACL_PROTECTED.bits()
            | SecDescControl::RM_CONTROL_VALID.bits()
            | SecDescControl::SELF_RELATIVE.bits();
        let descriptor = SecDesc::new(
            SecInfo::empty(),
            0x5a,
            SecDescControl::from_bits_retain(control),
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(descriptor.rm_control(), Some(0x5a));
        assert!(descriptor.owner_defaulted());
        assert!(descriptor.group_defaulted());
        assert!(descriptor.dacl_defaulted());
        assert!(descriptor.sacl_defaulted());
        assert!(descriptor.dacl_auto_inherit_required());
        assert!(descriptor.sacl_auto_inherit_required());
        assert!(descriptor.dacl_auto_inherited());
        assert!(descriptor.sacl_auto_inherited());
        assert!(descriptor.dacl_protected());
        assert!(descriptor.sacl_protected());
        assert!(!descriptor.control().contains(SecDescControl::SELF_RELATIVE));

        let descriptor = SecDesc::new(
            SecInfo::empty(),
            0x5a,
            SecDescControl::empty(),
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(descriptor.rm_control(), None);
    }

    #[test]
    fn serde_is_structural_and_validated() {
        let owner = sid("S-1-5-18");
        let dacl = valid_acl(8);
        let descriptor = SecDesc::new(
            SecInfo::OWNER | SecInfo::DACL,
            0x42,
            SecDescControl::DACL_PRESENT | SecDescControl::RM_CONTROL_VALID,
            Some(owner.clone()),
            None,
            Some(dacl.clone()),
            None,
        )
        .unwrap();
        let encoded = postcard::to_stdvec(&descriptor).unwrap();
        let expected = postcard::to_stdvec(&(
            (SecInfo::OWNER | SecInfo::DACL).bits(),
            1u8,
            0x42u8,
            SecDescControl::DACL_PRESENT.bits() | SecDescControl::RM_CONTROL_VALID.bits(),
            Some(owner),
            Option::<Sid>::None,
            Some(dacl),
            Option::<Vec<u8>>::None,
        ))
        .unwrap();
        assert_eq!(encoded, expected);
        assert_eq!(
            postcard::from_bytes::<SecDesc>(&encoded).unwrap(),
            descriptor
        );

        let malformed = postcard::to_stdvec(&(
            0u32,
            2u8,
            0u8,
            0u16,
            Option::<Sid>::None,
            Option::<Sid>::None,
            Option::<Vec<u8>>::None,
            Option::<Vec<u8>>::None,
        ))
        .unwrap();
        assert!(postcard::from_bytes::<SecDesc>(&malformed).is_err());
    }

    #[test]
    fn flag_and_revision_serde_preserve_native_values() {
        let info = SecInfo::OWNER | SecInfo::DACL;
        let control = SecDescControl::DACL_PRESENT | SecDescControl::DACL_PROTECTED;
        assert_eq!(
            postcard::to_stdvec(&info).unwrap(),
            postcard::to_stdvec(&info.bits()).unwrap()
        );
        assert_eq!(
            postcard::to_stdvec(&control).unwrap(),
            postcard::to_stdvec(&control.bits()).unwrap()
        );
        assert_eq!(postcard::to_stdvec(&Revision::One).unwrap(), vec![REVISION]);
        assert!(postcard::from_bytes::<Revision>(&[2]).is_err());

        for flags in [
            AceFlags::OBJECT_INHERIT | AceFlags::FAILED_ACCESS,
            AceFlags::from_bits_retain(0xff),
        ] {
            let encoded = postcard::to_stdvec(&flags).unwrap();
            assert_eq!(encoded, postcard::to_stdvec(&flags.bits()).unwrap());
            assert_eq!(postcard::from_bytes::<AceFlags>(&encoded).unwrap(), flags);
        }
        let object_flags = ObjectAceFlags::from_bits_retain(0x8000_0001);
        let encoded = postcard::to_stdvec(&object_flags).unwrap();
        assert_eq!(encoded, postcard::to_stdvec(&object_flags.bits()).unwrap());
        assert_eq!(
            postcard::from_bytes::<ObjectAceFlags>(&encoded).unwrap(),
            object_flags
        );

        for ace_type in [AceType::AccessAllowed, AceType::Unknown(0xfe)] {
            let encoded = postcard::to_stdvec(&ace_type).unwrap();
            assert_eq!(encoded, vec![u8::from(ace_type)]);
            assert_eq!(postcard::from_bytes::<AceType>(&encoded).unwrap(), ace_type);
        }
        for revision in [
            AclRevision::Basic,
            AclRevision::DirectoryService,
            AclRevision::Unknown(3),
        ] {
            let encoded = postcard::to_stdvec(&revision).unwrap();
            assert_eq!(encoded, vec![u8::from(revision)]);
            assert_eq!(
                postcard::from_bytes::<AclRevision>(&encoded).unwrap(),
                revision
            );
        }
    }

    #[test]
    fn self_relative_packet_round_trip() {
        let packet = [
            0x01, 0x5a, 0x15, 0xd0, 0x14, 0x00, 0x00, 0x00, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x30, 0x00, 0x00, 0x00, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05,
            0x12, 0x00, 0x00, 0x00, 0x01, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x20, 0x00,
            0x00, 0x00, 0x20, 0x02, 0x00, 0x00, 0x02, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let descriptor = SecDesc::from_bytes(&packet).unwrap();
        assert_eq!(descriptor.mask(), SecInfo::ALL);
        assert_eq!(descriptor.control().bits(), 0x5015);
        assert_eq!(descriptor.rm_control(), Some(0x5a));
        assert_eq!(descriptor.owner().unwrap().to_string(), "S-1-5-18");
        assert_eq!(descriptor.group().unwrap().to_string(), "S-1-5-32-544");
        assert!(descriptor.sacl_present());
        assert_eq!(descriptor.sacl(), None);
        assert_eq!(descriptor.dacl().map(Acl::as_bytes), Some(&packet[48..]));
        assert_eq!(descriptor.to_bytes(), packet);
    }

    #[test]
    fn self_relative_parser_tracks_selected_components() {
        let descriptor = SecDesc::new(
            SecInfo::ALL,
            0,
            SecDescControl::DACL_PRESENT | SecDescControl::DACL_PROTECTED,
            Some(sid("S-1-5-18")),
            Some(sid("S-1-5-32-544")),
            Some(valid_acl(8)),
            None,
        )
        .unwrap();
        let packet = descriptor.to_bytes();

        let selected = SecDesc::from_bytes_with_mask(&packet, SecInfo::DACL).unwrap();
        assert_eq!(selected.mask(), SecInfo::DACL);
        assert!(!selected.owner_loaded());
        assert_eq!(selected.owner(), None);
        assert!(selected.dacl_loaded());
        assert_eq!(
            selected.dacl().map(Acl::as_bytes),
            Some(valid_acl(8).as_bytes())
        );
        assert!(selected.dacl_protected());

        let empty = SecDesc::from_bytes_with_mask(&packet, SecInfo::empty()).unwrap();
        assert_eq!(empty.mask(), SecInfo::empty());
        assert_eq!(
            empty.control(),
            SecDescControl::DACL_PRESENT | SecDescControl::DACL_PROTECTED
        );
        assert_eq!(empty.owner(), None);
        assert_eq!(empty.dacl(), None);
    }

    #[test]
    fn self_relative_packet_writer_uses_canonical_component_order() {
        let descriptor = SecDesc::new(
            SecInfo::ALL,
            0,
            SecDescControl::DACL_PRESENT,
            Some(sid("S-1-5-18")),
            Some(sid("S-1-5-32-544")),
            Some(valid_acl(8)),
            None,
        )
        .unwrap();
        let packet = descriptor.to_bytes();
        assert_eq!(u32::from_le_bytes(packet[4..8].try_into().unwrap()), 20);
        assert_eq!(u32::from_le_bytes(packet[8..12].try_into().unwrap()), 32);
        assert_eq!(u32::from_le_bytes(packet[12..16].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(packet[16..20].try_into().unwrap()), 48);
        assert_eq!(SecDesc::from_bytes(&packet).unwrap(), descriptor);
    }

    #[test]
    fn rejects_malformed_self_relative_packets() {
        assert_eq!(
            SecDesc::from_bytes(&[0; SELF_RELATIVE_HEADER_LEN - 1]),
            Err(SecDescError::PacketLength)
        );

        let mut packet = [0; SELF_RELATIVE_HEADER_LEN];
        packet[0] = 1;
        assert_eq!(
            SecDesc::from_bytes(&packet),
            Err(SecDescError::NotSelfRelative)
        );

        packet[2..4].copy_from_slice(&SecDescControl::SELF_RELATIVE.bits().to_le_bytes());
        packet[4..8].copy_from_slice(&4u32.to_le_bytes());
        assert_eq!(
            SecDesc::from_bytes(&packet),
            Err(SecDescError::PacketOffset(SecDescComponent::Owner, 4))
        );

        packet[4..8].copy_from_slice(&20u32.to_le_bytes());
        assert_eq!(
            SecDesc::from_bytes(&packet),
            Err(SecDescError::PacketOffset(SecDescComponent::Owner, 20))
        );
    }

    #[test]
    fn owned_ace_builders_select_layouts_and_pad_application_data() {
        let trustee = sid("S-1-1-0");
        let object_type: Guid = "00112233-4455-6677-8899-aabbccddeeff".parse().unwrap();
        let basic = AceBuf::allow(
            &trustee,
            AccessMask::from_bits_retain(0x1234),
            AceBuildOptions::new()
                .flags(AceFlags::from_bits_retain(0x03))
                .application_data([1, 2, 3]),
        )
        .unwrap();
        assert_eq!(basic.ace_type(), AceType::AccessAllowed);
        assert_eq!(basic.flags().bits(), 0x03);
        assert_eq!(basic.application_data(), Some(&[1, 2, 3, 0][..]));
        assert_eq!(Ace::from_bytes(basic.as_bytes()).unwrap(), &*basic);

        let object = AceBuf::deny(
            &trustee,
            AccessMask::from_bits_retain(u32::MAX),
            AceBuildOptions::new().object_type(object_type).callback(),
        )
        .unwrap();
        assert_eq!(object.ace_type(), AceType::AccessDeniedCallbackObject);
        assert_eq!(
            object.object_flags(),
            Some(ObjectAceFlags::OBJECT_TYPE_PRESENT)
        );
        assert_eq!(object.object_type(), Some(object_type));
        assert_eq!(object.inherited_object_type(), None);
    }

    #[test]
    fn audit_builder_enforces_outcomes_and_reserves_audit_flags() {
        let trustee = sid("S-1-5-18");
        assert_eq!(
            AceBuf::audit(
                &trustee,
                AccessMask::from_specific_rights(1),
                false,
                false,
                AceBuildOptions::new()
            ),
            Err(AceBuildError::AuditOutcome)
        );
        assert_eq!(
            AceBuf::audit(
                &trustee,
                AccessMask::from_specific_rights(1),
                true,
                false,
                AceBuildOptions::new().flags(AceFlags::SUCCESSFUL_ACCESS),
            ),
            Err(AceBuildError::AuditFlags)
        );
        let audit = AceBuf::audit(
            &trustee,
            AccessMask::from_specific_rights(1),
            true,
            true,
            AceBuildOptions::new(),
        )
        .unwrap();
        assert_eq!(audit.ace_type(), AceType::SystemAudit);
        assert_eq!(audit.flags().bits(), 0xc0);
    }

    #[test]
    fn acl_builder_preserves_packets_and_selects_revision() {
        let trustee = sid("S-1-1-0");
        let basic = AceBuf::allow(
            &trustee,
            AccessMask::from_specific_rights(1),
            AceBuildOptions::new(),
        )
        .unwrap();
        let object = AceBuf::allow(
            &trustee,
            AccessMask::from_specific_rights(2),
            AceBuildOptions::new()
                .object_type("00000000-0000-0000-0000-000000000000".parse().unwrap()),
        )
        .unwrap();
        let acl = AclBuf::from_aces([&*basic], None).unwrap();
        assert_eq!(acl.revision(), AclRevision::Basic);
        assert_eq!(acl.aces().next().unwrap().as_bytes(), basic.as_bytes());

        let acl = AclBuf::from_aces([&*basic, &*object], None).unwrap();
        assert_eq!(acl.revision(), AclRevision::DirectoryService);
        assert_eq!(
            AclBuf::from_aces([&*object], Some(AclRevision::Basic)),
            Err(AclBuildError::ObjectRevision)
        );
        assert_eq!(
            AclBuf::from_aces([&*basic], Some(AclRevision::Unknown(3))),
            Err(AclBuildError::Revision(3))
        );
        assert_eq!(Acl::from_bytes(acl.as_bytes()).unwrap(), &*acl);
    }

    #[test]
    fn owned_packets_validate_raw_and_serde_inputs() {
        let trustee = sid("S-1-1-0");
        let ace = AceBuf::allow(
            &trustee,
            AccessMask::from_specific_rights(1),
            AceBuildOptions::new(),
        )
        .unwrap();
        let encoded = postcard::to_stdvec(&ace).unwrap();
        assert_eq!(postcard::from_bytes::<AceBuf>(&encoded).unwrap(), ace);
        assert!(AceBuf::try_from_bytes(vec![0, 0, 4, 0].into_boxed_slice()).is_err());

        let acl = AclBuf::from_aces([&*ace], None).unwrap();
        let encoded = postcard::to_stdvec(&acl).unwrap();
        assert_eq!(postcard::from_bytes::<AclBuf>(&encoded).unwrap(), acl);
        let mut malformed = acl.as_bytes().to_vec();
        malformed[2] = 0;
        assert!(AclBuf::try_from_bytes(malformed.into_boxed_slice()).is_err());
    }

    #[test]
    fn functional_updates_cover_component_states_and_controls() {
        let descriptor = SecDesc::new(
            SecInfo::empty(),
            0,
            SecDescControl::empty(),
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let concrete = AclBuf::from_aces(std::iter::empty::<&Ace>(), None).unwrap();
        let updated = descriptor
            .with(
                SecDescUpdate::new()
                    .owner(Some(sid("S-1-5-18")))
                    .dacl(Some(concrete.clone()))
                    .owner_defaulted(true)
                    .dacl_protected(true)
                    .rm_control(Some(0x5a)),
            )
            .unwrap();
        assert!(!descriptor.owner_loaded());
        assert_eq!(updated.owner().unwrap().to_string(), "S-1-5-18");
        assert_eq!(updated.dacl(), Some(&*concrete));
        assert!(updated.dacl_present());
        assert!(updated.owner_defaulted());
        assert!(updated.dacl_protected());
        assert_eq!(updated.rm_control(), Some(0x5a));

        let null = updated
            .with(SecDescUpdate::new().dacl(None).rm_control(None))
            .unwrap();
        assert!(null.dacl_present());
        assert_eq!(null.dacl(), None);
        assert_eq!(null.rm_control(), None);

        let absent = null.with(SecDescUpdate::new().dacl_present(false)).unwrap();
        assert!(absent.dacl_loaded());
        assert!(!absent.dacl_present());
        assert_eq!(
            descriptor.with(SecDescUpdate::new().dacl_present(true)),
            Err(SecDescError::AclPresenceRequiresValue(AclKind::Dacl))
        );
        let unloaded_present = SecDesc::new(
            SecInfo::empty(),
            0,
            SecDescControl::DACL_PRESENT,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            unloaded_present.with(SecDescUpdate::new().dacl_present(true)),
            Err(SecDescError::AclPresenceRequiresValue(AclKind::Dacl))
        );
        assert_eq!(
            descriptor.with(SecDescUpdate::new().dacl_protected(true)),
            Err(SecDescError::ComponentNotLoaded(SecDescComponent::Dacl))
        );
    }
}
