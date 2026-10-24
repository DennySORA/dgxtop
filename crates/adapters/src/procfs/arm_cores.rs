//! The kinds of cores an arm64 CPU has, by name, from `/proc/cpuinfo`
//! (D-132). An arm64 kernel writes no `model name`: each logical CPU's
//! block carries its main ID register's `CPU implementer` and `CPU part`
//! codes, which name the core's design as `lscpu` names it. The table holds
//! the designs of Arm, NVIDIA and Ampere, whose cores run beside NVIDIA
//! GPUs; a core it does not know reads as its codes.

/// Core designers by their `CPU implementer` code.
const IMPLEMENTERS: &[(u32, &str)] = &[
    (0x41, "Arm"),
    (0x42, "Broadcom"),
    (0x43, "Cavium"),
    (0x46, "Fujitsu"),
    (0x48, "HiSilicon"),
    (0x4e, "NVIDIA"),
    (0x50, "APM"),
    (0x51, "Qualcomm"),
    (0x61, "Apple"),
    (0x6d, "Microsoft"),
    (0xc0, "Ampere"),
];

/// Core designs by implementer and `CPU part` code.
const DESIGNS: &[(u32, u32, &str)] = &[
    (0x41, 0xd03, "Cortex-A53"),
    (0x41, 0xd04, "Cortex-A35"),
    (0x41, 0xd05, "Cortex-A55"),
    (0x41, 0xd07, "Cortex-A57"),
    (0x41, 0xd08, "Cortex-A72"),
    (0x41, 0xd09, "Cortex-A73"),
    (0x41, 0xd0a, "Cortex-A75"),
    (0x41, 0xd0b, "Cortex-A76"),
    (0x41, 0xd0c, "Neoverse-N1"),
    (0x41, 0xd0d, "Cortex-A77"),
    (0x41, 0xd0e, "Cortex-A76AE"),
    (0x41, 0xd40, "Neoverse-V1"),
    (0x41, 0xd41, "Cortex-A78"),
    (0x41, 0xd42, "Cortex-A78AE"),
    (0x41, 0xd44, "Cortex-X1"),
    (0x41, 0xd46, "Cortex-A510"),
    (0x41, 0xd47, "Cortex-A710"),
    (0x41, 0xd48, "Cortex-X2"),
    (0x41, 0xd49, "Neoverse-N2"),
    (0x41, 0xd4b, "Cortex-A78C"),
    (0x41, 0xd4c, "Cortex-X1C"),
    (0x41, 0xd4d, "Cortex-A715"),
    (0x41, 0xd4e, "Cortex-X3"),
    (0x41, 0xd4f, "Neoverse-V2"),
    (0x41, 0xd80, "Cortex-A520"),
    (0x41, 0xd81, "Cortex-A720"),
    (0x41, 0xd82, "Cortex-X4"),
    (0x41, 0xd83, "Neoverse-V3AE"),
    (0x41, 0xd84, "Neoverse-V3"),
    (0x41, 0xd85, "Cortex-X925"),
    (0x41, 0xd87, "Cortex-A725"),
    (0x41, 0xd88, "Cortex-A520AE"),
    (0x41, 0xd89, "Cortex-A720AE"),
    (0x41, 0xd8e, "Neoverse-N3"),
    (0x4e, 0x000, "Denver"),
    (0x4e, 0x003, "Denver 2"),
    (0x4e, 0x004, "Carmel"),
    (0xc0, 0xac3, "Ampere-1"),
    (0xc0, 0xac4, "Ampere-1a"),
];

/// One logical CPU's core design.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Core {
    implementer: u32,
    part: u32,
}

impl Core {
    /// The design's name; else its designer and part code; else both codes.
    fn name(self) -> String {
        let design = DESIGNS
            .iter()
            .find(|(implementer, part, _)| (*implementer, *part) == (self.implementer, self.part));
        if let Some((_, _, name)) = design {
            return (*name).to_owned();
        }
        match IMPLEMENTERS
            .iter()
            .find(|(implementer, _)| *implementer == self.implementer)
        {
            Some((_, designer)) => format!("{designer} part {:#05x}", self.part),
            None => format!(
                "implementer {:#04x} part {:#05x}",
                self.implementer, self.part
            ),
        }
    }
}

