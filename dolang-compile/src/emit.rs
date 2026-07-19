use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    io::{self, Write},
    mem,
};

use bytecode::Inst as BcInst;
use dolang_util::{alias::Box, intern::BinTable};

use dolang_bytecode::{
    self as bytecode, Certificate, InstEncoder, Phase, file,
    verify::{Context, Verifier},
};

use super::{
    Mode,
    cfg::{self, BlockId, BlockRef, Func, Graph, InstInfo, TermInfo},
    constant::{self, Const},
    sig::{self, Arg},
    source::{File, Span},
    sym,
};

struct SpanEntry {
    offset: usize,
    span: Span,
}

#[derive(Default)]
struct Block {
    bytecode: RefCell<Vec<u8>>,
    spans: RefCell<Vec<SpanEntry>>,
    term_offset: Cell<usize>,
    offset: Cell<usize>,
}

fn checked_signed_diff(left: usize, right: usize) -> Option<isize> {
    if left < right {
        0isize.checked_sub_unsigned(right - left)
    } else {
        (left - right).try_into().ok()
    }
}

struct FuncEmitter<'a, 'b> {
    emitter: &'b mut Emitter<'a>,
    order: Box<[BlockId]>,
    map: HashMap<BlockId, usize>,
    blocks: Box<[Block]>,
    func: &'b Func,
}

impl<'a, 'b> FuncEmitter<'a, 'b> {
    fn topological_sort_rec(
        emitter: &Emitter<'a>,
        block: BlockRef<'a>,
        seen: &mut HashSet<BlockId>,
        out: &mut Vec<BlockId>,
    ) {
        match block.term.0 {
            // No successors
            TermInfo::Ret | TermInfo::NlBranch(..) => (),
            TermInfo::Branch(bid) => {
                if seen.insert(bid) {
                    out.push(bid);
                    Self::topological_sort_rec(emitter, emitter.graph.block(bid), seen, out);
                }
            }
            TermInfo::If(tid, fid) => {
                if seen.insert(tid) {
                    out.push(tid);
                    Self::topological_sort_rec(emitter, emitter.graph.block(tid), seen, out);
                }
                if seen.insert(fid) {
                    out.push(fid);
                    Self::topological_sort_rec(emitter, emitter.graph.block(fid), seen, out);
                }
            }
        }
    }

    fn topological_sort(emitter: &Emitter<'a>, func: &Func) -> Box<[BlockId]> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let enter = func.enter;
        out.push(enter);
        Self::topological_sort_rec(emitter, emitter.graph.block(enter), &mut seen, &mut out);
        out.into()
    }

    fn new(emitter: &'b mut Emitter<'a>, func: &'b Func) -> Self {
        let order = Self::topological_sort(emitter, func);
        let map = order.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        let blocks = (0..order.len()).map(|_| Default::default()).collect();

        Self {
            emitter,
            map,
            order,
            blocks,
            func,
        }
    }

