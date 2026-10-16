//! Target-syntax paths, target-path conversion, and well-known locations.
//!
//! [`Path`] and [`PathBuf`] carry the path syntax ([`Kind`]) they were written
//! in, so a path built for a Windows target keeps Windows semantics even when it
//! is manipulated on a Unix host. They are the only path types that appear in
//! this crate's public API.
//!
//! Knowing the syntax is also what lets the component accessors handle a
//! Windows alternate data stream suffix (`file.txt:zone:$DATA`) as the two
//! things it is — a file name and a [`StreamSpec`] — rather than as one opaque
//! component. On a Unix path `:` is an ordinary filename character, so the
//! stream accessors are all no-ops there.

use serde::{Deserialize, Serialize};
use typed_path::{Utf8TypedPath, Utf8TypedPathBuf, Utf8UnixPath, Utf8WindowsPath};

use crate::error::{Error, ErrorKind, Result};

mod components;
pub(crate) mod stream;

pub use components::{Component, Components, WindowsPrefix};
pub use stream::{StreamSpec, StreamSpecBuf};

/// A standard location resolved by a VFS target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum WellKnownPath {
    /// User's home directory.
    HomeDir,
    /// Per-user cache directory.
    CacheDir,
    /// Directory for temporary files.
    TempDir,
    /// Per-user roaming application data directory.
    DataDir,
    /// Per-user local application data directory.
    DataLocalDir,
    /// Per-user state directory, where supported.
    StateDir,
}

/// Path syntax used by a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Kind {
    /// Unix syntax: `/` separators, no drive letters or prefixes.
    Unix,
    /// Windows syntax: `\` and `/` separators, drive letters, UNC and verbatim
    /// prefixes, and alternate data stream specifiers.
    Windows,
}

impl Kind {
    /// Returns the native host's path syntax.
    pub const fn native() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Unix
        }
    }
}

/// A borrowed path in a target's syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Path<'a>(pub(crate) Utf8TypedPath<'a>);

/// Applies the same expression to whichever concrete path a [`Path`] holds.
///
/// Going through the concrete type is what ties the result to the path's own
/// `'a` rather than to the borrow of `self`, which is why these methods can
/// hand out `&'a str` at all.
macro_rules! project {
    ($self:expr, |$path:ident| $body:expr) => {
        match $self.0 {
            Utf8TypedPath::Unix($path) => $body,
            Utf8TypedPath::Windows($path) => $body,
        }
    };
}

/// An owned path in a target's syntax.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PathBuf(pub(crate) Utf8TypedPathBuf);

impl<'a> Path<'a> {
    /// Creates a path with the given syntax.
    pub fn new(path: &'a (impl AsRef<str> + ?Sized), kind: Kind) -> Self {
        match kind {
            Kind::Unix => Self::unix(path),
            Kind::Windows => Self::windows(path),
        }
    }

    /// Creates a path with Unix syntax.
    pub fn unix(path: &'a (impl AsRef<str> + ?Sized)) -> Self {
        Self(Utf8TypedPath::Unix(Utf8UnixPath::new(path.as_ref())))
    }

    /// Creates a path with Windows syntax.
    pub fn windows(path: &'a (impl AsRef<str> + ?Sized)) -> Self {
        Self(Utf8TypedPath::Windows(Utf8WindowsPath::new(path.as_ref())))
    }

    /// Returns this path's syntax.
    pub fn kind(&self) -> Kind {
        match self.0 {
            Utf8TypedPath::Unix(_) => Kind::Unix,
            Utf8TypedPath::Windows(_) => Kind::Windows,
        }
    }

