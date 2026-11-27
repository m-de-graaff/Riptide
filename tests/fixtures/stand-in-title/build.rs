//! Links the stand-in title as a freestanding image: our own entry point and no C runtime,
//! so every import in the image is one the source names.

fn main() {
    for arg in ["/ENTRY:title_entry", "/NODEFAULTLIB", "/SUBSYSTEM:CONSOLE"] {
        println!("cargo:rustc-link-arg-bins={arg}");
    }
}
