use std::{error, fmt};

use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, SeqAccess, Visitor},
    ser::SerializeTuple,
};

use crate::Sid;

const REVISION: u8 = 1;

const OWNER_SECURITY_INFORMATION: u32 = 0x0000_0001;
const GROUP_SECURITY_INFORMATION: u32 = 0x0000_0002;
const DACL_SECURITY_INFORMATION: u32 = 0x0000_0004;
const SACL_SECURITY_INFORMATION: u32 = 0x0000_0008;
const ALL_SECURITY_INFORMATION: u32 = OWNER_SECURITY_INFORMATION
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
const SELF_RELATIVE_HEADER_LEN: usize = 20;

/// A portable representation of a Windows security descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecDesc {
    mask: u32,
    revision: u8,
    rm_control: u8,
    control: u16,
    owner: Option<Sid>,
    group: Option<Sid>,
    dacl: Option<Vec<u8>>,
    sacl: Option<Vec<u8>>,
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
        validate_acl(
            "DACL",
            mask & DACL_SECURITY_INFORMATION != 0,
            control & SE_DACL_PRESENT != 0,
            dacl.as_deref(),
        )?;
        validate_acl(
            "SACL",
            mask & SACL_SECURITY_INFORMATION != 0,
            control & SE_SACL_PRESENT != 0,
            sacl.as_deref(),
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
        if bytes.len() < SELF_RELATIVE_HEADER_LEN {
            return Err(SecDescError::PacketLength);
        }
        let revision = bytes[0];
        let rm_control = bytes[1];
        let control = u16::from_le_bytes(bytes[2..4].try_into().unwrap());
        if control & SE_SELF_RELATIVE == 0 {
            return Err(SecDescError::NotSelfRelative);
        }

        let owner = parse_sid(bytes, packet_offset(bytes, 4)?, "owner")?;
        let group = parse_sid(bytes, packet_offset(bytes, 8)?, "group")?;
        let sacl_offset = packet_offset(bytes, 12)?;
        let dacl_offset = packet_offset(bytes, 16)?;
        if control & SE_SACL_PRESENT == 0 && sacl_offset != 0 {
            return Err(SecDescError::AclNotPresent("SACL"));
        }
        if control & SE_DACL_PRESENT == 0 && dacl_offset != 0 {
            return Err(SecDescError::AclNotPresent("DACL"));
        }
        let sacl = parse_acl(bytes, sacl_offset, "SACL")?;
        let dacl = parse_acl(bytes, dacl_offset, "DACL")?;

        Self::new(
            ALL_SECURITY_INFORMATION,
            revision,
            rm_control,
            control,
            owner,
            group,
            dacl,
            sacl,
        )
    }

    /// Converts this descriptor to a canonical self-relative Windows packet.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = vec![0; SELF_RELATIVE_HEADER_LEN];
        bytes[0] = self.revision;
        bytes[1] = self.rm_control;
        bytes[2..4].copy_from_slice(&(self.control | SE_SELF_RELATIVE).to_le_bytes());

        append_component(&mut bytes, 4, self.owner.as_ref().map(Sid::to_bytes));
        append_component(&mut bytes, 8, self.group.as_ref().map(Sid::to_bytes));
        append_component(&mut bytes, 12, self.sacl.clone());
        append_component(&mut bytes, 16, self.dacl.clone());
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

    /// Returns the opaque DACL bytes, if the DACL is non-null.
    pub fn dacl(&self) -> Option<&[u8]> {
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

    /// Returns the opaque SACL bytes, if the SACL is non-null.
    pub fn sacl(&self) -> Option<&[u8]> {
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

fn append_component(bytes: &mut Vec<u8>, offset_at: usize, component: Option<Vec<u8>>) {
    let Some(component) = component else {
        return;
    };
    let offset = u32::try_from(bytes.len()).expect("security descriptor exceeds 4 GiB");
    bytes[offset_at..offset_at + 4].copy_from_slice(&offset.to_le_bytes());
    bytes.extend_from_slice(&component);
}

fn validate_acl(
    name: &'static str,
    loaded: bool,
    present: bool,
    acl: Option<&[u8]>,
) -> Result<(), SecDescError> {
    let Some(acl) = acl else {
        return Ok(());
    };
    if !loaded {
        return Err(SecDescError::AclNotLoaded(name));
    }
    if !present {
        return Err(SecDescError::AclNotPresent(name));
    }
    if acl.len() < ACL_HEADER_LEN || !acl.len().is_multiple_of(4) {
        return Err(SecDescError::AclLength(name, acl.len()));
    }
    let declared = u16::from_le_bytes(acl[2..4].try_into().unwrap());
    if usize::from(declared) != acl.len() {
        return Err(SecDescError::AclSize(name, declared, acl.len()));
    }
    Ok(())
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
    AclLength(&'static str, usize),
    AclSize(&'static str, u16, usize),
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
            Self::AclLength(name, length) => {
                write!(f, "{name} packet has invalid length {length}")
            }
            Self::AclSize(name, declared, actual) => write!(
                f,
                "{name} packet declares size {declared}, but contains {actual} bytes"
            ),
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
        assert_eq!(present.dacl(), Some(bytes.as_slice()));
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
    fn validates_only_the_acl_packet_boundary() {
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
            Err(SecDescError::AclLength("DACL", 4))
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
            Err(SecDescError::AclSize("DACL", 8, 12))
        ));

        let mut opaque = acl(12);
        opaque[4..].copy_from_slice(&[0xff; 8]);
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
        assert_eq!(descriptor.dacl(), Some(opaque.as_slice()));
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
        assert_eq!(descriptor.dacl(), Some(&packet[48..]));
        assert_eq!(descriptor.to_bytes(), packet);
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
}
