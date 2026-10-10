//! Theoretical GPU bandwidths from what the driver reports (D-48): the
//! device memory's peak from its maximum clock and bus width, and a PCIe
//! link's per-direction capacity from its generation and width. These are
//! capabilities of the hardware, never measurements of traffic: the memory
//! controller's busy share is not turned into bytes per second
//! (`gpu.memory.bandwidth.estimated` stays without a model).

/// Bits per byte.
const BITS_PER_BYTE: f64 = 8.0;
/// Data transfers per memory clock cycle: the driver reports half the data
/// rate of GDDR and HBM memory (a 15001 MHz GDDR7 clock carries 30 Gbit/s
/// per pin).
const TRANSFERS_PER_CLOCK: f64 = 2.0;

/// Peak device-memory bandwidth in bytes per second: maximum memory clock
/// (MHz) × 2 transfers × bus width (bits) / 8. `None` without a positive
/// clock and width.
pub fn memory_peak_bytes_per_second(max_memory_clock_mhz: f64, bus_width_bits: f64) -> Option<f64> {
    let valid = |value: f64| value.is_finite() && value > 0.0;
    (valid(max_memory_clock_mhz) && valid(bus_width_bits))
        .then(|| max_memory_clock_mhz * 1e6 * TRANSFERS_PER_CLOCK * bus_width_bits / BITS_PER_BYTE)
}

/// Usable bytes per second of one PCIe lane in one direction, by
/// generation: the transfer rate less the line encoding (8b/10b up to
/// Gen 2, 128b/130b for Gen 3 to 5, 242/256 FLIT for Gen 6).
fn lane_bytes_per_second(generation: u64) -> Option<f64> {
    let (giga_transfers, payload_bits, encoded_bits) = match generation {
        1 => (2.5, 8.0, 10.0),
        2 => (5.0, 8.0, 10.0),
        3 => (8.0, 128.0, 130.0),
        4 => (16.0, 128.0, 130.0),
        5 => (32.0, 128.0, 130.0),
        6 => (64.0, 242.0, 256.0),
        _ => return None,
    };
    Some(giga_transfers * 1e9 * payload_bits / encoded_bits / BITS_PER_BYTE)
}

/// Capacity of a PCIe link in one direction, in bytes per second; `None`
/// for an unknown generation or a zero width.
pub fn pcie_bytes_per_second(generation: u64, width: u64) -> Option<f64> {
    let lanes = u32::try_from(width).ok().filter(|lanes| *lanes > 0)?;
    Some(lane_bytes_per_second(generation)? * f64::from(lanes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(value: Option<f64>, expected: f64) -> bool {
        value.is_some_and(|value| (value - expected).abs() <= expected * 1e-9)
    }

    #[test]
    fn memory_peaks_match_the_published_figures() {
        // RTX 5080: 15001 MHz × 2 × 256 bit = 960 GB/s.
        assert!(close(
            memory_peak_bytes_per_second(15_001.0, 256.0),
            960.064e9
        ));
        // RTX 3060 12 GB: 7501 MHz × 2 × 192 bit = 360 GB/s.
        assert!(close(
            memory_peak_bytes_per_second(7_501.0, 192.0),
            360.048e9
        ));
        // H100 SXM: 2619 MHz HBM3 × 2 × 5120 bit = 3.35 TB/s.
        assert!(close(
            memory_peak_bytes_per_second(2_619.0, 5_120.0),
            3_352.32e9
        ));
        assert_eq!(memory_peak_bytes_per_second(0.0, 256.0), None);
        assert_eq!(memory_peak_bytes_per_second(15_001.0, f64::NAN), None);
    }

    #[test]
    fn pcie_links_follow_their_generation_and_lanes() {
        // Gen 1 x16: 2.5 GT/s with 8b/10b is 250 MB/s per lane.
        assert!(close(pcie_bytes_per_second(1, 16), 4e9));
        // Gen 4 x16: 16 GT/s with 128b/130b, about 31.5 GB/s.
        assert!(close(
            pcie_bytes_per_second(4, 16),
            16e9 * 128.0 / 130.0 / 8.0 * 16.0
        ));
        // Gen 5 x16: about 63 GB/s; an x4 slot is a quarter of it.
        let gen5 = pcie_bytes_per_second(5, 16).unwrap();
        assert!((gen5 / 1e9 - 63.0).abs() < 0.1, "{gen5}");
        assert!(close(pcie_bytes_per_second(5, 4), gen5 / 4.0));
        assert!(close(
            pcie_bytes_per_second(6, 1),
            64e9 * 242.0 / 256.0 / 8.0
        ));
        assert_eq!(pcie_bytes_per_second(7, 16), None);
        assert_eq!(pcie_bytes_per_second(0, 16), None);
        assert_eq!(pcie_bytes_per_second(4, 0), None);
    }
}
