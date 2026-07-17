use std::{borrow::Cow, mem, path::PathBuf, result};

#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;

use bstr::ByteSlice;
use diffy::{
    DiffOptions, PatchFormatter, apply_bytes,
    binary::{BinaryBlock, BinaryPatch},
    patch_set::{FileMode, FileOperation, FilePatch, ParseOptions, PatchKind, PatchSet},
};
use dolang::runtime::object::fmt;

use dolang::runtime::{
    Error, Instance, Object, Output, Result, Slot, State, Strand, Value,
    error::ResultExt,
    object::{Mut, TypeBuilder},
    unpack,
    value::{Nil, PinBin, PinStr, TypeObject},
    vm::Builder,
};

use crate::global::Global;

type BytePatch = diffy::Patch<'static, [u8]>;
type ByteOperation = FileOperation<'static, [u8]>;

#[derive(Clone)]
enum Backing<'v> {
    Str(PinStr<'v, 'static>),
    Bin(PinBin<'v, 'static>),
}

impl<'v> Backing<'v> {
    /// SAFETY: The returned pinned view borrows from `value` and must only be
    /// stored in an object that keeps `value` alive for at least as long as the
    /// pinned view via GC roots such as object slots or stack slots.
    unsafe fn from_value<'s>(
        strand: &mut Strand<'v, 's>,
        value: &Value<'v>,
    ) -> Result<'v, 's, Self> {
        if let Some(value) = value.as_str(strand.vm()) {
            Ok(Self::Str(unsafe { value.pin().into_static_unchecked() }))
        } else if let Some(value) = value.as_bin(strand.vm()) {
            Ok(Self::Bin(unsafe { value.pin().into_static_unchecked() }))
        } else {
            Err(Error::type_error(strand, "expected `str` or `bin`"))
        }
    }

    unsafe fn as_static_bytes(&self) -> &'static [u8] {
        unsafe {
            mem::transmute::<&[u8], &'static [u8]>(match self {
                Self::Str(value) => value.as_bytes(),
                Self::Bin(value) => value,
            })
        }
    }

    fn is_str(&self) -> bool {
        matches!(self, Self::Str(_))
    }
}

struct PatchPayload {
    operation: ByteOperation,
    kind: PatchKind<'static, [u8]>,
    old_mode: Option<FileMode>,
    new_mode: Option<FileMode>,
    is_git: bool,
}

impl PatchPayload {
    fn source_bytes(&self) -> Option<&[u8]> {
        let path = match &self.operation {
            FileOperation::Delete(path) => Some(path.as_ref()),
            FileOperation::Create(_) => None,
            FileOperation::Modify { original, .. } => Some(original.as_ref()),
            FileOperation::Rename { from, .. } => Some(from.as_ref()),
            FileOperation::Copy { from, .. } => Some(from.as_ref()),
        }?;
        Some(self.normalize_side(path, true))
    }

    fn target_bytes(&self) -> Option<&[u8]> {
        let path = match &self.operation {
            FileOperation::Delete(_) => None,
            FileOperation::Create(path) => Some(path.as_ref()),
            FileOperation::Modify { modified, .. } => Some(modified.as_ref()),
            FileOperation::Rename { to, .. } => Some(to.as_ref()),
            FileOperation::Copy { to, .. } => Some(to.as_ref()),
        }?;
        Some(self.normalize_side(path, false))
    }

    fn normalize_side<'a>(&self, path: &'a [u8], source: bool) -> &'a [u8] {
        if !self.is_git {
            return path;
        }
        let prefix = if source { b"a/" } else { b"b/" };
        path.strip_prefix(prefix).unwrap_or(path)
    }
}

pub(crate) struct Patch<'v> {
    payload: PatchPayload,
    _primary: Backing<'v>,
    _secondary: Option<Backing<'v>>,
}

pub(crate) struct ParseError;

pub(crate) struct ApplyError;

pub(crate) struct ErrorAnnex {
    message: String,
}

