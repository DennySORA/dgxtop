//! Build provenance rules shared by `build.rs` (included with `#[path]`) and
//! its tests: when the embedded commit counts as dirty, and which paths must
//! re-run the build script so the label never goes stale.

/// `git status` arguments for the dirty check. Untracked files count (a new
/// source file changes the binary); ignored files do not.
pub const STATUS_ARGS: [&str; 2] = ["status", "--porcelain"];

/// Whether `git status --porcelain` output describes a tree that differs
/// from the commit.
pub fn is_dirty(porcelain: &str) -> bool {
    porcelain.lines().any(|line| !line.trim().is_empty())
}

/// Build inputs relative to the build script. An unstaged edit or a new file
/// touches none of git's own files, so these are watched directly.
pub const BUILD_INPUTS: [&str; 3] = ["../../crates", "../../Cargo.toml", "../../Cargo.lock"];

/// Paths whose change must re-run the build script: the build inputs and
/// git's commit pointers (`HEAD`, the index, the checked-out branch ref and
/// `packed-refs`). A path that does not exist is left out, because Cargo
/// would otherwise re-run the script (and rebuild the crate) on every build.
pub fn rerun_paths(
    git_dir: Option<&str>,
    head_ref: Option<&str>,
    exists: impl Fn(&str) -> bool,
) -> Vec<String> {
    let mut paths: Vec<String> = BUILD_INPUTS.iter().map(|path| (*path).to_owned()).collect();
    if let Some(git_dir) = git_dir {
        let pointers = ["HEAD", "index", "packed-refs"].into_iter().chain(head_ref);
        paths.extend(pointers.map(|name| format!("{git_dir}/{name}")));
    }
    paths.retain(|path| exists(path));
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untracked_and_modified_files_make_the_build_dirty() {
        assert!(is_dirty("?? crates/manager/src/new_module.rs\n"));
        assert!(is_dirty(" M crates/core/src/metric.rs\n"));
        assert!(!is_dirty(""));
        assert!(!is_dirty("\n"));
        assert!(
            !STATUS_ARGS.contains(&"--untracked-files=no"),
            "untracked files must count"
        );
    }

    #[test]
    fn unstaged_edits_and_branch_moves_re_run_the_build_script() {
        let everything = rerun_paths(Some("/repo/.git"), Some("refs/heads/main"), |_| true);
        for expected in [
            "../../crates",
            "../../Cargo.toml",
            "../../Cargo.lock",
            "/repo/.git/HEAD",
            "/repo/.git/index",
            "/repo/.git/packed-refs",
            "/repo/.git/refs/heads/main",
        ] {
            assert!(everything.iter().any(|path| path == expected), "{expected}");
        }
        // A detached HEAD has no branch ref; missing files are not watched.
        let detached = rerun_paths(Some("/repo/.git"), None, |path| {
            !path.ends_with("packed-refs")
        });
        assert!(!detached.iter().any(|path| path.contains("refs/")));
        assert!(!detached.iter().any(|path| path.ends_with("packed-refs")));
        // Outside a repository only the build inputs are watched.
        assert_eq!(rerun_paths(None, None, |_| true).len(), BUILD_INPUTS.len());
    }
}
