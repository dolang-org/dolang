use std::{fmt, io};

use serde::{Deserialize, Serialize};

use crate::target::OperatingSystem;

/// Portable classification of an I/O or VFS error.
///
/// Most variants correspond directly to [`io::ErrorKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ErrorKind {
    NotFound,
    PermissionDenied,
    ConnectionRefused,
    ConnectionReset,
    HostUnreachable,
    NetworkUnreachable,
    ConnectionAborted,
    NotConnected,
    AddrInUse,
    AddrNotAvailable,
    NetworkDown,
    BrokenPipe,
    AlreadyExists,
    WouldBlock,
    NotADirectory,
    IsADirectory,
    DirectoryNotEmpty,
    ReadOnlyFilesystem,
    StaleNetworkFileHandle,
    InvalidInput,
    InvalidData,
    TimedOut,
    WriteZero,
    StorageFull,
    NotSeekable,
    QuotaExceeded,
    FileTooLarge,
    ResourceBusy,
    ExecutableFileBusy,
    Deadlock,
    CrossesDevices,
    TooManyLinks,
    InvalidFilename,
    ArgumentListTooLong,
    Interrupted,
    Unsupported,
    UnexpectedEof,
    OutOfMemory,
    Other,
    /// The file was closed, through this handle or a clone of it.
    Closed,
}

impl From<io::ErrorKind> for ErrorKind {
    fn from(kind: io::ErrorKind) -> Self {
        match kind {
            io::ErrorKind::NotFound => Self::NotFound,
            io::ErrorKind::PermissionDenied => Self::PermissionDenied,
            io::ErrorKind::ConnectionRefused => Self::ConnectionRefused,
            io::ErrorKind::ConnectionReset => Self::ConnectionReset,
            io::ErrorKind::HostUnreachable => Self::HostUnreachable,
            io::ErrorKind::NetworkUnreachable => Self::NetworkUnreachable,
            io::ErrorKind::ConnectionAborted => Self::ConnectionAborted,
            io::ErrorKind::NotConnected => Self::NotConnected,
            io::ErrorKind::AddrInUse => Self::AddrInUse,
            io::ErrorKind::AddrNotAvailable => Self::AddrNotAvailable,
            io::ErrorKind::NetworkDown => Self::NetworkDown,
            io::ErrorKind::BrokenPipe => Self::BrokenPipe,
            io::ErrorKind::AlreadyExists => Self::AlreadyExists,
            io::ErrorKind::WouldBlock => Self::WouldBlock,
            io::ErrorKind::NotADirectory => Self::NotADirectory,
            io::ErrorKind::IsADirectory => Self::IsADirectory,
            io::ErrorKind::DirectoryNotEmpty => Self::DirectoryNotEmpty,
            io::ErrorKind::ReadOnlyFilesystem => Self::ReadOnlyFilesystem,
            io::ErrorKind::StaleNetworkFileHandle => Self::StaleNetworkFileHandle,
            io::ErrorKind::InvalidInput => Self::InvalidInput,
            io::ErrorKind::InvalidData => Self::InvalidData,
            io::ErrorKind::TimedOut => Self::TimedOut,
            io::ErrorKind::WriteZero => Self::WriteZero,
            io::ErrorKind::StorageFull => Self::StorageFull,
            io::ErrorKind::NotSeekable => Self::NotSeekable,
            io::ErrorKind::QuotaExceeded => Self::QuotaExceeded,
            io::ErrorKind::FileTooLarge => Self::FileTooLarge,
            io::ErrorKind::ResourceBusy => Self::ResourceBusy,
            io::ErrorKind::ExecutableFileBusy => Self::ExecutableFileBusy,
            io::ErrorKind::Deadlock => Self::Deadlock,
            io::ErrorKind::CrossesDevices => Self::CrossesDevices,
            io::ErrorKind::TooManyLinks => Self::TooManyLinks,
            io::ErrorKind::InvalidFilename => Self::InvalidFilename,
            io::ErrorKind::ArgumentListTooLong => Self::ArgumentListTooLong,
            io::ErrorKind::Interrupted => Self::Interrupted,
            io::ErrorKind::Unsupported => Self::Unsupported,
            io::ErrorKind::UnexpectedEof => Self::UnexpectedEof,
            io::ErrorKind::OutOfMemory => Self::OutOfMemory,
            _ => Self::Other,
        }
    }
}

