//! GPU state shared by the NVML adapter and the Viewer (D-47): the reasons
//! the driver holds the clocks below their target ("throttle reasons"), by
//! their public NVML clocks-event bit values, and how each GPU reaches the
//! others (D-54: the PCIe path and direct peer access), so every layer
//! reads one meaning.

/// How much a throttle reason matters to someone reading the GPU view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ThrottleSeverity {
    /// Expected (no work, a configured clock).
    Benign,
    /// Clocks capped by the power budget or a setting worth knowing.
    Notice,
    /// Slowed down by heat or a hardware signal.
    Alert,
}

/// One throttle reason: its NVML bit and its words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThrottleReason {
    pub bit: u64,
    pub label: &'static str,
    pub severity: ThrottleSeverity,
}

/// Every reason NVML reports, in bit order.
pub const THROTTLE_REASONS: [ThrottleReason; 9] = [
    reason(0x1, "idle", ThrottleSeverity::Benign),
    reason(0x2, "application clocks", ThrottleSeverity::Benign),
    reason(0x4, "power cap", ThrottleSeverity::Notice),
    reason(0x8, "hardware slowdown", ThrottleSeverity::Alert),
    reason(0x10, "sync boost", ThrottleSeverity::Benign),
    reason(0x20, "thermal slowdown", ThrottleSeverity::Alert),
    reason(0x40, "hardware thermal slowdown", ThrottleSeverity::Alert),
    reason(0x80, "power brake", ThrottleSeverity::Alert),
    reason(0x100, "display clocks", ThrottleSeverity::Benign),
];

const fn reason(bit: u64, label: &'static str, severity: ThrottleSeverity) -> ThrottleReason {
    ThrottleReason {
        bit,
        label,
        severity,
    }
}

/// The known reasons set in `bits`, in bit order; unknown bits are ignored.
pub fn throttle_reasons(bits: u64) -> impl Iterator<Item = &'static ThrottleReason> {
    THROTTLE_REASONS
        .iter()
        .filter(move |reason| bits & reason.bit != 0)
}

/// The most serious severity among `bits`; `None` when no reason is set.
pub fn throttle_severity(bits: u64) -> Option<ThrottleSeverity> {
    throttle_reasons(bits).map(|reason| reason.severity).max()
}

/// How the PCIe fabric joins two GPUs, nearest first: NVML's topology
/// levels, which `nvidia-smi topo -m` prints as `PIX` to `SYS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GpuLink {
    /// Both on one board (a dual-GPU card).
    SameBoard,
    /// Through one PCIe switch.
    OneSwitch,
    /// Through several PCIe switches, not the host bridge.
    Switches,
    /// Through the CPU's PCIe host bridge.
    HostBridge,
    /// Between host bridges of one NUMA node.
    NumaNode,
    /// Across NUMA nodes, over the CPU interconnect.
    System,
}

impl GpuLink {
    const ALL: [Self; 6] = [
        Self::SameBoard,
        Self::OneSwitch,
        Self::Switches,
        Self::HostBridge,
        Self::NumaNode,
        Self::System,
    ];

    /// `nvidia-smi topo -m`'s name of the path.
    pub const fn code(self) -> &'static str {
        match self {
            Self::SameBoard => "BOARD",
            Self::OneSwitch => "PIX",
            Self::Switches => "PXB",
            Self::HostBridge => "PHB",
            Self::NumaNode => "NODE",
            Self::System => "SYS",
        }
    }

    /// What the path goes through, in words.
    pub const fn label(self) -> &'static str {
        match self {
            Self::SameBoard => "same board",
            Self::OneSwitch => "one PCIe switch",
            Self::Switches => "PCIe switches",
            Self::HostBridge => "host bridge",
            Self::NumaNode => "host bridges of one NUMA node",
            Self::System => "across NUMA nodes",
        }
    }

    fn parse(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|link| link.code() == code)
    }
}

/// Whether a GPU may read and write another GPU's memory directly (P2P);
/// without it, copies between them go through system memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerAccess {
    pub read: bool,
    pub write: bool,
}

