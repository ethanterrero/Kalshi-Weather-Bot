use anyhow::{Context, Result};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, patch};
use axum::{Json, Router};
use chrono::{DateTime, NaiveDate, Utc};
use clap::Parser;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tokio::sync::RwLock;
use tower_http::services::ServeDir;
use tracing::{info, warn};
use weather_backtest::{read_decisions, DecisionRow};
const TRADE_CACHE_TTL_SECS: i64 = 15;

#[derive(Debug, Parser)]
#[command(
    name = "weather-dashboard",
    about = "Lightweight dashboard for weather-bot trade tracking"
)]
struct Args {
    /// Host interface to bind.
    #[arg(long, default_value = "127.0.0.1")]
    host: IpAddr,
    /// HTTP port.
    #[arg(long, default_value_t = 8787)]
    port: u16,
    /// Directory containing weather-bot JSONL decision files.
    #[arg(long, default_value = "logs/decisions")]
    decisions_dir: PathBuf,
    /// File used to persist dashboard notes and status overrides.
    #[arg(long, default_value = "logs/dashboard-trade-journal.json")]
    journal_path: PathBuf,
    /// Number of most-recent JSONL files to read per refresh.
    #[arg(long, default_value_t = 3)]
    max_files: usize,
}

#[derive(Clone)]
struct AppState {
    decisions_dir: PathBuf,
    journal_path: PathBuf,
    max_files: usize,
    journal: Arc<RwLock<TradeJournal>>,
    trade_cache: Arc<RwLock<Option<TradeCache>>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TradeJournal {
    #[serde(default)]
    entries: BTreeMap<String, TradeJournalEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TradeJournalEntry {
    status: Option<TradeStatus>,
    notes: String,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TradeStatus {
    Held,
    Closed,
    Watch,
}

#[derive(Debug, Clone, Serialize)]
struct TradeSummary {
    made: usize,
    held: usize,
    closed: usize,
    watch: usize,
}

#[derive(Debug, Clone, Serialize)]
struct DashboardResponse {
    generated_at: DateTime<Utc>,
    summary: TradeSummary,
    trades: Vec<TradeView>,
}

#[derive(Debug, Clone, Serialize)]
struct TradeView {
    id: String,
    ticker: String,
    city: String,
    side: String,
    status: TradeStatus,
    status_source: String,
    resolution_date: NaiveDate,
    first_seen: DateTime<Utc>,
    last_seen: DateTime<Utc>,
    emissions: usize,
    execution_outcome: Option<String>,
    risk_outcome: Option<String>,
    sigma_source: String,
    contracts: u32,
    limit_price: Decimal,
    model_p_yes: Decimal,
    market_p_implied: Option<Decimal>,
    raw_edge: Option<Decimal>,
    net_ev_per_contract: Option<Decimal>,
    notes: String,
    updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
struct TradesQuery {
    status: Option<TradeStatus>,
    q: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UpdateTradePayload {
    status: Option<TradeStatus>,
    notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct TradeKey {
    ticker: String,
    resolution_date: NaiveDate,
    side: String,
    limit_price: String,
    contracts: u32,
}

#[derive(Debug, Clone)]
struct TradeAggregate {
    id: String,
    ticker: String,
    city: String,
    side: String,
    resolution_date: NaiveDate,
    first_seen: DateTime<Utc>,
    last_seen: DateTime<Utc>,
    emissions: usize,
    execution_outcome: Option<String>,
    risk_outcome: Option<String>,
    sigma_source: String,
    contracts: u32,
    limit_price: Decimal,
    model_p_yes: Decimal,
    market_p_implied: Option<Decimal>,
    raw_edge: Option<Decimal>,
    net_ev_per_contract: Option<Decimal>,
}

#[derive(Debug, Clone)]
struct TradeCache {
    generated_at: DateTime<Utc>,
    aggregates: Vec<TradeAggregate>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    init_tracing();

    let journal = TradeJournal::load(&args.journal_path).await?;
    let state = AppState {
        decisions_dir: args.decisions_dir.clone(),
        journal_path: args.journal_path.clone(),
        max_files: args.max_files,
        journal: Arc::new(RwLock::new(journal)),
        trade_cache: Arc::new(RwLock::new(None)),
    };

    let static_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static");
    let app = Router::new()
        .route("/api/health", get(health))
        .route("/api/trades", get(list_trades))
        .route("/api/summary", get(get_summary))
        .route("/api/trades/:id", patch(update_trade))
        .nest_service("/", ServeDir::new(static_dir))
        .with_state(state);

    let addr = SocketAddr::from((args.host, args.port));
    info!(
        "weather-dashboard listening on http://{} (decisions={}, journal={})",
        addr,
        args.decisions_dir.display(),
        args.journal_path.display()
    );
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({
        "ok": true,
        "service": "weather-dashboard",
        "ts": Utc::now(),
    }))
}

async fn get_summary(State(state): State<AppState>) -> Result<Json<TradeSummary>, ApiError> {
    let dashboard = build_dashboard(&state, None).await?;
    Ok(Json(dashboard.summary))
}

async fn list_trades(
    State(state): State<AppState>,
    Query(query): Query<TradesQuery>,
) -> Result<Json<DashboardResponse>, ApiError> {
    let dashboard = build_dashboard(&state, Some(query)).await?;
    Ok(Json(dashboard))
}

async fn update_trade(
    Path(id): Path<String>,
    State(state): State<AppState>,
    Json(payload): Json<UpdateTradePayload>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if payload.status.is_none() && payload.notes.is_none() {
        return Err(ApiError::bad_request(
            "Provide at least one field: status or notes",
        ));
    }

    let mut journal = state.journal.write().await;
    let entry = journal
        .entries
        .entry(id.clone())
        .or_insert_with(|| TradeJournalEntry {
            status: None,
            notes: String::new(),
            updated_at: Utc::now(),
        });

    if let Some(status) = payload.status {
        entry.status = Some(status);
    }
    if let Some(notes) = payload.notes {
        entry.notes = notes.trim().to_string();
    }
    entry.updated_at = Utc::now();
    let updated_at = entry.updated_at;
    journal.persist(&state.journal_path).await?;

    Ok(Json(serde_json::json!({
        "ok": true,
        "id": id,
        "updated_at": updated_at,
    })))
}

async fn build_dashboard(
    state: &AppState,
    query: Option<TradesQuery>,
) -> Result<DashboardResponse, ApiError> {
    let aggregates = cached_aggregates(state).await?;
    let now = Utc::now().date_naive();

    let journal = state.journal.read().await;
    let mut trades = Vec::with_capacity(aggregates.len());
    for trade in aggregates {
        let journal_entry = journal.entries.get(&trade.id);
        let auto_status = if trade.resolution_date < now {
            TradeStatus::Closed
        } else {
            TradeStatus::Held
        };
        let status = journal_entry
            .and_then(|entry| entry.status)
            .unwrap_or(auto_status);
        let status_source = if journal_entry.and_then(|entry| entry.status).is_some() {
            "manual".to_string()
        } else {
            "auto".to_string()
        };
        let notes = journal_entry
            .map(|entry| entry.notes.clone())
            .unwrap_or_default();
        let updated_at = journal_entry.map(|entry| entry.updated_at);

        trades.push(TradeView {
            id: trade.id,
            ticker: trade.ticker,
            city: trade.city,
            side: trade.side,
            status,
            status_source,
            resolution_date: trade.resolution_date,
            first_seen: trade.first_seen,
            last_seen: trade.last_seen,
            emissions: trade.emissions,
            execution_outcome: trade.execution_outcome,
            risk_outcome: trade.risk_outcome,
            sigma_source: trade.sigma_source,
            contracts: trade.contracts,
            limit_price: trade.limit_price,
            model_p_yes: trade.model_p_yes,
            market_p_implied: trade.market_p_implied,
            raw_edge: trade.raw_edge,
            net_ev_per_contract: trade.net_ev_per_contract,
            notes,
            updated_at,
        });
    }

    let summary = summarize(&trades);
    let filtered_trades = filter_trades(trades, query);
    Ok(DashboardResponse {
        generated_at: Utc::now(),
        summary,
        trades: filtered_trades,
    })
}

async fn cached_aggregates(state: &AppState) -> Result<Vec<TradeAggregate>, ApiError> {
    {
        let cache = state.trade_cache.read().await;
        if let Some(cache) = cache.as_ref() {
            let age_secs = Utc::now()
                .signed_duration_since(cache.generated_at)
                .num_seconds();
            if age_secs <= TRADE_CACHE_TTL_SECS {
                return Ok(cache.aggregates.clone());
            }
        }
    }

    let rows = load_trade_rows(&state.decisions_dir, state.max_files).await?;
    let aggregates = aggregate_trades(rows);

    let mut cache = state.trade_cache.write().await;
    *cache = Some(TradeCache {
        generated_at: Utc::now(),
        aggregates: aggregates.clone(),
    });
    Ok(aggregates)
}

fn filter_trades(mut trades: Vec<TradeView>, query: Option<TradesQuery>) -> Vec<TradeView> {
    if let Some(q) = query {
        if let Some(status) = q.status {
            trades.retain(|trade| trade.status == status);
        }
        if let Some(search) = q.q {
            let needle = search.to_ascii_lowercase();
            trades.retain(|trade| {
                trade.ticker.to_ascii_lowercase().contains(&needle)
                    || trade.city.to_ascii_lowercase().contains(&needle)
                    || trade.side.to_ascii_lowercase().contains(&needle)
            });
        }
    }
    trades
}

fn summarize(trades: &[TradeView]) -> TradeSummary {
    let mut summary = TradeSummary {
        made: trades.len(),
        held: 0,
        closed: 0,
        watch: 0,
    };
    for trade in trades {
        match trade.status {
            TradeStatus::Held => summary.held += 1,
            TradeStatus::Closed => summary.closed += 1,
            TradeStatus::Watch => summary.watch += 1,
        }
    }
    summary
}

async fn load_trade_rows(
    decisions_dir: &FsPath,
    max_files: usize,
) -> Result<Vec<DecisionRow>, ApiError> {
    let mut entries = fs::read_dir(decisions_dir).await.map_err(|err| {
        ApiError::internal_with_context(err, "failed to read decisions directory")
    })?;

    let mut paths = Vec::new();
    while let Some(entry) = entries.next_entry().await.map_err(|err| {
        ApiError::internal_with_context(err, "failed to iterate decisions directory")
    })? {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            paths.push(path);
        }
    }
    paths.sort();
    if max_files > 0 && paths.len() > max_files {
        paths = paths.split_off(paths.len() - max_files);
    }

    let mut rows = Vec::new();
    for path in paths {
        match read_decisions(&path) {
            Ok(mut chunk) => rows.append(&mut chunk),
            Err(err) => warn!("skipping malformed JSONL file {}: {}", path.display(), err),
        }
    }

    rows.retain(|row| row.decision.eq_ignore_ascii_case("trade"));
    rows.sort_by_key(|row| row.ts);
    Ok(rows)
}

fn aggregate_trades(rows: Vec<DecisionRow>) -> Vec<TradeAggregate> {
    let mut by_key: BTreeMap<TradeKey, TradeAggregate> = BTreeMap::new();

    for row in rows {
        let Some(key) = trade_key_from_row(&row) else {
            continue;
        };
        let id = trade_id(&key);
        if let Some(existing) = by_key.get_mut(&key) {
            existing.last_seen = row.ts;
            existing.emissions += 1;
            existing.execution_outcome = row.execution_outcome.clone();
            existing.risk_outcome = row.risk_outcome.clone();
            existing.model_p_yes = row.model_p_yes;
            existing.market_p_implied = row.market_p_implied;
            existing.raw_edge = row.raw_edge;
            existing.net_ev_per_contract = row.net_ev_per_contract;
            existing.sigma_source = row.sigma_source;
            continue;
        }

        by_key.insert(
            key,
            TradeAggregate {
                id,
                ticker: row.ticker,
                city: row.city,
                side: row.side.unwrap_or_else(|| "unknown".to_string()),
                resolution_date: row.resolution_date,
                first_seen: row.ts,
                last_seen: row.ts,
                emissions: 1,
                execution_outcome: row.execution_outcome,
                risk_outcome: row.risk_outcome,
                sigma_source: row.sigma_source,
                contracts: row.contracts.unwrap_or_default(),
                limit_price: row.limit_price.unwrap_or_default(),
                model_p_yes: row.model_p_yes,
                market_p_implied: row.market_p_implied,
                raw_edge: row.raw_edge,
                net_ev_per_contract: row.net_ev_per_contract,
            },
        );
    }

    let mut out: Vec<_> = by_key.into_values().collect();
    out.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
    out
}

fn trade_key_from_row(row: &DecisionRow) -> Option<TradeKey> {
    let side = row.side.as_ref()?.to_ascii_lowercase();
    let limit_price = row.limit_price?.normalize().to_string();
    let contracts = row.contracts?;
    Some(TradeKey {
        ticker: row.ticker.clone(),
        resolution_date: row.resolution_date,
        side,
        limit_price,
        contracts,
    })
}

fn trade_id(key: &TradeKey) -> String {
    format!(
        "{}|{}|{}|{}|{}",
        key.ticker, key.resolution_date, key.side, key.limit_price, key.contracts
    )
}

impl TradeJournal {
    async fn load(path: &FsPath) -> Result<Self> {
        let raw = match fs::read_to_string(path).await {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read {}", path.display()))
            }
        };
        serde_json::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))
    }

    async fn persist(&self, path: &FsPath) -> Result<()> {
        // Persist atomically to avoid truncating the journal on interrupted writes.
        let json = serde_json::to_string_pretty(self)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, json).await?;
        fs::rename(tmp, path).await?;
        Ok(())
    }
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn internal_with_context(err: impl std::fmt::Display, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("{}: {}", message.into(), err),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let payload = Json(serde_json::json!({ "error": self.message }));
        (self.status, payload).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: err.to_string(),
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).with_target(false).init();
}
