//! Kalshi city codes → NOAA NWS observation coordinates.
//!
//! The (lat, lon) pairs target the *same physical station* Kalshi cites in
//! each market's `rules_primary`. Picking the wrong station (e.g. O'Hare
//! instead of Midway for Chicago) silently shifts the forecast by 30+ km
//! and biases the model in ways that won't show up in dry-run logs but
//! will cost real money in live trading.
//!
//! Source for each entry: Kalshi market rules (`rules_primary` field on
//! KXHIGH{CITY} markets, fetched 2026-04-26 from demo-api). Coordinates
//! verified against the NWS / FAA station database for the named site.

#[derive(Debug, Clone, Copy)]
pub struct CityLocation {
    /// Kalshi's two-or-three-letter city code (the suffix on KXHIGH/KXLOW).
    pub code: &'static str,
    /// Human-readable name of the observation site Kalshi resolves on.
    /// Lifted verbatim from each market's `rules_primary`.
    pub site: &'static str,
    pub lat: f64,
    pub lon: f64,
}

/// All cities currently supported by `weather-pricing`. Adding a new city
/// requires (a) verifying the Kalshi `rules_primary` names a single ASOS
/// station and (b) adding the matching (lat, lon) here. Don't guess.
pub const CITIES: &[CityLocation] = &[
    CityLocation {
        code: "NY",
        site: "Central Park, New York",
        lat: 40.7790,
        lon: -73.9690,
    },
    CityLocation {
        // Kalshi resolves Chicago contracts on KMDW (Midway), NOT KORD (O'Hare).
        // Using O'Hare's coords would shift the forecast by ~30 km on a market
        // where the spread is often pennies — easy to miss, expensive to learn.
        code: "CHI",
        site: "Chicago Midway, IL",
        lat: 41.7868,
        lon: -87.7522,
    },
    CityLocation {
        code: "LAX",
        site: "Los Angeles International Airport, CA",
        lat: 33.9425,
        lon: -118.4081,
    },
    CityLocation {
        code: "MIA",
        site: "Miami International Airport, FL",
        lat: 25.7959,
        lon: -80.2870,
    },
    CityLocation {
        code: "AUS",
        site: "Austin-Bergstrom International Airport, TX",
        lat: 30.1975,
        lon: -97.6664,
    },
];

/// Look up the NOAA NWS observation coordinates for a Kalshi city code.
/// Returns None for codes not yet pinned against `rules_primary`.
pub fn lookup(code: &str) -> Option<&'static CityLocation> {
    CITIES.iter().find(|c| c.code == code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_known_codes() {
        assert_eq!(lookup("NY").unwrap().site, "Central Park, New York");
        assert_eq!(lookup("CHI").unwrap().site, "Chicago Midway, IL");
        assert!(lookup("LAX").is_some());
        assert!(lookup("MIA").is_some());
        assert!(lookup("AUS").is_some());
    }

    #[test]
    fn lookup_unknown_code_returns_none() {
        assert!(lookup("XYZ").is_none());
        assert!(lookup("").is_none());
        // Common pitfall: lower-case codes don't match.
        assert!(lookup("ny").is_none());
    }

    #[test]
    fn coordinates_are_in_valid_ranges() {
        for c in CITIES {
            assert!((-90.0..=90.0).contains(&c.lat), "{} lat out of range", c.code);
            assert!((-180.0..=180.0).contains(&c.lon), "{} lon out of range", c.code);
        }
    }

    #[test]
    fn no_duplicate_codes() {
        let mut seen = std::collections::HashSet::new();
        for c in CITIES {
            assert!(seen.insert(c.code), "duplicate city code: {}", c.code);
        }
    }
}
