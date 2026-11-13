//! Link flags for an image that `ferrite-dev` converts from ELF to PE: a static PIE with no libc, entered
//! at `efi_main`, whose segments start on their own 4 KiB pages to become PE sections.

fn main() {
    if std::env::var_os("CARGO_CFG_FERRITE_APP").is_none() {
        return;
    }
    for arg in ["-fuse-ld=lld", "-nostartfiles", "-nostdlib", "-Wl,--entry=efi_main", "-Wl,-zmax-page-size=4096"] {
        println!("cargo:rustc-link-arg-bins={arg}");
    }
}
