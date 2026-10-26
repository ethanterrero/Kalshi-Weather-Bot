# radar — NEXRAD images for the bot's cities

Renders a base-reflectivity map around each Kalshi settlement station from free NOAA
NEXRAD data. This is a viewing and sharing tool. It is not wired into the trading strategy.

Each PNG is 1600×1800 px and shows the lowest-tilt (0.5°) reflectivity from the nearest
WSR-88D. It uses the NWS reflectivity colormap and draws county and state lines. A star
marks the settlement station and a triangle marks the radar. The header gives the scan
time in UTC and in the city's local time, and the footer credits "Data: NOAA NEXRAD via
AWS Open Data".

## Setup

```bash
python3 -m venv radar/.venv
radar/.venv/bin/pip install -r radar/requirements.txt
```

The first render downloads Natural Earth coastlines and states (through cartopy) and
MetPy's county shapefile. Both are cached outside the repo, so later runs work offline
apart from fetching radar data.

## Usage

Run from the repo root:

```bash
radar/.venv/bin/python radar/render.py --city NY                          # latest scan
radar/.venv/bin/python radar/render.py --city CHI --time 2026-10-20T19:00Z
radar/.venv/bin/python radar/render.py --all --out radar/out/             # all five cities
```

- `--city`: one of `NY`, `CHI`, `LAX`, `MIA`, `AUS` (case-insensitive). Use `--all` for all five.
- `--time`: an ISO time. A trailing `Z` or a missing offset means UTC. The default is the newest volume in the bucket.
- `--out`: the output directory, `radar/out/` by default. Files are named `{CITY}_{RADAR}_{YYYYMMDDTHHMMZ}.png`.
- `--radius-km`: the map half-width around the station, 150 km by default.

The script prints one PNG path per city and exits non-zero if any city failed. Downloaded
volumes are cached under `radar/.cache/`, which is gitignored, so a re-render is quick.

## Cities and radars

The mapping lives in one table, `CITIES` in `nexrad.py`. Distances come from the radar
coordinates in the decoded volume headers.

| City | Settlement station | Radar | Distance | Notes |
|------|--------------------|-------|---------:|-------|
| NY   | KNYC Central Park  | KOKX Upton, NY | 94 km | KDIX (Fort Dix) is 100 km away; KOKX is the NYC office's own radar |
| CHI  | KMDW Midway        | KLOT Romeoville, IL | 34 km | |
| LAX  | KLAX               | KSOX Santa Ana Mtns | 71 km | KVTX (Sulphur Mtn) is 90 km away. KSOX is closer and its 0.5° beam is lower over LAX |
| MIA  | KMIA Miami Intl    | KAMX Miami | 22 km | |
| AUS  | KAUS Austin-Bergstrom | KEWX New Braunfels | 63 km | KGRK (Fort Hood) is 66 km away; KEWX is the Austin/San Antonio office's radar |

## Data source: Level II

Volumes come from the AWS Open Data bucket `unidata-nexrad-level2` (formerly
`noaa-nexrad-level2`), read anonymously with an unsigned boto3 client.

I chose Level II over Level III for three reasons:

- **Archive depth.** Level II covers 1991 to now with one file per volume, so any
  historical time works the same way as "latest".
- **Resolution.** You get the full super-resolution sweep: 0.5° × 250 m, out to 460 km.
  Level III N0Q is quantised to 0.5 dBZ and stored per product, so it is harder to reuse.
- **Room for a model later.** The same decode step gives the dual-pol moments if a model
  step ever wants them.

The cost is size. A volume is 3–15 MB and MetPy decodes all of it, which takes a few
seconds per city. That's fine for a CLI.

"Latest" means the newest object in the archive bucket. It usually lags real time by
5–10 minutes. The near-real-time chunk feed (`unidata-nexrad-level2-chunks`) is not used.

## How it handles bad volumes

Radar restarts sometimes leave a truncated volume that starts in the middle of the scan
pattern. Its lowest sweep is then a partial sector or a higher tilt. The renderer tries
the three volumes nearest the requested time. It skips any volume whose lowest
reflectivity sweep covers less than 90% of azimuths or sits above 1°, and logs a warning
when it does. It also warns if the scan it uses is more than 30 minutes from `--time`.

Example: the archive has no KEWX volumes on 2026-10-20 between 08:47 and 19:57Z. `--time
2026-10-20T19:00Z` skips the truncated 19:57 volume, renders the 20:04 one, and warns
that it is 64 minutes off.

## Library use

Each step can be imported on its own, either for scripting or for a later model-input step:

```python
from radar.nexrad import CITIES, nearest_scans, download, lowest_tilt_reflectivity, plot_sweep, render_city

t, key = nearest_scans("KLOT", when=None, n=1)[0]       # fetch: (scan time, S3 key)
sweep = lowest_tilt_reflectivity(download(key))         # decode: ReflectivitySweep
lat, lon = sweep.lat_lon()                              # gate-centre grids, same shape as sweep.dbz
plot_sweep(sweep, CITIES["CHI"], "chi.png")             # plot
```

`ReflectivitySweep.dbz` is a masked `(n_azimuth, n_gates)` array on a regular azimuth grid.
Bins the radar didn't sample are masked, and `coverage` gives the fraction of bins that
were filled.

## Reading the images

Values below 5 dBZ are not drawn. Clear-air days still show speckle and the bloom of
insect and bird returns around the radar. Those echoes are real data, not rain. The
renderer does no clutter or biological filtering. A dual-pol correlation-coefficient mask
could be added if that becomes a problem.
