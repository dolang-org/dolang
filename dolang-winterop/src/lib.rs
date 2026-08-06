#![deny(warnings)]

mod access_mask;
#[cfg(windows)]
mod apc;
mod guid;
mod sec_desc;
mod sid;
#[cfg(windows)]
mod win32_security;
mod win_error;

pub use access_mask::AccessMask;
#[cfg(windows)]
pub use apc::{ApcCancelled, ApcContext, ApcTask, Closed, Reactor, ReactorControl, TaskCancelled};
pub use guid::{Guid, GuidError};
pub use sec_desc::{
    ALL_SECURITY_INFORMATION, Ace, AceBuf, AceBuildError, AceBuildOptions, AceError, AceType, Aces,
    Acl, AclBuf, AclBuildError, AclError, DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION,
    OWNER_SECURITY_INFORMATION, SACL_SECURITY_INFORMATION, SecDesc, SecDescError, SecDescUpdate,
};
pub use sid::{Sid, SidError};
pub use win_error::{win_error_code, win_error_name};
#[cfg(windows)]
pub use win32_security::with_security_privilege;
