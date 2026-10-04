fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src");
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M");
    println!("cargo:rustc-env=ELLS_BUILD_TIME={now}");
}
