use dolang::runtime::value::fmt::Format;
use dolang::runtime::{
    Instance, Object, Output, Result, Strand,
    object::{TypeBuilder, fmt},
};

/// The dimensions of the real terminal, as `shell.Console.geometry()` reports
/// them.
pub(crate) struct HostGeometry;

/// `rows`/`cols` are independently optional: `DOLANG_CONSOLE` may pin one
/// without the other (e.g. `cols=120` alone), in which case the unpinned
/// dimension falls back to a live ioctl query that may itself come up empty.
pub(crate) struct HostGeometryAnnex {
    pub(crate) rows: Option<u32>,
    pub(crate) cols: Option<u32>,
}

impl<'v> Object<'v> for HostGeometry {
    const NAME: &'v str = "Geometry";
    const MODULE: &'v str = "shell";
    type Annex = HostGeometryAnnex;
    type Type = ();
    type TypeAnnex = ();

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
        let HostGeometryAnnex { rows, cols } = *this.annex();
        match (rows, cols) {
            (Some(rows), Some(cols)) => fmt!(strand, w, "<geometry {cols}x{rows}>"),
            (rows, cols) => fmt!(strand, w, "<geometry {cols:?}x{rows:?}>"),
        }
    }
}
