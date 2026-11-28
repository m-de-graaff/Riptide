//! The shape of a function's control flow: what an emitter of structured
//! code (if, else, loops) needs to know.
//!
//! [`ControlFlow::of`] computes, for a function: each block's predecessors,
//! a reverse post-order, immediate dominators, the back edges, and the loops
//! with their headers and bodies. A graph is reducible when every cycle is
//! entered through one block, its header; only then do loops nest properly
//! and only then can structured code be written without copying.
//! [`make_reducible`] copies blocks ("node splitting") until the graph is
//! reducible. Compilers produce the other kind when they rotate a loop and
//! jump into its middle.

use std::collections::BTreeSet;

use super::{Block, BlockId, Function, Op, Temp, Terminator, Value, Var};

/// One loop: a header and the blocks that can get back to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loop {
    /// The block every entry to the loop goes through.
    pub header: BlockId,
    /// The blocks of the loop, header included, sorted. Blocks of loops
    /// nested inside are included.
    pub body: Vec<BlockId>,
    /// The blocks with an edge back to the header.
    pub latches: Vec<BlockId>,
}

impl Loop {
    /// Whether the loop contains the block.
    pub fn contains(&self, block: BlockId) -> bool {
        self.body.binary_search(&block).is_ok()
    }
}

/// The control-flow facts of one function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlFlow {
    /// Each block's predecessors.
    pub predecessors: Vec<Vec<BlockId>>,
    /// The blocks reachable from the entry, in reverse post-order: with the
    /// back edges set aside, every block comes after all its predecessors.
    pub order: Vec<BlockId>,
    /// Each block's place in `order`, or `None` if it cannot be reached.
    pub place: Vec<Option<usize>>,
    /// Each block's immediate dominator; the entry's is itself, and a block
    /// that cannot be reached has none.
    pub dominator: Vec<Option<BlockId>>,
    /// The edges (from, to) whose target dominates their source.
    pub back_edges: Vec<(BlockId, BlockId)>,
    /// The loops, outer before inner where they nest.
    pub loops: Vec<Loop>,
    /// Whether every cycle goes through a back edge. If not, `loops` does
    /// not describe every cycle.
    pub reducible: bool,
}

impl ControlFlow {
    /// Analyses a function.
    pub fn of(function: &Function) -> ControlFlow {
        let successors: Vec<Vec<BlockId>> = function
            .blocks
            .iter()
            .map(|block| block.terminator.successors().collect())
            .collect();
        analyse(&successors)
    }

    /// Whether block `a` dominates block `b`: every path from the entry to
    /// `b` goes through `a`.
    pub fn dominates(&self, a: BlockId, b: BlockId) -> bool {
        let mut at = b;
        loop {
            if at == a {
                return true;
            }
            match self.dominator[at] {
                Some(up) if up != at => at = up,
                _ => return false,
            }
        }
    }

    /// The innermost loop that contains the block.
    pub fn innermost_loop(&self, block: BlockId) -> Option<&Loop> {
        self.loops
            .iter()
            .filter(|candidate| candidate.contains(block))
            .min_by_key(|candidate| candidate.body.len())
    }

    /// The headers of the loops that contain the block, outermost first.
    pub fn loop_nest(&self, block: BlockId) -> Vec<BlockId> {
        let mut nest: Vec<&Loop> = self
            .loops
            .iter()
            .filter(|candidate| candidate.contains(block))
            .collect();
        nest.sort_by_key(|candidate| std::cmp::Reverse(candidate.body.len()));
        nest.into_iter().map(|candidate| candidate.header).collect()
    }
}

