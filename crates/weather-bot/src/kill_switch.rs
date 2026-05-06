//! Dynamic operator-facing kill switch.
//!
//! Distinct from the executor's static `never_send=true` development guard
//! (which requires a code edit). This is the "halt the bot in 5 seconds
//! without redeploying" path. Each check is cheap and fails in a different
//! way:
//!
//! - **File flag** (`./KILL`) — drop a file on disk, halts on next pass.
//!   Survives bot restart unless removed; durable.
//! - **Env var** (`WEATHER_BOT_KILL`) — set in the supervising unit and
//!   `SIGHUP`/restart, useful when the operator can't write to CWD.
//! - **Drawdown soft-kill** — bot refuses new orders after a configured
//!   24h realised loss. Static kill is the operator protecting the bot;
//!   soft kill is the bot protecting the operator from a bad day.
//!
//! SIGTERM handling lives in the bot's main loop, not here — it's an
//! always-on cancellation signal, not a configurable check.
//!
//! All three return reasons rather than booleans so the per-pass log line
//! can show *why* the bot is sitting out (operator vs auto vs env).

use std::path::Path;

use rust_decimal::Decimal;
use weather_config::KillSwitchConfig;

/// Outcome of a per-pass kill-switch evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KillState {
    /// All gates clear — strategy may proceed.
    Active,
    /// At least one gate fired. Strategy must skip new order placement.
    /// Returns the first firing reason; if multiple gates fire, the
    /// caller can re-evaluate after handling the first.
    Halted(KillReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KillReason {
    /// `kill_file_path` exists on disk.
    FileFlag(String),
    /// `kill_env_var` is set and non-empty (and not literal `0`/`false`).
    EnvVar { name: String, value: String },
    /// 24h realised loss exceeded `max_drawdown_24h_usd`.
    DrawdownExceeded {
        loss_usd: Decimal,
        limit_usd: Decimal,
    },
}

impl std::fmt::Display for KillReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KillReason::FileFlag(path) => write!(f, "file flag present at {}", path),
            KillReason::EnvVar { name, value } => {
                write!(f, "env var {}={} (non-empty truthy)", name, value)
            }
            KillReason::DrawdownExceeded {
                loss_usd,
                limit_usd,
            } => {
                write!(
                    f,
                    "24h realised loss {} exceeds limit {}",
                    loss_usd, limit_usd
                )
            }
        }
    }
}

impl KillReason {
    /// Stable tag for structured logs / JSONL.
    pub fn tag(&self) -> &'static str {
        match self {
            KillReason::FileFlag(_) => "kill_file_flag",
            KillReason::EnvVar { .. } => "kill_env_var",
            KillReason::DrawdownExceeded { .. } => "kill_drawdown",
        }
    }
}

/// Evaluate every gate in `cfg` against the current process state.
///
/// `realised_loss_24h_usd` is the cumulative realised loss inside the
/// trailing 24h window. Pass `Decimal::ZERO` (or `None`) when the bot has
/// no fill ledger to evaluate against — dry-run is the canonical case.
pub fn evaluate(cfg: &KillSwitchConfig, realised_loss_24h_usd: Option<Decimal>) -> KillState {
    if !cfg.enabled {
        return KillState::Active;
    }

    if Path::new(&cfg.kill_file_path).exists() {
        return KillState::Halted(KillReason::FileFlag(cfg.kill_file_path.clone()));
    }

    if !cfg.kill_env_var.is_empty() {
        if let Ok(raw) = std::env::var(&cfg.kill_env_var) {
            if is_truthy(&raw) {
                return KillState::Halted(KillReason::EnvVar {
                    name: cfg.kill_env_var.clone(),
                    value: raw,
                });
            }
        }
    }

    if cfg.max_drawdown_24h_usd > Decimal::ZERO {
        if let Some(loss) = realised_loss_24h_usd {
            if loss >= cfg.max_drawdown_24h_usd {
                return KillState::Halted(KillReason::DrawdownExceeded {
                    loss_usd: loss,
                    limit_usd: cfg.max_drawdown_24h_usd,
                });
            }
        }
    }

    KillState::Active
}

