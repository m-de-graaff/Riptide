//! Deciding what each function a title imports is bound to.
//!
//! There are three answers and no fourth: Riptide's own implementation, the host's function
//! where the console's is the same, or a stub that names the import when it is called.
//! Nothing returns a guess.

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fmt;
use std::sync::{Arc, Mutex};

use riptide_pe::{Image, Resolver, Symbol};

use crate::stub::{Log, Stubs};
use crate::sys;

/// What an import is bound to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Binding {
    /// Riptide's own implementation of the console function.
    Riptide,
    /// The host's function of the same module and name, because the console's is the same.
    Host,
    /// A stub that names the import and stops the title if it is ever called.
    Stub,
}

impl fmt::Display for Binding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `pad` honours a width, so the census can line its columns up.
        f.pad(match self {
            Binding::Riptide => "riptide",
            Binding::Host => "host",
            Binding::Stub => "stub",
        })
    }
}

/// A module name and a symbol in it. Module names compare as Windows compares them: without
/// regard to case, and with `.dll` assumed when the name has no extension.
type Key = (String, Symbol);

fn key(module: &str, symbol: &Symbol) -> Key {
    (canonical(module), symbol.clone())
}

/// A module's name in lower case with its extension.
fn normalised(module: &str) -> String {
    let mut module = module.to_ascii_lowercase();
    if !module.contains('.') {
        module.push_str(".dll");
    }
    module
}

/// The name a module's functions are kept under. Titles import Windows' core functions
/// through API-set names (`api-ms-win-core-synch-l1-2-0.dll` for `Sleep`), which are other
/// names for `kernel32.dll`: all of them are kept as `kernel32.dll`, so a function is
/// implemented once whatever name a title imports it by.
fn canonical(module: &str) -> String {
    let module = normalised(module);
    if module.starts_with("api-ms-win-core-") || module == "kernelbase.dll" {
        "kernel32.dll".to_owned()
    } else {
        module
    }
}

/// Windows' own modules and the C and C++ runtime: a title's imports from these are the
/// host's functions unless Riptide implements them. Matched against the start of the
/// module's name. Console modules are never listed here: theirs are implemented or stubs.
const HOST_MODULES: &[&str] = &[
    "api-ms-win-",
    "ext-ms-win-",
    "kernel32.dll",
    "kernelbase.dll",
    "vcruntime140",
    "msvcp140",
    "ucrtbase.dll",
    "ws2_32.dll",
    "winhttp.dll",
    "bcrypt.dll",
    "advapi32.dll",
    "mfplat.dll",
    "mfreadwrite.dll",
    // The input library is the same on the console and on Windows.
    "gameinput.dll",
    // So is the audio library, by the same name.
    "xaudio2_9.dll",
    // Certificates, for the HTTP libraries a title brings in its package.
    "crypt32.dll",
];

/// The table of what every import is bound to. It is the [`Resolver`] `riptide-pe` binds a
/// title with.
///
/// It has to outlive every image bound through it: the stubs it makes are its own memory.
pub struct Imports {
    riptide: HashMap<Key, usize>,
    host: HashSet<Key>,
    /// Whether [`HOST_MODULES`] are passed to the host whole.
    host_modules: bool,
    stubs: Mutex<StubState>,
}

struct StubState {
    stubs: Stubs,
    /// The stub already made for an import, so asking twice gives the same address.
    made: HashMap<Key, usize>,
}

impl Imports {
    /// The table with every console function Riptide implements so far, logging called stubs
    /// to standard error.
    ///
    /// Few are implemented yet, so today most imports of a real title are stubs. The console
    /// API issues each add their functions here.
    pub fn new() -> Imports {
        Imports::with_log(Arc::new(|line: &str| eprintln!("riptide: {line}")))
    }

    /// As [`Imports::new`], with each stub's first call reported to `log` instead.
    pub fn with_log(log: Log) -> Imports {
        let mut imports = Imports::empty(log);
        imports.host_modules = true;
        crate::timers::register(&mut imports);
        crate::process::register(&mut imports);
        crate::xmem::register(&mut imports);
        crate::dstorage::register(&mut imports);
        crate::runtime::register(&mut imports);
        crate::files::register(&mut imports);
        crate::cores::register(&mut imports);
        crate::graphics::register(&mut imports);
        crate::bvh::register(&mut imports);
        crate::xg::register(&mut imports);
        crate::acphal::register(&mut imports);
        crate::xapu::register(&mut imports);
        crate::xdsp::register(&mut imports);
        crate::offline::register(&mut imports);
        imports
    }

    /// A table with nothing in it: every import is a stub until it is added.
    pub fn empty(log: Log) -> Imports {
        Imports {
            riptide: HashMap::new(),
            host: HashSet::new(),
            host_modules: false,
            stubs: Mutex::new(StubState {
                stubs: Stubs::new(log),
                made: HashMap::new(),
            }),
        }
    }

