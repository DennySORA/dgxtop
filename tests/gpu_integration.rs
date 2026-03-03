use dgxtop::collectors::Collector;
use dgxtop::collectors::gpu::{GpuCollector, init_nvml};
use dgxtop::collectors::gpu_process::GpuProcessCollector;

#[test]
#[ignore = "requires NVIDIA GPU with NVML drivers"]
fn nvml_initializes_successfully() {
    let nvml = init_nvml();
    assert!(nvml.is_ok(), "NVML init failed: {:?}", nvml.err());

    let nvml = nvml.unwrap();
    let count = nvml.device_count().expect("device_count failed");
    println!("NVML loaded — {count} GPU(s) detected");
    assert!(count > 0, "Expected at least 1 GPU");

    let driver = nvml.sys_driver_version().expect("driver version failed");
    println!("Driver: {driver}");
}

#[test]
#[ignore = "requires NVIDIA GPU with NVML drivers"]
fn gpu_collector_returns_valid_stats() {
    let mut collector = GpuCollector::try_new().expect("GpuCollector init failed");
    assert!(collector.is_available());

    let gpus = collector.collect().expect("GPU collect failed");
    assert!(!gpus.is_empty(), "Expected at least 1 GPU");

    for gpu in &gpus {
        println!(
            "GPU {}: {} — util={:.0}%, temp={:.0}°C, mem={}/{}",
            gpu.index,
            gpu.name,
            gpu.utilization_gpu,
            gpu.temperature,
            gpu.memory_used_bytes,
            gpu.memory_total_bytes,
        );
        assert!(!gpu.name.is_empty());
        assert!(gpu.memory_total_bytes > 0, "GPU memory total should be > 0");
        assert!(gpu.temperature >= 0.0, "Temperature should be non-negative");
        assert!(
            gpu.power_limit_watts > 0.0,
            "Power limit should be positive"
        );
    }
}

#[test]
#[ignore = "requires NVIDIA GPU with NVML drivers"]
fn gpu_process_collector_initializes() {
    let gpu_collector = GpuCollector::try_new().expect("GpuCollector init failed");
    let mut proc_collector =
        GpuProcessCollector::new(gpu_collector.device_count()).expect("GpuProcessCollector failed");

    let processes = proc_collector.collect().expect("Process collect failed");
    println!("Found {} GPU processes", processes.len());

    for proc in &processes {
        println!(
            "  PID {} — user={}, GPU={}, mem={}, cmd={}",
            proc.pid, proc.user, proc.gpu_index, proc.gpu_memory_bytes, proc.command,
        );
        assert!(proc.pid > 0);
    }
}
