fn main() {
    // whisper.cpp's Metal backend (via whisper-rs-sys) uses ObjC `@available`,
    // which the compiler lowers to `___isPlatformVersionAtLeast` from clang's
    // compiler-rt. The final binary is linked with `-nodefaultlibs`, so on macOS
    // runners whose SDK doesn't carry that symbol in libSystem the link fails
    // ("Undefined symbols: ___isPlatformVersionAtLeast"). Link clang_rt
    // explicitly to resolve it. Harmless when the symbol is already available
    // (the archive member is only pulled in on demand).
    #[cfg(target_os = "macos")]
    link_clang_rt_osx();

    tauri_build::build()
}

#[cfg(target_os = "macos")]
fn link_clang_rt_osx() {
    let Ok(output) = std::process::Command::new("clang")
        .arg("--print-resource-dir")
        .output()
    else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
    println!("cargo:rustc-link-search=native={dir}/lib/darwin");
    println!("cargo:rustc-link-lib=static=clang_rt.osx");
}
