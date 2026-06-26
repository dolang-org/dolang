use std::{
    cell::Cell,
    error,
    fmt::{self, Display},
    fs,
    io::Read,
    marker::PhantomData,
    ops::{ControlFlow, Deref},
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

use dolang_compile::Compiler;
use dolang_runtime::{
    error::{Error, ErrorKind, ResultExt},
    strand::Strand,
    value::{Output, Value, view::View},
    vm::{Builder, Bytecode, Stateful},
};

use tokio::{
    sync::{
        Mutex,
        mpsc::{UnboundedSender, error::SendError, unbounded_channel},
        oneshot,
    },
    task::JoinHandle,
};

use crate::backend::{Import, Settings};

#[derive(Debug)]
pub(crate) enum Cmd {
    Shutdown,
    ReadSettings(
        PathBuf,
        oneshot::Sender<Result<Settings, Box<dyn error::Error + Send + 'static>>>,
    ),
}

#[derive(Debug)]
pub(crate) struct Vm {
    send: UnboundedSender<Cmd>,
    handle: Mutex<Option<JoinHandle<Result<(), String>>>>,
}

struct VmInner {
    deadline: Cell<Instant>,
}

#[derive(Clone)]
struct VmState<'v>(Rc<VmInner>, PhantomData<fn() -> &'v ()>);

impl<'v> Deref for VmState<'v> {
    type Target = VmInner;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

struct VmTag;

impl<'v> Stateful<'v> for VmState<'v> {
    type Tag = VmTag;
}

#[derive(Debug)]
struct Stop;

impl Display for Stop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "compilation stopped")
    }
}

impl error::Error for Stop {}

#[derive(Debug)]
struct VmError(String);

impl Display for VmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl error::Error for VmError {}

impl VmInner {
    fn compiler<'a>(&self, path: &'a Path, source: &'a [u8]) -> Compiler<'a> {
        Compiler::new(path, source)
    }

