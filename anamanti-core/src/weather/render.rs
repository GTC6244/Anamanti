//! The spoken confirmation for a weather report.

use super::WeatherReport;

/// A short spoken confirmation for the model to relay once the forecast is on screen.
///
/// `is_home` is set when the forecast is for the household's own (device) location —
/// i.e. the caller omitted an explicit place and fell back to `home_location`. For a
/// **current-weather** ask at home we keep it plain and unfussy — no place name, no
/// "on the screen" trailer — per the confirmed phrasing:
/// `"It's partly cloudy and 23C. The expected high is 24C with a low of 10C."`
/// A named place, or a future-day request, keeps the fuller phrasing.
pub fn render_confirmation(report: &WeatherReport, is_home: bool) -> String {
    let unit = if report.units == "imperial" { "F" } else { "C" };
    let c = &report.current;
    // A future-day forecast: speak the day + its summary rather than "right now".
    if !report.when_label.is_empty() {
        let mut out = format!("{} will be {}", report.when_label, c.description);
        if !report.location_label.is_empty() {
            out.push_str(&format!(" in {}", report.location_label));
        }
        out.push_str(&format!(
            ", with a high of {} degrees {unit} and a low of {}. The forecast is on the screen.",
            c.high, c.low
        ));
        return out;
    }
    // Current weather at the device's own location: the plain, confirmed read-out.
    if is_home {
        return format!(
            "It's {} and {}{unit}. The expected high is {}{unit} with a low of {}{unit}.",
            c.description, c.temp, c.high, c.low
        );
    }
    let mut out = format!(
        "It's {} degrees {unit} and {} right now",
        c.temp, c.description
    );
    if !report.location_label.is_empty() {
        out.push_str(&format!(" in {}", report.location_label));
    }
    out.push_str(&format!(
        ", with a high of {} and a low of {}. The forecast is on the screen.",
        c.high, c.low
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::weather::providers::fixture_report;

    #[test]
    fn confirmation_mentions_temp_and_location() {
        let report = fixture_report("Austin, Texas", true);
        // A named (non-home) place: speak the location in the fuller phrasing.
        let c = render_confirmation(&report, false);
        assert!(c.contains("72"), "{c}");
        assert!(c.contains("partly cloudy"), "{c}");
        assert!(c.contains("Austin, Texas"), "{c}");
    }

    #[test]
    fn confirmation_at_home_is_plain_and_unfussy() {
        // Metric so the unit reads as "C" like the confirmed phrasing.
        let mut report = fixture_report("Austin, Texas", false);
        report.current.temp = 23;
        report.current.high = 24;
        report.current.low = 10;
        report.current.description = "partly cloudy".into();
        let c = render_confirmation(&report, true);
        assert_eq!(
            c,
            "It's partly cloudy and 23C. The expected high is 24C with a low of 10C."
        );
        // No place name and no "on the screen" trailer for the device's own location.
        assert!(!c.contains("Austin, Texas"), "{c}");
        assert!(!c.contains("on the screen"), "{c}");
        assert!(!c.contains("right now"), "{c}");
    }

    #[test]
    fn confirmation_speaks_the_day_for_a_future_forecast() {
        let mut report = WeatherReport {
            location_label: "Austin, Texas".into(),
            units: "imperial".into(),
            when_label: "Sat, Sep 26".into(),
            ..Default::default()
        };
        report.current.description = "rain".into();
        report.current.high = 75;
        report.current.low = 58;
        let c = render_confirmation(&report, false);
        assert!(c.contains("Sat, Sep 26"), "{c}");
        assert!(c.contains("rain"), "{c}");
        assert!(c.contains("high of 75"), "{c}");
        assert!(!c.contains("right now"), "{c}");
    }
}
