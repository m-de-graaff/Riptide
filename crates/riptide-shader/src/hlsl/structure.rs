//! From a control-flow graph to structured code: blocks, loops, ifs, and
//! jumps out of them.
//!
//! HLSL has no `goto`, so a function's graph has to become nested
//! statements. This module does it by the dominator tree, the way described
//! by Norman Ramsey ("Beyond Relooper", 2022), which works for every
//! reducible graph without copying code and without a state variable:
//!
//! - A block is written where its immediate dominator's code ends, inside
//!   everything the dominator is inside.
//! - A block that several forward edges reach (a *merge*) is written after a
//!   scope that encloses its dominator's code; a jump to it leaves that
//!   scope.
//! - A loop header opens a loop around everything it dominates; a back edge
//!   continues the loop.
//! - A block with one way in is written right where the jump to it is.
//!
//! The result is a tree of [`Node`]s. In it, control never runs off the end
//! of a list of nodes: every path ends in a jump or a return. That is what
//! lets the two arms of a branch be written as `if (c) { one }` followed by
//! the other, without nesting, and it is what the writer of HLSL relies on.
//!
//! A graph that is not reducible cannot be structured this way;
//! [`structure`] then returns `None`, and the emitter falls back to a loop
//! around a `switch` on a block number.

use crate::ir::cfg::ControlFlow;
use crate::ir::{BlockId, Function, Terminator, Value};

/// One piece of structured code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Node {
    /// The instructions of a block, without its terminator.
    Code(BlockId),
    /// One arm of a two-way branch; the other arm is what follows the node
    /// in its list. That is sound because no arm runs off its end.
    If {
        /// The flag tested.
        condition: Value,
        /// Whether the arm runs when the flag does not hold.
        negate: bool,
        /// What runs then.
        then: Vec<Node>,
    },
    /// A scope that jumps to `follow` leave. The code of `follow` comes
    /// right after the node.
    Scope {
        /// The merge block written after the scope.
        follow: BlockId,
        /// What is inside.
        body: Vec<Node>,
    },
    /// A loop. A jump to `header` from inside starts the next trip. Nothing
    /// follows a loop in its list: whatever comes after the loop in the
    /// program is inside it, on a path that leaves.
    Loop {
        /// The loop's header.
        header: BlockId,
        /// What is inside, the header's own code first.
        body: Vec<Node>,
    },
    /// Leaves the scope whose follower is this block.
    Break(BlockId),
    /// Starts the next trip of the loop with this header.
    Continue(BlockId),
    /// Ends the program for the lane.
    Return,
}

/// What the builder needs to know about the graph.
struct Builder<'a> {
    function: &'a Function,
    flow: &'a ControlFlow,
    /// For each block, the blocks it immediately dominates that are merges,
    /// latest in the order first.
    merges: Vec<Vec<BlockId>>,
    /// Whether a block is the target of a back edge.
    header: Vec<bool>,
    /// Whether two or more forward edges reach a block.
    merge: Vec<bool>,
}

/// Turns a function's graph into structured code, or returns `None` if the
/// graph is not reducible.
pub(crate) fn structure(function: &Function, flow: &ControlFlow) -> Option<Vec<Node>> {
    if !flow.reducible || function.blocks.is_empty() {
        return None;
    }
    let count = function.blocks.len();
    let mut header = vec![false; count];
    for &(_, to) in &flow.back_edges {
        header[to] = true;
    }
    let mut merge = vec![false; count];
    for &block in &flow.order {
        let forward = flow.predecessors[block]
            .iter()
            .filter(|&&pred| {
                flow.place[pred].is_some() && !flow.back_edges.contains(&(pred, block))
            })
            .count();
        merge[block] = forward >= 2;
    }
    let mut merges = vec![Vec::new(); count];
    // Walking the order backward puts the latest merge first.
    for &block in flow.order.iter().rev() {
        if merge[block]
            && let Some(parent) = flow.dominator[block]
            && parent != block
        {
            merges[parent].push(block);
        }
    }
    let builder = Builder {
        function,
        flow,
        merges,
        header,
        merge,
    };
    let mut out = Vec::new();
    builder.tree(0, &mut out);
    Some(out)
}

