use std::{fmt, io};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OperatingSystem {
    FreeBsd,
    Linux,
    Macos,
    Windows,
}

impl OperatingSystem {
    pub fn current() -> Self {
        #[cfg(target_os = "linux")]
        return Self::Linux;
        #[cfg(target_os = "macos")]
        return Self::Macos;
        #[cfg(target_os = "freebsd")]
        return Self::FreeBsd;
        #[cfg(windows)]
        return Self::Windows;
        #[cfg(not(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "freebsd",
            windows
        )))]
        compile_error!("unsupported target operating system");
    }

    pub const fn path_type(&self) -> typed_path::PathType {
        match self {
            Self::Linux | Self::Macos | Self::FreeBsd => typed_path::PathType::Unix,
            Self::Windows => typed_path::PathType::Windows,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
            ErrorKind::Other => Self::Other,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemCode {
    operating_system: OperatingSystem,
    raw: i32,
}

impl SystemCode {
    pub const fn new(operating_system: OperatingSystem, raw: i32) -> Self {
        Self {
            operating_system,
            raw,
        }
    }

    pub const fn operating_system(self) -> OperatingSystem {
        self.operating_system
    }

    pub const fn raw(self) -> i32 {
        self.raw
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Error {
    kind: ErrorKind,
    message: String,
    system_code: Option<SystemCode>,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            system_code: None,
        }
    }

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

    pub fn from_raw_os_error(raw: i32) -> Self {
        io::Error::from_raw_os_error(raw).into()
    }

    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub const fn system_code(&self) -> Option<SystemCode> {
        self.system_code
    }

    pub const fn raw_os_error(&self) -> Option<i32> {
        match self.system_code {
            Some(code) => Some(code.raw()),
            None => None,
        }
    }

    pub fn into_io_error(self) -> io::Error {
        io::Error::new(self.kind.into(), self)
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        let kind = error.kind().into();
        let message = error.to_string();
        match error.raw_os_error() {
            Some(raw) => Self::from_system_code(kind, message, OperatingSystem::current(), raw),
            None => Self::new(kind, message),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

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
}
