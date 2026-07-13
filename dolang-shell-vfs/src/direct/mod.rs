use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::Arc,
};

use tokio::{
    fs::{self, File, OpenOptions},
    process::Command as TokioCommand,
    sync::Mutex,
};

use wax::{
    Glob,
    walk::{DepthBehavior, DepthMax, Entry, LinkBehavior, WalkBehavior},
};

use crate::{
    Attrs, Child, ChownIdentity, Command, DefaultHandle, FsMetadata, Metadata, Permissions,
    PipeRecv, PipeSend, ReadDir, StreamEntry, Utf8TypedPath, Utf8TypedPathBuf, Vfs, WellKnownPath,
    XattrEntry, XattrNamespace, native_path, typed_path,
};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[derive(Debug, Clone)]
pub struct Direct {
    path_cache: Arc<PathCache>,
}

#[derive(Debug, Default)]
pub struct DirectOpenOptions {
    read: bool,
    write: bool,
    append: bool,
    create: bool,
    create_new: bool,
    truncate: bool,
    no_follow: bool,
}

pub struct DirectCommand<'a> {
    direct: &'a Direct,
    program: PathBuf,
    args: Vec<String>,
    env: HashMap<String, Option<String>>,
    cwd: Option<PathBuf>,
    stdin: Option<Stdio>,
    stdout: Option<Stdio>,
    stderr: Option<Stdio>,
    error: Option<io::Error>,
}

pub struct DirectChild {
    inner: tokio::process::Child,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct CacheKey {
    program: PathBuf,
    path: Option<String>,
    cwd: Option<PathBuf>,
}

#[derive(Debug, Default)]
struct PathCache {
    map: Mutex<HashMap<CacheKey, PathBuf>>,
}

impl PathCache {
    fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    async fn resolve(
        &self,
        program: &Path,
        path: Option<&str>,
        cwd: Option<&Path>,
    ) -> Option<PathBuf> {
        let key = CacheKey {
            program: program.to_path_buf(),
            path: path.map(|p| p.to_string()),
            cwd: cwd.map(|p| p.to_path_buf()),
        };

        let cached = {
            let map = self.map.lock().await;
            map.get(&key).cloned()
        };

        if let Some(cached) = cached {
            return Some(cached);
        }

        let path_env = path
            .map(|p| p.into())
            .or_else(|| std::env::var_os("PATH"))
            .unwrap_or_else(|| "".into());

        let program = program.to_path_buf();
        let cwd = cwd.map(|p| p.to_path_buf());

        let resolved = tokio::task::spawn_blocking(move || {
            which::which_in(
                &program,
                Some(path_env),
                cwd.as_deref().unwrap_or(Path::new("")),
            )
            .ok()
        })
        .await
        .unwrap_or(None);

        if let Some(ref resolved_path) = resolved {
            let mut map = self.map.lock().await;
            map.insert(key, resolved_path.clone());
        }

        resolved
    }

    async fn clear(&self) {
        self.map.lock().await.clear();
    }
}

impl Default for Direct {
    fn default() -> Self {
        Self {
            path_cache: Arc::new(PathCache::new()),
        }
    }
}

impl<'a> DirectCommand<'a> {
    fn new(direct: &'a Direct, program: Utf8TypedPath<'_>) -> Self {
        let program = native_path(program);
        Self {
            direct,
            program: program.as_ref().cloned().unwrap_or_default(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            stdin: None,
            stdout: None,
            stderr: None,
            error: program.err(),
        }
    }
}

impl DirectChild {
    fn new(child: tokio::process::Child) -> Self {
        Self { inner: child }
    }
}

impl Child for DirectChild {
    async fn wait(&mut self) -> Result<ExitStatus, io::Error> {
        self.inner.wait().await
    }

    async fn terminate(self) -> Result<ExitStatus, io::Error> {
        self.impl_terminate().await
    }
}

impl Command for DirectCommand<'_> {
    type Child = DirectChild;

    fn arg(&mut self, arg: &str) -> &mut Self {
        self.args.push(arg.to_owned());
        self
    }

