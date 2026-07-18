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

use crate::{Guid, Sid};

const REVISION: u8 = 1;

pub const OWNER_SECURITY_INFORMATION: u32 = 0x0000_0001;
pub const GROUP_SECURITY_INFORMATION: u32 = 0x0000_0002;
pub const DACL_SECURITY_INFORMATION: u32 = 0x0000_0004;
pub const SACL_SECURITY_INFORMATION: u32 = 0x0000_0008;
pub const ALL_SECURITY_INFORMATION: u32 = OWNER_SECURITY_INFORMATION
    | GROUP_SECURITY_INFORMATION
    | DACL_SECURITY_INFORMATION
    | SACL_SECURITY_INFORMATION;

const SE_OWNER_DEFAULTED: u16 = 0x0001;
const SE_GROUP_DEFAULTED: u16 = 0x0002;
const SE_DACL_PRESENT: u16 = 0x0004;
const SE_DACL_DEFAULTED: u16 = 0x0008;
const SE_SACL_PRESENT: u16 = 0x0010;
const SE_SACL_DEFAULTED: u16 = 0x0020;
const SE_DACL_AUTO_INHERIT_REQ: u16 = 0x0100;
const SE_SACL_AUTO_INHERIT_REQ: u16 = 0x0200;
const SE_DACL_AUTO_INHERITED: u16 = 0x0400;
const SE_SACL_AUTO_INHERITED: u16 = 0x0800;
const SE_DACL_PROTECTED: u16 = 0x1000;
const SE_SACL_PROTECTED: u16 = 0x2000;
const SE_RM_CONTROL_VALID: u16 = 0x4000;
const SE_SELF_RELATIVE: u16 = 0x8000;