impl From<ErrorKind> for io::ErrorKind {
    fn from(kind: ErrorKind) -> Self {
        match kind {
            ErrorKind::NotFound => Self::NotFound,
            ErrorKind::PermissionDenied => Self::PermissionDenied,
            ErrorKind::ConnectionRefused => Self::ConnectionRefused,
            ErrorKind::ConnectionReset => Self::ConnectionReset,
            ErrorKind::HostUnreachable => Self::HostUnreachable,
            ErrorKind::NetworkUnreachable => Self::NetworkUnreachable,
            ErrorKind::ConnectionAborted => Self::ConnectionAborted,
            ErrorKind::NotConnected => Self::NotConnected,
            ErrorKind::AddrInUse => Self::AddrInUse,
            ErrorKind::AddrNotAvailable => Self::AddrNotAvailable,
            ErrorKind::NetworkDown => Self::NetworkDown,
            ErrorKind::BrokenPipe => Self::BrokenPipe,
            ErrorKind::AlreadyExists => Self::AlreadyExists,
            ErrorKind::WouldBlock => Self::WouldBlock,
            ErrorKind::NotADirectory => Self::NotADirectory,
            ErrorKind::IsADirectory => Self::IsADirectory,
            ErrorKind::DirectoryNotEmpty => Self::DirectoryNotEmpty,
            ErrorKind::ReadOnlyFilesystem => Self::ReadOnlyFilesystem,
            ErrorKind::StaleNetworkFileHandle => Self::StaleNetworkFileHandle,
            ErrorKind::InvalidInput => Self::InvalidInput,
            ErrorKind::InvalidData => Self::InvalidData,
            ErrorKind::TimedOut => Self::TimedOut,
            ErrorKind::WriteZero => Self::WriteZero,
            ErrorKind::StorageFull => Self::StorageFull,
            ErrorKind::NotSeekable => Self::NotSeekable,
            ErrorKind::QuotaExceeded => Self::QuotaExceeded,
            ErrorKind::FileTooLarge => Self::FileTooLarge,
            ErrorKind::ResourceBusy => Self::ResourceBusy,
            ErrorKind::ExecutableFileBusy => Self::ExecutableFileBusy,
            ErrorKind::Deadlock => Self::Deadlock,
            ErrorKind::CrossesDevices => Self::CrossesDevices,
            ErrorKind::TooManyLinks => Self::TooManyLinks,
            ErrorKind::InvalidFilename => Self::InvalidFilename,
            ErrorKind::ArgumentListTooLong => Self::ArgumentListTooLong,
            ErrorKind::Interrupted => Self::Interrupted,
            ErrorKind::Unsupported => Self::Unsupported,
            ErrorKind::UnexpectedEof => Self::UnexpectedEof,
            ErrorKind::OutOfMemory => Self::OutOfMemory,
            ErrorKind::Other | ErrorKind::Closed => Self::Other,
        }
    }
}

impl PartialEq<io::ErrorKind> for ErrorKind {
    fn eq(&self, other: &io::ErrorKind) -> bool {
        *self == Self::from(*other)
    }
}

impl PartialEq<ErrorKind> for io::ErrorKind {
    fn eq(&self, other: &ErrorKind) -> bool {
        ErrorKind::from(*self) == *other
    }
}

/// A native operating-system error number tagged with its source platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemCode {
    operating_system: OperatingSystem,
    raw: i32,
}

impl SystemCode {
    /// Creates a tagged native error number.
    pub const fn new(operating_system: OperatingSystem, raw: i32) -> Self {
        Self {
            operating_system,
            raw,
        }
    }

