//! Tests of the emitter that need no device: the structured control flow
//! against the graph it came from, the text of what is declared and bound,
//! the errors, and (where the DirectX shader compiler is installed) that
//! every family of operation compiles. What a shader computes is checked on
//! a device, in `device.rs`.

use crate::container::signature::{ComponentType, Signature, SignatureElement, SystemValue};
use crate::container::testing::{TestElement, TestProgram, dxbc, dxil_part, machine_code};
use crate::container::xbox::HardwareStage;
use crate::ir::cfg::{ControlFlow, make_reducible};
use crate::ir::{
    Address, AtomicOp, AtomicPlace, AttributeRow, BinaryOp, BlockId, BufferPosition, CompareOp,
    Dimension, ExportTarget, FlagOp, Function, ImageKind, ImageOp, Input, Interface, Level, Op,
    ReduceOp, Sampler, Terminator, TernaryOp, UnaryOp, Value, Var, VertexTerm, Weights,
};
use crate::lift::tests::asm::{self, O};

use super::structure::{Node, structure};
use super::testing::{Maker, assert_compiles, compile};
use super::{
    EmitError, Emitted, Options, Signatures, TargetKind, TranslateError, emit,
    rectangle_geometry_shader, translate,
};

/// Emits a function with no signatures and the default options.
fn emitted(function: &Function) -> Emitted {
    emit(function, Signatures::default(), &Options::default()).unwrap()
}

/// The reason a function is not emitted.
fn refused(function: &Function) -> EmitError {
    emit(function, Signatures::default(), &Options::default()).unwrap_err()
}

