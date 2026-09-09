fn main() {
    // Source clients pin a full committed fork revision, not a package version.
    println!("cargo:rerun-if-env-changed=FUNES_BUILD_REVISION");
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|out| out.status.success())
            .and_then(|out| String::from_utf8(out.stdout).ok())
            .map(|value| value.trim().to_owned())
    };
    for name in ["HEAD", "packed-refs"] {
        if let Some(path) = git(&["rev-parse", "--git-path", name]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"]) {
        if let Some(path) = git(&["rev-parse", "--git-path", &reference]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    let revision = git(&["rev-parse", "--verify", "HEAD"])
        .or_else(|| std::env::var("FUNES_BUILD_REVISION").ok())
        .expect("build requires Git HEAD or FUNES_BUILD_REVISION");
    assert!(
        revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "FUNES_BUILD_REVISION must be a full 40-character committed SHA"
    );
    println!("cargo:rustc-env=FUNES_BUILD_REVISION={}", revision.to_ascii_lowercase());
    // The BLAS backend's macOS seam calls Accelerate (cblas_sgemm + vForce). Link the framework
    // only when that backend is actually compiled: feature on AND target is macOS. A default build
    // (feature off) links nothing here; the Linux seam is pure Rust (faer) and needs no link.
    let blas = std::env::var("CARGO_FEATURE_BLAS").is_ok();
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if blas && target_os == "macos" {
        println!("cargo:rustc-link-lib=framework=Accelerate");
    }
}