    /// Returns the path text.
    pub fn as_str(&self) -> &'a str {
        project!(self, |path| path.as_str())
    }

    /// Converts this path into an owned path.
    pub fn to_path_buf(&self) -> PathBuf {
        PathBuf(self.0.to_path_buf())
    }

    /// Converts this path into a native host path.
    ///
    /// # Errors
    ///
    /// Fails if this path's syntax is not the host's syntax.
    pub fn to_native(&self) -> Result<std::path::PathBuf> {
        if self.kind() != Kind::native() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "path style does not match VFS target",
            ));
        }
        Ok(std::path::PathBuf::from(self.as_str()))
    }

    /// Converts this path into the given syntax.
    ///
    /// Returns the path unchanged when it already uses `kind`.
    ///
    /// # Errors
    ///
    /// Only relative, unrooted paths carrying no alternate data stream
    /// specifier can be converted between syntaxes.
    pub fn to_kind(&self, kind: Kind) -> Result<PathBuf> {
        if self.kind() == kind {
            return Ok(self.to_path_buf());
        }
        let convertible = match self.0 {
            Utf8TypedPath::Windows(path) => {
                !path.has_root()
                    && path.components().prefix_kind().is_none()
                    && !path.file_name().is_some_and(|name| name.contains(':'))
            }
            Utf8TypedPath::Unix(path) => !path.has_root(),
        };
        if !convertible || self.is_absolute() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "only relative, unrooted paths can be converted between path types",
            ));
        }
        let converted = match kind {
            Kind::Unix => self.0.with_unix_encoding_checked(),
            Kind::Windows => self.0.with_windows_encoding_checked(),
        };
        converted.map(PathBuf).map_err(|_| {
            Error::new(
                ErrorKind::InvalidInput,
                "path cannot be converted between path types",
            )
        })
    }

    /// Returns whether the path is absolute.
    pub fn is_absolute(&self) -> bool {
        self.0.is_absolute()
    }

    /// Returns whether the path is relative.
    pub fn is_relative(&self) -> bool {
        self.0.is_relative()
    }

    /// Returns whether the path has a root component.
    pub fn has_root(&self) -> bool {
        self.0.has_root()
    }

    /// Returns whether the path begins with `base`.
    pub fn starts_with(&self, base: impl AsRef<str>) -> bool {
        self.0.starts_with(base)
    }

    /// Returns whether the path ends with `child`.
    pub fn ends_with(&self, child: impl AsRef<str>) -> bool {
        self.0.ends_with(child)
    }

    /// Returns the path with `base` removed from its front.
    ///
    /// # Errors
    ///
    /// Fails if the path does not begin with `base`.
    pub fn strip_prefix(&self, base: impl AsRef<str>) -> Result<Path<'a>> {
        let base = base.as_ref();
        let stripped = match self.0 {
            Utf8TypedPath::Unix(path) => path
                .strip_prefix(Utf8UnixPath::new(base))
                .map(Utf8TypedPath::Unix),
            Utf8TypedPath::Windows(path) => path
                .strip_prefix(Utf8WindowsPath::new(base))
                .map(Utf8TypedPath::Windows),
        };
        stripped.map(Path).map_err(|_| {
            Error::new(
                ErrorKind::InvalidInput,
                "path does not start with the given prefix",
            )
        })
    }

    /// Returns the path without its final component.
    pub fn parent(&self) -> Option<Self> {
        match self.0 {
            Utf8TypedPath::Unix(path) => path.parent().map(Utf8TypedPath::Unix),
            Utf8TypedPath::Windows(path) => path.parent().map(Utf8TypedPath::Windows),
        }
        .map(Self)
    }

    /// Returns the final component, excluding any alternate data stream
    /// suffix.
    pub fn file_name(&self) -> Option<&'a str> {
        self.split_stream().0
    }

    /// Returns the final component exactly as it is spelled, including any
    /// alternate data stream suffix.
    pub fn file_name_raw(&self) -> Option<&'a str> {
        project!(self, |path| path.file_name())
    }

    /// Returns the final component without its extension or its alternate data
    /// stream suffix.
    pub fn file_stem(&self) -> Option<&'a str> {
        match self.split_stream() {
            (Some(base), Some(_)) => Path::new(base, self.kind()).file_stem_raw(),
            _ => self.file_stem_raw(),
        }
    }

    /// Returns the extension of the final component, ignoring any alternate
    /// data stream suffix.
    pub fn extension(&self) -> Option<&'a str> {
        match self.split_stream() {
            (Some(base), Some(_)) => Path::new(base, self.kind()).extension_raw(),
            _ => self.extension_raw(),
        }
    }

    fn file_stem_raw(&self) -> Option<&'a str> {
        project!(self, |path| path.file_stem())
    }

    fn extension_raw(&self) -> Option<&'a str> {
        project!(self, |path| path.extension())
    }

    /// Splits the final component into its base name and alternate data stream
    /// specifier.
    ///
    /// A malformed suffix reads here as no stream at all, which is what keeps
    /// the component accessors infallible; [`Path::stream`] is the accessor
    /// that reports the difference.
    fn split_stream(&self) -> (Option<&'a str>, Option<StreamSpec<'a>>) {
        let Some(name) = self.file_name_raw() else {
            return (None, None);
        };
        if self.kind() != Kind::Windows {
            return (Some(name), None);
        }
        match stream::split_suffix(name) {
            Ok((base, spec)) => (Some(base), spec),
            Err(_) => (Some(name), None),
        }
    }

    /// Returns the alternate data stream specified by the final component.
    ///
    /// Always `None` for a Unix path.
    ///
    /// # Errors
    ///
    /// Fails if the final component carries a suffix that does not follow the
    /// `name:stream[:$TYPE]` grammar.
    pub fn stream(&self) -> Result<Option<StreamSpec<'a>>> {
        if self.kind() != Kind::Windows {
            return Ok(None);
        }
        match self.file_name_raw() {
            Some(name) => Ok(stream::split_suffix(name)?.1),
            None => Ok(None),
        }
    }

    /// Returns this path with its alternate data stream specifier replaced.
    ///
    /// Has no effect on a Unix path.
    pub fn with_stream(&self, spec: Option<StreamSpec<'_>>) -> PathBuf {
        let (Some(base), _) = self.split_stream() else {
            return self.to_path_buf();
        };
        if self.kind() != Kind::Windows {
            return self.to_path_buf();
        }
        self.with_file_name_raw(stream::join_suffix(base, spec))
    }

    /// Returns this path with any alternate data stream specifier removed.
    pub fn without_stream(&self) -> PathBuf {
        self.with_stream(None)
    }

    /// Returns an iterator over the path's components.
    pub fn components(&self) -> Components<'a> {
        Components(self.0.components())
    }

    /// Returns the Windows prefix, if this is a prefixed Windows path.
    pub fn windows_prefix(&self) -> Option<WindowsPrefix<'a>> {
        match self.0 {
            // Taking the prefix off the first component, rather than off the
            // component iterator, is what keeps the borrow tied to `'a`.
            Utf8TypedPath::Windows(path) => path
                .components()
                .next()
                .and_then(|component| component.prefix_kind())
                .map(WindowsPrefix::from),
            Utf8TypedPath::Unix(_) => None,
        }
    }

    /// Returns this path with `path` appended.
    pub fn join(&self, path: impl AsRef<str>) -> PathBuf {
        PathBuf(self.0.join(path))
    }

    /// Returns this path with its final component replaced, keeping any
    /// alternate data stream suffix.
    pub fn with_file_name(&self, file_name: impl AsRef<str>) -> PathBuf {
        match self.split_stream() {
            (_, Some(spec)) => {
                self.with_file_name_raw(stream::join_suffix(file_name.as_ref(), Some(spec)))
            }
            _ => self.with_file_name_raw(file_name),
        }
    }

    /// Returns this path with its final component replaced verbatim, dropping
    /// any alternate data stream suffix along with the name it was attached to.
    pub fn with_file_name_raw(&self, file_name: impl AsRef<str>) -> PathBuf {
        PathBuf(self.0.with_file_name(file_name))
    }

    /// Returns this path with the extension of its final component replaced,
    /// keeping any alternate data stream suffix.
    pub fn with_extension(&self, extension: impl AsRef<str>) -> PathBuf {
        let (Some(base), Some(spec)) = self.split_stream() else {
            return PathBuf(self.0.with_extension(extension));
        };
        let base = Path::new(base, self.kind()).0.with_extension(extension);
        self.with_file_name_raw(stream::join_suffix(base.as_str(), Some(spec)))
    }

    /// Returns this path with `.` components removed and `..` components
    /// resolved lexically.
    ///
    /// This is a purely textual operation: no symlink is resolved and the
    /// target is never consulted.
    pub fn normalize(&self) -> PathBuf {
        let has_root = self.has_root();
        let mut components = Vec::new();

        for component in self.components() {
            if component.is_current() {
                continue;
            }
            if component.is_parent() {
                if components.last().is_some_and(Component::is_normal) {
                    components.pop();
                } else if !has_root {
                    components.push(component);
                }
            } else {
                components.push(component);
            }
        }

        let mut normalized = PathBuf::empty(self.kind());
        for component in components {
            normalized.push(component.as_str());
        }
        normalized
    }
}

