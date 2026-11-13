//! Embeds the application icon and the Windows application manifest (Common Controls v6, Windows
//! 10/11 compatibility, long paths) into ferrous.exe.

fn main() {
    println!("cargo:rerun-if-changed=windows/ferrous.rc");
    println!("cargo:rerun-if-changed=windows/ferrous.manifest");
    println!("cargo:rerun-if-changed=windows/ferrous.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("windows/ferrous.rc", embed_resource::NONE).manifest_required().unwrap();
    }
}
