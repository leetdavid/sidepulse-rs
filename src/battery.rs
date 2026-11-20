use serde::Serialize;

use crate::led;

const LOW_RED: &str = "#FF2600";
const MID_AMBER: &str = "#FFB000";
const HIGH_GREEN: &str = "#00FF66";
const CHARGING_MINT: &str = "#80FFC8";
const OFF: &str = "#000000";
const TRANSITION_MS: u32 = 360;

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Battery {
    pub present: bool,
    pub percent: u8,
    pub plugged: bool,
    pub charging: bool,
    pub charged: bool,
    pub adapter_watts: f64,
}

struct Fill {
    full: usize,
    partial: f64,
}

fn fill(percent: u8, count: usize) -> Fill {
    let raw = count as f64 * f64::from(percent.min(100)) / 100.0;
    let full = (raw.floor() as usize).min(count);
    let partial = if full >= count { 0.0 } else { raw - full as f64 };
    Fill { full, partial }
}

fn scale(color: &str, ratio: f64) -> String {
    let ratio = ratio.clamp(0.0, 1.0);
    let channel = |i: usize| {
        let value = u8::from_str_radix(&color[i..i + 2], 16).unwrap_or(0);
        (f64::from(value) * ratio).round() as u8
    };
    format!("#{:02X}{:02X}{:02X}", channel(1), channel(3), channel(5))
}

fn fill_color(index: usize, fill: &Fill, color: &str) -> String {
    if index < fill.full {
        color.to_string()
    } else if index == fill.full && fill.partial > 0.0 {
        scale(color, fill.partial)
    } else {
        OFF.to_string()
    }
}

fn level_color(percent: u8) -> &'static str {
    match percent {
        0..=20 => LOW_RED,
        21..=50 => MID_AMBER,
        _ => HIGH_GREEN,
    }
}

fn segments(count: usize, color_at: impl Fn(usize) -> String) -> String {
    (0..count)
        .map(|i| format!("{i}:{} {TRANSITION_MS}ms ease", color_at(i)))
        .collect::<Vec<_>>()
        .join(";")
}

pub fn program(battery: &Battery, led_count: u8, full_charge_watts: f64, brightness: u8) -> String {
    let count = usize::from(led_count.clamp(1, 8));
    let percent = battery.percent.min(100);
    let fill = fill(percent, count);
    let pulse_index = fill.full.min(count - 1);

    let body = if battery.charged || percent >= 100 {
        segments(count, |_| HIGH_GREEN.to_string())
    } else if !battery.plugged || !battery.charging {
        let color = level_color(percent);
        segments(count, |i| fill_color(i, &fill, color))
    } else {
        let ratio = if full_charge_watts > 0.0 { (battery.adapter_watts / full_charge_watts).clamp(0.0, 1.0) } else { 0.0 };
        let pulse_ms = (180.0 + 1220.0 * ratio).round() as u32;
        let cycle_ms = ((3.0 - 2.1 * ratio).max(0.9) * 1000.0).round() as u32;
        let hold_ms = cycle_ms.saturating_sub(TRANSITION_MS + pulse_ms);
        let mut lines = vec![segments(count, |i| fill_color(i, &fill, CHARGING_MINT))];
        lines.push(format!("{pulse_index}:{CHARGING_MINT} {pulse_ms}ms pulse"));
        if hold_ms > 0 {
            lines.push(format!("{pulse_index}:{} {hold_ms}ms none", fill_color(pulse_index, &fill, CHARGING_MINT)));
        }
        lines.push("repeat".into());
        lines.join("\n")
    };
    led::apply_brightness(&body, brightness)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discharging_half_battery_is_amber_with_partial_led() {
        let b = Battery { present: true, percent: 45, ..Default::default() };
        assert_eq!(
            program(&b, 8, 96.0, 255),
            "0:#FFB000 360ms ease;1:#FFB000 360ms ease;2:#FFB000 360ms ease;3:#996A00 360ms ease;4:#000000 360ms ease;5:#000000 360ms ease;6:#000000 360ms ease;7:#000000 360ms ease"
        );
    }

    #[test]
    fn charging_pulses_at_frontier() {
        let b = Battery { present: true, percent: 60, plugged: true, charging: true, adapter_watts: 96.0, ..Default::default() };
        let p = program(&b, 8, 96.0, 255);
        assert_eq!(
            p,
            "0:#80FFC8 360ms ease;1:#80FFC8 360ms ease;2:#80FFC8 360ms ease;3:#80FFC8 360ms ease;4:#66CCA0 360ms ease;5:#000000 360ms ease;6:#000000 360ms ease;7:#000000 360ms ease\n4:#80FFC8 1400ms pulse\nrepeat"
        );
        assert!(led::validate(&p).is_ok());
    }

    #[test]
    fn full_battery_is_all_green() {
        let b = Battery { present: true, percent: 100, plugged: true, ..Default::default() };
        assert_eq!(program(&b, 2, 96.0, 255), "0:#00FF66 360ms ease;1:#00FF66 360ms ease");
    }
}