impl PathBuf {
    /// Creates a path with the given syntax.
    pub fn new(path: impl AsRef<str>, kind: Kind) -> Self {
        match kind {
            Kind::Unix => Self::from_unix(path),
            Kind::Windows => Self::from_windows(path),
        }
    }

    /// Creates a path with Unix syntax.
    pub fn from_unix(path: impl AsRef<str>) -> Self {
        Self(Utf8TypedPathBuf::from_unix(path))
    }

    /// Creates a path with Windows syntax.
    pub fn from_windows(path: impl AsRef<str>) -> Self {
        Self(Utf8TypedPathBuf::from_windows(path))
    }

    /// Creates an empty path with the given syntax.
    pub fn empty(kind: Kind) -> Self {
        Self::new("", kind)
    }

    /// Creates a path from a native host path.
    ///
    /// # Errors
    ///
    /// Fails if the path is not valid UTF-8.
    pub fn from_native(path: std::path::PathBuf) -> Result<Self> {
        let path = path
            .into_os_string()
            .into_string()
            .map_err(|_| Error::new(ErrorKind::InvalidData, "path is not valid UTF-8"))?;
        Ok(Self::new(path, Kind::native()))
    }

    /// Borrows this path.
    pub fn to_path(&self) -> Path<'_> {
        Path(self.0.to_path())
    }

    /// Appends `path`.
    pub fn push(&mut self, path: impl AsRef<str>) {
        self.0.push(path);
    }

    /// Removes the final component, returning whether one was removed.
    pub fn pop(&mut self) -> bool {
        self.0.pop()
    }

    /// Replaces the final component, keeping any alternate data stream suffix.
    pub fn set_file_name(&mut self, file_name: impl AsRef<str>) {
        let next = self.to_path().with_file_name(file_name);
        *self = next;
    }

    /// Replaces the extension of the final component, keeping any alternate
    /// data stream suffix, and returns whether the path had a final component
    /// to modify.
    pub fn set_extension(&mut self, extension: impl AsRef<str>) -> bool {
        if self.to_path().split_stream().1.is_none() {
            return self.0.set_extension(extension);
        }
        let next = self.to_path().with_extension(extension);
        *self = next;
        true
    }

    /// Replaces the alternate data stream specifier of the final component.
    ///
    /// Has no effect on a Unix path.
    pub fn set_stream(&mut self, spec: Option<StreamSpec<'_>>) {
        let next = self.to_path().with_stream(spec);
        *self = next;
    }
}

