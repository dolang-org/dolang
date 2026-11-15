use dolang::runtime::{AllocExt, Object, State, object::TypeBuilder};

use crate::global::Global;

pub(crate) struct LinkTarget(pub(crate) dolang_vfs_winreg::LinkTarget);

impl<'v> Object<'v> for LinkTarget {
    const NAME: &'v str = "LinkTarget";
    const MODULE: &'v str = "winreg";
    type Annex = State<'v, Global<'v>>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("native", |this, strand, out| {
                let native = this.borrow(strand)?.0.native.clone();
                strand.store(out, native.as_str());
                Ok(())
            })
            .get("root", |this, strand, out| {
                let global = *this.annex();
                let borrow = this.borrow(strand)?;
                if let Some(root) = borrow.0.root {
                    let sym = match root {
                        dolang_vfs_winreg::PredefinedRoot::ClassesRoot => global.syms.classes_root,
                        dolang_vfs_winreg::PredefinedRoot::CurrentUser => global.syms.current_user,
                        dolang_vfs_winreg::PredefinedRoot::LocalMachine => {
                            global.syms.local_machine
                        }
                        dolang_vfs_winreg::PredefinedRoot::Users => global.syms.users,
                        dolang_vfs_winreg::PredefinedRoot::CurrentConfig => {
                            global.syms.current_config
                        }
                    };
                    strand.store(out, sym);
                }
                Ok(())
            })
            .get("subpath", |this, strand, out| {
                let subpath = this.borrow(strand)?.0.subpath.clone();
                if let Some(subpath) = &subpath {
                    strand.store(out, subpath.as_str());
                }
                Ok(())
            })
    }
}