const ACL_HEADER_LEN: usize = 8;
const ACE_HEADER_LEN: usize = 4;
const SELF_RELATIVE_HEADER_LEN: usize = 20;

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
    pub const fn revision(&self) -> u8 {
        self.0[0]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AceType {
    AccessAllowed,
    AccessDenied,
    SystemAudit,
    SystemAlarm,
    AccessAllowedCompound,
    AccessAllowedObject,
    AccessDeniedObject,
    SystemAuditObject,
    SystemAlarmObject,
    AccessAllowedCallback,
    AccessDeniedCallback,
    AccessAllowedCallbackObject,
    AccessDeniedCallbackObject,
    SystemAuditCallback,
    SystemAlarmCallback,
    SystemAuditCallbackObject,
    SystemAlarmCallbackObject,
    SystemMandatoryLabel,
    SystemResourceAttribute,
    SystemScopedPolicyId,
    SystemProcessTrustLabel,
    SystemAccessFilter,
    Unknown(u8),
}

/// An immutable borrowed native Windows access-control entry (ACE).
#[repr(transparent)]
pub struct Ace([u8]);

#[derive(Debug)]
struct AceBody {
    mask: u32,
    sid: Sid,
    object_flags: Option<u32>,
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
    pub const fn ace_type(&self) -> AceType {
        match self.type_code() {
            0 => AceType::AccessAllowed,
            1 => AceType::AccessDenied,
            2 => AceType::SystemAudit,
            3 => AceType::SystemAlarm,
            4 => AceType::AccessAllowedCompound,
            5 => AceType::AccessAllowedObject,
            6 => AceType::AccessDeniedObject,
            7 => AceType::SystemAuditObject,
            8 => AceType::SystemAlarmObject,
            9 => AceType::AccessAllowedCallback,
            10 => AceType::AccessDeniedCallback,
            11 => AceType::AccessAllowedCallbackObject,
            12 => AceType::AccessDeniedCallbackObject,
            13 => AceType::SystemAuditCallback,
            14 => AceType::SystemAlarmCallback,
            15 => AceType::SystemAuditCallbackObject,
            16 => AceType::SystemAlarmCallbackObject,
            17 => AceType::SystemMandatoryLabel,
            18 => AceType::SystemResourceAttribute,
            19 => AceType::SystemScopedPolicyId,
            20 => AceType::SystemProcessTrustLabel,
            21 => AceType::SystemAccessFilter,
            code => AceType::Unknown(code),
        }
    }

    /// Returns the native ACE flags byte.
    pub const fn flags(&self) -> u8 {
        self.0[1]
    }

    /// Returns the declared ACE size.
    pub fn size(&self) -> u16 {
        u16::from_le_bytes(self.0[2..4].try_into().unwrap())
    }

    /// Returns the access mask for ACE layouts that contain one.
    pub fn mask(&self) -> Option<u32> {
        self.body().map(|body| body.mask)
    }

    /// Returns the trustee SID for ACE layouts that contain one.
    pub fn sid(&self) -> Option<Sid> {
        self.body().map(|body| body.sid)
    }

    /// Returns object-specific flags for object ACE layouts.
    pub fn object_flags(&self) -> Option<u32> {
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
        let mask = read_u32(&self.0, 4)?;
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
        let mask = read_u32(&self.0, 4)?;
        let object_flags = read_u32(&self.0, 8)?;
        let mut offset = 12;
        let object_type = if object_flags & 1 != 0 {
            let value = Guid::from_bytes(self.0.get(offset..offset + 16).ok_or(AceError::Body)?)
                .map_err(|_| AceError::Body)?;
            offset += 16;
            Some(value)
        } else {
            None
        };
        let inherited_object_type = if object_flags & 2 != 0 {
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
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AceBuildOptions {
    pub flags: u8,
    pub object_type: Option<Guid>,
    pub inherited_object_type: Option<Guid>,
    pub callback: bool,
    pub application_data: Vec<u8>,
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
    pub fn allow(sid: &Sid, mask: u32, options: AceBuildOptions) -> Result<Self, AceBuildError> {
        Self::build(AceFamily::Allow, sid, mask, false, false, options)
    }

    /// Builds an access-denied ACE.
    pub fn deny(sid: &Sid, mask: u32, options: AceBuildOptions) -> Result<Self, AceBuildError> {
        Self::build(AceFamily::Deny, sid, mask, false, false, options)
    }

    /// Builds a system-audit ACE.
    pub fn audit(
        sid: &Sid,
        mask: u32,
        successful: bool,
        failed: bool,
        options: AceBuildOptions,
    ) -> Result<Self, AceBuildError> {
        if !successful && !failed {
            return Err(AceBuildError::AuditOutcome);
        }
        if options.flags & 0xc0 != 0 {
            return Err(AceBuildError::AuditFlags);
        }
        Self::build(AceFamily::Audit, sid, mask, successful, failed, options)
    }

    fn build(
        family: AceFamily,
        sid: &Sid,
        mask: u32,
        successful: bool,
        failed: bool,
        options: AceBuildOptions,
    ) -> Result<Self, AceBuildError> {
        let object = options.object_type.is_some() || options.inherited_object_type.is_some();
        let type_code = match (family, options.callback, object) {
            (AceFamily::Allow, false, false) => 0,
            (AceFamily::Deny, false, false) => 1,
            (AceFamily::Audit, false, false) => 2,
            (AceFamily::Allow, false, true) => 5,
            (AceFamily::Deny, false, true) => 6,
            (AceFamily::Audit, false, true) => 7,
            (AceFamily::Allow, true, false) => 9,
            (AceFamily::Deny, true, false) => 10,
            (AceFamily::Allow, true, true) => 11,
            (AceFamily::Deny, true, true) => 12,
            (AceFamily::Audit, true, false) => 13,
            (AceFamily::Audit, true, true) => 15,
        };
        let mut flags = options.flags;
        if successful {
            flags |= 0x40;
        }
        if failed {
            flags |= 0x80;
        }

        let mut bytes = vec![type_code, flags, 0, 0];
        bytes.extend_from_slice(&mask.to_le_bytes());
        if object {
            let object_flags = u32::from(options.object_type.is_some())
                | (u32::from(options.inherited_object_type.is_some()) << 1);
            bytes.extend_from_slice(&object_flags.to_le_bytes());
            if let Some(value) = options.object_type {
                bytes.extend_from_slice(value.as_bytes());
            }
            if let Some(value) = options.inherited_object_type {
                bytes.extend_from_slice(value.as_bytes());
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

    /// Builds an ACL from already-validated ACE packets.
    pub fn from_aces<I, A>(aces: I, revision: Option<u8>) -> Result<Self, AclBuildError>
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
        let revision = revision.unwrap_or(if has_object { 4 } else { 2 });
        if !matches!(revision, 2 | 4) {
            return Err(AclBuildError::Revision(revision));
        }
        if revision == 2 && has_object {
            return Err(AclBuildError::ObjectRevision);
        }

        let mut bytes = Vec::with_capacity(size);
        bytes.extend_from_slice(&[revision, 0]);
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
    AuditOutcome,
    AuditFlags,
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
    Revision(u8),
    ObjectRevision,
    Count(usize),
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
    Length(usize),
    Size(u16, usize),
    AceCount(u16, usize),
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
    Length(usize),
    Size(u16, usize),
    Alignment(usize),
    Bounds(usize),
    Body,
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

/// A portable representation of a Windows security descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecDesc {
    mask: u32,
    revision: u8,
    rm_control: u8,
    control: u16,
    owner: Option<Sid>,
    group: Option<Sid>,
    dacl: Option<AclBuf>,
    sacl: Option<AclBuf>,
}

/// A functional update to a [`SecDesc`].
#[derive(Clone, Debug, Default)]
pub struct SecDescUpdate {
    pub owner: Option<Option<Sid>>,
    pub group: Option<Option<Sid>>,
    pub dacl: Option<Option<AclBuf>>,
    pub sacl: Option<Option<AclBuf>>,
    pub owner_defaulted: Option<bool>,
    pub group_defaulted: Option<bool>,
    pub dacl_present: Option<bool>,
    pub dacl_defaulted: Option<bool>,
    pub dacl_auto_inherit_required: Option<bool>,
    pub dacl_auto_inherited: Option<bool>,
    pub dacl_protected: Option<bool>,
    pub sacl_present: Option<bool>,
    pub sacl_defaulted: Option<bool>,
    pub sacl_auto_inherit_required: Option<bool>,
    pub sacl_auto_inherited: Option<bool>,
    pub sacl_protected: Option<bool>,
    pub rm_control: Option<Option<u8>>,
}

impl SecDesc {
    /// Creates a security descriptor from its structural components.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        mask: u32,
        revision: u8,
        rm_control: u8,
        control: u16,
        owner: Option<Sid>,
        group: Option<Sid>,
        dacl: Option<Vec<u8>>,
        sacl: Option<Vec<u8>>,
    ) -> Result<Self, SecDescError> {
        if revision != REVISION {
            return Err(SecDescError::Revision(revision));
        }
        if mask & OWNER_SECURITY_INFORMATION == 0 && owner.is_some() {
            return Err(SecDescError::OwnerNotLoaded);
        }
        if mask & GROUP_SECURITY_INFORMATION == 0 && group.is_some() {
            return Err(SecDescError::GroupNotLoaded);
        }
        let dacl = validate_acl(
            "DACL",
            mask & DACL_SECURITY_INFORMATION != 0,
            control & SE_DACL_PRESENT != 0,
            dacl,
        )?;
        let sacl = validate_acl(
            "SACL",
            mask & SACL_SECURITY_INFORMATION != 0,
            control & SE_SACL_PRESENT != 0,
            sacl,
        )?;

        Ok(Self {
            mask,
            revision,
            rm_control,
            control: control & !SE_SELF_RELATIVE,
            owner,
            group,
            dacl,
            sacl,
        })
    }

    /// Parses a self-relative Windows security descriptor packet.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, SecDescError> {
        Self::from_bytes_with_mask(bytes, ALL_SECURITY_INFORMATION)
    }

    /// Parses the selected components of a self-relative Windows security descriptor packet.
    pub fn from_bytes_with_mask(bytes: &[u8], mask: u32) -> Result<Self, SecDescError> {
        if bytes.len() < SELF_RELATIVE_HEADER_LEN {
            return Err(SecDescError::PacketLength);
        }
        let revision = bytes[0];
        let rm_control = bytes[1];
        let control = u16::from_le_bytes(bytes[2..4].try_into().unwrap());
        if control & SE_SELF_RELATIVE == 0 {
            return Err(SecDescError::NotSelfRelative);
        }

        let owner_offset = packet_offset(bytes, 4)?;
        let group_offset = packet_offset(bytes, 8)?;
        let sacl_offset = packet_offset(bytes, 12)?;
        let dacl_offset = packet_offset(bytes, 16)?;
        if control & SE_SACL_PRESENT == 0 && sacl_offset != 0 {
            return Err(SecDescError::AclNotPresent("SACL"));
        }
        if control & SE_DACL_PRESENT == 0 && dacl_offset != 0 {
            return Err(SecDescError::AclNotPresent("DACL"));
        }
        let owner = (mask & OWNER_SECURITY_INFORMATION != 0)
            .then(|| parse_sid(bytes, owner_offset, "owner"))
            .transpose()?
            .flatten();
        let group = (mask & GROUP_SECURITY_INFORMATION != 0)
            .then(|| parse_sid(bytes, group_offset, "group"))
            .transpose()?
            .flatten();
        let sacl = (mask & SACL_SECURITY_INFORMATION != 0)
            .then(|| parse_acl(bytes, sacl_offset, "SACL"))
            .transpose()?
            .flatten();
        let dacl = (mask & DACL_SECURITY_INFORMATION != 0)
            .then(|| parse_acl(bytes, dacl_offset, "DACL"))
            .transpose()?
            .flatten();

        Self::new(
            mask, revision, rm_control, control, owner, group, dacl, sacl,
        )
    }

    /// Converts this descriptor to a canonical self-relative Windows packet.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = vec![0; SELF_RELATIVE_HEADER_LEN];
        bytes[0] = self.revision;
        bytes[1] = self.rm_control;
        bytes[2..4].copy_from_slice(&(self.control | SE_SELF_RELATIVE).to_le_bytes());

        let owner = self.owner.as_ref().map(Sid::to_bytes);
        let group = self.group.as_ref().map(Sid::to_bytes);
        append_component(&mut bytes, 4, owner.as_deref());
        append_component(&mut bytes, 8, group.as_deref());
        append_component(&mut bytes, 12, self.sacl.as_deref().map(Acl::as_bytes));
        append_component(&mut bytes, 16, self.dacl.as_deref().map(Acl::as_bytes));
        bytes
    }

    /// Returns the native SECURITY_INFORMATION mask associated with the descriptor.
    pub const fn mask(&self) -> u32 {
        self.mask
    }

    /// Returns the security descriptor revision.
    pub const fn revision(&self) -> u8 {
        self.revision
    }

    /// Returns the security descriptor control mask.
    pub const fn control(&self) -> u16 {
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
        self.control & SE_RM_CONTROL_VALID != 0
    }

    /// Returns whether the owner component was loaded.
    pub const fn owner_loaded(&self) -> bool {
        self.mask & OWNER_SECURITY_INFORMATION != 0
    }

    /// Returns the owner SID, if present.
    pub const fn owner(&self) -> Option<&Sid> {
        self.owner.as_ref()
    }

    /// Returns whether the owner SID was supplied by a default mechanism.
    pub const fn owner_defaulted(&self) -> bool {
        self.control & SE_OWNER_DEFAULTED != 0
    }

    /// Returns whether the group component was loaded.
    pub const fn group_loaded(&self) -> bool {
        self.mask & GROUP_SECURITY_INFORMATION != 0
    }

    /// Returns the primary group SID, if present.
    pub const fn group(&self) -> Option<&Sid> {
        self.group.as_ref()
    }

    /// Returns whether the group SID was supplied by a default mechanism.
    pub const fn group_defaulted(&self) -> bool {
        self.control & SE_GROUP_DEFAULTED != 0
    }

    /// Returns whether the DACL component was loaded.
    pub const fn dacl_loaded(&self) -> bool {
        self.mask & DACL_SECURITY_INFORMATION != 0
    }

    /// Returns the DACL, if it is non-null.
    pub fn dacl(&self) -> Option<&Acl> {
        self.dacl.as_deref()
    }

    /// Returns whether the descriptor marks the DACL as present.
    pub const fn dacl_present(&self) -> bool {
        self.control & SE_DACL_PRESENT != 0
    }

    /// Returns whether the DACL was supplied by a default mechanism.
    pub const fn dacl_defaulted(&self) -> bool {
        self.control & SE_DACL_DEFAULTED != 0
    }

    /// Returns whether DACL inheritance computation was requested.
    pub const fn dacl_auto_inherit_required(&self) -> bool {
        self.control & SE_DACL_AUTO_INHERIT_REQ != 0
    }

    /// Returns whether the DACL was produced through inheritance.
    pub const fn dacl_auto_inherited(&self) -> bool {
        self.control & SE_DACL_AUTO_INHERITED != 0
    }

    /// Returns whether the DACL is protected from inheritance.
    pub const fn dacl_protected(&self) -> bool {
        self.control & SE_DACL_PROTECTED != 0
    }

    /// Returns whether the SACL component was loaded.
    pub const fn sacl_loaded(&self) -> bool {
        self.mask & SACL_SECURITY_INFORMATION != 0
    }

    /// Returns the SACL, if it is non-null.
    pub fn sacl(&self) -> Option<&Acl> {
        self.sacl.as_deref()
    }

    /// Returns whether the descriptor marks the SACL as present.
    pub const fn sacl_present(&self) -> bool {
        self.control & SE_SACL_PRESENT != 0
    }

    /// Returns whether the SACL was supplied by a default mechanism.
    pub const fn sacl_defaulted(&self) -> bool {
        self.control & SE_SACL_DEFAULTED != 0
    }

    /// Returns whether SACL inheritance computation was requested.
    pub const fn sacl_auto_inherit_required(&self) -> bool {
        self.control & SE_SACL_AUTO_INHERIT_REQ != 0
    }

    /// Returns whether the SACL was produced through inheritance.
    pub const fn sacl_auto_inherited(&self) -> bool {
        self.control & SE_SACL_AUTO_INHERITED != 0
    }

    /// Returns whether the SACL is protected from inheritance.
    pub const fn sacl_protected(&self) -> bool {
        self.control & SE_SACL_PROTECTED != 0
    }

    /// Returns a new descriptor with the supplied component and control updates.
    pub fn with(&self, update: SecDescUpdate) -> Result<Self, SecDescError> {
        let mut mask = self.mask;
        let mut control = self.control;

        let owner = match update.owner {
            Some(value) => {
                mask |= OWNER_SECURITY_INFORMATION;
                value
            }
            None => self.owner.clone(),
        };
        let group = match update.group {
            Some(value) => {
                mask |= GROUP_SECURITY_INFORMATION;
                value
            }
            None => self.group.clone(),
        };

        let (dacl, dacl_explicit) = match update.dacl {
            Some(value) => {
                mask |= DACL_SECURITY_INFORMATION;
                set_control(&mut control, SE_DACL_PRESENT, true);
                (value, true)
            }
            None => (self.dacl.clone(), false),
        };
        let (sacl, sacl_explicit) = match update.sacl {
            Some(value) => {
                mask |= SACL_SECURITY_INFORMATION;
                set_control(&mut control, SE_SACL_PRESENT, true);
                (value, true)
            }
            None => (self.sacl.clone(), false),
        };

        let dacl = apply_presence(
            "DACL",
            &mut mask,
            &mut control,
            DACL_SECURITY_INFORMATION,
            SE_DACL_PRESENT,
            update.dacl_present,
            dacl_explicit,
            dacl,
        )?;
        let sacl = apply_presence(
            "SACL",
            &mut mask,
            &mut control,
            SACL_SECURITY_INFORMATION,
            SE_SACL_PRESENT,
            update.sacl_present,
            sacl_explicit,
            sacl,
        )?;

        apply_component_flag(
            "owner",
            mask & OWNER_SECURITY_INFORMATION != 0,
            &mut control,
            SE_OWNER_DEFAULTED,
            update.owner_defaulted,
        )?;
        apply_component_flag(
            "group",
            mask & GROUP_SECURITY_INFORMATION != 0,
            &mut control,
            SE_GROUP_DEFAULTED,
            update.group_defaulted,
        )?;
        for (name, loaded, flag, value) in [
            (
                "DACL",
                mask & DACL_SECURITY_INFORMATION != 0,
                SE_DACL_DEFAULTED,
                update.dacl_defaulted,
            ),
            (
                "DACL",
                mask & DACL_SECURITY_INFORMATION != 0,
                SE_DACL_AUTO_INHERIT_REQ,
                update.dacl_auto_inherit_required,
            ),
            (
                "DACL",
                mask & DACL_SECURITY_INFORMATION != 0,
                SE_DACL_AUTO_INHERITED,
                update.dacl_auto_inherited,
            ),
            (
                "DACL",
                mask & DACL_SECURITY_INFORMATION != 0,
                SE_DACL_PROTECTED,
                update.dacl_protected,
            ),
            (
                "SACL",
                mask & SACL_SECURITY_INFORMATION != 0,
                SE_SACL_DEFAULTED,
                update.sacl_defaulted,
            ),
            (
                "SACL",
                mask & SACL_SECURITY_INFORMATION != 0,
                SE_SACL_AUTO_INHERIT_REQ,
                update.sacl_auto_inherit_required,
            ),
            (
                "SACL",
                mask & SACL_SECURITY_INFORMATION != 0,
                SE_SACL_AUTO_INHERITED,
                update.sacl_auto_inherited,
            ),
            (
                "SACL",
                mask & SACL_SECURITY_INFORMATION != 0,
                SE_SACL_PROTECTED,
                update.sacl_protected,
            ),
        ] {
            apply_component_flag(name, loaded, &mut control, flag, value)?;
        }

        let rm_control = match update.rm_control {
            Some(Some(value)) => {
                set_control(&mut control, SE_RM_CONTROL_VALID, true);
                value
            }
            Some(None) => {
                set_control(&mut control, SE_RM_CONTROL_VALID, false);
                0
            }
            None => self.rm_control,
        };

        Ok(Self {
            mask,
            revision: self.revision,
            rm_control,
            control,
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

fn apply_component_flag(
    name: &'static str,
    loaded: bool,
    control: &mut u16,
    flag: u16,
    value: Option<bool>,
) -> Result<(), SecDescError> {
    if let Some(value) = value {
        if !loaded {
            return Err(SecDescError::ComponentNotLoaded(name));
        }
        set_control(control, flag, value);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn apply_presence(
    name: &'static str,
    mask: &mut u32,
    control: &mut u16,
    mask_flag: u32,
    present_flag: u16,
    requested: Option<bool>,
    explicit: bool,
    mut acl: Option<AclBuf>,
) -> Result<Option<AclBuf>, SecDescError> {
    if let Some(present) = requested {
        let was_loaded = *mask & mask_flag != 0;
        if !present {
            if explicit {
                return Err(SecDescError::AclPresenceConflict(name));
            }
            acl = None;
            set_control(control, present_flag, false);
        } else {
            if !explicit && (!was_loaded || *control & present_flag == 0) {
                return Err(SecDescError::AclPresenceRequiresValue(name));
            }
            set_control(control, present_flag, true);
        }
        *mask |= mask_flag;
    }
    Ok(acl)
}

fn packet_offset(bytes: &[u8], at: usize) -> Result<usize, SecDescError> {
    let offset = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    usize::try_from(offset).map_err(|_| SecDescError::PacketOffset("component", offset))
}

fn validate_offset(bytes: &[u8], offset: usize, name: &'static str) -> Result<(), SecDescError> {
    if offset < SELF_RELATIVE_HEADER_LEN || !offset.is_multiple_of(4) || offset >= bytes.len() {
        return Err(SecDescError::PacketOffset(
            name,
            u32::try_from(offset).unwrap_or(u32::MAX),
        ));
    }
    Ok(())
}

fn parse_sid(bytes: &[u8], offset: usize, name: &'static str) -> Result<Option<Sid>, SecDescError> {
    if offset == 0 {
        return Ok(None);
    }
    validate_offset(bytes, offset, name)?;
    let header = bytes
        .get(offset..offset + 8)
        .ok_or(SecDescError::PacketComponent(name))?;
    let length = 8 + usize::from(header[1]) * 4;
    let sid = bytes
        .get(offset..offset + length)
        .ok_or(SecDescError::PacketComponent(name))?;
    Sid::from_bytes(sid)
        .map(Some)
        .map_err(|_| SecDescError::PacketComponent(name))
}

fn parse_acl(
    bytes: &[u8],
    offset: usize,
    name: &'static str,
) -> Result<Option<Vec<u8>>, SecDescError> {
    if offset == 0 {
        return Ok(None);
    }
    validate_offset(bytes, offset, name)?;
    let header = bytes
        .get(offset..offset + ACL_HEADER_LEN)
        .ok_or(SecDescError::PacketComponent(name))?;
    let length = usize::from(u16::from_le_bytes(header[2..4].try_into().unwrap()));
    let acl = bytes
        .get(offset..offset + length)
        .ok_or(SecDescError::PacketComponent(name))?;
    Ok(Some(acl.to_vec()))
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
    name: &'static str,
    loaded: bool,
    present: bool,
    acl: Option<Vec<u8>>,
) -> Result<Option<AclBuf>, SecDescError> {
    let Some(acl) = acl else {
        return Ok(None);
    };
    if !loaded {
        return Err(SecDescError::AclNotLoaded(name));
    }
    if !present {
        return Err(SecDescError::AclNotPresent(name));
    }
    AclBuf::try_from_bytes(acl.into_boxed_slice())
        .map(Some)
        .map_err(|error| SecDescError::Acl(name, error))
}

impl Serialize for SecDesc {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut tuple = serializer.serialize_tuple(8)?;
        tuple.serialize_element(&self.mask)?;
        tuple.serialize_element(&self.revision)?;
        tuple.serialize_element(&self.rm_control)?;
        tuple.serialize_element(&self.control)?;
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
                let mask = next(&mut seq, 0, &self)?;
                let revision = next(&mut seq, 1, &self)?;
                let rm_control = next(&mut seq, 2, &self)?;
                let control = next(&mut seq, 3, &self)?;
                let owner = next(&mut seq, 4, &self)?;
                let group = next(&mut seq, 5, &self)?;
                let dacl = next(&mut seq, 6, &self)?;
                let sacl = next(&mut seq, 7, &self)?;
                SecDesc::new(
                    mask, revision, rm_control, control, owner, group, dacl, sacl,
                )
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
    Revision(u8),
    OwnerNotLoaded,
    GroupNotLoaded,
    AclNotLoaded(&'static str),
    AclNotPresent(&'static str),
    AclPresenceConflict(&'static str),
    AclPresenceRequiresValue(&'static str),
    ComponentNotLoaded(&'static str),
    Acl(&'static str, AclError),
    PacketLength,
    NotSelfRelative,
    PacketOffset(&'static str, u32),
    PacketComponent(&'static str),
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

        assert_eq!(acl.revision(), 2);
        assert_eq!(usize::from(acl.size()), bytes.len());
        assert_eq!(acl.ace_count(), 2);
        assert_eq!(acl.as_bytes(), bytes);

        let mut aces = acl.aces();
        let first = aces.next().unwrap();
        assert_eq!(first.ace_type(), AceType::AccessAllowed);
        assert_eq!(first.flags(), 0x13);
        assert_eq!(first.mask(), Some(0x1234_5678));
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
                bytes.extend_from_slice(object_type.as_bytes());
            }
            if object_flags & 2 != 0 {
                bytes.extend_from_slice(inherited_type.as_bytes());
            }
            bytes.extend_from_slice(&trustee.to_bytes());
            bytes.extend_from_slice(&[1, 2, 3, 4]);
            let size = u16::try_from(bytes.len()).unwrap();
            bytes[2..4].copy_from_slice(&size.to_le_bytes());

            let ace = Ace::from_bytes(&bytes).unwrap();
            assert_eq!(ace.ace_type(), AceType::AccessAllowedCallbackObject);
            assert_eq!(ace.object_flags(), Some(object_flags));
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
        let unloaded = SecDesc::new(0, 1, 0, 0, None, None, None, None).unwrap();
        assert!(!unloaded.owner_loaded());
        assert!(!unloaded.dacl_loaded());

        let absent = SecDesc::new(
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            1,
            0,
            0,
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
            DACL_SECURITY_INFORMATION,
            1,
            0,
            SE_DACL_PRESENT,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(null.dacl_present());
        assert_eq!(null.dacl(), None);

        let bytes = acl(8);
        let present = SecDesc::new(
            DACL_SECURITY_INFORMATION,
            1,
            0,
            SE_DACL_PRESENT,
            None,
            None,
            Some(bytes.clone()),
            None,
        )
        .unwrap();
        assert_eq!(present.dacl().map(Acl::as_bytes), Some(bytes.as_slice()));
    }

    #[test]
    fn rejects_inconsistent_components() {
        assert_eq!(
            SecDesc::new(0, 1, 0, 0, Some(sid("S-1-5-18")), None, None, None),
            Err(SecDescError::OwnerNotLoaded)
        );
        assert_eq!(
            SecDesc::new(0, 1, 0, 0, None, Some(sid("S-1-5-18")), None, None),
            Err(SecDescError::GroupNotLoaded)
        );
        assert_eq!(
            SecDesc::new(0, 1, 0, SE_DACL_PRESENT, None, None, Some(acl(8)), None),
            Err(SecDescError::AclNotLoaded("DACL"))
        );
        assert_eq!(
            SecDesc::new(
                DACL_SECURITY_INFORMATION,
                1,
                0,
                0,
                None,
                None,
                Some(acl(8)),
                None,
            ),
            Err(SecDescError::AclNotPresent("DACL"))
        );
    }

    #[test]
    fn validates_acl_packet_and_ace_boundaries() {
        assert!(matches!(
            SecDesc::new(
                DACL_SECURITY_INFORMATION,
                1,
                0,
                SE_DACL_PRESENT,
                None,
                None,
                Some(vec![0; 4]),
                None,
            ),
            Err(SecDescError::Acl("DACL", AclError::Length(4)))
        ));

        let mut wrong_size = acl(8);
        wrong_size.extend_from_slice(&[0; 4]);
        assert!(matches!(
            SecDesc::new(
                DACL_SECURITY_INFORMATION,
                1,
                0,
                SE_DACL_PRESENT,
                None,
                None,
                Some(wrong_size),
                None,
            ),
            Err(SecDescError::Acl("DACL", AclError::Size(8, 12)))
        ));

        let mut opaque = acl(12);
        opaque[8..].copy_from_slice(&[0xff; 4]);
        let descriptor = SecDesc::new(
            DACL_SECURITY_INFORMATION,
            1,
            0,
            SE_DACL_PRESENT,
            None,
            None,
            Some(opaque.clone()),
            None,
        )
        .unwrap();
        assert_eq!(
            descriptor.dacl().map(Acl::as_bytes),
            Some(opaque.as_slice())
        );
    }

    #[test]
    fn projects_control_flags_and_normalizes_storage_form() {
        let control = SE_OWNER_DEFAULTED
            | SE_GROUP_DEFAULTED
            | SE_DACL_DEFAULTED
            | SE_SACL_DEFAULTED
            | SE_DACL_AUTO_INHERIT_REQ
            | SE_SACL_AUTO_INHERIT_REQ
            | SE_DACL_AUTO_INHERITED
            | SE_SACL_AUTO_INHERITED
            | SE_DACL_PROTECTED
            | SE_SACL_PROTECTED
            | SE_RM_CONTROL_VALID
            | SE_SELF_RELATIVE;
        let descriptor = SecDesc::new(0, 1, 0x5a, control, None, None, None, None).unwrap();
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
        assert_eq!(descriptor.control() & SE_SELF_RELATIVE, 0);

        let descriptor = SecDesc::new(0, 1, 0x5a, 0, None, None, None, None).unwrap();
        assert_eq!(descriptor.rm_control(), None);
    }

    #[test]
    fn serde_is_structural_and_validated() {
        let owner = sid("S-1-5-18");
        let dacl = acl(8);
        let descriptor = SecDesc::new(
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            1,
            0x42,
            SE_DACL_PRESENT | SE_RM_CONTROL_VALID,
            Some(owner.clone()),
            None,
            Some(dacl.clone()),
            None,
        )
        .unwrap();
        let encoded = postcard::to_stdvec(&descriptor).unwrap();
        let expected = postcard::to_stdvec(&(
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            1u8,
            0x42u8,
            SE_DACL_PRESENT | SE_RM_CONTROL_VALID,
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
    fn self_relative_packet_round_trip() {
        let packet = [
            0x01, 0x5a, 0x15, 0xd0, 0x14, 0x00, 0x00, 0x00, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x30, 0x00, 0x00, 0x00, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05,
            0x12, 0x00, 0x00, 0x00, 0x01, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x20, 0x00,
            0x00, 0x00, 0x20, 0x02, 0x00, 0x00, 0x02, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let descriptor = SecDesc::from_bytes(&packet).unwrap();
        assert_eq!(descriptor.mask(), ALL_SECURITY_INFORMATION);
        assert_eq!(descriptor.control(), 0x5015);
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
            ALL_SECURITY_INFORMATION,
            1,
            0,
            SE_DACL_PRESENT | SE_DACL_PROTECTED,
            Some(sid("S-1-5-18")),
            Some(sid("S-1-5-32-544")),
            Some(acl(8)),
            None,
        )
        .unwrap();
        let packet = descriptor.to_bytes();

        let selected = SecDesc::from_bytes_with_mask(&packet, DACL_SECURITY_INFORMATION).unwrap();
        assert_eq!(selected.mask(), DACL_SECURITY_INFORMATION);
        assert!(!selected.owner_loaded());
        assert_eq!(selected.owner(), None);
        assert!(selected.dacl_loaded());
        assert_eq!(selected.dacl().map(Acl::as_bytes), Some(acl(8).as_slice()));
        assert!(selected.dacl_protected());

        let empty = SecDesc::from_bytes_with_mask(&packet, 0).unwrap();
        assert_eq!(empty.mask(), 0);
        assert_eq!(empty.control(), SE_DACL_PRESENT | SE_DACL_PROTECTED);
        assert_eq!(empty.owner(), None);
        assert_eq!(empty.dacl(), None);
    }

    #[test]
    fn self_relative_packet_writer_uses_canonical_component_order() {
        let descriptor = SecDesc::new(
            ALL_SECURITY_INFORMATION,
            1,
            0,
            SE_DACL_PRESENT,
            Some(sid("S-1-5-18")),
            Some(sid("S-1-5-32-544")),
            Some(acl(8)),
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

        packet[2..4].copy_from_slice(&SE_SELF_RELATIVE.to_le_bytes());
        packet[4..8].copy_from_slice(&4u32.to_le_bytes());
        assert_eq!(
            SecDesc::from_bytes(&packet),
            Err(SecDescError::PacketOffset("owner", 4))
        );

        packet[4..8].copy_from_slice(&20u32.to_le_bytes());
        assert_eq!(
            SecDesc::from_bytes(&packet),
            Err(SecDescError::PacketOffset("owner", 20))
        );
    }

    #[test]
    fn owned_ace_builders_select_layouts_and_pad_application_data() {
        let trustee = sid("S-1-1-0");
        let object_type: Guid = "00112233-4455-6677-8899-aabbccddeeff".parse().unwrap();
        let basic = AceBuf::allow(
            &trustee,
            0x1234,
            AceBuildOptions {
                flags: 0x03,
                application_data: vec![1, 2, 3],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(basic.ace_type(), AceType::AccessAllowed);
        assert_eq!(basic.flags(), 0x03);
        assert_eq!(basic.application_data(), Some(&[1, 2, 3, 0][..]));
        assert_eq!(Ace::from_bytes(basic.as_bytes()).unwrap(), &*basic);

        let object = AceBuf::deny(
            &trustee,
            u32::MAX,
            AceBuildOptions {
                object_type: Some(object_type),
                callback: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(object.ace_type(), AceType::AccessDeniedCallbackObject);
        assert_eq!(object.object_flags(), Some(1));
        assert_eq!(object.object_type(), Some(object_type));
        assert_eq!(object.inherited_object_type(), None);
    }

    #[test]
    fn audit_builder_enforces_outcomes_and_reserves_audit_flags() {
        let trustee = sid("S-1-5-18");
        assert_eq!(
            AceBuf::audit(&trustee, 1, false, false, AceBuildOptions::default()),
            Err(AceBuildError::AuditOutcome)
        );
        assert_eq!(
            AceBuf::audit(
                &trustee,
                1,
                true,
                false,
                AceBuildOptions {
                    flags: 0x40,
                    ..Default::default()
                },
            ),
            Err(AceBuildError::AuditFlags)
        );
        let audit = AceBuf::audit(&trustee, 1, true, true, AceBuildOptions::default()).unwrap();
        assert_eq!(audit.ace_type(), AceType::SystemAudit);
        assert_eq!(audit.flags(), 0xc0);
    }

    #[test]
    fn acl_builder_preserves_packets_and_selects_revision() {
        let trustee = sid("S-1-1-0");
        let basic = AceBuf::allow(&trustee, 1, AceBuildOptions::default()).unwrap();
        let object = AceBuf::allow(
            &trustee,
            2,
            AceBuildOptions {
                object_type: Some("00000000-0000-0000-0000-000000000000".parse().unwrap()),
                ..Default::default()
            },
        )
        .unwrap();
        let acl = AclBuf::from_aces([&*basic], None).unwrap();
        assert_eq!(acl.revision(), 2);
        assert_eq!(acl.aces().next().unwrap().as_bytes(), basic.as_bytes());

        let acl = AclBuf::from_aces([&*basic, &*object], None).unwrap();
        assert_eq!(acl.revision(), 4);
        assert_eq!(
            AclBuf::from_aces([&*object], Some(2)),
            Err(AclBuildError::ObjectRevision)
        );
        assert_eq!(
            AclBuf::from_aces([&*basic], Some(3)),
            Err(AclBuildError::Revision(3))
        );
        assert_eq!(Acl::from_bytes(acl.as_bytes()).unwrap(), &*acl);
    }

    #[test]
    fn owned_packets_validate_raw_and_serde_inputs() {
        let trustee = sid("S-1-1-0");
        let ace = AceBuf::allow(&trustee, 1, AceBuildOptions::default()).unwrap();
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
        let descriptor = SecDesc::new(0, 1, 0, 0, None, None, None, None).unwrap();
        let concrete = AclBuf::from_aces(std::iter::empty::<&Ace>(), None).unwrap();
        let updated = descriptor
            .with(SecDescUpdate {
                owner: Some(Some(sid("S-1-5-18"))),
                dacl: Some(Some(concrete.clone())),
                owner_defaulted: Some(true),
                dacl_protected: Some(true),
                rm_control: Some(Some(0x5a)),
                ..Default::default()
            })
            .unwrap();
        assert!(!descriptor.owner_loaded());
        assert_eq!(updated.owner().unwrap().to_string(), "S-1-5-18");
        assert_eq!(updated.dacl(), Some(&*concrete));
        assert!(updated.dacl_present());
        assert!(updated.owner_defaulted());
        assert!(updated.dacl_protected());
        assert_eq!(updated.rm_control(), Some(0x5a));

        let null = updated
            .with(SecDescUpdate {
                dacl: Some(None),
                rm_control: Some(None),
                ..Default::default()
            })
            .unwrap();
        assert!(null.dacl_present());
        assert_eq!(null.dacl(), None);
        assert_eq!(null.rm_control(), None);

        let absent = null
            .with(SecDescUpdate {
                dacl_present: Some(false),
                ..Default::default()
            })
            .unwrap();
        assert!(absent.dacl_loaded());
        assert!(!absent.dacl_present());
        assert_eq!(
            descriptor.with(SecDescUpdate {
                dacl_present: Some(true),
                ..Default::default()
            }),
            Err(SecDescError::AclPresenceRequiresValue("DACL"))
        );
        let unloaded_present =
            SecDesc::new(0, 1, 0, SE_DACL_PRESENT, None, None, None, None).unwrap();
        assert_eq!(
            unloaded_present.with(SecDescUpdate {
                dacl_present: Some(true),
                ..Default::default()
            }),
            Err(SecDescError::AclPresenceRequiresValue("DACL"))
        );
        assert_eq!(
            descriptor.with(SecDescUpdate {
                dacl_protected: Some(true),
                ..Default::default()
            }),
            Err(SecDescError::ComponentNotLoaded("DACL"))
        );
    }
}
