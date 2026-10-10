//! `/sys/class/net` layout: interface discovery (ifindex, operstate,
//! loopback type) and the per-interface counter read. The ifindex is checked
//! before and after reading the counters, so counters of a re-created
//! interface are never attributed to the old entity.

use std::io;

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::{EntityId, Scope};
use dgxtop_core::metric::{ids, native};
use dgxtop_core::naming::{attributes, entity, groups};
use dgxtop_core::observation::{Attribute, EntityKind, EntityRecord, Observation, ReadPlan};
use dgxtop_core::quality::{Quality, RawKind, ReadStatus};

use crate::common::{SystemFiles, parse_u64};
use crate::procfs::catalog::{
    EntityParts, KeyContext, LiveSource, SnapshotBuilder, SourceDraft, SourceSpec, source_draft,
};
use crate::procfs::mechanics::{ReprobeGate, TimedRead, invariant, io_failure, timed};
use crate::procfs::records::{
    PARSE_ERROR, ReadWindow, deferred_record, failed_record, u64_text_record,
};

pub(crate) const NET_CLASS: &str = "/sys/class/net";
pub(crate) const FAMILY_INTERFACE: &str = "interface";
const DRIVER: &str = "sysfs-net";
/// `ARPHRD_LOOPBACK` in `/sys/class/net/<if>/type`.
const ARPHRD_LOOPBACK: u64 = 772;
/// Entity attribute marking the loopback interface (presentation may hide it).
pub const LOOPBACK_ATTRIBUTE: &str = "loopback";
const INTERFACE_RECREATED: &str = "INTERFACE_RECREATED";
/// `ENOTDIR`: a plain file such as `bonding_masters` is not an interface.
const ENOTDIR: i32 = 20;
/// IFNAMSIZ is 16 including the terminator.
const MAX_IFNAME_BYTES: usize = 15;

/// The kernel counters emitted per interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NetCounter {
    RxBytes,
    TxBytes,
    RxPackets,
    TxPackets,
    RxErrors,
    TxErrors,
    RxDropped,
    TxDropped,
}

const NET_COUNTERS: [NetCounter; 8] = [
    NetCounter::RxBytes,
    NetCounter::TxBytes,
    NetCounter::RxPackets,
    NetCounter::TxPackets,
    NetCounter::RxErrors,
    NetCounter::TxErrors,
    NetCounter::RxDropped,
    NetCounter::TxDropped,
];

impl NetCounter {
    fn file(self) -> &'static str {
        match self {
            Self::RxBytes => "rx_bytes",
            Self::TxBytes => "tx_bytes",
            Self::RxPackets => "rx_packets",
            Self::TxPackets => "tx_packets",
            Self::RxErrors => "rx_errors",
            Self::TxErrors => "tx_errors",
            Self::RxDropped => "rx_dropped",
            Self::TxDropped => "tx_dropped",
        }
    }

    fn metric_id(self) -> &'static str {
        match self {
            Self::RxBytes => ids::NET_RX_BYTES,
            Self::TxBytes => ids::NET_TX_BYTES,
            Self::RxPackets => ids::NET_RX_PACKETS,
            Self::TxPackets => ids::NET_TX_PACKETS,
            Self::RxErrors => ids::NET_RX_ERRORS,
            Self::TxErrors => ids::NET_TX_ERRORS,
            Self::RxDropped => ids::NET_RX_DROPS,
            Self::TxDropped => ids::NET_TX_DROPS,
        }
    }

    fn native_unit(self) -> &'static str {
        match self {
            Self::RxBytes | Self::TxBytes => native::BYTES,
            _ => native::COUNT,
        }
    }
}

/// How the read path obtains one interface counter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NetLocator {
    pub ifname: String,
    pub ifindex: u32,
    counter: NetCounter,
}

/// Kernel interface names cannot contain `/`, `:` or whitespace; anything
/// else (and `.`/`..`) is refused so paths stay inside `/sys/class/net`.
fn valid_ifname(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_IFNAME_BYTES
        && name != "."
        && name != ".."
        && !name.chars().any(|character| {
            character == '/'
                || character == ':'
                || character.is_whitespace()
                || character.is_control()
        })
}

fn interface_path(ifname: &str, file: &str) -> String {
    format!("{NET_CLASS}/{ifname}/{file}")
}

/// Outcome of reading an interface's ifindex.
enum IfindexRead {
    Index(u32),
    /// Not an interface (plain file) or already gone.
    Absent,
    Unparsable,
    Failed(io::Error),
}

