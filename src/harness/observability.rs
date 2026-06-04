//! Observability metrics for trajectory analysis.
//!
//! Provides Prometheus metrics for monitoring agent trajectory behavior,
//! including tool call counts, iteration counts, and duration metrics.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use prometheus::{
    Encoder, Gauge, Histogram, HistogramOpts, HistogramVec, IntCounterVec, Registry, TextEncoder,
};

/// Metrics for trajectory analysis.
#[derive(Clone)]
pub struct TrajectoryMetrics {
    registry: Registry,
    recorded_trajectories: Arc<AtomicU64>,
    /// Total number of trajectories recorded, labeled by bounded exit status.
    pub trajectories_total: IntCounterVec,
    /// Total number of tool calls, labeled by server, tool name, and success state.
    pub tool_calls_total: IntCounterVec,
    /// Average iterations per trajectory (gauge).
    pub avg_iterations_per_trajectory: Gauge,
    /// Trajectory duration in seconds.
    pub trajectory_duration_seconds: Histogram,
    /// Tool call duration in milliseconds, labeled by server and tool name.
    pub tool_call_duration_seconds: HistogramVec,
}

impl TrajectoryMetrics {
    /// Create a new TrajectoryMetrics instance and register it with the given registry.
    pub fn new(registry: &Registry) -> Self {
        let trajectories_total = IntCounterVec::new(
            prometheus::Opts::new(
                "trajectories_total",
                "Total number of trajectories recorded",
            )
            .namespace("xiaomaolv"),
            &["status"],
        )
        .expect("failed to create trajectories_total counter");

        let tool_calls_total = IntCounterVec::new(
            prometheus::Opts::new("tool_calls_total", "Total number of tool calls recorded")
                .namespace("xiaomaolv"),
            &["server", "tool", "ok"],
        )
        .expect("failed to create tool_calls_total counter");

        let avg_iterations_per_trajectory = Gauge::new(
            "xiaomaolv_avg_iterations_per_trajectory",
            "Average number of iterations per trajectory",
        )
        .expect("failed to create avg_iterations_per_trajectory gauge");

        let trajectory_duration_seconds = Histogram::with_opts(
            HistogramOpts::new(
                "xiaomaolv_trajectory_duration_seconds",
                "Duration of trajectories in seconds",
            )
            .buckets(vec![
                0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0,
            ]),
        )
        .expect("failed to create trajectory_duration_seconds histogram");

        let tool_call_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "tool_call_duration_seconds",
                "Duration of tool calls in seconds",
            )
            .namespace("xiaomaolv")
            .buckets(vec![
                0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
            ]),
            &["server", "tool", "ok"],
        )
        .expect("failed to create tool_call_duration_seconds histogram");

        registry
            .register(Box::new(trajectories_total.clone()))
            .expect("failed to register trajectories_total");
        registry
            .register(Box::new(tool_calls_total.clone()))
            .expect("failed to register tool_calls_total");
        registry
            .register(Box::new(avg_iterations_per_trajectory.clone()))
            .expect("failed to register avg_iterations_per_trajectory");
        registry
            .register(Box::new(trajectory_duration_seconds.clone()))
            .expect("failed to register trajectory_duration_seconds");
        registry
            .register(Box::new(tool_call_duration_seconds.clone()))
            .expect("failed to register tool_call_duration_seconds");

        Self {
            registry: registry.clone(),
            recorded_trajectories: Arc::new(AtomicU64::new(0)),
            trajectories_total,
            tool_calls_total,
            avg_iterations_per_trajectory,
            trajectory_duration_seconds,
            tool_call_duration_seconds,
        }
    }

    /// Record a completed trajectory.
    pub fn record_trajectory(
        &self,
        duration_secs: f64,
        iterations: usize,
        _tool_calls: usize,
        status: &str,
    ) {
        self.trajectories_total.with_label_values(&[status]).inc();
        self.trajectory_duration_seconds.observe(duration_secs);

        // Update average iterations gauge
        // Using a simple moving average approach
        let current = self.avg_iterations_per_trajectory.get();
        let count = (self.recorded_trajectories.fetch_add(1, Ordering::Relaxed) + 1) as f64;
        if count > 0.0 {
            let new_avg = current + (iterations as f64 - current) / count;
            self.avg_iterations_per_trajectory.set(new_avg);
        }
    }

    /// Record a tool call.
    pub fn record_tool_call(&self, duration_ms: u64, server: &str, tool: &str, _ok: bool) {
        let ok = if _ok { "true" } else { "false" };
        self.tool_calls_total
            .with_label_values(&[server, tool, ok])
            .inc();
        self.tool_call_duration_seconds
            .with_label_values(&[server, tool, ok])
            .observe(duration_ms as f64 / 1000.0);
    }

    pub fn render_prometheus(&self) -> String {
        let encoder = TextEncoder::new();
        let families = self.registry.gather();
        let mut buffer = Vec::new();
        encoder
            .encode(&families, &mut buffer)
            .expect("failed to encode trajectory metrics");
        String::from_utf8(buffer).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trajectory_metrics_creation() {
        let registry = Registry::new();
        let metrics = TrajectoryMetrics::new(&registry);
        assert_eq!(
            metrics
                .trajectories_total
                .with_label_values(&["final_answer"])
                .get(),
            0
        );
        assert_eq!(metrics.avg_iterations_per_trajectory.get(), 0.0);
    }

    #[test]
    fn test_record_trajectory() {
        let registry = Registry::new();
        let metrics = TrajectoryMetrics::new(&registry);

        metrics.record_trajectory(1.5, 3, 5, "final_answer");
        assert_eq!(
            metrics
                .trajectories_total
                .with_label_values(&["final_answer"])
                .get(),
            1
        );
        assert_eq!(metrics.avg_iterations_per_trajectory.get(), 3.0);

        metrics.record_trajectory(2.0, 5, 8, "max_iterations");
        assert_eq!(
            metrics
                .trajectories_total
                .with_label_values(&["max_iterations"])
                .get(),
            1
        );
        // Average should be (3 + 5) / 2 = 4.0
        assert_eq!(metrics.avg_iterations_per_trajectory.get(), 4.0);
    }

    #[test]
    fn test_record_tool_call() {
        let registry = Registry::new();
        let metrics = TrajectoryMetrics::new(&registry);

        metrics.record_tool_call(100, "test-server", "test-tool", true);
        metrics.record_tool_call(200, "test-server", "test-tool", true);
        metrics.record_tool_call(150, "other-server", "other-tool", false);

        assert_eq!(
            metrics
                .tool_calls_total
                .with_label_values(&["test-server", "test-tool", "true"])
                .get(),
            2
        );
        assert_eq!(
            metrics
                .tool_calls_total
                .with_label_values(&["other-server", "other-tool", "false"])
                .get(),
            1
        );
    }

    #[test]
    fn test_render_prometheus_includes_status_and_ok_labels() {
        let registry = Registry::new();
        let metrics = TrajectoryMetrics::new(&registry);

        metrics.record_trajectory(1.0, 2, 1, "final_answer");
        metrics.record_tool_call(100, "server-a", "tool-x", true);

        let rendered = metrics.render_prometheus();
        assert!(rendered.contains("xiaomaolv_trajectories_total"));
        assert!(rendered.contains("status=\"final_answer\""));
        assert!(rendered.contains("xiaomaolv_tool_calls_total"));
        assert!(rendered.contains("ok=\"true\""));
    }
}
