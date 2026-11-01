#[cfg(not(feature = "wasm"))]
extern crate dolang_ext_shell;

use std::{env, path::Path};

use dolang::compile::Config;
use dolang_private_bundle::{Bundle, CompileMode, NameMode, Root, Typelibs};
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
    // Every consumer links these.
    Row {
        when: &[],
        globs: &["stub/{std,io,math,strand,compile,digest,load,term,time,uuid}.dol"],
    },
    // Only the native shell links these.
    Row {
        when: &["!wasm"],
        globs: &[
            "stub/{fs,proc,security,shell,shlex,sys}{.dol,/**/*.dol}",
            "stub/_shell.dol",
            "lib/{_compile,_display,_script,args,dodo,exec,xdg}.dol",
            "lib/_container/common.dol",
            "lib/_vm/{common,provision}.dol",
        ],
    },
    Row {
        when: &["base64"],
        globs: &["stub/base64.dol"],
    },
    Row {
        when: &["!wasm", "compress"],
        globs: &["stub/compress.dol"],
    },
    Row {
        when: &["glob"],
        globs: &["stub/glob.dol"],
    },
    Row {
        when: &["http", "json", "url"],
        globs: &["stub/http.dol"],
    },
    Row {
        when: &["http_mock", "url"],
        globs: &["stub/http/mock.dol"],
    },
    Row {
        when: &["json"],
        globs: &["stub/json.dol"],
    },
    Row {
        when: &["!wasm", "patch"],
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
        when: &["!wasm", "sqlite"],
        globs: &["stub/sqlite.dol"],
    },
    Row {
        when: &["!wasm", "tar"],
        globs: &["stub/tar.dol"],
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
        when: &["!wasm", "winnet"],
        globs: &["stub/winnet.dol"],
    },
    Row {
        when: &["!wasm", "winreg"],
        globs: &["stub/winreg.dol"],
    },
    Row {
        when: &["!wasm", "winscm"],
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
        when: &["!wasm", "zip"],
        globs: &["stub/zip.dol"],
    },
    Row {
        when: &["!wasm", "url"],
        globs: &["lib/systemd.dol"],
    },
    Row {
        when: &["!wasm", "progress"],
        globs: &["lib/test.dol"],
    },
    Row {
        when: &["!wasm", "base64", "rand", "regex"],
        globs: &["lib/{_ssh,_vfs,_wsl,admin,ssh,sudo,toolbx,wsl}.dol"],
    },
    Row {
        when: &["!wasm", "yaml"],
        globs: &["lib/_vm/cloud_init.dol"],
    },
    Row {
        when: &["!wasm", "xml"],
        globs: &["lib/_vm/unattend.dol"],
    },
    Row {
        when: &[
            "!wasm", "base64", "compress", "http", "json", "progress", "rand", "regex", "tar",
            "url", "zip",
        ],
        globs: &["lib/transfer.dol"],
    },
    Row {
        when: &[
            "!wasm", "base64", "compress", "http", "json", "patch", "progress", "rand", "regex",
            "tar", "url", "zip",
        ],
        globs: &["lib/{docker,podman}.dol", "lib/_container/dockman.dol"],
    },
    Row {
        when: &[
            "!wasm", "base64", "compress", "http", "json", "progress", "rand", "regex", "tar",
            "url", "xml", "yaml", "zip",
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
#[cfg(not(feature = "wasm"))]
fn configure(config: &mut Config<'_>) {
    use dolang::extension::CompilerExt;

    for ext in config.extensions() {
        ext.apply(config).unwrap();
    }
}

/// No library module is selected for the playground.
#[cfg(feature = "wasm")]
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

    // The shell's pipelines connect stages with `proc`'s pipes, as the checks that
    // read the typelibs have them. Without `proc`, as in wasm, a pipe is a channel.
    let pipes = match selected(Path::new("stub/proc.dol")) {
        true => [("proc", "PipeSender"), ("proc", "PipeReceiver")],
        false => [("strand", "Sender"), ("strand", "Receiver")],
    };

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
        typelibs: Some(Typelibs {
            name: "typelibs",
            pipes,
        }),
    });
}