/// Analyses a graph given as each block's successors, with block 0 the
/// entry.
fn analyse(successors: &[Vec<BlockId>]) -> ControlFlow {
    let count = successors.len();
    let mut predecessors = vec![Vec::new(); count];
    for (from, targets) in successors.iter().enumerate() {
        for &to in targets {
            if !predecessors[to].contains(&from) {
                predecessors[to].push(from);
            }
        }
    }

    // Post-order by an explicit stack, so that deep graphs cannot overflow
    // the thread's stack.
    let mut post = Vec::with_capacity(count);
    let mut seen = vec![false; count];
    if count > 0 {
        let mut stack = vec![(0usize, 0usize)];
        seen[0] = true;
        while let Some(&mut (block, ref mut next)) = stack.last_mut() {
            if let Some(&to) = successors[block].get(*next) {
                *next += 1;
                if !seen[to] {
                    seen[to] = true;
                    stack.push((to, 0));
                }
            } else {
                post.push(block);
                stack.pop();
            }
        }
    }
    let order: Vec<BlockId> = post.iter().rev().copied().collect();
    let mut place = vec![None; count];
    for (index, &block) in order.iter().enumerate() {
        place[block] = Some(index);
    }

    // Immediate dominators by the iterative algorithm of Cooper, Harvey and
    // Kennedy.
    let mut dominator: Vec<Option<BlockId>> = vec![None; count];
    if count > 0 {
        dominator[0] = Some(0);
    }
    let intersect = |dominator: &[Option<BlockId>], mut a: BlockId, mut b: BlockId| {
        while a != b {
            while place[a] > place[b] {
                a = dominator[a].unwrap_or(0);
            }
            while place[b] > place[a] {
                b = dominator[b].unwrap_or(0);
            }
        }
        a
    };
    let mut changed = true;
    while changed {
        changed = false;
        for &block in order.iter().skip(1) {
            let mut new = None;
            for &pred in &predecessors[block] {
                if dominator[pred].is_none() {
                    continue;
                }
                new = Some(match new {
                    None => pred,
                    Some(current) => intersect(&dominator, pred, current),
                });
            }
            if new.is_some() && dominator[block] != new {
                dominator[block] = new;
                changed = true;
            }
        }
    }

    let mut flow = ControlFlow {
        predecessors,
        order,
        place,
        dominator,
        back_edges: Vec::new(),
        loops: Vec::new(),
        reducible: true,
    };
    for &from in &flow.order {
        for &to in &successors[from] {
            if flow.dominates(to, from) {
                flow.back_edges.push((from, to));
            } else if flow.place[to] <= flow.place[from] {
                // An edge that goes backward in the order to a block that
                // does not dominate its source closes a cycle with a second
                // entry.
                flow.reducible = false;
            }
        }
    }

    // The natural loop of each header: the header and every block that
    // reaches one of its latches without passing the header.
    let mut headers: Vec<BlockId> = flow.back_edges.iter().map(|&(_, to)| to).collect();
    headers.sort_unstable();
    headers.dedup();
    for header in headers {
        let latches: Vec<BlockId> = flow
            .back_edges
            .iter()
            .filter(|&&(_, to)| to == header)
            .map(|&(from, _)| from)
            .collect();
        let mut body = BTreeSet::from([header]);
        let mut work = latches.clone();
        while let Some(block) = work.pop() {
            if body.insert(block) {
                work.extend(flow.predecessors[block].iter().copied());
            }
        }
        flow.loops.push(Loop {
            header,
            body: body.into_iter().collect(),
            latches,
        });
    }
    flow.loops
        .sort_by_key(|candidate| std::cmp::Reverse(candidate.body.len()));
    flow
}

