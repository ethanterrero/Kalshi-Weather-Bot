//! Convert a NOAA point forecast into a model probability for a
//! `WeatherThreshold` market.
//!
//! TODO v1: simple model — assume forecast error is normally distributed
//! around the NWS point estimate, with a fixed standard deviation per
//! horizon (next-day ~2°F, +5d ~5°F based on published NWS verification
//! stats). P(YES) = Phi((forecast - threshold) / sigma) for "≥ threshold"
//! markets, flipped for "≤". Calibrate sigma later from observed errors.
