//! Weather-only gate. Every event detected on-chain must pass this filter
//! before it reaches the mint/dump executors.
//!
//! Two checks:
//!   1. Slug-pattern regex: the event's slug must match
//!      `highest-temperature-in-{city}-on-{month}-{day}-{year}`.
//!   2. Oracle allowlist: the `oracle` emitting the `ConditionPreparation`
//!      or `MarketPrepared` log must be one we recognize as the Polymarket
//!      weather resolver.
//!
//! Belt-and-suspenders: if either check fails, the event is silently dropped
//! at the watcher layer before any mint/dump logic runs.

use alloy_primitives::{address, Address};
use once_cell::sync::Lazy;
use regex::Regex;

/// Polymarket UMA optimistic-oracle adapter — the only address allowed to
/// resolve weather temperature markets today. This needs periodic verification
/// against a live market; query gamma if in doubt:
///   curl -s 'https://gamma-api.polymarket.com/events/slug/highest-temperature-in-lucknow-on-april-15-2026' \
///     | jq '.markets[0] | {resolutionSource, umaResolver, oracle: .marketMakerAddress}'
///
/// Current value is the UmaCtfAdapter used by most Polymarket conditions. If
/// the allowlist is empty, the filter falls back to slug-only gating (dev
/// mode). Production MUST populate this.
pub const WEATHER_ORACLE_ALLOWLIST: &[Address] = &[
    // Polymarket UmaCtfAdapter on Polygon
    address!("6A9D222616C90FcA5754cd1333cFD9b7fb6a4F74"),
    // Polymarket NegRiskUmaCtfAdapter
    address!("2F5e3684cb1F318ec51b00Edba38d79Ac2c7c53C"),
];

/// Compiled once, reused per call. Matches:
///   `highest-temperature-in-{city}-on-{month}-{day}-{year}`
/// where `city` may contain letters, digits, and internal hyphens,
/// `month` is a lowercase English month name, and year is 4 digits.
static SLUG_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^highest-temperature-in-(?P<city>[a-z0-9]+(?:-[a-z0-9]+)*?)-on-(?P<month>january|february|march|april|may|june|july|august|september|october|november|december)-(?P<day>\d{1,2})-(?P<year>20\d{2})(?:-.*)?$",
    )
    .expect("weather slug regex is valid")
});

/// Returns `Some((city, resolution_date_yyyy_mm_dd))` if the slug matches the
/// weather template, else `None`.
pub fn parse_weather_slug(slug: &str) -> Option<(String, String)> {
    let caps = SLUG_RE.captures(slug)?;
    let city = caps.name("city")?.as_str().to_string();
    let month = match caps.name("month")?.as_str() {
        "january" => 1,
        "february" => 2,
        "march" => 3,
        "april" => 4,
        "may" => 5,
        "june" => 6,
        "july" => 7,
        "august" => 8,
        "september" => 9,
        "october" => 10,
        "november" => 11,
        "december" => 12,
        _ => return None,
    };
    let day: u32 = caps.name("day")?.as_str().parse().ok()?;
    let year: i32 = caps.name("year")?.as_str().parse().ok()?;
    let date = chrono::NaiveDate::from_ymd_opt(year, month, day)?;
    Some((city, date.format("%Y-%m-%d").to_string()))
}

/// Oracle allowlist check. Dev mode: if the allowlist is empty, every oracle
/// passes. Production: only explicit addresses match.
pub fn is_oracle_allowed(oracle: Address) -> bool {
    if WEATHER_ORACLE_ALLOWLIST.is_empty() {
        return true;
    }
    WEATHER_ORACLE_ALLOWLIST.iter().any(|a| *a == oracle)
}

/// Full weather gate: both slug and oracle must pass.
pub fn passes_weather_gate(slug: &str, oracle: Address) -> bool {
    parse_weather_slug(slug).is_some() && is_oracle_allowed(oracle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_lucknow_slug() {
        let (city, date) =
            parse_weather_slug("highest-temperature-in-lucknow-on-april-15-2026").unwrap();
        assert_eq!(city, "lucknow");
        assert_eq!(date, "2026-04-15");
    }

    #[test]
    fn accepts_new_york_city_slug() {
        let (city, date) =
            parse_weather_slug("highest-temperature-in-new-york-city-on-april-15-2026").unwrap();
        assert_eq!(city, "new-york-city");
        assert_eq!(date, "2026-04-15");
    }

    #[test]
    fn accepts_slug_with_bucket_suffix() {
        assert!(parse_weather_slug(
            "highest-temperature-in-lucknow-on-april-15-2026-40c"
        )
        .is_some());
        assert!(parse_weather_slug(
            "highest-temperature-in-nyc-on-april-15-2026-79forbelow"
        )
        .is_some());
    }

    #[test]
    fn rejects_non_weather_slug() {
        assert!(parse_weather_slug("will-trump-win-the-2024-election").is_none());
        assert!(parse_weather_slug("btc-updown-5m-1776000000").is_none());
        assert!(parse_weather_slug("sol-price-above-200").is_none());
    }

    #[test]
    fn rejects_weather_but_bogus_date() {
        assert!(parse_weather_slug("highest-temperature-in-foo-on-banana-99-xyz").is_none());
        assert!(parse_weather_slug("highest-temperature-in-foo-on-april-99-2026").is_none()); // day 99 invalid
    }

    #[test]
    fn allowlist_accepts_known_oracle() {
        let uma = address!("6A9D222616C90FcA5754cd1333cFD9b7fb6a4F74");
        assert!(is_oracle_allowed(uma));
    }

    #[test]
    fn allowlist_rejects_unknown_oracle() {
        let bad = address!("0000000000000000000000000000000000000001");
        assert!(!is_oracle_allowed(bad));
    }
}