/// How a jump from one block to another is written.
enum Way {
    /// A single node: a break or a continue.
    Jump(Node),
    /// The target's code, written in place.
    Inline(BlockId),
}

impl Builder<'_> {
    /// How the edge from `from` to `to` is written.
    fn way(&self, from: BlockId, to: BlockId) -> Way {
        if self.flow.back_edges.contains(&(from, to)) {
            Way::Jump(Node::Continue(to))
        } else if self.merge[to] {
            Way::Jump(Node::Break(to))
        } else {
            Way::Inline(to)
        }
    }

    /// Writes the block `start` and everything it dominates to `out`.
    ///
    /// A chain of blocks that follow one another is written by going round
    /// the loop here, not by recursion, so that a long program does not
    /// need a deep stack: the recursion is only as deep as the code nests.
    fn tree(&self, start: BlockId, out: &mut Vec<Node>) {
        let mut next = Some(start);
        while let Some(block) = next {
            if self.header[block] {
                let mut body = Vec::new();
                if let Some(more) = self.within(block, &self.merges[block], &mut body) {
                    self.tree(more, &mut body);
                }
                out.push(Node::Loop {
                    header: block,
                    body,
                });
                return;
            }
            next = self.within(block, &self.merges[block], out);
        }
    }

    /// Writes `block`'s code inside a scope for each of `merges`, then
    /// returns the block whose code comes next in the same list, if any.
    fn within(&self, block: BlockId, merges: &[BlockId], out: &mut Vec<Node>) -> Option<BlockId> {
        let Some((&follow, inner)) = merges.split_first() else {
            return self.code(block, out);
        };
        let mut body = Vec::new();
        if let Some(more) = self.within(block, inner, &mut body) {
            self.tree(more, &mut body);
        }
        out.push(Node::Scope { follow, body });
        Some(follow)
    }

    /// Writes a block's instructions and its terminator. Returns the block
    /// whose code comes next in the same list, if any.
    fn code(&self, block: BlockId, out: &mut Vec<Node>) -> Option<BlockId> {
        out.push(Node::Code(block));
        match self.function.blocks[block].terminator {
            Terminator::Return => {
                out.push(Node::Return);
                None
            }
            Terminator::Jump(to) => self.go(block, to, out),
            Terminator::Branch {
                if_true, if_false, ..
            } if if_true == if_false => self.go(block, if_true, out),
            Terminator::Branch {
                condition,
                if_true,
                if_false,
            } => match (self.way(block, if_true), self.way(block, if_false)) {
                (Way::Jump(jump), _) => {
                    out.push(Node::If {
                        condition,
                        negate: false,
                        then: vec![jump],
                    });
                    self.go(block, if_false, out)
                }
                (Way::Inline(_), Way::Jump(jump)) => {
                    out.push(Node::If {
                        condition,
                        negate: true,
                        then: vec![jump],
                    });
                    self.go(block, if_true, out)
                }
                (Way::Inline(first), Way::Inline(second)) => {
                    let mut then = Vec::new();
                    self.tree(first, &mut then);
                    out.push(Node::If {
                        condition,
                        negate: false,
                        then,
                    });
                    Some(second)
                }
            },
        }
    }

    /// Writes the jump from `from` to `to`, or returns `to` for its code to
    /// be written next.
    fn go(&self, from: BlockId, to: BlockId, out: &mut Vec<Node>) -> Option<BlockId> {
        match self.way(from, to) {
            Way::Jump(jump) => {
                out.push(jump);
                None
            }
            Way::Inline(to) => Some(to),
        }
    }
}