    fn env(&mut self, key: &str, val: &str) -> &mut Self {
        self.env.insert(key.to_owned(), Some(val.to_owned()));
        self
    }

    fn env_remove(&mut self, key: &str) -> &mut Self {
        self.env.insert(key.to_owned(), None);
        self
    }

    fn current_dir(&mut self, dir: Utf8TypedPath<'_>) -> &mut Self {
        match native_path(dir) {
            Ok(dir) => self.cwd = Some(dir),
            Err(error) => self.error = Some(error),
        }
        self
    }

    fn stdin_pipe(&mut self, pipe: PipeRecv) -> io::Result<&mut Self> {
        self.stdin = Some(pipe.into_stdio()?);
        Ok(self)
    }

    fn stdout_pipe(&mut self, pipe: PipeSend) -> io::Result<&mut Self> {
        self.stdout = Some(pipe.into_stdio()?);
        Ok(self)
    }

    fn stdin_inherit(&mut self) -> io::Result<&mut Self> {
        self.stdin = Some(Stdio::inherit());
        Ok(self)
    }

    fn stdout_inherit(&mut self) -> io::Result<&mut Self> {
        self.stdout = Some(Stdio::inherit());
        Ok(self)
    }

    fn stdin_handle(&mut self, handle: DefaultHandle) -> &mut Self {
        self.stdin = Some(Stdio::from(handle));
        self
    }

    fn stdout_handle(&mut self, handle: DefaultHandle) -> &mut Self {
        self.stdout = Some(Stdio::from(handle));
        self
    }

    fn stdin_null(&mut self) -> &mut Self {
        self.stdin = Some(Stdio::null());
        self
    }

    fn stdout_null(&mut self) -> &mut Self {
        self.stdout = Some(Stdio::null());
        self
    }

    fn stderr_pipe(&mut self, pipe: PipeSend) -> io::Result<&mut Self> {
        self.stderr = Some(pipe.into_stdio()?);
        Ok(self)
    }

    fn stderr_inherit(&mut self) -> io::Result<&mut Self> {
        self.stderr = Some(Stdio::inherit());
        Ok(self)
    }

    fn stderr_inherit_stdout(&mut self) -> io::Result<&mut Self> {
        self.impl_stderr_inherit_stdout()
    }

    fn stderr_handle(&mut self, handle: DefaultHandle) -> &mut Self {
        self.stderr = Some(Stdio::from(handle));
        self
    }

    fn stderr_null(&mut self) -> &mut Self {
        self.stderr = Some(Stdio::null());
        self
    }

    async fn spawn(self) -> io::Result<Self::Child> {
        if let Some(error) = self.error {
            return Err(error);
        }
        let path_override = self
            .env
            .get("PATH")
            .map(|path| path.as_deref().unwrap_or(""));
        let resolved = self
            .direct
            .path_cache
            .resolve(&self.program, path_override, self.cwd.as_deref())
            .await;
        let resolved = resolved.ok_or_else(Direct::program_not_found_error)?;

        let mut command = TokioCommand::new(&resolved);
        command.args(&self.args);

        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }

        for (k, v) in self.env {
            match v {
                Some(val) => {
                    command.env(k, val);
                }
                None => {
                    command.env_remove(k);
                }
            }
        }

        if let Some(stdin) = self.stdin {
            command.stdin(stdin);
        }
        if let Some(stdout) = self.stdout {
            command.stdout(stdout);
        }
        if let Some(stderr) = self.stderr {
            command.stderr(stderr);
        }

        command.spawn().map(DirectChild::new)
    }
}

impl DirectOpenOptions {
    fn as_tokio(&self) -> OpenOptions {
        let mut opts = OpenOptions::new();
        opts.read(self.read)
            .write(self.write)
            .append(self.append)
            .create(self.create)
            .create_new(self.create_new)
            .truncate(self.truncate);
        self.apply_no_follow_flags(&mut opts);
        opts
    }
}

impl crate::OpenOptions for DirectOpenOptions {
    fn read(&mut self, read: bool) -> &mut Self {
        self.read = read;
        self
    }

