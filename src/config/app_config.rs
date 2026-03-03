use serde::{Deserialize, Serialize};

use super::cli::CliArgs;

/// Persistent application configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub update_interval_secs: f64,
    pub color_theme: String,
    pub gpu_enabled: bool,
    pub redline_threshold: f64,
    pub history_length: usize,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            update_interval_secs: 1.0,
            color_theme: "cyan".to_owned(),
            gpu_enabled: true,
            redline_threshold: 80.0,
            history_length: 120,
        }
    }
}

impl AppConfig {
    /// Merge CLI arguments into the config (CLI takes precedence).
    pub fn apply_cli(&mut self, args: &CliArgs) {
        self.update_interval_secs = args.interval;
        self.color_theme.clone_from(&args.theme);
        if args.no_gpu {
            self.gpu_enabled = false;
        }
        self.sanitize();
    }

    /// Clamp all values to safe ranges to prevent DoS via malicious config.
    pub fn sanitize(&mut self) {
        self.update_interval_secs = self.update_interval_secs.clamp(0.1, 10.0);
        if !self.update_interval_secs.is_finite() {
            self.update_interval_secs = 1.0;
        }
        self.history_length = self.history_length.clamp(10, 10_000);
        self.redline_threshold = self.redline_threshold.clamp(0.0, 100.0);
        if !self.redline_threshold.is_finite() {
            self.redline_threshold = 80.0;
        }
    }
}
