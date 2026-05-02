//! City-code → forecast-point + settlement-station mapping.
//!
//! Kalshi weather markets settle on the **NWS Daily Climate Report (CLI)** for
//! a specific airport-class station (e.g. NYC = KNYC / Central Park, Chicago
//! = KMDW / Midway, LA = KLAX, Miami = KMIA, Austin = KAUS). To trade with
//! edge we need both:
//!
//!   1. A `(lat, lon)` for the NWS gridded forecast — passed to
//!      `/points/{lat,lon}` to discover the gridpoint.
//!   2. The **same** station (ICAO id) that Kalshi will settle on, so the
//!      forecast we model and the value Kalshi pays out share a source.
//!      Mismatching these is the easiest way to lose money on weather markets.
//!
//! Source: settlement-station codes match the Kalshi help-center page
//! (https://help.kalshi.com/en/articles/13823837-weather-markets) which says
//! settlement uses the final NWS Daily Climate Report. CLI reports are issued
//! per WFO+station and identified by ICAO id; Iowa State IEM mirrors them at
//! https://mesonet.agron.iastate.edu/wx/afos/list.phtml.

use chrono::FixedOffset;

/// One supported city. `kalshi_code` matches what the scanner pulls out of the
/// series ticker (e.g. `"NY"` from `KXHIGHNY`).
#[derive(Debug, Clone, Copy)]
pub struct CitySpec {
    pub kalshi_code: &'static str,
    /// Human-readable name for logs.
    pub name: &'static str,
    /// Forecast point — fed to NWS `/points/{lat},{lon}`.
    pub lat: f64,
    pub lon: f64,
    /// ICAO station id of the NWS CLI report Kalshi settles on.
    pub icao: &'static str,
    /// IANA-style standard-time UTC offset. Used to compute the local-standard
    /// 1:00am→12:59am observation window that Kalshi uses for high-temp
    /// settlement (the window is in **standard time**, not local clock time —
    /// see help.kalshi.com).
    pub standard_utc_offset_hours: i32,
}

impl CitySpec {
    /// Standard-time offset as a chrono `FixedOffset`. Always negative for the
    /// US cities we support.
    pub fn standard_offset(&self) -> FixedOffset {
        FixedOffset::east_opt(self.standard_utc_offset_hours * 3600)
            .expect("city standard offsets are within ±24h")
    }
}

/// v1 supported cities. Coordinates are the airport-class station the NWS CLI
/// references; for NYC we use Central Park because Kalshi's NY high/low
/// markets settle on KNYC, not KLGA/KJFK.
const CITIES: &[CitySpec] = &[
    CitySpec {
        kalshi_code: "NY",
        name: "New York (Central Park)",
        lat: 40.7794,
        lon: -73.9692,
        icao: "KNYC",
        standard_utc_offset_hours: -5, // EST
    },
    CitySpec {
        // Kalshi's KXHIGHCHI / KXLOWCHI markets settle on **Midway (KMDW)**,
        // not O'Hare (KORD). Verified against the live `rules_primary` field
        // on demo-api ("the highest temperature recorded at Chicago Midway,
        // IL ... according to the National Weather Service's Climatological
        // Report (Daily)"). KORD is ~27 km north — using it would silently
        // bias every Chicago forecast.
        kalshi_code: "CHI",
        name: "Chicago (Midway)",
        lat: 41.7868,
        lon: -87.7522,
        icao: "KMDW",
        standard_utc_offset_hours: -6, // CST
    },
    CitySpec {
        kalshi_code: "LAX",
        name: "Los Angeles (LAX)",
        lat: 33.9416,
        lon: -118.4085,
        icao: "KLAX",
        standard_utc_offset_hours: -8, // PST
    },
    CitySpec {
        kalshi_code: "MIA",
        name: "Miami (MIA)",
        lat: 25.7959,
        lon: -80.2870,
        icao: "KMIA",
        standard_utc_offset_hours: -5, // EST (Miami doesn't observe DST in standard ref)
    },
    CitySpec {
        kalshi_code: "AUS",
        name: "Austin (Bergstrom)",
        lat: 30.1945,
        lon: -97.6699,
        icao: "KAUS",
        standard_utc_offset_hours: -6, // CST
    },
];

/// Look up a city by its Kalshi code (the suffix on the series ticker, e.g.
/// `"NY"` from `KXHIGHNY`). Returns `None` if we don't have a station mapped
/// — the strategy layer must refuse to trade unmapped cities so we never
/// model a forecast against a station Kalshi isn't settling on.
pub fn lookup_city(kalshi_code: &str) -> Option<&'static CitySpec> {
    CITIES.iter().find(|c| c.kalshi_code == kalshi_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_cities_resolve() {
        assert_eq!(lookup_city("NY").unwrap().icao, "KNYC");
        assert_eq!(lookup_city("CHI").unwrap().icao, "KMDW");
        assert_eq!(lookup_city("LAX").unwrap().icao, "KLAX");
    }

    #[test]
    fn unknown_city_returns_none() {
        assert!(lookup_city("ZZZ").is_none());
        assert!(lookup_city("").is_none());
    }

    #[test]
    fn standard_offsets_are_negative_for_us_cities() {
        for c in CITIES {
            assert!(c.standard_utc_offset_hours <= 0, "{} offset", c.name);
        }
    }
}