    /// Binds `module`'s `symbol` to Riptide's implementation at `address`.
    ///
    /// # Safety
    ///
    /// `address` is a function with the Windows x64 calling convention and the signature
    /// titles expect of that import, and it stays valid for as long as this table is used.
    pub unsafe fn implement(&mut self, module: &str, symbol: Symbol, address: usize) {
        self.riptide.insert(key(module, &symbol), address);
    }

    /// Binds `module`'s function `name` to the host's function of the same module and name.
    ///
    /// Each function is listed on its own, never a whole module: every one is a judgement
    /// that the console's function behaves as the host's does.
    pub fn pass_through(&mut self, module: &str, name: &str) {
        self.host
            .insert(key(module, &Symbol::Name(name.to_owned())));
    }

    /// What `module`'s `symbol` is bound to. Asking runs nothing of the title's. For a
    /// function passed through to the host it loads that Windows module, if it is not loaded
    /// already, to see whether this Windows has the function.
    pub fn binding(&self, module: &str, symbol: &Symbol) -> Binding {
        let key = key(module, symbol);
        if self.riptide.contains_key(&key) {
            Binding::Riptide
        } else if self.is_host(module, &key) && host_address(module, symbol).is_some() {
            Binding::Host
        } else {
            Binding::Stub
        }
    }

    /// Lists every import of `image` with what it is bound to, without loading or running
    /// the image. Sorted by module, then by function.
    pub fn census(&self, image: &Image) -> Census {
        let mut lines: Vec<CensusLine> = image
            .imports
            .iter()
            .flat_map(|module| {
                module.imports.iter().map(|import| CensusLine {
                    module: module.name.clone(),
                    symbol: import.symbol.clone(),
                    binding: self.binding(&module.name, &import.symbol),
                })
            })
            .collect();
        lines.sort_by(|a, b| {
            (a.module.to_ascii_lowercase(), a.symbol.to_string())
                .cmp(&(b.module.to_ascii_lowercase(), b.symbol.to_string()))
        });
        Census { lines }
    }
}

impl Imports {
    /// Whether an import that Riptide does not implement goes to the host.
    fn is_host(&self, module: &str, key: &Key) -> bool {
        if self.host.contains(key) {
            return true;
        }
        let module = normalised(module);
        self.host_modules && HOST_MODULES.iter().any(|host| module.starts_with(host))
    }
}

impl Default for Imports {
    fn default() -> Self {
        Imports::new()
    }
}

impl Resolver for Imports {
    /// Always gives an address: the implementation, the host's function, or a stub.
    /// `None` only if Windows gives no memory for a stub.
    fn resolve(&self, module: &str, symbol: &Symbol) -> Option<usize> {
        let key = key(module, symbol);
        if let Some(&address) = self.riptide.get(&key) {
            return Some(address);
        }
        if self.is_host(module, &key)
            && let Some(address) = host_address(module, symbol)
        {
            return Some(address);
        }
        let mut state = self
            .stubs
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(&address) = state.made.get(&key) {
            return Some(address);
        }
        let address = state.stubs.add(format!("{module}!{symbol}"))?;
        state.made.insert(key, address);
        Some(address)
    }
}

/// The address of the host's own function, if this Windows has that module and function.
fn host_address(module: &str, symbol: &Symbol) -> Option<usize> {
    // A bare module name only: a path would name a file of the title's choosing.
    if module.contains(['/', '\\', ':']) {
        return None;
    }
    let module = CString::new(module).ok()?;
    let name = match symbol {
        Symbol::Name(name) => Some(CString::new(name.as_str()).ok()?),
        Symbol::Ordinal(_) => None,
    };
    // SAFETY: both strings are null-terminated. Loading a module runs its initialisation, as
    // it would for any program that imports it; only Windows' own modules are looked for, so
    // a file of the same name in the title's folder is never loaded.
    let handle = unsafe {
        sys::LoadLibraryExA(
            module.as_ptr().cast(),
            std::ptr::null_mut(),
            sys::LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    };
    if handle.is_null() {
        return None;
    }
    // A function is asked for by its name, or by its ordinal passed in place of the name's
    // address, which is how Windows takes one.
    let wanted = match (&name, symbol) {
        (Some(name), _) => name.as_ptr().cast::<u8>(),
        (None, Symbol::Ordinal(ordinal)) => usize::from(*ordinal) as *const u8,
        (None, Symbol::Name(_)) => return None,
    };
    // SAFETY: `handle` is a loaded module and `wanted` is a null-terminated name or an
    // ordinal below 65536.
    let address = unsafe { sys::GetProcAddress(handle, wanted) };
    (!address.is_null()).then_some(address as usize)
}

/// One import and what it is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CensusLine {
    pub module: String,
    pub symbol: Symbol,
    pub binding: Binding,
}

impl fmt::Display for CensusLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:<8}{}!{}", self.binding, self.module, self.symbol)
    }
}

