use std::{error, fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// A Windows globally unique identifier (GUID).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Guid([u8; 16]);

impl Guid {
    /// Parses the native 16-byte Windows GUID representation.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, GuidError> {
        let bytes = bytes.try_into().map_err(|_| GuidError::PacketLength)?;
        Ok(Self(bytes))
    }

    /// Returns the native 16-byte Windows GUID representation.
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Returns the native 16-byte Windows GUID representation.
    pub const fn to_bytes(self) -> [u8; 16] {
        self.0
    }
}

impl TryFrom<&[u8]> for Guid {
    type Error = GuidError;

    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        Self::from_bytes(value)
    }
}

impl fmt::Display for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let data1 = u32::from_le_bytes(self.0[0..4].try_into().unwrap());
        let data2 = u16::from_le_bytes(self.0[4..6].try_into().unwrap());
        let data3 = u16::from_le_bytes(self.0[6..8].try_into().unwrap());
        write!(
            f,
            "{data1:08x}-{data2:04x}-{data3:04x}-{:02x}{:02x}-",
            self.0[8], self.0[9]
        )?;
        for byte in &self.0[10..] {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for Guid {
    type Err = GuidError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 36
            || value.as_bytes()[8] != b'-'
            || value.as_bytes()[13] != b'-'
            || value.as_bytes()[18] != b'-'
            || value.as_bytes()[23] != b'-'
        {
            return Err(GuidError::StringSyntax);
        }
        let parse = |start, end| {
            u64::from_str_radix(&value[start..end], 16).map_err(|_| GuidError::StringSyntax)
        };
        let data1 = u32::try_from(parse(0, 8)?).unwrap();
        let data2 = u16::try_from(parse(9, 13)?).unwrap();
        let data3 = u16::try_from(parse(14, 18)?).unwrap();
        let data4a = u16::try_from(parse(19, 23)?).unwrap();
        let data4b = parse(24, 36)?;
        let mut bytes = [0; 16];
        bytes[0..4].copy_from_slice(&data1.to_le_bytes());
        bytes[4..6].copy_from_slice(&data2.to_le_bytes());
        bytes[6..8].copy_from_slice(&data3.to_le_bytes());
        bytes[8..10].copy_from_slice(&data4a.to_be_bytes());
        bytes[10..16].copy_from_slice(&data4b.to_be_bytes()[2..]);
        Ok(Self(bytes))
    }
}

impl Serialize for Guid {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Guid {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let bytes = <[u8; 16]>::deserialize(deserializer)?;
        Guid::from_bytes(&bytes).map_err(de::Error::custom)
    }
}

/// Error returned when parsing a GUID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuidError {
    PacketLength,
    StringSyntax,
}

impl fmt::Display for GuidError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PacketLength => f.write_str("GUID packet must contain exactly 16 bytes"),
            Self::StringSyntax => f.write_str("invalid canonical GUID string"),
        }
    }
}

impl error::Error for GuidError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_and_native_packet_round_trip() {
        let guid: Guid = "00112233-4455-6677-8899-aabbccddeeff".parse().unwrap();
        assert_eq!(
            guid.to_bytes(),
            [
                0x33, 0x22, 0x11, 0x00, 0x55, 0x44, 0x77, 0x66, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff,
            ]
        );
        assert_eq!(guid.to_string(), "00112233-4455-6677-8899-aabbccddeeff");
        assert_eq!(
            "00112233-4455-6677-8899-AABBCCDDEEFF"
                .parse::<Guid>()
                .unwrap(),
            guid
        );
        assert_eq!(Guid::from_bytes(guid.as_bytes()).unwrap(), guid);
    }

    #[test]
    fn rejects_noncanonical_text_and_packet_lengths() {
        assert!(
            "{00112233-4455-6677-8899-aabbccddeeff}"
                .parse::<Guid>()
                .is_err()
        );
        assert!("00112233445566778899aabbccddeeff".parse::<Guid>().is_err());
        assert!(Guid::from_bytes(&[0; 15]).is_err());
    }

    #[test]
    fn serde_round_trip() {
        let guid: Guid = "00112233-4455-6677-8899-aabbccddeeff".parse().unwrap();
        let encoded = postcard::to_stdvec(&guid).unwrap();
        assert_eq!(postcard::from_bytes::<Guid>(&encoded).unwrap(), guid);
    }
}
