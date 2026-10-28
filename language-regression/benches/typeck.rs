#![deny(warnings)]

use std::{cell::OnceCell, hint::black_box, path::Path};

use criterion::{Criterion, criterion_group, criterion_main};
use dolang::compile::{Config, ErrorKind, Severity, typeck};

#[path = "../tests/typeck.rs"]
#[allow(dead_code, reason = "the harness also holds the regression tests")]
mod harness;

use harness::{Case, validate};

include!(concat!(env!("OUT_DIR"), "/generated_typeck_benches.rs"));

#[cfg(not(asan))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn benchmarks(c: &mut Criterion) {
    assert!(
        std::env::var_os("DOLANG_TEST_UPDATE").is_none(),
        "benchmarks must not rewrite expectations; unset DOLANG_TEST_UPDATE"
    );
    for &(path, modes) in BENCH_CASES {
        let case = Case::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join(path));
        let name = path.strip_prefix("tests/typeck/").unwrap();
        let validated = OnceCell::new();
        for &mode in modes {
            if mode == "compile" {
                for source in case.sources.iter().filter(|source| source.case) {
                    c.bench_function(&format!("{name}/compile/{}", source.file), |b| {
                        validated.get_or_init(|| validate(&case));
                        let config = case.config(source, true);
                        b.iter(|| black_box(config.unit(&source.path, black_box(source.text()))));
                    });
                }
                continue;
            }
            let operations: &[&str] = match mode {
                "source" => &["source"],
                "typelib" => &["typelib", "typelib-base"],
                _ => unreachable!("the generator validates modes"),
            };
            for &operation in operations {
                c.bench_function(&format!("{name}/{operation}"), |b| {
                    validated.get_or_init(|| validate(&case));
                    let units = case.compile();
                    if operation == "source" {
                        b.iter(|| {
                            let mut builder = case.builder();
                            for unit in black_box(&units) {
                                match builder.unit(unit) {
                                    Ok(_) => {}
                                    Err(e) if matches!(e.kind(), ErrorKind::Fail) => {}
                                    Err(e) => panic!("{e}"),
                                }
                            }
                            black_box(builder.check())
                        });
                    } else {
                        assert!(
                            case.sources.iter().any(|s| s.module.is_none()),
                            "{path}: typelib benchmarks require main.dol"
                        );
                        // Written by a check of every unit from source
                        let mut builder = case.builder();
                        builder.typelibs(true);
                        let ids: Vec<_> = units
                            .iter()
                            .map(|unit| match builder.unit(unit) {
                                Ok(id) => Some(id),
                                Err(e) if matches!(e.kind(), ErrorKind::Fail) => None,
                                Err(e) => panic!("{e}"),
                            })
                            .collect();
                        let check = builder.check();
                        let bytes: Vec<_> = case
                            .sources
                            .iter()
                            .zip(&ids)
                            .map(|(source, id)| {
                                source.module.as_ref()?;
                                let mut bytes = Vec::new();
                                check.typelib((*id)?, &mut bytes).unwrap();
                                Some(bytes)
                            })
                            .collect();
                        let libraries: Vec<_> = bytes
                            .iter()
                            .map(|bytes| {
                                bytes
                                    .as_ref()
                                    .map(|bytes| typeck::Typelib::read(bytes).unwrap())
                            })
                            .collect();
                        b.iter(|| {
                            let mut builder = case.builder();
                            for ((source, unit), library) in
                                case.sources.iter().zip(&units).zip(black_box(&libraries))
                            {
                                if let Some(library) = library {
                                    builder.typelib(library).unwrap();
                                } else if source.module.is_none() && operation == "typelib" {
                                    match builder.unit(unit) {
                                        Ok(_) => {}
                                        Err(e) if matches!(e.kind(), ErrorKind::Fail) => {}
                                        Err(e) => panic!("{e}"),
                                    }
                                }
                            }
                            black_box(builder.check())
                        });
                    }
                });
            }
        }
    }
    c.bench_function("repository/dodo/compile", |b| {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../dodo.dol");
        let source = std::fs::read_to_string(&path).unwrap();
        let mut config = Config::new();
        config.recover(true).document(true).typecheck(true);
        // Match the LSP's default settings for shell scripts.
        config
            .prelude()
            .import_module("shell")
            .import_items("proc")
            .items(["run", "sub"])
            .commit()
            .import_items("shell")
            .items(["cd", "env", "exit"])
            .commit()
            .import_items("term")
            .items(["echo", "print"])
            .commit();
        let unit = config.unit(&path, source.as_str());
        let errors: Vec<_> = unit
            .diagnostics()
            .filter(|d| d.severity() == Severity::Error)
            .map(|d| dolang_private_test::render_diag_display("dodo.dol", &source, &d))
            .collect();
        assert!(errors.is_empty(), "{}", errors.join("\n"));
        b.iter(|| black_box(config.unit(&path, black_box(source.as_str()))));
    });
}

criterion_group!(benches, benchmarks);
criterion_main!(benches);