    /// Parse a single item entry within a module's import list.
    ///
    /// `key` is `None` for positional entries (array element or numeric key)
    /// and `Some(k)` for named entries (sym or string key in a dict/record).
    /// `value` is the item name or binding target.
    fn parse_prelude_item<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        module: &str,
        key: Option<&Value<'v>>,
        value: &Value<'v>,
    ) -> Result<Import, String> {
        let vm = strand.vm();

        // Resolve item name from key (if present) or treat as positional
        let item_name = match key {
            None => None,
            Some(k) => match k.view(vm) {
                View::Sym(sym) => Some(sym.as_str(vm).to_owned()),
                View::Str(s) => Some(s.into()),
                View::Int(_) => None, // numeric index → positional
                _ => return Err("item in prelude was of unexpected type".to_owned()),
            },
        };

        // Resolve binding target from value
        let bind = match value.view(vm) {
            View::Str(s) => s.into(),
            View::Sym(sym) => sym.as_str(vm).to_owned(),
            _ => return Err("item in prelude was of unexpected type".to_owned()),
        };

        Ok(match item_name {
            None => Import::Item(module.to_owned(), bind),
            Some(item) => Import::ItemAs(module.to_owned(), item, bind),
        })
    }

    fn parse_prelude_entry<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        key: Option<&Value<'v>>,
        value: &Value<'v>,
    ) -> Result<Vec<Import>, String> {
        let vm = strand.vm();

        // Resolve module name from key (if present) or treat as positional
        let module_name = match key {
            None => None,
            Some(k) => match k.view(vm) {
                View::Sym(sym) => Some(sym.as_str(vm).to_owned()),
                View::Str(s) => Some(s.into()),
                View::Int(_) => None, // numeric index → positional
                _ => return Err("module in prelude was of unexpected type".to_owned()),
            },
        };

        match module_name {
            None => {
                // Positional: value is the module name itself
                let module = match value.view(vm) {
                    View::Str(s) => s.into(),
                    View::Sym(sym) => sym.as_str(vm).to_owned(),
                    _ => return Err("module in prelude was of unexpected type".to_owned()),
                };
                Ok(vec![Import::Module(module)])
            }
            Some(module) => match value.view(vm) {
                // "module": "bind-as"
                View::Str(bind) => Ok(vec![Import::ModuleAs(module, bind.into())]),
                View::Sym(bind) => Ok(vec![Import::ModuleAs(module, bind.as_str(vm).to_owned())]),
                // "module": [item, ...]
                View::Array(handle) => {
                    let len = handle
                        .len(strand)
                        .map_err(|_| "concurrent access to prelude array".to_owned())?;
                    let mut imports = Vec::new();
                    for i in 0..len {
                        let import = strand.with_slots_sync(|strand, [mut elem]| {
                            handle
                                .get(strand, i, &mut elem)
                                .map_err(|_| "concurrent access to prelude array".to_owned())?;
                            self.parse_prelude_item(strand, &module, None, &elem)
                        })?;
                        imports.push(import);
                    }
                    Ok(imports)
                }
                // "module": {item: "bind", ...}  or  (item: "bind", ...)
                View::Dict(handle) => {
                    let mut imports = Vec::new();
                    let mut pairs = handle.pairs();
                    strand.with_slots_sync(|strand, [mut k, mut v]| -> Result<(), String> {
                        while pairs
                            .next(strand, &mut k, &mut v)
                            .map_err(|_| "concurrent access to prelude dict".to_owned())?
                        {
                            imports.push(self.parse_prelude_item(strand, &module, Some(&k), &v)?);
                        }
                        Ok(())
                    })?;
                    Ok(imports)
                }
                View::Record(handle) => {
                    let mut imports = Vec::new();
                    let mut pairs = handle.pairs();
                    strand.with_slots_sync(|strand, [mut k, mut v]| -> Result<(), String> {
                        while pairs
                            .next(strand, &mut k, &mut v)
                            .map_err(|_| "concurrent access to prelude record".to_owned())?
                        {
                            imports.push(self.parse_prelude_item(strand, &module, Some(&k), &v)?);
                        }
                        Ok(())
                    })?;
                    Ok(imports)
                }
                _ => {
                    Err("module in prelude was not a string, array, or map as required".to_owned())
                }
            },
        }
    }

    fn parse_settings_value<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        settings: &Value<'v>,
    ) -> Result<Settings, String> {
        let mut prelude = Vec::new();

        macro_rules! process_pairs {
            ($handle:expr) => {{
                let mut pairs = $handle.pairs();
                strand.with_slots_sync(|strand, [mut k, mut v]| {
                    while pairs
                        .next(strand, &mut k, &mut v)
                        .map_err(|_| "concurrent access to settings".to_owned())?
                    {
                        let key_str = match k.view(strand.vm()) {
                            View::Str(s) => s.into(),
                            View::Sym(sym) => sym.as_str(strand.vm()).to_owned(),
                            _ => return Err("settings key was not a string".to_owned()),
                        };
                        match key_str.as_str() {
                            "prelude" => prelude.extend(self.parse_prelude_value(strand, &v)?),
                            _ => return Err("unexpected key in settings".to_owned()),
                        }
                    }
                    Ok(())
                })?
            }};
        }

        match settings.view(strand.vm()) {
            View::Dict(handle) => process_pairs!(handle),
            View::Record(handle) => process_pairs!(handle),
            _ => return Err("settings are not a map as expected".to_owned()),
        }

        Ok(Settings { prelude })
    }

    fn parse_prelude_value<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        value: &Value<'v>,
    ) -> Result<Vec<Import>, String> {
        macro_rules! process_map {
            ($handle:expr) => {{
                let mut pairs = $handle.pairs();
                let mut entries = Vec::new();
                strand.with_slots_sync(|strand, [mut pk, mut pv]| -> Result<(), String> {
                    while pairs
                        .next(strand, &mut pk, &mut pv)
                        .map_err(|_| "concurrent access to prelude".to_owned())?
                    {
                        entries.extend(self.parse_prelude_entry(strand, Some(&pk), &pv)?);
                    }
                    Ok(())
                })?;
                entries
            }};
        }

        match value.view(strand.vm()) {
            View::Array(handle) => {
                let n = handle
                    .len(strand)
                    .map_err(|_| "concurrent access to prelude".to_owned())?;
                let mut entries = Vec::new();
                for j in 0..n {
                    let entry = strand.with_slots_sync(|strand, [mut elem]| {
                        handle
                            .get(strand, j, &mut elem)
                            .map_err(|_| "concurrent access to prelude".to_owned())?;
                        self.parse_prelude_entry(strand, None, &elem)
                    })?;
                    entries.extend(entry);
                }
                Ok(entries)
            }
            View::Dict(handle) => Ok(process_map!(handle)),
            View::Record(handle) => Ok(process_map!(handle)),
            _ => Err("prelude is not an array or map as expected".to_owned()),
        }
    }

    fn parse_settings<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        settings: &Value<'v>,
    ) -> Result<Settings, Error<'v, 's>> {
        self.parse_settings_value(strand, settings).into_do(strand)
    }

    fn hashed_path(&self, path: &Path) -> PathBuf {
        dirs::cache_dir()
            .or_else(dirs::data_local_dir)
            .expect("no cache directory available")
            .join("dolang-lsp")
            .join("bytecode-cache")
            .join(format!(
                "{}.dolc",
                blake3::hash(path.as_os_str().as_encoded_bytes()).to_hex()
            ))
    }

    fn read_file(&self, path: &Path) -> Option<Vec<u8>> {
        fs::read(path).ok()
    }

    fn write_file(&self, path: &Path, content: &[u8]) -> bool {
        path.parent()
            .and_then(|dir| fs::create_dir_all(dir).ok())
            .and_then(|()| fs::write(path, content).ok())
            .is_some()
    }

    fn file_is_newer(&self, older: &Path, newer: &Path) -> bool {
        let older = fs::metadata(older).and_then(|older| older.modified());
        let newer = fs::metadata(newer).and_then(|newer| newer.modified());
        older
            .and_then(|older| newer.map(|newer| newer > older))
            .unwrap_or(false)
    }

    async fn load_file<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        path: &Path,
        mut out: impl Output<'v>,
    ) -> Result<(), Error<'v, 's>> {
        let hashed = self.hashed_path(path);
        if self.file_is_newer(path, &hashed)
            && let Some(content) = self.read_file(&hashed)
        {
            log::debug!(
                "using cached bytecode: {} ({})",
                path.display(),
                hashed.display()
            );
            let bytecode = Bytecode::new(content);
            if let Err(e) = bytecode.run(strand, &mut out).await {
                if e.kind() == ErrorKind::Bytecode {
                    // Not a fatal error, as the bytecode format can change backward-incompatibly
                    log::debug!(
                        "cached bytecode error: {} ({}): {e}",
                        path.display(),
                        hashed.display()
                    )
                } else {
                    return Err(e);
                }
            }
        }
        let mut file = fs::File::open(path).unwrap();
        let mut content = Vec::new();
        file.read_to_end(&mut content).unwrap();
        let compiler = self.compiler(path, &content);
        let mut bytecode = Vec::new();
        compiler
            .compile(&mut bytecode, &mut |_| -> ControlFlow<Stop> {
                ControlFlow::Continue(())
            })
            .map_err(|e| Error::compile(strand, e))?;
        // Save cache
        if !self.write_file(&hashed, &bytecode) {
            // Failure here is not fatal
            log::warn!(
                "could not cache bytecode: {} ({})",
                path.display(),
                hashed.display()
            )
        }
        let bytecode = Bytecode::new(bytecode);
        bytecode.run(strand, out).await
    }

    async fn read_settings<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        path: &Path,
    ) -> Result<Settings, Box<dyn error::Error + Send + 'static>> {
        strand
            .with_slots(async move |strand, [mut res]| {
                self.load_file(strand, path, &mut res).await?;
                self.parse_settings(strand, &res)
            })
            .await
            .map_err(|e: Error| {
                Box::new(VmError(e.to_string())) as Box<dyn error::Error + Send + 'static>
            })
    }
}

