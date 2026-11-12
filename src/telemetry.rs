//! Process telemetry: gateway state for `/health`, Prometheus metrics for
//! `/metrics`, the panic hook and supervised background tasks.
//!
//! Everything here is plain atomics and one `DashMap` keyed by command name,
//! so recording a command costs a few atomic operations and no allocation
//! after the first call of that command.
//!
//! Showcase copy: the per-guild economy gauge and its database loop were
//! removed from this file; everything else matches the bot's module.

use dashmap::DashMap;
use std::fmt::Write as _;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{error, warn};

/// How often the gateway monitor samples the shard runners.
const GATEWAY_POLL: Duration = Duration::from_secs(10);
/// A gateway sample older than this counts as "not connected" (monitor stuck).
const GATEWAY_STALE_SECS: i64 = 60;

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

// ---------------------------------------------------------------------------
// Gateway state
// ---------------------------------------------------------------------------

/// Last sampled Discord gateway state, shared with the web server.
pub struct GatewayState {
    connected: AtomicBool,
    /// Highest heartbeat latency across shards in ms; -1 when not known yet.
    latency_ms: AtomicI64,
    shards_total: AtomicU32,
    shards_connected: AtomicU32,
    /// Unix seconds of the last sample; 0 before the first one.
    updated_at: AtomicI64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GatewaySnapshot {
    pub connected: bool,
    pub latency_ms: Option<u64>,
    pub shards: u32,
    pub shards_connected: u32,
    /// Seconds since the last sample, None before the first one.
    pub sample_age_secs: Option<i64>,
}

impl GatewayState {
    pub const fn new() -> Self {
        Self {
            connected: AtomicBool::new(false),
            latency_ms: AtomicI64::new(-1),
            shards_total: AtomicU32::new(0),
            shards_connected: AtomicU32::new(0),
            updated_at: AtomicI64::new(0),
        }
    }

    /// Stores one sample. The gateway counts as connected only when there is
    /// at least one shard and every shard is in the `Connected` stage.
    pub fn record(
        &self,
        shards_total: u32,
        shards_connected: u32,
        latency: Option<Duration>,
        now: i64,
    ) {
        self.shards_total.store(shards_total, Ordering::Relaxed);
        self.shards_connected
            .store(shards_connected, Ordering::Relaxed);
        self.latency_ms.store(
            latency.map_or(-1, |d| d.as_millis().min(i64::MAX as u128) as i64),
            Ordering::Relaxed,
        );
        self.connected.store(
            shards_total > 0 && shards_connected == shards_total,
            Ordering::Relaxed,
        );
        self.updated_at.store(now, Ordering::Release);
    }

    pub fn snapshot_at(&self, now: i64) -> GatewaySnapshot {
        let updated_at = self.updated_at.load(Ordering::Acquire);
        let sample_age_secs = (updated_at > 0).then(|| (now - updated_at).max(0));
        let fresh = sample_age_secs.is_some_and(|age| age <= GATEWAY_STALE_SECS);
        let latency = self.latency_ms.load(Ordering::Relaxed);
        GatewaySnapshot {
            connected: fresh && self.connected.load(Ordering::Relaxed),
            latency_ms: (latency >= 0).then_some(latency as u64),
            shards: self.shards_total.load(Ordering::Relaxed),
            shards_connected: self.shards_connected.load(Ordering::Relaxed),
            sample_age_secs,
        }
    }