    fn write(&mut self, write: bool) -> &mut Self {
        self.write = write;
        self
    }

    fn append(&mut self, append: bool) -> &mut Self {
        self.append = append;
        self
    }

    fn create(&mut self, create: bool) -> &mut Self {
        self.create = create;
        self
    }

    fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.create_new = create_new;
        self
    }

    fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.truncate = truncate;
        self
    }

    fn no_follow(&mut self, no_follow: bool) -> &mut Self {
        self.no_follow = no_follow;
        self
    }

    async fn open(&self, path: Utf8TypedPath<'_>) -> Result<File, io::Error> {
        self.as_tokio().open(native_path(path)?).await
    }
}

impl Direct {
    async fn copy_symlink(src: &Path, dst: &Path) -> io::Result<()> {
        let target = fs::read_link(src).await?;
        // FIXME: this won't work on Windows
        Self::impl_symlink(Path::new(""), &target, dst).await
    }

    async fn copy_local(from: &Path, to: &Path, all: bool) -> io::Result<()> {
        let metadata = fs::symlink_metadata(from).await?;

        if metadata.is_dir() {
            if !all {
                return Err(Self::directory_requires_all_error());
            }

            fs::create_dir(to).await?;
            let mut stack = vec![(from.to_path_buf(), to.to_path_buf())];
            while let Some((src_dir, dst_dir)) = stack.pop() {
                let mut entries = fs::read_dir(&src_dir).await?;
                while let Some(entry) = entries.next_entry().await? {
                    let src_path = entry.path();
                    let dst_path = dst_dir.join(entry.file_name());
                    let metadata = fs::symlink_metadata(&src_path).await?;
                    if metadata.is_dir() {
                        fs::create_dir(&dst_path).await?;
                        stack.push((src_path, dst_path));
                    } else if metadata.is_file() {
                        fs::copy(&src_path, &dst_path).await?;
                    } else if metadata.file_type().is_symlink() {
                        Self::copy_symlink(&src_path, &dst_path).await?;
                    } else {
                        return Err(io::Error::other("unsupported file type"));
                    }
                }
            }
        } else if metadata.is_file() {
            fs::copy(from, to).await?;
        } else if metadata.file_type().is_symlink() {
            Self::copy_symlink(from, to).await?;
        } else {
            return Err(io::Error::other("unsupported file type"));
        }

        Ok(())
    }

    async fn move_local(from: &Path, to: &Path, all: bool) -> io::Result<()> {
        let metadata = fs::symlink_metadata(from).await?;
        let is_dir = metadata.is_dir();

        if is_dir && !all {
            return Err(Self::directory_requires_all_error());
        }

        match fs::rename(from, to).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::CrossesDevices => {
                Self::copy_local(from, to, all).await?;
                if is_dir {
                    fs::remove_dir_all(from).await
                } else {
                    fs::remove_file(from).await
                }
            }
            Err(err) => Err(err),
        }
    }

    async fn read_dir_paths(path: &Path) -> io::Result<Vec<PathBuf>> {
        let mut read_dir = fs::read_dir(path).await?;
        let mut paths = Vec::new();
        while let Some(entry) = read_dir.next_entry().await? {
            paths.push(entry.path());
        }
        Ok(paths)
    }

    async fn remove_dir_empty_tree_local(path: &Path, ignore: bool) -> io::Result<bool> {
        let metadata = fs::symlink_metadata(path).await?;
        if !metadata.is_dir() {
            return Err(Self::not_a_directory_error());
        }

        struct Frame {
            path: PathBuf,
            entries: Vec<PathBuf>,
            next: usize,
            removable: bool,
        }

        let mut stack = vec![Frame {
            path: path.to_owned(),
            entries: Self::read_dir_paths(path).await?,
            next: 0,
            removable: true,
        }];
        let mut last_result = None;

        while let Some(frame) = stack.last_mut() {
            if let Some(child_removed) = last_result.take() {
                frame.removable &= child_removed;
            }

            if frame.next == frame.entries.len() {
                let removable = frame.removable;
                let path = frame.path.clone();
                stack.pop();
                if removable {
                    fs::remove_dir(&path).await?;
                }
                last_result = Some(removable);
                continue;
            }

            let child_path = frame.entries[frame.next].clone();
            frame.next += 1;
            let metadata = fs::symlink_metadata(&child_path).await?;
            if metadata.is_dir() {
                stack.push(Frame {
                    path: child_path.clone(),
                    entries: Self::read_dir_paths(&child_path).await?,
                    next: 0,
                    removable: true,
                });
            } else if ignore {
                frame.removable = false;
            } else {
                return Err(Self::directory_not_empty_error());
            }
        }

        Ok(last_result.unwrap_or(false))
    }
}