    fn visit_insts(&self, output: &Block, input: &BlockRef) {
        use InstInfo::*;

        let mut borrow = output.bytecode.borrow_mut();
        let mut cursor = io::Cursor::new(&mut *borrow);
        // All method calls below should succeed when writing to a `Vec<u8>`
        let mut write = InstEncoder::new(&mut cursor);
        let mut spans = output.spans.borrow_mut();

        for inst in input.insts.iter() {
            spans.push(SpanEntry {
                offset: write.offset().unwrap(),
                span: inst.1,
            });
            let inst = match inst.0 {
                Nop => {
                    spans.pop();
                    continue;
                }
                Add => BcInst::Add,
                Div => BcInst::Div,
                Ediv => BcInst::Ediv,
                Dup => BcInst::Dup,
                Swap(i, j) => BcInst::Swap(i, j),
                Mod => BcInst::Mod,
                Mul => BcInst::Mul,
                Neg => BcInst::Neg,
                Not => BcInst::Not,
                BitNot => BcInst::BitNot,
                BitAnd => BcInst::BitAnd,
                BitOr => BcInst::BitOr,
                BitXor => BcInst::BitXor,
                Shl => BcInst::Shl,
                Shr => BcInst::Shr,
                Pop => BcInst::Pop,
                Eq => BcInst::Eq,
                Ne => BcInst::Ne,
                Gt => BcInst::Gt,
                Lt => BcInst::Lt,
                Gte => BcInst::Gte,
                Lte => BcInst::Lte,
                Sub => BcInst::Sub,
                PopUpvars => BcInst::PopUpvars,
                Call(id) => BcInst::Call(id.index()),
                MethodCall(sym, sig) => BcInst::MethodCall(sym.index(), sig.index()),
                Builtin(idx, sig) => BcInst::Builtin(idx, sig.index()),
                LoadConst(id) => BcInst::LoadConst(id.index()),
                Close(id) => BcInst::Close(id.index()),
                LoadLocal(index) => BcInst::LoadLocal(index),
                StoreLocal(index) => BcInst::StoreLocal(index),
                PushUpvars(count) => BcInst::PushUpvars(count),
                LoadUpvar(index, depth) => BcInst::LoadUpvar(index, depth),
                StoreUpvar(index, depth) => BcInst::StoreUpvar(index, depth),
                Get(sym) => BcInst::Get(sym.index()),
                Set(sym) => BcInst::Set(sym.index()),
                Index => BcInst::Index,
                Assign => BcInst::Assign,
                Reify(id) => BcInst::Reify(id.index()),
                Next => BcInst::Next,
                Unpack(id) => BcInst::Unpack(id.index()),
                NlGuard(id) => BcInst::NlGuard(id.index()),
            };
            inst.write(&mut write).unwrap();
        }
        output.term_offset.set(borrow.len());
    }

    fn visit_term(&self, output: &Block, input: &BlockRef, next: Option<BlockId>, initial: bool) {
        use TermInfo::*;

        let mut borrow = output.bytecode.borrow_mut();
        let len = borrow.len();
        let trunc = output.term_offset.get();
        borrow.truncate(trunc);
        let mut cursor = io::Cursor::new(&mut *borrow);
        cursor.set_position(trunc.try_into().unwrap());
        // All method calls below should succeed when writing to a `Vec<u8>`
        let mut write = InstEncoder::new(&mut cursor);

        let mut spans = output.spans.borrow_mut();

        if initial {
            spans.push(SpanEntry {
                offset: trunc,
                span: input.term.1,
            });
        }

        let inst = match &input.term.0 {
            Ret => return BcInst::Ret.write(&mut write).unwrap(),
            Branch(id) if Some(*id) != next => {
                let i = self.map[id];
                let dest = self.blocks[i].offset.get();
                let mut src = output.offset.get().checked_add(len).unwrap();
                if initial {
                    // Guess a 1-byte offset
                    src = src.checked_add(1).unwrap();
                }
                BcInst::Branch(checked_signed_diff(dest, src).unwrap())
            }
            Branch(_) => {
                spans.pop();
                return;
            }
            If(tid, fid) => {
                if Some(*tid) == next {
                    let i = self.map[fid];
                    let dest = self.blocks[i].offset.get();
                    let mut src = output.offset.get().checked_add(len).unwrap();
                    if initial {
                        src = src.checked_add(1).unwrap();
                    }
                    BcInst::BranchFalse(checked_signed_diff(dest, src).unwrap())
                } else if Some(*fid) == next {
                    let i = self.map[tid];
                    let dest = self.blocks[i].offset.get();
                    let mut src = output.offset.get().checked_add(len).unwrap();
                    if initial {
                        src = src.checked_add(1).unwrap();
                    }
                    BcInst::BranchTrue(checked_signed_diff(dest, src).unwrap())
                } else {
                    panic!("neither successor is next in block order");
                }
            }
            NlBranch(depth, indicator) => BcInst::NlBranch(*depth, *indicator as usize),
        };
        inst.write(&mut write).unwrap()
    }

