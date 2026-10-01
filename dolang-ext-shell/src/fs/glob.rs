use std::collections::VecDeque;

use dolang::runtime::{
    Instance, Object, Output, Result, Slot, State, Strand,
    object::{ArrayLike, ArrayView, TypeBuilder, Unpack},
    value::TypeObject,
};

use dolang_vfs::path as vfs_path;

use crate::{
    fs::path::{PathAnnex, create_path_annex},
    global::FsGlobal,
};

/// Iterator over glob results, yielding Path objects.
pub(crate) struct GlobIter {
    pub(crate) paths: VecDeque<vfs_path::PathBuf>,
}

pub(crate) struct GlobIterAnnex<'v> {
    pub(crate) global: State<'v, FsGlobal<'v>>,
    /// Prefix to prepend to each result path.
    pub(crate) prefix: vfs_path::PathBuf,
}

impl<'v> Object<'v> for GlobIter {
    const NAME: &'v str = "GlobIter";
    const MODULE: &'v str = "fs";
    type Annex = GlobIterAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.supertype(TypeObject::Iter)
    }

    /// Returns this iterator.
    async fn iter<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
        Ok(())
    }

    async fn next<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let mut borrow = this.borrow_mut(strand)?;
        let annex = this.annex();
        let global = annex.global;

        match borrow.paths.pop_front() {
            Some(path) => {
                // Prepend prefix if present (used by Path::glob)
                // Create a new Path object for this result
                let annex = PathAnnex::try_new(strand, annex.prefix.join(path.as_str()), global)?;
                create_path_annex(strand, annex, out);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn unpack<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let len = this.borrow(strand)?.paths.len();
        ArrayView::unpack(this, GlobPaths { len }, strand, unpack)
    }
}

struct GlobPaths {
    len: usize,
}

impl<'v> ArrayLike<'v> for GlobPaths {
    type Object = GlobIter;
    const MODULE: &'v str = "fs";
    const NAME: &'v str = "GlobRest";

    fn len(&self, _this: Instance<'v, '_, GlobIter>, _strand: &mut Strand<'v, '_>) -> usize {
        self.len
    }

    fn get<'a, 's>(
        &self,
        this: Instance<'v, '_, GlobIter>,
        strand: &'a mut Strand<'v, 's>,
        index: usize,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        if borrow.paths.len() != self.len {
            return Err(dolang::runtime::Error::concurrency_msg(
                strand,
                "glob rest invalidated by iterator advancement",
            ));
        }
        let path = borrow
            .paths
            .get(index)
            .ok_or_else(|| dolang::runtime::Error::index(strand))?;
        let annex = this.annex();
        let annex = PathAnnex::try_new(strand, annex.prefix.join(path.as_str()), annex.global)?;
        create_path_annex(strand, annex, out);
        Ok(())
    }
}