impl Vfs for Direct {
    type OpenOptions<'a>
        = DirectOpenOptions
    where
        Self: 'a;
    type Command<'a>
        = DirectCommand<'a>
    where
        Self: 'a;

    fn open_options(&self) -> Self::OpenOptions<'_> {
        DirectOpenOptions::default()
    }

    fn command(&self, program: Utf8TypedPath<'_>) -> Self::Command<'_> {
        DirectCommand::new(self, program)
    }

    async fn read_dir(&self, path: Utf8TypedPath<'_>) -> Result<ReadDir, io::Error> {
        ReadDir::open(&native_path(path)?).await
    }

    async fn which(
        &self,
        program: Utf8TypedPath<'_>,
        path: Option<&str>,
        cwd: Option<Utf8TypedPath<'_>>,
    ) -> Result<Option<Utf8TypedPathBuf>, io::Error> {
        let program = native_path(program)?;
        let cwd = cwd.map(native_path).transpose()?;
        self.path_cache
            .resolve(&program, path, cwd.as_deref())
            .await
            .map(typed_path)
            .transpose()
    }

    async fn well_known_path(
        &self,
        key: WellKnownPath,
        env: &HashMap<String, Option<String>>,
    ) -> Result<Utf8TypedPathBuf, io::Error> {
        let path = match key {
            WellKnownPath::HomeDir => Self::home_dir_platform(env),
            WellKnownPath::CacheDir => Self::cache_dir_platform(env),
        }?;
        typed_path(path)
    }

    async fn clear_cache(&self) -> Result<(), io::Error> {
        self.path_cache.clear().await;
        Ok(())
    }

    async fn xattrs(
        &self,
        path: Utf8TypedPath<'_>,
        namespace: XattrNamespace<'_>,
        follow: bool,
    ) -> Result<Vec<XattrEntry>, io::Error> {
        self.impl_xattrs(&native_path(path)?, namespace, follow)
            .await
    }

    async fn streams(
        &self,
        path: Utf8TypedPath<'_>,
        follow: bool,
    ) -> Result<Vec<StreamEntry>, io::Error> {
        self.impl_streams(&native_path(path)?, follow).await
    }

    async fn xattr(
        &self,
        path: Utf8TypedPath<'_>,
        name: &str,
        namespace: Option<&str>,
        follow: bool,
    ) -> Result<Vec<u8>, io::Error> {
        self.impl_xattr(&native_path(path)?, name, namespace, follow)
            .await
    }

    async fn set_xattr(
        &self,
        path: Utf8TypedPath<'_>,
        name: &str,
        namespace: Option<&str>,
        value: &[u8],
        follow: bool,
    ) -> Result<(), io::Error> {
        self.impl_set_xattr(&native_path(path)?, name, namespace, value, follow)
            .await
    }

    async fn remove_xattr(
        &self,
        path: Utf8TypedPath<'_>,
        name: &str,
        namespace: Option<&str>,
        follow: bool,
    ) -> Result<(), io::Error> {
        self.impl_remove_xattr(&native_path(path)?, name, namespace, follow)
            .await
    }

    async fn file_xattrs(
        &self,
        file: &File,
        namespace: XattrNamespace<'_>,
    ) -> Result<Vec<XattrEntry>, io::Error> {
        self.impl_file_xattrs(file, namespace).await
    }

    async fn file_xattr(
        &self,
        file: &File,
        name: &str,
        namespace: Option<&str>,
    ) -> Result<Vec<u8>, io::Error> {
        self.impl_file_xattr(file, name, namespace).await
    }

    async fn file_streams(&self, file: &File) -> Result<Vec<StreamEntry>, io::Error> {
        self.impl_file_streams(file).await
    }

    async fn file_set_xattr(
        &self,
        file: &File,
        name: &str,
        namespace: Option<&str>,
        value: &[u8],
    ) -> Result<(), io::Error> {
        self.impl_file_set_xattr(file, name, namespace, value).await
    }

    async fn file_remove_xattr(
        &self,
        file: &File,
        name: &str,
        namespace: Option<&str>,
    ) -> Result<(), io::Error> {
        self.impl_file_remove_xattr(file, name, namespace).await
    }

    async fn remove(
        &self,
        path: Utf8TypedPath<'_>,
        all: bool,
        ignore: bool,
    ) -> Result<(), io::Error> {
        let path = native_path(path)?;
        let path = path.as_path();
        let result = if all {
            match fs::symlink_metadata(path).await {
                Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path).await,
                Ok(_) => fs::remove_file(path).await,
                Err(e) => Err(e),
            }
        } else {
            fs::remove_file(path).await
        };
        match result {
            Ok(()) => Ok(()),
            Err(e) if ignore && e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn metadata(&self, path: Utf8TypedPath<'_>) -> Result<Metadata, io::Error> {
        fs::metadata(native_path(path)?)
            .await
            .map(crate::metadata_from_std)
    }

    async fn file_fs_metadata(&self, file: &File) -> Result<FsMetadata, io::Error> {
        let file = file.try_clone().await?;
        tokio::task::spawn_blocking(move || Self::fs_metadata_from_file(&file))
            .await
            .unwrap_or_else(|_| Err(io::Error::other("failed to join fs metadata query task")))
    }

    async fn fs_metadata(
        &self,
        path: Utf8TypedPath<'_>,
        follow: bool,
    ) -> Result<FsMetadata, io::Error> {
        let path = native_path(path)?;
        tokio::task::spawn_blocking(move || Self::fs_metadata_from_path(&path, follow))
            .await
            .unwrap_or_else(|_| Err(io::Error::other("failed to join fs metadata query task")))
    }

    async fn create_dir(&self, path: Utf8TypedPath<'_>, all: bool) -> Result<(), io::Error> {
        let path = native_path(path)?;
        if all {
            fs::create_dir_all(path).await
        } else {
            fs::create_dir(path).await
        }
    }

    async fn remove_dir(
        &self,
        path: Utf8TypedPath<'_>,
        all: bool,
        ignore: bool,
    ) -> Result<(), io::Error> {
        let path = native_path(path)?;
        let result = if all {
            Self::remove_dir_empty_tree_local(&path, ignore)
                .await
                .map(|_| ())
        } else {
            fs::remove_dir(path).await
        };
        match result {
            Ok(()) => Ok(()),
            Err(e) if ignore && e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn copy(
        &self,
        from: Utf8TypedPath<'_>,
        to: Utf8TypedPath<'_>,
        all: bool,
    ) -> Result<(), io::Error> {
        Self::copy_local(&native_path(from)?, &native_path(to)?, all).await
    }

    async fn rename(
        &self,
        from: Utf8TypedPath<'_>,
        to: Utf8TypedPath<'_>,
    ) -> Result<(), io::Error> {
        fs::rename(native_path(from)?, native_path(to)?).await
    }

    async fn move_(
        &self,
        from: Utf8TypedPath<'_>,
        to: Utf8TypedPath<'_>,
        all: bool,
    ) -> Result<(), io::Error> {
        Self::move_local(&native_path(from)?, &native_path(to)?, all).await
    }

    async fn symlink(
        &self,
        cwd: Utf8TypedPath<'_>,
        src: Utf8TypedPath<'_>,
        dst: Utf8TypedPath<'_>,
    ) -> Result<(), io::Error> {
        Self::impl_symlink(&native_path(cwd)?, &native_path(src)?, &native_path(dst)?).await
    }

    async fn hard_link(
        &self,
        src: Utf8TypedPath<'_>,
        dst: Utf8TypedPath<'_>,
    ) -> Result<(), io::Error> {
        fs::hard_link(native_path(src)?, native_path(dst)?).await
    }

    async fn symlink_dir(
        &self,
        src: Utf8TypedPath<'_>,
        dst: Utf8TypedPath<'_>,
    ) -> Result<(), io::Error> {
        Self::impl_symlink_dir(&native_path(src)?, &native_path(dst)?).await
    }

    async fn symlink_file(
        &self,
        src: Utf8TypedPath<'_>,
        dst: Utf8TypedPath<'_>,
    ) -> Result<(), io::Error> {
        Self::impl_symlink_file(&native_path(src)?, &native_path(dst)?).await
    }

    async fn symlink_metadata(&self, path: Utf8TypedPath<'_>) -> Result<Metadata, io::Error> {
        fs::symlink_metadata(native_path(path)?)
            .await
            .map(crate::metadata_from_std)
    }

    async fn attrs(&self, path: Utf8TypedPath<'_>, follow: bool) -> Result<Attrs, io::Error> {
        self.impl_attrs(&native_path(path)?, follow).await
    }

    async fn set_attrs(&self, path: Utf8TypedPath<'_>, attrs: Attrs) -> Result<(), io::Error> {
        self.impl_set_attrs(&native_path(path)?, attrs).await
    }

    async fn canonicalize(&self, path: Utf8TypedPath<'_>) -> Result<Utf8TypedPathBuf, io::Error> {
        typed_path(self.impl_canonicalize(&native_path(path)?).await?)
    }

    async fn read_link(&self, path: Utf8TypedPath<'_>) -> Result<Utf8TypedPathBuf, io::Error> {
        typed_path(fs::read_link(native_path(path)?).await?)
    }

    async fn glob(
        &self,
        pattern: impl Into<String>,
        root: Utf8TypedPath<'_>,
        follow_symlinks: bool,
        max_depth: Option<usize>,
    ) -> Result<Vec<Utf8TypedPathBuf>, io::Error> {
        let pattern = pattern.into();
        let root = native_path(root)?;
        tokio::task::spawn_blocking(move || {
            let (prefix, glob) = Glob::new(&pattern)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid glob pattern"))?
                .partition();
            let walk_root = root.join(&prefix);

            let mut behavior = WalkBehavior::default();
            if follow_symlinks {
                behavior.link = LinkBehavior::ReadTarget;
            }
            if let Some(depth) = max_depth {
                behavior.depth =
                    DepthBehavior::Max(DepthMax(depth.saturating_sub(prefix.components().count())));
            }

            let mut paths = Vec::new();
            let walk = match glob {
                Some(g) => g.walk_with_behavior(&walk_root, behavior),
                None => Glob::tree().walk_with_behavior(&walk_root, behavior),
            };

            for entry in walk {
                paths.push(prefix.join(entry?.root_relative_paths().1));
            }

            paths.sort();
            paths.into_iter().map(typed_path).collect()
        })
        .await
        .unwrap_or_else(|e| Err(io::Error::other(e)))
    }

    async fn set_permissions(
        &self,
        path: Utf8TypedPath<'_>,
        perm: Permissions,
    ) -> Result<(), io::Error> {
        self.impl_set_permissions(&native_path(path)?, perm).await
    }

    async fn set_times(
        &self,
        path: Utf8TypedPath<'_>,
        accessed: Option<(i64, u32)>,
        modified: Option<(i64, u32)>,
        created: Option<(i64, u32)>,
    ) -> Result<(), io::Error> {
        self.impl_set_times(&native_path(path)?, accessed, modified, created)
            .await
    }

    async fn chown(
        &self,
        path: Utf8TypedPath<'_>,
        user: Option<ChownIdentity>,
        group: Option<ChownIdentity>,
        follow: bool,
    ) -> Result<(), io::Error> {
        self.impl_chown(&native_path(path)?, user, group, follow)
            .await
    }
}
