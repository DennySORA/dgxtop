//! dgxtop binary entry point.

// The executable is the one crate nothing depends on: a `pub` item left
// unused in it is dead.
#![deny(dead_code_pub_in_binary)]

use std::process::ExitCode;

use clap::Parser;
use dgxtop::{cli, diagnostics, host_mode, modes, monitor};

fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    if cli.adapter_host.is_some() {
        return host_mode::run(&cli);
    }
    if cli.version {
        return modes::print_version(cli.verbose);
    }
    if cli.check_config {
        return modes::check_config(&cli);
    }
    if cli.import_legacy.is_some() {
        return modes::import_legacy(&cli);
    }
    if cli.diagnostics_json {
        return diagnostics::run(&cli);
    }
    if cli.snapshot_json {
        return monitor::snapshot(&cli);
    }
    monitor::run(&cli)
}
