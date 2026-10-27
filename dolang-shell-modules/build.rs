#[cfg(feature = "shell")]
extern crate dolang_ext_shell;

use std::{env, path::Path};

use dolang::compile::Config;
use dolang_private_bundle::{Bundle, CompileMode, NameMode, Root};
use wax::{Glob, Program as _};

/// Sources to bundle when every feature in `when` is enabled, or disabled for a
/// `!` entry. Globs are relative to the crate.
struct Row {
    when: &'static [&'static str],
    globs: &'static [&'static str],
}

/// What each feature set bundles. A row requires the features of every module
/// its sources import, directly or not, since an import of a module this build
/// leaves out fails the build.
const TABLE: &[Row] = &[
    Row {
        when: &[],
        globs: &["stub/{std,io,math,strand}.dol"],
    },
    Row {
        when: &["base64"],
        globs: &["stub/base64.dol"],
    },
    Row {
        when: &["compile", "term"],
        globs: &["stub/compile.dol"],
    },
    Row {
        when: &["compress", "shell", "time", "uuid"],
        globs: &["stub/compress.dol"],
    },
    Row {
        when: &["digest"],
        globs: &["stub/digest.dol"],
    },
    Row {
        when: &["glob"],
        globs: &["stub/glob.dol"],
    },
    Row {
        when: &["http", "json", "time", "url"],
        globs: &["stub/http.dol"],
    },
    Row {
        when: &["http_mock", "time", "url"],
        globs: &["stub/http/mock.dol"],
    },
    Row {
        when: &["json"],
        globs: &["stub/json.dol"],
    },
    Row {
        when: &["load"],
        globs: &["stub/load.dol"],
    },
    Row {
        when: &["patch", "shell", "time", "uuid"],
        globs: &["stub/patch.dol"],
    },
    Row {
        when: &["progress"],
        globs: &["stub/progress.dol"],
    },
    Row {
        when: &["rand"],
        globs: &["stub/rand.dol"],
    },
    Row {
        when: &["regex"],
        globs: &["stub/regex.dol"],
    },
    Row {
        when: &["shell", "time", "uuid"],
        globs: &["stub/{fs,proc,security,shell,shlex,sys}{.dol,/**/*.dol}"],
    },
    Row {
        when: &["shell", "compile", "term", "time", "uuid"],
        globs: &["stub/_shell.dol"],
    },
    Row {
        when: &["sqlite", "shell", "time", "uuid"],
        globs: &["stub/sqlite.dol"],
    },
    Row {
        when: &["tar", "shell", "time", "uuid"],
        globs: &["stub/tar.dol"],
    },
    Row {
        when: &["term"],
        globs: &["stub/term.dol"],
    },
    Row {
        when: &["time"],
        globs: &["stub/time.dol"],
    },
    Row {
        when: &["toml"],
        globs: &["stub/toml.dol"],
    },
    Row {
        when: &["url"],
        globs: &["stub/url.dol"],
    },
    Row {
        when: &["uuid"],
        globs: &["stub/uuid.dol"],
    },
    Row {
        when: &["winnet", "shell", "time", "uuid"],
        globs: &["stub/winnet.dol"],
    },
    Row {
        when: &["winreg", "shell", "time", "uuid"],
        globs: &["stub/winreg.dol"],
    },
    Row {
        when: &["winscm", "shell", "time", "uuid"],
        globs: &["stub/winscm.dol"],
    },
    Row {
        when: &["xml"],
        globs: &["stub/xml.dol"],
    },
    Row {
        when: &["yaml"],
        globs: &["stub/yaml.dol"],
    },
    Row {
        when: &["zip", "shell", "time", "uuid"],
        globs: &["stub/zip.dol"],
    },
    Row {
        when: &["shell", "time", "uuid"],
        globs: &[
            "lib/{args,xdg}.dol",
            "lib/_container/common.dol",
            "lib/_vm/{common,provision}.dol",
        ],
    },
    Row {
        when: &["shell", "term", "time", "uuid"],
        globs: &["lib/_display.dol"],
    },
    Row {
        when: &["shell", "term", "time", "url", "uuid"],
        globs: &["lib/systemd.dol"],
    },
    Row {
        when: &["shell", "compile", "term", "time", "uuid"],
        globs: &["lib/_compile.dol"],
    },
    Row {
        when: &["shell", "compile", "digest", "load", "term", "time", "uuid"],
        globs: &["lib/{_script,dodo,exec}.dol"],
    },
    Row {
        when: &[
            "shell", "compile", "load", "progress", "term", "time", "uuid",
        ],
        globs: &["lib/test.dol"],
    },
    Row {
        when: &["shell", "base64", "rand", "regex", "time", "uuid"],
        globs: &["lib/{_ssh,_vfs,_wsl,admin,ssh,sudo,toolbx,wsl}.dol"],
    },
    Row {
        when: &["shell", "digest", "time", "uuid", "yaml"],
        globs: &["lib/_vm/cloud_init.dol"],
    },
    Row {
        when: &["shell", "time", "uuid", "xml"],
        globs: &["lib/_vm/unattend.dol"],
    },
    Row {
        when: &[
            "shell", "base64", "compress", "digest", "http", "json", "progress", "rand", "regex",
            "tar", "time", "url", "uuid", "zip",
        ],
        globs: &["lib/transfer.dol"],
    },
    Row {
        when: &[
            "shell", "base64", "compress", "digest", "http", "json", "patch", "progress", "rand",
            "regex", "tar", "time", "url", "uuid", "zip",
        ],
        globs: &["lib/{docker,podman}.dol", "lib/_container/dockman.dol"],
    },
    Row {
        when: &[
            "shell", "base64", "compile", "compress", "digest", "http", "json", "progress", "rand",
            "regex", "tar", "term", "time", "url", "uuid", "xml", "yaml", "zip",
        ],
        globs: &["lib/libvirt.dol"],
    },
    Row {
        when: &["wasm"],
        globs: &["wasm/stub/test.dol"],
    },
];