pub(crate) struct PatchIter<'v> {
    iter: PatchSet<'static, [u8]>,
    is_git: bool,
    _backing: Backing<'v>,
}

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    let source_sym = builder.sym("source");
    let target_sym = builder.sym("target");

    builder
        .module("patch")
        .value("ParseError", global.types.parse_error)
        .value("ApplyError", global.types.apply_error)
        .value("Patch", global.types.patch)
        .value("PatchIter", global.types.patch_iter)
        .function("decode", async move |strand, args, mut out| {
            let ([input], []) = unpack!(strand, args, 1, 0)?;
            // SAFETY: `input` is rooted on the stack here and is copied into
            // slot 0 of the created iterator object before this function returns.
            let backing = unsafe { Backing::from_value(strand, &input)? };
            let bytes = unsafe { backing.as_static_bytes() };
            let is_git = is_git_diff(bytes);
            let iter = PatchSet::parse_bytes(
                bytes,
                if is_git {
                    ParseOptions::gitdiff()
                } else {
                    ParseOptions::unidiff()
                },
            );

            global.types.patch_iter.create(
                strand,
                PatchIter {
                    iter,
                    is_git,
                    _backing: backing,
                },
                &mut out,
            );
            let mut borrow = global
                .types
                .patch_iter
                .downcast(&out)
                .unwrap()
                .borrow_mut_unwrap();
            Output::set(strand, Mut::slot_mut::<0>(&mut borrow), input);
            Ok(())
        })
        .function("diff", async move |strand, args, mut out| {
            let ([before, after], [source, target]) =
                unpack!(strand, args, 2, 0, source_sym = None, target_sym = None)?;
            // SAFETY: `before` and `after` are stack-rooted here and copied into
            // the patch object's slots before this function returns.
            let before_backing = unsafe { Backing::from_value(strand, &before)? };
            let after_backing = unsafe { Backing::from_value(strand, &after)? };
            if before_backing.is_str() != after_backing.is_str() {
                return Err(Error::type_error(
                    strand,
                    "before/after must both be `str` or both be `bin`",
                ));
            }

            let mut opts = DiffOptions::new();
            if let Some(source) = source {
                opts.set_original_filename(path_value_to_filename(strand, &source)?);
            }
            if let Some(target) = target {
                opts.set_modified_filename(path_value_to_filename(strand, &target)?);
            }

            let patch = unsafe {
                mem::transmute::<diffy::Patch<'_, [u8]>, diffy::Patch<'static, [u8]>>(
                    opts.create_patch_bytes(
                        before_backing.as_static_bytes(),
                        after_backing.as_static_bytes(),
                    ),
                )
            };
            let operation = FileOperation::Modify {
                original: Cow::Owned(patch.original().unwrap_or(b"original").to_vec()),
                modified: Cow::Owned(patch.modified().unwrap_or(b"modified").to_vec()),
            };

            global.types.patch.create(
                strand,
                Patch {
                    payload: PatchPayload {
                        operation,
                        kind: PatchKind::Text(patch),
                        old_mode: None,
                        new_mode: None,
                        is_git: false,
                    },
                    _primary: before_backing,
                    _secondary: Some(after_backing),
                },
                &mut out,
            );
            let mut borrow = global
                .types
                .patch
                .downcast(&out)
                .unwrap()
                .borrow_mut_unwrap();
            Output::set(strand, Mut::slot_mut::<0>(&mut borrow), before);
            Output::set(strand, Mut::slot_mut::<1>(&mut borrow), after);
            Ok(())
        })
        .function_with_slots(
            "encode",
            async move |strand, args, out, [mut input, mut item]| {
                let ([value], []) = unpack!(strand, args, 1, 0)?;

                let bytes = if let Some(patch) = global.types.patch.downcast(&value) {
                    let borrow = patch.borrow(strand)?;
                    encode_payload(strand, &borrow.payload)?
                } else {
                    value.iter(strand, &mut input).await?;
                    let mut bytes = Vec::new();
                    while input.next(strand, &mut item).await? {
                        let patch = global.types.patch.downcast(&item).ok_or_else(|| {
                            Error::type_error(strand, "expected iterable of `Patch`")
                        })?;
                        let borrow = patch.borrow(strand)?;
                        bytes.extend(encode_payload(strand, &borrow.payload)?);
                    }
                    bytes
                };
                output_bytes(strand, out, bytes);
                Ok(())
            },
        )
        .commit();
}