/// Methods shared with [`Path`], forwarded for convenience.
impl PathBuf {
    /// Returns this path's syntax.
    pub fn kind(&self) -> Kind {
        self.to_path().kind()
    }

    /// Returns the path text.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Converts this path into a native host path.
    ///
    /// # Errors
    ///
    /// Fails if this path's syntax is not the host's syntax.
    pub fn to_native(&self) -> Result<std::path::PathBuf> {
        self.to_path().to_native()
    }

    /// Converts this path into the given syntax.
    ///
    /// # Errors
    ///
    /// See [`Path::to_kind`].
    pub fn to_kind(&self, kind: Kind) -> Result<Self> {
        self.to_path().to_kind(kind)
    }

    /// Returns whether the path is absolute.
    pub fn is_absolute(&self) -> bool {
        self.0.is_absolute()
    }

    /// Returns whether the path is relative.
    pub fn is_relative(&self) -> bool {
        self.0.is_relative()
    }

    /// Returns whether the path has a root component.
    pub fn has_root(&self) -> bool {
        self.0.has_root()
    }

    /// Returns whether the path begins with `base`.
    pub fn starts_with(&self, base: impl AsRef<str>) -> bool {
        self.0.starts_with(base)
    }

    /// Returns whether the path ends with `child`.
    pub fn ends_with(&self, child: impl AsRef<str>) -> bool {
        self.0.ends_with(child)
    }