/// The strongly connected components with more than one block, or with a
/// block that goes to itself, of the graph induced on `nodes`.
fn components(successors: &[Vec<BlockId>], nodes: &BTreeSet<BlockId>) -> Vec<BTreeSet<BlockId>> {
    // Tarjan's algorithm with an explicit stack.
    let mut index = std::collections::HashMap::new();
    let mut low = std::collections::HashMap::new();
    let mut on_stack = BTreeSet::new();
    let mut stack = Vec::new();
    let mut out = Vec::new();
    let mut counter = 0usize;
    for &root in nodes {
        if index.contains_key(&root) {
            continue;
        }
        let mut work = vec![(root, 0usize)];
        index.insert(root, counter);
        low.insert(root, counter);
        counter += 1;
        stack.push(root);
        on_stack.insert(root);
        while let Some(&mut (node, ref mut next)) = work.last_mut() {
            let targets = &successors[node];
            if let Some(&to) = targets.get(*next) {
                *next += 1;
                if !nodes.contains(&to) {
                    continue;
                }
                if let std::collections::hash_map::Entry::Vacant(slot) = index.entry(to) {
                    slot.insert(counter);
                    low.insert(to, counter);
                    counter += 1;
                    stack.push(to);
                    on_stack.insert(to);
                    work.push((to, 0));
                } else if on_stack.contains(&to) {
                    let lower = low[&node].min(index[&to]);
                    low.insert(node, lower);
                }
            } else {
                work.pop();
                if let Some(&(parent, _)) = work.last() {
                    let lower = low[&parent].min(low[&node]);
                    low.insert(parent, lower);
                }
                if low[&node] == index[&node] {
                    let mut component = BTreeSet::new();
                    while let Some(member) = stack.pop() {
                        on_stack.remove(&member);
                        component.insert(member);
                        if member == node {
                            break;
                        }
                    }
                    if component.len() > 1 || successors[node].contains(&node) {
                        out.push(component);
                    }
                }
            }
        }
    }
    out
}

/// Finds a cycle with more than one entry inside `nodes` and returns its
/// blocks and its entries. `entry` is the function's entry block, which
/// counts as entered from outside.
fn second_entry(
    successors: &[Vec<BlockId>],
    predecessors: &[Vec<BlockId>],
    nodes: &BTreeSet<BlockId>,
) -> Option<(BTreeSet<BlockId>, Vec<BlockId>)> {
    for component in components(successors, nodes) {
        let entries: Vec<BlockId> = component
            .iter()
            .copied()
            .filter(|&block| {
                block == 0
                    || predecessors[block]
                        .iter()
                        .any(|pred| !component.contains(pred))
            })
            .collect();
        if entries.len() > 1 {
            return Some((component, entries));
        }
        // One entry: a proper loop. Look for trouble inside it, with its
        // header taken out.
        let mut inner = component.clone();
        if let Some(&header) = entries.first() {
            inner.remove(&header);
        } else {
            // A cycle nothing enters cannot be reached.
            continue;
        }
        if let Some(found) = second_entry(successors, predecessors, &inner) {
            return Some(found);
        }
    }
    None
}

/// How many blocks [`make_reducible`] may add before it gives up.
const SPLIT_LIMIT: usize = 256;

/// Copies blocks until the function's graph is reducible. Returns how many
/// blocks were added, or `None` if the limit was reached first, in which
/// case the function is left partly split and still not reducible.
///
/// Each step takes a cycle with several entries and one of those entries,
/// the smallest, and gives the edges that come from outside the cycle a copy
/// of that block to go to. The copy behaves the same and then joins the
/// cycle, which has one entry fewer.
pub fn make_reducible(function: &mut Function) -> Option<usize> {
    let mut added = 0;
    loop {
        let successors: Vec<Vec<BlockId>> = function
            .blocks
            .iter()
            .map(|block| block.terminator.successors().collect())
            .collect();
        let flow = analyse(&successors);
        if flow.reducible {
            return Some(added);
        }
        let reachable: BTreeSet<BlockId> = flow.order.iter().copied().collect();
        let (component, entries) = second_entry(&successors, &flow.predecessors, &reachable)?;
        if added >= SPLIT_LIMIT {
            return None;
        }
        // The entry block of the function cannot be copied away from its
        // place, so it is never the one split.
        let split = entries
            .iter()
            .copied()
            .filter(|&block| block != 0)
            .min_by_key(|&block| function.blocks[block].insts.len())?;
        let copy = function.blocks.len();
        let block = copy_block(function, split);
        function.blocks.push(block);
        for pred in flow.predecessors[split].clone() {
            if !component.contains(&pred) {
                function.blocks[pred].terminator.retarget(split, copy);
            }
        }
        added += 1;
    }
}

