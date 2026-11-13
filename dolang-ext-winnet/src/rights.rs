//! Account rights, shared by the `User` and `Group` types.

use dolang::runtime::AllocExt;
use dolang::runtime::{
    Args, Error, Instance, Object, Output, Result, Slot, Strand, object::TypeBuilder, unpack,
    value::Nil,
};
use dolang_ext_shell::ResultExt;
use dolang_winterop::security::Sid;

/// A principal whose rights can be managed: a user or group capability.
pub(crate) trait Principal {
    /// The principal's name, as it appears in error messages.
    const NAME: &'static str;

    /// The principal's SID, or `None` once it has been deleted.
    fn sid(&self) -> Option<&Sid>;
}

/// Reads the SID of a receiver that has not been deleted.
fn sid<'v, 's, T: Object<'v> + Principal>(
    this: &Instance<'v, '_, T>,
    strand: &mut Strand<'v, 's>,
) -> Result<'v, 's, Sid> {
    this.borrow(strand)?.sid().cloned().ok_or_else(|| {
        Error::state_error(strand, format!("{} was deleted", <T as Principal>::NAME))
    })
}

/// Coerces the single right-name argument.
fn right_arg<'v, 's>(strand: &mut Strand<'v, 's>, args: Args<'v, '_>) -> Result<'v, 's, String> {
    let ([right], []) = unpack!(strand, args, 1, 0)?;
    Ok(right
        .as_str(strand)
        .ok_or_else(|| Error::type_error(strand, "right must be a Str"))?
        .to_string())
}

/// Builds an array of right names.
fn make_rights<'v, 's>(
    strand: &mut Strand<'v, 's>,
    rights: Vec<String>,
    mut out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    strand.with_slots_sync(|strand, [mut item]| {
        strand.new_array(&mut out);
        let array = out.as_array(strand).unwrap();
        for right in rights {
            Output::set(strand, &mut item, right.as_str());
            array.push(strand, &item)?;
        }
        Ok(())
    })
}

/// Registers the account-rights methods on a principal type.
///
/// Rights are held in the local security policy against a SID, so users and
/// groups carry the same three methods.
pub(crate) fn build<'v, 'a, T: Object<'v> + Principal>(
    builder: TypeBuilder<'v, 'a, T>,
) -> TypeBuilder<'v, 'a, T> {
    builder
        .method("rights", async move |this, strand, args, out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            let sid = sid(&this, strand)?;
            let vfs = dolang_ext_shell::vfs(strand);
            let rights = dolang_vfs_winnet::rights::list(&vfs, &sid)
                .await
                .into_sys(strand)?;
            make_rights(strand, rights, out)
        })
        .method("grant_right", async move |this, strand, args, out| {
            let right = right_arg(strand, args)?;
            let sid = sid(&this, strand)?;
            let vfs = dolang_ext_shell::vfs(strand);
            dolang_vfs_winnet::rights::grant(&vfs, &sid, right)
                .await
                .into_sys(strand)?;
            Output::set(strand, out, Nil);
            Ok(())
        })
        .method("revoke_right", async move |this, strand, args, out| {
            let right = right_arg(strand, args)?;
            let sid = sid(&this, strand)?;
            let vfs = dolang_ext_shell::vfs(strand);
            dolang_vfs_winnet::rights::revoke(&vfs, &sid, right)
                .await
                .into_sys(strand)?;
            Output::set(strand, out, Nil);
            Ok(())
        })
}
