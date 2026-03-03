use serde::{Deserialize, Serialize};

/// Static system information gathered once at startup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemInfo {
    pub hostname: String,
    pub kernel_version: String,
    pub os_name: String,
    pub architecture: String,
    pub uptime_seconds: u64,
    pub gpu_driver_version: Option<String>,
    pub cuda_version: Option<String>,
    pub gpu_count: u32,
}

impl SystemInfo {
    pub fn uptime_display(&self) -> String {
        let secs = self.uptime_seconds;
        let days = secs / 86400;
        let hours = (secs % 86400) / 3600;
        let minutes = (secs % 3600) / 60;

        if days > 0 {
            format!("{days}d {hours}h {minutes}m")
        } else if hours > 0 {
            format!("{hours}h {minutes}m")
        } else {
            format!("{minutes}m")
        }
    }
}
