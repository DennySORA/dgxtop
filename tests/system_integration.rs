use dgxtop::collectors::Collector;
use dgxtop::collectors::cpu::CpuCollector;
use dgxtop::collectors::disk::DiskCollector;
use dgxtop::collectors::memory::MemoryCollector;
use dgxtop::collectors::network::NetworkCollector;

#[test]
fn cpu_collector_returns_valid_stats() {
    let mut collector = CpuCollector::new();
    assert!(collector.is_available());

    // First collection establishes baseline
    let stats = collector.collect().expect("CPU collect failed");
    assert!(stats.core_count > 0, "Expected at least 1 core");
    println!(
        "CPU: {:.1}% — {} cores, {:.0} MHz",
        stats.usage_percent, stats.core_count, stats.frequency_mhz
    );

    // Second collection computes deltas
    std::thread::sleep(std::time::Duration::from_millis(100));
    let stats2 = collector.collect().expect("CPU collect 2 failed");
    assert!(stats2.core_count > 0);
    assert!(
        stats2.cores.len() == stats2.core_count,
        "Core count mismatch"
    );
}

#[test]
fn memory_collector_returns_valid_stats() {
    let mut collector = MemoryCollector::new();
    assert!(collector.is_available());

    let stats = collector.collect().expect("Memory collect failed");
    assert!(stats.total_bytes > 0, "Total memory should be > 0");
    assert!(
        stats.used_bytes <= stats.total_bytes,
        "Used should be <= total"
    );
    println!(
        "Memory: {:.1} GB / {:.1} GB ({:.1}%)",
        stats.used_bytes as f64 / 1024.0 / 1024.0 / 1024.0,
        stats.total_bytes as f64 / 1024.0 / 1024.0 / 1024.0,
        stats.usage_percent(),
    );
}

#[test]
fn disk_collector_returns_stats_on_second_call() {
    let mut collector = DiskCollector::new();
    assert!(collector.is_available());

    // First call establishes baseline (returns empty because no delta)
    let _first = collector.collect().expect("Disk collect 1 failed");

    std::thread::sleep(std::time::Duration::from_millis(100));
    let stats = collector.collect().expect("Disk collect 2 failed");
    println!("Found {} disk devices", stats.len());
    for disk in &stats {
        println!(
            "  {} — R:{:.1} KB/s, W:{:.1} KB/s",
            disk.device_name,
            disk.read_bytes_per_sec / 1024.0,
            disk.write_bytes_per_sec / 1024.0,
        );
    }
}

#[test]
fn network_collector_returns_interfaces() {
    let mut collector = NetworkCollector::new();
    assert!(collector.is_available());

    // First call establishes baseline
    let _first = collector.collect().expect("Network collect 1 failed");

    std::thread::sleep(std::time::Duration::from_millis(100));
    let stats = collector.collect().expect("Network collect 2 failed");
    println!("Found {} network interfaces", stats.len());
    for net in &stats {
        println!(
            "  {} — up={}, RX:{:.1} KB/s, TX:{:.1} KB/s",
            net.name,
            net.is_up,
            net.rx_bytes_per_sec / 1024.0,
            net.tx_bytes_per_sec / 1024.0,
        );
    }
}
