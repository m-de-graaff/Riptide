//! The DirectX shader compiler, which turns a translated shader's HLSL into what the host
//! runs (design T3).
//!
//! The compiler is a library of Microsoft's (`dxcompiler.dll`, with `dxil.dll` beside it to
//! sign what it makes). It is not Riptide's to ship in its source, so it is looked for:
//! beside `riptide.exe`, then in the folder `RIPTIDE_DXC` names, then in the newest Windows
//! SDK, then in a Vulkan SDK. It is loaded the first time something is compiled and stays
//! for the run.

// Nothing draws with translated shaders yet; a test on a real GPU does (`hosttest.rs`).
#![allow(dead_code)]

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::Dxc::{
    CLSID_DxcCompiler, DXC_CP_UTF8, DXC_OUT_ERRORS, DXC_OUT_OBJECT, DxcBuffer, IDxcBlob,
    IDxcBlobUtf8, IDxcCompiler3, IDxcResult,
};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_WITH_ALTERED_SEARCH_PATH, LoadLibraryExW,
};
use windows::core::{GUID, HRESULT, HSTRING, Interface, PCWSTR, s};

use crate::trace;

/// `DxcCreateInstance`: `(class, interface, object out)`.
type CreateInstance =
    unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;

/// The folders the compiler is looked for in, in order.
fn folders() -> Vec<PathBuf> {
    let mut folders = Vec::new();
    if let Some(beside) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_owned))
    {
        folders.push(beside);
    }
    if let Some(named) = std::env::var_os("RIPTIDE_DXC") {
        folders.push(PathBuf::from(named));
    }
    // The Windows SDK: the newest version's folder for this kind of machine.
    let kits = std::env::var_os("ProgramFiles(x86)").map(|programs| {
        PathBuf::from(programs)
            .join("Windows Kits")
            .join("10")
            .join("bin")
    });
    if let Some(kits) = kits {
        folders.extend(
            newest_first(&kits)
                .into_iter()
                .map(|version| version.join("x64")),
        );
    }
    // A Vulkan SDK: where its installer says, and then the usual place.
    if let Some(sdk) = std::env::var_os("VULKAN_SDK") {
        folders.push(PathBuf::from(sdk).join("Bin"));
    }
    folders.extend(
        newest_first(Path::new("C:/VulkanSDK"))
            .into_iter()
            .map(|version| version.join("Bin")),
    );
    folders
}

/// The folders inside `folder`, the one whose name sorts last first: SDK versions are
/// numbers with dots, and a later version sorts later.
fn newest_first(folder: &Path) -> Vec<PathBuf> {
    let mut inside: Vec<PathBuf> = std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    inside.sort();
    inside.reverse();
    inside
}

/// The compiler library: where it was found and how to make its objects.
struct Library {
    create: CreateInstance,
    folder: PathBuf,
}

fn library() -> Option<&'static Library> {
    static LIBRARY: OnceLock<Option<Library>> = OnceLock::new();
    LIBRARY
        .get_or_init(|| {
            for folder in folders() {
                let file = folder.join("dxcompiler.dll");
                if !file.is_file() {
                    continue;
                }
                // Loaded with its own folder searched first, so that it finds the
                // `dxil.dll` that came with it.
                // SAFETY: loading a library by its full path, and asking it for a function
                // it exports under that name with that signature.
                let create = unsafe {
                    let module: HMODULE = match LoadLibraryExW(
                        &HSTRING::from(file.as_os_str()),
                        None,
                        LOAD_WITH_ALTERED_SEARCH_PATH,
                    ) {
                        Ok(module) => module,
                        Err(error) => {
                            trace!("{} could not be loaded: {error}", file.display());
                            continue;
                        }
                    };
                    GetProcAddress(module, s!("DxcCreateInstance"))
                        .map(|function| std::mem::transmute::<_, CreateInstance>(function))
                };
                let Some(create) = create else {
                    trace!("{} has no DxcCreateInstance", file.display());
                    continue;
                };
                trace!(
                    "the shader compiler is {}{}",
                    file.display(),
                    if folder.join("dxil.dll").is_file() {
                        ""
                    } else {
                        " (with no dxil.dll beside it: it has to sign what it makes itself)"
                    }
                );
                return Some(Library { create, folder });
            }
            trace!(
                "no shader compiler (dxcompiler.dll) was found beside riptide.exe, in \
                 RIPTIDE_DXC, in a Windows SDK or in a Vulkan SDK: nothing can be drawn"
            );
            None
        })
        .as_ref()
}

