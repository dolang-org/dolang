use serde::{Deserialize, Deserializer, Serialize, de};
use std::{collections::HashSet, error, fmt};

/// POSIX.1e ACL permissions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PosixAclPermissions {
    pub read: bool,
    pub write: bool,
    pub execute: bool,
}

/// The principal or class selected by a POSIX.1e ACL entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PosixAclQualifier {
    UserObj,
    User(u32),
    GroupObj,
    Group(u32),
    Mask,
    Other,
}

/// A portable POSIX.1e ACL entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PosixAce {
    pub qualifier: PosixAclQualifier,
    pub permissions: PosixAclPermissions,
}

/// A validated, portable POSIX.1e access-control list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PosixAcl {
    entries: Vec<PosixAce>,
}

impl PosixAcl {
    pub fn new(entries: Vec<PosixAce>) -> Result<Self, PosixAclError> {
        validate(&entries)?;
        Ok(Self { entries })
    }

    pub fn entries(&self) -> &[PosixAce] {
        &self.entries
    }
}

impl<'de> Deserialize<'de> for PosixAcl {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            entries: Vec<PosixAce>,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.entries).map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PosixAclError {
    Empty,
    Missing(PosixAclQualifier),
    Duplicate(PosixAclQualifier),
    MissingMask,
}

impl fmt::Display for PosixAclError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("ACL must contain entries"),
            Self::Missing(qualifier) => write!(f, "ACL is missing {qualifier} entry"),
            Self::Duplicate(qualifier) => write!(f, "ACL contains duplicate {qualifier} entries"),
            Self::MissingMask => f.write_str("ACL with named entries must contain a mask"),
        }
    }
}

impl error::Error for PosixAclError {}

impl fmt::Display for PosixAclQualifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UserObj => f.write_str("owner user"),
            Self::User(id) => write!(f, "user {id}"),
            Self::GroupObj => f.write_str("owner group"),
            Self::Group(id) => write!(f, "group {id}"),
            Self::Mask => f.write_str("mask"),
            Self::Other => f.write_str("other"),
        }
    }
}

fn validate(entries: &[PosixAce]) -> Result<(), PosixAclError> {
    if entries.is_empty() {
        return Err(PosixAclError::Empty);
    }
    let mut qualifiers = HashSet::new();
    let mut named = false;
    for entry in entries {
        if !qualifiers.insert(entry.qualifier) {
            return Err(PosixAclError::Duplicate(entry.qualifier));
        }
        named |= matches!(
            entry.qualifier,
            PosixAclQualifier::User(_) | PosixAclQualifier::Group(_)
        );
    }
    for required in [
        PosixAclQualifier::UserObj,
        PosixAclQualifier::GroupObj,
        PosixAclQualifier::Other,
    ] {
        if !qualifiers.contains(&required) {
            return Err(PosixAclError::Missing(required));
        }
    }
    if named && !qualifiers.contains(&PosixAclQualifier::Mask) {
        return Err(PosixAclError::MissingMask);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ace(qualifier: PosixAclQualifier) -> PosixAce {
        PosixAce {
            qualifier,
            permissions: PosixAclPermissions::default(),
        }
    }

    #[test]
    fn validates_acl_shape() {
        assert_eq!(PosixAcl::new(Vec::new()), Err(PosixAclError::Empty));
        assert_eq!(
            PosixAcl::new(vec![
                ace(PosixAclQualifier::UserObj),
                ace(PosixAclQualifier::GroupObj),
                ace(PosixAclQualifier::Other),
                ace(PosixAclQualifier::User(1)),
            ]),
            Err(PosixAclError::MissingMask)
        );
        PosixAcl::new(vec![
            ace(PosixAclQualifier::UserObj),
            ace(PosixAclQualifier::GroupObj),
            ace(PosixAclQualifier::Other),
            ace(PosixAclQualifier::User(1)),
            ace(PosixAclQualifier::Mask),
        ])
        .unwrap();
    }

    #[test]
    fn serde_round_trip_preserves_entries() {
        let acl = PosixAcl::new(vec![
            ace(PosixAclQualifier::Other),
            ace(PosixAclQualifier::Mask),
            ace(PosixAclQualifier::GroupObj),
            ace(PosixAclQualifier::User(42)),
            ace(PosixAclQualifier::UserObj),
        ])
        .unwrap();
        let bytes = postcard::to_stdvec(&acl).unwrap();
        assert_eq!(postcard::from_bytes::<PosixAcl>(&bytes).unwrap(), acl);
    }
}