    /// Returns the path with `base` removed from its front.
    ///
    /// # Errors
    ///
    /// Fails if the path does not begin with `base`.
    pub fn strip_prefix(&self, base: impl AsRef<str>) -> Result<Path<'_>> {
        self.to_path().strip_prefix(base)
    }

    /// Returns the path without its final component.
    pub fn parent(&self) -> Option<Path<'_>> {
        self.0.parent().map(Path)
    }

    /// Returns the final component, excluding any alternate data stream
    /// suffix.
    pub fn file_name(&self) -> Option<&str> {
        self.to_path().file_name()
    }

    /// Returns the final component exactly as it is spelled, including any
    /// alternate data stream suffix.
    pub fn file_name_raw(&self) -> Option<&str> {
        self.0.file_name()
    }

    /// Returns the final component without its extension or its alternate data
    /// stream suffix.
    pub fn file_stem(&self) -> Option<&str> {
        self.to_path().file_stem()
    }

    /// Returns the extension of the final component, ignoring any alternate
    /// data stream suffix.
    pub fn extension(&self) -> Option<&str> {
        self.to_path().extension()
    }

    /// Returns the alternate data stream specified by the final component.
    ///
    /// # Errors
    ///
    /// See [`Path::stream`].
    pub fn stream(&self) -> Result<Option<StreamSpec<'_>>> {
        self.to_path().stream()
    }

    /// Returns this path with any alternate data stream specifier removed.
    pub fn without_stream(&self) -> Self {
        self.to_path().without_stream()
    }

    /// Returns an iterator over the path's components.
    pub fn components(&self) -> Components<'_> {
        Components(self.0.components())
    }

    /// Returns this path with `path` appended.
    pub fn join(&self, path: impl AsRef<str>) -> Self {
        self.to_path().join(path)
    }

    /// Returns this path with its final component replaced, keeping any
    /// alternate data stream suffix.
    pub fn with_file_name(&self, file_name: impl AsRef<str>) -> Self {
        self.to_path().with_file_name(file_name)
    }

    /// Returns this path with the extension of its final component replaced,
    /// keeping any alternate data stream suffix.
    pub fn with_extension(&self, extension: impl AsRef<str>) -> Self {
        self.to_path().with_extension(extension)
    }

    /// Returns this path with `.` components removed and `..` components
    /// resolved lexically.
    pub fn normalize(&self) -> Self {
        self.to_path().normalize()
    }

    /// Returns the Windows prefix, if this is a prefixed Windows path.
    pub fn windows_prefix(&self) -> Option<WindowsPrefix<'_>> {
        self.to_path().windows_prefix()
    }
}

