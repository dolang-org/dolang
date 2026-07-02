#![deny(warnings)]

mod detail {
    use dolang::{
        compile::{self, Compiler, Mode},
        runtime::{
            self, Args, Bytecode, Error, Instance, Object, Output, Slot, Sym, call, unpack,
            vm::Builder,
        },
    };
    use dolang_runtime::{error::ResultExt, strand::Strand};
    use std::{
        fmt, io,
        ops::ControlFlow,
        path::{Path, PathBuf},
        pin::Pin,
        task::{Context as TaskContext, Poll},
    };

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
        Result<Bytecode, compile::Error<io::Error>>,
        Vec<dolang::compile::Diag>,
        Vec<dolang_private_test::Directive>,
    ) {
        let mut compiler = Compiler::new(path, content);
        let directives = dolang_private_test::configure_compiler(&mut compiler, content);
        // Add extra prelude items specific to dolang tests (from regression2 module)
        compiler
            .prelude()
            .import_items("regression2")
            .items(["async", "callme", "makefoo", "noop", "MIRI", "DEBUG"])
            .commit();
        if let Some(name) = module {
            compiler.mode(Mode::Module { name });
        }
        let mut out = Vec::new();
        let mut diags = Vec::new();
        let res = compiler.compile(&mut out, &mut |diag| {
            diags.push(diag);
            ControlFlow::Continue(())
        });
        (res.map(|_| Bytecode::new(out)), diags, directives)
    }

    #[derive(Clone, Debug)]
    struct Foo;

    impl<'v> Object<'v> for Foo {
        const NAME: &'v str = "Foo";
        const MODULE: &'v str = "regression2";

        type Annex = ();
        type Type = ();
        type TypeAnnex = ();

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
            w: &mut dyn fmt::Write,
        ) -> runtime::Result<'v, 's, ()> {
            Self::debug(this, strand, w)
        }

        fn debug<'a, 's>(
            _this: Instance<'v, 'a, Self>,
            strand: &'a mut Strand<'v, 's>,
            w: &mut dyn fmt::Write,
        ) -> runtime::Result<'v, 's, ()> {
            write!(w, "foo").into_do(strand)
        }
    }

    async fn vm_main<'v>(
        strand: &mut Strand<'v, '_>,
        path: &Path,
        test_state: &dolang_private_test::TestState,
        [mut retval]: [Slot<'v, '_>; 1],
    ) {
        let content = dolang_private_test::read_file(path);
        let update_mode = std::env::var("DOLANG_TEST_UPDATE").is_ok();
        let file = path.file_name().unwrap().to_str().unwrap();
        let source = std::str::from_utf8(&content).unwrap();

        let (res, diags, mut directives) = compile(path, &content, None);

        // Process diagnostics
        let mut unexpected_diag = false;
        let mut pending_updates: Vec<(u32, String)> = Vec::new();
        for d in diags {
            if let Some(rendered) =
                dolang_private_test::match_diagnostic(&mut directives, &d, file, source)
            {
                if update_mode {
                    pending_updates.push((d.span().end().line_number(), rendered));
                } else {
                    let display = dolang_private_test::render_diag_display(file, source, &d);
                    eprintln!("unexpected diagnostic:\n{display}");
                    unexpected_diag = true;
                }
            }
        }
        if !pending_updates.is_empty() {
            dolang_private_test::apply_diagnostic_updates(path, &content, pending_updates);
        }

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
                    let footy = vm.register_type::<Foo>();

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
                    .function("makefoo", async move |strand, args, out| {
                        let _ = unpack!(strand, args, 0, 0)?;
                        footy.create(strand, Foo, out);
                        Ok(())
                    })
                    .function("noop", async move |_strand, _args, _out| Ok(()))
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
