"""Fetch, decode and plot NEXRAD Level II reflectivity for the bot's cities.

Three layers, each importable on its own:

    fetch   nearest_scans() / download()      -> a Level II volume on disk
    decode  lowest_tilt_reflectivity()        -> a ReflectivitySweep (numpy)
    plot    plot_sweep()                      -> a PNG

`render_city()` chains them; `render.py` is the CLI over it.
"""

from __future__ import annotations

import logging
from dataclasses import dataclass, replace
from datetime import datetime, timedelta, timezone
from pathlib import Path
from zoneinfo import ZoneInfo

import numpy as np

log = logging.getLogger(__name__)

BUCKET = "unidata-nexrad-level2"
RADAR_DIR = Path(__file__).resolve().parent
DEFAULT_CACHE = RADAR_DIR / ".cache"
DEFAULT_OUT = RADAR_DIR / "out"


@dataclass(frozen=True)
class City:
    code: str  # Kalshi city code
    name: str
    station: str  # ICAO station Kalshi settles on (NWS CLI)
    station_name: str
    lat: float
    lon: float
    radar: str  # WSR-88D site used for this city
    tz: str


# One table for the whole mapping. Station coordinates match research/cities.py.
# Radar choice: the nearest WSR-88D with a clear low-level view of the station
# (distances from the decoded volume headers; see README for the LA choice).
CITIES: dict[str, City] = {
    c.code: c
    for c in (
        City("NY", "New York City", "KNYC", "Central Park", 40.7790, -73.9692, "KOKX", "America/New_York"),
        City("CHI", "Chicago", "KMDW", "Midway", 41.7860, -87.7524, "KLOT", "America/Chicago"),
        City("LAX", "Los Angeles", "KLAX", "LAX", 33.9382, -118.3866, "KSOX", "America/Los_Angeles"),
        City("MIA", "Miami", "KMIA", "Miami Intl", 25.7881, -80.3169, "KAMX", "America/New_York"),
        City("AUS", "Austin", "KAUS", "Austin-Bergstrom", 30.1831, -97.6799, "KEWX", "America/Chicago"),
    )
}


# --------------------------------------------------------------------------- fetch


def _s3():
    import boto3
    from botocore import UNSIGNED
    from botocore.config import Config

    return boto3.client("s3", config=Config(signature_version=UNSIGNED), region_name="us-east-1")


def _scan_time(key: str) -> datetime | None:
    # 2026/10/26/KOKX/KOKX20261026_130302_V06 (older years end in .gz / have no _V06)
    name = key.rsplit("/", 1)[-1]
    if name.endswith("_MDM") or len(name) < 19:
        return None
    try:
        return datetime.strptime(name[4:19], "%Y%m%d_%H%M%S").replace(tzinfo=timezone.utc)
    except ValueError:
        return None


def _utc(t: datetime) -> datetime:
    """Naive datetimes are taken as UTC."""
    return t.replace(tzinfo=timezone.utc) if t.tzinfo is None else t.astimezone(timezone.utc)


def list_scans(site: str, day: datetime, s3=None) -> list[tuple[datetime, str]]:
    """All Level II volumes for `site` on the UTC date of `day`, oldest first."""
    s3 = s3 or _s3()
    prefix = f"{day:%Y/%m/%d}/{site}/"
    scans = []
    for page in s3.get_paginator("list_objects_v2").paginate(Bucket=BUCKET, Prefix=prefix):
        for obj in page.get("Contents", []):
            t = _scan_time(obj["Key"])
            if t is not None:
                scans.append((t, obj["Key"]))
    return sorted(scans)


def nearest_scans(site: str, when: datetime | None = None, n: int = 3, s3=None) -> list[tuple[datetime, str]]:
    """Up to `n` volumes, closest to `when` (UTC) first, or newest first if `when` is None."""
    s3 = s3 or _s3()
    if when is None:
        today = datetime.now(timezone.utc)
        for day in (today, today - timedelta(days=1)):
            scans = list_scans(site, day, s3)
            if scans:
                return scans[::-1][:n]
        raise LookupError(f"no {site} volumes in {BUCKET} for today or yesterday")

    when = _utc(when)
    scans = [s for d in (-1, 0, 1) for s in list_scans(site, when + timedelta(days=d), s3)]
    if not scans:
        raise LookupError(f"no {site} volumes in {BUCKET} near {when:%Y-%m-%d %H:%MZ}")
    return sorted(scans, key=lambda s: abs(s[0] - when))[:n]