/// Makes a copy of a block with fresh temporaries.
fn copy_block(function: &mut Function, source: BlockId) -> Block {
    let mut block = function.blocks[source].clone();
    let mut renamed = std::collections::HashMap::new();
    let rename = |renamed: &std::collections::HashMap<Temp, Temp>, value: &mut Value| {
        if let Value::Temp(temp) = value
            && let Some(&new) = renamed.get(temp)
        {
            *temp = new;
        }
    };
    for inst in &mut block.insts {
        map_operands(&mut inst.op, |value| rename(&renamed, value));
        let results = inst.op.result_count();
        if results > 0 {
            let first = Temp(function.temps);
            function.temps += u32::from(results);
            for index in 0..u32::from(results) {
                renamed.insert(Temp(inst.result.0 + index), Temp(first.0 + index));
            }
            inst.result = first;
        }
    }
    if let Terminator::Branch { condition, .. } = &mut block.terminator {
        rename(&renamed, condition);
    }
    block
}

/// Calls `change` on every value an operation reads, so that it can be
/// replaced.
pub(crate) fn map_operands(op: &mut Op, mut change: impl FnMut(&mut Value)) {
    use super::{ImageKind, Level};
    match op {
        Op::Get(_) | Op::Input(_) | Op::LaneIndex | Op::Barrier => {}
        Op::Atomic {
            place,
            value,
            compare,
            guard,
            ..
        } => {
            match place {
                super::AtomicPlace::Memory(address) => {
                    change(&mut address.low);
                    change(&mut address.high);
                }
                super::AtomicPlace::Buffer {
                    descriptor,
                    position,
                } => {
                    descriptor.iter_mut().for_each(&mut change);
                    change(&mut position.index);
                    change(&mut position.offset);
                }
                super::AtomicPlace::Shared(offset) => change(offset),
            }
            change(value);
            change(compare);
            change(guard);
        }
        Op::LoadShared { offset, .. } => change(offset),
        Op::StoreShared {
            offset,
            values,
            guard,
            ..
        } => {
            change(offset);
            values.iter_mut().for_each(&mut change);
            change(guard);
        }
        Op::Reduce { value, active, .. } | Op::PrefixSum { value, active } => {
            change(value);
            change(active);
        }
        Op::Set(_, a)
        | Op::Unary(_, a)
        | Op::FlagNot(a)
        | Op::Discard(a)
        | Op::CountLanes(a)
        | Op::FirstLaneIndex(a)
        | Op::AnyLane(a)
        | Op::QuadAny(a) => change(a),
        Op::Binary(_, a, b) | Op::Compare(_, a, b) | Op::Flag(_, a, b) => {
            change(a);
            change(b);
        }
        Op::Ternary(_, a, b, c) => {
            change(a);
            change(b);
            change(c);
        }
        Op::FloatClass { value, classes } => {
            change(value);
            change(classes);
        }
        Op::Select {
            condition,
            if_true,
            if_false,
        } => {
            change(condition);
            change(if_true);
            change(if_false);
        }
        Op::Carry { a, b, carry_in } => {
            change(a);
            change(b);
            change(carry_in);
        }
        Op::Borrow { a, b, borrow_in } => {
            change(a);
            change(b);
            change(borrow_in);
        }
        Op::LoadMemory { address, .. } => {
            change(&mut address.low);
            change(&mut address.high);
        }
        Op::StoreMemory {
            address,
            values,
            guard,
            ..
        } => {
            change(&mut address.low);
            change(&mut address.high);
            values.iter_mut().for_each(&mut change);
            change(guard);
        }
        Op::LoadBuffer {
            descriptor,
            position,
            ..
        } => {
            descriptor.iter_mut().for_each(&mut change);
            change(&mut position.index);
            change(&mut position.offset);
        }
        Op::StoreBuffer {
            descriptor,
            position,
            values,
            guard,
            ..
        } => {
            descriptor.iter_mut().for_each(&mut change);
            change(&mut position.index);
            change(&mut position.offset);
            values.iter_mut().for_each(&mut change);
            change(guard);
        }
        Op::LoadFormat {
            descriptor,
            position,
            format,
        } => {
            descriptor.iter_mut().for_each(&mut change);
            change(&mut position.index);
            change(&mut position.offset);
            change(format);
        }
        Op::StoreFormat {
            descriptor,
            position,
            format,
            values,
            guard,
        } => {
            descriptor.iter_mut().for_each(&mut change);
            change(&mut position.index);
            change(&mut position.offset);
            change(format);
            values.iter_mut().for_each(&mut change);
            change(guard);
        }
        Op::LoadCode { offset, .. } => change(offset),
        Op::Image(image) => {
            change(&mut image.texture);
            change(&mut image.format);
            if let Some(super::Sampler::Heap(index)) = &mut image.sampler {
                change(index);
            }
            for value in [
                &mut image.offset,
                &mut image.bias,
                &mut image.reference,
                &mut image.level_clamp,
                &mut image.mip,
            ]
            .into_iter()
            .flatten()
            {
                change(value);
            }
            image.coordinates.iter_mut().for_each(&mut change);
            match &mut image.level {
                Level::Implicit | Level::Zero => {}
                Level::Explicit(level) => change(level),
                Level::Gradients(dx, dy) => {
                    dx.iter_mut().chain(dy).for_each(&mut change);
                }
            }
            if let ImageKind::Store { values, guard } = &mut image.kind {
                values.iter_mut().for_each(&mut change);
                change(guard);
            }
        }
        Op::Export { values, guard, .. } => {
            values.iter_mut().flatten().for_each(&mut change);
            change(guard);
        }
        Op::FirstLane { value, active } => {
            change(value);
            change(active);
        }
        Op::ReadLane { value, lane } => {
            change(value);
            change(lane);
        }
        Op::LanesBelow { mask, .. } | Op::BallotWord { mask, .. } => change(mask),
        Op::LaneBit { low, high } => {
            change(low);
            change(high);
        }
        Op::QuadPermute { value, active, .. } => {
            change(value);
            change(active);
        }
    }
}