    fn debug_table(&self) -> file::FuncDebug {
        let path = self.emitter.file.path().to_string_lossy();
        let path = self.emitter.debugbintab.borrow_mut().id_str(&path);
        let path = file::StrId {
            start: path.start(),
            end: path.end(),
        };
        let name_str: Cow<'_, str> = self.emitter.qualified_name(self.func).into();
        let name = self.emitter.debugbintab.borrow_mut().id_str(&name_str);
        let name = file::StrId {
            start: name.start(),
            end: name.end(),
        };

        let mut sourcemap = Vec::new();

        let mut spans = self.blocks.iter().flat_map(|block| {
            let spans = block.spans.borrow();
            let offset = block.offset.get();
            spans
                .iter()
                .map(|s| {
                    (
                        s.offset + offset,
                        self.emitter
                            .file
                            .coord_span(s.span)
                            .start
                            .line
                            .try_into()
                            .unwrap(),
                    )
                })
                .collect::<Vec<_>>()
        });
        let mut last = spans.next().expect("empty block?!");
        sourcemap.push(file::SourceLine {
            offset_delta: 0,
            line_delta: last.1,
            file: path.clone(),
        });
        for span in spans {
            if span.1 != last.1 {
                sourcemap.push(file::SourceLine {
                    offset_delta: (span.0 - last.0) - 1,
                    line_delta: span.1 - last.1,
                    file: path.clone(),
                });
                last = span
            }
        }

        file::FuncDebug { name, sourcemap }
    }

    fn emit(&mut self) -> (Vec<u8>, file::FuncDebug) {
        let mut offset = 0usize;
        for (i, (id, block)) in self.order.iter().zip(self.blocks.iter()).enumerate() {
            block.offset.set(offset);
            let source = self.emitter.graph.block(*id);
            self.visit_insts(block, &source);
            offset = offset.checked_add(block.bytecode.borrow().len()).unwrap();
            self.visit_term(block, &source, self.order.get(i + 1).copied(), true);
        }
        let mut changing = true;
        while changing {
            offset = 0usize;
            changing = false;
            for (i, (id, block)) in self.order.iter().zip(self.blocks.iter()).enumerate() {
                if block.offset.get() != offset {
                    block.offset.set(offset);
                    changing = true;
                }
                self.visit_term(
                    block,
                    &self.emitter.graph.block(*id),
                    self.order.get(i + 1).copied(),
                    false,
                );
                offset = offset.checked_add(block.bytecode.borrow().len()).unwrap();
            }
        }
        let mut bytecode = Vec::new();
        let write = &mut bytecode;
        for block in self.blocks.iter() {
            write.write_all(&block.bytecode.borrow()).unwrap();
        }
        (bytecode, self.debug_table())
    }
}

struct Emit;

impl Phase for Emit {
    type Bytes = Box<[u8]>;
}

type EmitFunc = bytecode::Func<Emit>;

pub(crate) struct Emitter<'a> {
    pub(crate) file: &'a File<'a>,
    pub(crate) graph: &'a Graph,
    pub(crate) bintab: &'a BinTable,
    pub(crate) symtab: &'a sym::Table,
    pub(crate) consttab: &'a constant::Table,
    pub(crate) packtab: &'a sig::PackTable,
    pub(crate) unpacktab: &'a sig::UnpackTable,
    pub(crate) debugbintab: RefCell<BinTable>,
    pub(crate) mode: Mode<'a>,
}

struct EmitContext<'c>(&'c Emitter<'c>, &'c [EmitFunc]);

impl<'c> Context for EmitContext<'c> {
    type Phase = Emit;

    fn slice<'a>(&'a self, bytes: &'a <Self::Phase as Phase>::Bytes) -> &'a [u8] {
        &bytes[..]
    }

    fn function(&self, index: usize) -> Option<&EmitFunc> {
        self.1.get(index)
    }

    fn pack(&self, index: usize) -> Option<impl Iterator<Item = bytecode::Arg>> {
        self.0.packtab.get_by_index(index).map(|p| {
            p.iter().map(|p| match p {
                Arg::Value => bytecode::Arg::Value,
                Arg::Pack => bytecode::Arg::Pack,
                Arg::Key(id) => bytecode::Arg::Key(id.index()),
            })
        })
    }