def find_scan(site: str, when: datetime | None = None, s3=None) -> tuple[datetime, str]:
    """The volume nearest to `when` (UTC), or the latest one if `when` is None."""
    return nearest_scans(site, when, 1, s3)[0]


def download(key: str, cache_dir: Path = DEFAULT_CACHE, s3=None) -> Path:
    """Download one volume (cached by key)."""
    path = cache_dir / key
    if path.exists() and path.stat().st_size > 0:
        return path
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(".part")
    (s3 or _s3()).download_file(BUCKET, key, str(tmp))
    tmp.rename(path)
    return path


# -------------------------------------------------------------------------- decode


@dataclass
class ReflectivitySweep:
    site: str
    site_lat: float
    site_lon: float
    time: datetime  # sweep start, UTC
    elevation: float  # degrees
    azimuth: np.ndarray  # (n_az,) regular bin centres, degrees, 0..360
    range_km: np.ndarray  # (n_gates,) gate centres
    dbz: np.ma.MaskedArray  # (n_az, n_gates); bins with no ray are fully masked
    coverage: float  # fraction of azimuth bins that got a ray

    def lat_lon(self) -> tuple[np.ndarray, np.ndarray]:
        """Gate-centre latitude/longitude grids, same shape as `dbz`."""
        from metpy.calc import azimuth_range_to_lat_lon

        lon, lat = azimuth_range_to_lat_lon(self.azimuth, self.range_km * 1000.0, self.site_lon, self.site_lat)
        return lat, lon

    def crop(self, max_range_km: float) -> ReflectivitySweep:
        """A copy with gates beyond `max_range_km` dropped."""
        keep = self.range_km <= max_range_km
        return replace(self, range_km=self.range_km[keep], dbz=self.dbz[:, keep])


def _ray_time(ray) -> datetime:
    hdr = ray[0]
    return datetime(1970, 1, 1, tzinfo=timezone.utc) + timedelta(days=hdr.date - 1, milliseconds=hdr.time_ms)


