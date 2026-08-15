//! Embeds the Win32 application manifest and icon into the executable.
//!
//! The manifest is what makes the single-file release self-sufficient in
//! practice: it requests `requireAdministrator`, so double-clicking the exe
//! raises the UAC prompt instead of launching an unprivileged process that
//! can only report that packet capture needs elevation.
//!
//! The icon is resource id 2, which `platform::tray_icon_handle` loads for the
//! notification area and which Explorer and the taskbar pick up on their own.

fn main() {
    println!("cargo:rerun-if-changed=shinra-bpsr.rc");
    println!("cargo:rerun-if-changed=shinra-bpsr.manifest");
    println!("cargo:rerun-if-changed=assets/shinra.ico");

    // Only Windows binaries have a resource section; on a host build (the
    // unit tests, `cargo check --workspace`) there is nothing to embed.
    if std::env::var("CARGO_CFG_WINDOWS").is_err() {
        return;
    }

    // `manifest_required` fails the build rather than silently producing an
    // executable that never elevates — a UAC prompt that quietly went missing
    // would surface much later as an unexplained capture failure.
    embed_resource::compile("shinra-bpsr.rc", embed_resource::NONE)
        .manifest_required()
        .expect("embedding the application manifest");
}