    /// Returns the platform whose numbering scheme produced this value.
    pub const fn operating_system(self) -> OperatingSystem {
        self.operating_system
    }

    /// Returns the native error number.
    pub const fn raw(self) -> i32 {
        self.raw
    }
}

/// An error returned by a VFS operation.
///
/// In addition to a portable [`ErrorKind`], this retains the original message
/// and, when available, the originating system's native error number.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Error {
    kind: ErrorKind,
    message: String,
    system_code: Option<SystemCode>,
}

impl Error {
    /// Creates an error without a native error number.
    pub fn new(kind: ErrorKind, message: impl ToString) -> Self {
        Self {
            kind,
            message: message.to_string(),
            system_code: None,
        }
    }

    /// Creates an unclassified error from a displayable value.
    pub fn other(error: impl ToString) -> Self {
        Self::new(ErrorKind::Other, error.to_string())
    }

    /// Creates an error with a native error number and its source platform.
    pub fn from_system_code(
        kind: ErrorKind,
        message: impl Into<String>,
        operating_system: OperatingSystem,
        raw: i32,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            system_code: Some(SystemCode::new(operating_system, raw)),
        }
    }

    /// Converts a native error number from the current host.
    pub fn from_raw_os_error(raw: i32) -> Self {
        let error = io::Error::from_raw_os_error(raw);
        Self::from_raw_os_error_with_message(raw, error.to_string())
    }

    /// Captures the calling thread's last operating-system error.
    pub fn last_os_error() -> Self {
        io::Error::last_os_error().into()
    }

    /// Converts a native error number from the current host while preserving
    /// a caller-supplied message.
    pub fn from_raw_os_error_with_message(raw: i32, message: impl Into<String>) -> Self {
        let kind = io::Error::from_raw_os_error(raw).kind().into();
        Self::from_system_code(kind, message, OperatingSystem::current(), raw)
    }

    /// Converts a native error number from the current host while preserving
    /// a caller-supplied kind.
    pub fn from_raw_os_error_with_kind(raw: i32, kind: ErrorKind) -> Self {
        let error = io::Error::from_raw_os_error(raw);
        Self::from_system_code(kind, error.to_string(), OperatingSystem::current(), raw)
    }

    /// Returns this error's portable classification.
    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Returns the human-readable error message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns the tagged native error number, if one was supplied.
    pub const fn system_code(&self) -> Option<SystemCode> {
        self.system_code
    }

    /// Returns the untagged native error number, if one was supplied.
    pub const fn raw_os_error(&self) -> Option<i32> {
        match self.system_code {
            Some(code) => Some(code.raw()),
            None => None,
        }
    }

    /// Converts this error into [`io::Error`], preserving its portable kind.
    pub fn into_io_error(self) -> io::Error {
        io::Error::new(self.kind.into(), self)
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        // An error that crossed an `io` interface, such as the cursor traits,
        // comes back as itself rather than as its `io` kind.
        let error = match error.downcast::<Self>() {
            Ok(error) => return error,
            Err(error) => error,
        };
        let kind = error.kind().into();
        let message = error.to_string();
        match error.raw_os_error() {
            Some(raw) => Self::from_system_code(kind, message, OperatingSystem::current(), raw),
            None => Self::new(kind, message),
        }
    }
}

impl From<dolang_rpc::Error> for Error {
    fn from(error: dolang_rpc::Error) -> Self {
        match error {
            dolang_rpc::Error::Io(error) => error.into(),
            dolang_rpc::Error::Serialize(_)
            | dolang_rpc::Error::Deserialize(_)
            | dolang_rpc::Error::Protocol(_) => Self::new(ErrorKind::InvalidData, error),
            dolang_rpc::Error::Auth(_) => Self::new(ErrorKind::PermissionDenied, error),
            dolang_rpc::Error::ConnectionClosed => Self::new(ErrorKind::ConnectionReset, error),
            dolang_rpc::Error::Cancelled => Self::new(ErrorKind::Interrupted, error),
            dolang_rpc::Error::UnsupportedCapability => Self::new(ErrorKind::Unsupported, error),
        }
    }
}

