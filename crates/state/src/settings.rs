//! State settings derived from the validated configuration.

use dgxtop_core::config::StoreConfig;

/// Everything the live state needs from `store`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StoreSettings {
    pub max_processes: usize,
    pub max_process_gpu_links: usize,
    /// `store.raw_history_seconds`: overload relief keeps half of it.
    pub raw_history_ns: u64,
    /// Canonical keys not committed for this long leave the snapshot.
    pub key_ttl_ns: u64,
    /// Process keys leave the snapshot sooner (`ProcessKey` TTL cleanup).
    pub process_key_ttl_ns: u64,
}

fn to_usize(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

impl StoreSettings {
    pub(crate) fn new(store: &StoreConfig) -> Self {
        let raw_history_ns = store.raw_history_ns();
        Self {
            max_processes: to_usize(store.max_processes),
            max_process_gpu_links: to_usize(store.max_process_gpu_links),
            raw_history_ns,
            key_ttl_ns: store.aggregate_retention_ns(),
            process_key_ttl_ns: raw_history_ns,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::clock::NANOS_PER_SECOND;
    use dgxtop_core::config::Config;

    #[test]
    fn defaults_follow_the_store_configuration() {
        let config = Config::defaults().unwrap();
        let settings = StoreSettings::new(&config.store);
        assert_eq!(settings.raw_history_ns, 300 * NANOS_PER_SECOND);
        assert_eq!(settings.process_key_ttl_ns, 300 * NANOS_PER_SECOND);
        assert_eq!(settings.key_ttl_ns, 24 * 3600 * NANOS_PER_SECOND);
    }
}