impl AsRef<str> for Path<'_> {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for PathBuf {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Display for Path<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::fmt::Display for PathBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<'a> From<Path<'a>> for PathBuf {
    fn from(path: Path<'a>) -> Self {
        path.to_path_buf()
    }
}

impl<'a> From<&'a PathBuf> for Path<'a> {
    fn from(path: &'a PathBuf) -> Self {
        path.to_path()
    }
}

impl TryFrom<std::path::PathBuf> for PathBuf {
    type Error = Error;

    fn try_from(path: std::path::PathBuf) -> Result<Self> {
        Self::from_native(path)
    }
}

impl TryFrom<PathBuf> for std::path::PathBuf {
    type Error = Error;

    fn try_from(path: PathBuf) -> Result<Self> {
        path.to_native()
    }
}

/// Wire representation: the syntax tag plus the literal path text.
///
/// Serializing through this shape keeps a path's syntax and its exact spelling
/// intact across targets, which a plain string cannot do.
#[derive(Serialize, Deserialize)]
#[serde(rename = "PathBuf")]
struct Wire {
    kind: Kind,
    path: String,
}

impl Serialize for PathBuf {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        Wire {
            kind: self.kind(),
            path: self.as_str().to_owned(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PathBuf {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let wire = Wire::deserialize(deserializer)?;
        Ok(Self::new(wire.path, wire.kind))
    }
}

#[cfg(test)]
mod tests {
    use super::{Kind, Path, PathBuf};

    #[test]
    fn round_trip_preserves_unix_kind_and_literal_form() {
        let path = PathBuf::from_unix(r"foo\bar/baz");
        let bytes = postcard::to_stdvec(&path).unwrap();
        let decoded: PathBuf = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.kind(), Kind::Unix);
        assert_eq!(decoded.as_str(), r"foo\bar/baz");
    }

    #[test]
    fn round_trip_preserves_windows_kind_and_literal_form() {
        let path = PathBuf::from_windows(r"C:\foo/bar");
        let bytes = postcard::to_stdvec(&path).unwrap();
        let decoded: PathBuf = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.kind(), Kind::Windows);
        assert_eq!(decoded.as_str(), r"C:\foo/bar");
    }

    #[test]
    fn native_conversion_rejects_the_other_path_kind() {
        let path = if cfg!(windows) {
            PathBuf::from_unix("foo")
        } else {
            PathBuf::from_windows("foo")
        };
        assert!(path.to_native().is_err());
    }

    #[test]
    fn to_kind_rejects_rooted_and_stream_bearing_paths() {
        assert!(Path::windows(r"C:\foo").to_kind(Kind::Unix).is_err());
        assert!(Path::windows(r"\foo").to_kind(Kind::Unix).is_err());
        assert!(Path::windows("file.txt:zone").to_kind(Kind::Unix).is_err());
        assert!(Path::unix("/foo").to_kind(Kind::Windows).is_err());

        let converted = Path::windows(r"foo\bar").to_kind(Kind::Unix).unwrap();
        assert_eq!(converted.kind(), Kind::Unix);
        assert_eq!(converted.as_str(), "foo/bar");
    }

    #[test]
    fn to_kind_is_identity_for_the_same_kind() {
        let path = Path::windows(r"C:\foo");
        assert_eq!(path.to_kind(Kind::Windows).unwrap().as_str(), r"C:\foo");
    }

    #[test]
    fn normalize_resolves_dot_and_dotdot() {
        assert_eq!(Path::unix("a/./b/../c").normalize().as_str(), "a/c");
        assert_eq!(Path::unix("../a/../b").normalize().as_str(), "../b");
        assert_eq!(Path::unix("/../a").normalize().as_str(), "/a");
    }
}