/// A scrambling of a number.
fn mix(value: u64) -> u64 {
    let mut z = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A function of `count` blocks with random jumps and branches and nothing
/// else, made reducible. `None` if that takes too many copies.
fn random_graph(seed: u64, count: usize) -> Option<Function> {
    let mut m = Maker::new();
    for block in 0..count {
        let hash = mix(seed << 8 | block as u64);
        let target = |salt: u64| (mix(hash ^ salt) % count as u64) as usize;
        let terminator = match hash % 7 {
            0 => Terminator::Return,
            1 | 2 => Terminator::Jump(target(1)),
            _ => {
                let flag = m.op(Op::Get(Var::Vcc));
                Terminator::Branch {
                    condition: flag,
                    if_true: target(2),
                    if_false: target(3),
                }
            }
        };
        m.end(terminator);
    }
    let mut function = m.function(HardwareStage::Compute, Interface::default());
    make_reducible(&mut function)?;
    Some(function)
}

/// How a list of nodes was left.
enum Left {
    Break(BlockId),
    Continue(BlockId),
    Return,
    /// The walk was cut short at its step limit.
    Cut,
}

/// Walks structured code, noting each block whose code runs. `decide` is
/// asked which way each branch goes.
fn walk_nodes(
    nodes: &[Node],
    function: &Function,
    decide: &mut dyn FnMut() -> bool,
    trace: &mut Vec<BlockId>,
    limit: usize,
) -> Left {
    let mut block = 0;
    for node in nodes {
        match node {
            Node::Code(code) => {
                if trace.len() == limit {
                    return Left::Cut;
                }
                trace.push(*code);
                block = *code;
            }
            Node::If { negate, then, .. } => {
                // The branch is the one the block before it ends in.
                assert!(matches!(
                    function.blocks[block].terminator,
                    Terminator::Branch { .. }
                ));
                if decide() != *negate {
                    return walk_nodes(then, function, decide, trace, limit);
                }
            }
            Node::Scope { follow, body } => {
                match walk_nodes(body, function, decide, trace, limit) {
                    Left::Break(to) if to == *follow => {}
                    other => return other,
                }
            }
            Node::Loop { header, body } => loop {
                match walk_nodes(body, function, decide, trace, limit) {
                    Left::Continue(to) if to == *header => {}
                    other => return other,
                }
            },
            Node::Break(to) => return Left::Break(*to),
            Node::Continue(to) => return Left::Continue(*to),
            Node::Return => return Left::Return,
        }
    }
    panic!("structured code ran off the end of a list");
}

/// Walks the graph itself.
fn walk_graph(function: &Function, decide: &mut dyn FnMut() -> bool, limit: usize) -> Vec<BlockId> {
    let mut trace = Vec::new();
    let mut block = 0;
    while trace.len() < limit {
        trace.push(block);
        block = match function.blocks[block].terminator {
            Terminator::Return => break,
            Terminator::Jump(to) => to,
            Terminator::Branch {
                if_true, if_false, ..
            } if if_true == if_false => if_true,
            Terminator::Branch {
                if_true, if_false, ..
            } => {
                if decide() {
                    if_true
                } else {
                    if_false
                }
            }
        };
    }
    trace
}

#[test]
fn structured_code_visits_the_blocks_the_graph_visits_whatever_the_branches_do() {
    let mut graphs = 0;
    for seed in 0..600u64 {
        let Some(function) = random_graph(seed, 2 + (seed % 13) as usize) else {
            continue;
        };
        let flow = ControlFlow::of(&function);
        let nodes = structure(&function, &flow).expect("a reducible graph is structured");
        graphs += 1;
        for run in 0..8u64 {
            let mut asked = 0u64;
            let mut decide = || {
                asked += 1;
                mix(seed << 20 | run << 12 | asked) & 1 != 0
            };
            let expected = walk_graph(&function, &mut decide, 200);
            let mut asked = 0u64;
            let mut decide = || {
                asked += 1;
                mix(seed << 20 | run << 12 | asked) & 1 != 0
            };
            let mut trace = Vec::new();
            let left = walk_nodes(&nodes, &function, &mut decide, &mut trace, 200);
            assert_eq!(trace, expected, "graph {seed}, run {run}\n{function}");
            assert!(
                matches!(left, Left::Return | Left::Cut),
                "graph {seed} left its outermost list by a jump"
            );
        }
    }
    assert!(graphs > 400, "only {graphs} graphs were reducible");
}

#[test]
fn a_graph_that_is_not_reducible_is_written_as_a_dispatch_loop_and_says_so() {
    // Two blocks that jump to each other, both entered from the first.
    let mut m = Maker::new();
    let flag = m.op(Op::Get(Var::Vcc));
    m.end(Terminator::Branch {
        condition: flag,
        if_true: 1,
        if_false: 2,
    });
    for other in [2, 1] {
        let flag = m.op(Op::Get(Var::Scc));
        m.end(Terminator::Branch {
            condition: flag,
            if_true: other,
            if_false: 3,
        });
    }
    m.end(Terminator::Return);
    let function = m.function(HardwareStage::Compute, Interface::default());
    let flow = ControlFlow::of(&function);
    assert!(!flow.reducible);
    assert!(structure(&function, &flow).is_none());
    let shader = emitted(&function);
    assert!(shader.dispatched);
    assert!(shader.source.contains("switch (at)"));
    assert!(shader.source.contains("fuel"));
    assert_compiles(&shader.source, "cs_6_6");
}

/// A compute function of one block that ends by storing `results`.
fn storing(mut m: Maker, results: Vec<Value>) -> Function {
    m.op(Op::StoreMemory {
        address: Address {
            low: Value::Word(0x1000),
            high: Value::Word(0),
        },
        width: 32,
        values: results,
        guard: Value::Flag(true),
    });
    m.end(Terminator::Return);
    m.function(
        HardwareStage::Compute,
        Interface {
            group_size: [8, 4, 2],
            ..Interface::default()
        },
    )
}

/// Three words that differ from lane to lane.
fn three_words(m: &mut Maker) -> [Value; 3] {
    [0, 1, 2].map(|axis| m.op(Op::Input(Input::ThreadId(axis))))
}

#[test]
fn a_compute_shader_declares_its_group_size_and_reads_its_inputs_from_space_two() {
    let mut m = Maker::new();
    let [a, b, _] = three_words(&mut m);
    let group = m.op(Op::Input(Input::GroupId(1)));
    let user = m.op(Op::Input(Input::UserData(9)));
    let sum = m.op(Op::Binary(BinaryOp::Add, a, b));
    let shader = emitted(&storing(m, vec![sum, group, user]));
    let source = &shader.source;
    assert!(source.contains("register(b0, space2)"), "{source}");
    assert!(source.contains("[numthreads(8, 4, 2)]"));
    assert!(source.contains("SV_GroupThreadID"));
    assert!(source.contains("SV_GroupID"));
    // User-data word 9 is the second word of the third row of four.
    assert!(source.contains("input_words[2].y"));
    assert!(source.contains(super::VERSION));
    assert_eq!((shader.rows, shader.flat_rows), (0, 0));
    assert!(!shader.dispatched);
    assert_compiles(source, "cs_6_6");
}

#[test]
fn only_the_helpers_a_program_uses_are_in_its_shader() {
    let mut m = Maker::new();
    let [a, b, _] = three_words(&mut m);
    let product = m.op(Op::Binary(BinaryOp::FloatMulLegacy, a, b));
    let source = emitted(&storing(m, vec![product])).source;
    assert!(source.contains("uint f_mul_legacy("));
    assert!(source.contains("void mem_store("));
    // The store needs the page table; nothing needs a load or a minimum.
    assert!(source.contains("page_table : register(t0, space0)"));
    assert!(!source.contains("f_min("));
    assert!(!source.contains("mem_load32("));
    assert!(!source.contains("fmt_info"));
}

const ALL_UNARY: [UnaryOp; 26] = [
    UnaryOp::Not,
    UnaryOp::CountBits,
    UnaryOp::FindLowBit,
    UnaryOp::LeadingZeros,
    UnaryOp::LeadingSignBits,
    UnaryOp::ReverseBits,
    UnaryOp::Saturate,
    UnaryOp::Reciprocal,
    UnaryOp::ReciprocalSqrt,
    UnaryOp::Sqrt,
    UnaryOp::Exp2,
    UnaryOp::Log2,
    UnaryOp::SinTurns,
    UnaryOp::CosTurns,
    UnaryOp::Floor,
    UnaryOp::Ceil,
    UnaryOp::Trunc,
    UnaryOp::RoundEven,
    UnaryOp::Fract,
    UnaryOp::FloatFromSigned,
    UnaryOp::FloatFromUnsigned,
    UnaryOp::SignedFromFloat,
    UnaryOp::UnsignedFromFloat,
    UnaryOp::HalfFromFloat,
    UnaryOp::HalfFromFloatTowardZero,
    UnaryOp::FloatFromHalf,
];

const ALL_BINARY: [BinaryOp; 22] = [
    BinaryOp::Add,
    BinaryOp::Sub,
    BinaryOp::Mul,
    BinaryOp::MulHighUnsigned,
    BinaryOp::MulHighSigned,
    BinaryOp::And,
    BinaryOp::Or,
    BinaryOp::Xor,
    BinaryOp::ShiftLeft,
    BinaryOp::ShiftRight,
    BinaryOp::ShiftRightSigned,
    BinaryOp::MinUnsigned,
    BinaryOp::MaxUnsigned,
    BinaryOp::MinSigned,
    BinaryOp::MaxSigned,
    BinaryOp::FloatAdd,
    BinaryOp::FloatSub,
    BinaryOp::FloatMul,
    BinaryOp::FloatMulLegacy,
    BinaryOp::FloatMin,
    BinaryOp::FloatMax,
    BinaryOp::FloatScale,
];

const ALL_TERNARY: [TernaryOp; 8] = [
    TernaryOp::FloatFused,
    TernaryOp::FloatMulAdd,
    TernaryOp::FloatMedian,
    TernaryOp::PermuteBytes,
    TernaryOp::CubeFace,
    TernaryOp::CubeS,
    TernaryOp::CubeT,
    TernaryOp::CubeMajor,
];

const ALL_COMPARE: [CompareOp; 24] = [
    CompareOp::Equal,
    CompareOp::NotEqual,
    CompareOp::UnsignedLess,
    CompareOp::UnsignedLessEqual,
    CompareOp::UnsignedGreater,
    CompareOp::UnsignedGreaterEqual,
    CompareOp::SignedLess,
    CompareOp::SignedLessEqual,
    CompareOp::SignedGreater,
    CompareOp::SignedGreaterEqual,
    CompareOp::FloatEqual,
    CompareOp::FloatLess,
    CompareOp::FloatLessEqual,
    CompareOp::FloatGreater,
    CompareOp::FloatGreaterEqual,
    CompareOp::FloatLessOrGreater,
    CompareOp::FloatOrdered,
    CompareOp::FloatUnordered,
    CompareOp::FloatNotEqual,
    CompareOp::FloatNotLess,
    CompareOp::FloatNotLessEqual,
    CompareOp::FloatNotGreater,
    CompareOp::FloatNotGreaterEqual,
    CompareOp::FloatNotLessOrGreater,
];

#[test]
fn every_arithmetic_operation_compiles() {
    let mut m = Maker::new();
    let [a, b, c] = three_words(&mut m);
    let mut results = Vec::new();
    for op in ALL_UNARY {
        results.push(m.op(Op::Unary(op, a)));
    }
    for op in ALL_BINARY {
        results.push(m.op(Op::Binary(op, a, b)));
        results.push(m.op(Op::Binary(op, a, Value::Word(35))));
    }
    for op in ALL_TERNARY {
        results.push(m.op(Op::Ternary(op, a, b, c)));
    }
    let mut flags = Vec::new();
    for op in ALL_COMPARE {
        flags.push(m.op(Op::Compare(op, a, b)));
    }
    flags.push(m.op(Op::FloatClass {
        value: a,
        classes: b,
    }));
    let carry = m.op(Op::Carry {
        a,
        b,
        carry_in: flags[0],
    });
    let borrow = m.op(Op::Borrow {
        a,
        b,
        borrow_in: carry,
    });
    for op in [FlagOp::And, FlagOp::Or, FlagOp::Xor] {
        flags.push(m.op(Op::Flag(op, carry, borrow)));
    }
    flags.push(m.op(Op::FlagNot(carry)));
    flags.push(m.op(Op::Select {
        condition: carry,
        if_true: borrow,
        if_false: Value::Flag(true),
    }));
    for flag in flags {
        results.push(m.op(Op::Select {
            condition: flag,
            if_true: a,
            if_false: Value::Word(7),
        }));
    }
    let shader = emitted(&storing(m, results));
    assert_compiles(&shader.source, "cs_6_6");
}

/// A buffer descriptor of three varying words and a constant.
fn descriptor(m: &mut Maker) -> [Value; 4] {
    let [a, b, c] = three_words(m);
    [a, b, c, Value::Word(1 << 28)]
}

#[test]
fn every_memory_and_buffer_operation_compiles() {
    let mut m = Maker::new();
    let descriptor = descriptor(&mut m);
    let [a, b, c] = three_words(&mut m);
    let position = BufferPosition {
        index: a,
        offset: b,
    };
    let mut results = Vec::new();
    for width in [8, 16, 32] {
        results.extend(m.all(Op::LoadMemory {
            address: Address { low: a, high: b },
            width,
            words: 3,
        }));
        results.extend(m.all(Op::LoadBuffer {
            descriptor,
            position,
            width,
            words: 2,
        }));
        m.op(Op::StoreMemory {
            address: Address { low: c, high: b },
            width,
            values: vec![a, b],
            guard: Value::Flag(true),
        });
        let guard = m.op(Op::Compare(CompareOp::Equal, a, b));
        m.op(Op::StoreBuffer {
            descriptor,
            position,
            width,
            values: vec![a, c],
            guard,
        });
    }
    // A word at an address rounded down to a multiple of four is read in
    // one piece.
    let rounded = m.op(Op::Binary(BinaryOp::And, a, Value::Word(!3)));
    results.extend(m.all(Op::LoadMemory {
        address: Address {
            low: rounded,
            high: Value::Word(0),
        },
        width: 32,
        words: 2,
    }));
    results.extend(m.all(Op::LoadFormat {
        descriptor,
        position,
        format: Value::Word(56),
    }));
    results.extend(m.all(Op::LoadFormat {
        descriptor,
        position,
        format: c,
    }));
    m.op(Op::StoreFormat {
        descriptor,
        position,
        format: Value::Word(29),
        values: [a, b, c, a],
        guard: Value::Flag(true),
    });
    let mut function = storing(m, results);
    // A table of constants after the code, read at a constant place and at
    // a computed one.
    function.code = vec![0x1111_1111, 0x2222_2222, 0x3333_3333];
    let shader = emitted(&function);
    assert!(shader.source.contains("mem_load32_aligned(t"));
    assert!(shader.source.contains("mem_load32_aligned_at(t"));
    assert_compiles(&shader.source, "cs_6_6");
}

#[test]
fn a_table_after_the_code_is_read_as_constants_or_through_a_table() {
    let build = |offset: fn(&mut Maker) -> Value| {
        let mut m = Maker::new();
        let offset = offset(&mut m);
        let words = m.all(Op::LoadCode { offset, words: 2 });
        let mut function = storing(m, words);
        function.code = vec![0x1111_1111, 0x2222_2222, 0x3333_3333];
        emitted(&function).source
    };
    // At a constant place the words themselves are written, and one past
    // the end reads zero.
    let constant = build(|_| Value::Word(8));
    assert!(constant.contains("0x33333333u"));
    assert!(!constant.contains("code_words"));
    let computed = build(|m| m.op(Op::Input(Input::ThreadId(0))));
    assert!(
        computed.contains(
            "static const uint code_words[3] = { 0x11111111u, 0x22222222u, 0x33333333u };"
        )
    );
    assert_compiles(&computed, "cs_6_6");
}

#[test]
fn a_typed_buffer_access_in_a_format_that_is_not_known_or_not_written_is_refused() {
    for (format, store) in [(0, false), (0, true), (36, true)] {
        let mut m = Maker::new();
        let descriptor = descriptor(&mut m);
        let position = BufferPosition {
            index: Value::Word(0),
            offset: Value::Word(0),
        };
        let op = if store {
            Op::StoreFormat {
                descriptor,
                position,
                format: Value::Word(format),
                values: [Value::Word(0); 4],
                guard: Value::Flag(true),
            }
        } else {
            Op::LoadFormat {
                descriptor,
                position,
                format: Value::Word(format),
            }
        };
        m.all(op);
        let error = refused(&storing(m, vec![]));
        assert_eq!(error.op, if store { "StoreFormat" } else { "LoadFormat" });
    }
}

/// An image operation of a kind on a dimension, with the operands the kind
/// needs taken from `words`.
fn image(kind: ImageKind, dimension: Dimension, words: [Value; 3]) -> ImageOp {
    let sampled = matches!(
        kind,
        ImageKind::Sample | ImageKind::Gather { .. } | ImageKind::QueryLevel
    );
    let coordinates = match kind {
        ImageKind::Info => vec![words[0]],
        _ => (0..dimension.coordinates())
            .map(|index| words[index % 3])
            .collect(),
    };
    ImageOp {
        kind,
        dimension,
        texture: words[0],
        format: words[1],
        sampler: sampled.then_some(Sampler::Heap(words[2])),
        compare: false,
        offset: None,
        bias: None,
        reference: None,
        coordinates,
        level: if sampled {
            Level::Implicit
        } else {
            Level::Zero
        },
        level_clamp: None,
        mip: None,
        unnormalized: false,
    }
}

/// The dimensions that are sampled.
const SAMPLED: [Dimension; 6] = [
    Dimension::One,
    Dimension::Two,
    Dimension::Three,
    Dimension::Cube,
    Dimension::OneArray,
    Dimension::TwoArray,
];

const ALL_DIMENSIONS: [Dimension; 8] = [
    Dimension::One,
    Dimension::Two,
    Dimension::Three,
    Dimension::Cube,
    Dimension::OneArray,
    Dimension::TwoArray,
    Dimension::TwoMultisample,
    Dimension::TwoMultisampleArray,
];

/// Every image operation the emitter writes, on every dimension it writes
/// it for.
fn all_images(words: [Value; 3]) -> Vec<ImageOp> {
    let [a, b, c] = words;
    let mut all = Vec::new();
    for dimension in SAMPLED {
        let sample = image(ImageKind::Sample, dimension, words);
        let gradients = |count: usize| {
            Level::Gradients(
                vec![a, b, c][..count].to_vec(),
                vec![c, b, a][..count].to_vec(),
            )
        };
        all.push(sample.clone());
        all.push(ImageOp {
            bias: Some(a),
            level_clamp: Some(b),
            ..sample.clone()
        });
        all.push(ImageOp {
            level: Level::Zero,
            offset: Some(Value::Word(0x0001_FF02)),
            ..sample.clone()
        });
        all.push(ImageOp {
            level: Level::Explicit(c),
            offset: Some(b),
            unnormalized: true,
            ..sample.clone()
        });
        all.push(ImageOp {
            level: gradients(dimension.gradient_components()),
            level_clamp: Some(c),
            ..sample.clone()
        });
        // The host compares in everything but a volume.
        if dimension != Dimension::Three {
            all.push(ImageOp {
                compare: true,
                reference: Some(c),
                ..sample.clone()
            });
            all.push(ImageOp {
                compare: true,
                reference: Some(c),
                level: Level::Zero,
                offset: Some(Value::Word(0x0000_0101)),
                ..sample.clone()
            });
        }
        all.push(image(ImageKind::QueryLevel, dimension, words));
    }
    for dimension in [Dimension::Two, Dimension::TwoArray, Dimension::Cube] {
        let gather = ImageOp {
            level: Level::Zero,
            ..image(ImageKind::Gather { channel: 2 }, dimension, words)
        };
        all.push(gather.clone());
        all.push(ImageOp {
            offset: Some(a),
            ..gather.clone()
        });
        all.push(ImageOp {
            compare: true,
            reference: Some(b),
            ..gather.clone()
        });
        if dimension != Dimension::Cube {
            all.push(ImageOp {
                level: Level::Explicit(c),
                offset: Some(a),
                ..gather
            });
        }
    }
    for dimension in ALL_DIMENSIONS {
        let load = image(ImageKind::Load, dimension, words);
        all.push(load.clone());
        all.push(ImageOp {
            mip: Some(c),
            offset: Some(b),
            ..load
        });
        all.push(image(ImageKind::Info, dimension, words));
    }
    for dimension in SAMPLED {
        all.push(image(
            ImageKind::Store {
                values: [a, b, c, a],
                guard: Value::Flag(true),
            },
            dimension,
            words,
        ));
    }
    all
}

#[test]
fn every_image_operation_compiles_in_a_pixel_shader() {
    let mut m = Maker::new();
    let words = [0, 1, 2].map(|channel| {
        m.op(Op::Input(Input::Interpolated {
            attribute: 0,
            channel,
            weights: Weights::PerspectiveCenter,
        }))
    });
    let mut sum = Value::Word(0);
    for op in all_images(words) {
        for result in m.all(Op::Image(Box::new(op))) {
            sum = m.op(Op::Binary(BinaryOp::Xor, sum, result));
        }
    }
    m.op(Op::Export {
        target: ExportTarget::Color(0),
        values: [Some(sum); 4],
        guard: Value::Flag(true),
    });
    m.end(Terminator::Return);
    let function = m.function(HardwareStage::Pixel, Interface::default());
    let shader = emitted(&function);
    // The comparing form of a sampler is 1,024 places on, and a record
    // that names no texture reads the view of nothing for the kind.
    assert!(
        shader
            .source
            .contains("SamplerDescriptorHeap[min(smp, 1023u) + 1024u]")
    );
    assert!(shader.source.contains("place = 33u"));
    assert!(shader.source.contains("t.Sample(s, uv)"));
    assert_compiles(&shader.source, "ps_6_6");
}

#[test]
fn every_image_operation_compiles_in_a_compute_shader_where_there_are_no_derivatives() {
    let mut m = Maker::new();
    let words = three_words(&mut m);
    let mut results = Vec::new();
    for op in all_images(words) {
        results.extend(m.all(Op::Image(Box::new(op))));
    }
    results.truncate(8);
    let shader = emitted(&storing(m, results));
    // No sample at a level from derivatives is left.
    assert!(!shader.source.contains("t.Sample(s"));
    assert!(!shader.source.contains("SampleBias"));
    assert!(!shader.source.contains("CalculateLevelOfDetail"));
    assert_compiles(&shader.source, "cs_6_6");
}

#[test]
fn image_operations_the_host_has_no_form_for_are_refused_by_name() {
    let refuse = |change: fn(&mut ImageOp), kind: ImageKind, dimension: Dimension| {
        let mut m = Maker::new();
        let words = three_words(&mut m);
        let mut op = image(kind, dimension, words);
        change(&mut op);
        m.all(Op::Image(Box::new(op)));
        refused(&storing(m, vec![]))
    };
    let gather = ImageKind::Gather { channel: 0 };
    for error in [
        // A gather at a level the derivatives give.
        refuse(|_| {}, gather.clone(), Dimension::Two),
        // A gather of a volume.
        refuse(|op| op.level = Level::Zero, gather, Dimension::Three),
        // A comparing sample of a volume.
        refuse(
            |op| {
                op.compare = true;
                op.reference = Some(Value::Word(0));
            },
            ImageKind::Sample,
            Dimension::Three,
        ),
        // A sample of a multisampled image, and one without a sampler.
        refuse(|_| {}, ImageKind::Sample, Dimension::TwoMultisample),
        refuse(|op| op.sampler = None, ImageKind::Sample, Dimension::Two),
        refuse(
            |_| {},
            ImageKind::Store {
                values: [Value::Word(0); 4],
                guard: Value::Flag(true),
            },
            Dimension::TwoMultisample,
        ),
    ] {
        assert_eq!(error.op, "Image", "{error}");
    }
}

#[test]
fn samplers_the_root_signature_fixes_are_listed_once_each_in_the_order_of_their_input_words() {
    let first = [0x400, 0x00C0_0000, 0x18F0_0000, 0];
    let second = [0x092, 0x00FF_F000, 0, 0];
    let mut m = Maker::new();
    let words = three_words(&mut m);
    let mut results = Vec::new();
    for sampler in [first, second, first] {
        let op = ImageOp {
            sampler: Some(Sampler::Immediate(sampler)),
            level: Level::Zero,
            ..image(ImageKind::Sample, Dimension::Two, words)
        };
        results.extend(m.all(Op::Image(Box::new(op))));
    }
    results.truncate(4);
    let shader = emitted(&storing(m, results));
    assert_eq!(shader.samplers, [first, second]);
    // Words 67 and 68 carry their places.
    assert!(shader.source.contains("input_words[16].w"));
    assert!(shader.source.contains("input_words[17].x"));
    assert!(!shader.source.contains("input_words[17].y"));
    // The three samples are of one shape and share one function.
    assert!(shader.source.contains("uint4 image_0("));
    assert!(!shader.source.contains("image_1("));

    // A twelfth has no input word.
    let mut m = Maker::new();
    let words = three_words(&mut m);
    for index in 0..12 {
        let op = ImageOp {
            sampler: Some(Sampler::Immediate([index, 0, 0, 0])),
            level: Level::Zero,
            ..image(ImageKind::Sample, Dimension::Two, words)
        };
        m.all(Op::Image(Box::new(op)));
    }
    assert_eq!(refused(&storing(m, vec![])).op, "Image");
}

/// A pixel function that exports the given words to colour target 0.
fn exporting(mut m: Maker, values: [Value; 4]) -> Function {
    m.op(Op::Export {
        target: ExportTarget::Color(0),
        values: values.map(Some),
        guard: Value::Flag(true),
    });
    m.end(Terminator::Return);
    m.function(HardwareStage::Pixel, Interface::default())
}

#[test]
fn a_pixel_shader_declares_each_row_by_how_the_program_reads_it() {
    let mut m = Maker::new();
    let interpolated = |attribute, weights| Input::Interpolated {
        attribute,
        channel: 1,
        weights,
    };
    let term = |attribute, term| Input::Attribute {
        attribute,
        channel: 2,
        term,
    };
    // Row 0 at the centroid; row 1 not read; row 2 flat; row 3 by its
    // per-vertex values; row 4 both ways; row 5 at the centre and at the
    // sample.
    let values = [
        m.op(Op::Input(interpolated(0, Weights::PerspectiveCentroid))),
        m.op(Op::Input(term(2, VertexTerm::P0))),
        m.op(Op::Input(term(3, VertexTerm::P10))),
        m.op(Op::Input(interpolated(4, Weights::LinearCenter))),
    ];
    let extra = [
        m.op(Op::Input(term(3, VertexTerm::P0))),
        m.op(Op::Input(term(4, VertexTerm::P20))),
        m.op(Op::Input(interpolated(5, Weights::PerspectiveCenter))),
        m.op(Op::Input(interpolated(5, Weights::PerspectiveSample))),
        m.op(Op::Input(Input::Weight {
            weights: Weights::PerspectiveCenter,
            second: true,
        })),
    ];
    let mut sum = values[0];
    for value in extra {
        sum = m.op(Op::Binary(BinaryOp::Xor, sum, value));
    }
    let shader = emitted(&exporting(m, [sum, values[1], values[2], values[3]]));
    let source = &shader.source;
    assert_eq!(shader.rows, 6);
    // Only row 2 carries bits that must stay as they are: row 3 and row 4
    // are floats the program weighs itself.
    assert_eq!(shader.flat_rows, 0b100);
    for line in [
        "    centroid float4 row0 : ROW0;",
        "    float4 row1 : ROW1;",
        "    nointerpolation float4 row2 : ROW2;",
        "    nointerpolation float4 row3 : ROW3;",
        "    nointerpolation float4 row4 : ROW4;",
        "    float4 row5 : ROW5;",
        "    float3 weights : SV_Barycentrics0;",
        "    noperspective float3 weights_flat : SV_Barycentrics1;",
        "    in_row2_v0 = input.row2;",
        "    in_row3_v0 = GetAttributeAtVertex(input.row3, 0);",
        "    in_row3_v1 = GetAttributeAtVertex(input.row3, 1);",
        "    in_row5_sample = EvaluateAttributeAtSample(input.row5, input.sample_index);",
        "    in_row4_flat_center = in_weights_flat_center.x * in_row4_v0 + in_weights_flat_center.y * in_row4_v1 + in_weights_flat_center.z * in_row4_v2;",
    ] {
        assert!(source.contains(line), "missing {line:?} in\n{source}");
    }
    // The flat row needs only its first vertex.
    assert!(!source.contains("in_row2_v1"));
    assert!(source.contains("register(b0, space1)"));
    assert_compiles(source, "ps_6_6");
}

#[test]
fn a_pixel_shader_reads_what_the_stage_hands_it_and_writes_each_target_in_its_own_type() {
    let mut m = Maker::new();
    let mut sum = Value::Word(0);
    for input in [
        Input::Position(0),
        Input::Position(3),
        Input::FrontFace,
        Input::Ancillary,
        Input::SampleCoverage,
        Input::FixedPosition,
        Input::UserData(8),
    ] {
        let value = m.op(Op::Input(input));
        sum = m.op(Op::Binary(BinaryOp::Add, sum, value));
    }
    let live = m.op(Op::Compare(CompareOp::NotEqual, sum, Value::Word(0)));
    let dead = m.op(Op::FlagNot(live));
    for target in 0..3 {
        m.op(Op::Export {
            target: ExportTarget::Color(target),
            values: [Some(sum), Some(sum), None, Some(sum)],
            guard: live,
        });
    }
    m.op(Op::Export {
        target: ExportTarget::Depth,
        values: [Some(sum), Some(sum), Some(sum), None],
        guard: Value::Flag(true),
    });
    m.op(Op::Discard(dead));
    m.end(Terminator::Return);
    let function = m.function(HardwareStage::Pixel, Interface::default());
    let mut options = Options::default();
    options.targets[1] = TargetKind::Unsigned;
    options.targets[2] = TargetKind::Signed;
    let source = emit(&function, Signatures::default(), &options)
        .unwrap()
        .source;
    for line in [
        "    float4 position : SV_Position;",
        "    bool front : SV_IsFrontFace;",
        "    uint coverage : SV_Coverage;",
        "    uint sample_index : SV_SampleIndex;",
        "    float4 target0 : SV_Target0;",
        "    uint4 target1 : SV_Target1;",
        "    int4 target2 : SV_Target2;",
        "    float depth : SV_Depth;",
        "    uint stencil : SV_StencilRef;",
        "discard;",
        // The console hands over one over w.
        "f_rcp(asuint(in_position.w))",
    ] {
        assert!(source.contains(line), "missing {line:?} in\n{source}");
    }
    assert_compiles(&source, "ps_6_6");
}

/// One element of a signature.
fn element(name: &str, index: u32, ty: ComponentType, register: u32, mask: u8) -> SignatureElement {
    SignatureElement {
        stream: 0,
        semantic_name: name.to_string(),
        semantic_index: index,
        system_value: SystemValue(0),
        component_type: ty,
        register,
        mask,
        read_write_mask: mask,
        min_precision: 0,
    }
}

#[test]
fn a_vertex_shader_declares_its_inputs_by_the_signature_and_its_rows_by_what_it_exports() {
    let inputs = Signature {
        elements: vec![
            element("POSITION", 0, ComponentType::Float32, 0, 0b0111),
            element("BLENDINDICES", 1, ComponentType::UInt32, 1, 0b0011),
            element("TEXCOORD", 3, ComponentType::SInt32, 1, 0b1100),
        ],
    };
    let outputs = Signature {
        elements: vec![
            element("SV_Position", 0, ComponentType::Float32, 0, 0b1111),
            element("SV_ClipDistance", 0, ComponentType::Float32, 1, 0b0111),
            element("SV_CullDistance", 0, ComponentType::Float32, 1, 0b1000),
        ],
    };
    let mut m = Maker::new();
    let attribute = |row, channel| Input::VertexAttribute { row, channel };
    let values = [
        m.op(Op::Input(attribute(0, 0))),
        // Past what the signature declares: the input assembler's to fill.
        m.op(Op::Input(attribute(0, 3))),
        m.op(Op::Input(attribute(1, 1))),
        // The second element of row 1 starts at its third channel.
        m.op(Op::Input(attribute(1, 3))),
    ];
    // A row no element is in reads 0, 0, 0, 1.
    let nothing = m.op(Op::Input(attribute(7, 3)));
    let vertex = m.op(Op::Input(Input::VertexId));
    let instance = m.op(Op::Input(Input::InstanceId));
    m.op(Op::Export {
        target: ExportTarget::Position(0),
        values: values.map(Some),
        guard: Value::Flag(true),
    });
    m.op(Op::Export {
        target: ExportTarget::Position(1),
        values: [Some(vertex), Some(instance), Some(nothing), Some(vertex)],
        guard: Value::Flag(true),
    });
    m.op(Op::Export {
        target: ExportTarget::Parameter(2),
        values: [Some(vertex), None, None, Some(nothing)],
        guard: Value::Flag(true),
    });
    m.end(Terminator::Return);
    let function = m.function(HardwareStage::Vertex, Interface::default());
    let shader = emit(
        &function,
        Signatures {
            inputs: Some(&inputs),
            outputs: Some(&outputs),
        },
        &Options::default(),
    )
    .unwrap();
    let source = &shader.source;
    assert_eq!((shader.rows, shader.flat_rows), (3, 0));
    for line in [
        "    float4 a0 : POSITION0;",
        "    uint4 a1 : BLENDINDICES1;",
        "    int4 a2 : TEXCOORD3;",
        "asuint(in_a0.x)",
        "asuint(in_a0.w)",
        "in_a1.y",
        "(uint)in_a2.y",
        // The console counts from the draw's first vertex and instance.
        "    in_vertex = input.vertex + input_words[19].w;",
        "    in_instance = input.instance + input_words[19].z;",
        "    float4 row0 : ROW0;",
        "    float4 row2 : ROW2;",
        "    float3 clip0 : SV_ClipDistance0;",
        "    float cull0 : SV_CullDistance0;",
        "out_clip0.z = asfloat(",
        "= 0x3f800000u;",
        "out_cull0 = asfloat(",
        "register(b0, space0)",
    ] {
        assert!(source.contains(line), "missing {line:?} in\n{source}");
    }
    // The position comes after the rows.
    assert!(source.find("ROW2").unwrap() < source.find("SV_Position").unwrap());
    assert_compiles(source, "vs_6_6");
}

/// A function of one cross-lane operation whose result is exported or
/// stored.
fn cross_lane(stage: HardwareStage, op: fn([Value; 3], Value) -> Op) -> Function {
    let mut m = Maker::new();
    let words = match stage {
        HardwareStage::Pixel => [0, 1, 2].map(|channel| {
            m.op(Op::Input(Input::Interpolated {
                attribute: 0,
                channel,
                weights: Weights::PerspectiveCenter,
            }))
        }),
        _ => three_words(&mut m),
    };
    let flag = m.op(Op::Compare(CompareOp::UnsignedLess, words[0], words[1]));
    let op = op(words, flag);
    let is_flag = op.result_type() == Some(crate::ir::Ty::Flag);
    let mut result = m.op(op);
    if is_flag {
        result = m.op(Op::Select {
            condition: result,
            if_true: Value::Word(1),
            if_false: Value::Word(0),
        });
    }
    match stage {
        HardwareStage::Pixel => exporting(m, [result; 4]),
        _ => storing(m, vec![result]),
    }
}

/// The cross-lane operations that every stage has a form for.
const WAVE_OPS: [fn([Value; 3], Value) -> Op; 8] = [
    |[a, _, _], flag| Op::FirstLane {
        value: a,
        active: flag,
    },
    |[a, b, _], _| Op::ReadLane { value: a, lane: b },
    |_, flag| Op::CountLanes(flag),
    |_, flag| Op::FirstLaneIndex(flag),
    |_, flag| Op::AnyLane(flag),
    |_, flag| Op::LanesBelow {
        mask: flag,
        half: Some(true),
    },
    |_, flag| Op::BallotWord {
        mask: flag,
        high: false,
    },
    |[a, b, _], _| Op::LaneBit { low: a, high: b },
];

#[test]
fn cross_lane_operations_become_wave_intrinsics_and_a_helper_lane_answers_for_itself() {
    for (index, op) in WAVE_OPS.into_iter().enumerate() {
        let compute = emitted(&cross_lane(HardwareStage::Compute, op)).source;
        assert!(compute.contains("Wave"), "operation {index}\n{compute}");
        assert!(!compute.contains("IsHelperLane"));
        assert_compiles(&compute, "cs_6_6");
        let pixel = emitted(&cross_lane(HardwareStage::Pixel, op)).source;
        // The lane's own bit of a number looks at no other lane.
        assert_eq!(pixel.contains("IsHelperLane()"), index != 7, "{pixel}");
        assert_compiles(&pixel, "ps_6_6");
    }
}

#[test]
fn quad_operations_are_a_pixel_shaders_and_refused_elsewhere() {
    let permute: fn([Value; 3], Value) -> Op = |[a, _, _], flag| Op::QuadPermute {
        value: a,
        lanes: [1, 0, 3, 2],
        active: flag,
    };
    let any: fn([Value; 3], Value) -> Op = |_, flag| Op::QuadAny(flag);
    let source = emitted(&cross_lane(HardwareStage::Pixel, permute)).source;
    // Each place of the quad reads the lane beside it.
    assert!(
        source.contains("uint4(1u, 1u, 1u, 1u)[in_quad]"),
        "{source}"
    );
    assert!(source.contains("QuadReadAcrossX"));
    assert_compiles(&source, "ps_6_6");
    let source = emitted(&cross_lane(HardwareStage::Pixel, any)).source;
    assert!(source.contains("quad_any("));
    assert_compiles(&source, "ps_6_6");
    assert_eq!(
        refused(&cross_lane(HardwareStage::Compute, permute)).op,
        "QuadPermute"
    );
    assert_eq!(
        refused(&cross_lane(HardwareStage::Compute, any)).op,
        "QuadAny"
    );
}

#[test]
fn operations_that_need_the_consoles_lane_numbers_are_refused_by_name() {
    let read: fn([Value; 3], Value) -> Op = |[a, _, _], _| Op::ReadLane {
        value: a,
        lane: Value::Word(5),
    };
    let bit: fn([Value; 3], Value) -> Op = |_, _| Op::LaneBit {
        low: Value::Word(0x5555_5555),
        high: Value::Word(0x5555_5555),
    };
    let error = refused(&cross_lane(HardwareStage::Compute, read));
    assert_eq!(error.op, "ReadLane");
    assert_eq!(
        refused(&cross_lane(HardwareStage::Compute, bit)).op,
        "LaneBit"
    );
    // In a pixel program a pattern that repeats every four lanes is a fact
    // about the lane's place in its quad, which the host knows; any other
    // pattern is not.
    let source = emitted(&cross_lane(HardwareStage::Pixel, bit)).source;
    assert!(source.contains("((5u >> in_quad) & 1u) != 0u"), "{source}");
    assert_compiles(&source, "ps_6_6");
    let uneven: fn([Value; 3], Value) -> Op = |_, _| Op::LaneBit {
        low: Value::Word(0x5555_5555),
        high: Value::Word(0x5555_5551),
    };
    assert_eq!(
        refused(&cross_lane(HardwareStage::Pixel, uneven)).op,
        "LaneBit"
    );
    // The message fits a log line and names the operation.
    let text = TranslateError::Emit(error).to_string();
    assert!(
        text.starts_with("emit: ReadLane has no HLSL form: "),
        "{text}"
    );
    assert!(!text.contains('\n'));
}

#[test]
fn inputs_and_exports_of_another_stage_are_refused() {
    let mut m = Maker::new();
    let value = m.op(Op::Input(Input::VertexId));
    assert_eq!(refused(&exporting(m, [value; 4])).op, "Input");
    let mut m = Maker::new();
    m.op(Op::Export {
        target: ExportTarget::Color(0),
        values: [None; 4],
        guard: Value::Flag(true),
    });
    assert_eq!(refused(&storing(m, vec![])).op, "Export");
    let mut m = Maker::new();
    m.op(Op::Discard(Value::Flag(true)));
    assert_eq!(refused(&storing(m, vec![])).op, "Discard");
}

#[test]
fn a_loop_is_limited_and_a_jump_out_of_two_scopes_goes_by_a_flag() {
    // A loop whose body leaves from inside an inner scope to a block after
    // the loop that two edges reach.
    let mut m = Maker::new();
    let [a, b, _] = three_words(&mut m);
    m.op(Op::Set(Var::Vector(0), a));
    m.op(Op::Set(Var::Vector(1), b));
    m.end(Terminator::Jump(1));
    // 1: the header. Leaves for 5 when v0 is zero.
    let left = m.op(Op::Get(Var::Vector(0)));
    let done = m.op(Op::Compare(CompareOp::Equal, left, Value::Word(0)));
    m.end(Terminator::Branch {
        condition: done,
        if_true: 5,
        if_false: 2,
    });
    // 2: either 3 or 4, which both go on to the latch 6... or leave for 5.
    let other = m.op(Op::Get(Var::Vector(1)));
    let odd = m.op(Op::Binary(BinaryOp::And, other, Value::Word(1)));
    let odd = m.op(Op::Compare(CompareOp::NotEqual, odd, Value::Word(0)));
    m.end(Terminator::Branch {
        condition: odd,
        if_true: 3,
        if_false: 4,
    });
    // 3: leaves the loop from the middle.
    let other = m.op(Op::Get(Var::Vector(1)));
    let big = m.op(Op::Compare(
        CompareOp::UnsignedGreater,
        other,
        Value::Word(100),
    ));
    m.end(Terminator::Branch {
        condition: big,
        if_true: 5,
        if_false: 6,
    });
    // 4.
    m.end(Terminator::Jump(6));
    // 5: after the loop.
    let result = m.op(Op::Get(Var::Vector(1)));
    m.op(Op::StoreMemory {
        address: Address {
            low: Value::Word(0x1000),
            high: Value::Word(0),
        },
        width: 32,
        values: vec![result],
        guard: Value::Flag(true),
    });
    m.end(Terminator::Return);
    // 6: the latch.
    let left = m.op(Op::Get(Var::Vector(0)));
    let left = m.op(Op::Binary(BinaryOp::Sub, left, Value::Word(1)));
    m.op(Op::Set(Var::Vector(0), left));
    m.end(Terminator::Jump(1));
    let function = m.function(HardwareStage::Compute, Interface::default());
    let source = emitted(&function).source;
    assert!(source.contains("uint fuel = 65536u;"), "{source}");
    assert!(source.contains("for (; fuel != 0u; fuel--) {"));
    assert!(source.contains("continue;"));
    assert!(source.contains("uint go = 0u;"), "{source}");
    assert!(source.contains("if (go != 0u) break;"));
    // The variables read are written by name, not copied first.
    assert!(source.contains("v0 = v0 - 1u;"), "{source}");
    assert_compiles(&source, "cs_6_6");
}

#[test]
fn the_rectangle_shader_continues_each_row_or_copies_it_and_compiles() {
    let source = rectangle_geometry_shader(3, 0b010);
    assert!(source.contains("made.row0 = a.row0 + b.row0 - corner.row0;"));
    assert!(source.contains("made.row1 = first.row1;"));
    assert!(source.contains("made.row2 = a.row2 + b.row2 - corner.row2;"));
    assert!(source.contains("made.position = a.position + b.position - corner.position;"));
    assert!(source.find("ROW2").unwrap() < source.find("SV_Position").unwrap());
    assert!(source.contains("[maxvertexcount(4)]"));
    assert_compiles(&source, "gs_6_6");
    assert_compiles(&rectangle_geometry_shader(0, 0), "gs_6_6");
}

/// A container with one vertex program assembled here: the vertex number
/// and 1.0 to the position, and 1.0 to parameter 0.
fn vertex_container() -> Vec<u8> {
    let code = asm::Code::new()
        .i(asm::vop1("v_mov_b32", 1, O::One))
        .i(asm::exp(12, [0, 1, 1, 1], true))
        .i(asm::exp(32, [1, 1, 1, 1], false))
        .end();
    let mut program = TestProgram::new(0x28, false, &code);
    // The stage data of a vertex program: no inputs, one output.
    let mut data = vec![0u8; 0x18C];
    data[0xC8..0xCE].copy_from_slice(&[6, 1, 0, 0, 0, 0]);
    data[0x188] = 1;
    program.stage_data = data;
    let outputs = crate::container::testing::signature(&[TestElement {
        name: "SV_Position",
        index: 0,
        system_value: 1,
        component_type: 3,
        register: 0,
        mask: 0b1111,
    }]);
    let dxil = dxil_part(1, &[(0x0C, machine_code(&[program]))]);
    dxbc(&[
        (b"ISG1", crate::container::testing::signature(&[])),
        (b"OSG1", outputs),
        (b"DXIL", dxil),
    ])
}

#[test]
fn a_container_is_translated_from_its_bytes_and_each_failing_step_is_named() {
    let container = vertex_container();
    let shader = translate(&container, HardwareStage::Vertex, &Options::default()).unwrap();
    assert_eq!(shader.profile, "vs_6_6");
    assert_eq!(shader.stage, HardwareStage::Vertex);
    assert_eq!(shader.rows, 1);
    assert!(shader.samplers.is_empty());
    assert!(shader.source.contains("SV_VertexID"));
    assert!(shader.source.contains("out_row0 = asfloat(uint4("));
    assert_compiles(&shader.source, "vs_6_6");

    let none = translate(&container, HardwareStage::Pixel, &Options::default()).unwrap_err();
    assert_eq!(none, TranslateError::NoProgram(HardwareStage::Pixel));
    assert_eq!(none.to_string(), "read: no PS program in the container");
    let read = translate(b"nothing", HardwareStage::Vertex, &Options::default()).unwrap_err();
    assert!(matches!(read, TranslateError::Read(_)));
    assert!(read.to_string().starts_with("read: "));
    let mesh = translate(&container, HardwareStage::Mesh, &Options::default()).unwrap_err();
    assert!(mesh.to_string().starts_with("emit: "));

    // An instruction the lift has no rule for names itself.
    let code = asm::Code::new().i(asm::sopp("s_sendmsg", 0)).end();
    let mut program = TestProgram::new(0x28, false, &code);
    program.stage_data = vec![0u8; 0x18C];
    let dxil = dxil_part(1, &[(0x0C, machine_code(&[program]))]);
    let container = dxbc(&[(b"DXIL", dxil)]);
    let lift = translate(&container, HardwareStage::Vertex, &Options::default()).unwrap_err();
    assert!(matches!(lift, TranslateError::Lift(_)), "{lift}");
    assert!(lift.to_string().starts_with("lift: "), "{lift}");
    assert!(!lift.to_string().contains('\n'));
}

#[test]
fn attribute_rows_of_the_interface_do_not_change_what_is_declared() {
    // The interface's table of rows is by signature row; the shaders pass
    // rows by attribute number, as the design says.
    let mut m = Maker::new();
    let value = m.op(Op::Input(Input::Interpolated {
        attribute: 1,
        channel: 0,
        weights: Weights::PerspectiveCenter,
    }));
    m.op(Op::Export {
        target: ExportTarget::Color(0),
        values: [Some(value); 4],
        guard: Value::Flag(true),
    });
    m.end(Terminator::Return);
    let function = m.function(
        HardwareStage::Pixel,
        Interface {
            pixel_inputs: vec![AttributeRow {
                attribute: 1,
                row: 9,
                mask: 0xF,
                flat: false,
            }],
            ..Interface::default()
        },
    );
    let shader = emitted(&function);
    assert_eq!(shader.rows, 2);
    assert!(shader.source.contains("float4 row1 : ROW1;"));
    assert!(!shader.source.contains("ROW9"));
    // Nothing to compile against here is needed for the text to be whole.
    let _ = compile(&shader.source, "ps_6_6").map(|result| result.unwrap());
}

#[test]
fn reductions_and_prefix_sums_become_wave_intrinsics() {
    let reductions = [
        (ReduceOp::Add, "WaveActiveSum("),
        (ReduceOp::FloatAdd, "WaveActiveSum(asfloat("),
        (ReduceOp::And, "WaveActiveBitAnd("),
        (ReduceOp::Or, "WaveActiveBitOr("),
        (ReduceOp::Xor, "WaveActiveBitXor("),
        (ReduceOp::MinUnsigned, "WaveActiveMin("),
        (ReduceOp::MaxUnsigned, "WaveActiveMax("),
        (ReduceOp::MinSigned, "WaveActiveMin("),
        (ReduceOp::MaxSigned, "WaveActiveMax("),
        (ReduceOp::FloatMin, "WaveActiveMin(f_key("),
        (ReduceOp::FloatMax, "WaveActiveMax(f_key("),
    ];
    for (op, intrinsic) in reductions {
        for stage in [HardwareStage::Compute, HardwareStage::Pixel] {
            let mut m = Maker::new();
            let words = match stage {
                HardwareStage::Pixel => [0, 1, 2].map(|channel| {
                    m.op(Op::Input(Input::Interpolated {
                        attribute: 0,
                        channel,
                        weights: Weights::PerspectiveCenter,
                    }))
                }),
                _ => three_words(&mut m),
            };
            let active = m.op(Op::Compare(CompareOp::UnsignedLess, words[0], words[1]));
            let folded = m.op(Op::Reduce {
                op,
                value: words[2],
                active,
            });
            let function = match stage {
                HardwareStage::Pixel => exporting(m, [folded; 4]),
                _ => storing(m, vec![folded]),
            };
            let source = emitted(&function).source;
            assert!(source.contains(intrinsic), "{op:?}\n{source}");
            // The lanes that take no part count as the identity.
            assert!(
                source.contains(&format!("{:#x}u", op.identity()))
                    || op.identity() == 0 && source.contains(", 0u)"),
                "{op:?}\n{source}"
            );
            // A helper lane of a pixel shader answers for itself.
            assert_eq!(
                source.contains("IsHelperLane()"),
                stage == HardwareStage::Pixel
            );
            let profile = if stage == HardwareStage::Pixel {
                "ps_6_6"
            } else {
                "cs_6_6"
            };
            assert_compiles(&source, profile);
        }
    }
    let prefix: fn([Value; 3], Value) -> Op = |[a, _, _], flag| Op::PrefixSum {
        value: a,
        active: flag,
    };
    let index: fn([Value; 3], Value) -> Op = |_, _| Op::LaneIndex;
    for (op, intrinsic) in [(prefix, "WavePrefixSum("), (index, "WaveGetLaneIndex()")] {
        let compute = emitted(&cross_lane(HardwareStage::Compute, op)).source;
        assert!(compute.contains(intrinsic), "{compute}");
        assert_compiles(&compute, "cs_6_6");
        let pixel = emitted(&cross_lane(HardwareStage::Pixel, op)).source;
        assert_compiles(&pixel, "ps_6_6");
    }
}

#[test]
fn atomic_operations_are_interlocked_ones_and_the_signed_ones_are_refused() {
    let operations = [
        (AtomicOp::Swap, "InterlockedExchange("),
        (AtomicOp::CompareSwap, "InterlockedCompareExchange("),
        (AtomicOp::Add, "InterlockedAdd("),
        (AtomicOp::Sub, "InterlockedAdd("),
        (AtomicOp::Increment, "InterlockedAdd("),
        (AtomicOp::Decrement, "InterlockedAdd("),
        (AtomicOp::MinUnsigned, "InterlockedMin("),
        (AtomicOp::MaxUnsigned, "InterlockedMax("),
        (AtomicOp::And, "InterlockedAnd("),
        (AtomicOp::Or, "InterlockedOr("),
        (AtomicOp::Xor, "InterlockedXor("),
    ];
    // The three places an atomic operation can work on.
    let places = |m: &mut Maker| {
        let descriptor = descriptor(m);
        let [a, b, _] = three_words(m);
        [
            AtomicPlace::Memory(Address { low: a, high: b }),
            AtomicPlace::Buffer {
                descriptor,
                position: BufferPosition {
                    index: a,
                    offset: b,
                },
            },
            AtomicPlace::Shared(a),
        ]
    };
    for (op, intrinsic) in operations {
        let mut m = Maker::new();
        let [a, b, c] = three_words(&mut m);
        let guard = m.op(Op::Compare(CompareOp::UnsignedLess, a, b));
        let mut results = Vec::new();
        for place in places(&mut m) {
            results.push(m.op(Op::Atomic {
                op,
                place,
                value: c,
                compare: b,
                guard,
            }));
        }
        let source = emitted(&storing(m, results)).source;
        // Once for title memory, once for the shared memory.
        assert!(source.matches(intrinsic).count() >= 2, "{op:?}\n{source}");
        assert!(source.contains("groupshared uint shared_words["));
        assert_compiles(&source, "cs_6_6");
    }
    for op in [AtomicOp::MinSigned, AtomicOp::MaxSigned] {
        let mut m = Maker::new();
        let [a, b, c] = three_words(&mut m);
        let old = m.op(Op::Atomic {
            op,
            place: AtomicPlace::Memory(Address { low: a, high: b }),
            value: c,
            compare: Value::Word(0),
            guard: Value::Flag(true),
        });
        let error = refused(&storing(m, vec![old]));
        assert_eq!(error.op, "Atomic");
        assert!(error.reason.contains("signed"));
    }
}

/// A compute function with shared memory of 1,024 bytes and the given
/// group size, made of the blocks `build` makes.
fn sharing(group_size: [u32; 3], build: impl FnOnce(&mut Maker)) -> Function {
    let mut m = Maker::new();
    build(&mut m);
    m.function(
        HardwareStage::Compute,
        Interface {
            group_size,
            shared_bytes: 1024,
            ..Interface::default()
        },
    )
}

/// Writes the thread's word to the shared memory.
fn share(m: &mut Maker) {
    let [a, b, _] = three_words(m);
    for width in [8, 16, 32] {
        m.op(Op::StoreShared {
            offset: a,
            width,
            values: vec![b, a],
            guard: Value::Flag(true),
        });
    }
}

/// Reads the shared memory and stores what was read.
fn take(m: &mut Maker) {
    let [a, _, _] = three_words(m);
    let mut read = Vec::new();
    for width in [8, 16, 32] {
        read.extend(m.all(Op::LoadShared {
            offset: a,
            width,
            words: 2,
        }));
    }
    m.op(Op::StoreMemory {
        address: Address {
            low: Value::Word(0x1000),
            high: Value::Word(0),
        },
        width: 32,
        values: read,
        guard: Value::Flag(true),
    });
}

#[test]
fn a_group_of_one_wave_gets_the_barriers_the_console_did_not_need() {
    const BARRIER: &str = "GroupMemoryBarrierWithGroupSync();";
    // A write and then a read, in the one block every thread runs: one
    // barrier between them.
    let straight = |m: &mut Maker| {
        share(m);
        take(m);
        m.end(Terminator::Return);
    };
    let source = emitted(&sharing([8, 8, 1], straight)).source;
    assert!(source.contains("static const uint SHARED_BYTES = 1024u;"));
    assert!(source.contains("groupshared uint shared_words[256];"));
    assert_eq!(source.matches(BARRIER).count(), 1, "{source}");
    let barrier = source.find(BARRIER).unwrap();
    assert!(source.rfind("shared_store(").unwrap() < barrier);
    assert!(source[barrier..].contains("shared_load("));
    assert_compiles(&source, "cs_6_6");

    // A group of more than one wave keeps to the title's own barriers, and
    // a group of one thread needs none.
    for group in [[16, 8, 1], [1, 1, 1]] {
        let source = emitted(&sharing(group, straight)).source;
        assert!(!source.contains(BARRIER), "{source}");
        assert_compiles(&source, "cs_6_6");
    }

    // The title's own barrier is written where it is, and is enough.
    let fenced = |m: &mut Maker| {
        share(m);
        m.op(Op::Barrier);
        take(m);
        m.end(Terminator::Return);
    };
    let source = emitted(&sharing([8, 8, 1], fenced)).source;
    assert_eq!(source.matches(BARRIER).count(), 1, "{source}");
    assert_compiles(&source, "cs_6_6");

    // A read, a write and a read again: two barriers.
    let twice = |m: &mut Maker| {
        take(m);
        share(m);
        take(m);
        m.end(Terminator::Return);
    };
    let source = emitted(&sharing([8, 8, 1], twice)).source;
    assert_eq!(source.matches(BARRIER).count(), 2, "{source}");

    // Writes on one side of a branch only, read after the join: the
    // barrier stands at the join, which every thread passes.
    let branched = |m: &mut Maker| {
        let [a, b, _] = three_words(m);
        let flag = m.op(Op::Compare(CompareOp::UnsignedLess, a, b));
        m.end(Terminator::Branch {
            condition: flag,
            if_true: 1,
            if_false: 2,
        });
        share(m);
        m.end(Terminator::Jump(2));
        take(m);
        m.end(Terminator::Return);
    };
    let source = emitted(&sharing([8, 8, 1], branched)).source;
    assert_eq!(source.matches(BARRIER).count(), 1, "{source}");
    assert_compiles(&source, "cs_6_6");
}

#[test]
fn shared_memory_that_no_barrier_can_order_is_refused() {
    // A read inside a branch after a write before it: only some threads get
    // to the read, and a barrier has to be reached by all.
    let inside = |m: &mut Maker| {
        let [a, b, _] = three_words(m);
        share(m);
        let flag = m.op(Op::Compare(CompareOp::UnsignedLess, a, b));
        m.end(Terminator::Branch {
            condition: flag,
            if_true: 1,
            if_false: 2,
        });
        take(m);
        m.end(Terminator::Jump(2));
        m.end(Terminator::Return);
    };
    // The barrier stands at the end of the block before the branch, which
    // every thread passes.
    let source = emitted(&sharing([8, 8, 1], inside)).source;
    assert_eq!(
        source.matches("GroupMemoryBarrierWithGroupSync();").count(),
        1,
        "{source}"
    );
    let barrier = source.find("GroupMemoryBarrierWithGroupSync();").unwrap();
    assert!(source.rfind("shared_store(").unwrap() < barrier);
    assert!(source[barrier..].contains("shared_load("));
    assert_compiles(&source, "cs_6_6");

    // A write and then a read, both inside the branch: no place for a
    // barrier that every thread reaches lies between them.
    let both = |m: &mut Maker| {
        let [a, b, _] = three_words(m);
        let flag = m.op(Op::Compare(CompareOp::UnsignedLess, a, b));
        m.end(Terminator::Branch {
            condition: flag,
            if_true: 1,
            if_false: 2,
        });
        share(m);
        take(m);
        m.end(Terminator::Jump(2));
        m.end(Terminator::Return);
    };
    let error = refused(&sharing([8, 8, 1], both));
    assert_eq!(error.op, "LoadShared");
    assert!(error.reason.contains("no barrier can stand"));
    // The same program in a group of several waves is the title's to
    // order.
    assert_compiles(&emitted(&sharing([16, 16, 1], both)).source, "cs_6_6");

    // A write after a read, in a loop.
    let looped = |m: &mut Maker| {
        m.end(Terminator::Jump(1));
        let [a, b, _] = three_words(m);
        take(m);
        share(m);
        let flag = m.op(Op::Compare(CompareOp::UnsignedLess, a, b));
        m.end(Terminator::Branch {
            condition: flag,
            if_true: 1,
            if_false: 2,
        });
        m.end(Terminator::Return);
    };
    let error = refused(&sharing([8, 8, 1], looped));
    assert!(error.reason.contains("no barrier can stand"), "{error}");

    // Outside a compute shader there is no shared memory and no barrier.
    let mut m = Maker::new();
    m.op(Op::StoreShared {
        offset: Value::Word(0),
        width: 32,
        values: vec![Value::Word(1)],
        guard: Value::Flag(true),
    });
    m.end(Terminator::Return);
    let vertex = m.function(HardwareStage::Vertex, Interface::default());
    assert_eq!(refused(&vertex).op, "LoadShared");
    let mut m = Maker::new();
    m.op(Op::Barrier);
    m.end(Terminator::Return);
    let vertex = m.function(HardwareStage::Vertex, Interface::default());
    assert_eq!(refused(&vertex).op, "Barrier");
}