fn read_ifindex(files: &dyn SystemFiles, ifname: &str) -> IfindexRead {
    match files.read_to_string(&interface_path(ifname, "ifindex")) {
        Ok(text) => parse_u64(&text)
            .and_then(|index| u32::try_from(index).ok())
            .filter(|index| *index > 0)
            .map_or(IfindexRead::Unparsable, IfindexRead::Index),
        Err(error) if is_absent(&error) => IfindexRead::Absent,
        Err(error) => IfindexRead::Failed(error),
    }
}

fn is_absent(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(ENOTDIR)
}

/// Inputs of interface discovery besides the snapshot builder.
pub(crate) struct InterfaceDiscovery<'a> {
    pub keys: &'a KeyContext,
    pub files: &'a dyn SystemFiles,
    pub net_namespace: &'a str,
}

/// Adds one `/sys/class/net` entry. A transient failure carries the
/// previously known interface of that name over instead of removing it.
pub(crate) fn add_interface(
    builder: &mut SnapshotBuilder<'_, NetLocator>,
    discovery: &InterfaceDiscovery<'_>,
    ifname: &str,
) {
    if !valid_ifname(ifname) {
        return;
    }
    let ifindex = match read_ifindex(discovery.files, ifname) {
        IfindexRead::Index(index) => index,
        IfindexRead::Absent => return,
        IfindexRead::Unparsable => {
            builder.error("IFINDEX_UNPARSABLE");
            return;
        }
        IfindexRead::Failed(error) => {
            builder.carry_over_matching(|record| is_interface_named(record, ifname));
            builder.error(format!("IFINDEX_UNREADABLE:{}", io_failure(&error).1));
            return;
        }
    };
    let Ok(entity_id) = entity::network_interface(discovery.net_namespace, ifindex) else {
        builder.error("INTERFACE_IDENTITY_INVALID");
        return;
    };
    // A rename keeps the ifindex but is still treated as a new incarnation:
    // re-starting a rate is harmless, continuing a foreign counter is not.
    let Some(incarnation) = builder.incarnation(&entity_id, &format!("{ifname}#{ifindex}")) else {
        return;
    };
    let sources = interface_sources(builder, discovery.keys, &entity_id, ifname, ifindex);
    let parts = EntityParts {
        entity_id,
        kind: EntityKind::NetworkInterface,
        parent_id: Some(invariant(EntityId::new(entity::HOST), entity::HOST)),
        display_name: ifname.to_owned(),
        attributes: interface_attributes(discovery.files, ifname),
    };
    builder.add(FAMILY_INTERFACE, parts, incarnation, sources);
}

fn interface_attributes(files: &dyn SystemFiles, ifname: &str) -> Vec<Attribute> {
    let operstate = files
        .read_to_string(&interface_path(ifname, "operstate"))
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|state| !state.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());
    let loopback = files
        .read_to_string(&interface_path(ifname, "type"))
        .ok()
        .and_then(|text| parse_u64(&text))
        .is_some_and(|kind| kind == ARPHRD_LOOPBACK);
    vec![
        Attribute {
            key: attributes::IFNAME.to_owned(),
            value: ifname.to_owned(),
        },
        Attribute {
            key: attributes::OPERSTATE.to_owned(),
            value: operstate,
        },
        Attribute {
            key: LOOPBACK_ATTRIBUTE.to_owned(),
            value: loopback.to_string(),
        },
    ]
}

fn interface_sources(
    builder: &SnapshotBuilder<'_, NetLocator>,
    keys: &KeyContext,
    entity_id: &EntityId,
    ifname: &str,
    ifindex: u32,
) -> Vec<SourceDraft<NetLocator>> {
    let scope = invariant(Scope::new(entity::SCOPE_INTERFACE), entity::SCOPE_INTERFACE);
    let device = format!("netif.{ifindex}");
    NET_COUNTERS
        .into_iter()
        .filter_map(|counter| {
            let path = interface_path(ifname, &format!("statistics/{}", counter.file()));
            let origin = format!("sysfs:{}:{}", entity_id.as_str(), counter.file());
            let spec = SourceSpec {
                device_id: &device,
                channel: counter.file(),
                entity: entity_id,
                scope: &scope,
                metric_id: counter.metric_id(),
                group: groups::FAST,
                native_unit: counter.native_unit(),
                raw_kind: RawKind::Counter,
                driver: DRIVER,
                device_path: &path,
                origin: &origin,
                update_interval_ms: None,
                label: Some(ifname),
                rank: 0,
            };
            let locator = NetLocator {
                ifname: ifname.to_owned(),
                ifindex,
                counter,
            };
            source_draft(builder.config(), keys, &spec, locator)
        })
        .collect()
}