const ALLOC_LIMIT: usize = 4 * 1024 * 1024;
const TIME_LIMIT: Duration = Duration::from_millis(250);

impl Vm {
    pub(crate) fn new() -> Self {
        let (send, mut recv) = unbounded_channel();
        let handle = tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(Builder::build(async |config| {
                let this = VmState(
                    Rc::new(VmInner {
                        deadline: Cell::new(Instant::now()),
                    }),
                    PhantomData,
                );
                let this = config.register_state(this.clone());
                config.trap(move |strand| {
                    if Instant::now() > this.deadline.get() {
                        return Err(Error::abort(strand, "timeout exceeded"));
                    }
                    if strand.gc_allocated_size() > ALLOC_LIMIT {
                        return Err(Error::abort(strand, "memory usage limit exceeded"));
                    }
                    Ok(())
                });
                config
                    .enter(async |strand| {
                        while let Some(cmd) = recv.recv().await {
                            this.deadline.set(Instant::now() + TIME_LIMIT);
                            match cmd {
                                Cmd::Shutdown => break,
                                Cmd::ReadSettings(path, send) => {
                                    let _ = send.send(this.read_settings(strand, &path).await);
                                }
                            }
                        }
                        Ok(())
                    })
                    .await
            }))
        });
        Vm {
            send,
            handle: Mutex::new(Some(handle)),
        }
    }

    pub(crate) async fn send(&self, cmd: Cmd) -> Result<(), SendError<Cmd>> {
        self.send.send(cmd)
    }

    pub(crate) async fn join(&self) -> Result<(), String> {
        let _ = self.send(Cmd::Shutdown).await;
        self.handle
            .lock()
            .await
            .take()
            .expect("attempt to join twice")
            .await
            .map_err(|e| e.to_string())?
    }
}
