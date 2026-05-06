# Paper-trade mode design

Status: research / design sketch. No code yet. Author: research pass for the
"flip `never_send` to false safely" milestone in `ROADMAP.md`.

## Background

`KalshiOrderClient` in `crates/weather-executor/src/orders.rs` already builds,
signs (RSA-PSS-SHA256), and logs `POST /portfolio/orders` payloads, but a
hard-coded `never_send=true` short-circuits before the HTTP send. Before
flipping it off we want a paper-trade mode that drives the same code path
end-to-end — cancel-and-reprice, queue position, partial fills, post-only
rejection, timeouts, idempotent retry on `client_order_id`.

Two candidate routes:

1. **Kalshi demo** — `https://demo-api.kalshi.co` (REST host
   `external-api.demo.kalshi.co`, WS `external-api-ws.demo.kalshi.co`). The
   scanner already reads from this host.
2. **Local mock** — an in-process fake client that simulates fills against a
   read-only orderbook feed. More code, fully under our control.

## Demo environment: what we know

**Confirmed from docs:**

- Demo accepts the full write surface — `CreateOrder`, `CancelOrder`,
  `AmendOrder`, `DecreaseOrder`, batch variants ([Kalshi API guide][1],
  [demo env page][2]).
- REST base: `https://external-api.demo.kalshi.co/trade-api/v2`
  ([Get Fills][3]).
- WebSocket: `wss://external-api-ws.demo.kalshi.co/trade-api/ws/v2`, same
  channels as prod including private `orderbook_delta`, `fill`,
  `market_positions`, `order_group_updates` ([WS quick start][4]).
- Credentials are *not* shared with prod ([demo env page][2]). Need a
  separate RSA key pair minted via `https://demo.kalshi.co/`, stored under
  a distinct env var.
- Demo accounts are *not* pre-funded — self-fund with mock payment methods
  ([demo account help][5]).
- All order lifecycle endpoints are v2 paths:
  `POST/GET/PUT/DELETE /portfolio/orders[/{id}]`,
  `GET /portfolio/fills`, `GET /portfolio/positions`,
  `GET /portfolio/balance`, and `GET /portfolio/orders/queue_positions`
  (price-time-priority depth per resting order — useful introspection)
  ([create order][6], [get fills][3], [queue positions][7]).

**Not documented — needs empirical test:**

- Whether demo's orderbook has *any* live counterparty liquidity (other
  demo users, Kalshi-internal makers, mirror of prod) or only your own
  resting orders. Secondary sources call demo "real-ish" or "mirrors prod"
  ([AgentBets][8], [amiable.dev][9]) but no official doc commits.
- Whether posted orders actually fill and how realistic partial-fill /
  queue-position behaviour is.
- Whether demo enforces the same rate limits, risk caps, and `post_only`
  rejection rules as prod.

Honest read: demo is *guaranteed* to exercise auth, request/response
schemas, cancel/amend lifecycle, `client_order_id` idempotency, and the WS
handshake. It is *not* guaranteed to produce realistic fills — the book may
be sparse with our own orders the only liquidity. A 30-minute manual smoke
test (post a marketable YES limit on a liquid demo market, watch the WS
`fill` channel) will resolve this faster than more reading.

## Local mock: what it would look like

A pure-Rust fake client plus a deterministic matcher. The matcher consumes
orderbook snapshots (read from demo or prod, never written) and simulates
whether our orders would have filled given queue priority and observed
trades.

- `PaperOrderBook` per ticker, fed by the existing scanner WS feed.
- `PaperLifecycle` state machine mirroring Kalshi states (`resting` →
  `partially_filled` → `filled` | `canceled` | `expired`).
- Trait abstraction so the bot swaps `Live` / `Demo` / `Paper` behind one
  interface.

Pros: deterministic replays, no creds in CI, cheap iteration counts. Cons:
simulator fidelity is our problem — partial fills and queue position are
easy to get subtly wrong, and we'd be validating against our own
assumptions.

## Recommended path

**Demo + local mock hybrid.** Demo is the only way to exercise the signed
HTTP round-trip, the `client_order_id` idempotency contract, and the real
WS message shapes — treat it as the integration environment. Layer a local
mock on top for strategy testing; the matcher belongs conceptually in
`weather-backtest` and we'll need it there anyway. The mock is where we
shake out cancel-and-reprice at high iteration counts without burning rate
limit.

