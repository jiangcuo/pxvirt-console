use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PXVIRT_GIT_REV");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let pkg_version = env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");

    let git_rev = env::var("PXVIRT_GIT_REV")
        .ok()
        .and_then(normalize_rev)
        .or_else(|| read_git_revision_file(&manifest_dir))
        .or_else(|| git_rev_from_repo(&manifest_dir));

    let app_version = match git_rev {
        Some(rev) => format!("{}+{}", pkg_version, rev),
        None => pkg_version,
    };

    println!("cargo:rustc-env=APP_VERSION={}", app_version);

    emit_git_rerun_paths(&manifest_dir);
}

fn normalize_rev(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn read_git_revision_file(manifest_dir: &Path) -> Option<String> {
    let path = manifest_dir.join(".git-revision");
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }

    fs::read_to_string(path).ok().and_then(normalize_rev)
}

fn git_rev_from_repo(manifest_dir: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .current_dir(manifest_dir)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    normalize_rev(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn emit_git_rerun_paths(manifest_dir: &Path) {
    let Some(git_dir) = resolve_git_dir(manifest_dir) else {
        return;
    };

    let head_path = git_dir.join("HEAD");
    if head_path.exists() {
        println!("cargo:rerun-if-changed={}", head_path.display());
    }

    let Ok(head_contents) = fs::read_to_string(&head_path) else {
        return;
    };

    let head_contents = head_contents.trim();
    if let Some(reference) = head_contents.strip_prefix("ref: ") {
        let ref_path = git_dir.join(reference);
        if ref_path.exists() {
            println!("cargo:rerun-if-changed={}", ref_path.display());
        } else {
            let packed_refs = git_dir.join("packed-refs");
            if packed_refs.exists() {
                println!("cargo:rerun-if-changed={}", packed_refs.display());
            }
        }
    }
}

fn resolve_git_dir(manifest_dir: &Path) -> Option<PathBuf> {
    let dot_git = manifest_dir.join(".git");

    if dot_git.is_dir() {
        return Some(dot_git);
    }

    let contents = fs::read_to_string(dot_git).ok()?;
    let gitdir = contents.trim().strip_prefix("gitdir: ")?.trim();
    let path = Path::new(gitdir);

    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        Some(manifest_dir.join(path))
    }
}
