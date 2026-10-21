use dolang::runtime::value::fmt::Format;
use dolang::runtime::{
    Args, Instance, Object, Output, Result, Slot, Strand, Type, Value,
    object::{TypeBuilder, fmt},
    unpack,
};

use crate::global::Tag;

/// The dimensions of a console, as `term.Console.geometry()` reports them.
///
/// Each dimension is independently optional: a stream has neither, and a
/// terminal may answer one query but not the other, or have one pinned by the
/// host while the other is unknown.
pub struct Geometry;

/// The dimensions, fixed when the geometry is created.
pub struct GeometryAnnex {
    pub rows: Option<u32>,
    pub cols: Option<u32>,
}

impl<'v> Object<'v> for Geometry {
    const NAME: &'v str = "Geometry";
    const MODULE: &'v str = "term";
    type Annex = GeometryAnnex;
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = strand.state::<Tag>();
        let (rows_sym, cols_sym) = (global.syms.rows, global.syms.cols);
        let ([], [rows, cols]) = unpack!(strand, args, 0, 0, rows_sym = None, cols_sym = None)?;
        let rows = dimension(strand, rows.as_deref())?;
        let cols = dimension(strand, cols.as_deref())?;
        this.create_with_annex(strand, Geometry, GeometryAnnex { rows, cols }, out);
        Ok(())
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("rows", |this, strand, out| {
                if let Some(rows) = this.annex().rows {
                    Output::set(strand, out, rows);
                }
                Ok(())
            })
            .get("cols", |this, strand, out| {
                if let Some(cols) = this.annex().cols {
                    Output::set(strand, out, cols);
                }
                Ok(())
            })
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let GeometryAnnex { rows, cols } = *this.annex();
        match (rows, cols) {
            (Some(rows), Some(cols)) => fmt!(strand, w, "<geometry {cols}x{rows}>"),
            (rows, cols) => fmt!(strand, w, "<geometry {cols:?}x{rows:?}>"),
        }
    }
}

fn dimension<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: Option<&Value<'v>>,
) -> Result<'v, 's, Option<u32>> {
    match value {
        Some(value) if !value.is_nil() => value.to_u32(strand).map(Some),
        _ => Ok(None),
    }
}

/// Creates a `term.Geometry` with the given dimensions.
pub fn create_geometry<'v>(
    strand: &mut Strand<'v, '_>,
    rows: Option<u32>,
    cols: Option<u32>,
    out: Slot<'v, '_>,
) {
    let global = strand.state::<Tag>();
    global
        .types
        .geometry
        .create_with_annex(strand, Geometry, GeometryAnnex { rows, cols }, out);
}