    pub fn snapshot(&self) -> GatewaySnapshot {
        self.snapshot_at(unix_now())
    }
}

impl Default for GatewayState {
    fn default() -> Self {
        Self::new()
    }
}

pub static GATEWAY: GatewayState = GatewayState::new();

/// Samples the shard runners every few seconds into [`GATEWAY`].
pub fn start_gateway_monitor(manager: Arc<serenity::gateway::ShardManager>) {
    spawn_supervised("gateway-monitor", move |restarted| {
        let manager = manager.clone();
        async move {
            let mut tick = loop_interval(GATEWAY_POLL, restarted);
            loop {
                tick.tick().await;
                let (total, connected, latency) = {
                    let runners = manager.runners.lock().await;
                    let total = runners.len() as u32;
                    let connected = runners
                        .values()
                        .filter(|r| {
                            matches!(r.stage, serenity::gateway::ConnectionStage::Connected)
                        })
                        .count() as u32;
                    let latency = runners.values().filter_map(|r| r.latency).max();
                    (total, connected, latency)
                };
                GATEWAY.record(total, connected, latency, unix_now());
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

/// Upper bounds (seconds) of the command latency histogram buckets.
const LATENCY_BUCKETS: [f64; 9] = [0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0];

#[derive(Default, Clone)]
struct CommandStats {
    calls: u64,
    errors: u64,
    /// Non-cumulative counts per bucket; the last slot is "+Inf".
    buckets: [u64; LATENCY_BUCKETS.len() + 1],
    observed: u64,
    sum_secs: f64,
}

/// Start time of one command, stored in poise's per-invocation data.
pub struct CommandTimer(pub Instant);

pub struct Metrics {
    started: Instant,
    commands: DashMap<String, CommandStats>,
    db_errors: AtomicU64,
    panics: AtomicU64,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            commands: DashMap::new(),
            db_errors: AtomicU64::new(0),
            panics: AtomicU64::new(0),
        }
    }

    pub fn command_started(&self, name: &str) {
        if let Some(mut s) = self.commands.get_mut(name) {
            s.calls += 1;
            return;
        }
        self.commands.entry(name.to_string()).or_default().calls += 1;
    }

    /// Records the outcome of a command; `elapsed` is None when the start
    /// time is unknown (the call is still counted as an error if it failed).
    pub fn command_finished(&self, name: &str, elapsed: Option<Duration>, failed: bool) {
        let mut s = match self.commands.get_mut(name) {
            Some(s) => s,
            None => self.commands.entry(name.to_string()).or_default(),
        };
        if failed {
            s.errors += 1;
        }
        if let Some(d) = elapsed {
            let secs = d.as_secs_f64();
            let idx = LATENCY_BUCKETS
                .iter()
                .position(|&le| secs <= le)
                .unwrap_or(LATENCY_BUCKETS.len());
            s.buckets[idx] += 1;
            s.observed += 1;
            s.sum_secs += secs;
        }
    }

    pub fn db_error(&self) {
        self.db_errors.fetch_add(1, Ordering::Relaxed);
    }

    pub fn panic_seen(&self) {
        self.panics.fetch_add(1, Ordering::Relaxed);
    }

    /// Prometheus text exposition format (version 0.0.4).
    pub fn render(&self, voice_sessions: usize, gateway: &GatewaySnapshot) -> String {
        let mut out = String::with_capacity(4096);

        gauge(
            &mut out,
            "tetbot_uptime_seconds",
            "Seconds since the process started.",
            self.started.elapsed().as_secs(),
        );
        gauge(
            &mut out,
            "tetbot_gateway_connected",
            "1 when every Discord shard is connected.",
            u8::from(gateway.connected),
        );
        if let Some(ms) = gateway.latency_ms {
            gauge(
                &mut out,
                "tetbot_gateway_latency_seconds",
                "Highest shard heartbeat latency.",
                ms as f64 / 1000.0,
            );
        }
        gauge(
            &mut out,
            "tetbot_voice_sessions_active",
            "Open voice sessions being tracked.",
            voice_sessions,
        );
        counter(
            &mut out,
            "tetbot_db_errors_total",
            "Database errors seen by commands and background tasks.",
            self.db_errors.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "tetbot_panics_total",
            "Panics caught by the panic hook.",
            self.panics.load(Ordering::Relaxed),
        );

        let mut commands: Vec<(String, CommandStats)> = self
            .commands
            .iter()
            .map(|e| (e.key().clone(), e.value().clone()))
            .collect();
        commands.sort_by(|a, b| a.0.cmp(&b.0));

        header(
            &mut out,
            "tetbot_commands_total",
            "Commands invoked.",
            "counter",
        );
        for (name, s) in &commands {
            let _ = writeln!(
                out,
                "tetbot_commands_total{{command=\"{}\"}} {}",
                escape(name),
                s.calls
            );
        }
        header(
            &mut out,
            "tetbot_command_errors_total",
            "Commands that returned an error.",
            "counter",
        );
        for (name, s) in &commands {
            let _ = writeln!(
                out,
                "tetbot_command_errors_total{{command=\"{}\"}} {}",
                escape(name),
                s.errors
            );
        }
        header(
            &mut out,
            "tetbot_command_duration_seconds",
            "Command run time.",
            "histogram",
        );
        for (name, s) in &commands {
            let name = escape(name);
            let mut cumulative = 0;
            for (i, le) in LATENCY_BUCKETS.iter().enumerate() {
                cumulative += s.buckets[i];
                let _ = writeln!(
                    out,
                    "tetbot_command_duration_seconds_bucket{{command=\"{name}\",le=\"{le}\"}} {cumulative}"
                );
            }
            let _ = writeln!(
                out,
                "tetbot_command_duration_seconds_bucket{{command=\"{name}\",le=\"+Inf\"}} {}",
                s.observed
            );
            let _ = writeln!(
                out,
                "tetbot_command_duration_seconds_sum{{command=\"{name}\"}} {}",
                s.sum_secs
            );
            let _ = writeln!(
                out,
                "tetbot_command_duration_seconds_count{{command=\"{name}\"}} {}",
                s.observed
            );
        }

        out
    }
}

pub static METRICS: LazyLock<Metrics> = LazyLock::new(Metrics::new);

fn header(out: &mut String, name: &str, help: &str, kind: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

fn gauge(out: &mut String, name: &str, help: &str, value: impl std::fmt::Display) {
    header(out, name, help, "gauge");
    let _ = writeln!(out, "{name} {value}");
}

fn counter(out: &mut String, name: &str, help: &str, value: u64) {
    header(out, name, help, "counter");
    let _ = writeln!(out, "{name} {value}");
}

/// Escapes a Prometheus label value.
fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

// ---------------------------------------------------------------------------
// Panics and supervised tasks
// ---------------------------------------------------------------------------

/// Logs every panic through `tracing` (with location) and counts it, then
/// lets the default unwinding continue. With `panic = "unwind"` a panic in a
/// Tokio task ends only that task; the process keeps running.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        METRICS.panic_seen();
        let payload = info.payload();
        let msg = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        let thread = std::thread::current();
        error!(
            thread = thread.name().unwrap_or("<unnamed>"),
            location = %location,
            "panic: {msg}"
        );
    }));
}

/// Restart pacing for [`spawn_supervised`]: the pause after a panic doubles
/// from `initial` up to `max`, and goes back to `initial` once the task has
/// run `reset_after` without panicking.
#[derive(Clone, Copy, Debug)]
pub struct Backoff {
    pub initial: Duration,
    pub max: Duration,
    pub reset_after: Duration,
}

impl Backoff {
    pub const DEFAULT: Backoff = Backoff {
        initial: Duration::from_secs(5),
        max: Duration::from_secs(300),
        reset_after: Duration::from_secs(600),
    };