/// Every import of one image with what it is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Census {
    pub lines: Vec<CensusLine>,
}

impl Census {
    /// How many imports have this binding.
    pub fn count(&self, binding: Binding) -> usize {
        self.lines.iter().filter(|l| l.binding == binding).count()
    }
}

impl fmt::Display for Census {
    /// One line per import, then a line of totals.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for line in &self.lines {
            writeln!(f, "{line}")?;
        }
        write!(
            f,
            "{} imports: {} implemented by Riptide, {} passed to the host, {} stubs",
            self.lines.len(),
            self.count(Binding::Riptide),
            self.count(Binding::Host),
            self.count(Binding::Stub)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(name: &str) -> Symbol {
        Symbol::Name(name.to_owned())
    }

    extern "system" fn implemented() -> u32 {
        7
    }

    fn table() -> Imports {
        let mut imports = Imports::empty(Arc::new(|_: &str| {}));
        // SAFETY: `implemented` is a real function; these tests never call through it with
        // another signature.
        unsafe {
            imports.implement(
                "Console.dll",
                name("Built"),
                implemented as *const () as usize,
            );
            imports.implement(
                "console.dll",
                Symbol::Ordinal(3),
                implemented as *const () as usize,
            );
        }
        imports.pass_through("kernel32.dll", "GetCurrentThreadId");
        imports.pass_through("kernel32.dll", "NoSuchFunctionInAnyWindows");
        imports
    }

    #[test]
    fn an_implemented_function_is_bound_to_riptide_whatever_the_modules_case() {
        let imports = table();
        assert_eq!(
            imports.binding("CONSOLE.DLL", &name("Built")),
            Binding::Riptide
        );
        assert_eq!(
            imports.resolve("console.dll", &name("Built")),
            Some(implemented as *const () as usize)
        );
        assert_eq!(
            imports.binding("console.dll", &Symbol::Ordinal(3)),
            Binding::Riptide
        );
        // A module named without its extension is the same module.
        assert_eq!(imports.binding("CONSOLE", &name("Built")), Binding::Riptide);
        // Function names keep their case.
        assert_eq!(
            imports.binding("console.dll", &name("built")),
            Binding::Stub
        );
    }

    #[test]
    fn a_passed_through_function_is_the_hosts_own() {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            safe fn GetCurrentThreadId() -> u32;
        }
        let imports = table();
        let symbol = name("GetCurrentThreadId");
        assert_eq!(imports.binding("kernel32.dll", &symbol), Binding::Host);
        let address = imports.resolve("kernel32.dll", &symbol).unwrap();
        // SAFETY: the address is the host's GetCurrentThreadId, which has this signature.
        let host = unsafe { std::mem::transmute::<usize, extern "system" fn() -> u32>(address) };
        assert_eq!(host(), GetCurrentThreadId());
    }

    #[test]
    fn a_passed_through_function_the_host_lacks_is_a_stub() {
        let imports = table();
        let symbol = name("NoSuchFunctionInAnyWindows");
        assert_eq!(imports.binding("kernel32.dll", &symbol), Binding::Stub);
        assert!(imports.resolve("kernel32.dll", &symbol).is_some());
    }

    #[test]
    fn an_unknown_function_gets_one_stub_however_often_it_is_asked_for() {
        let imports = table();
        let symbol = name("NotBuilt");
        assert_eq!(imports.binding("console.dll", &symbol), Binding::Stub);
        let first = imports.resolve("console.dll", &symbol).unwrap();
        let again = imports.resolve("Console.DLL", &symbol).unwrap();
        let other = imports
            .resolve("console.dll", &name("AlsoNotBuilt"))
            .unwrap();
        assert_eq!(first, again);
        assert_ne!(first, other);
    }

    #[test]
    fn a_function_passed_through_by_name_is_not_passed_through_by_ordinal() {
        let imports = table();
        assert_eq!(
            imports.binding("kernel32.dll", &Symbol::Ordinal(1)),
            Binding::Stub
        );
    }

    #[test]
    fn a_census_line_and_its_totals_read_as_a_table() {
        let census = Census {
            lines: vec![
                CensusLine {
                    module: "console.dll".to_owned(),
                    symbol: name("Built"),
                    binding: Binding::Riptide,
                },
                CensusLine {
                    module: "kernel32.dll".to_owned(),
                    symbol: name("Sleep"),
                    binding: Binding::Host,
                },
                CensusLine {
                    module: "console.dll".to_owned(),
                    symbol: Symbol::Ordinal(9),
                    binding: Binding::Stub,
                },
            ],
        };
        assert_eq!(
            census.to_string(),
            "riptide console.dll!Built\n\
             host    kernel32.dll!Sleep\n\
             stub    console.dll!#9\n\
             3 imports: 1 implemented by Riptide, 1 passed to the host, 1 stubs"
        );
    }
}
