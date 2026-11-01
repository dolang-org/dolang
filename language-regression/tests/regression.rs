mod detail {
    use dolang::runtime::object::fmt;

    use dolang::{
        compile::{self, Config, Mode},
        runtime::{
            self, Args, Bytecode, Error, Instance, Object, Output, Slot, Sym, call,
            error::ErrorKind,
            io::{self, ByteHandle, ByteIo, IntoByteIo, IoCaps, IoResultExt as _},
            object::{
                FlagLike, FlagLikeExt, Flags, FlagsInstanceExt, FlagsTypeExt, InterfaceTag,
                Request, TypeBuilder,
            },
            unpack,
            vm::Builder,
        },
    };
    use dolang_runtime::strand::Strand;
    use futures::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt, Cursor};
    use std::{
        io::SeekFrom,
        mem,
        ops::{BitAnd, BitOr, BitXor, Not},
        path::{Path, PathBuf},
        pin::Pin,
        sync::{Arc, Mutex},
        task::{Context as TaskContext, Poll},
    };

    use dolang::runtime::value::fmt::Format;

    const MOD_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/mod");

    struct DummyAsync<T>(Option<T>, bool);

    impl<T> DummyAsync<T> {
        fn new(value: T) -> Self {
            Self(Some(value), false)
        }
    }

    impl<T: Unpin> Future for DummyAsync<T> {
        type Output = T;

        fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
            let slf = self.get_mut();
            if slf.1 {
                Poll::Ready(slf.0.take().unwrap())
            } else {
                slf.1 = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    fn compile<'a>(
        path: &'a Path,
        content: &'a [u8],
        module: Option<&'a str>,
    ) -> (
        Result<Bytecode, compile::Error>,
        Vec<dolang_private_test::Located>,
        Vec<dolang_private_test::Directive>,
    ) {
        let mut config = Config::new();
        let directives = dolang_private_test::configure_compiler(&mut config, content);
        // Add extra prelude items specific to dolang tests (from regression2 module)
        config
            .prelude()
            .import_items("regression2")
            .items([
                "async",
                "callme",
                "makefoo",
                "noop",
                "MIRI",
                "DEBUG",
                "TestFlags",
                "coerce_flags",
            ])
            .commit();
        if let Some(name) = module {
            config.mode(Mode::Module { name: name.into() });
        }
        let mut out = Vec::new();
        let source = std::str::from_utf8(content)
            .unwrap_or_else(|e| panic!("{}: not UTF-8: {e}", path.display()));
        let unit = config.unit(path, source);
        let diags: Vec<_> = (unit.diagnostics())
            .map(|diag| dolang_private_test::Located::new(diag, &unit))
            .collect();
        let res = unit.emit(&mut out);
        (
            res.map(|_| Bytecode::new(out).expect("compiler output must verify")),
            diags,
            directives,
        )
    }

    #[derive(Clone, Debug)]
    struct Foo;

    impl<'v> Object<'v> for Foo {
        const NAME: &'v str = "Foo";
        const MODULE: &'v str = "regression2";

        type Annex = ();
        type Type = i64;
        type TypeAnnex = ();

        async fn type_method<'a, 's>(
            _this: runtime::Type<'v, Self>,
            strand: &'a mut Strand<'v, 's>,
            method: Sym<'v, 'a>,
            args: Args<'v, 'a>,
            out: Slot<'v, 'a>,
        ) -> runtime::Result<'v, 's, ()> {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            if method.as_str(strand) != "dynamic_method" {
                return Err(Error::field(strand, method));
            }
            Output::set(strand, out, "dynamic method");
            Ok(())
        }

        fn type_get<'a, 's>(
            this: runtime::Type<'v, Self>,
            strand: &'a mut Strand<'v, 's>,
            field: Sym<'v, 'a>,
            out: Slot<'v, 'a>,
        ) -> runtime::Result<'v, 's, ()> {
            if field.as_str(strand) != "dynamic_property" {
                return Err(Error::field(strand, field));
            }
            let value = *this.borrow(strand)?;
            Output::set(strand, out, value);
            Ok(())
        }

        fn type_set<'a, 's>(
            this: runtime::Type<'v, Self>,
            strand: &'a mut Strand<'v, 's>,
            field: Sym<'v, 'a>,
            value: Slot<'v, 'a>,
        ) -> runtime::Result<'v, 's, ()> {
            if field.as_str(strand) != "dynamic_property" {
                return Err(Error::field(strand, field));
            }
            *this.borrow_mut(strand)? = value.to_i64(strand)?;
            Ok(())
        }

        async fn call<'a, 's>(
            _this: Instance<'v, 'a, Self>,
            strand: &'a mut Strand<'v, 's>,
            _args: Args<'v, 'a>,
            out: Slot<'v, 'a>,
        ) -> runtime::Result<'v, 's, ()> {
            Output::set(strand, out, 42);
            Ok(())
        }

        async fn method<'a, 's>(
            _this: Instance<'v, 'a, Self>,
            strand: &'a mut Strand<'v, 's>,
            method: Sym<'v, 'a>,
            _args: Args<'v, 'a>,
            out: Slot<'v, 'a>,
        ) -> runtime::Result<'v, 's, ()> {
            Output::set(strand, out, method.as_str(strand));
            Ok(())
        }

        fn display<'a, 's>(
            this: Instance<'v, 'a, Self>,
            strand: &'a mut Strand<'v, 's>,
            w: &mut dyn Format<'v>,
        ) -> runtime::Result<'v, 's, ()> {
            Self::debug(this, strand, w)
        }

        fn debug<'a, 's>(
            _this: Instance<'v, 'a, Self>,
            strand: &'a mut Strand<'v, 's>,
            w: &mut dyn Format<'v>,
        ) -> runtime::Result<'v, 's, ()> {
            fmt!(strand, w, "foo")
        }
    }

    /// Interface lent by [`Counter`] to an interface query.
    trait Bump {
        fn bump(&mut self) -> i64;
    }

    struct BumpTag;

    impl InterfaceTag for BumpTag {
        type Interface<'v, 'a> = dyn Bump + 'a;
    }

    /// Native counter providing [`BumpTag`], for testing interface queries.
    struct Counter {
        n: i64,
        closed: bool,
    }

    impl Bump for Counter {
        fn bump(&mut self) -> i64 {
            self.n += 1;
            self.n
        }
    }

    impl<'v> Object<'v> for Counter {
        const NAME: &'v str = "Counter";
        const MODULE: &'v str = "regression2";

        type Annex = ();
        type Type = ();
        type TypeAnnex = ();

        async fn new<'a, 's>(
            this: runtime::Type<'v, Self>,
            strand: &'a mut Strand<'v, 's>,
            args: Args<'v, 'a>,
            out: Slot<'v, 'a>,
        ) -> runtime::Result<'v, 's, ()> {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            this.create(
                strand,
                Counter {
                    n: 0,
                    closed: false,
                },
                out,
            );
            Ok(())
        }

        fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
            builder
                .get("value", |this, strand, out| {
                    let n = this.borrow(strand)?.n;
                    Output::set(strand, out, n);
                    Ok(())
                })
                // Stops providing `BumpTag`
                .method("close", async move |this, strand, args, _out| {
                    let ([], []) = unpack!(strand, args, 0, 0)?;
                    this.borrow_mut(strand)?.closed = true;
                    Ok(())
                })
                // Calls `f` while holding a mutable borrow of the counter
                .method("while_borrowed", async move |this, strand, args, out| {
                    let ([f], []) = unpack!(strand, args, 1, 0)?;
                    let _guard = this.borrow_mut(strand)?;
                    call!(strand, f, out).await
                })
        }

        fn provide<'a, 's>(
            this: Instance<'v, 'a, Self>,
            request: &mut Request<'v, 's, '_>,
        ) -> runtime::Result<'v, 's, ()> {
            if request.wants::<BumpTag>() {
                let mut state = this.borrow_mut(request)?;
                if !state.closed {
                    request.provide::<BumpTag>(&mut *state)?;
                }
            }
            Ok(())
        }
    }

    /// Bumps `value` through [`BumpTag`], returning the count, or nil if nothing provides it.
    fn bump<'v, 's>(
        strand: &mut Strand<'v, 's>,
        value: &runtime::Value<'v>,
        out: Slot<'v, '_>,
        fail: bool,
    ) -> runtime::Result<'v, 's, ()> {
        let res = value.query::<BumpTag, _>(strand, |strand, iface| {
            let n = iface.bump();
            if fail {
                return Err(Error::value(strand, "consumer failed"));
            }
            Ok(n)
        });
        match res {
            Ok(n) => Output::set(strand, out, n),
            Err(e) if e.kind() == ErrorKind::Unsupported => {}
            Err(e) => return Err(e),
        }
        Ok(())
    }

    /// Native in-memory buffer providing [`ByteIo`], with simulated read-ahead.
    struct ByteBuf {
        cursor: Option<Cursor<Vec<u8>>>,
        ahead: Vec<u8>,
        caps: IoCaps,
        /// Where a duplex buffer's writes go, kept after the handle is taken.
        written: Option<Arc<Mutex<Vec<u8>>>>,
        /// The error every read fails with, as a mode character.
        fail: Option<char>,
    }

    /// Fails every read, with malformed data (`v`) or an OS error (`o`).
    struct FailRead(char);

    impl AsyncRead for FailRead {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
            _buf: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Err(match self.0 {
                'v' => std::io::Error::new(std::io::ErrorKind::InvalidData, "bad data"),
                _ => std::io::Error::from_raw_os_error(2),
            }))
        }
    }

    /// Reads from a cursor and writes to a separate sink, like a socket.
    struct Duplex {
        reader: Cursor<Vec<u8>>,
        written: Arc<Mutex<Vec<u8>>>,
    }

    impl AsyncRead for Duplex {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
            buf: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.get_mut().reader).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for Duplex {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.written.lock().unwrap().extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl ByteBuf {
        fn handle(&self, cursor: Cursor<Vec<u8>>) -> ByteHandle {
            if let Some(fail) = self.fail {
                return ByteHandle::read(FailRead(fail));
            }
            if let Some(written) = &self.written {
                return ByteHandle::read_write(Duplex {
                    reader: cursor,
                    written: written.clone(),
                });
            }
            match (
                self.caps.contains(IoCaps::WRITE),
                self.caps.contains(IoCaps::SEEK),
            ) {
                (false, false) => ByteHandle::read(cursor),
                (false, true) => ByteHandle::read_seek(cursor),
                (true, false) => ByteHandle::read_write(cursor),
                (true, true) => ByteHandle::read_write_seek(cursor),
            }
        }

        fn cursor(&self) -> &Cursor<Vec<u8>> {
            self.cursor.as_ref().expect("handle already taken")
        }
    }

    impl<'v> IntoByteIo<'v> for ByteBuf {
        fn caps<'s>(&mut self, _strand: &mut Strand<'v, 's>) -> runtime::Result<'v, 's, IoCaps> {
            self.cursor();
            Ok(self.caps)
        }

        fn get<'s>(&mut self, _strand: &mut Strand<'v, 's>) -> runtime::Result<'v, 's, ByteHandle> {
            let cursor = self.cursor.take().expect("handle already taken");
            Ok(self.handle(cursor).with_prefix(mem::take(&mut self.ahead)))
        }
    }

    impl<'v> Object<'v> for ByteBuf {
        const NAME: &'v str = "ByteBuf";
        const MODULE: &'v str = "regression2";

        type Annex = ();
        type Type = ();
        type TypeAnnex = ();

        /// `ByteBuf data ahead modes`: `ahead` bytes of `data` are already read ahead, and
        /// `modes` adds `w` (write) and `s` (seek) to reading. `d` (duplex) writes to a
        /// separate sink that `written` returns, and cannot seek. `v` fails reads with
        /// malformed data and `o` with an OS error.
        async fn new<'a, 's>(
            this: runtime::Type<'v, Self>,
            strand: &'a mut Strand<'v, 's>,
            args: Args<'v, 'a>,
            out: Slot<'v, 'a>,
        ) -> runtime::Result<'v, 's, ()> {
            let ([data, ahead, modes], []) = unpack!(strand, args, 3, 0)?;
            let data = bin_arg(strand, &data)?;
            let ahead = ahead
                .as_int(strand)
                .and_then(|n| usize::try_from(n).ok())
                .filter(|&n| n <= data.len())
                .ok_or_else(|| Error::value(strand, "bad read-ahead"))?;
            let mut caps = IoCaps::READ;
            let mut written = None;
            let mut fail = None;
            let modes = modes
                .as_str(strand.vm())
                .map(|s| s.to_string())
                .ok_or_else(|| Error::value(strand, "bad modes"))?;
            for c in modes.chars() {
                caps |= match c {
                    'w' => IoCaps::WRITE,
                    's' => IoCaps::SEEK,
                    'd' => {
                        written = Some(Arc::default());
                        IoCaps::WRITE
                    }
                    'v' | 'o' => {
                        fail = Some(c);
                        IoCaps::empty()
                    }
                    _ => return Err(Error::value(strand, "bad modes")),
                };
            }
            if written.is_some() && caps.contains(IoCaps::SEEK) {
                return Err(Error::value(strand, "bad modes"));
            }
            let ahead_bytes = data[..ahead].to_vec();
            let mut cursor = Cursor::new(data);
            cursor.set_position(ahead as u64);
            this.create(
                strand,
                ByteBuf {
                    cursor: Some(cursor),
                    ahead: ahead_bytes,
                    caps,
                    written,
                    fail,
                },
                out,
            );
            Ok(())
        }

        fn provide<'a, 's>(
            this: Instance<'v, 'a, Self>,
            request: &mut Request<'v, 's, '_>,
        ) -> runtime::Result<'v, 's, ()> {
            if request.wants::<ByteIo>() {
                let mut state = this.borrow_mut(request)?;
                if state.cursor.is_none() {
                    return Err(Error::state_error(request, "buffer is closed"));
                }
                request.provide::<ByteIo>(&mut *state)?;
            }
            Ok(())
        }

        fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
            builder.method("written", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let written = this.borrow(strand)?.written.clone();
                let written = written.ok_or_else(|| Error::value(strand, "not duplex"))?;
                let bytes = written.lock().unwrap().clone();
                Output::set(strand, out, &bytes[..]);
                Ok(())
            })
        }
    }

    fn bin_arg<'v, 's>(
        strand: &mut Strand<'v, 's>,
        value: &runtime::Value<'v>,
    ) -> runtime::Result<'v, 's, Vec<u8>> {
        match value.as_bin(strand.vm()) {
            Some(bin) => Ok(Vec::from(bin)),
            None => Err(Error::value(strand, "expected bin")),
        }
    }

    /// Gets `value`'s byte handle, raising if it lacks `need`.
    fn byte_handle<'v, 's>(
        strand: &mut Strand<'v, 's>,
        value: &runtime::Value<'v>,
        need: IoCaps,
    ) -> runtime::Result<'v, 's, ByteHandle> {
        io::get(strand, value, |strand, caps| {
            if !caps.contains(need) {
                return Err(Error::value(strand, "missing capability"));
            }
            Ok(())
        })
    }

    /// Test bitset for exercising `dolang_runtime::object::flags`.
    #[derive(Clone, Copy, PartialEq, Eq, Hash)]
    struct TestFlags(u8);

    impl BitOr for TestFlags {
        type Output = Self;
        fn bitor(self, rhs: Self) -> Self {
            TestFlags(self.0 | rhs.0)
        }
    }

    impl BitAnd for TestFlags {
        type Output = Self;
        fn bitand(self, rhs: Self) -> Self {
            TestFlags(self.0 & rhs.0)
        }
    }

    impl BitXor for TestFlags {
        type Output = Self;
        fn bitxor(self, rhs: Self) -> Self {
            TestFlags(self.0 ^ rhs.0)
        }
    }

    impl Not for TestFlags {
        type Output = Self;
        fn not(self) -> Self {
            TestFlags(!self.0)
        }
    }

    impl FlagLike for TestFlags {
        const ZERO: Self = TestFlags(0);
        const MODULE: &'static str = "regression2";
        const NAME: &'static str = "TestFlags";
        const BITS: &'static [(&'static str, Self)] = &[
            ("NONE", TestFlags(0)),
            ("READ_WRITE", TestFlags(3)),
            ("READ", TestFlags(1)),
            ("WRITE", TestFlags(2)),
            ("EXEC", TestFlags(4)),
        ];

        fn rank(self) -> usize {
            self.0.count_ones() as usize
        }

        fn build<'v, 'a>(
            builder: TypeBuilder<'v, 'a, Flags<Self>>,
        ) -> TypeBuilder<'v, 'a, Flags<Self>> {
            builder
                .get("int", |this, strand, out| {
                    Output::set(strand, out, this.flags().0);
                    Ok(())
                })
                .method("is_exec", async move |this, strand, args, out| {
                    let ([], []) = unpack!(strand, args, 0, 0)?;
                    Output::set(strand, out, this.flags().0 & 4 != 0);
                    Ok(())
                })
                .type_method("from_int", async move |this, strand, args, out| {
                    let ([value], []) = unpack!(strand, args, 1, 0)?;
                    let value = value.to_i64(strand)?;
                    let value = u8::try_from(value)
                        .map_err(|_| Error::value(strand, "flags integer out of range"))?;
                    this.create_flags(strand, TestFlags(value), out);
                    Ok(())
                })
                // Hidden by the instance method of the same name
                .type_method("is_exec", async move |_this, strand, _args, out| {
                    Output::set(strand, out, "type method");
                    Ok(())
                })
                .type_get("type_property", |_this, strand, out| {
                    Output::set(strand, out, 42_i64);
                    Ok(())
                })
                .type_set("type_property", |_this, strand, value| {
                    value.to_i64(strand)?;
                    Ok(())
                })
                .type_get("type_readonly", |_this, strand, out| {
                    Output::set(strand, out, 43_i64);
                    Ok(())
                })
                .type_set("type_writeonly", |_this, strand, value| {
                    value.to_i64(strand)?;
                    Ok(())
                })
                .method("hidden_property", async move |_this, strand, args, out| {
                    let ([], []) = unpack!(strand, args, 0, 0)?;
                    Output::set(strand, out, "instance method");
                    Ok(())
                })
                .type_get("hidden_property", |_this, strand, out| {
                    Output::set(strand, out, 44_i64);
                    Ok(())
                })
                .type_set("hidden_property", |_this, strand, value| {
                    value.to_i64(strand)?;
                    Ok(())
                })
        }
    }

    async fn vm_main<'v>(
        strand: &mut Strand<'v, '_>,
        path: &Path,
        test_state: &dolang_private_test::TestState,
        [mut retval]: [Slot<'v, '_>; 1],
    ) {
        let content = dolang_private_test::read_file(path);
        let update_mode = dolang_private_test::update_mode();
        let file = path.file_name().unwrap().to_str().unwrap();

        let (res, diags, mut directives) = compile(path, &content, None);
        let unexpected_diag =
            dolang_private_test::match_diagnostics(path, &content, file, &diags, &mut directives);

        // Check for compile error vs runtime execution
        let compile_failed = res.is_err();
        let mut unexpected_err = false;

        if let Ok(bytecode) = res {
            match bytecode
                .run(strand, &mut retval)
                .await
                .and_then(|_| retval.to_string(strand))
            {
                Ok(res) => eprintln!("Result: {}", res),
                Err(e) => {
                    dolang_private_test::print_error_backtrace(strand, &e);
                    unexpected_err = true;
                }
            }
        }

        // Check all results
        let passed = dolang_private_test::check_results(
            &directives,
            compile_failed,
            unexpected_diag,
            unexpected_err,
            test_state,
        );

        if !passed && !update_mode {
            panic!("test failed")
        }
    }

    pub(super) fn run(path: &Path) {
        const STACK_SIZE: usize = 8 * 1024 * 1024;
        let path = path.to_path_buf();
        std::thread::Builder::new()
            .stack_size(STACK_SIZE)
            .spawn(move || {
                futures::executor::block_on(Builder::build(async |vm| {
                    let msg = vm.sym("msg");
                    let optional = vm.sym("optional");
                    let footy = vm.register_type::<Foo>();
                    let counter_ty = vm.register_type::<Counter>();
                    let byte_buf_ty = vm.register_type::<ByteBuf>();
                    let test_flags_ty = TestFlags::register_type(vm);

                    // Configure standard regression functions from dolang-private-test
                    let test_state = dolang_private_test::configure_vm(vm);

                    vm.importer(async move |strand, name, out| {
                        let path = format!("{}.dol", name);
                        let path = Path::new(&path);
                        let path: PathBuf = [Path::new(MOD_DIR), path].into_iter().collect();
                        if !path.exists() {
                            return Err(Error::import(strand, name));
                        }
                        let content = dolang_private_test::read_file(&path);
                        let (bytecode, _, _) = compile(&path, &content, Some(name));
                        let bytecode = bytecode.unwrap();
                        bytecode.run(strand, out).await
                    })
                    // Add custom functions to regression2 module
                    .module("regression2")
                    .value("MIRI", cfg!(miri))
                    .value("DEBUG", cfg!(debug_assertions))
                    .value("TestFlags", test_flags_ty)
                    .value("Counter", counter_ty)
                    .value("ByteBuf", byte_buf_ty)
                    .function("async", async |strand, args, mut out| {
                        let ([mut value], _) = unpack!(strand, args, 1, 0)?;
                        DummyAsync::new(()).await;
                        Output::swap(&mut out, &mut value);
                        Ok(())
                    })
                    .function("callme", async move |strand, args, out| {
                        let ([func, input], _) = unpack!(strand, args, 1, 0, msg)?;
                        call!(strand, func, out, msg: input).await
                    })
                    .function("keyword_rest", async move |strand, args, out| {
                        let ([func, _required], [_optional], rest) = args
                            .unpack::<1, 1, 0, 1, 2, 1, false, true>(strand, [msg], [optional])?;
                        func.call(strand, rest.unwrap(), out).await
                    })
                    .function("mixed_rest", async move |strand, args, out| {
                        let ([func, _required], [_optional], rest) =
                            unpack!(strand, args, 1, 0, msg, optional = None, ...)?;
                        func.call(strand, rest, out).await
                    })
                    .function("makefoo", async move |strand, args, out| {
                        let _ = unpack!(strand, args, 0, 0)?;
                        footy.create(strand, Foo, out);
                        Ok(())
                    })
                    .function("noop", async move |_strand, _args, _out| Ok(()))
                    .function("bump", async move |strand, args, out| {
                        let ([value], []) = unpack!(strand, args, 1, 0)?;
                        bump(strand, &value, out, false)
                    })
                    .function("bump_fail", async move |strand, args, out| {
                        let ([value], []) = unpack!(strand, args, 1, 0)?;
                        bump(strand, &value, out, true)
                    })
                    .function("byte_read", async move |strand, args, out| {
                        let ([value], []) = unpack!(strand, args, 1, 0)?;
                        let handle = byte_handle(strand, &value, IoCaps::READ)?;
                        let mut io = handle.into_read().unwrap();
                        let mut buf = Vec::new();
                        let res = io.read_to_end(&mut buf).await;
                        res.into_do_io(strand)?;
                        Output::set(strand, out, &buf[..]);
                        Ok(())
                    })
                    .function("byte_tell", async move |strand, args, out| {
                        let ([value], []) = unpack!(strand, args, 1, 0)?;
                        let handle = byte_handle(strand, &value, IoCaps::READ | IoCaps::SEEK)?;
                        let mut io = handle.into_read_seek().unwrap();
                        let res = io.stream_position().await;
                        let pos = res.into_do_io(strand)?;
                        Output::set(strand, out, pos);
                        Ok(())
                    })
                    .function("byte_seek", async move |strand, args, out| {
                        let ([value, offset], []) = unpack!(strand, args, 2, 0)?;
                        let offset = offset
                            .as_int(strand)
                            .and_then(|n| u64::try_from(n).ok())
                            .ok_or_else(|| Error::value(strand, "bad offset"))?;
                        let handle = byte_handle(strand, &value, IoCaps::READ | IoCaps::SEEK)?;
                        let mut io = handle.into_read_seek().unwrap();
                        let mut buf = Vec::new();
                        let res = async {
                            io.seek(SeekFrom::Start(offset)).await?;
                            io.read_to_end(&mut buf).await
                        }
                        .await;
                        res.into_do_io(strand)?;
                        Output::set(strand, out, &buf[..]);
                        Ok(())
                    })
                    .function("byte_overwrite", async move |strand, args, out| {
                        let ([value, bytes], []) = unpack!(strand, args, 2, 0)?;
                        let bytes = bin_arg(strand, &bytes)?;
                        let need = IoCaps::READ | IoCaps::WRITE | IoCaps::SEEK;
                        let handle = byte_handle(strand, &value, need)?;
                        let mut io = handle.into_read_write_seek().unwrap();
                        let mut buf = Vec::new();
                        let res = async {
                            io.write_all(&bytes).await?;
                            io.flush().await?;
                            io.seek(SeekFrom::Start(0)).await?;
                            io.read_to_end(&mut buf).await
                        }
                        .await;
                        res.into_do_io(strand)?;
                        Output::set(strand, out, &buf[..]);
                        Ok(())
                    })
                    .function("byte_write", async move |strand, args, _out| {
                        let ([value, bytes], []) = unpack!(strand, args, 2, 0)?;
                        let bytes = bin_arg(strand, &bytes)?;
                        let handle = byte_handle(strand, &value, IoCaps::WRITE)?;
                        let mut io = handle.into_write().unwrap();
                        let res = async {
                            io.write_all(&bytes).await?;
                            io.close().await
                        }
                        .await;
                        res.into_do_io(strand)
                    })
                    // Reads while writing `bytes`, returning what the read got.
                    .function("byte_duplex", async move |strand, args, out| {
                        let ([value, bytes], []) = unpack!(strand, args, 2, 0)?;
                        let bytes = bin_arg(strand, &bytes)?;
                        let need = IoCaps::READ | IoCaps::WRITE;
                        let handle = byte_handle(strand, &value, need)?;
                        let (mut reader, mut writer) = handle.into_read_write().unwrap().split();
                        let mut buf = vec![0; bytes.len()];
                        let (read, written) = futures::join!(reader.read(&mut buf), async {
                            writer.write_all(&bytes).await?;
                            writer.flush().await
                        });
                        let res = written.and(read);
                        let n = res.into_do_io(strand)?;
                        Output::set(strand, out, &buf[..n]);
                        Ok(())
                    })
                    .function("coerce_flags", async move |strand, args, out| {
                        let ([value], []) = unpack!(strand, args, 1, 0)?;
                        let bits = test_flags_ty.coerce(strand, &value).await?;
                        test_flags_ty.create_flags(strand, bits, out);
                        Ok(())
                    })
                    .commit();

                    vm.enter_with_slots(async move |strand, slots| {
                        vm_main(strand, &path, &test_state, slots).await
                    })
                    .await
                }))
            })
            .expect("failed to spawn regression thread")
            .join()
            .expect("regression thread panicked")
    }
}

use detail::run;

include!(concat!(env!("OUT_DIR"), "/generated_tests.rs"));

#[test]
fn constant_pattern_without_prelude() {
    use dolang::{
        compile::Config,
        runtime::{Bytecode, error::ErrorKind, vm::Builder},
    };
    let mut config = Config::new();
    config.prelude().clear();
    let mut bytes = Vec::new();
    config
        .unit(std::path::Path::new("constant.dol"), "let 200 = 404\n")
        .emit(&mut bytes)
        .unwrap();
    let bytecode = Bytecode::new(bytes).expect("compiler output must verify");
    futures::executor::block_on(Builder::build(async |vm| {
        vm.importer(async |_strand, _name, _out| panic!("unexpected import"));
        vm.enter_with_slots(async |strand, [mut out]| {
            let error = bytecode.run(strand, &mut out).await.unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Type);
        })
        .await;
    }));
}