fn enabled(feature: &str) -> bool {
    let var = format!("CARGO_FEATURE_{}", feature.to_uppercase());
    env::var_os(var).is_some()
}

/// Whether the table bundles a source, by its path relative to the crate.
fn selected(path: &Path) -> bool {
    TABLE
        .iter()
        .filter(|row| {
            row.when
                .iter()
                .all(|feature| match feature.strip_prefix('!') {
                    Some(feature) => !enabled(feature),
                    None => enabled(feature),
                })
        })
        .flat_map(|row| row.globs)
        .any(|glob| Glob::new(glob).unwrap().is_match(path))
}

/// The library modules run under the shell's prelude.
#[cfg(feature = "shell")]
fn configure(config: &mut Config<'_>) {
    use dolang::extension::CompilerExt;

    for ext in config.extensions() {
        ext.apply(config).unwrap();
    }
}

/// No library module is selected without the shell.
#[cfg(not(feature = "shell"))]
fn configure(_config: &mut Config<'_>) {}

fn main() {
    let manifest_dir = Path::new(&env::var_os("CARGO_MANIFEST_DIR").unwrap()).to_owned();
    let out_dir = env::var_os("OUT_DIR").unwrap();
    let under = |root: &'static str| move |path: &Path| selected(&Path::new(root).join(path));
    let (lib, stub, wasm) = (under("lib"), under("stub"), under("wasm/stub"));

    // `libvirt`'s guest script goes with it.
    println!("cargo::rustc-check-cfg=cfg(provision)");
    if selected(Path::new("lib/libvirt.dol")) {
        println!("cargo::rustc-cfg=provision");
    }

    dolang_private_bundle::bundle(&Bundle {
        roots: &[
            Root {
                dir: &manifest_dir.join("lib"),
                virtual_root: "<bundled>",
                configure,
                bytecode: true,
                select: &lib,
            },
            Root {
                dir: &manifest_dir.join("stub"),
                virtual_root: "dolang-shell-modules/stub",
                configure: |_| {},
                bytecode: false,
                select: &stub,
            },
            Root {
                dir: &manifest_dir.join("wasm/stub"),
                virtual_root: "dolang-shell-modules/wasm/stub",
                configure: |_| {},
                bytecode: false,
                select: &wasm,
            },
        ],
        out_dir: Path::new(&out_dir),
        compile_mode: CompileMode::Module,
        name_mode: NameMode::Module,
        bytecode: Some("bundled_modules"),
        typelibs: Some("typelibs"),
    });
}