impl PeerAccess {
    const fn code(self) -> &'static str {
        match (self.read, self.write) {
            (true, true) => "rw",
            (true, false) => "r",
            (false, true) => "w",
            (false, false) => "none",
        }
    }

    fn parse(code: &str) -> Option<Self> {
        let (read, write) = match code {
            "rw" => (true, true),
            "r" => (true, false),
            "w" => (false, true),
            "none" => (false, false),
            _ => return None,
        };
        Some(Self { read, write })
    }

    /// `P2P read/write`, `P2P read only`, `no P2P`.
    pub const fn label(self) -> &'static str {
        match (self.read, self.write) {
            (true, true) => "P2P read/write",
            (true, false) => "P2P read only",
            (false, true) => "P2P write only",
            (false, false) => "no P2P",
        }
    }
}

/// How one GPU reaches another, by the other's NVML index; a part the
/// driver could not answer is `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuPeer {
    pub index: u32,
    pub link: Option<GpuLink>,
    pub access: Option<PeerAccess>,
}

/// Unknown part of a peer entry.
const UNKNOWN_PART: &str = "?";

/// The entity attribute of a GPU's peers: `index:path:access` entries
/// separated by spaces (`1:PHB:none`), `?` for an unknown part.
pub fn format_peers(peers: &[GpuPeer]) -> String {
    let entries: Vec<String> = peers
        .iter()
        .map(|peer| {
            format!(
                "{}:{}:{}",
                peer.index,
                peer.link.map_or(UNKNOWN_PART, GpuLink::code),
                peer.access.map_or(UNKNOWN_PART, PeerAccess::code)
            )
        })
        .collect();
    entries.join(" ")
}

/// The peers of [`format_peers`]' attribute; an entry that is not
/// `index:path:access` (one cut by the attribute's length bound) is
/// skipped.
pub fn parse_peers(attribute: &str) -> Vec<GpuPeer> {
    attribute
        .split_whitespace()
        .filter_map(|entry| {
            let mut parts = entry.split(':');
            let index = parts.next()?.parse().ok()?;
            let link = parts.next()?;
            let access = parts.next()?;
            if parts.next().is_some() {
                return None;
            }
            let link = known(link, GpuLink::parse)?;
            let access = known(access, PeerAccess::parse)?;
            Some(GpuPeer {
                index,
                link,
                access,
            })
        })
        .collect()
}

/// `Some(None)` for the unknown part, `None` for an invalid one.
fn known<T>(part: &str, parse: impl FnOnce(&str) -> Option<T>) -> Option<Option<T>> {
    if part == UNKNOWN_PART {
        Some(None)
    } else {
        parse(part).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peers_round_trip_and_bad_entries_are_skipped() {
        let peers = [
            GpuPeer {
                index: 1,
                link: Some(GpuLink::HostBridge),
                access: Some(PeerAccess {
                    read: false,
                    write: false,
                }),
            },
            GpuPeer {
                index: 2,
                link: None,
                access: Some(PeerAccess {
                    read: true,
                    write: true,
                }),
            },
        ];
        let attribute = format_peers(&peers);
        assert_eq!(attribute, "1:PHB:none 2:?:rw");
        assert_eq!(parse_peers(&attribute), peers);
        // A cut entry, an unknown code and a missing part are skipped.
        assert_eq!(
            parse_peers("1:PHB:none 2:PIX 3:LAN:rw x:SYS:r 4:SYS:r:extra 5:SYS:r"),
            vec![
                peers[0],
                GpuPeer {
                    index: 5,
                    link: Some(GpuLink::System),
                    access: Some(PeerAccess {
                        read: true,
                        write: false,
                    }),
                },
            ]
        );
        assert!(parse_peers("").is_empty());
    }

    #[test]
    fn bits_decode_to_reasons_in_order() {
        let labels: Vec<&str> = throttle_reasons(0x4 | 0x40 | 0x1000)
            .map(|reason| reason.label)
            .collect();
        assert_eq!(labels, vec!["power cap", "hardware thermal slowdown"]);
        assert_eq!(throttle_reasons(0).count(), 0);
        let mut bits: Vec<u64> = THROTTLE_REASONS.iter().map(|reason| reason.bit).collect();
        bits.dedup();
        assert_eq!(bits.len(), 9, "every reason has its own bit");
    }

    #[test]
    fn the_worst_reason_sets_the_severity() {
        assert_eq!(throttle_severity(0), None);
        assert_eq!(throttle_severity(0x1), Some(ThrottleSeverity::Benign));
        assert_eq!(throttle_severity(0x1 | 0x4), Some(ThrottleSeverity::Notice));
        assert_eq!(throttle_severity(0x4 | 0x20), Some(ThrottleSeverity::Alert));
    }
}
