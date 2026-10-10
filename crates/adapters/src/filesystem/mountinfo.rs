//! `/proc/self/mountinfo` parsing (octal escapes such as `\040` decoded)
//! and mount classification. Network filesystems are never probed, so a hung
//! server can never block a capacity read.

use std::collections::BTreeMap;

/// One mountinfo line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MountEntry {
    pub mount_id: u64,
    pub parent_id: u64,
    pub major: u32,
    pub minor: u32,
    /// Root of the mount inside its filesystem (differs for bind mounts).
    pub root: String,
    pub mount_point: String,
    pub fs_type: String,
    /// Mount source (`/dev/nvme0n1p2`, `server:/export`, `none`, ...).
    pub source: String,
}

impl MountEntry {
    /// Identity-defining facts: a reused mount id with other facts is a new
    /// incarnation.
    pub(crate) fn fingerprint(&self) -> String {
        format!(
            "{}:{}|{}|{}|{}|{}",
            self.major, self.minor, self.root, self.mount_point, self.fs_type, self.source
        )
    }

    pub(crate) fn major_minor(&self) -> String {
        format!("{}:{}", self.major, self.minor)
    }
}

/// How the adapter treats a mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MountClass {
    /// Disk-backed filesystem whose capacity is read with statvfs.
    Capacity,
    /// Network filesystem: reported as unsupported, never probed.
    NetworkSkipped,
    /// Pseudo/virtual filesystem or squashfs (always full): not reported.
    Ignored,
}

/// Network and cluster filesystems whose statvfs can block on a server.
const NETWORK_FILESYSTEMS: &[&str] = &[
    "nfs",
    "nfs4",
    "cifs",
    "smb3",
    "smbfs",
    "ceph",
    "glusterfs",
    "9p",
    "afs",
    "lustre",
    "gpfs",
    "beegfs",
    "orangefs",
    "pvfs2",
    "ncpfs",
    "coda",
    "davfs",
    "fuse.sshfs",
    "fuse.glusterfs",
    "fuse.s3fs",
    "fuse.rclone",
    "fuse.gcsfuse",
    "fuse.juicefs",
    "fuse.cephfs",
];

/// Disk-backed filesystems whose mounts report an anonymous (major 0)
/// device number, e.g. btrfs subvolumes.
const ANONYMOUS_DEVICE_DISK_FILESYSTEMS: &[&str] = &["btrfs", "zfs", "bcachefs"];

pub(crate) fn classify(entry: &MountEntry) -> MountClass {
    let fs_type = entry.fs_type.as_str();
    if NETWORK_FILESYSTEMS.contains(&fs_type) {
        return MountClass::NetworkSkipped;
    }
    if fs_type == "squashfs" {
        return MountClass::Ignored;
    }
    if entry.major != 0 || ANONYMOUS_DEVICE_DISK_FILESYSTEMS.contains(&fs_type) {
        MountClass::Capacity
    } else {
        MountClass::Ignored
    }
}

/// Lines that parse; malformed lines are skipped.
pub(crate) fn parse_mountinfo(text: &str) -> Vec<MountEntry> {
    text.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<MountEntry> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let separator = fields
        .iter()
        .skip(6)
        .position(|field| *field == "-")?
        .checked_add(6)?;
    let (major, minor) = fields.get(2)?.split_once(':')?;
    Some(MountEntry {
        mount_id: fields.first()?.parse().ok()?,
        parent_id: fields.get(1)?.parse().ok()?,
        major: major.parse().ok()?,
        minor: minor.parse().ok()?,
        root: decode_octal_escapes(fields.get(3)?),
        mount_point: decode_octal_escapes(fields.get(4)?),
        fs_type: decode_octal_escapes(fields.get(separator + 1)?),
        source: decode_octal_escapes(fields.get(separator + 2)?),
    })
}

