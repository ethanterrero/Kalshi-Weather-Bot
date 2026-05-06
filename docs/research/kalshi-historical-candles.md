# Kalshi Historical Candle Backfill — Research Report

Researched 2026-05-05 against the live Kalshi developer docs at `docs.kalshi.com`
(the old `trading-api.readme.io` host now 302-redirects there).

## Endpoints found

Kalshi splits candle data across two endpoints, with the boundary defined by a
server-side cutoff timestamp.

### 1. Live: `GET /trade-api/v2/series/{series_ticker}/markets/{ticker}/candlesticks`

The endpoint we already use. Required query params:
- `start_ts` (unix seconds, inclusive)
- `end_ts`   (unix seconds, inclusive)
- `period_interval` minutes — only `1`, `60`, `1440` are valid
- optional `include_latest_before_start` (bool, prepends a synthetic candle)

Lookback: docs do **not** publish an explicit max range or max-rows-per-call.
What they do say is that "candlesticks for markets that settled before the
historical cutoff are only available via the historical endpoint" — i.e. the
live route's effective lookback is bounded by the cutoff, not by a fixed
window. The target live window is roughly **3 months**.

### 2. Historical: `GET /trade-api/v2/historical/markets/{ticker}/candlesticks`

Same `start_ts` / `end_ts` / `period_interval` shape and the same response
schema (OHLC for `price`, `yes_bid`, `yes_ask`, plus `volume` and
`open_interest` per bucket). Note the path drops `series/{series_ticker}` —
historical addresses markets directly by ticker. All three intervals
(1m / 60m / 1440m) are accepted on both endpoints.

### 3. `GET /trade-api/v2/historical/cutoff`

Returns three timestamps: `market_settled_ts`, `trades_created_ts`,
`orders_updated_ts`. For candle backfill, `market_settled_ts` is the one that
matters: any market that settled before it must be queried via the
`/historical/...` route. Cursor pagination is the same as on live endpoints.

Docs do not document a per-call candle cap or a max range; would need to test
with `?start_ts=` set to a year ago and `period_interval=1` to see whether the
server truncates or paginates.

## Third-party archives

- **Kalshi GitHub org** (`github.com/Kalshi`): no historical OHLC dump. The
  only data-adjacent repo is `tools-and-analysis` (Jupyter notebooks) and a
  Python starter. No S3 bucket or research dataset is published.
- **Kingsets** (`kingsets.com`): bulk CSV / BigQuery for series, events,
  markets, trades — but **no candlestick/OHLC product**, only raw trades.
  Free tier is 30 days, weather-market coverage not confirmed.
- **Lychee** (`lycheedata.com`): advertises a 36 GB historical Kalshi dataset
  with CSV/JSON/XLSX export. Says "historical prices" and "volume over time"
  but does not explicitly confirm per-minute OHLC; pricing not public.
- **mickbransfield/kalshi**: hobby Python scripts; snapshots only, not OHLC.

No first-party S3 dump, Kaggle mirror, or Hugging Face dataset turned up.

## Recommended approach

**Use the two-endpoint pattern with chunked queries**, gated on
`/historical/cutoff`:

1. On startup, call `GET /historical/cutoff` once and cache `market_settled_ts`.
2. For each market we want to backfill, choose the route by comparing the
   market's settlement time to the cutoff:
   - settled after cutoff (or still open) -> `/series/.../candlesticks`
   - settled before cutoff -> `/historical/markets/{ticker}/candlesticks`
3. Walk `[start_ts, end_ts]` in fixed chunks (e.g. 7 days at `period_interval=1`,
   90 days at `period_interval=60`) until we hit a real response cap, then tune.
4. Respect the token-bucket rate limiter (default 10 tokens/req); add
   exponential backoff on 429.

This is strictly better than the "ship the static table for now" fallback and
avoids the cost/coverage risk of a third-party vendor. No scraping cron is
needed — the endpoints serve what we want directly.

## Open questions

- Max candles per response and max `[start_ts, end_ts]` range per call (docs
  silent — empirical test required).
- Token cost of `/historical/...` calls vs the live route (docs say "see each
  operation's reference page"; we did not see it called out explicitly).
- Whether `include_latest_before_start` exists on the historical route or only
  on the live one (docs only show it on live).
- Confirm the `/historical/...` path is reachable on the demo / sandbox
  environment we use for replay, not just production.

## Sources

- https://docs.kalshi.com/api-reference/historical/get-historical-market-candlesticks
- https://docs.kalshi.com/api-reference/market/get-market-candlesticks
- https://docs.kalshi.com/api-reference/historical/get-historical-cutoff-timestamps
- https://docs.kalshi.com/getting_started/historical_data
- https://docs.kalshi.com/getting_started/rate_limits
- https://docs.kalshi.com/changelog
- https://github.com/Kalshi
- https://github.com/mickbransfield/kalshi
- https://kingsets.com
- https://lycheedata.com/guides/kalshi-historical-data
