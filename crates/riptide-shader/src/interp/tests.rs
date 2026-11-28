//! Tests of the reference interpreter on functions built by hand. The
//! tests that compare it with the machine interpreter are with the lift, in
//! `lift/tests.rs`.

use std::collections::BTreeMap;

use super::{Host, InterpError, run_wave};
use crate::container::xbox::HardwareStage;
use crate::gfx10::WaveSize;
use crate::ir::cfg::push;
use crate::ir::{
    BinaryOp, Block, BufferPosition, CompareOp, ExportTarget, Function, ImageOp, Input, Inst,
    Interface, Op, Terminator, Value, Var,
};

/// A host whose only input is the vertex number, one more than the lane,
/// and that records what each lane exports first.
#[derive(Default)]
struct Recorder {
    exports: BTreeMap<usize, u32>,
}

impl Host for Recorder {
    fn input(&mut self, lane: usize, _input: Input) -> u32 {
        lane as u32 + 1
    }

    fn load(&mut self, _address: u64, _width: u8) -> u32 {
        0
    }

    fn store(&mut self, _address: u64, _width: u8, _value: u32) {}

    fn image(&mut self, _lane: usize, _access: &ImageOp) -> [u32; 4] {
        [0; 4]
    }

    fn export(&mut self, lane: usize, _target: ExportTarget, values: [Option<u32>; 4]) {
        self.exports.insert(lane, values[0].unwrap_or(0));
    }

    fn discard(&mut self, _lane: usize) {}
}

/// Builds a function from blocks of instructions and terminators.
fn function(blocks: Vec<(Vec<Inst>, Terminator)>, temps: u32) -> Function {
    let function = Function {
        stage: HardwareStage::Vertex,
        wave: WaveSize::Wave64,
        blocks: blocks
            .into_iter()
            .map(|(insts, terminator)| Block {
                offset: None,
                insts,
                terminator,
            })
            .collect(),
        temps,
        interface: Interface::default(),
        code: Vec::new(),
    };
    function.verify().unwrap();
    function
}

/// Exports one word to position 0.
fn export(insts: &mut Vec<Inst>, temps: &mut u32, value: Value) {
    push(
        insts,
        temps,
        Op::Export {
            target: ExportTarget::Position(0),
            values: [Some(value), None, None, None],
            guard: Value::Flag(true),
        },
    );
}

#[test]
fn a_cross_lane_operation_in_a_loop_sees_the_lanes_of_the_same_trip() {
    // Lane n goes round n + 1 times; each trip adds the number of lanes
    // that are in that trip with it.
    let mut temps = 0;
    let mut entry = Vec::new();
    let trips = Value::Temp(push(&mut entry, &mut temps, Op::Input(Input::VertexId)));
    push(&mut entry, &mut temps, Op::Set(Var::Vector(0), trips));
    let mut body = Vec::new();
    let together = Value::Temp(push(
        &mut body,
        &mut temps,
        Op::CountLanes(Value::Flag(true)),
    ));
    let sum = Value::Temp(push(&mut body, &mut temps, Op::Get(Var::Vector(1))));
    let sum = Value::Temp(push(
        &mut body,
        &mut temps,
        Op::Binary(BinaryOp::Add, sum, together),
    ));
    push(&mut body, &mut temps, Op::Set(Var::Vector(1), sum));
    let left = Value::Temp(push(&mut body, &mut temps, Op::Get(Var::Vector(0))));
    let left = Value::Temp(push(
        &mut body,
        &mut temps,
        Op::Binary(BinaryOp::Sub, left, Value::Word(1)),
    ));
    push(&mut body, &mut temps, Op::Set(Var::Vector(0), left));
    let again = Value::Temp(push(
        &mut body,
        &mut temps,
        Op::Compare(CompareOp::NotEqual, left, Value::Word(0)),
    ));
    let mut after = Vec::new();
    // After the loop every lane is back together.
    let all = Value::Temp(push(
        &mut after,
        &mut temps,
        Op::CountLanes(Value::Flag(true)),
    ));
    let sum = Value::Temp(push(&mut after, &mut temps, Op::Get(Var::Vector(1))));
    let total = Value::Temp(push(
        &mut after,
        &mut temps,
        Op::Binary(BinaryOp::ShiftLeft, all, Value::Word(16)),
    ));
    let total = Value::Temp(push(
        &mut after,
        &mut temps,
        Op::Binary(BinaryOp::Or, total, sum),
    ));
    export(&mut after, &mut temps, total);
    let function = function(
        vec![
            (entry, Terminator::Jump(1)),
            (
                body,
                Terminator::Branch {
                    condition: again,
                    if_true: 1,
                    if_false: 2,
                },
            ),
            (after, Terminator::Return),
        ],
        temps,
    );
    let mut host = Recorder::default();
    run_wave(&function, 10, &mut host).unwrap();
    for lane in 0..10u32 {
        // In trip k the lanes k to 9 are still going round.
        let expected: u32 = (0..=lane).map(|trip| 10 - trip).sum();
        assert_eq!(host.exports[&(lane as usize)], 10 << 16 | expected);
    }
}

#[test]
fn a_quad_read_of_a_lane_that_is_not_there_gives_zero() {
    let mut temps = 0;
    let mut insts = Vec::new();
    let own = Value::Temp(push(&mut insts, &mut temps, Op::Input(Input::VertexId)));
    let read = Value::Temp(push(
        &mut insts,
        &mut temps,
        Op::QuadPermute {
            value: own,
            lanes: [3, 3, 3, 3],
            active: Value::Flag(true),
        },
    ));
    let any = Value::Temp(push(&mut insts, &mut temps, Op::QuadAny(Value::Flag(true))));
    let any = Value::Temp(push(
        &mut insts,
        &mut temps,
        Op::Select {
            condition: any,
            if_true: Value::Word(0x100),
            if_false: Value::Word(0),
        },
    ));
    let both = Value::Temp(push(
        &mut insts,
        &mut temps,
        Op::Binary(BinaryOp::Or, read, any),
    ));
    export(&mut insts, &mut temps, both);
    let function = function(vec![(insts, Terminator::Return)], temps);
    let mut host = Recorder::default();
    run_wave(&function, 6, &mut host).unwrap();
    // Lanes 0 to 3 read lane 3; lanes 4 and 5 would read lane 7.
    assert_eq!(host.exports[&0], 0x104);
    assert_eq!(host.exports[&3], 0x104);
    assert_eq!(host.exports[&4], 0x100);
    assert_eq!(host.exports[&5], 0x100);
}

#[test]
fn a_run_that_cannot_be_done_says_why() {
    let mut temps = 0;
    let mut insts = Vec::new();
    push(
        &mut insts,
        &mut temps,
        Op::LoadFormat {
            descriptor: [Value::Word(0); 4],
            position: BufferPosition {
                index: Value::Word(0),
                offset: Value::Word(0),
            },
            format: Value::Word(0),
        },
    );
    let function = function(vec![(insts, Terminator::Return)], temps);
    let mut host = Recorder::default();
    assert_eq!(
        run_wave(&function, 1, &mut host),
        Err(InterpError::UnknownFormat(0))
    );
    assert_eq!(
        run_wave(&function, 65, &mut host),
        Err(InterpError::TooManyLanes)
    );
    // No lanes: nothing runs, and that is not an error.
    assert_eq!(run_wave(&function, 0, &mut host), Ok(()));
}