/// Whether `record` is the interface named `ifname` (carry-over lookup).
pub(crate) fn is_interface_named(record: &EntityRecord, ifname: &str) -> bool {
    record.attribute(attributes::IFNAME) == Some(ifname)
}

/// Everything one interface read needs.
pub(crate) struct InterfaceRead<'a> {
    pub plan: &'a ReadPlan,
    pub files: &'a dyn SystemFiles,
    pub clock: &'a dyn TimeProvider,
}

/// Reads the selected counters of one interface (all with the same
/// `ifname`/`ifindex`), verifying the ifindex before and after.
pub(crate) fn read_interface(
    context: &InterfaceRead<'_>,
    sources: &[&LiveSource<NetLocator>],
    gate: &mut ReprobeGate,
) -> Vec<Observation> {
    let Some(first) = sources.first() else {
        return Vec::new();
    };
    let (ifname, ifindex) = (first.locator.ifname.as_str(), first.locator.ifindex);
    let before = timed(context.clock, || Ok(read_ifindex(context.files, ifname)));
    if let Some(failure) = identity_failure(&before, ifindex) {
        return failed_all(context, sources, &failure, ReadWindow::from(&before));
    }
    let records: Vec<Observation> = sources
        .iter()
        .filter_map(|source| counter_record(context, source, gate))
        .collect();
    let after = timed(context.clock, || Ok(read_ifindex(context.files, ifname)));
    match identity_failure(&after, ifindex) {
        Some(failure) => failed_all(context, sources, &failure, ReadWindow::from(&after)),
        None => records,
    }
}

/// Status and reason when the interface is not the catalogued one any more.
fn identity_failure(read: &TimedRead<IfindexRead>, expected: u32) -> Option<(ReadStatus, String)> {
    match &read.result {
        Ok(IfindexRead::Index(index)) if *index == expected => None,
        Ok(IfindexRead::Index(_)) => Some((ReadStatus::Removed, INTERFACE_RECREATED.to_owned())),
        Ok(IfindexRead::Absent) => Some((ReadStatus::Removed, "SOURCE_REMOVED".to_owned())),
        Ok(IfindexRead::Unparsable) => Some((ReadStatus::Error, PARSE_ERROR.to_owned())),
        Ok(IfindexRead::Failed(error)) | Err(error) => Some(io_failure(error)),
    }
}

fn failed_all(
    context: &InterfaceRead<'_>,
    sources: &[&LiveSource<NetLocator>],
    failure: &(ReadStatus, String),
    window: ReadWindow,
) -> Vec<Observation> {
    sources
        .iter()
        .filter_map(|source| {
            failed_record(
                context.plan,
                &source.descriptor,
                failure.0,
                &failure.1,
                window,
            )
        })
        .collect()
}

fn counter_record(
    context: &InterfaceRead<'_>,
    source: &LiveSource<NetLocator>,
    gate: &mut ReprobeGate,
) -> Option<Observation> {
    let descriptor = &source.descriptor;
    let key = descriptor.source_id.as_str();
    let now = context.clock.now_boot_ns();
    if let Some(failure) = gate.deferred(key, now) {
        return deferred_record(context.plan, descriptor, &failure, now);
    }
    let path = interface_path(
        &source.locator.ifname,
        &format!("statistics/{}", source.locator.counter.file()),
    );
    let read = timed(context.clock, || context.files.read_to_string(&path));
    let window = ReadWindow::from(&read);
    match &read.result {
        Ok(text) => {
            gate.record_success(key);
            u64_text_record(
                context.plan,
                descriptor,
                text,
                Quality::Verified,
                window,
                Vec::new(),
            )
        }
        Err(error) => {
            let (status, reason) = io_failure(error);
            gate.record_failure(key, read.finished_boot_ns, status, &reason);
            failed_record(context.plan, descriptor, status, &reason, window)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_names_are_validated() {
        assert!(valid_ifname("eth0"));
        assert!(valid_ifname("eth0.100"));
        assert!(valid_ifname("veth3f2a1b"));
        assert!(!valid_ifname(""));
        assert!(!valid_ifname(".."));
        assert!(!valid_ifname("a/b"));
        assert!(!valid_ifname("a:b"));
        assert!(!valid_ifname("a b"));
        assert!(!valid_ifname("sixteen-chars-xx"));
    }
}