/// A place for each variable in a bit set.
fn var_bit(var: Var) -> usize {
    match var {
        Var::Vector(n) => usize::from(n),
        Var::Scalar(n) => 256 + usize::from(n),
        Var::Mask(n) => 512 + usize::from(n),
        Var::Exec => 768,
        Var::Vcc => 769,
        Var::Scc => 770,
        Var::M0 => 771,
        Var::Exported => 772,
    }
}

/// A set of variables.
#[derive(Clone, PartialEq, Eq)]
struct VarSet([u64; 13]);

impl VarSet {
    fn has(&self, var: Var) -> bool {
        let bit = var_bit(var);
        self.0[bit / 64] >> (bit % 64) & 1 != 0
    }

    fn set(&mut self, var: Var, on: bool) {
        let bit = var_bit(var);
        if on {
            self.0[bit / 64] |= 1 << (bit % 64);
        } else {
            self.0[bit / 64] &= !(1 << (bit % 64));
        }
    }

    fn union(&mut self, other: &VarSet) {
        for (word, other) in self.0.iter_mut().zip(&other.0) {
            *word |= other;
        }
    }
}

/// Whether an operation does something beyond defining its results.
fn has_effect(op: &Op) -> bool {
    match op {
        Op::Set(..)
        | Op::StoreMemory { .. }
        | Op::StoreBuffer { .. }
        | Op::StoreFormat { .. }
        | Op::StoreShared { .. }
        | Op::Atomic { .. }
        | Op::Barrier
        | Op::Export { .. }
        | Op::Discard(_) => true,
        Op::Image(image) => matches!(image.kind, super::ImageKind::Store { .. }),
        _ => false,
    }
}

