use dolang::runtime::{
    Args, Error, Object, Result, Slot, Strand, Type, object::TypeBuilder, unpack,
};

/// The dimensions of a terminal-backed console.
///
/// Returned by `term.Console.geometry()`, which answers `nil` for a console
/// that is just a stream. So the presence of a `Geometry` — not the identity of
/// the console — is the "does this have a layout" test.
///
/// Native extension types cannot be abstract, so the fields here throw rather
/// than being absent. A Do class subclassing this declares `pub field rows` and
/// `pub field cols`, which shadow them; a native host names it as a nominal
/// supertype via [`crate::geometry_type`].
pub struct Geometry;

impl<'v> Object<'v> for Geometry {
    const NAME: &'v str = "Geometry";
    const MODULE: &'v str = "term";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    /// Constructible so that Do classes can subclass it: a native supertype has
    /// to be initializable for `Geometry.(init) $self` to fill its slot.
    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([], []) = unpack!(strand, args, 0, 0)?;
        this.create(strand, Geometry, out);
        Ok(())
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("rows", |_this, strand, _out| {
                Err(Error::not_supported(strand))
            })
            .get("cols", |_this, strand, _out| {
                Err(Error::not_supported(strand))
            })
    }
}