def lowest_tilt_reflectivity(path: Path | str) -> ReflectivitySweep:
    """Decode the lowest-elevation reflectivity sweep from a Level II volume."""
    from metpy.io import Level2File

    f = Level2File(str(path))
    idx = next((i for i, s in enumerate(f.sweeps) if s and b"REF" in s[0][4]), None)
    if idx is None:
        raise ValueError(f"{path}: no reflectivity sweep")
    sweep = f.sweeps[idx]
    # Prefer the nominal VCP angle (measured rays sit a few hundredths low), unless
    # the volume has no VCP message or MetPy had to renumber a truncated volume.
    elevation = float(np.median([ray[0].el_angle for ray in sweep]))
    vcp = getattr(f, "vcp_info", None)
    if vcp is not None and idx < len(vcp.els) and abs(vcp.els[idx].el_angle - elevation) < 0.2:
        elevation = vcp.els[idx].el_angle

    site = f.stid.decode() if isinstance(f.stid, bytes) else str(f.stid)
    vol = sweep[0][1]
    hdr = sweep[0][4][b"REF"][0]
    rng = np.arange(hdr.num_gates) * hdr.gate_width + hdr.first_gate

    # Rays land on a regular azimuth grid (0.5° super-res or 1° legacy) so a
    # partial sweep leaves masked bins instead of one wedge smeared across the gap.
    az = np.array([ray[0].az_angle for ray in sweep]) % 360.0
    res = 0.5 if np.median(np.abs(np.diff(az))) < 0.75 else 1.0
    n_az = int(round(360.0 / res))
    rays = np.ma.masked_invalid(np.array([ray[4][b"REF"][1] for ray in sweep], dtype=float))
    dbz = np.ma.masked_all((n_az, rng.size))
    bins = (az // res).astype(int) % n_az
    dbz[bins] = rays

    return ReflectivitySweep(
        site=site,
        site_lat=float(vol.lat),
        site_lon=float(vol.lon),
        time=_ray_time(sweep[0]),
        elevation=round(float(elevation), 1),
        azimuth=(np.arange(n_az) + 0.5) * res,
        range_km=rng,
        dbz=dbz,
        coverage=np.unique(bins).size / n_az,
    )


# ---------------------------------------------------------------------------- plot


def _edges(centres: np.ndarray) -> np.ndarray:
    """Cell edges for evenly spaced centres."""
    step = centres[1] - centres[0]
    return np.append(centres - step / 2, centres[-1] + step / 2)


def _extent(city: City, radius_km: float) -> list[float]:
    dlat = radius_km / 111.0
    dlon = radius_km / (111.0 * np.cos(np.radians(city.lat)))
    return [city.lon - dlon, city.lon + dlon, city.lat - dlat, city.lat + dlat]


def plot_sweep(sweep: ReflectivitySweep, city: City, out_path: Path | str, radius_km: float = 150.0) -> Path:
    """Plot a reflectivity sweep on a map centred on the city's settlement station."""
    import cartopy.crs as ccrs
    import cartopy.feature as cfeature
    import matplotlib

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    from metpy.plots import USCOUNTIES, ctables

    norm, cmap = ctables.registry.get_with_steps("NWSReflectivity", 5, 5)

    # Gate edges in the radar's own azimuthal-equidistant frame; cartopy reprojects.
    az_e = np.radians(_edges(sweep.azimuth))
    rng_e = _edges(sweep.range_km) * 1000.0
    x = np.sin(az_e)[:, None] * rng_e[None, :]
    y = np.cos(az_e)[:, None] * rng_e[None, :]
    radar_crs = ccrs.AzimuthalEquidistant(central_longitude=sweep.site_lon, central_latitude=sweep.site_lat)
    dbz = np.ma.masked_less(sweep.dbz, 5.0)

    proj = ccrs.LambertConformal(central_longitude=city.lon, central_latitude=city.lat)
    fig = plt.figure(figsize=(8, 9), dpi=200)  # 1600 x 1800 px
    ax = fig.add_axes([0.03, 0.15, 0.94, 0.72], projection=proj)
    ax.set_extent(_extent(city, radius_km), crs=ccrs.PlateCarree())

    ax.add_feature(cfeature.LAND.with_scale("10m"), facecolor="#f1efe9", zorder=0)
    ax.add_feature(cfeature.OCEAN.with_scale("10m"), facecolor="#d6e4ee", zorder=0)
    ax.add_feature(cfeature.LAKES.with_scale("10m"), facecolor="#d6e4ee", edgecolor="#9fb6c6", linewidth=0.4, zorder=0)
    mesh = ax.pcolormesh(x, y, dbz, cmap=cmap, norm=norm, transform=radar_crs, zorder=1)
    ax.add_feature(USCOUNTIES.with_scale("5m"), edgecolor="#8a8a8a", linewidth=0.35, zorder=2)
    ax.add_feature(cfeature.STATES.with_scale("10m"), edgecolor="#333333", linewidth=1.0, zorder=2)
    ax.add_feature(cfeature.COASTLINE.with_scale("10m"), edgecolor="#333333", linewidth=0.8, zorder=2)

    pc = ccrs.PlateCarree()
    halo = dict(boxstyle="round,pad=0.3", facecolor="white", edgecolor="#333333", linewidth=0.6, alpha=0.9)
    side = -1 if sweep.site_lon > city.lon else 1  # put the station label away from the radar
    ax.plot(city.lon, city.lat, marker="*", markersize=22, color="white", markeredgecolor="black",
            markeredgewidth=1.6, transform=pc, zorder=6)
    ax.annotate(f"{city.station} · {city.station_name}", (city.lon, city.lat), xycoords=pc._as_mpl_transform(ax),
                xytext=(14 * side, 10), textcoords="offset points", fontsize=12, fontweight="bold",
                ha="left" if side > 0 else "right", va="bottom", bbox=halo, zorder=6)
    ax.plot(sweep.site_lon, sweep.site_lat, marker="^", markersize=11, color="black", markeredgecolor="white",
            transform=pc, zorder=6)
    ax.annotate(f"{sweep.site}", (sweep.site_lon, sweep.site_lat), xycoords=pc._as_mpl_transform(ax),
                xytext=(0, -12), textcoords="offset points", fontsize=10.5, ha="center", va="top",
                bbox=halo, zorder=6, annotation_clip=True)

    local = sweep.time.astimezone(ZoneInfo(city.tz))
    fig.text(0.03, 0.965, f"{city.name} ({city.code}) — {sweep.site} radar", fontsize=20, fontweight="bold",
             ha="left", va="top")
    fig.text(0.03, 0.925, f"{sweep.elevation:.1f}° base reflectivity (Level II)", fontsize=13, ha="left", va="top")
    fig.text(0.03, 0.897, f"{sweep.time:%Y-%m-%d %H:%M} UTC  ·  {local:%a %b %-d, %-I:%M %p %Z}",
             fontsize=13, ha="left", va="top", color="#333333")

    fig.text(0.97, 0.897, "☆ settlement station   ▲ radar", fontsize=11, ha="right", va="top", color="#333333")

    cax = fig.add_axes([0.10, 0.105, 0.80, 0.02])
    cb = fig.colorbar(mesh, cax=cax, orientation="horizontal", ticks=range(5, 80, 10))
    cb.set_label("Reflectivity (dBZ)", fontsize=12)
    cb.ax.tick_params(labelsize=11)
    fig.text(0.5, 0.018, "Data: NOAA NEXRAD via AWS Open Data  ·  Decoded and plotted with MetPy",
             fontsize=10.5, ha="center", va="bottom", color="#555555")

    out_path = Path(out_path)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(out_path, facecolor="white")
    plt.close(fig)
    return out_path


# -------------------------------------------------------------------------- render


def _distance_km(lat1: float, lon1: float, lat2: float, lon2: float) -> float:
    p1, p2 = np.radians(lat1), np.radians(lat2)
    dp, dl = p2 - p1, np.radians(lon2 - lon1)
    a = np.sin(dp / 2) ** 2 + np.cos(p1) * np.cos(p2) * np.sin(dl / 2) ** 2
    return float(6371.0 * 2 * np.arcsin(np.sqrt(a)))


def render_city(
    code: str,
    when: datetime | None = None,
    out_dir: Path | str = DEFAULT_OUT,
    cache_dir: Path = DEFAULT_CACHE,
    radius_km: float = 150.0,
) -> Path:
    """Fetch, decode and plot one city. Returns the PNG path."""
    city = CITIES[code]
    when = _utc(when) if when is not None else None
    s3 = _s3()
    # A volume cut by a radar restart can start mid-VCP: the "lowest" sweep is then
    # a partial sector or a higher tilt. Fall back to the next-nearest volume.
    for scan_time, key in nearest_scans(city.radar, when, 3, s3):
        log.info("%s: %s (%s)", code, key, scan_time.isoformat())
        sweep = lowest_tilt_reflectivity(download(key, cache_dir, s3))
        if sweep.coverage >= 0.9 and sweep.elevation <= 1.0:
            break
        log.warning("%s: %s is incomplete (%.0f%% of azimuths, %.1f° lowest tilt); trying the next volume",
                    code, key, sweep.coverage * 100, sweep.elevation)
    else:
        raise LookupError(f"{city.radar}: no complete volume among the 3 nearest")
    if when is not None and abs(sweep.time - when) > timedelta(minutes=30):
        log.warning("%s: using the %s scan, %d min from the requested %s", code, f"{sweep.time:%H:%MZ}",
                    abs(sweep.time - when).total_seconds() // 60, f"{when:%Y-%m-%d %H:%MZ}")

    # Only plot gates out to the map's far corner (sqrt 2 * radius past the city).
    sweep = sweep.crop(_distance_km(city.lat, city.lon, sweep.site_lat, sweep.site_lon) + radius_km * 1.5)

    out = Path(out_dir) / f"{code}_{sweep.site}_{sweep.time:%Y%m%dT%H%MZ}.png"
    return plot_sweep(sweep, city, out, radius_km)