/// Removes what nothing reads: writes to variables that are overwritten or
/// never read again, and operations without effect whose results are not
/// used. Returns how many instructions were removed.
///
/// The lift writes every register an instruction writes, the condition
/// code among them, and most of those writes are never read. Removing them
/// here keeps the function an emitter gets to what the program does.
pub fn remove_dead(function: &mut Function) -> usize {
    let count = function.blocks.len();
    // The variables read after each block, to a fixed point, going
    // backward.
    let mut live_in = vec![VarSet([0; 13]); count];
    let transfer = |block: &Block, mut live: VarSet| -> VarSet {
        for inst in block.insts.iter().rev() {
            match inst.op {
                Op::Set(var, _) => live.set(var, false),
                Op::Get(var) => live.set(var, true),
                _ => {}
            }
        }
        live
    };
    let live_out = |function: &Function, live_in: &[VarSet], block: usize| -> VarSet {
        let mut live = VarSet([0; 13]);
        for successor in function.blocks[block].terminator.successors() {
            live.union(&live_in[successor]);
        }
        live
    };
    let mut changed = true;
    while changed {
        changed = false;
        for block in (0..count).rev() {
            let live = transfer(&function.blocks[block], live_out(function, &live_in, block));
            if live != live_in[block] {
                live_in[block] = live;
                changed = true;
            }
        }
    }

    let mut removed = 0;
    let mut used = vec![false; function.temps as usize];
    for block in 0..count {
        let mut live = live_out(function, &live_in, block);
        let terminator = function.blocks[block].terminator;
        if let Terminator::Branch {
            condition: Value::Temp(temp),
            ..
        } = terminator
        {
            used[temp.0 as usize] = true;
        }
        let insts = std::mem::take(&mut function.blocks[block].insts);
        let mut kept = Vec::with_capacity(insts.len());
        for inst in insts.into_iter().rev() {
            let keep = match inst.op {
                Op::Set(var, _) => {
                    let wanted = live.has(var);
                    live.set(var, false);
                    wanted
                }
                ref op if has_effect(op) => true,
                _ => inst.results().any(|temp| used[temp.0 as usize]),
            };
            if !keep {
                removed += 1;
                continue;
            }
            if let Op::Get(var) = inst.op {
                live.set(var, true);
            }
            inst.op.for_each_operand(|value| {
                if let Value::Temp(temp) = value {
                    used[temp.0 as usize] = true;
                }
            });
            kept.push(inst);
        }
        kept.reverse();
        function.blocks[block].insts = kept;
    }
    removed
}