/// Decodes the kernel's `\ooo` escapes (space, tab, newline, backslash).
pub(crate) fn decode_octal_escapes(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match octal_escape(&bytes[index..]) {
            Some(byte) => {
                decoded.push(byte);
                index += 4;
            }
            None => {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// `\ooo` at the start of `bytes` as one byte value (≤ 0o377).
fn octal_escape(bytes: &[u8]) -> Option<u8> {
    let [b'\\', first, second, third, ..] = bytes else {
        return None;
    };
    let digits = [*first, *second, *third];
    if !digits.iter().all(|digit| (b'0'..=b'7').contains(digit)) {
        return None;
    }
    let value = digits
        .iter()
        .fold(0u32, |value, digit| value * 8 + u32::from(digit - b'0'));
    u8::try_from(value).ok()
}

/// Whether `ancestor` is `path` or one of its parent directories
/// (component-wise: `/mnt` contains `/mnt/data` but not `/mnt2`).
pub(crate) fn is_within(ancestor: &str, path: &str) -> bool {
    if ancestor == "/" || ancestor == path {
        return true;
    }
    path.strip_prefix(ancestor)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// A parsed mount table with the lookups the read path needs.
#[derive(Debug, Default)]
pub(crate) struct MountTable {
    entries: Vec<MountEntry>,
    by_id: BTreeMap<u64, usize>,
}

impl MountTable {
    pub(crate) fn new(entries: Vec<MountEntry>) -> Self {
        let by_id = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.mount_id, index))
            .collect();
        Self { entries, by_id }
    }

    pub(crate) fn get(&self, mount_id: u64) -> Option<&MountEntry> {
        self.entries.get(*self.by_id.get(&mount_id)?)
    }

    /// Another mount stacked on top of `entry` at the same mount point makes
    /// its path resolve to the upper filesystem.
    pub(crate) fn is_shadowed(&self, entry: &MountEntry) -> bool {
        self.entries.iter().any(|other| {
            other.parent_id == entry.mount_id
                && other.mount_id != entry.mount_id
                && other.mount_point == entry.mount_point
        })
    }

    /// Whether resolving `entry`'s mount point walks through a network
    /// filesystem (which could block).
    pub(crate) fn has_network_in_path(&self, entry: &MountEntry) -> bool {
        self.entries.iter().any(|other| {
            classify(other) == MountClass::NetworkSkipped
                && is_within(&other.mount_point, &entry.mount_point)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTINFO: &str = "\
69 1 254:2 / / rw,relatime shared:1 - ext4 /dev/mapper/vg-root rw,errors=remount-ro
36 69 0:6 / /dev rw,nosuid shared:2 - devtmpfs devtmpfs rw
80 69 259:5 / /mnt/my\\040disk rw,relatime shared:30 master:1 - ext4 /dev/nvme0n1p5 rw
81 69 0:50 / /data rw - nfs4 server:/export\\040x rw,vers=4.2
82 69 7:0 / /snap/core/1 ro - squashfs /dev/loop0 ro
83 69 0:31 /@home /home rw - btrfs /dev/nvme1n1p2 rw
84 80 259:5 /sub /mnt/my\\040disk/bind rw - ext4 /dev/nvme0n1p5 rw
garbage line
85 69 8:1 / /broken rw shared:9
";

    #[test]
    fn mountinfo_fields_and_escapes_decode() {
        let entries = parse_mountinfo(MOUNTINFO);
        assert_eq!(entries.len(), 7);
        let disk = &entries[2];
        assert_eq!(disk.mount_id, 80);
        assert_eq!(disk.parent_id, 69);
        assert_eq!((disk.major, disk.minor), (259, 5));
        assert_eq!(disk.mount_point, "/mnt/my disk");
        assert_eq!(disk.fs_type, "ext4");
        assert_eq!(disk.source, "/dev/nvme0n1p5");
        assert_eq!(entries[3].source, "server:/export x");
        assert_eq!(entries[6].root, "/sub");
    }

    #[test]
    fn classification_skips_network_pseudo_and_squashfs() {
        let entries = parse_mountinfo(MOUNTINFO);
        let classes: Vec<MountClass> = entries.iter().map(classify).collect();
        assert_eq!(
            classes,
            vec![
                MountClass::Capacity,
                MountClass::Ignored,
                MountClass::Capacity,
                MountClass::NetworkSkipped,
                MountClass::Ignored,
                MountClass::Capacity,
                MountClass::Capacity,
            ]
        );
    }

    #[test]
    fn octal_escapes_cover_space_tab_newline_and_backslash() {
        assert_eq!(
            decode_octal_escapes(r"a\040b\011c\012d\134e"),
            "a b\tc\nd\\e"
        );
        assert_eq!(decode_octal_escapes(r"keep\08x\777"), r"keep\08x\777");
        assert_eq!(decode_octal_escapes(r"trailing\04"), r"trailing\04");
    }

    #[test]
    fn path_containment_is_component_wise() {
        assert!(is_within("/mnt", "/mnt/data"));
        assert!(is_within("/mnt", "/mnt"));
        assert!(is_within("/", "/anything"));
        assert!(!is_within("/mnt", "/mnt2"));
        assert!(!is_within("/mnt/data", "/mnt"));
    }

    #[test]
    fn shadowing_and_network_paths_are_detected() {
        let mut entries = parse_mountinfo(MOUNTINFO);
        entries.push(MountEntry {
            mount_id: 90,
            parent_id: 80,
            major: 0,
            minor: 60,
            root: "/".to_owned(),
            mount_point: "/mnt/my disk".to_owned(),
            fs_type: "tmpfs".to_owned(),
            source: "tmpfs".to_owned(),
        });
        entries.push(MountEntry {
            mount_id: 91,
            parent_id: 81,
            major: 8,
            minor: 17,
            root: "/".to_owned(),
            mount_point: "/data/local".to_owned(),
            fs_type: "xfs".to_owned(),
            source: "/dev/sdb1".to_owned(),
        });
        let table = MountTable::new(entries);
        let shadowed = table.get(80).unwrap();
        assert!(table.is_shadowed(shadowed));
        assert!(!table.is_shadowed(table.get(69).unwrap()));
        assert!(table.has_network_in_path(table.get(91).unwrap()));
        assert!(!table.has_network_in_path(table.get(83).unwrap()));
    }
}