Gate behind a `KalshiTransport` enum (`Live` | `Demo` | `Paper`); default
the binary to `Demo` until manual sign-off. Keep `never_send` as a
belt-and-braces kill switch on the `Live` transport only.

## Implementation sketch in Rust

In `crates/weather-executor/src/orders.rs`:

- Extract a trait `KalshiOrders` with async `place`, `cancel`, `amend`,
  `get_order`, `list_open`, `queue_positions`. Current struct becomes one
  impl.
- `KalshiTransport { Live, Demo, Paper }` parsed from `KALSHI_TRANSPORT`
  (default `Demo`). `Live` requires both the env var *and* a code-level
  `allow_real_sends()` call.
- Demo and Live share the existing impl with different `base_url` +
  `key_id`. Wrap creds in a `KalshiCreds { env, key_id, signer }` struct
  that pins all three together to prevent cross-environment leaks at the
  type level.
- `PaperOrderClient` holds `Arc<RwLock<PaperState>>` keyed by
  `client_order_id`. Transitions driven by a `MatcherTask` consuming the
  scanner orderbook stream.
- New types: `OrderId(String)`, `OrderState`, and
  `LifecycleEvent { Accepted, Resting { queue_pos }, PartialFill { filled,
  remaining }, Filled, Canceled, Rejected { reason } }`.
- Path constants in one module so demo/live diff is only the host.

In `crates/weather-bot`:

- Replace direct `KalshiOrderClient` with `Box<dyn KalshiOrders>`.
- `LifecycleWatcher` task polls `get_open_orders` + `queue_positions` on a
  2 s tick (or subscribes to WS `fill` once wired), emitting
  `LifecycleEvent`s into the per-pass log line.
- Reprice logic moves into a `RepricePolicy` struct, unit-testable against
  fake `LifecycleEvent` streams.

In `crates/weather-config`:

- `kalshi.transport: live|demo|paper` (default `demo`).
- `kalshi.demo_key_id` / `kalshi.demo_key_path` distinct from prod.

## Open questions

1. **Does demo actually fill?** Empirical test required: post a marketable
   YES limit at 99 c on a liquid demo market, watch WS `fill` for 60 s.
   Outcome decides whether demo alone suffices or whether the local mock
   becomes mandatory for partial-fill testing.
2. **Demo rate limits and risk caps** — not clearly documented as
   demo-specific. Probe with a burst before enabling reprice.
3. **WS auth scope in demo.** Confirm the demo signer covers private
   channels (`fill`, `market_positions`) — the scanner only uses public.
4. **Idempotency.** Confirm Kalshi returns the existing order (not 409)
   on `client_order_id` collision in demo. Current code mints a UUID per
   `Signal` and assumes that contract.
5. **Cleanup.** Need a `cancel_all_open` helper to leave demo accounts
   clean after test runs.
6. **Time-in-force.** `OrderRequest` has no TIF field; if demo requires
   explicit `expires_at` for non-marketable orders, find out there.

## Sources

- [1] [Kalshi API guide (write-limited endpoints)](https://docs.kalshi.com/getting_started/quick_start_create_order)
- [2] [Test In The Demo Environment](https://docs.kalshi.com/getting_started/demo_env)
- [3] [Get Fills](https://docs.kalshi.com/api-reference/portfolio/get-fills)
- [4] [Quick Start: WebSockets](https://docs.kalshi.com/getting_started/quick_start_websockets)
- [5] [How Do I Set Up and Use a Kalshi Demo Account?](https://help.kalshi.com/en/articles/13823775-demo-account)
- [6] [Quick Start: Create Your First Order](https://docs.kalshi.com/getting_started/quick_start_create_order)
- [7] [Get Queue Positions for Orders](https://docs.kalshi.com/api-reference/orders/get-queue-positions-for-orders)
- [8] [Kalshi API Guide: Python SDK Setup, RSA Auth & Demo Sandbox](https://agentbets.ai/guides/kalshi-api-guide/)
- [9] [Kalshi Demo Environment Support — amiable.dev](https://amiable.dev/blog/arbiter-bot/2026-01-22-kalshi-demo-environment/)
- [10] [Kalshi API — Help Center](https://help.kalshi.com/en/articles/13823854-kalshi-api)
- [11] [Get Positions](https://docs.kalshi.com/api-reference/portfolio/get-positions)
- [12] [API reference index (readme.io)](https://trading-api.readme.io/reference)