/// Appends an instruction to a list and returns its first result.
#[cfg(test)]
pub(crate) fn push(insts: &mut Vec<super::Inst>, temps: &mut u32, op: Op) -> Temp {
    let result = Temp(*temps);
    *temps += u32::from(op.result_count());
    insts.push(super::Inst { result, op });
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::xbox::HardwareStage;
    use crate::gfx10::WaveSize;
    use crate::ir::{Interface, Var};

    /// A function whose blocks only branch: `edges[b]` lists where block b
    /// goes (none, one or two blocks).
    fn graph(edges: &[&[BlockId]]) -> Function {
        let mut temps = 0;
        let blocks = edges
            .iter()
            .map(|targets| {
                let mut insts = Vec::new();
                let terminator = match **targets {
                    [] => Terminator::Return,
                    [to] => Terminator::Jump(to),
                    [if_true, if_false] => {
                        let flag = push(&mut insts, &mut temps, Op::Get(Var::Scc));
                        Terminator::Branch {
                            condition: Value::Temp(flag),
                            if_true,
                            if_false,
                        }
                    }
                    _ => panic!("a block has at most two successors"),
                };
                Block {
                    offset: None,
                    insts,
                    terminator,
                }
            })
            .collect();
        Function {
            stage: HardwareStage::Compute,
            wave: WaveSize::Wave64,
            blocks,
            temps,
            interface: Interface::default(),
            code: Vec::new(),
        }
    }

    #[test]
    fn a_diamond_has_no_loops_and_its_join_is_dominated_by_its_head() {
        let function = graph(&[&[1, 2], &[3], &[3], &[]]);
        let flow = ControlFlow::of(&function);
        assert!(flow.reducible);
        assert!(flow.loops.is_empty());
        assert_eq!(flow.dominator, [Some(0), Some(0), Some(0), Some(0)]);
        assert_eq!(flow.order[0], 0);
        assert_eq!(flow.order[3], 3);
        assert!(flow.dominates(0, 3));
        assert!(!flow.dominates(1, 3));
    }

    #[test]
    fn nested_loops_are_found_with_their_headers_bodies_and_latches() {
        // 0 -> 1; 1 -> 2; 2 -> 2 or 3; 3 -> 1 or 4; 4 ends.
        let function = graph(&[&[1], &[2], &[2, 3], &[1, 4], &[]]);
        let flow = ControlFlow::of(&function);
        assert!(flow.reducible);
        assert_eq!(flow.back_edges.len(), 2);
        assert_eq!(flow.loops.len(), 2);
        assert_eq!(flow.loops[0].header, 1);
        assert_eq!(flow.loops[0].body, [1, 2, 3]);
        assert_eq!(flow.loops[0].latches, [3]);
        assert_eq!(flow.loops[1].header, 2);
        assert_eq!(flow.loops[1].body, [2]);
        assert_eq!(flow.loop_nest(2), [1, 2]);
        assert_eq!(flow.innermost_loop(3).unwrap().header, 1);
        assert!(flow.innermost_loop(4).is_none());
    }

    #[test]
    fn a_loop_entered_in_two_places_is_split_until_it_has_one_entry() {
        // 0 goes to 1 or 2, which go to each other; 2 can also leave.
        let mut function = graph(&[&[1, 2], &[2], &[1, 3], &[]]);
        assert!(!ControlFlow::of(&function).reducible);
        let added = make_reducible(&mut function).unwrap();
        assert_eq!(added, 1);
        let flow = ControlFlow::of(&function);
        assert!(flow.reducible);
        assert_eq!(flow.loops.len(), 1);
        function.verify().unwrap();
    }

    #[test]
    fn writes_nothing_reads_and_the_work_behind_them_are_removed() {
        let mut temps = 0;
        let mut insts = Vec::new();
        // s1 = s0 + 1, overwritten before anyone reads it; s1 = s0; the
        // condition code set and never read; v0 = s1, read by the export.
        let a = Value::Temp(push(&mut insts, &mut temps, Op::Get(Var::Scalar(0))));
        let sum = Value::Temp(push(
            &mut insts,
            &mut temps,
            Op::Binary(crate::ir::BinaryOp::Add, a, Value::Word(1)),
        ));
        push(&mut insts, &mut temps, Op::Set(Var::Scalar(1), sum));
        push(&mut insts, &mut temps, Op::Set(Var::Scc, Value::Flag(true)));
        push(&mut insts, &mut temps, Op::Set(Var::Scalar(1), a));
        let b = Value::Temp(push(&mut insts, &mut temps, Op::Get(Var::Scalar(1))));
        push(&mut insts, &mut temps, Op::Set(Var::Vector(0), b));
        push(&mut insts, &mut temps, Op::Set(Var::Vector(9), b));
        let mut second = Vec::new();
        let c = Value::Temp(push(&mut second, &mut temps, Op::Get(Var::Vector(0))));
        push(
            &mut second,
            &mut temps,
            Op::Export {
                target: crate::ir::ExportTarget::Position(0),
                values: [Some(c), None, None, None],
                guard: Value::Flag(true),
            },
        );
        let mut function = graph(&[&[1], &[]]);
        function.blocks[0].insts = insts;
        function.blocks[1].insts = second;
        function.temps = temps;
        let removed = remove_dead(&mut function);
        assert_eq!(removed, 4);
        function.verify().unwrap();
        let kept: Vec<&Op> = function.blocks[0]
            .insts
            .iter()
            .map(|inst| &inst.op)
            .collect();
        assert_eq!(
            kept,
            [
                &Op::Get(Var::Scalar(0)),
                &Op::Set(Var::Scalar(1), a),
                &Op::Get(Var::Scalar(1)),
                &Op::Set(Var::Vector(0), b),
            ]
        );
        assert_eq!(function.blocks[1].insts.len(), 2);
    }

    #[test]
    fn a_reducible_function_is_left_alone() {
        let mut function = graph(&[&[1], &[1, 2], &[]]);
        let before = function.clone();
        assert_eq!(make_reducible(&mut function), Some(0));
        assert_eq!(function, before);
    }
}
