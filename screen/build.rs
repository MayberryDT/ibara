fn main() {
    let core=std::fs::read_to_string("../Cargo.toml").expect("read core package version");
    let version=core.lines().find_map(|line|line.strip_prefix("version = \"" ).and_then(|v|v.strip_suffix('"'))).expect("core package version");
    println!("cargo:rustc-env=IBARA_CORE_VERSION={version}");
    println!("cargo:rerun-if-changed=../Cargo.toml");
    cc::Build::new()
        .file("vpp-parameters.c")
        .compile("screen_vpp_parameters");
    println!("cargo:rerun-if-changed=vpp-parameters.c");
    println!("cargo:rustc-link-lib=EGL");
    println!("cargo:rustc-link-lib=GLESv2");
    println!("cargo:rustc-link-lib=wayland-egl");
}
