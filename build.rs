fn git(args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn main() {
    // 版本号 = 提交计数 + 完整 commit id；无 .git 的构建环境回退占位值。
    let count = git(&["rev-list", "--count", "HEAD"]).unwrap_or_else(|| "0".into());
    let sha = git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=HARUKA_GIT_COUNT={count}");
    println!("cargo:rustc-env=HARUKA_GIT_SHA={sha}");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");
    println!("cargo:rerun-if-env-changed=HARUKA_GIT_COUNT");
}
