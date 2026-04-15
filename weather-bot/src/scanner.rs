//! Slug parsing helpers for weather temperature events.
//!
//! The old polling `scan_all_cities` / `find_cheap_outcomes` path was deleted
//! after the IWantYourMoney deep dive proved the $0.05 CLOB liquidity didn't
//! exist. The HFT pipeline is event-driven — see `watchers/onchain.rs`.
//!
//! This module keeps only the slug utilities that the on-chain watcher reuses
//! when it correlates a NegRiskAdapter event with a gamma-API event slug.

use chrono::NaiveDate;
use std::collections::HashMap;

/// Parse a Polymarket temperature-event slug into (city, YYYY-MM-DD).
///
/// Pattern: `highest-temperature-in-{city}-on-{month}-{day}-{year}` with an
/// optional trailing bucket suffix like `-40c` or `-37corbelow`. Returns
/// `None` for anything that doesn't match the weather template.
pub fn parse_temperature_slug(slug: &str) -> Option<(String, String)> {
    let months: HashMap<&str, u32> = [
        ("january", 1), ("february", 2), ("march", 3), ("april", 4),
        ("may", 5), ("june", 6), ("july", 7), ("august", 8),
        ("september", 9), ("october", 10), ("november", 11), ("december", 12),
    ]
    .into_iter()
    .collect();

    // Must start with the weather template
    let rest = slug.strip_prefix("highest-temperature-in-")?;
    let parts: Vec<&str> = rest.split('-').collect();
    let on_idx = parts.iter().position(|p| *p == "on")?;
    if on_idx == 0 || on_idx + 3 >= parts.len() {
        return None;
    }
    let city = parts[..on_idx].join("-");
    let month = months.get(parts[on_idx + 1])?;
    let day: u32 = parts[on_idx + 2].parse().ok()?;
    let year: i32 = parts[on_idx + 3].parse().ok()?;
    let date = NaiveDate::from_ymd_opt(year, *month, day)?;
    Some((city, date.format("%Y-%m-%d").to_string()))
}

/// Extract the temperature label from a sub-market question string.
/// Examples:
///   "Will the highest temperature in Lucknow be 40°C on April 15?" → "40°C"
///   "Will the highest temperature in NYC be 79°F or below on April 15?" → "79°F or below"
pub fn extract_temp_label(question: &str) -> String {
    // Prefer the "N°C" / "N°F" substring when present
    if let Some(deg_pos) = question.find('°') {
        let before = &question[..deg_pos];
        let start = before
            .rfind(|c: char| !c.is_ascii_digit() && c != '-')
            .map(|i| i + 1)
            .unwrap_or(0);
        // `°` is 2 UTF-8 bytes; the unit letter (C/F) is 1 more byte.
        // Clamp to a valid char boundary just in case.
        let mut after_end = deg_pos + '°'.len_utf8() + 1;
        while after_end < question.len() && !question.is_char_boundary(after_end) {
            after_end += 1;
        }
        if after_end > question.len() {
            after_end = question.len();
        }
        let mut label = question[start..after_end].to_string();
        // Append "or below" / "or higher" / "or above" if the question contains them
        for suffix in ["or below", "or higher", "or above"] {
            if question.contains(suffix) {
                label.push(' ');
                label.push_str(suffix);
                break;
            }
        }
        return label;
    }
    question.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lucknow_slug() {
        let (city, date) =
            parse_temperature_slug("highest-temperature-in-lucknow-on-april-15-2026").unwrap();
        assert_eq!(city, "lucknow");
        assert_eq!(date, "2026-04-15");
    }

    #[test]
    fn parses_multi_word_city() {
        let (city, date) =
            parse_temperature_slug("highest-temperature-in-new-york-city-on-march-28-2026")
                .unwrap();
        assert_eq!(city, "new-york-city");
        assert_eq!(date, "2026-03-28");
    }

    #[test]
    fn rejects_non_temperature_slug() {
        assert!(parse_temperature_slug("will-trump-win-the-2024-election").is_none());
    }

    #[test]
    fn rejects_malformed_date() {
        assert!(parse_temperature_slug("highest-temperature-in-foo-on-banana-99-xyz").is_none());
    }

    #[test]
    fn extracts_celsius_label() {
        let q = "Will the highest temperature in Lucknow be 40°C on April 15?";
        assert_eq!(extract_temp_label(q), "40°C");
    }

    #[test]
    fn extracts_celsius_or_below() {
        let q = "Will the highest temperature in NYC be 79°F or below on April 15?";
        assert_eq!(extract_temp_label(q), "79°F or below");
    }
}