impl<'v> Object<'v> for Patch<'v> {
    const NAME: &'v str = "Patch";
    const MODULE: &'v str = "patch";
    const SLOTS: usize = 2;
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let create = builder.sym("CREATE");
        let delete = builder.sym("DELETE");
        let modify = builder.sym("MODIFY");
        let move_ = builder.sym("MOVE");
        let copy = builder.sym("COPY");
        let source = builder.sym("source");
        let target = builder.sym("target");
        builder
            .get("type", move |this, strand, out| {
                let borrow = this.borrow(strand)?;
                let sym = match borrow.payload.operation {
                    FileOperation::Create(_) => create,
                    FileOperation::Delete(_) => delete,
                    FileOperation::Modify { .. } => modify,
                    FileOperation::Rename { .. } => move_,
                    FileOperation::Copy { .. } => copy,
                };
                Output::set(strand, out, sym);
                Ok(())
            })
            .get("source", move |this, strand, out| {
                let borrow = this.borrow(strand)?;
                let Some(source) = borrow.payload.source_bytes() else {
                    return Err(Error::field(strand, source));
                };
                let path = bytes_to_pathbuf(source).into_do(strand)?;
                dolang_ext_shell::path(strand, path, out)
            })
            .get("target", move |this, strand, out| {
                let borrow = this.borrow(strand)?;
                let Some(target) = borrow.payload.target_bytes() else {
                    return Err(Error::field(strand, target));
                };
                let path = bytes_to_pathbuf(target).into_do(strand)?;
                dolang_ext_shell::path(strand, path, out)
            })
            .method("apply", async move |this, strand, args, out| {
                let ([base], []) = unpack!(strand, args, 1, 0)?;
                let borrow = this.borrow(strand)?;
                let (is_str, bytes) = match &borrow.payload.kind {
                    PatchKind::Text(patch) => {
                        if let Some(base) = base.as_str(strand.vm()) {
                            (
                                true,
                                strand
                                    .access(|access| {
                                        apply_bytes(base.as_str(access).as_bytes(), patch)
                                    })
                                    .map_err(|err| apply_error(strand, err.to_string()))?,
                            )
                        } else if let Some(base) = base.as_bin(strand.vm()) {
                            (
                                false,
                                strand
                                    .access(|access| apply_bytes(base.as_slice(access), patch))
                                    .map_err(|err| apply_error(strand, err.to_string()))?,
                            )
                        } else {
                            return Err(Error::type_error(strand, "expected `str` or `bin`"));
                        }
                    }
                    PatchKind::Binary(patch) => {
                        if let Some(base) = base.as_str(strand.vm()) {
                            (
                                true,
                                strand
                                    .access(|access| patch.apply(base.as_str(access).as_bytes()))
                                    .map_err(|err| apply_error(strand, err.to_string()))?,
                            )
                        } else if let Some(base) = base.as_bin(strand.vm()) {
                            (
                                false,
                                strand
                                    .access(|access| patch.apply(base.as_slice(access)))
                                    .map_err(|err| apply_error(strand, err.to_string()))?,
                            )
                        } else {
                            return Err(Error::type_error(strand, "expected `str` or `bin`"));
                        }
                    }
                };
                if is_str {
                    let text = String::from_utf8(bytes).map_err(|_| {
                        apply_error(strand, "patched result is not valid UTF-8".to_owned())
                    })?;
                    Output::set(strand, out, text.as_str());
                } else {
                    Output::set(strand, out, bytes.as_slice());
                }
                Ok(())
            })
    }
}

impl<'v> Object<'v> for ParseError {
    const NAME: &'v str = "ParseError";
    const MODULE: &'v str = "patch";
    type Annex = ErrorAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.nominal_supertype(TypeObject::ValueError)
    }

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn dolang::runtime::Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "{}", this.annex().message)
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn dolang::runtime::Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "<patch.ParseError ")?;
        Self::display(this, strand, w)?;
        fmt!(strand, w, ">")
    }
}

impl<'v> Object<'v> for ApplyError {
    const NAME: &'v str = "ApplyError";
    const MODULE: &'v str = "patch";
    type Annex = ErrorAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.nominal_supertype(TypeObject::RuntimeError)
    }

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn dolang::runtime::Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "{}", this.annex().message)
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn dolang::runtime::Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "<patch.ApplyError ")?;
        Self::display(this, strand, w)?;
        fmt!(strand, w, ">")
    }
}

