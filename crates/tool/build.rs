// Copyright The eha-sdk Contributors

//! `eha-tool` 自身的构建身份。
//!
//! 构建证据必须反映本次构建时的 Git 状态。Cargo 的通常输入追踪不包含
//! 未跟踪文件和已还原的工作区修改，因此故意登记一个永不存在的 `OUT_DIR`
//! 路径，让构建脚本在每次 Cargo 调用时重新取得这些事实；不扫描 `target/`。

use std::{
    env,
    error::Error,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn main() -> Result<(), Box<dyn Error>> {
    emit_rerun_rules()?;

    let version = env::var("CARGO_PKG_VERSION")?;
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let source = source_state(&manifest_dir);
    let tool_version = format!(
        "{version} (source={}; tree={}; target={}; profile={})",
        source.revision_or_unknown(),
        source.tree,
        env::var("TARGET")?,
        env::var("PROFILE")?,
    );
    println!("cargo:rustc-env=EHA_TOOL_VERSION={tool_version}");
    Ok(())
}

fn emit_rerun_rules() -> Result<(), Box<dyn Error>> {
    for variable in ["CARGO_PKG_VERSION", "PROFILE", "TARGET"] {
        println!("cargo:rerun-if-env-changed={variable}");
    }

    let out_dir = env::var("OUT_DIR")?;
    println!(
        "cargo:rerun-if-changed={}/eha-build-metadata-must-not-exist",
        out_dir
    );
    Ok(())
}

fn source_state(manifest_dir: &Path) -> SourceState {
    let Some(workspace_root) = manifest_dir.parent().and_then(Path::parent) else {
        return SourceState::unknown();
    };
    let Ok(expected_root) = fs::canonicalize(workspace_root) else {
        return SourceState::unknown();
    };
    let Ok(root) = git_output(&expected_root, ["rev-parse", "--show-toplevel"]) else {
        return SourceState::unknown();
    };
    let root = String::from_utf8_lossy(&root.stdout).trim().to_owned();
    let Ok(root) = fs::canonicalize(root) else {
        return SourceState::unknown();
    };
    if root != expected_root {
        return SourceState::unknown();
    }

    let Ok(revision) = git_output(&root, ["rev-parse", "--verify", "HEAD"]) else {
        return SourceState::unknown();
    };
    let revision = String::from_utf8_lossy(&revision.stdout).trim().to_owned();
    if revision.is_empty() {
        return SourceState::unknown();
    }
    let tree = git_output(
        &root,
        [
            "status",
            "--porcelain=v1",
            "--untracked-files=normal",
            "--ignore-submodules=none",
        ],
    )
    .ok()
    .map(|output| {
        if output.stdout.is_empty() {
            "clean"
        } else {
            "dirty"
        }
    })
    .unwrap_or("unknown");

    SourceState { revision, tree }
}

fn git_output<const N: usize>(directory: &Path, arguments: [&str; N]) -> Result<Output, ()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .output()
        .map_err(|_| ())?;
    output.status.success().then_some(output).ok_or(())
}

struct SourceState {
    revision: String,
    tree: &'static str,
}

impl SourceState {
    fn unknown() -> Self {
        Self {
            revision: String::new(),
            tree: "unknown",
        }
    }

    fn revision_or_unknown(&self) -> &str {
        if self.revision.is_empty() {
            "unknown"
        } else {
            &self.revision
        }
    }
}