    /// Pause before the next restart. `previous` is the pause used before the
    /// run that just panicked (None for the first panic), `ran_for` how long
    /// that run lasted.
    pub fn next_delay(&self, previous: Option<Duration>, ran_for: Duration) -> Duration {
        match previous {
            Some(prev) if ran_for < self.reset_after => (prev * 2).min(self.max),
            _ => self.initial,
        }
    }
}

/// Interval for a periodic background loop. On the first start it ticks at
/// once; after a restart (`restarted = true`) the first tick comes one full
/// `period` later, so a loop that panicked after doing its work does not run
/// again right away.
pub fn loop_interval(period: Duration, restarted: bool) -> tokio::time::Interval {
    let start = if restarted {
        tokio::time::Instant::now() + period
    } else {
        tokio::time::Instant::now()
    };
    tokio::time::interval_at(start, period)
}

/// Runs a background loop in its own task and restarts it after a panic, with
/// exponential backoff ([`Backoff::DEFAULT`]: 5 s doubling up to 5 min, reset
/// after 10 min without a panic). `make(restarted)` builds the loop; periodic
/// loops pass `restarted` to [`loop_interval`]. Returns when the task finishes
/// normally or is cancelled.
pub fn spawn_supervised<F, Fut>(name: &'static str, make: F)
where
    F: Fn(bool) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    spawn_supervised_with(name, Backoff::DEFAULT, make);
}

fn spawn_supervised_with<F, Fut>(name: &'static str, backoff: Backoff, make: F)
where
    F: Fn(bool) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let mut restarted = false;
        let mut delay: Option<Duration> = None;
        loop {
            let started = Instant::now();
            match tokio::spawn(make(restarted)).await {
                Ok(()) => break,
                Err(e) if e.is_panic() => {
                    let next = backoff.next_delay(delay, started.elapsed());
                    warn!("Background task '{name}' panicked; restarting in {next:?}");
                    tokio::time::sleep(next).await;
                    delay = Some(next);
                    restarted = true;
                }
                Err(_) => break,
            }
        }
    });
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn gateway_snapshot_requires_all_shards_and_fresh_sample() {
        let g = GatewayState::new();
        // No sample yet: not connected.
        assert!(!g.snapshot_at(1_000).connected);

        g.record(2, 2, Some(Duration::from_millis(42)), 1_000);
        let s = g.snapshot_at(1_010);
        assert!(s.connected);
        assert_eq!(s.latency_ms, Some(42));
        assert_eq!((s.shards, s.shards_connected), (2, 2));

        // Stale sample: the monitor stopped, so do not trust it.
        assert!(!g.snapshot_at(1_000 + GATEWAY_STALE_SECS + 1).connected);

        // One shard reconnecting.
        g.record(2, 1, None, 2_000);
        let s = g.snapshot_at(2_000);
        assert!(!s.connected);
        assert_eq!(s.latency_ms, None);

        // Zero shards is not connected either.
        g.record(0, 0, None, 3_000);
        assert!(!g.snapshot_at(3_000).connected);
    }

    #[test]
    fn metrics_render_prometheus_text() {
        let m = Metrics::new();
        m.command_started("stocks buy");
        m.command_finished("stocks buy", Some(Duration::from_millis(80)), false);
        m.command_started("stocks buy");
        m.command_finished("stocks buy", Some(Duration::from_secs(3)), true);
        m.command_started("daily");
        m.db_error();

        let gw = GatewaySnapshot {
            connected: true,
            latency_ms: Some(120),
            shards: 1,
            shards_connected: 1,
            sample_age_secs: Some(1),
        };
        let text = m.render(3, &gw);

        assert!(text.contains("tetbot_commands_total{command=\"stocks buy\"} 2"));
        assert!(text.contains("tetbot_commands_total{command=\"daily\"} 1"));
        assert!(text.contains("tetbot_command_errors_total{command=\"stocks buy\"} 1"));
        // 80 ms falls in le=0.1; 3 s in le=5; buckets are cumulative.
        assert!(text.contains(
            "tetbot_command_duration_seconds_bucket{command=\"stocks buy\",le=\"0.05\"} 0"
        ));
        assert!(text.contains(
            "tetbot_command_duration_seconds_bucket{command=\"stocks buy\",le=\"0.1\"} 1"
        ));
        assert!(
            text.contains(
                "tetbot_command_duration_seconds_bucket{command=\"stocks buy\",le=\"5\"} 2"
            )
        );
        assert!(text.contains(
            "tetbot_command_duration_seconds_bucket{command=\"stocks buy\",le=\"+Inf\"} 2"
        ));
        assert!(text.contains("tetbot_command_duration_seconds_count{command=\"stocks buy\"} 2"));
        assert!(text.contains("tetbot_db_errors_total 1"));
        assert!(text.contains("tetbot_voice_sessions_active 3"));
        assert!(text.contains("tetbot_gateway_connected 1"));
        assert!(text.contains("tetbot_gateway_latency_seconds 0.12"));
        // Every sample line has a TYPE declared before it.
        assert!(text.contains("# TYPE tetbot_command_duration_seconds histogram"));
    }

    #[test]
    fn label_values_are_escaped() {
        assert_eq!(escape("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }

    #[tokio::test]
    async fn supervised_task_restarts_after_panic() {
        use std::sync::atomic::AtomicUsize;
        let runs = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let tx = Arc::new(Mutex::new(Some(tx)));
        let r = runs.clone();
        let backoff = Backoff {
            initial: Duration::from_millis(10),
            max: Duration::from_millis(40),
            reset_after: Duration::from_secs(600),
        };
        spawn_supervised_with("test", backoff, move |restarted| {
            let r = r.clone();
            let tx = tx.clone();
            async move {
                let run = r.fetch_add(1, Ordering::SeqCst);
                // Only the restarted run is told so.
                assert_eq!(restarted, run > 0);
                if run == 0 {
                    panic!("first run fails");
                }
                if let Some(tx) = tx.lock().ok().and_then(|mut t| t.take()) {
                    let _ = tx.send(());
                }
            }
        });
        tokio::time::timeout(Duration::from_secs(60), rx)
            .await
            .expect("task restarted")
            .expect("sender kept");
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn backoff_doubles_up_to_max_and_resets_after_a_healthy_run() {
        let b = Backoff::DEFAULT;
        let quick = Duration::from_secs(1);
        let mut d = b.next_delay(None, quick);
        assert_eq!(d, Duration::from_secs(5));
        let mut seen = vec![d];
        for _ in 0..8 {
            d = b.next_delay(Some(d), quick);
            seen.push(d);
        }
        assert_eq!(
            seen.iter().map(|d| d.as_secs()).collect::<Vec<_>>(),
            vec![5, 10, 20, 40, 80, 160, 300, 300, 300]
        );
        // Ten minutes without a panic: back to the start.
        assert_eq!(
            b.next_delay(Some(Duration::from_secs(300)), Duration::from_secs(600)),
            Duration::from_secs(5)
        );
    }

    #[tokio::test]
    async fn restarted_loop_waits_one_period_before_the_first_tick() {
        let period = Duration::from_millis(300);

        let started = Instant::now();
        loop_interval(period, false).tick().await;
        assert!(
            started.elapsed() < Duration::from_millis(150),
            "first start ticks at once"
        );

        let started = Instant::now();
        loop_interval(period, true).tick().await;
        assert!(
            started.elapsed() >= Duration::from_millis(280),
            "restart waits a period"
        );
    }
}