impl<'v> Object<'v> for PatchIter<'v> {
    const NAME: &'v str = "PatchIter";
    const MODULE: &'v str = "patch";
    const SLOTS: usize = 1;
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.supertype(TypeObject::Iter)
    }

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
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let mut borrow = this.borrow_mut(strand)?;
        let Some(item) = borrow.iter.next() else {
            return Ok(false);
        };
        let item = item.map_err(|err| parse_error(strand, err.to_string()))?;
        let payload = into_payload(item, borrow.is_git);
        let global = strand.state::<Global<'v>>();
        // SAFETY: slot 0 roots the original patch stream for as long as the
        // produced patch object lives.
        let primary = unsafe { Backing::from_value(strand, Mut::slot::<0>(&borrow))? };
        global.types.patch.create(
            strand,
            Patch {
                payload,
                _primary: primary,
                _secondary: None,
            },
            &mut out,
        );
        let mut patch = global
            .types
            .patch
            .downcast(&out)
            .unwrap()
            .borrow_mut_unwrap();
        Output::set(
            strand,
            Mut::slot_mut::<0>(&mut patch),
            Mut::slot::<0>(&borrow),
        );
        Output::set(strand, Mut::slot_mut::<1>(&mut patch), Nil);
        Ok(true)
    }
}

fn parse_error<'v, 's>(strand: &mut Strand<'v, 's>, message: String) -> Error<'v, 's> {
    let global = strand.state::<Global<'v>>();
    Error::object_with_annex(
        strand,
        global.types.parse_error,
        ParseError,
        ErrorAnnex { message },
    )
}

fn apply_error<'v, 's>(strand: &mut Strand<'v, 's>, message: String) -> Error<'v, 's> {
    let global = strand.state::<Global<'v>>();
    Error::object_with_annex(
        strand,
        global.types.apply_error,
        ApplyError,
        ErrorAnnex { message },
    )
}

fn is_git_diff(bytes: &[u8]) -> bool {
    bytes.starts_with(b"diff --git ")
        || bytes.find(b"\ndiff --git ").is_some()
        || bytes.find(b"\nGIT binary patch").is_some()
}

fn into_payload(item: FilePatch<'static, [u8]>, is_git: bool) -> PatchPayload {
    PatchPayload {
        operation: item.operation().clone(),
        old_mode: item.old_mode().copied(),
        new_mode: item.new_mode().copied(),
        kind: item.into_patch(),
        is_git,
    }
}

fn path_value_to_filename<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
) -> Result<'v, 's, String> {
    if let Some(path) = dolang_ext_shell::as_path(strand, value) {
        Ok(path.to_string_lossy().replace('\\', "/"))
    } else if let Some(value) = value.as_str(strand) {
        Ok(value.to_string())
    } else {
        Err(Error::type_error(strand, "expected `Path` or `str`"))
    }
}

fn bytes_to_pathbuf(bytes: &[u8]) -> result::Result<PathBuf, String> {
    #[cfg(unix)]
    {
        Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec())))
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(bytes.to_vec())
            .map(PathBuf::from)
            .map_err(|_| "patch path is not valid UTF-8".to_owned())
    }
}

fn output_bytes<'v, 'a>(strand: &mut Strand<'v, '_>, out: Slot<'v, 'a>, bytes: Vec<u8>) {
    match String::from_utf8(bytes) {
        Ok(text) => Output::set(strand, out, text.as_str()),
        Err(bytes) => Output::set(strand, out, bytes.as_bytes()),
    }
}

fn encode_payload<'v, 's>(
    strand: &mut Strand<'v, 's>,
    payload: &PatchPayload,
) -> Result<'v, 's, Vec<u8>> {
    match &payload.kind {
        PatchKind::Text(patch) => encode_text_payload(strand, payload, patch),
        PatchKind::Binary(patch) => Ok(encode_binary_payload(payload, patch)),
    }
}

fn encode_text_payload<'v, 's>(
    strand: &mut Strand<'v, 's>,
    payload: &PatchPayload,
    patch: &BytePatch,
) -> Result<'v, 's, Vec<u8>> {
    let mut out = Vec::new();
    if needs_git_headers(payload) {
        write_git_headers(payload, &mut out);
    }
    PatchFormatter::new()
        .write_patch_into(patch, &mut out)
        .into_do(strand)?;
    Ok(out)
}

