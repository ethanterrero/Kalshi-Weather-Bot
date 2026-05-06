# METAR / ASOS Observation Endpoints — Research

Goal: pick a fetcher for the Phase 2 "intraday nowcast" path so the bot can read
the running daily-high temperature directly from observations on settlement day
and update P(YES) for thin late-day Kalshi high/low markets.

Verified live against the API on 2026-05-05/06 UTC.

## Endpoint shape

NWS observations live under the same `api.weather.gov` host as the forecast
endpoints we already use ([NWS API docs][nws-docs]).

- Latest single observation:
  `GET https://api.weather.gov/stations/{ICAO}/observations/latest`
- List (most recent first, paginated):
  `GET https://api.weather.gov/stations/{ICAO}/observations?limit=N`
- Time window (good for "today so far"):
  `GET https://api.weather.gov/stations/{ICAO}/observations?start=<ISO8601Z>&end=<ISO8601Z>`
- Station metadata (lat/lon, provider="ASOS", timezone):
  `GET https://api.weather.gov/stations/{ICAO}`

Working sample URL fetched during this research:
`https://api.weather.gov/stations/KMIA/observations/latest` →
`200 OK`, `content-type: application/geo+json`, `cache-control: public, max-age=87, s-maxage=300`.
The wire format is GeoJSON `Feature`, identical to the forecast endpoints —
existing reqwest client and contact-bearing `User-Agent` reuse cleanly. No
published rate limit; docs ask for "reasonable" use ([NWS docs][nws-docs]),
which is fine for a 5-city poll.

## Per-city station verification

All five Kalshi v1 cities have a working NWS observations endpoint. KNYC
(Central Park) is real ASOS — the question in the prompt was a false alarm.
Verified by fetching `/stations/{ICAO}/observations/latest` for each:

| City    | Kalshi CLI station              | NWS obs station | Provider | Latest ts seen (UTC)     | Status |
|---------|----------------------------------|-----------------|----------|---------------------------|--------|
| NYC     | KNYC (Central Park)             | KNYC            | ASOS     | 2026-05-06T04:51:00Z      | OK     |
| Chicago | KMDW (Midway)                   | KMDW            | ASOS     | 2026-05-06T05:00:00Z      | OK     |
| LA      | KLAX                            | KLAX            | ASOS     | 2026-05-06T04:50:00Z      | OK     |
| Miami   | KMIA                            | KMIA            | ASOS     | 2026-05-06T04:50:00Z      | OK     |
| Austin  | KAUS                            | KAUS            | ASOS     | 2026-05-06T05:05:00Z      | OK     |

Station metadata for KNYC confirms `"provider": "ASOS"` and
`"timeZone": "America/New_York"` (`https://api.weather.gov/stations/KNYC`).
Same ICAOs that Kalshi settles against publish METARs we can read intraday —
no station-mismatch risk.

## Response schema

Real fetched payload from `/stations/KMIA/observations/latest` (trimmed,
`properties` block):

```json
{
  "stationId": "KMIA",
  "stationName": "Miami, Miami International Airport",
  "timestamp": "2026-05-06T04:50:00+00:00",
  "rawMessage": "",
  "textDescription": "Partly Cloudy",
  "temperature":               { "unitCode": "wmoUnit:degC", "value": 26,    "qualityControl": "V" },
  "dewpoint":                  { "unitCode": "wmoUnit:degC", "value": 22,    "qualityControl": "V" },
  "relativeHumidity":          { "unitCode": "wmoUnit:percent", "value": 78.6, "qualityControl": "V" },
  "windDirection":             { "unitCode": "wmoUnit:degree_(angle)", "value": 70 },
  "windSpeed":                 { "unitCode": "wmoUnit:km_h-1", "value": 11.124 },
  "barometricPressure":        { "unitCode": "wmoUnit:Pa", "value": 101625.52 },
  "maxTemperatureLast24Hours": { "unitCode": "wmoUnit:degC", "value": null },
  "minTemperatureLast24Hours": { "unitCode": "wmoUnit:degC", "value": null }
}
```

Key facts for the Rust client:

- **Units are SI, not imperial.** Temperature comes back as `wmoUnit:degC`.
  Convert to °F at the boundary; do not assume `value` is the unit Kalshi
  uses. Same applies to `windSpeed` (km/h) and `barometricPressure` (Pa).
