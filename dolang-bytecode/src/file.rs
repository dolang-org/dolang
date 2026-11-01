use std::{io, ops::Range};

use serde::{Deserialize, Serialize};

use super::{
    Arg, Certificate, Error, Result, limit,
    verify::{Context, Verifier},
};

use dolang_util::verified::Verified;

pub use super::Func;

const MAGIC: [u8; 8] = *b"\xffdobytec";
const VERSION: [u8; 3] = [0, 0, 8];

const HEADER: Header = Header {
    magic: MAGIC,
    version: VERSION,
};

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
struct Header {
    magic: [u8; 8],
    version: [u8; 3],
}

/// Location of a region within the file, in absolute byte offsets
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
struct Region {
    #[serde(with = "postcard::fixint::le")]
    offset: u64,
    #[serde(with = "postcard::fixint::le")]
    len: u64,
}

/// Fixed-size directory of the file's regions, at the very end of the file
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
struct Trailer {
    main: Region,
    debug: Region,
    index: Region,
}

const TRAILER_SIZE: usize = 48;

/// Serde helpers encoding spans as (start, length), which keeps varints short
pub(crate) mod span {
    use std::ops::Range;

    use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error};

    pub(crate) fn serialize<S: Serializer>(
        span: &Range<usize>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        (span.start, span.len()).serialize(serializer)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Range<usize>, D::Error> {
        let (start, len) = <(usize, usize)>::deserialize(deserializer)?;
        let end = start
            .checked_add(len)
            .ok_or_else(|| D::Error::custom("span overflow"))?;
        Ok(start..end)
    }

    pub(crate) mod option {
        use std::ops::Range;

        use serde::{Deserialize, Deserializer, Serialize, Serializer};

        #[derive(Serialize, Deserialize)]
        struct Span(#[serde(with = "super")] Range<usize>);

        pub(crate) fn serialize<S: Serializer>(
            span: &Option<Range<usize>>,
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            span.clone().map(Span).serialize(serializer)
        }

        pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<Range<usize>>, D::Error> {
            Ok(Option::<Span>::deserialize(deserializer)?.map(|s| s.0))
        }
    }
}

/// A decoded bytecode file.
///
/// Region ranges are absolute offsets into the buffer the file was decoded
/// from. Spans in the index are relative to their region: function bodies,
/// symbol names and constants to `main`, debug information to `debug`.
#[derive(Debug, PartialEq)]
pub struct Content {
    pub main: Range<usize>,
    pub debug: Range<usize>,
    pub index: Index,
}

impl Content {
    /// The main region: strings, binary constants and function instructions
    pub fn main<'b>(&self, buffer: &'b [u8]) -> &'b [u8] {
        &buffer[self.main.clone()]
    }

    /// The debug region: debug strings, valid UTF-8 as a whole
    pub fn debug<'b>(&self, buffer: &'b [u8]) -> &'b [u8] {
        &buffer[self.debug.clone()]
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct Index {
    pub symtab: SymTable,
    pub consttab: ConstTable,
    pub packtab: PackTable,
    pub unpacktab: UnpackTable,
    pub functab: FuncTable,
    pub funcdebugtab: FuncDebugTable,
    /// Span in the debug region
    #[serde(with = "span::option")]
    pub module_name: Option<StrId>,
}

