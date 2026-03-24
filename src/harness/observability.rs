//! Observability metrics for trajectory analysis.
//!
//! Provides Prometheus metrics for monitoring agent trajectory behavior,
//! including tool call counts, iteration counts, and duration metrics.

use prometheus::{
    Gauge, Histogram, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, Registry,
};

/// Metrics for trajectory analysis.
#[derive(Clone)]
pub struct TrajectoryMetrics {
    /// Total number of trajectories recorded.
    pub trajectories_total: IntCounter,
    /// Total number of tool calls, labeled by server and tool name.
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
        let trajectories_total = IntCounter::new(
            "xiaomaolv_trajectories_total",
            "Total number of trajectories recorded",
        )
        .expect("failed to create trajectories_total counter");

        let tool_calls_total = IntCounterVec::new(
            prometheus::Opts::new("tool_calls_total", "Total number of tool calls recorded")
                .namespace("xiaomaolv"),
            &["server", "tool"],
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
            &["server", "tool"],
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
            trajectories_total,
            tool_calls_total,
            avg_iterations_per_trajectory,
            trajectory_duration_seconds,
            tool_call_duration_seconds,
        }
    }

    /// Record a completed trajectory.
    pub fn record_trajectory(&self, duration_secs: f64, iterations: usize, _tool_calls: usize) {
        self.trajectories_total.inc();
        self.trajectory_duration_seconds.observe(duration_secs);

        // Update average iterations gauge
        // Using a simple moving average approach
        let current = self.avg_iterations_per_trajectory.get();
        let count = self.trajectories_total.get() as f64;
        if count > 0.0 {
            let new_avg = current + (iterations as f64 - current) / count;
            self.avg_iterations_per_trajectory.set(new_avg);
        }
    }

    /// Record a tool call.
    pub fn record_tool_call(&self, duration_ms: u64, server: &str, tool: &str, _ok: bool) {
        self.tool_calls_total
            .with_label_values(&[server, tool])
            .inc();
        self.tool_call_duration_seconds
            .with_label_values(&[server, tool])
            .observe(duration_ms as f64 / 1000.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trajectory_metrics_creation() {
        let registry = Registry::new();
        let metrics = TrajectoryMetrics::new(&registry);
        assert_eq!(metrics.trajectories_total.get(), 0);
        assert_eq!(metrics.avg_iterations_per_trajectory.get(), 0.0);
    }

    #[test]
    fn test_record_trajectory() {
        let registry = Registry::new();
        let metrics = TrajectoryMetrics::new(&registry);

        metrics.record_trajectory(1.5, 3, 5);
        assert_eq!(metrics.trajectories_total.get(), 1);
        assert_eq!(metrics.avg_iterations_per_trajectory.get(), 3.0);

        metrics.record_trajectory(2.0, 5, 8);
        assert_eq!(metrics.trajectories_total.get(), 2);
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
                .with_label_values(&["test-server", "test-tool"])
                .get(),
            2
        );
        assert_eq!(
            metrics
                .tool_calls_total
                .with_label_values(&["other-server", "other-tool"])
                .get(),
            1
        );
    }
}
