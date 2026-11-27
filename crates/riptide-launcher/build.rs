//! Builds the launcher's icon and version information into its .exe, so Explorer, the
//! taskbar and the file's Properties page show them.

fn main() {
    println!("cargo:rerun-if-changed=assets/riptide.ico");
    println!("cargo:rerun-if-changed=build.rs");

    let mut resource = winresource::WindowsResource::new();
    resource
        .set_icon("assets/riptide.ico")
        .set("ProductName", "Riptide")
        .set("FileDescription", "Riptide launcher")
        .set("OriginalFilename", "riptide-launcher.exe")
        .set("InternalName", "riptide-launcher");
    // The resource compiler comes with the Windows SDK. A machine without it still builds
    // a launcher that works; it only lacks the icon and the version page.
    if let Err(error) = resource.compile() {
        println!("cargo:warning=the launcher has no icon or version information: {error}");
    }
}