pub type StrId = Range<usize>;
pub type BinId = Range<usize>;

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub enum Const {
    Nil,
    Int(i128),
    VerbatimInt(i128, #[serde(with = "span")] StrId),
    F64(f64),
    VerbatimF64(f64, #[serde(with = "span")] StrId),
    Bool(bool),
    Str(#[serde(with = "span")] StrId),
    Sym(usize),
    Bin(#[serde(with = "span")] BinId),
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct ConstTable {
    pub content: Vec<Const>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct SymEntry {
    #[serde(with = "span")]
    pub name: StrId,
    pub private: bool,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct SymTable {
    pub content: Vec<SymEntry>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct PackTable {
    pub content: Vec<Vec<Arg>>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum UnpackKeyKind {
    /// Symbol table index
    Sym(usize),
    /// Constant table index
    Const(usize),
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct UnpackKey {
    /// Key kind (symbol or constant)
    pub kind: UnpackKeyKind,
    /// Constant table index of default value, if key is optional
    pub default: Option<usize>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct UnpackSig {
    /// Required positional arguments
    pub required: usize,
    /// Optional positional arguments (constant table index of default value)
    pub optional: Vec<usize>,
    /// Key arguments (required and optional)
    pub keys: Vec<UnpackKey>,
    /// Rest mode
    pub variadic: super::Variadic,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct UnpackTable {
    pub content: Vec<UnpackSig>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct FuncEntry {
    pub func: Func,
    pub cert: Certificate,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct FuncTable {
    pub content: Vec<FuncEntry>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct SourceLine {
    pub offset_delta: usize,
    pub line_delta: i32,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct FuncDebug {
    /// Span in the debug region
    #[serde(with = "span")]
    pub name: StrId,
    /// Source file path, a span in the debug region
    #[serde(with = "span")]
    pub file: StrId,
    pub sourcemap: Vec<SourceLine>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct FuncDebugTable {
    pub content: Vec<FuncDebug>,
}

pub fn deserialize(buffer: &[u8]) -> Result<Verified<Content>> {
    let content = deserialize_raw(buffer)?;
    verify(content, buffer)
}

fn deserialize_raw(buffer: &[u8]) -> Result<Content> {
    if buffer.len() > limit::BYTECODE_FILE_SIZE {
        return Err(Error::FileSizeLimit);
    }

    let (header, rest): (Header, _) = postcard::take_from_bytes(buffer)?;
    if header != HEADER {
        return Err(Error::InvalidHeader);
    }
    let header_end = buffer.len() - rest.len();

    let trailer_start = rest
        .len()
        .checked_sub(TRAILER_SIZE)
        .ok_or(Error::InvalidTrailer)?
        + header_end;
    let trailer: Trailer = postcard::from_bytes(&buffer[trailer_start..])?;

    // Regions lie between the header and the trailer; overlaps and gaps are harmless
    let region = |r: &Region| {
        let start = usize::try_from(r.offset).ok()?;
        let end = start.checked_add(usize::try_from(r.len).ok()?)?;
        (start >= header_end && end <= trailer_start).then_some(start..end)
    };
    let main = region(&trailer.main).ok_or(Error::InvalidTrailer)?;
    let debug = region(&trailer.debug).ok_or(Error::InvalidTrailer)?;
    let index = region(&trailer.index).ok_or(Error::InvalidTrailer)?;

    // FIXME: the overall file size limit mitigates this, but the inability to (easily) impose limits
    // during deserialization is a problem
    let (index, rest): (Index, _) = postcard::take_from_bytes(&buffer[index])?;
    if !rest.is_empty() {
        return Err(Error::IndexTrailingJunk);
    }

    Ok(Content { main, debug, index })
}

pub fn serialize<W: io::Write>(
    index: &Index,
    main: &[u8],
    debug: &[u8],
    w: &mut W,
) -> io::Result<()> {
    let header = postcard::to_stdvec(&HEADER).map_err(io::Error::other)?;
    let index = postcard::to_stdvec(index).map_err(io::Error::other)?;

    let mut offset = header.len() as u64;
    let mut region = |bytes: &[u8]| {
        let region = Region {
            offset,
            len: bytes.len() as u64,
        };
        offset += region.len;
        region
    };
    let trailer = Trailer {
        main: region(main),
        debug: region(debug),
        index: region(&index),
    };
    let trailer = postcard::to_stdvec(&trailer).map_err(io::Error::other)?;
    debug_assert_eq!(trailer.len(), TRAILER_SIZE);

    w.write_all(&header)?;
    w.write_all(main)?;
    w.write_all(debug)?;
    w.write_all(&index)?;
    w.write_all(&trailer)?;
    Ok(())
}

struct FileContext<'a> {
    index: &'a Index,
    code: &'a [u8],
}

impl Context for FileContext<'_> {
    fn code(&self) -> &[u8] {
        self.code
    }

    fn function(&self, index: usize) -> Option<&Func> {
        self.index.functab.content.get(index).map(|e| &e.func)
    }

    fn pack(&self, index: usize) -> Option<impl Iterator<Item = Arg>> {
        self.index
            .packtab
            .content
            .get(index)
            .map(|v| v.iter().cloned())
    }

    fn unpack_arity(&self, index: usize) -> Option<usize> {
        self.index.unpacktab.content.get(index).map(|s| {
            s.required
                .strict_add(s.optional.len())
                .strict_add(s.keys.len())
                .strict_add(s.variadic.captures())
        })
    }

    fn symbol_valid(&self, index: usize) -> bool {
        self.index.symtab.content.get(index).is_some()
    }

    fn constant_valid(&self, index: usize) -> bool {
        self.index.consttab.content.get(index).is_some()
    }
}

fn invalid_str(s: &StrId, tab: &[u8]) -> bool {
    invalid_bin(s, tab) || std::str::from_utf8(&tab[s.start..s.end]).is_err()
}

fn invalid_bin(s: &BinId, tab: &[u8]) -> bool {
    s.start > s.end || s.end > tab.len() || s.end - s.start >= limit::STRING_LENGTH
}

fn verify(content: Content, buffer: &[u8]) -> Result<Verified<Content>> {
    let main = content.main(buffer);
    let debug = content.debug(buffer);
    let index = &content.index;
    verify_main(main)?;
    verify_debug(debug)?;
    verify_module_name(index.module_name.as_ref(), debug)?;
    let symtab_len = verify_symtab(&index.symtab, main)?;
    verify_unpacktab(&index.unpacktab, symtab_len, index.consttab.content.len())?;
    verify_consttab(&index.consttab, main, symtab_len)?;
    verify_packtab(&index.packtab, symtab_len)?;
    verify_functab(&index.functab, &index.unpacktab, main)?;
    verify_funcdebugtab(&index.funcdebugtab, &index.functab, debug)?;

    let context = FileContext { index, code: main };
    let verifier = Verifier::new(&context);
    verifier.check(index.functab.content.iter().map(|e| (&e.func, &e.cert)))?;

    // 🫡 Don't crash
    Ok(unsafe { Verified::new(content) })
}

fn verify_debug(debug: &[u8]) -> Result<()> {
    if debug.len() > limit::DEBUG_BIN_TAB_SIZE {
        return Err(Error::DebugBinTabLimit);
    }
    std::str::from_utf8(debug).map_err(|_| Error::InvalidUtf8InDebugBinTab)?;
    Ok(())
}

// Perfunctory, instruction-level analysis done by dedicated verifier
fn verify_functab(functab: &FuncTable, unpacktab: &UnpackTable, main: &[u8]) -> Result<()> {
    if functab.content.is_empty() {
        return Err(Error::FuncTabEmpty);
    }

    if functab.content.len() > limit::FUNC_TAB_ENTRIES {
        return Err(Error::FuncTabLimit);
    }

    for (i, func) in functab.content.iter().enumerate() {
        // Function signature must be in bounds
        if func.func.sig >= unpacktab.content.len() {
            return Err(Error::InvalidUnpackInFuncTab(i));
        }

        // Validate that function signature doesn't contain constant keys
        let sig = &unpacktab.content[func.func.sig];
        for (j, key) in sig.keys.iter().enumerate() {
            if matches!(key.kind, UnpackKeyKind::Const(_)) {
                return Err(Error::ConstKeyInFunctionParam(func.func.sig, j));
            }
        }
        // Bytecode must be in bounds, non-empty and within limit
        let bytecode = &func.func.bytecode;
        if bytecode.start > bytecode.end || bytecode.end > main.len() {
            return Err(Error::InvalidCodeInFuncTab(i));
        }
        let bytecode_len = bytecode.len();
        if bytecode_len == 0 {
            return Err(Error::EmptyCode(i));
        }
        if bytecode_len > limit::FUNC_SIZE {
            return Err(Error::CodeLimit(i));
        }
        let cert_len = func.cert.blocks.len();
        if cert_len > limit::CERT_ENTRIES {
            return Err(Error::CertLimit(i));
        }
        // Stack frame size must be within limit
        if func.cert.max_operand_depth.saturating_add(func.func.locals) > limit::FUNC_FRAME_SLOTS {
            return Err(Error::StackSlotLimit(i));
        }
        // We could check upvar limits now, but they could still be exceeded by individual
        // instructions pushing the upvar stack, so this is deferred to instruction-level analysis
    }

    Ok(())
}

fn verify_main(main: &[u8]) -> Result<()> {
    if main.len() > limit::BIN_TAB_SIZE {
        return Err(Error::BinTabLimit);
    }
    Ok(())
}

fn verify_symtab(symtab: &SymTable, main: &[u8]) -> Result<usize> {
    let symtab_len = symtab.content.len();
    if symtab_len > limit::SYMBOL_TAB_ENTRIES {
        return Err(Error::SymTabLimit);
    }
    for (i, s) in symtab.content.iter().enumerate() {
        if invalid_str(&s.name, main) {
            return Err(Error::InvalidStrInSymTab(i));
        }
    }
    Ok(symtab_len)
}

fn verify_module_name(module_name: Option<&StrId>, debug: &[u8]) -> Result<()> {
    if let Some(name) = module_name
        && invalid_str(name, debug)
    {
        return Err(Error::InvalidModuleName);
    }
    Ok(())
}

fn verify_unpacktab(
    unpacktab: &UnpackTable,
    symtab_len: usize,
    consttab_len: usize,
) -> Result<usize> {
    let unpacktab_len = unpacktab.content.len();

    if unpacktab_len > limit::UNPACK_TAB_ENTRIES {
        return Err(Error::UnpackTabLimit);
    }

    for (i, u) in unpacktab.content.iter().enumerate() {
        if u.required
            .saturating_add(u.optional.len())
            .saturating_add(u.keys.len())
            .saturating_add(u.variadic.captures())
            > limit::UNPACK_ENTRIES
        {
            return Err(Error::UnpackLimit(i));
        }

        for (j, UnpackKey { kind, default }) in u.keys.iter().enumerate() {
            match kind {
                UnpackKeyKind::Sym(sym) => {
                    if *sym >= symtab_len {
                        return Err(Error::InvalidSymInUnpackTab(i, j));
                    }
                }
                UnpackKeyKind::Const(c) => {
                    if *c >= consttab_len {
                        return Err(Error::InvalidConstInUnpackTab(i, j));
                    }
                }
            }
            if let Some(default) = default
                && *default >= consttab_len
            {
                return Err(Error::InvalidConstInUnpackTab(i, j));
            }
        }
    }
    Ok(unpacktab_len)
}

fn verify_packtab(packtab: &PackTable, symtab_len: usize) -> Result<()> {
    if packtab.content.len() > limit::PACK_TAB_ENTRIES {
        return Err(Error::PackTabLimit);
    }

    for (i, p) in packtab.content.iter().enumerate() {
        if p.len() > limit::PACK_ENTRIES {
            return Err(Error::PackLimit(i));
        }

        for (j, arg) in p.iter().enumerate() {
            if let Arg::Key(idx) = arg
                && *idx >= symtab_len
            {
                return Err(Error::InvalidSymInPackTab(i, j));
            }
        }
    }
    Ok(())
}

fn verify_funcdebugtab(
    funcdebugtab: &FuncDebugTable,
    functab: &FuncTable,
    debug: &[u8],
) -> Result<()> {
    if !funcdebugtab.content.is_empty() && funcdebugtab.content.len() != functab.content.len() {
        return Err(Error::FuncDebugTabWrongSize);
    }

    for (i, c) in funcdebugtab.content.iter().enumerate() {
        if invalid_str(&c.name, debug) {
            return Err(Error::InvalidStrInFuncDebugTab(i));
        }
        if invalid_str(&c.file, debug) {
            return Err(Error::InvalidFileInFuncDebugTab(i));
        }
        if c.sourcemap.is_empty() {
            return Err(Error::SourceMapEmpty(i));
        }
        if c.sourcemap.len() > limit::SOURCE_MAP_ENTRIES {
            return Err(Error::SourceMapLimit(i));
        }
        let bytecode_len = functab.content[i].func.bytecode.len();
        let mut offset = 0usize;
        let mut line = 0u32;
        for (j, m) in c.sourcemap.iter().enumerate() {
            if j != 0 {
                offset = offset
                    .checked_add(m.offset_delta)
                    .and_then(|o| o.checked_add(1))
                    .ok_or_else(|| Error::SourceMapOffsetBounds(i, j))?;
                if offset >= bytecode_len {
                    return Err(Error::SourceMapOffsetBounds(i, j));
                }

                if m.line_delta == 0 {
                    return Err(Error::SourceMapLineDeltaZero(i, j));
                }
            }
            line = line
                .checked_add_signed(m.line_delta)
                .ok_or_else(|| Error::SourceMapLineBounds(i, j))?;
        }
    }
    Ok(())
}

fn verify_consttab(consttab: &ConstTable, main: &[u8], symtab_len: usize) -> Result<()> {
    if consttab.content.len() > limit::CONST_TAB_ENTRIES {
        return Err(Error::ConstTabLimit);
    }

    for (i, c) in consttab.content.iter().enumerate() {
        if let Const::Str(s) | Const::VerbatimInt(_, s) | Const::VerbatimF64(_, s) = c
            && invalid_str(s, main)
        {
            return Err(Error::InvalidStrInConstTab(i));
        } else if let Const::Bin(b) = c
            && invalid_bin(b, main)
        {
            return Err(Error::InvalidBinInConstTab(i));
        } else if let Const::Sym(idx) = c
            && *idx >= symtab_len
        {
            return Err(Error::InvalidSymInConstTab(i));
        }
    }

    Ok(())
}

#[cfg(test)]
mod test {
    use std::ops::{Deref, DerefMut};

    use super::*;
    use crate::{Certificate, Encode, Inst, Rest, Variadic, verify::Verifier};

    fn encode_raw(insts: &[Inst]) -> Vec<u8> {
        let mut bytecode = Vec::new();
        for inst in insts {
            inst.encode(&mut bytecode).unwrap();
        }
        bytecode
    }

    fn empty_sig() -> UnpackSig {
        UnpackSig {
            required: 0,
            optional: vec![],
            keys: vec![],
            variadic: Variadic::NONE,
        }
    }

    /// An unserialized file: index plus region contents
    struct Fixture {
        index: Index,
        main: Vec<u8>,
        debug: Vec<u8>,
    }

    impl Deref for Fixture {
        type Target = Index;

        fn deref(&self) -> &Index {
            &self.index
        }
    }

    impl DerefMut for Fixture {
        fn deref_mut(&mut self) -> &mut Index {
            &mut self.index
        }
    }

    impl Fixture {
        fn verify(self) -> Result<Verified<Content>> {
            let mut buffer = self.main;
            let main = 0..buffer.len();
            buffer.extend_from_slice(&self.debug);
            let debug = main.end..buffer.len();
            let content = Content {
                main,
                debug,
                index: self.index,
            };
            verify(content, &buffer)
        }

        fn serialize(&self) -> Vec<u8> {
            let mut out = Vec::new();
            serialize(&self.index, &self.main, &self.debug, &mut out).unwrap();
            out
        }

        /// Appends `code` to the main region and returns its span
        fn append_code(&mut self, code: &[u8]) -> BinId {
            let start = self.main.len();
            self.main.extend_from_slice(code);
            start..self.main.len()
        }
    }

    fn valid_content_with(strings: &[u8], bytecode: &[u8], debug: &[u8]) -> Fixture {
        let mut main = strings.to_vec();
        main.extend_from_slice(bytecode);
        let mut fixture = Fixture {
            index: Index {
                symtab: SymTable {
                    content: vec![SymEntry {
                        name: 0..7,
                        private: false,
                    }],
                },
                consttab: ConstTable {
                    content: vec![Const::Str(7..12)],
                },
                packtab: PackTable {
                    content: vec![vec![]],
                },
                unpacktab: UnpackTable {
                    content: vec![empty_sig()],
                },
                functab: FuncTable {
                    content: vec![FuncEntry {
                        func: Func {
                            sig: 0,
                            locals: 0,
                            upvars: vec![],
                            bytecode: strings.len()..main.len(),
                        },
                        cert: Certificate::default(),
                    }],
                },
                funcdebugtab: FuncDebugTable { content: vec![] },
                module_name: None,
            },
            main,
            debug: debug.to_vec(),
        };

        let context = FileContext {
            index: &fixture.index,
            code: &fixture.main,
        };
        let certs = Verifier::new(&context)
            .compute(fixture.index.functab.content.iter().map(|e| &e.func))
            .unwrap();
        for (entry, cert) in fixture
            .index
            .functab
            .content
            .iter_mut()
            .zip(certs.into_vec())
        {
            entry.cert = cert;
        }
        fixture
    }

    fn with_valid_content<T>(f: impl FnOnce(Fixture) -> T) -> T {
        let bytecode = encode_raw(&[Inst::LoadConst(0), Inst::Ret]);
        let content = valid_content_with(b"symnameconst", &bytecode, b"file\0func");
        f(content)
    }

    fn debug_entry(name: StrId, file: StrId, sourcemap: Vec<SourceLine>) -> FuncDebug {
        FuncDebug {
            name,
            file,
            sourcemap,
        }
    }

    fn line(offset_delta: usize, line_delta: i32) -> SourceLine {
        SourceLine {
            offset_delta,
            line_delta,
        }
    }

    fn expect_error<T>(res: Result<T>, pred: impl FnOnce(&Error) -> bool) {
        match res {
            Err(err) if pred(&err) => eprintln!("expected error: {err}"),
            Err(err) => panic!("unexpected error: {err}"),
            Ok(_) => panic!("unexpected success"),
        }
    }

    #[test]
    fn serialize_roundtrip_deserialize() {
        with_valid_content(|mut content| {
            content.funcdebugtab.content = vec![debug_entry(5..9, 0..4, vec![line(0, 1)])];
            content.module_name = Some(5..9);
            let bytes = content.serialize();
            let verified = deserialize(&bytes).unwrap();
            assert_eq!(verified.index, content.index);
            assert_eq!(verified.main(&bytes), &content.main[..]);
            assert_eq!(verified.debug(&bytes), &content.debug[..]);
        });
    }

    #[test]
    fn trailer_size() {
        let region = || Region {
            offset: u64::MAX,
            len: 0,
        };
        let trailer = Trailer {
            main: region(),
            debug: region(),
            index: region(),
        };
        assert_eq!(postcard::to_stdvec(&trailer).unwrap().len(), TRAILER_SIZE);
    }

    #[test]
    fn deserialize_rejects_invalid_header() {
        with_valid_content(|content| {
            let mut bytes = content.serialize();
            bytes[0] ^= 0xff;
            expect_error(deserialize(&bytes), |err| {
                matches!(err, Error::InvalidHeader)
            });
        });
    }

    #[test]
    fn deserialize_rejects_invalid_trailer() {
        // Too short to hold a trailer
        with_valid_content(|content| {
            let bytes = content.serialize();
            let header_len = postcard::to_stdvec(&HEADER).unwrap().len();
            expect_error(deserialize(&bytes[..header_len + 4]), |err| {
                matches!(err, Error::InvalidTrailer)
            });
        });

        // Trailing junk shifts the trailer, pushing the index region out of place
        with_valid_content(|content| {
            let mut bytes = content.serialize();
            bytes.push(0);
            expect_error(deserialize(&bytes), |err| !matches!(err, Error::Verify(_)));
        });

        // Region running into the trailer
        with_valid_content(|content| {
            let mut bytes = content.serialize();
            let len = bytes.len();
            let main_len = &mut bytes[len - TRAILER_SIZE + 8..len - TRAILER_SIZE + 16];
            main_len.copy_from_slice(&u64::MAX.to_le_bytes());
            expect_error(deserialize(&bytes), |err| {
                matches!(err, Error::InvalidTrailer)
            });
        });

        // Region overlapping the header
        with_valid_content(|content| {
            let mut bytes = content.serialize();
            let len = bytes.len();
            bytes[len - TRAILER_SIZE..len - TRAILER_SIZE + 8].copy_from_slice(&0u64.to_le_bytes());
            expect_error(deserialize(&bytes), |err| {
                matches!(err, Error::InvalidTrailer)
            });
        });
    }

    #[test]
    fn deserialize_rejects_index_trailing_junk() {
        with_valid_content(|content| {
            let header = postcard::to_stdvec(&HEADER).unwrap();
            let mut index = postcard::to_stdvec(&content.index).unwrap();
            index.push(0);
            let mut bytes = header;
            let region = |bytes: &mut Vec<u8>, data: &[u8]| {
                let offset = bytes.len() as u64;
                bytes.extend_from_slice(data);
                Region {
                    offset,
                    len: data.len() as u64,
                }
            };
            let trailer = Trailer {
                main: region(&mut bytes, &content.main),
                debug: region(&mut bytes, &content.debug),
                index: region(&mut bytes, &index),
            };
            bytes.extend_from_slice(&postcard::to_stdvec(&trailer).unwrap());
            expect_error(deserialize(&bytes), |err| {
                matches!(err, Error::IndexTrailingJunk)
            });
        });
    }

    #[test]
    fn verify_rejects_invalid_module_name() {
        with_valid_content(|mut content| {
            content.module_name = Some(99..100);
            expect_error(content.verify(), |err| {
                matches!(err, Error::InvalidModuleName)
            });
        });
    }

    #[test]
    fn verify_rejects_invalid_debugbintab_utf8() {
        let invalid_debugbintab = [0xff];
        let bytecode = encode_raw(&[Inst::LoadConst(0), Inst::Ret]);
        let content = valid_content_with(b"symnameconst", &bytecode, &invalid_debugbintab);
        expect_error(content.verify(), |err| {
            matches!(err, Error::InvalidUtf8InDebugBinTab)
        });
    }

    #[test]
    fn verify_rejects_invalid_symtab_string() {
        with_valid_content(|mut content| {
            content.symtab.content[0].name = 30..31;
            expect_error(content.verify(), |err| {
                matches!(err, Error::InvalidStrInSymTab(0))
            });
        });
    }

    #[test]
    fn verify_rejects_invalid_const_string_and_bin_and_sym() {
        with_valid_content(|mut content| {
            content.consttab.content[0] = Const::Str(30..31);
            expect_error(content.verify(), |err| {
                matches!(err, Error::InvalidStrInConstTab(0))
            });
        });

        with_valid_content(|mut content| {
            content.consttab.content[0] = Const::Bin(30..31);
            expect_error(content.verify(), |err| {
                matches!(err, Error::InvalidBinInConstTab(0))
            });
        });

        with_valid_content(|mut content| {
            content.consttab.content[0] = Const::Sym(4);
            expect_error(content.verify(), |err| {
                matches!(err, Error::InvalidSymInConstTab(0))
            });
        });
    }

    #[test]
    fn verify_rejects_invalid_pack_and_unpack_references() {
        with_valid_content(|mut content| {
            content.packtab.content[0] = vec![Arg::Key(9)];
            expect_error(content.verify(), |err| {
                matches!(err, Error::InvalidSymInPackTab(0, 0))
            });
        });

        with_valid_content(|mut content| {
            content.unpacktab.content[0].keys = vec![UnpackKey {
                kind: UnpackKeyKind::Sym(9),
                default: None,
            }];
            expect_error(content.verify(), |err| {
                matches!(err, Error::InvalidSymInUnpackTab(0, 0))
            });
        });

        with_valid_content(|mut content| {
            content.unpacktab.content[0].keys = vec![UnpackKey {
                kind: UnpackKeyKind::Const(9),
                default: None,
            }];
            expect_error(content.verify(), |err| {
                matches!(err, Error::InvalidConstInUnpackTab(0, 0))
            });
        });

        with_valid_content(|mut content| {
            content.unpacktab.content[0].keys = vec![UnpackKey {
                kind: UnpackKeyKind::Sym(0),
                default: Some(9),
            }];
            expect_error(content.verify(), |err| {
                matches!(err, Error::InvalidConstInUnpackTab(0, 0))
            });
        });
    }

    #[test]
    fn verify_rejects_const_key_in_function_param() {
        with_valid_content(|mut content| {
            content.unpacktab.content[0].keys = vec![UnpackKey {
                kind: UnpackKeyKind::Const(0),
                default: None,
            }];
            expect_error(content.verify(), |err| {
                matches!(err, Error::ConstKeyInFunctionParam(0, 0))
            });
        });
    }

    #[test]
    fn verify_rejects_invalid_unpack_in_functab() {
        with_valid_content(|mut content| {
            content.functab.content[0].func.sig = 1;
            expect_error(content.verify(), |err| {
                matches!(err, Error::InvalidUnpackInFuncTab(0))
            });
        });
    }

    #[test]
    fn verify_rejects_wrong_funcdebugtab_size() {
        with_valid_content(|mut content| {
            content.funcdebugtab.content = vec![
                FuncDebug {
                    name: 5..9,
                    file: 0..4,
                    sourcemap: vec![line(0, 1)],
                },
                FuncDebug {
                    name: 5..9,
                    file: 0..4,
                    sourcemap: vec![line(0, 1)],
                },
            ];
            expect_error(content.verify(), |err| {
                matches!(err, Error::FuncDebugTabWrongSize)
            });
        });
    }

    #[test]
    fn verify_rejects_funcdebugtab_and_sourcemap_errors() {
        with_valid_content(|mut content| {
            content.funcdebugtab.content = vec![FuncDebug {
                name: 30..31,
                file: 0..4,
                sourcemap: vec![line(0, 1)],
            }];
            expect_error(content.verify(), |err| {
                matches!(err, Error::InvalidStrInFuncDebugTab(0))
            });
        });

        with_valid_content(|mut content| {
            content.funcdebugtab.content = vec![FuncDebug {
                name: 5..9,
                file: 0..4,
                sourcemap: vec![],
            }];
            expect_error(content.verify(), |err| {
                matches!(err, Error::SourceMapEmpty(0))
            });
        });

        with_valid_content(|mut content| {
            content.funcdebugtab.content = vec![FuncDebug {
                name: 5..9,
                file: 0..4,
                sourcemap: vec![line(0, 1), line(10, 1)],
            }];
            expect_error(content.verify(), |err| {
                matches!(err, Error::SourceMapOffsetBounds(0, 1))
            });
        });

        with_valid_content(|mut content| {
            content.funcdebugtab.content = vec![FuncDebug {
                name: 5..9,
                file: 0..4,
                sourcemap: vec![line(0, 1), line(0, 0)],
            }];
            expect_error(content.verify(), |err| {
                matches!(err, Error::SourceMapLineDeltaZero(0, 1))
            });
        });

        with_valid_content(|mut content| {
            content.funcdebugtab.content = vec![FuncDebug {
                name: 5..9,
                file: 0..4,
                sourcemap: vec![line(0, -1)],
            }];
            expect_error(content.verify(), |err| {
                matches!(err, Error::SourceMapLineBounds(0, 0))
            });
        });

        with_valid_content(|mut content| {
            content.funcdebugtab.content = vec![FuncDebug {
                name: 5..9,
                file: 30..31,
                sourcemap: vec![line(0, 1)],
            }];
            expect_error(content.verify(), |err| {
                matches!(err, Error::InvalidFileInFuncDebugTab(0))
            });
        });
    }

    #[test]
    fn deserialize_rejects_file_size_limit() {
        let bytes = vec![0; limit::BYTECODE_FILE_SIZE + 1];
        expect_error(deserialize(&bytes), |err| {
            matches!(err, Error::FileSizeLimit)
        });
    }

    #[test]
    fn verify_rejects_bin_and_debug_bin_size_limits() {
        let oversized_bintab = vec![0; limit::BIN_TAB_SIZE + 1];
        let bytecode = encode_raw(&[Inst::LoadConst(0), Inst::Ret]);
        let content = valid_content_with(&oversized_bintab, &bytecode, b"file\0func");
        expect_error(content.verify(), |err| matches!(err, Error::BinTabLimit));

        let oversized_debugbintab = vec![b'a'; limit::DEBUG_BIN_TAB_SIZE + 1];
        let bytecode = encode_raw(&[Inst::LoadConst(0), Inst::Ret]);
        let content = valid_content_with(b"symnameconst", &bytecode, &oversized_debugbintab);
        expect_error(content.verify(), |err| {
            matches!(err, Error::DebugBinTabLimit)
        });
    }

    #[test]
    #[cfg(not(miri))]
    fn verify_rejects_symbol_and_constant_table_size_limits() {
        with_valid_content(|mut content| {
            content.symtab.content = (0..(limit::SYMBOL_TAB_ENTRIES + 1))
                .map(|_| SymEntry {
                    name: 0..1,
                    private: false,
                })
                .collect();
            expect_error(content.verify(), |err| matches!(err, Error::SymTabLimit));
        });

        with_valid_content(|mut content| {
            content.consttab.content = (0..(limit::CONST_TAB_ENTRIES + 1))
                .map(|_| Const::Nil)
                .collect();
            expect_error(content.verify(), |err| matches!(err, Error::ConstTabLimit));
        });
    }

    #[test]
    #[cfg(not(miri))]
    fn verify_rejects_pack_and_unpack_table_size_limits() {
        with_valid_content(|mut content| {
            content.packtab.content = (0..(limit::PACK_TAB_ENTRIES + 1)).map(|_| vec![]).collect();
            expect_error(content.verify(), |err| matches!(err, Error::PackTabLimit));
        });

        with_valid_content(|mut content| {
            content.unpacktab.content = (0..(limit::UNPACK_TAB_ENTRIES + 1))
                .map(|_| empty_sig())
                .collect();
            expect_error(content.verify(), |err| matches!(err, Error::UnpackTabLimit));
        });
    }

    #[test]
    fn verify_rejects_pack_and_unpack_entry_size_limits() {
        with_valid_content(|mut content| {
            content.packtab.content[0] =
                (0..(limit::PACK_ENTRIES + 1)).map(|_| Arg::Value).collect();
            expect_error(content.verify(), |err| matches!(err, Error::PackLimit(0)));
        });

        with_valid_content(|mut content| {
            content.unpacktab.content[0].keys = (0..(limit::UNPACK_ENTRIES + 1))
                .map(|_| UnpackKey {
                    kind: UnpackKeyKind::Sym(0),
                    default: None,
                })
                .collect();
            expect_error(content.verify(), |err| matches!(err, Error::UnpackLimit(0)));
        });

        // Rest captures count toward the limit
        with_valid_content(|mut content| {
            content.unpacktab.content[0].keys = (0..(limit::UNPACK_ENTRIES - 1))
                .map(|_| UnpackKey {
                    kind: UnpackKeyKind::Sym(0),
                    default: None,
                })
                .collect();
            content.unpacktab.content[0].variadic = Variadic::Split(Rest::Capture, Rest::Capture);
            expect_error(content.verify(), |err| matches!(err, Error::UnpackLimit(0)));
        });
    }

    #[test]
    #[cfg(not(miri))]
    fn verify_rejects_function_table_and_function_limits() {
        with_valid_content(|mut content| {
            content.functab.content.clear();
            expect_error(content.verify(), |err| matches!(err, Error::FuncTabEmpty));
        });

        with_valid_content(|mut content| {
            content.functab.content[0].func.bytecode = 0..0;
            expect_error(content.verify(), |err| matches!(err, Error::EmptyCode(0)));
        });

        with_valid_content(|mut content| {
            let end = content.main.len() + 1;
            content.functab.content[0].func.bytecode = 12..end;
            expect_error(content.verify(), |err| {
                matches!(err, Error::InvalidCodeInFuncTab(0))
            });
        });

        with_valid_content(|mut content| {
            let entry = content.functab.content.pop().unwrap();
            content.functab.content = (0..(limit::FUNC_TAB_ENTRIES + 1))
                .map(|_| FuncEntry {
                    func: Func {
                        sig: entry.func.sig,
                        locals: entry.func.locals,
                        upvars: entry.func.upvars.clone(),
                        bytecode: entry.func.bytecode.clone(),
                    },
                    cert: Certificate::default(),
                })
                .collect();
            expect_error(content.verify(), |err| matches!(err, Error::FuncTabLimit));
        });

        with_valid_content(|mut content| {
            let code = content.append_code(&vec![0; limit::FUNC_SIZE + 1]);
            content.functab.content[0].func.bytecode = code;
            expect_error(content.verify(), |err| matches!(err, Error::CodeLimit(0)));
        });

        with_valid_content(|mut content| {
            content.functab.content[0].cert.blocks = (0..(limit::CERT_ENTRIES + 1))
                .map(|_| (0, Default::default()))
                .collect();
            expect_error(content.verify(), |err| matches!(err, Error::CertLimit(0)));
        });

        with_valid_content(|mut content| {
            content.functab.content[0].cert.max_operand_depth = limit::FUNC_FRAME_SLOTS + 1;
            expect_error(content.verify(), |err| {
                matches!(err, Error::StackSlotLimit(0))
            });
        });
    }

    #[test]
    fn verify_rejects_source_map_size_limit() {
        with_valid_content(|mut content| {
            content.funcdebugtab.content = vec![FuncDebug {
                name: 5..9,
                file: 0..4,
                sourcemap: (0..(limit::SOURCE_MAP_ENTRIES + 1))
                    .map(|i| line(usize::from(i != 0), 1))
                    .collect(),
            }];
            expect_error(content.verify(), |err| {
                matches!(err, Error::SourceMapLimit(0))
            });
        });
    }
}
