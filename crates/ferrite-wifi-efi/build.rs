//! Link flags for an image that `ferrite-dev` converts from ELF to PE: the firmware enters at
//! `efi_main`, and segments must start on their own 4 KiB pages to become PE sections.

fn main() {
    println!("cargo:rustc-link-arg-bins=--entry=efi_main");
    println!("cargo:rustc-link-arg-bins=-zmax-page-size=4096");
}