/// `10× Cortex-A725 + 10× Cortex-X925`: each kind of core `cpuinfo` lists
/// with how many logical CPUs are of it, in the order of its first one;
/// `None` when no CPU block names both its codes.
pub(crate) fn arm_core_kinds(cpuinfo: &str) -> Option<String> {
    let mut kinds: Vec<(Core, usize)> = Vec::new();
    for core in cores(cpuinfo) {
        match kinds.iter_mut().find(|(kind, _)| *kind == core) {
            Some((_, count)) => *count += 1,
            None => kinds.push((core, 1)),
        }
    }
    let names: Vec<String> = kinds
        .iter()
        .map(|(core, count)| format!("{count}× {}", core.name()))
        .collect();
    (!names.is_empty()).then(|| names.join(" + "))
}

/// The core of every CPU block that names both its codes, in order.
fn cores(cpuinfo: &str) -> Vec<Core> {
    let mut cores = Vec::new();
    let mut block: (Option<u32>, Option<u32>) = (None, None);
    for (key, value) in cpuinfo.lines().filter_map(|line| line.split_once(':')) {
        match key.trim() {
            "processor" => cores.extend(core_of(std::mem::take(&mut block))),
            "CPU implementer" => block.0 = code(value),
            "CPU part" => block.1 = code(value),
            _ => {}
        }
    }
    cores.extend(core_of(block));
    cores
}

fn core_of((implementer, part): (Option<u32>, Option<u32>)) -> Option<Core> {
    Some(Core {
        implementer: implementer?,
        part: part?,
    })
}

/// A code as the kernel writes it (`0x41`); anything else is none.
fn code(value: &str) -> Option<u32> {
    u32::from_str_radix(value.trim().strip_prefix("0x")?, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cpuinfo(cores: &[(&str, &str)]) -> String {
        cores
            .iter()
            .enumerate()
            .map(|(cpu, (implementer, part))| {
                format!(
                    "processor\t: {cpu}\nCPU implementer\t: {implementer}\n\
                     CPU architecture: 8\nCPU part\t: {part}\n\n"
                )
            })
            .collect()
    }

    /// One kind of core reads as that kind with its count.
    #[test]
    fn one_kind_of_core_reads_with_its_count() {
        let grace = cpuinfo(&[("0x41", "0xd4f"); 4]);
        assert_eq!(arm_core_kinds(&grace).as_deref(), Some("4× Neoverse-V2"));
    }

    /// A design the table does not know reads as its designer and part, an
    /// unknown designer as both codes, so no core goes uncounted.
    #[test]
    fn unknown_designs_read_as_their_codes() {
        let mixed = cpuinfo(&[("0x41", "0xd99"), ("0x41", "0xd99"), ("0x99", "0x123")]);
        assert_eq!(
            arm_core_kinds(&mixed).as_deref(),
            Some("2× Arm part 0xd99 + 1× implementer 0x99 part 0x123")
        );
        let ampere = cpuinfo(&[("0xc0", "0xac3")]);
        assert_eq!(arm_core_kinds(&ampere).as_deref(), Some("1× Ampere-1"));
    }

    /// A block without both codes, or with a code that is not hexadecimal,
    /// names no core; without any, the CPU has no model.
    #[test]
    fn blocks_without_both_codes_name_no_core() {
        let partial = "processor\t: 0\nCPU implementer\t: 0x41\n\n\
                       processor\t: 1\nCPU implementer\t: 0x41\nCPU part\t: d85\n\n\
                       processor\t: 2\nCPU implementer\t: 0x41\nCPU part\t: 0xd85\n";
        assert_eq!(arm_core_kinds(partial).as_deref(), Some("1× Cortex-X925"));
        assert_eq!(arm_core_kinds("processor\t: 0\nflags\t: fpu\n"), None);
        assert_eq!(arm_core_kinds(""), None);
    }
}
