//! Printing a function as text, for reading and for test failures.
//!
//! One line per instruction: the temporaries it defines, the operation's
//! name with what is fixed about it, and its operands in the order
//! [`Op::for_each_operand`] visits them.

use std::fmt;

use super::{Function, ImageKind, Op, Terminator, Value, Var};

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Value::Temp(temp) => write!(f, "t{}", temp.0),
            Value::Word(word) => write!(f, "{word:#x}"),
            Value::Flag(flag) => write!(f, "{flag}"),
        }
    }
}

impl fmt::Display for Var {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Var::Vector(n) => write!(f, "v{n}"),
            Var::Scalar(n) => write!(f, "s{n}"),
            Var::Mask(n) => write!(f, "mask{n}"),
            Var::Exec => f.write_str("exec"),
            Var::Vcc => f.write_str("vcc"),
            Var::Scc => f.write_str("scc"),
            Var::M0 => f.write_str("m0"),
            Var::Exported => f.write_str("exported"),
        }
    }
}

/// The name of an operation with the parts of it that are not operands.
fn head(op: &Op) -> String {
    match op {
        Op::Get(var) => format!("get {var}"),
        Op::Set(var, _) => format!("set {var}"),
        Op::Unary(kind, _) => format!("{kind:?}"),
        Op::Binary(kind, ..) => format!("{kind:?}"),
        Op::Ternary(kind, ..) => format!("{kind:?}"),
        Op::Compare(kind, ..) => format!("{kind:?}"),
        Op::FloatClass { .. } => "FloatClass".into(),
        Op::Select { .. } => "Select".into(),
        Op::FlagNot(_) => "FlagNot".into(),
        Op::Flag(kind, ..) => format!("Flag{kind:?}"),
        Op::Carry { .. } => "Carry".into(),
        Op::Borrow { .. } => "Borrow".into(),
        Op::Input(input) => format!("input {input:?}"),
        Op::LoadMemory { width, .. } => format!("LoadMemory{width}"),
        Op::StoreMemory { width, .. } => format!("StoreMemory{width}"),
        Op::LoadBuffer { width, .. } => format!("LoadBuffer{width}"),
        Op::StoreBuffer { width, .. } => format!("StoreBuffer{width}"),
        Op::LoadFormat { .. } => "LoadFormat".into(),
        Op::StoreFormat { .. } => "StoreFormat".into(),
        Op::LoadCode { .. } => "LoadCode".into(),
        Op::Image(image) => {
            let kind = match &image.kind {
                ImageKind::Sample => "Sample".to_string(),
                ImageKind::Gather { channel } => format!("Gather{channel}"),
                ImageKind::Load => "Load".to_string(),
                ImageKind::Store { .. } => "Store".to_string(),
                ImageKind::Info => "Info".to_string(),
                ImageKind::QueryLevel => "QueryLevel".to_string(),
            };
            let sampler = match image.sampler {
                Some(super::Sampler::Immediate(words)) => format!(" sampler {words:x?}"),
                Some(super::Sampler::Heap(_)) => " sampler heap".to_string(),
                None => String::new(),
            };
            format!(
                "Image{kind} {:?}{}{sampler}",
                image.dimension,
                if image.compare { " compare" } else { "" }
            )
        }
        Op::Export { target, .. } => format!("export {target:?}"),
        Op::Discard(_) => "Discard".into(),
        Op::FirstLane { .. } => "FirstLane".into(),
        Op::ReadLane { .. } => "ReadLane".into(),
        Op::CountLanes(_) => "CountLanes".into(),
        Op::FirstLaneIndex(_) => "FirstLaneIndex".into(),
        Op::AnyLane(_) => "AnyLane".into(),
        Op::LanesBelow { half, .. } => format!("LanesBelow {half:?}"),
        Op::BallotWord { high, .. } => format!("BallotWord high={high}"),
        Op::LaneBit { .. } => "LaneBit".into(),
        Op::LaneIndex => "LaneIndex".into(),
        Op::Reduce { op, .. } => format!("Reduce {op:?}"),
        Op::PrefixSum { .. } => "PrefixSum".into(),
        Op::Atomic { op, place, .. } => {
            let place = match place {
                super::AtomicPlace::Memory(_) => "memory",
                super::AtomicPlace::Buffer { .. } => "buffer",
                super::AtomicPlace::Shared(_) => "shared",
            };
            format!("Atomic{op:?} {place}")
        }
        Op::LoadShared { width, .. } => format!("LoadShared{width}"),
        Op::StoreShared { width, .. } => format!("StoreShared{width}"),
        Op::Barrier => "Barrier".into(),
        Op::QuadPermute { lanes, .. } => format!("QuadPermute {lanes:?}"),
        Op::QuadAny(_) => "QuadAny".into(),
    }
}

impl fmt::Display for Function {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "function {} {:?}", self.stage.short_name(), self.wave)?;
        for (id, block) in self.blocks.iter().enumerate() {
            match block.offset {
                Some(offset) => writeln!(f, "b{id}: ; from {offset:#x}")?,
                None => writeln!(f, "b{id}:")?,
            }
            for inst in &block.insts {
                f.write_str("  ")?;
                let results: Vec<String> = inst.results().map(|t| format!("t{}", t.0)).collect();
                if !results.is_empty() {
                    write!(f, "{} = ", results.join(", "))?;
                }
                f.write_str(&head(&inst.op))?;
                let mut operands = Vec::new();
                inst.op
                    .for_each_operand(|value| operands.push(value.to_string()));
                if !operands.is_empty() {
                    write!(f, " {}", operands.join(", "))?;
                }
                writeln!(f)?;
            }
            match block.terminator {
                Terminator::Jump(to) => writeln!(f, "  jump b{to}")?,
                Terminator::Branch {
                    condition,
                    if_true,
                    if_false,
                } => writeln!(f, "  branch {condition} ? b{if_true} : b{if_false}")?,
                Terminator::Return => writeln!(f, "  return")?,
            }
        }
        Ok(())
    }
}