/// Treat empty / `0` / `false` (any case) / whitespace-only as "not set".
/// Anything else is truthy. Lets the operator unset the kill switch by
/// `WEATHER_BOT_KILL=` (empty) without unsetting the env var entirely.
fn is_truthy(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.eq_ignore_ascii_case("0")
        || trimmed.eq_ignore_ascii_case("false")
        || trimmed.eq_ignore_ascii_case("no")
        || trimmed.eq_ignore_ascii_case("off")
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use std::sync::Mutex;

    // Env-var manipulation in Rust tests is *process-global* — every test
    // in this module shares the same env. A single mutex serialises any
    // test that touches `WEATHER_BOT_KILL` (or its suffixed friends) so
    // they don't stomp each other when run in parallel.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn cfg_with(file: &str, env: &str, drawdown: Decimal) -> KillSwitchConfig {
        KillSwitchConfig {
            enabled: true,
            kill_file_path: file.to_string(),
            kill_env_var: env.to_string(),
            max_drawdown_24h_usd: drawdown,
        }
    }

    fn unique_path(label: &str) -> String {
        let p = std::env::temp_dir().join(format!(
            "weather-bot-kill-{}-{}-{}",
            label,
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn disabled_config_short_circuits_all_gates() {
        let _guard = ENV_LOCK.lock().unwrap();
        let var = "WEATHER_BOT_KILL_DISABLED_TEST";
        std::env::set_var(var, "1");
        let cfg = KillSwitchConfig {
            enabled: false,
            kill_file_path: "/tmp/never-exists-please".into(),
            kill_env_var: var.into(),
            max_drawdown_24h_usd: dec!(1),
        };
        assert_eq!(evaluate(&cfg, Some(dec!(1000))), KillState::Active);
        std::env::remove_var(var);
    }

    #[test]
    fn missing_file_and_env_returns_active() {
        let _guard = ENV_LOCK.lock().unwrap();
        let var = "WEATHER_BOT_KILL_MISSING_TEST";
        std::env::remove_var(var);
        let cfg = cfg_with("/tmp/never-exists-please", var, Decimal::ZERO);
        assert_eq!(evaluate(&cfg, None), KillState::Active);
    }

    #[test]
    fn file_flag_halts() {
        let path = unique_path("file");
        std::fs::write(&path, b"halt").unwrap();
        let cfg = cfg_with(&path, "", Decimal::ZERO);
        match evaluate(&cfg, None) {
            KillState::Halted(KillReason::FileFlag(p)) => assert_eq!(p, path),
            other => panic!("expected FileFlag halt, got {:?}", other),
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn env_var_truthy_value_halts() {
        let _guard = ENV_LOCK.lock().unwrap();
        let var = "WEATHER_BOT_KILL_TRUTHY_TEST";
        std::env::set_var(var, "yes");
        let cfg = cfg_with("/tmp/never-exists-please", var, Decimal::ZERO);
        match evaluate(&cfg, None) {
            KillState::Halted(KillReason::EnvVar { name, value }) => {
                assert_eq!(name, var);
                assert_eq!(value, "yes");
            }
            other => panic!("expected EnvVar halt, got {:?}", other),
        }
        std::env::remove_var(var);
    }

    #[test]
    fn env_var_falsy_values_do_not_halt() {
        let _guard = ENV_LOCK.lock().unwrap();
        let var = "WEATHER_BOT_KILL_FALSY_TEST";
        let cfg = cfg_with("/tmp/never-exists-please", var, Decimal::ZERO);
        for falsy in ["0", "false", "FALSE", "no", "off", "  ", ""] {
            std::env::set_var(var, falsy);
            assert_eq!(
                evaluate(&cfg, None),
                KillState::Active,
                "value {:?} should be falsy",
                falsy
            );
        }
        std::env::remove_var(var);
    }

    #[test]
    fn drawdown_at_or_above_limit_halts() {
        let _guard = ENV_LOCK.lock().unwrap();
        let var = "WEATHER_BOT_KILL_DRAWDOWN_TEST";
        std::env::remove_var(var);
        let cfg = cfg_with("/tmp/never-exists-please", var, dec!(50));
        match evaluate(&cfg, Some(dec!(50))) {
            KillState::Halted(KillReason::DrawdownExceeded {
                loss_usd,
                limit_usd,
            }) => {
                assert_eq!(loss_usd, dec!(50));
                assert_eq!(limit_usd, dec!(50));
            }
            other => panic!("expected DrawdownExceeded halt, got {:?}", other),
        }
    }

    #[test]
    fn drawdown_below_limit_is_active() {
        let _guard = ENV_LOCK.lock().unwrap();
        let var = "WEATHER_BOT_KILL_DRAWDOWN_OK_TEST";
        std::env::remove_var(var);
        let cfg = cfg_with("/tmp/never-exists-please", var, dec!(50));
        assert_eq!(evaluate(&cfg, Some(dec!(49.99))), KillState::Active);
    }

    #[test]
    fn drawdown_disabled_when_limit_zero() {
        let _guard = ENV_LOCK.lock().unwrap();
        let var = "WEATHER_BOT_KILL_DRAWDOWN_ZERO_TEST";
        std::env::remove_var(var);
        let cfg = cfg_with("/tmp/never-exists-please", var, Decimal::ZERO);
        // Even with a huge realised loss reported, the soft kill is off.
        assert_eq!(evaluate(&cfg, Some(dec!(10_000))), KillState::Active);
    }

    #[test]
    fn file_flag_takes_precedence_over_env() {
        // File should fire first (cheaper to check + matches function order).
        let _guard = ENV_LOCK.lock().unwrap();
        let var = "WEATHER_BOT_KILL_PRIORITY_TEST";
        std::env::set_var(var, "1");

        let path = unique_path("priority");
        std::fs::write(&path, b"halt").unwrap();
        let cfg = cfg_with(&path, var, Decimal::ZERO);
        match evaluate(&cfg, None) {
            KillState::Halted(KillReason::FileFlag(_)) => {}
            other => panic!("file flag should win over env var, got {:?}", other),
        }
        std::fs::remove_file(&path).ok();
        std::env::remove_var(var);
    }

    #[test]
    fn kill_reason_display_and_tag_are_stable() {
        let r = KillReason::FileFlag("/tmp/KILL".into());
        assert_eq!(r.tag(), "kill_file_flag");
        assert!(format!("{}", r).contains("/tmp/KILL"));

        let r = KillReason::EnvVar {
            name: "WEATHER_BOT_KILL".into(),
            value: "1".into(),
        };
        assert_eq!(r.tag(), "kill_env_var");

        let r = KillReason::DrawdownExceeded {
            loss_usd: dec!(60),
            limit_usd: dec!(50),
        };
        assert_eq!(r.tag(), "kill_drawdown");
    }
}
