use std::env;
use std::process::Command;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(distribute)");
    println!("cargo::rerun-if-changed=../../.git/HEAD");
    if env::var("PROFILE").as_deref() == Ok("distribute") {
        println!("cargo::rustc-cfg=distribute");
    }
    let revision = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|revision| revision.trim().to_owned())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo::rustc-env=JOEDLNA_GIT_REVISION={revision}");
}
