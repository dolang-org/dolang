use dolang::runtime::{
    Instance, Object, Result, Strand, object::TypeBuilder, strand::InterruptMask, unpack,
};

use crate::error::ResultExt as _;

pub(crate) struct FileLock {
    lock: Option<dolang_vfs::file::FileLock>,
}

impl FileLock {
    pub(crate) fn create<'v>(
        strand: &mut Strand<'v, '_>,
        ty: dolang::runtime::Type<'v, Self>,
        lock: Option<dolang_vfs::file::FileLock>,
        out: impl dolang::runtime::Output<'v>,
    ) {
        ty.create(strand, Self { lock }, out);
    }

    pub(crate) async fn release<'v, 's>(
        this: Instance<'v, '_, Self>,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let Some(mut lock) = this.borrow_mut(strand)?.lock.take() else {
            return Ok(());
        };
        let result = lock.release().await.into_sys(strand);
        if result.is_err() {
            this.borrow_mut(strand)?.lock = Some(lock);
        }
        result
    }
}

impl<'v> Object<'v> for FileLock {
    const NAME: &'v str = "FileLock";
    const MODULE: &'v str = "fs";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.method("release", async move |this, strand, args, _out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            strand
                .with_interrupt_mask(InterruptMask::all(), async move |strand| {
                    FileLock::release(this, strand).await
                })
                .await
        })
    }
}
