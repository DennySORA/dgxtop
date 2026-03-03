use clap::Parser;

/// DGXTop — Interactive system monitor for NVIDIA DGX systems.
#[derive(Debug, Parser)]
#[command(name = "dgxtop", version, about, long_about = None)]
pub struct CliArgs {
    /// Update interval in seconds (0.1–10.0).
    #[arg(short, long, default_value_t = 1.0, value_parser = parse_interval)]
    pub interval: f64,

    /// Color theme: cyan, green, amber.
    #[arg(short = 't', long, default_value = "cyan")]
    pub theme: String,

    /// Disable GPU monitoring (useful on systems without NVIDIA GPUs).
    #[arg(long, default_value_t = false)]
    pub no_gpu: bool,

    /// Log level: error, warn, info, debug, trace.
    #[arg(long, default_value = "warn")]
    pub log_level: String,
}

fn parse_interval(s: &str) -> Result<f64, String> {
    let val: f64 = s
        .parse()
        .map_err(|_| format!("'{s}' is not a valid number"))?;
    if !(0.1..=10.0).contains(&val) {
        return Err(format!("interval must be between 0.1 and 10.0, got {val}"));
    }
    Ok(val)
}
