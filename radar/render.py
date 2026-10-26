"""Render NEXRAD base-reflectivity PNGs for the cities the bot trades.

    python radar/render.py --city NY
    python radar/render.py --all --time 2026-10-20T19:00Z --out radar/out/
"""

from __future__ import annotations

import argparse
import logging
import sys
from datetime import datetime, timezone
from pathlib import Path

from nexrad import CITIES, DEFAULT_OUT, render_city


def parse_time(s: str) -> datetime:
    t = datetime.fromisoformat(s.replace("Z", "+00:00").replace("z", "+00:00"))
    return t if t.tzinfo else t.replace(tzinfo=timezone.utc)


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    which = p.add_mutually_exclusive_group(required=True)
    which.add_argument("--city", choices=sorted(CITIES), type=str.upper, help="Kalshi city code")
    which.add_argument("--all", action="store_true", help="render all five cities")
    p.add_argument("--time", type=parse_time, help="UTC time, e.g. 2026-10-20T19:00Z (default: latest scan)")
    p.add_argument("--out", type=Path, default=DEFAULT_OUT, help="output directory (default: radar/out/)")
    p.add_argument("--radius-km", type=float, default=150.0, help="map half-width around the station")
    args = p.parse_args(argv)

    logging.basicConfig(level=logging.WARNING, format="%(levelname)s %(name)s: %(message)s")
    logging.getLogger("nexrad").setLevel(logging.INFO)
    logging.getLogger("metpy.io").setLevel(logging.ERROR)  # "Unknown message: 32" etc. are harmless
    failed = 0
    for code in sorted(CITIES) if args.all else [args.city]:
        try:
            print(render_city(code, args.time, args.out, radius_km=args.radius_km))
        except Exception as e:  # keep going so one bad site doesn't sink --all
            logging.error("%s: %s", code, e)
            failed += 1
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
