//! The words of a slowed GPU's banner (D-146, D-147): what holds it back,
//! in plain words, the clock it runs at, whether that is expected and what
//! the owner can do, and that `Esc` hides it once it is known.
//!
//! `GPU 0 slowed down: at its power limit of 360 W · clock 2715 MHz (boost
//! max 3090) · expected under heavy load; raising the power limit (up to
//! 390 W) lifts it · Esc hides`

use dgxtop_core::gpu_state::{ThrottleSeverity, throttle_reasons, throttle_severity};
use dgxtop_core::view::{Banner, BannerLevel, BannerTopic};

use crate::slowdown::GpuSlowdown;

/// Prefix of the topic of a GPU slowdown banner.
const TOPIC_PREFIX: &str = "gpu-slowdown:";

/// NVML's clocks-event bits (`gpu_state::THROTTLE_REASONS`).
const POWER_CAP: u64 = 0x4;
const HARDWARE_SLOWDOWN: u64 = 0x8;
const THERMAL_SLOWDOWN: u64 = 0x20;
const HARDWARE_THERMAL_SLOWDOWN: u64 = 0x40;
const POWER_BRAKE: u64 = 0x80;
const BOARD_LIMIT: u64 = 0x200;
const VOLTAGE_LIMIT: u64 = 0x400;

/// The banner of one slowed GPU, named `label`: an error for heat or a
/// hardware signal, a warning for a power cap.
pub(crate) fn slowdown_banner(label: &str, slowdown: &GpuSlowdown) -> Banner {
    let level = match throttle_severity(slowdown.reasons) {
        Some(ThrottleSeverity::Alert) => BannerLevel::Error,
        _ => BannerLevel::Warning,
    };
    let mut parts = vec![causes_text(slowdown)];
    if let Some((now, maximum)) = slowdown.clock_mhz {
        parts.push(format!("clock {now:.0} MHz (boost max {maximum:.0})"));
    }
    parts.extend(advice(slowdown));
    parts.push("Esc hides".to_owned());
    Banner {
        level,
        text: format!("{label} slowed down: {}", parts.join(" · ")),
        topic: Some(BannerTopic {
            subject: format!("{TOPIC_PREFIX}{}", slowdown.gpu.as_str()),
            causes: slowdown.reasons,
        }),
    }
}

/// Every reason in words, joined: `at its power limit of 360 W; too hot
/// (88 °C), the driver lowers the clock`.
fn causes_text(slowdown: &GpuSlowdown) -> String {
    let causes: Vec<String> = throttle_reasons(slowdown.reasons)
        .map(|reason| cause_text(reason.bit, reason.label, slowdown))
        .collect();
    causes.join("; ")
}

fn cause_text(bit: u64, label: &str, slowdown: &GpuSlowdown) -> String {
    match bit {
        POWER_CAP => slowdown.power_limit_w.map_or_else(
            || "at its power limit".to_owned(),
            |limit| format!("at its power limit of {limit:.0} W"),
        ),
        THERMAL_SLOWDOWN => format!("too hot{}, the driver lowers the clock", degrees(slowdown)),
        HARDWARE_THERMAL_SLOWDOWN => format!(
            "too hot{}, the hardware's protection lowers the clock",
            degrees(slowdown)
        ),
        HARDWARE_SLOWDOWN => {
            "a hardware signal (heat or the power supply) lowers the clock".to_owned()
        }
        POWER_BRAKE => "the power brake is on (the system asks it to draw less)".to_owned(),
        VOLTAGE_LIMIT => "at its voltage limit, the top of its boost".to_owned(),
        BOARD_LIMIT => "held by the board's limit policy".to_owned(),
        _ => label.to_owned(),
    }
}

/// ` (88 °C)`, or nothing without a reading.
fn degrees(slowdown: &GpuSlowdown) -> String {
    slowdown
        .temperature_c
        .map_or_else(String::new, |celsius| format!(" ({celsius:.0} °C)"))
}

/// What the owner can make of the reasons, each said once: a power cap
/// or the voltage limit is expected under load, and a power cap can be
/// raised while the board allows more; heat and hardware signals point at
/// the cooling or the power supply.
fn advice(slowdown: &GpuSlowdown) -> Vec<String> {
    let reasons = slowdown.reasons;
    let mut advice = Vec::new();
    if reasons & (POWER_CAP | VOLTAGE_LIMIT | BOARD_LIMIT) != 0 {
        advice.push(format!("expected under heavy load{}", raise_hint(slowdown)));
    }
    if reasons & (THERMAL_SLOWDOWN | HARDWARE_THERMAL_SLOWDOWN) != 0 {
        advice.push("check the fans and airflow".to_owned());
    }
    if reasons & (HARDWARE_SLOWDOWN | POWER_BRAKE) != 0 {
        advice.push("check the cooling, power supply and power cables".to_owned());
    }
    advice
}

