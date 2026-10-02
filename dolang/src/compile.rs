pub use dolang_compile::{
    BinderKind, Config, Context, EmitToken, Error, ErrorKind, ImplicitKind, Items, Kind, Mode,
    Node, NodeId, Prelude, RestKind, Token, TypeArg, TypeArgKind, TypeArgs, TypeConst, TypeExpr,
    TypeExprs, TypeKind, TypeParam, TypeParamKind, TypeParams, TypeQuant, Unit, UnitId,
    diag::{
        Annotation, AnnotationKind, Diag, Note, NoteKind, Patch, Pos, Severity, SourceSpan, Span,
    },
    typeck,
};

static TYPELIBS: &[(&str, &[u8])] = crate::typelibs!();

/// The typelibs of the modules this crate and the linked extensions provide,
/// paired with their module names: std and `strand`, then each extension's
/// [`TYPELIBS`](crate::extension::Extension::TYPELIBS).
///
/// Read one with [`typeck::Typelib::read`] to check code against the module.
pub fn typelibs() -> impl Iterator<Item = (&'static str, &'static [u8])> {
    let extensions = crate::extension::extensions().flat_map(|ext| ext.typelibs());
    TYPELIBS.iter().chain(extensions).copied()
}
