use std::{collections::VecDeque, path::PathBuf};

use dolang::runtime::{
    Error, Instance, Object, Output, Result, Slot, State, Strand,
    object::{TypeBuilder, Unpack, UnpackItem},
    value::TypeObject,
};

use crate::{
    fs::path::{Path, PathAnnex},
    global::Global,
};

/// Iterator over glob results, yielding Path objects.
pub(crate) struct GlobIter {
    pub(crate) paths: VecDeque<PathBuf>,
}

pub(crate) struct GlobIterAnnex<'v> {
    pub(crate) global: State<'v, Global<'v>>,
    /// Prefix to prepend to each result path.
    pub(crate) prefix: PathBuf,
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

    /// GlobIter is both an input and output iterator
    async fn input<'a, 's>(
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
                global.types.path.create_with_annex(
                    strand,
                    Path,
                    PathAnnex::new(annex.prefix.join(&path), global),
                    out,
                );
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn unpack<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        // Reject unpacks with required keys
        if let Some(key) = unpack.first_required_key() {
            return Err(Error::missing_key(strand, key));
        }

        let required_pos = unpack.required();
        let optional_pos = unpack.optional();
        let total_pos = required_pos + optional_pos;

        let available = this.borrow(strand)?.paths.len();

        if available < required_pos {
            return Err(Error::missing_positional(strand, available));
        }

        if unpack.exhaustive() && available > total_pos {
            return Err(Error::unexpected_positional(strand, total_pos));
        }

        let mut pos_index: usize = 0;
        for item in unpack.iter() {
            match item {
                UnpackItem::Pos { mut slot, default } => {
                    if pos_index < available {
                        if !Self::next(this, strand, Slot::reborrow(&mut slot)).await? {
                            unreachable!("checked availability above")
                        }
                    } else {
                        Output::set(strand, slot, default.unwrap());
                    }
                    pos_index += 1;
                }
                UnpackItem::SymKey { slot, default, .. }
                | UnpackItem::ConstKey { slot, default, .. } => {
                    // All keyed items must have defaults (checked above)
                    Output::set(strand, slot, default.unwrap());
                }
                UnpackItem::Rest { slot } => Output::set(strand, slot, this),
            }
        }
        Ok(())
    }
}
