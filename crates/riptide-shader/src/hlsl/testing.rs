//! What the emitter's tests share: a builder of functions by hand, and the
//! DirectX shader compiler where one is installed.

#[path = "../../examples/support/dxc.rs"]
#[allow(dead_code)]
pub(crate) mod dxc;

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::container::xbox::HardwareStage;
use crate::gfx10::WaveSize;
use crate::ir::{Block, Function, Inst, Interface, Op, Temp, Terminator, Value};

/// Builds a function block by block.
#[derive(Default)]
pub(crate) struct Maker {
    temps: u32,
    blocks: Vec<Block>,
    current: Vec<Inst>,
}

impl Maker {
    pub(crate) fn new() -> Maker {
        Maker::default()
    }

    /// Appends an operation to the block being built and returns its
    /// results.
    pub(crate) fn all(&mut self, op: Op) -> Vec<Value> {
        let first = self.temps;
        let count = u32::from(op.result_count());
        self.temps += count;
        self.current.push(Inst {
            result: Temp(first),
            op,
        });
        (first..first + count)
            .map(|temp| Value::Temp(Temp(temp)))
            .collect()
    }

    /// Appends an operation and returns its first result. An operation
    /// without one gives a value nothing may read.
    pub(crate) fn op(&mut self, op: Op) -> Value {
        self.all(op)
            .first()
            .copied()
            .unwrap_or(Value::Word(0xDEAD_DEAD))
    }

    /// Ends the block being built and returns its number.
    pub(crate) fn end(&mut self, terminator: Terminator) -> usize {
        self.blocks.push(Block {
            offset: None,
            insts: std::mem::take(&mut self.current),
            terminator,
        });
        self.blocks.len() - 1
    }

    /// The function, which must pass the form's own checks.
    pub(crate) fn function(self, stage: HardwareStage, interface: Interface) -> Function {
        let function = Function {
            stage,
            wave: WaveSize::Wave64,
            blocks: self.blocks,
            temps: self.temps,
            interface,
            code: Vec::new(),
        };
        function.verify().unwrap();
        function
    }
}

/// The compiler, if one is installed.
pub(crate) fn compiler() -> Option<&'static PathBuf> {
    static COMPILER: OnceLock<Option<PathBuf>> = OnceLock::new();
    COMPILER.get_or_init(dxc::find).as_ref()
}

/// Compiles a shader with the emitter's arguments. `None` where no compiler
/// is installed: the test then has nothing to say.
pub(crate) fn compile(source: &str, profile: &str) -> Option<Result<Vec<u8>, String>> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let compiler = compiler()?;
    let scratch = std::env::temp_dir().join(format!(
        "riptide-shader-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    Some(dxc::compile(
        compiler,
        source,
        profile,
        super::ARGUMENTS,
        &scratch,
    ))
}

/// Asserts that a shader compiles, where a compiler is installed.
pub(crate) fn assert_compiles(source: &str, profile: &str) {
    if let Some(Err(said)) = compile(source, profile) {
        panic!("the shader does not compile:\n{said}\n{source}");
    }
}