/// The folder the compiler was found in, if it was. For preflight and the session log.
#[allow(dead_code)] // the first user is the report of what a session ran with
pub(crate) fn found_in() -> Option<&'static Path> {
    library().map(|library| library.folder.as_path())
}

thread_local! {
    /// One compiler for each thread that compiles: a compiler object is not to be used by
    /// two threads at once.
    static COMPILER: Option<IDxcCompiler3> = library().and_then(|library| {
        let mut object = std::ptr::null_mut();
        // SAFETY: the library's own function, with a valid out-pointer; on success the
        // object is an `IDxcCompiler3`, as asked for.
        unsafe {
            (library.create)(&CLSID_DxcCompiler, &IDxcCompiler3::IID, &mut object)
                .ok()
                .ok()?;
            Some(IDxcCompiler3::from_raw(object))
        }
    });
}

/// Compiles `source`, whose entry point is `entry`, for the shader model `profile` (as
/// `vs_6_6`). `arguments` are passed to the compiler after those. Returns the compiled
/// program, or what the compiler said was wrong.
pub(crate) fn compile(
    source: &str,
    entry: &str,
    profile: &str,
    arguments: &[&str],
) -> Result<Vec<u8>, String> {
    COMPILER.with(|compiler| {
        let Some(compiler) = compiler else {
            return Err("the shader compiler could not be loaded".to_owned());
        };
        let mut words: Vec<HSTRING> = ["-E", entry, "-T", profile]
            .iter()
            .chain(arguments)
            .map(|&word| HSTRING::from(word))
            .collect();
        // Warnings are not errors here: translated code is full of what a person would
        // not write.
        words.push(HSTRING::from("-no-warnings"));
        let pointers: Vec<PCWSTR> = words.iter().map(|word| PCWSTR(word.as_ptr())).collect();
        let buffer = DxcBuffer {
            Ptr: source.as_ptr().cast(),
            Size: source.len(),
            Encoding: DXC_CP_UTF8.0,
        };
        // SAFETY: plain calls on the compiler and its result. The source and the
        // arguments outlive the call, and each blob is read inside its own size.
        unsafe {
            let result: IDxcResult = compiler
                .Compile(&buffer, Some(&pointers), None)
                .map_err(|error| format!("the compiler did not run: {error}"))?;
            let status = result.GetStatus().map_err(|error| error.to_string())?;
            if status.is_err() {
                let mut said: Option<IDxcBlobUtf8> = None;
                let _ = result.GetOutput(DXC_OUT_ERRORS, std::ptr::null_mut(), &mut said);
                let text = said
                    .map(|said| {
                        let bytes = std::slice::from_raw_parts(
                            said.GetStringPointer().0,
                            said.GetStringLength(),
                        );
                        String::from_utf8_lossy(bytes).into_owned()
                    })
                    .unwrap_or_else(|| format!("{status:?}"));
                return Err(text);
            }
            let mut object: Option<IDxcBlob> = None;
            result
                .GetOutput(DXC_OUT_OBJECT, std::ptr::null_mut(), &mut object)
                .map_err(|error| error.to_string())?;
            let object = object.ok_or("the compiler gave no program")?;
            Ok(std::slice::from_raw_parts(
                object.GetBufferPointer().cast::<u8>(),
                object.GetBufferSize(),
            )
            .to_vec())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newest_version_of_a_folder_comes_first() {
        let folder = std::env::temp_dir().join(format!("riptide-dxc-{}", std::process::id()));
        for version in ["10.0.19041.0", "10.0.26100.0", "10.0.22621.0"] {
            std::fs::create_dir_all(folder.join(version)).unwrap();
        }
        let names: Vec<String> = newest_first(&folder)
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["10.0.26100.0", "10.0.22621.0", "10.0.19041.0"]);
        std::fs::remove_dir_all(&folder).unwrap();
        // A folder that is not there holds nothing.
        assert!(newest_first(&folder).is_empty());
    }

    #[test]
    fn a_shader_compiles_and_a_wrong_one_says_what_is_wrong() {
        // Only where a compiler is installed: it is not this project's to bring.
        if library().is_none() {
            return;
        }
        let good =
            "float4 main(uint id : SV_VertexID) : SV_Position { return float4(id, 0, 0, 1); }";
        let program = compile(good, "main", "vs_6_6", &[]).unwrap();
        assert_eq!(&program[..4], b"DXBC");
        let wrong = compile(
            "float4 main() : SV_Position { return nothing; }",
            "main",
            "vs_6_6",
            &[],
        )
        .unwrap_err();
        assert!(wrong.contains("nothing"), "{wrong}");
    }
}
