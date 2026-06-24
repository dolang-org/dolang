use dolang::runtime::{Error, Strand};

pub(crate) fn print_backtrace<'v, 's>(strand: &mut Strand<'v, 's>, error: Error<'v, 's>) {
    dolang_ext_diagnostic::print_error_stderr(
        strand,
        error,
        dolang_ext_diagnostic::ColorMode::Auto,
    );
}