/// How far a power cap can be raised; nothing for other reasons, which
/// no setting lifts.
fn raise_hint(slowdown: &GpuSlowdown) -> String {
    if slowdown.reasons & POWER_CAP == 0 {
        return String::new();
    }
    match (slowdown.power_limit_w, slowdown.power_limit_maximum_w) {
        (Some(limit), Some(maximum)) if maximum > limit + 0.5 => {
            format!("; raising the power limit (up to {maximum:.0} W) lifts it")
        }
        (_, Some(_)) => "; the limit is already the board's highest".to_owned(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::identity::EntityId;

    fn slowed(reasons: u64) -> GpuSlowdown {
        GpuSlowdown {
            gpu: EntityId::new("gpu:GPU-aaaa1111").unwrap(),
            reasons,
            clock_mhz: Some((2715.0, 3090.0)),
            power_limit_w: Some(360.0),
            power_limit_maximum_w: Some(390.0),
            temperature_c: Some(88.0),
        }
    }

    /// A power cap names the limit, says it is expected and how far the
    /// limit can be raised; it is a warning the user may hide.
    #[test]
    fn a_power_cap_names_its_limit_and_how_to_raise_it() {
        let banner = slowdown_banner("GPU 0", &slowed(POWER_CAP));
        assert_eq!(banner.level, BannerLevel::Warning);
        assert_eq!(
            banner.text,
            "GPU 0 slowed down: at its power limit of 360 W · clock 2715 MHz (boost max 3090) \
             · expected under heavy load; raising the power limit (up to 390 W) lifts it · Esc hides"
        );
        assert_eq!(
            banner.topic,
            Some(BannerTopic {
                subject: "gpu-slowdown:gpu:GPU-aaaa1111".into(),
                causes: POWER_CAP,
            })
        );
    }

    /// The voltage limit, which an RTX 5080 under load reports, is named
    /// as the top of the boost and expected; no power limit is offered.
    #[test]
    fn the_voltage_limit_is_named_as_the_top_of_the_boost() {
        let banner = slowdown_banner("GPU 0", &slowed(VOLTAGE_LIMIT));
        assert_eq!(banner.level, BannerLevel::Warning);
        assert_eq!(
            banner.text,
            "GPU 0 slowed down: at its voltage limit, the top of its boost \
             · clock 2715 MHz (boost max 3090) · expected under heavy load · Esc hides"
        );
        assert!(
            slowdown_banner("GPU 0", &slowed(BOARD_LIMIT))
                .text
                .contains("held by the board's limit policy")
        );
    }

    /// At the board's highest limit nothing can be raised; without the
    /// readings the cause is still said in words.
    #[test]
    fn a_power_cap_at_the_highest_limit_or_without_readings() {
        let mut highest = slowed(POWER_CAP);
        highest.power_limit_w = Some(390.0);
        assert!(
            slowdown_banner("GPU 0", &highest)
                .text
                .contains("expected under heavy load; the limit is already the board's highest")
        );
        let bare = GpuSlowdown {
            clock_mhz: None,
            power_limit_w: None,
            power_limit_maximum_w: None,
            temperature_c: None,
            ..slowed(POWER_CAP)
        };
        assert_eq!(
            slowdown_banner("GPU 0", &bare).text,
            "GPU 0 slowed down: at its power limit · expected under heavy load · Esc hides"
        );
    }

    /// Heat is an error with its temperature and the advice to check the
    /// cooling, each piece of advice said once.
    #[test]
    fn heat_and_hardware_signals_say_what_to_check() {
        let banner = slowdown_banner(
            "GPU 1",
            &slowed(THERMAL_SLOWDOWN | HARDWARE_THERMAL_SLOWDOWN | POWER_BRAKE),
        );
        assert_eq!(banner.level, BannerLevel::Error);
        assert_eq!(
            banner.text,
            "GPU 1 slowed down: too hot (88 °C), the driver lowers the clock; \
             too hot (88 °C), the hardware's protection lowers the clock; \
             the power brake is on (the system asks it to draw less) \
             · clock 2715 MHz (boost max 3090) · check the fans and airflow \
             · check the cooling, power supply and power cables · Esc hides"
        );
        assert!(
            slowdown_banner("GPU 1", &slowed(HARDWARE_SLOWDOWN))
                .text
                .contains("a hardware signal (heat or the power supply) lowers the clock")
        );
    }
}
