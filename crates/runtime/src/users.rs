//! The host's user names (D-138): the local `passwd(5)` file, read once at
//! startup and bounded. Only the file is read: the name service
//! (`getpwuid`), which may ask the network, is never called.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use dgxtop_core::users::UserNames;

const PASSWD_PATH: &str = "etc/passwd";
/// A passwd file is a few kilobytes; the cap bounds a generated one.
const MAX_PASSWD_BYTES: usize = 1024 * 1024;

/// The names `/etc/passwd` lists; none when it cannot be read, so every
/// process owner keeps its uid.
pub fn local_user_names() -> UserNames {
    user_names_at(Path::new("/"))
}

/// [`local_user_names`] with `/etc` resolved under `root`.
pub fn user_names_at(root: &Path) -> UserNames {
    read_bounded(&root.join(PASSWD_PATH), MAX_PASSWD_BYTES)
        .map(|bytes| UserNames::from_passwd(&String::from_utf8_lossy(&bytes)))
        .unwrap_or_default()
}

/// The first `limit` bytes of `path`, without the line the limit cut: half
/// an entry could name another uid.
fn read_bounded(path: &Path, limit: usize) -> Option<Vec<u8>> {
    let file = File::open(path).ok()?;
    let mut bytes = Vec::new();
    let cap = u64::try_from(limit).unwrap_or(u64::MAX);
    file.take(cap).read_to_end(&mut bytes).ok()?;
    if bytes.len() >= limit {
        let whole_lines = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |at| at + 1);
        bytes.truncate(whole_lines);
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// A throw-away root with an `etc/passwd` of `contents`.
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn with_passwd(label: &str, contents: &[u8]) -> Self {
            let root = std::env::temp_dir().join(format!(
                "dgxtop-runtime-users-{label}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("etc")).unwrap();
            fs::write(root.join(PASSWD_PATH), contents).unwrap();
            Self(root)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn names_come_from_the_local_passwd_file() {
        let root = TempRoot::with_passwd(
            "names",
            b"root:x:0:0:root:/root:/bin/bash\nalice:x:1000:1000::/home/alice:/bin/zsh\n",
        );
        let users = user_names_at(&root.0);
        assert_eq!(users.name_of(1000), Some("alice"));
        assert_eq!(users.name_of(0), Some("root"));
        assert_eq!(users.name_of(1001), None);
    }

    /// Without a readable file nobody is named, and a line that is not
    /// UTF-8 costs only its own name.
    #[test]
    fn a_missing_file_names_nobody_and_a_bad_line_only_itself() {
        let missing = std::env::temp_dir().join("dgxtop-runtime-users-none");
        assert!(user_names_at(&missing).is_empty());
        let root = TempRoot::with_passwd(
            "bytes",
            b"bad\xff\xfe:x:5:5::/:/bin/sh\nbob:x:1001:1001::/home/bob:/bin/bash\n",
        );
        let users = user_names_at(&root.0);
        assert_eq!(users.name_of(5), None);
        assert_eq!(users.name_of(1001), Some("bob"));
    }

    /// A file longer than the cap loses the entry the cap cut, never half
    /// of it: `carol:x:1002` cut to `carol:x:10` would name uid 10.
    #[test]
    fn the_line_the_limit_cuts_is_dropped() {
        let text = b"alice:x:1000:1000::/home/alice:/bin/zsh\ncarol:x:1002:1002::/h:/bin/sh\n";
        let root = TempRoot::with_passwd("cut", text);
        let path = root.0.join(PASSWD_PATH);
        let cut = text.iter().position(|byte| *byte == b'\n').unwrap() + 1 + "carol:x:10".len();
        let bytes = read_bounded(&path, cut).unwrap();
        let users = UserNames::from_passwd(&String::from_utf8_lossy(&bytes));
        assert_eq!(users.len(), 1);
        assert_eq!(users.name_of(1000), Some("alice"));
        assert_eq!(users.name_of(10), None);
        // Within the limit every line stands.
        let whole = read_bounded(&path, text.len() + 1).unwrap();
        assert_eq!(whole, text);
    }
}