    fn unpack_arity(&self, index: usize) -> Option<usize> {
        self.0.unpacktab.get_by_index(index).map(|s| s.len())
    }

    fn symbol_valid(&self, index: usize) -> bool {
        self.0.symtab.get_by_index(index).is_some()
    }

    fn constant_valid(&self, index: usize) -> bool {
        self.0.consttab.get_by_index(index).is_some()
    }
}

impl<'a> Emitter<'a> {
    fn upvars(&self, func: &cfg::Func) -> Vec<usize> {
        let mut upvars = Vec::new();
        let mut scope = self.graph.scope(self.graph.block(func.enter).scope);

        // Push guard frame
        if scope.is_nl_guard {
            upvars.push(0);
        }

        while let Some(id) = scope.parent {
            scope = self.graph.scope(id);
            if scope.has_upvars() {
                upvars.push(scope.caps);
            }
            // Push guard frame
            if scope.is_nl_guard {
                upvars.push(0);
            }
        }

        upvars.reverse();
        upvars
    }

    fn emit_funcs(&mut self) -> (Vec<(EmitFunc, Certificate)>, Vec<file::FuncDebug>) {
        let mut funcs = Vec::with_capacity(self.graph.func_count());
        let mut debugs = Vec::with_capacity(self.graph.func_count());

        for id in self.graph.iter_funcs() {
            let func = self.graph.func(id);
            let mut emitter = FuncEmitter::new(self, &func);
            let (bytecode, debug) = emitter.emit();
            #[cfg(feature = "debug")]
            {
                use dolang_bytecode::InstDecoder;

                let cursor = io::Cursor::new(&bytecode);
                eprintln!("Function #{} bytecode:", id.index());
                let width = ((bytecode.len() - 1).max(1).ilog2() + 1).div_ceil(4).max(2) as usize;
                for item in InstDecoder::new(cursor).with_offsets() {
                    let item = item.unwrap();
                    eprintln!("  {:0width$x} {}", item.before, item.inst)
                }
            }
            funcs.push(EmitFunc {
                bytecode: bytecode.into(),
                sig: func.sig.index(),
                locals: func.locals,
                upvars: self.upvars(&func),
            });
            debugs.push(debug);
        }

        let context = EmitContext(&*self, &funcs[..]);
        let verifier = Verifier::new(&context);
        let certs = verifier
            .compute(&funcs)
            .expect("internal compiler error: verification failed");

        (
            funcs
                .into_iter()
                .map(|f| EmitFunc {
                    bytecode: f.bytecode,
                    sig: f.sig,
                    locals: f.locals,
                    upvars: f.upvars,
                })
                .zip(certs)
                .collect(),
            debugs,
        )
    }

