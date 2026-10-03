fn main() {
    cc::Build::new()
        .file("vpp-parameters.c")
        .compile("screen_vpp_parameters");
    println!("cargo:rerun-if-changed=vpp-parameters.c");
    println!("cargo:rustc-link-lib=EGL");
    println!("cargo:rustc-link-lib=GLESv2");
    println!("cargo:rustc-link-lib=wayland-egl");
}