fn encode_binary_payload(payload: &PatchPayload, patch: &BinaryPatch<'static>) -> Vec<u8> {
    let mut out = Vec::new();
    write_git_headers(payload, &mut out);
    match patch {
        BinaryPatch::Marker => {
            let source = payload
                .source_bytes()
                .or(payload.target_bytes())
                .unwrap_or_default();
            let target = payload
                .target_bytes()
                .or(payload.source_bytes())
                .unwrap_or_default();
            out.extend_from_slice(
                format!(
                    "Binary files {} and {} differ\n",
                    git_side_path(source, true),
                    git_side_path(target, false)
                )
                .as_bytes(),
            );
        }
        BinaryPatch::Full { forward, reverse } => {
            out.extend_from_slice(b"GIT binary patch\n");
            write_binary_block(forward, &mut out);
            write_binary_block(reverse, &mut out);
        }
    }
    out
}

fn write_binary_block(block: &BinaryBlock<'_>, out: &mut Vec<u8>) {
    let kind = match block.kind {
        diffy::binary::BinaryBlockKind::Literal => "literal",
        diffy::binary::BinaryBlockKind::Delta => "delta",
    };
    out.extend_from_slice(format!("{kind} {}\n", block.data.size).as_bytes());
    out.extend_from_slice(block.data.data);
    if !block.data.data.ends_with(b"\n") {
        out.push(b'\n');
    }
    out.push(b'\n');
}

fn needs_git_headers(payload: &PatchPayload) -> bool {
    payload.old_mode.is_some()
        || payload.new_mode.is_some()
        || !matches!(
            payload.operation,
            FileOperation::Modify { ref original, ref modified } if original == modified
        )
}

fn write_git_headers(payload: &PatchPayload, out: &mut Vec<u8>) {
    let diff_old = payload
        .source_bytes()
        .or(payload.target_bytes())
        .unwrap_or_default();
    let diff_new = payload
        .target_bytes()
        .or(payload.source_bytes())
        .unwrap_or_default();
    out.extend_from_slice(
        format!(
            "diff --git {} {}\n",
            git_side_path(diff_old, true),
            git_side_path(diff_new, false)
        )
        .as_bytes(),
    );

    match &payload.operation {
        FileOperation::Create(_) => {
            if let Some(mode) = payload.new_mode {
                out.extend_from_slice(format!("new file mode {}\n", file_mode(mode)).as_bytes());
            }
        }
        FileOperation::Delete(_) => {
            if let Some(mode) = payload.old_mode {
                out.extend_from_slice(
                    format!("deleted file mode {}\n", file_mode(mode)).as_bytes(),
                );
            }
        }
        FileOperation::Rename { from, to } => {
            out.extend_from_slice(format!("rename from {}\n", raw_path(from.as_ref())).as_bytes());
            out.extend_from_slice(format!("rename to {}\n", raw_path(to.as_ref())).as_bytes());
        }
        FileOperation::Copy { from, to } => {
            out.extend_from_slice(format!("copy from {}\n", raw_path(from.as_ref())).as_bytes());
            out.extend_from_slice(format!("copy to {}\n", raw_path(to.as_ref())).as_bytes());
        }
        FileOperation::Modify { .. } => {}
    }

    if let (Some(old_mode), Some(new_mode)) = (payload.old_mode, payload.new_mode)
        && old_mode != new_mode
    {
        out.extend_from_slice(format!("old mode {}\n", file_mode(old_mode)).as_bytes());
        out.extend_from_slice(format!("new mode {}\n", file_mode(new_mode)).as_bytes());
    }
}

fn file_mode(mode: FileMode) -> &'static str {
    match mode {
        FileMode::Regular => "100644",
        FileMode::Executable => "100755",
        FileMode::Symlink => "120000",
        FileMode::Gitlink => "160000",
    }
}

fn git_side_path(path: &[u8], source: bool) -> String {
    let prefix = if source { "a/" } else { "b/" };
    let rendered = raw_path(path);
    if rendered.starts_with("a/") || rendered.starts_with("b/") {
        rendered
    } else {
        format!("{prefix}{rendered}")
    }
}

fn raw_path(path: &[u8]) -> String {
    match std::str::from_utf8(path) {
        Ok(path) if !path.contains(['\t', '\n', '\r', '"', '\\']) => path.to_owned(),
        Ok(path) => format!(
            "\"{}\"",
            path.replace('\\', "\\\\")
                .replace('\t', "\\t")
                .replace('\n', "\\n")
                .replace('\r', "\\r")
                .replace('"', "\\\"")
        ),
        Err(_) => format!("\"{}\"", String::from_utf8_lossy(path)),
    }
}
