use crate::{attr::required_string, check::validate_text};
use dolang::runtime::{
    Args, Instance, Object, Output, Result, Slot, Strand, Type,
    object::{TypeBuilder, fmt},
    unpack,
    value::fmt::Format,
};

pub(crate) struct Comment {
    pub(crate) text: String,
}

pub(crate) fn validate_comment<'v, 's>(
    strand: &mut Strand<'v, 's>,
    text: &str,
) -> Result<'v, 's, ()> {
    validate_text(strand, text, "XML comment")?;
    if text.contains("--") || text.ends_with('-') {
        return Err(dolang::runtime::Error::value(
            strand,
            "XML comments cannot contain -- or end in -",
        ));
    }
    Ok(())
}

impl<'v> Object<'v> for Comment {
    const MODULE: &'static str = "xml";
    const NAME: &'static str = "Comment";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();
    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([text], []) = unpack!(strand, args, 1, 0)?;
        let text = required_string(strand, &text, "comment text")?;
        validate_comment(strand, &text)?;
        this.create(strand, Self { text }, out);
        Ok(())
    }
    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        fmt!(strand, w, "<xml.Comment {:?}>", borrow.text)
    }
    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        fmt!(strand, w, "{}", borrow.text)
    }
    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("text", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                Output::set(strand, out, borrow.text.as_str());
                Ok(())
            })
            .set("text", |this, strand, value| {
                let text = required_string(strand, &value, "comment text")?;
                validate_comment(strand, &text)?;
                this.borrow_mut(strand)?.text = text;
                Ok(())
            })
    }
}