- **`qualityControl`** is one of `V` (valid), `C` (coarse), `S` (subjective),
  `Z` (preliminary/unchecked). For settlement-grade math, gate on `V` or `C`.
- **`maxTemperatureLast24Hours` was `null`** on every station I sampled
  (KMIA, KNYC, KMDW, KLAX, KAUS). Treat it as unreliable — see next section.
- **Cadence:** routine METAR every hour at HH:53 plus SPECI between, and
  ASOS 5-minute METARs at major airports. Pulling
  `/observations?start=...&end=...` for a single 24h window on KMIA returned
  **289 features** for 2026-05-05, i.e. roughly every 5 min including SPECI.
- **Latency:** the latest timestamp on the response was ~28 minutes behind
  wall clock (`04:50Z` vs query at `05:18Z`). NWS's edge cache sets
  `s-maxage=300`, and the underlying ingest from the airport circuit takes
  another few minutes. Expect a 2pm-local METAR to be queryable by ~2:10pm
  local; design for that floor when computing whether to re-trade.

## "Running daily max" derivation

Two clean options. I'd ship **option A** as primary, **option B** as fallback.

**A. NWS list + local max (primary).** `GET /stations/{ICAO}/observations
?start=<00:00 local as UTC>&end=now`, walk `features[*].properties.temperature`,
filter `qualityControl in {V,C}`, take max. On KMIA for 2026-05-05 this
returned 289 features and a clean max of 31.1 °C at 15:53 UTC (87.98 °F),
which matches the IEM cross-check. Because NWS only commits to
`maxTemperatureLast24Hours` opportunistically (it was `null` on every station
sampled), we cannot rely on the pre-computed field — we *must* derive locally.

**B. IEM ASOS `currents.json` (fallback).**
`GET https://mesonet.agron.iastate.edu/api/1/currents.json?network={STATE}_ASOS`
returns a row per station with `max_tmpf`, `min_tmpf`, `tmpf`, `local_date`,
`local_valid`. Sample for `station=MIA` showed `max_tmpf: 78.0`, `tmpf: 78.0`,
`local_date: 2026-05-06`. Already in °F, already keyed to local civil date —
exactly Kalshi's settlement convention. Network IDs we'll need: `NY_ASOS`
(KNYC), `IL_ASOS` (KMDW), `CA_ASOS` (KLAX), `FL_ASOS` (KMIA), `TX_ASOS`
(KAUS). [Iowa State IEM ASOS docs][iem-docs] cover the schema.

Bulk historical CSV is also available
(`https://mesonet.agron.iastate.edu/cgi-bin/request/asos.py?...`); verified
returns hourly `tmpf` rows as flat CSV — useful for backfilling unit tests
and historical replay.

Rust impl: one trait `ObservationSource` with
`fetch_today_max(icao, local_tz) -> Result<Observed>`, two impls (NWS, IEM),
prefer NWS, fall back to IEM if NWS returns < N features or all are
QC-rejected.

## Open questions / risks

- **Local-date boundary for `maxTemperatureLast24Hours`** is rolling 24h, not
  civil-day. Even if NWS starts populating it, we still need to local-aggregate
  to match Kalshi's "calendar day in station-local time" rule.
- **Kalshi rounding rule** (whole-degree vs tenths) is not in this research's
  scope — confirm that we round °C→°F the same way Kalshi does before trading
  on a 1°F edge.
- **IEM `currents.json` freshness lag** wasn't fully measured here — the row
  I pulled for KMIA was ~24 min behind wall clock, similar to NWS. Worth
  pinning a real number with a quick scheduled measurement before we depend
  on it for the 2pm decision window.
- **No rate-limit headers** were returned by `api.weather.gov`. Add a
  conservative client-side limiter (e.g. 1 req/s per host) and exponential
  backoff on 429/503 — same pattern as the forecast fetcher.
- **Quality flag `Z`** (preliminary) shows up on derived fields (windGust,
  precipitationLast3Hours) but I did not see it on `temperature` in the
  sampled stations; add a metric so we notice if it ever gates out the
  running-max calc.

[nws-docs]: https://www.weather.gov/documentation/services-web-api
[iem-docs]: https://mesonet.agron.iastate.edu/ASOS/