impl From<std::ffi::NulError> for Error {
    fn from(error: std::ffi::NulError) -> Self {
        io::Error::from(error).into()
    }
}

#[cfg(unix)]
impl From<nix::errno::Errno> for Error {
    fn from(error: nix::errno::Errno) -> Self {
        Self::from_raw_os_error(error as i32)
    }
}

impl From<Error> for io::Error {
    fn from(error: Error) -> Self {
        error.into_io_error()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// A consuming conversion that did not take effect, returned with the handle
/// it was given.
///
/// Modelled on [`std::io::IntoInnerError`]: an operation that takes ownership
/// has nowhere to put the value back on failure, so it hands it to the caller
/// along with the reason. Receiving one means *nothing was surrendered* — the
/// handle is exactly as usable as it was before the call — so a caller that
/// only wants the error can discard it, and one that wants to retry can keep
/// it.
///
/// It deliberately does not implement `Debug` in terms of the handle, so that
/// `unwrap()` works on handles that are not themselves `Debug`.
pub struct HandoffError<H> {
    handle: H,
    error: Error,
}

impl<H> HandoffError<H> {
    /// Pairs a handle with the reason its conversion did not happen.
    pub fn new(handle: H, error: impl Into<Error>) -> Self {
        Self {
            handle,
            error: error.into(),
        }
    }

    /// Returns the reason the conversion did not happen.
    pub fn error(&self) -> &Error {
        &self.error
    }

    /// Recovers the handle, discarding the reason.
    pub fn into_handle(self) -> H {
        self.handle
    }

    /// Discards the handle, keeping the reason.
    pub fn into_error(self) -> Error {
        self.error
    }

    /// Splits into the recovered handle and the reason.
    pub fn into_parts(self) -> (H, Error) {
        (self.handle, self.error)
    }
}

impl<H> fmt::Debug for HandoffError<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HandoffError")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl<H> fmt::Display for HandoffError<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.error, f)
    }
}

impl<H> std::error::Error for HandoffError<H> {}

impl<H> From<HandoffError<H>> for Error {
    fn from(error: HandoffError<H>) -> Self {
        error.error
    }
}

/// The result type returned by VFS operations.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::{Error, ErrorKind, OperatingSystem};
    use std::io;

    #[test]
    fn io_error_preserves_formatted_message_and_origin() {
        #[cfg(unix)]
        let raw = libc::ENOENT;
        #[cfg(windows)]
        let raw = windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND as i32;

        let io_error = io::Error::from_raw_os_error(raw);
        let message = io_error.to_string();
        let error = Error::from(io_error);
        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert_eq!(error.message(), message);
        let code = error.system_code().unwrap();
        assert_eq!(code.operating_system(), OperatingSystem::current());
        assert_eq!(code.raw(), raw);
    }

    #[test]
    fn foreign_system_code_keeps_supplied_message() {
        let error = Error::from_system_code(
            ErrorKind::PermissionDenied,
            "access is denied",
            OperatingSystem::Windows,
            5,
        );
        assert_eq!(error.message(), "access is denied");
        assert_eq!(
            error.system_code().unwrap().operating_system(),
            OperatingSystem::Windows
        );
    }

    #[test]
    fn raw_os_error_with_message_derives_kind_and_preserves_details() {
        #[cfg(unix)]
        let raw = libc::ENOENT;
        #[cfg(windows)]
        let raw = windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND as i32;

        let error = Error::from_raw_os_error_with_message(raw, "custom message");
        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert_eq!(error.message(), "custom message");
        let code = error.system_code().unwrap();
        assert_eq!(code.operating_system(), OperatingSystem::current());
        assert_eq!(code.raw(), raw);
    }
}