    fn with_content(
        &'a mut self,
        f: &mut dyn FnMut(&file::Content) -> io::Result<()>,
    ) -> io::Result<()> {
        let (mut funcs, debugs) = self.emit_funcs();
        let funcs: Vec<_> = funcs
            .iter_mut()
            .map(
                |(
                    EmitFunc {
                        sig,
                        locals,
                        upvars,
                        bytecode,
                    },
                    cert,
                )| file::FuncEntry {
                    func: file::Func {
                        sig: *sig,
                        locals: *locals,
                        upvars: mem::take(upvars),
                        bytecode,
                    },
                    cert: mem::take(cert),
                },
            )
            .collect();
        let functab = file::FuncTable { content: funcs };
        let consttab_entries: Vec<_> = self
            .consttab
            .iter()
            .map(|(_, c)| match c {
                Const::Nil => file::Const::Nil,
                Const::Int(v) => file::Const::Int(*v),
                Const::VerbatimInt(v, id) => file::Const::VerbatimInt(
                    *v,
                    file::StrId {
                        start: id.start(),
                        end: id.end(),
                    },
                ),
                Const::F64(v) => file::Const::F64(*v),
                Const::VerbatimF64(v, id) => file::Const::VerbatimF64(
                    *v,
                    file::StrId {
                        start: id.start(),
                        end: id.end(),
                    },
                ),
                Const::Bool(v) => file::Const::Bool(*v),
                Const::Str(id) => file::Const::Str(file::StrId {
                    start: id.start(),
                    end: id.end(),
                }),
                Const::Sym(id) => file::Const::Sym(id.index()),
                Const::Bin(id) => file::Const::Bin(file::BinId {
                    start: id.start(),
                    end: id.end(),
                }),
            })
            .collect();
        let bintab = file::BinTable {
            content: self.bintab.as_slice(),
        };
        let symtab = file::SymTable {
            content: self
                .symtab
                .iter()
                .map(|(sym_id, id)| file::SymEntry {
                    name: file::StrId {
                        start: id.start(),
                        end: id.end(),
                    },
                    private: self.symtab.is_fresh(sym_id),
                })
                .collect(),
        };
        let consttab = file::ConstTable {
            content: consttab_entries,
        };
        let packtab = file::PackTable {
            content: self
                .packtab
                .iter()
                .map(|(_, s)| {
                    s.iter()
                        .map(|p| match p {
                            Arg::Value => bytecode::Arg::Value,
                            Arg::Pack => bytecode::Arg::Pack,
                            Arg::Key(id) => bytecode::Arg::Key(id.index()),
                        })
                        .collect()
                })
                .collect(),
        };
        let unpacktab = file::UnpackTable {
            content: self
                .unpacktab
                .iter()
                .map(|(_, u)| file::UnpackSig {
                    required: u.required(),
                    optional: u.optional().map(|c| c.index()).collect(),
                    keys: u
                        .iter_keys()
                        .map(|key| file::UnpackKey {
                            kind: match &key.kind {
                                sig::UnpackKeyKind::Sym(sym) => {
                                    file::UnpackKeyKind::Sym(sym.index())
                                }
                                sig::UnpackKeyKind::Const(c) => {
                                    file::UnpackKeyKind::Const(c.index())
                                }
                            },
                            default: key.default.map(|c| c.index()),
                        })
                        .collect(),
                    variadic: u.variadic(),
                })
                .collect(),
        };
        let debugstrtab_inner = self.debugbintab.get_mut();
        let module_name = match self.mode {
            Mode::Module { name } => {
                let id = debugstrtab_inner.id_str(name);
                Some(file::StrId {
                    start: id.start(),
                    end: id.end(),
                })
            }
            _ => None,
        };
        let debugbintab = file::BinTable {
            content: debugstrtab_inner.as_slice(),
        };
        let funcdebugtab = file::FuncDebugTable { content: debugs };
        let content = file::Content {
            bintab,
            symtab,
            consttab,
            packtab,
            unpacktab,
            functab,
            funcdebugtab,
            debugbintab,
            module_name,
        };
        f(&content)
    }

    pub(crate) fn emit(&'a mut self, mut w: impl Write) -> io::Result<()> {
        self.with_content(&mut |content| file::serialize(content, &mut w))
    }

    fn qualified_name(&self, func: &cfg::Func) -> String {
        let mut parts = Vec::new();
        if let Some(span) = func.class_name {
            parts.push(self.file.str(span).to_string());
        }
        let mut scope_id = Some(self.graph.block(func.enter).scope);

        while let Some(id) = scope_id {
            let scope = self.graph.scope(id);
            // Function-entry scope: record the def name or lambda index.
            if let Some(func_id) = scope.func {
                if let Some(span) = self.graph.func(func_id).name {
                    parts.push(self.file.str(span).to_string());
                } else if scope.parent.is_some() {
                    // Unnamed function (lambda); use index to distinguish siblings.
                    parts.push(format!("{}", func_id.index()));
                }
                // scope.parent.is_none() → top-level <main>, omit from name.
            }
            scope_id = scope.parent;
        }

        if parts.is_empty() {
            "<main>".into()
        } else {
            parts.reverse();
            parts.join(".")
        }
    }
}
