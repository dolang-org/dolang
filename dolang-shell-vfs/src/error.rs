use std::{fmt, io};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OperatingSystem {
    Linux,
    Macos,
    Windows,
    Other(String),
}

impl OperatingSystem {
    pub fn current() -> Self {
        if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(target_os = "macos") {
            Self::Macos
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Other(std::env::consts::OS.to_owned())
        }
    }
}

#[derive(Debug, Clone)]
pub struct SystemError {
    operating_system: OperatingSystem,
    code: i32,
    kind: io::ErrorKind,
    message: String,
}

impl SystemError {
    pub fn new(
        operating_system: OperatingSystem,
        code: i32,
        kind: io::ErrorKind,
        message: impl Into<String>,
    ) -> Self {
        Self {
            operating_system,
            code,
            kind,
            message: message.into(),
        }
    }

    pub fn operating_system(&self) -> &OperatingSystem {
        &self.operating_system
    }

    pub fn code(&self) -> i32 {
        self.code
    }

    pub fn kind(&self) -> io::ErrorKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    System(SystemError),
}

impl Error {
    pub fn kind(&self) -> io::ErrorKind {
        match self {
            Self::Io(error) => error.kind(),
            Self::System(error) => error.kind(),
        }
    }

    pub fn system(&self) -> Option<&SystemError> {
        match self {
            Self::Io(_) => None,
            Self::System(error) => Some(error),
        }
    }

    pub fn raw_os_error(&self) -> Option<i32> {
        match self {
            Self::Io(error) => error.raw_os_error(),
            Self::System(error) => Some(error.code()),
        }
    }

    pub fn into_io_error(self) -> io::Error {
        match self {
            Self::Io(error) => error,
            Self::System(error) if error.operating_system() == &OperatingSystem::current() => {
                io::Error::from_raw_os_error(error.code())
            }
            error => io::Error::new(error.kind(), error),
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        match error.raw_os_error() {
            Some(code) => Self::System(SystemError::new(
                OperatingSystem::current(),
                code,
                error.kind(),
                error.to_string(),
            )),
            None => Self::Io(error),
        }
    }
}

impl From<SystemError> for Error {
    fn from(error: SystemError) -> Self {
        Self::System(error)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(f),
            Self::System(error) => f.write_str(error.message()),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::System(_) => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::{Error, OperatingSystem};
    use std::io;

    #[test]
    fn raw_io_error_becomes_originated_system_error() {
        #[cfg(unix)]
        let code = libc::ENOENT;
        #[cfg(windows)]
        let code = windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND as i32;

        let error = Error::from(io::Error::from_raw_os_error(code));
        let system = error.system().unwrap();
        assert_eq!(system.operating_system(), &OperatingSystem::current());
        assert_eq!(system.code(), code);
        assert_eq!(system.kind(), io::ErrorKind::NotFound);
        assert!(!system.message().is_empty());
    }
}
