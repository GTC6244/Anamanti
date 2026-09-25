//! Build script: capture git + build metadata for the config page's About tab so a
//! running orchestrator can report exactly which build it is.

use std::process::Command;

fn main() {
    let git = |args: &[&str]| -> String {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    };

    let sha = git(&["rev-parse", "--short", "HEAD"]);
    let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"]);
    let dirty = if !git(&["status", "--porcelain"]).is_empty() {
        "-dirty"
    } else {
        ""
    };
    let build_time = Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=ANAMANTI_GIT_SHA={sha}{dirty}");
    println!("cargo:rustc-env=ANAMANTI_GIT_BRANCH={branch}");
    println!("cargo:rustc-env=ANAMANTI_BUILD_TIME={build_time}");

    // Rebuild when the checked-out commit changes so the SHA stays accurate.
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=.git/HEAD");
}
