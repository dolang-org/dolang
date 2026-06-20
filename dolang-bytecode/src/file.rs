use std::{io, marker::PhantomData, ops::Range};

use serde::{Deserialize, Serialize};

use super::{
    Arg, Certificate, Error, Phase, Result, limit,
    verify::{Context, Verifier},
};

use dolang_util::verified::Verified;

const MAGIC: [u8; 8] = *b"\xffdobytec";
const VERSION: [u8; 3] = [0, 0, 2];

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
struct Header {
    magic: [u8; 8],
    version: [u8; 3],
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct Content<'a> {
    #[serde(borrow)]
    pub bintab: BinTable<'a>,
    pub symtab: SymTable,
    pub consttab: ConstTable,
    pub packtab: PackTable,
    pub unpacktab: UnpackTable,
    #[serde(borrow)]
    pub functab: FuncTable<'a>,
    #[serde(borrow)]
    pub debugbintab: BinTable<'a>,
    pub funcdebugtab: FuncDebugTable,
    #[serde(default)]
    pub module_name: Option<StrId>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct BinTable<'a> {
    pub content: &'a [u8],
}

pub type StrId = Range<usize>;
pub type BinId = Range<usize>;

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub enum Const {
    Nil,
    Int(i128),
    VerbatimInt(i128, StrId),
    F64(f64),
    VerbatimF64(f64, StrId),
    Bool(bool),
    Str(StrId),
    Sym(usize),
    Bin(BinId),
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct ConstTable {
    pub content: Vec<Const>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct SymEntry {
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
    /// Variadic mode (None, Discard, or Capture)
    pub variadic: super::Variadic,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct UnpackTable {
    pub content: Vec<UnpackSig>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Serde<'a>(PhantomData<&'a ()>);

impl<'a> Phase for Serde<'a> {
    type Bytes = &'a [u8];
}

pub type Func<'a> = super::Func<Serde<'a>>;

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct FuncEntry<'a> {
    #[serde(borrow)]
    pub func: Func<'a>,
    pub cert: Certificate,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct FuncTable<'a> {
    #[serde(borrow)]
    pub content: Vec<FuncEntry<'a>>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct SourceLine {
    pub offset_delta: usize,
    pub line_delta: i32,
    pub file: StrId,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct FuncDebug {
    pub name: StrId,
    pub sourcemap: Vec<SourceLine>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct FuncDebugTable {
    pub content: Vec<FuncDebug>,
}

pub fn deserialize(buffer: &[u8]) -> Result<Verified<Content<'_>>> {
    deserialize_raw(buffer).and_then(verify)
}

fn deserialize_raw(buffer: &[u8]) -> Result<Content<'_>> {
    if buffer.len() > limit::BYTECODE_FILE_SIZE {
        return Err(Error::FileSizeLimit);
    }

    const HEADER: Header = Header {
        magic: MAGIC,
        version: VERSION,
    };
    let (header, read): (Header, _) = postcard::take_from_bytes(buffer)?;
    if header != HEADER {
        return Err(Error::InvalidHeader);
    }

    // FIXME: the overall file size limit mitigates this, but the inability to (easily) impose limits
    // during deserialization is a problem
    let (content, read): (Content, _) = postcard::take_from_bytes(read)?;
    if !read.is_empty() {
        return Err(Error::TrailingJunk(
            read.as_ptr().addr() - buffer.as_ptr().addr(),
        ));
    }

    Ok(content)
}

pub fn serialize<W: io::Write>(content: &Content, w: &mut W) -> io::Result<()> {
    const HEADER: Header = Header {
        magic: MAGIC,
        version: VERSION,
    };
    let w = postcard::to_io(&HEADER, w).map_err(io::Error::other)?;
    postcard::to_io(&content, w).map_err(io::Error::other)?;
    Ok(())
}

impl<'c> Context for Content<'c> {
    type Phase = Serde<'c>;

    fn slice<'a>(&'a self, bytes: &'a <Self::Phase as Phase>::Bytes) -> &'a [u8] {
        bytes
    }

    fn function(&self, index: usize) -> Option<&Func<'c>> {
        self.functab.content.get(index).map(|e| &e.func)
    }

    fn pack(&self, index: usize) -> Option<impl Iterator<Item = Arg>> {
        self.packtab.content.get(index).map(|v| v.iter().cloned())
    }

    fn unpack_arity(&self, index: usize) -> Option<usize> {
        self.unpacktab.content.get(index).map(|s| {
            s.required
                .strict_add(s.optional.len())
                .strict_add(s.keys.len())
                .strict_add(match s.variadic {
                    super::Variadic::None | super::Variadic::Discard => 0,
                    super::Variadic::Capture => 1,
                })
        })
    }

    fn symbol_valid(&self, index: usize) -> bool {
        self.symtab.content.get(index).is_some()
    }

    fn constant_valid(&self, index: usize) -> bool {
        self.consttab.content.get(index).is_some()
    }
}

fn invalid_str(s: &StrId, tab: &[u8]) -> bool {
    s.start > s.end
        || s.end > tab.len()
        || s.end - s.start >= limit::STRING_LENGTH
        || std::str::from_utf8(&tab[s.start..s.end]).is_err()
}

fn invalid_bin(s: &BinId, tab: &[u8]) -> bool {
    s.start > s.end || s.end > tab.len() || s.end - s.start >= limit::STRING_LENGTH
}

fn verify(content: Content) -> Result<Verified<Content>> {
    verify_bintab(&content.bintab)?;
    verify_debugbintab(&content.debugbintab)?;
    verify_module_name(content.module_name.as_ref(), content.debugbintab.content)?;
    let symtab_len = verify_symtab(&content.symtab, content.bintab.content)?;
    verify_unpacktab(
        &content.unpacktab,
        symtab_len,
        content.consttab.content.len(),
    )?;
    verify_consttab(&content.consttab, content.bintab.content, symtab_len)?;
    verify_packtab(&content.packtab, symtab_len)?;
    verify_functab(&content.functab, &content.unpacktab)?;
    verify_funcdebugtab(
        &content.funcdebugtab,
        &content.functab,
        content.debugbintab.content,
    )?;

    let verifier = Verifier::new(&content);
    verifier.check(content.functab.content.iter().map(|e| (&e.func, &e.cert)))?;

    // 🫡 Don't crash
    Ok(unsafe { Verified::new(content) })
}

fn verify_debugbintab(debugbintab: &BinTable) -> Result<()> {
    let debugbintab_len = debugbintab.content.len();
    if debugbintab_len > limit::DEBUG_BIN_TAB_SIZE {
        return Err(Error::DebugBinTabLimit);
    }
    std::str::from_utf8(debugbintab.content).map_err(|_| Error::InvalidUtf8InDebugBinTab)?;
    Ok(())
}

// Perfunctory, instruction-level analysis done by dedicated verifier
fn verify_functab(functab: &FuncTable, unpacktab: &UnpackTable) -> Result<()> {
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
        // Bytecode must be non-empty and within limit
        let bytecode_len = func.func.bytecode.len();
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

fn verify_bintab(bintab: &BinTable) -> Result<()> {
    let bintab_len = bintab.content.len();
    if bintab_len > limit::BIN_TAB_SIZE {
        return Err(Error::BinTabLimit);
    }
    Ok(())
}

fn verify_symtab(symtab: &SymTable, bintab: &[u8]) -> Result<usize> {
    let symtab_len = symtab.content.len();
    if symtab_len > limit::SYMBOL_TAB_ENTRIES {
        return Err(Error::SymTabLimit);
    }
    for (i, s) in symtab.content.iter().enumerate() {
        if invalid_str(&s.name, bintab) {
            return Err(Error::InvalidStrInSymTab(i));
        }
    }
    Ok(symtab_len)
}

fn verify_module_name(module_name: Option<&StrId>, bintab: &[u8]) -> Result<()> {
    if let Some(name) = module_name
        && invalid_str(name, bintab)
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
    debugbintab: &[u8],
) -> Result<()> {
    if !funcdebugtab.content.is_empty() && funcdebugtab.content.len() != functab.content.len() {
        return Err(Error::FuncDebugTabWrongSize);
    }

    for (i, c) in funcdebugtab.content.iter().enumerate() {
        if invalid_str(&c.name, debugbintab) {
            return Err(Error::InvalidStrInFuncDebugTab(i));
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
            if invalid_str(&m.file, debugbintab) {
                return Err(Error::InvalidStrInSourceMap(i, j));
            }
        }
    }
    Ok(())
}

fn verify_consttab(consttab: &ConstTable, bintab: &[u8], symtab_len: usize) -> Result<()> {
    if consttab.content.len() > limit::CONST_TAB_ENTRIES {
        return Err(Error::ConstTabLimit);
    }

    for (i, c) in consttab.content.iter().enumerate() {
        if let Const::Str(s) = c
            && invalid_str(s, bintab)
        {
            return Err(Error::InvalidStrInConstTab(i));
        } else if let Const::Bin(b) = c
            && invalid_bin(b, bintab)
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
    use super::*;
    use crate::{Certificate, Encode, Inst, Variadic, verify::Verifier};

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
            variadic: Variadic::None,
        }
    }

    fn valid_content_with<'a>(
        bintab: &'a [u8],
        bytecode: &'a [u8],
        debugbintab: &'a [u8],
    ) -> Content<'a> {
        let mut content = Content {
            bintab: BinTable { content: bintab },
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
                        bytecode,
                    },
                    cert: Certificate::default(),
                }],
            },
            debugbintab: BinTable {
                content: debugbintab,
            },
            funcdebugtab: FuncDebugTable { content: vec![] },
            module_name: None,
        };

        let certs = Verifier::new(&content)
            .compute(content.functab.content.iter().map(|e| &e.func))
            .unwrap();
        for (entry, cert) in content.functab.content.iter_mut().zip(certs.into_vec()) {
            entry.cert = cert;
        }
        content
    }

    fn with_valid_content<T>(f: impl FnOnce(Content<'_>) -> T) -> T {
        let bytecode = encode_raw(&[Inst::LoadConst(0), Inst::Ret]);
        let content = valid_content_with(b"symnameconst", &bytecode, b"file\0func");
        f(content)
    }

    fn serialize_bytes(content: &Content<'_>) -> Vec<u8> {
        let mut out = Vec::new();
        serialize(content, &mut out).unwrap();
        out
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
        with_valid_content(|content| {
            let bytes = serialize_bytes(&content);
            let verified = deserialize(&bytes).unwrap();
            assert_eq!(
                verified.functab.content.len(),
                content.functab.content.len()
            );
            assert_eq!(verified.consttab, content.consttab);
            assert_eq!(verified.symtab, content.symtab);
            assert_eq!(verified.module_name, content.module_name);
        });
    }

    #[test]
    fn deserialize_rejects_invalid_header() {
        with_valid_content(|content| {
            let mut bytes = serialize_bytes(&content);
            bytes[0] ^= 0xff;
            expect_error(deserialize(&bytes), |err| {
                matches!(err, Error::InvalidHeader)
            });
        });
    }

    #[test]
    fn deserialize_rejects_trailing_junk() {
        with_valid_content(|content| {
            let mut bytes = serialize_bytes(&content);
            bytes.push(0);
            expect_error(deserialize(&bytes), |err| {
                matches!(err, Error::TrailingJunk(_))
            });
        });
    }

    #[test]
    fn verify_rejects_invalid_module_name() {
        with_valid_content(|mut content| {
            content.module_name = Some(99..100);
            expect_error(verify(content), |err| {
                matches!(err, Error::InvalidModuleName)
            });
        });
    }

    #[test]
    fn verify_rejects_invalid_debugbintab_utf8() {
        let invalid_debugbintab = [0xff];
        let bytecode = encode_raw(&[Inst::LoadConst(0), Inst::Ret]);
        let content = valid_content_with(b"symnameconst", &bytecode, &invalid_debugbintab);
        expect_error(verify(content), |err| {
            matches!(err, Error::InvalidUtf8InDebugBinTab)
        });
    }

    #[test]
    fn verify_rejects_invalid_symtab_string() {
        with_valid_content(|mut content| {
            content.symtab.content[0].name = 30..31;
            expect_error(verify(content), |err| {
                matches!(err, Error::InvalidStrInSymTab(0))
            });
        });
    }

    #[test]
    fn verify_rejects_invalid_const_string_and_bin_and_sym() {
        with_valid_content(|mut content| {
            content.consttab.content[0] = Const::Str(30..31);
            expect_error(verify(content), |err| {
                matches!(err, Error::InvalidStrInConstTab(0))
            });
        });

        with_valid_content(|mut content| {
            content.consttab.content[0] = Const::Bin(30..31);
            expect_error(verify(content), |err| {
                matches!(err, Error::InvalidBinInConstTab(0))
            });
        });

        with_valid_content(|mut content| {
            content.consttab.content[0] = Const::Sym(4);
            expect_error(verify(content), |err| {
                matches!(err, Error::InvalidSymInConstTab(0))
            });
        });
    }

    #[test]
    fn verify_rejects_invalid_pack_and_unpack_references() {
        with_valid_content(|mut content| {
            content.packtab.content[0] = vec![Arg::Key(9)];
            expect_error(verify(content), |err| {
                matches!(err, Error::InvalidSymInPackTab(0, 0))
            });
        });

        with_valid_content(|mut content| {
            content.unpacktab.content[0].keys = vec![UnpackKey {
                kind: UnpackKeyKind::Sym(9),
                default: None,
            }];
            expect_error(verify(content), |err| {
                matches!(err, Error::InvalidSymInUnpackTab(0, 0))
            });
        });

        with_valid_content(|mut content| {
            content.unpacktab.content[0].keys = vec![UnpackKey {
                kind: UnpackKeyKind::Const(9),
                default: None,
            }];
            expect_error(verify(content), |err| {
                matches!(err, Error::InvalidConstInUnpackTab(0, 0))
            });
        });

        with_valid_content(|mut content| {
            content.unpacktab.content[0].keys = vec![UnpackKey {
                kind: UnpackKeyKind::Sym(0),
                default: Some(9),
            }];
            expect_error(verify(content), |err| {
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
            expect_error(verify(content), |err| {
                matches!(err, Error::ConstKeyInFunctionParam(0, 0))
            });
        });
    }

    #[test]
    fn verify_rejects_invalid_unpack_in_functab() {
        with_valid_content(|mut content| {
            content.functab.content[0].func.sig = 1;
            expect_error(verify(content), |err| {
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
                    sourcemap: vec![SourceLine {
                        offset_delta: 0,
                        line_delta: 1,
                        file: 0..4,
                    }],
                },
                FuncDebug {
                    name: 5..9,
                    sourcemap: vec![SourceLine {
                        offset_delta: 0,
                        line_delta: 1,
                        file: 0..4,
                    }],
                },
            ];
            expect_error(verify(content), |err| {
                matches!(err, Error::FuncDebugTabWrongSize)
            });
        });
    }

    #[test]
    fn verify_rejects_funcdebugtab_and_sourcemap_errors() {
        with_valid_content(|mut content| {
            content.funcdebugtab.content = vec![FuncDebug {
                name: 30..31,
                sourcemap: vec![SourceLine {
                    offset_delta: 0,
                    line_delta: 1,
                    file: 0..4,
                }],
            }];
            expect_error(verify(content), |err| {
                matches!(err, Error::InvalidStrInFuncDebugTab(0))
            });
        });

        with_valid_content(|mut content| {
            content.funcdebugtab.content = vec![FuncDebug {
                name: 5..9,
                sourcemap: vec![],
            }];
            expect_error(verify(content), |err| {
                matches!(err, Error::SourceMapEmpty(0))
            });
        });

        with_valid_content(|mut content| {
            content.funcdebugtab.content = vec![FuncDebug {
                name: 5..9,
                sourcemap: vec![
                    SourceLine {
                        offset_delta: 0,
                        line_delta: 1,
                        file: 0..4,
                    },
                    SourceLine {
                        offset_delta: 10,
                        line_delta: 1,
                        file: 0..4,
                    },
                ],
            }];
            expect_error(verify(content), |err| {
                matches!(err, Error::SourceMapOffsetBounds(0, 1))
            });
        });

        with_valid_content(|mut content| {
            content.funcdebugtab.content = vec![FuncDebug {
                name: 5..9,
                sourcemap: vec![
                    SourceLine {
                        offset_delta: 0,
                        line_delta: 1,
                        file: 0..4,
                    },
                    SourceLine {
                        offset_delta: 0,
                        line_delta: 0,
                        file: 0..4,
                    },
                ],
            }];
            expect_error(verify(content), |err| {
                matches!(err, Error::SourceMapLineDeltaZero(0, 1))
            });
        });

        with_valid_content(|mut content| {
            content.funcdebugtab.content = vec![FuncDebug {
                name: 5..9,
                sourcemap: vec![SourceLine {
                    offset_delta: 0,
                    line_delta: -1,
                    file: 0..4,
                }],
            }];
            expect_error(verify(content), |err| {
                matches!(err, Error::SourceMapLineBounds(0, 0))
            });
        });

        with_valid_content(|mut content| {
            content.funcdebugtab.content = vec![FuncDebug {
                name: 5..9,
                sourcemap: vec![SourceLine {
                    offset_delta: 0,
                    line_delta: 1,
                    file: 30..31,
                }],
            }];
            expect_error(verify(content), |err| {
                matches!(err, Error::InvalidStrInSourceMap(0, 0))
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
        expect_error(verify(content), |err| matches!(err, Error::BinTabLimit));

        let oversized_debugbintab = vec![b'a'; limit::DEBUG_BIN_TAB_SIZE + 1];
        let bytecode = encode_raw(&[Inst::LoadConst(0), Inst::Ret]);
        let content = valid_content_with(b"symnameconst", &bytecode, &oversized_debugbintab);
        expect_error(verify(content), |err| {
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
            expect_error(verify(content), |err| matches!(err, Error::SymTabLimit));
        });

        with_valid_content(|mut content| {
            content.consttab.content = (0..(limit::CONST_TAB_ENTRIES + 1))
                .map(|_| Const::Nil)
                .collect();
            expect_error(verify(content), |err| matches!(err, Error::ConstTabLimit));
        });
    }

    #[test]
    #[cfg(not(miri))]
    fn verify_rejects_pack_and_unpack_table_size_limits() {
        with_valid_content(|mut content| {
            content.packtab.content = (0..(limit::PACK_TAB_ENTRIES + 1)).map(|_| vec![]).collect();
            expect_error(verify(content), |err| matches!(err, Error::PackTabLimit));
        });

        with_valid_content(|mut content| {
            content.unpacktab.content = (0..(limit::UNPACK_TAB_ENTRIES + 1))
                .map(|_| empty_sig())
                .collect();
            expect_error(verify(content), |err| matches!(err, Error::UnpackTabLimit));
        });
    }

    #[test]
    fn verify_rejects_pack_and_unpack_entry_size_limits() {
        with_valid_content(|mut content| {
            content.packtab.content[0] =
                (0..(limit::PACK_ENTRIES + 1)).map(|_| Arg::Value).collect();
            expect_error(verify(content), |err| matches!(err, Error::PackLimit(0)));
        });

        with_valid_content(|mut content| {
            content.unpacktab.content[0].keys = (0..(limit::UNPACK_ENTRIES + 1))
                .map(|_| UnpackKey {
                    kind: UnpackKeyKind::Sym(0),
                    default: None,
                })
                .collect();
            expect_error(verify(content), |err| matches!(err, Error::UnpackLimit(0)));
        });
    }

    #[test]
    #[cfg(not(miri))]
    fn verify_rejects_function_table_and_function_limits() {
        with_valid_content(|mut content| {
            content.functab.content.clear();
            expect_error(verify(content), |err| matches!(err, Error::FuncTabEmpty));
        });

        with_valid_content(|mut content| {
            content.functab.content[0].func.bytecode = &[];
            expect_error(verify(content), |err| matches!(err, Error::EmptyCode(0)));
        });

        let valid_bytecode = encode_raw(&[Inst::LoadConst(0), Inst::Ret]);
        let mut content = valid_content_with(b"symnameconst", &valid_bytecode, b"file\0func");
        let entry = FuncEntry {
            func: Func {
                sig: 0,
                locals: 0,
                upvars: vec![],
                bytecode: &valid_bytecode,
            },
            cert: Certificate::default(),
        };
        content.functab.content = (0..(limit::FUNC_TAB_ENTRIES + 1))
            .map(|_| FuncEntry {
                func: Func {
                    sig: entry.func.sig,
                    locals: entry.func.locals,
                    upvars: entry.func.upvars.clone(),
                    bytecode: entry.func.bytecode,
                },
                cert: Certificate::default(),
            })
            .collect();
        expect_error(verify(content), |err| matches!(err, Error::FuncTabLimit));

        let valid_bytecode = encode_raw(&[Inst::LoadConst(0), Inst::Ret]);
        let oversized_bytecode = vec![0; limit::FUNC_SIZE + 1];
        let mut content = valid_content_with(b"symnameconst", &valid_bytecode, b"file\0func");
        content.functab.content[0].func.bytecode = &oversized_bytecode;
        expect_error(verify(content), |err| matches!(err, Error::CodeLimit(0)));

        with_valid_content(|mut content| {
            content.functab.content[0].cert.blocks = (0..(limit::CERT_ENTRIES + 1))
                .map(|_| (0, Default::default()))
                .collect();
            expect_error(verify(content), |err| matches!(err, Error::CertLimit(0)));
        });

        with_valid_content(|mut content| {
            content.functab.content[0].cert.max_operand_depth = limit::FUNC_FRAME_SLOTS + 1;
            expect_error(verify(content), |err| {
                matches!(err, Error::StackSlotLimit(0))
            });
        });
    }

    #[test]
    fn verify_rejects_source_map_size_limit() {
        with_valid_content(|mut content| {
            content.funcdebugtab.content = vec![FuncDebug {
                name: 5..9,
                sourcemap: (0..(limit::SOURCE_MAP_ENTRIES + 1))
                    .map(|i| SourceLine {
                        offset_delta: usize::from(i != 0),
                        line_delta: 1,
                        file: 0..4,
                    })
                    .collect(),
            }];
            expect_error(verify(content), |err| {
                matches!(err, Error::SourceMapLimit(0))
            });
        });
    }
}
