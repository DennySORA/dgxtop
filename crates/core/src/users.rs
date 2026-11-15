//! The names of the host's users (D-138): uid to login name, as the host's
//! own `passwd(5)` file lists them. The composition root reads the file
//! once at startup; the Manager names a process's owner from the table and
//! falls back to the uid. Only the local file is ever consulted: a name
//! service that would ask the network (LDAP, SSSD) is never called, so a
//! user known only there keeps its number.

use std::collections::BTreeMap;

use crate::text::sanitize_display;

/// Users kept at most; a file listing more keeps its first ones.
pub const MAX_USERS: usize = 4096;
/// Longest login name kept (Linux allows 32 bytes).
pub const MAX_USER_NAME_BYTES: usize = 32;

/// Fields of a passwd line before the uid: `name:password:uid`.
const UID_FIELD: usize = 2;

/// The login name of each local uid.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UserNames {
    names: BTreeMap<u32, String>,
}

impl UserNames {
    /// The table of a `passwd(5)` text
    /// (`name:password:uid:gid:gecos:home:shell`). The first entry of a
    /// uid names it, as `getpwuid` answers; comments, malformed lines,
    /// names that do not survive sanitizing or are too long, and entries
    /// past [`MAX_USERS`] are left out.
    pub fn from_passwd(text: &str) -> Self {
        let mut names = BTreeMap::new();
        for line in text.lines() {
            if names.len() >= MAX_USERS {
                break;
            }
            let Some((uid, name)) = passwd_entry(line) else {
                continue;
            };
            names.entry(uid).or_insert(name);
        }
        Self { names }
    }

    /// The login name of `uid`; `None` for a uid the file does not list.
    pub fn name_of(&self, uid: u32) -> Option<&str> {
        self.names.get(&uid).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

/// `(uid, name)` of one passwd line.
fn passwd_entry(line: &str) -> Option<(u32, String)> {
    if line.starts_with('#') {
        return None;
    }
    let mut fields = line.split(':');
    let name = fields.next()?;
    let uid = fields.nth(UID_FIELD - 1)?.parse::<u32>().ok()?;
    let clean = sanitize_display(name, MAX_USER_NAME_BYTES);
    // A name that had to be cut or cleaned, or whose bytes were not text,
    // is not the user's name.
    let intact = clean == name && !name.contains(char::REPLACEMENT_CHARACTER);
    (!clean.is_empty() && intact).then_some((uid, clean))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "\
root:x:0:0:root:/root:/bin/bash
# a comment
daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin
alice:x:1000:1000:Alice,,,:/home/alice:/bin/zsh
toor:x:0:0:second name of uid 0:/root:/bin/sh
bob:x:1001:1001::/home/bob:/bin/bash
";

    #[test]
    fn names_follow_the_first_entry_of_each_uid() {
        let users = UserNames::from_passwd(PASSWD);
        assert_eq!(users.len(), 4);
        assert_eq!(users.name_of(0), Some("root"), "as getpwuid answers");
        assert_eq!(users.name_of(1000), Some("alice"));
        assert_eq!(users.name_of(1001), Some("bob"));
        assert_eq!(users.name_of(4242), None);
        assert!(UserNames::default().is_empty());
        assert_eq!(UserNames::from_passwd("").name_of(0), None);
    }

    /// A line that is not a passwd entry, a uid that is no number and a
    /// name unfit for display name nobody: the uid keeps its number.
    #[test]
    fn malformed_entries_and_unsafe_names_are_left_out() {
        let long = "n".repeat(MAX_USER_NAME_BYTES + 1);
        let text = format!(
            "nouid:x\n\
             text:x:abc:1::/:/bin/sh\n\
             negative:x:-1:1::/:/bin/sh\n\
             huge:x:4294967296:1::/:/bin/sh\n\
             :x:7:7::/:/bin/sh\n\
             bad\u{1b}[31mname:x:8:8::/:/bin/sh\n\
             {long}:x:9:9::/:/bin/sh\n\
             max:x:4294967295:1::/:/bin/sh\n\
             ok:x:10:10::/:/bin/sh\n"
        );
        let users = UserNames::from_passwd(&text);
        for uid in [7, 8, 9] {
            assert_eq!(users.name_of(uid), None, "{uid}");
        }
        assert_eq!(users.name_of(10), Some("ok"));
        assert_eq!(users.name_of(u32::MAX), Some("max"));
        assert_eq!(users.len(), 2);
    }

    #[test]
    fn the_table_is_bounded() {
        let text: String = (0..MAX_USERS + 50)
            .map(|uid| format!("user{uid}:x:{uid}:{uid}::/:/bin/sh\n"))
            .collect();
        let users = UserNames::from_passwd(&text);
        assert_eq!(users.len(), MAX_USERS);
        assert_eq!(users.name_of(0), Some("user0"));
        let last = u32::try_from(MAX_USERS).unwrap();
        assert_eq!(users.name_of(last), None, "past the bound");
    }
}
