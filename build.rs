use std::process::Command;

fn env_trimmed(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
        .filter(|v| !v.is_empty())
}

fn run_git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value = stdout.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=APP_NAME");
    println!("cargo:rerun-if-env-changed=APP_BUILD");
    println!("cargo:rerun-if-env-changed=APP_COMMIT");

    if let Some(name) = env_trimmed("APP_NAME") {
        println!("cargo:rustc-env=APP_NAME={}", name);
    }

    let build =
        env_trimmed("APP_BUILD").or_else(|| run_git(&["describe", "--tags", "--exact-match"]));
    if let Some(build) = build {
        println!("cargo:rustc-env=APP_BUILD={}", build);
    }

    let commit = env_trimmed("APP_COMMIT").or_else(|| run_git(&["rev-parse", "HEAD"]));
    if let Some(commit) = commit {
        println!("cargo:rustc-env=APP_COMMIT={}", commit);
    }
}
