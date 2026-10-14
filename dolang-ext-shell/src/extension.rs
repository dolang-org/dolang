use std::{
    error,
    fmt::{self, Debug, Display, Formatter},
};

use dolang::{
    compile::Config,
    extension,
    extension::{Extension, Version},
    runtime::{AllocExt, vm::Builder},
};

use crate::{
    console, fs,
    global::{
        ErrorGlobal, ErrorTag, FsGlobal, FsTag, Global, MacosSecurityGlobal, MacosSecurityTag,
        Nfs4SecurityGlobal, Nfs4SecurityTag, PipeGlobal, PipeTag, ProcGlobal, ProcTag, SecurityTag,
        ShellGlobal, ShellTag, ShlexTag, SysGlobal, SysTag, UnixSecurityGlobal, UnixSecurityTag,
        WindowsSecurityGlobal, WindowsSecurityTag,
    },
    pipe_channel, platform, proc, security, shell, shlex, sys,
};

/// Shell extension
pub struct Shell;

#[derive(Debug)]
pub enum Infallible {}

impl Display for Infallible {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Debug::fmt(self, f)
    }
}

impl error::Error for Infallible {}

impl Extension for Shell {
    type Error = Infallible;
    const NAME: &str = "shell";
    const VERSION: Version = dolang::package_version!();
    const DESCRIPTION: &str = "Do Shell Extension";
    const DEPENDS: &'static [&'static str] = &[
        <dolang_ext_time::TimeExt as Extension>::NAME,
        <dolang_ext_term::TermExt as Extension>::NAME,
    ];

    fn apply_compiler(&self, config: &mut Config) -> Result<(), Infallible> {
        shell::configure_compiler(config);
        security::configure_compiler(config);
        sys::configure_compiler(config);
        proc::configure_compiler(config);
        shlex::configure_compiler(config);
        Ok(())
    }

    fn apply_vm<'v>(&self, builder: &mut Builder<'v>) -> Result<(), Infallible> {
        let global = Global::new(builder);
        let global = builder.register_state(global);
        let local = global.local;
        pipe_channel::install(builder);
        console::install(builder, global);
        builder.lazy::<ErrorTag>(
            &[
                "sys.unix",
                "sys.freebsd",
                "sys.linux",
                "sys.macos",
                "sys.windows",
            ],
            move |reg| {
                let state = ErrorGlobal::new(reg, local);
                let state = reg.register_state(state);
                platform::configure_vm(reg, state);
            },
        );
        builder.lazy::<PipeTag>(&[], move |reg| {
            let state = PipeGlobal::new(reg, local);
            reg.register_state(state);
        });
        builder.lazy::<FsTag>(&["fs", "fs.unix", "fs.windows"], move |reg| {
            let state = FsGlobal::new(reg, local);
            let state = reg.register_state(state);
            fs::configure_vm(reg, state);
        });
        builder.lazy::<ShellTag>(&["shell"], move |reg| {
            let state = ShellGlobal::new(reg, local);
            let state = reg.register_state(state);
            shell::configure_vm(reg, global, state);
        });
        builder.lazy::<ProcTag>(&["proc", "proc.windows"], move |reg| {
            let errors = reg.force_state::<ErrorGlobal<'v>>();
            let pipes = reg.force_state::<PipeGlobal<'v>>();
            let state = ProcGlobal::new(reg, local);
            let state = reg.register_state(state);
            proc::configure_vm(reg, state, errors, pipes);
        });
        builder.lazy::<SysTag>(&["sys"], move |reg| {
            let errors = reg.force_state::<ErrorGlobal<'v>>();
            let state = SysGlobal::new(reg, local);
            let state = reg.register_state(state);
            sys::configure_vm(reg, state, errors);
        });
        builder.lazy::<SecurityTag>(&["security"], move |reg| {
            security::configure_vm(reg, local);
        });
        builder.lazy::<UnixSecurityTag>(&["security.unix"], move |reg| {
            let state = UnixSecurityGlobal::new(reg, local);
            let state = reg.register_state(state);
            security::unix::configure_vm(reg, state);
        });
        builder.lazy::<Nfs4SecurityTag>(&["security.nfs4"], |reg| {
            let state = Nfs4SecurityGlobal::new(reg);
            let state = reg.register_state(state);
            security::nfs4::configure_vm(reg, state);
        });
        builder.lazy::<MacosSecurityTag>(&["security.macos"], move |reg| {
            let state = MacosSecurityGlobal::new(reg, local);
            let state = reg.register_state(state);
            security::macos::configure_vm(reg, state);
        });
        builder.lazy::<WindowsSecurityTag>(&["security.windows"], move |reg| {
            let state = WindowsSecurityGlobal::new(reg, local);
            let state = reg.register_state(state);
            security::windows::configure_vm(reg, state);
        });
        builder.lazy::<ShlexTag>(&["shlex"], shlex::configure_vm);
        Ok(())
    }
}

extension!(Shell);
