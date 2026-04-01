use dolang_util::alias::Box;
use std::mem;

use super::cfg::{Block, BlockId, Graph, Inst, Term, TermInfo};

#[derive(Clone, Copy)]
pub(crate) struct Context<'a> {
    pub(crate) id: BlockId,
    pub(crate) block: &'a Block,
}

pub(crate) trait Analysis {
    type State: Clone + Eq;
    type Error;

    fn neutral(&self) -> Self::State;

    fn inst(
        &mut self,
        state: &mut Self::State,
        ctx: &Context,
        index: usize,
        inst: &Inst,
    ) -> Result<(), Self::Error>;

    fn term(
        &mut self,
        state: &mut Self::State,
        ctx: &Context,
        term: &Term,
    ) -> Result<(), Self::Error>;

    fn merge(
        &mut self,
        src: Self::State,
        src_ctx: &Context,
        dst: &Self::State,
        dst_ctx: &Context,
    ) -> Result<Self::State, Self::Error>;

    fn fork<'a>(
        &mut self,
        state: Self::State,
        _src: &Context,
        dsts: impl Iterator<Item = Context<'a>>,
    ) -> Result<impl Iterator<Item = Self::State>, Self::Error> {
        Ok(dsts.map(move |_| state.clone()))
    }
}

pub(crate) struct Flow<'a, A: Analysis> {
    graph: &'a Graph,
    flow: &'a mut A,
    states: Box<[A::State]>,
    dirty: Box<[bool]>,
}

#[expect(dead_code)]
impl<'a, A: Analysis + 'a> Flow<'a, A> {
    pub(crate) fn new(graph: &'a Graph, flow: &'a mut A) -> Self {
        let states = (0..graph.block_count())
            .map(|_| flow.neutral())
            .collect::<Vec<_>>();
        Self {
            flow,
            states: states.into(),
            dirty: (0..graph.block_count())
                .map(|_| false)
                .collect::<Vec<_>>()
                .into(),
            graph,
        }
    }

    pub(crate) fn step_block(
        &mut self,
        ctx: &Context,
        mut state: A::State,
    ) -> Result<A::State, A::Error> {
        for (i, inst) in ctx.block.insts.iter().enumerate() {
            self.flow.inst(&mut state, ctx, i, inst)?;
        }
        self.flow.term(&mut state, ctx, &ctx.block.term)?;
        Ok(state)
    }

    pub(crate) fn merge(
        &mut self,
        src: &Context,
        dst: &Context,
        state: A::State,
    ) -> Result<bool, A::Error> {
        let slot = &mut self.states[dst.id.index()];
        let new = self.flow.merge(state, src, slot, dst)?;
        Ok(if new != *slot {
            *slot = new;
            true
        } else {
            false
        } && !mem::replace(&mut self.dirty[dst.id.index()], true))
    }

    pub(crate) fn forward(
        &mut self,
        init: impl IntoIterator<Item = (BlockId, A::State)>,
    ) -> Result<(), A::Error> {
        let mut queue = Vec::new();
        for (id, state) in init.into_iter() {
            self.states[id.index()] = state;
            queue.push(id);
        }
        while let Some(id) = queue.pop() {
            self.dirty[id.index()] = false;
            let state = self.states[id.index()].clone();
            let block = self.graph.block(id);
            let ctx = Context { id, block: &block };
            let state = self.step_block(&Context { id, block: &block }, state)?;
            match block.term {
                Term(TermInfo::Branch(bid), _) => {
                    if self.merge(
                        &ctx,
                        &Context {
                            id: bid,
                            block: &self.graph.block(bid),
                        },
                        state,
                    )? {
                        queue.push(bid)
                    }
                }
                Term(TermInfo::If(tid, fid), _) => {
                    let tblock = self.graph.block(tid);
                    let fblock = self.graph.block(fid);
                    let dsts = [
                        Context {
                            id: tid,
                            block: &tblock,
                        },
                        Context {
                            id: fid,
                            block: &fblock,
                        },
                    ];
                    let mut iter = self.flow.fork(state, &ctx, dsts.iter().copied())?;
                    let tstate = iter.next().expect("insufficent forked states");
                    let fstate = iter.next().expect("insufficent forked states");
                    assert!(iter.next().is_none());
                    mem::drop(iter);
                    if self.merge(&ctx, &dsts[0], tstate)? {
                        queue.push(tid)
                    }
                    if self.merge(&ctx, &dsts[1], fstate)? {
                        queue.push(fid);
                    }
                }
                Term(TermInfo::Ret | TermInfo::NlBranch(..), _) => (),
            }
        }
        Ok(())
    }

    #[expect(dead_code)]
    pub(crate) fn into_states(self) -> Box<[A::State]> {
        self.states
    }
}
