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
        ShellGlobal, ShellTag, ShlexTag, SysGlobal, SysTag, Tag, UnixSecurityGlobal,
        UnixSecurityTag, WindowsSecurityGlobal, WindowsSecurityTag,
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
        let global = builder.register_state::<Tag>(global);
        let local = global.local;
        pipe_channel::install(builder);
        crate::error::install(builder);
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
                let state = reg.register_state::<ErrorTag>(state);
                platform::configure_vm(reg, state);
            },
        );
        builder.lazy::<PipeTag>(&[], move |reg| {
            let state = PipeGlobal::new(reg, local);
            reg.register_state::<PipeTag>(state);
        });
        builder.lazy::<FsTag>(&["fs", "fs.unix", "fs.windows"], move |reg| {
            let state = FsGlobal::new(reg, local);
            let state = reg.register_state::<FsTag>(state);
            fs::configure_vm(reg, state);
        });
        builder.lazy::<ShellTag>(&["shell"], move |reg| {
            let state = ShellGlobal::new(reg, local);
            let state = reg.register_state::<ShellTag>(state);
            shell::configure_vm(reg, global, state);
        });
        builder.lazy::<ProcTag>(&["proc", "proc.windows"], move |reg| {
            let errors = reg.force_state::<ErrorTag>();
            let pipes = reg.force_state::<PipeTag>();
            let state = ProcGlobal::new(reg, local);
            let state = reg.register_state::<ProcTag>(state);
            proc::configure_vm(reg, state, errors, pipes);
        });
        builder.lazy::<SysTag>(&["sys"], move |reg| {
            let errors = reg.force_state::<ErrorTag>();
            let state = SysGlobal::new(reg, local);
            let state = reg.register_state::<SysTag>(state);
            sys::configure_vm(reg, state, errors);
        });
        builder.lazy::<SecurityTag>(&["security"], move |reg| {
            reg.register_state::<SecurityTag>(());
            security::configure_vm(reg, local);
        });
        builder.lazy::<UnixSecurityTag>(&["security.unix"], move |reg| {
            let state = UnixSecurityGlobal::new(reg, local);
            let state = reg.register_state::<UnixSecurityTag>(state);
            security::unix::configure_vm(reg, state);
        });
        builder.lazy::<Nfs4SecurityTag>(&["security.nfs4"], |reg| {
            let state = Nfs4SecurityGlobal::new(reg);
            let state = reg.register_state::<Nfs4SecurityTag>(state);
            security::nfs4::configure_vm(reg, state);
        });
        builder.lazy::<MacosSecurityTag>(&["security.macos"], move |reg| {
            let state = MacosSecurityGlobal::new(reg, local);
            let state = reg.register_state::<MacosSecurityTag>(state);
            security::macos::configure_vm(reg, state);
        });
        builder.lazy::<WindowsSecurityTag>(&["security.windows"], move |reg| {
            let state = WindowsSecurityGlobal::new(reg, local);
            let state = reg.register_state::<WindowsSecurityTag>(state);
            security::windows::configure_vm(reg, state);
        });
        builder.lazy::<ShlexTag>(&["shlex"], |reg| {
            reg.register_state::<ShlexTag>(());
            shlex::configure_vm(reg);
        });
        Ok(())
    }
}

extension!(Shell);
