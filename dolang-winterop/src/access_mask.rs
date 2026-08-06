/// Generic Windows `ACCESS_MASK` bits that apply to any securable object
/// type (registry keys, services, files, ...), as opposed to bits whose
/// meaning is specific to one object type (e.g. `KEY_QUERY_VALUE`,
/// `SERVICE_START`).
///
/// Plain data: extension crates build their own local bitflag types from
/// these constants rather than this type implementing any Do-runtime trait
/// directly, since this crate has no `dolang-runtime` dependency (it stays
/// portable for use by wire-protocol crates that may run in a lightweight
/// remote agent).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AccessMask(pub u32);

impl AccessMask {
    pub const DELETE: AccessMask = AccessMask(0x0001_0000);
    pub const READ_CONTROL: AccessMask = AccessMask(0x0002_0000);
    pub const WRITE_DAC: AccessMask = AccessMask(0x0004_0000);
    pub const WRITE_OWNER: AccessMask = AccessMask(0x0008_0000);
    pub const SYNCHRONIZE: AccessMask = AccessMask(0x0010_0000);
    pub const STANDARD_RIGHTS_REQUIRED: AccessMask = AccessMask(0x000F_0000);
    pub const STANDARD_RIGHTS_ALL: AccessMask = AccessMask(0x001F_0000);
    pub const ACCESS_SYSTEM_SECURITY: AccessMask = AccessMask(0x0100_0000);
    pub const MAXIMUM_ALLOWED: AccessMask = AccessMask(0x0200_0000);
    pub const GENERIC_ALL: AccessMask = AccessMask(0x1000_0000);
    pub const GENERIC_EXECUTE: AccessMask = AccessMask(0x2000_0000);
    pub const GENERIC_WRITE: AccessMask = AccessMask(0x4000_0000);
    pub const GENERIC_READ: AccessMask = AccessMask(0x8000_0000);
}
