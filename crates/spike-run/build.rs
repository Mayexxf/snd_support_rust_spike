//! Stamp the build with the commit it came from.
//!
//! Not vanity. The harness is copied between a development Mac, a VM and the
//! target machine, and the numbers it prints are the deliverable — so "which
//! build produced this?" is a question that will be asked about every table of
//! results. It has already been asked once, when a fix looked like it had not
//! worked and the answer was that the old binary had been run.
//!
//! On the target machine, where there is one trip to spend, guessing at that is
//! not affordable.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");

    let hash = run(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "без git".to_owned());

    // Uncommitted changes matter more than the hash: a stamp that says a commit
    // while the working tree says something else is worse than no stamp.
    let dirty = run(&["status", "--porcelain"]).is_some_and(|s| !s.trim().is_empty());
    let stamp = if dirty { format!("{hash}+правки") } else { hash };

    println!("cargo:rustc-env=SPIKE_BUILD={stamp}");
}

fn run(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}
