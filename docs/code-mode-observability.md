# Code Mode Observability

This guide covers Prometheus scraping and alerting for code mode safety/performance signals.

## Endpoints

- `GET /v1/code-mode/diag` (JSON snapshot; requires bearer token)
- `GET /v1/code-mode/metrics` (Prometheus text; requires bearer token)

The token is configured via:

```toml
[channels.http]
enabled = true
diag_bearer_token = "${HTTP_DIAG_BEARER_TOKEN}"
diag_rate_limit_per_minute = 120
```

## Prometheus Scrape Example

```yaml
scrape_configs:
  - job_name: xiaomaolv-code-mode
    metrics_path: /v1/code-mode/metrics
    scheme: http
    static_configs:
      - targets: ["127.0.0.1:8080"]
    authorization:
      type: Bearer
      credentials: ${HTTP_DIAG_BEARER_TOKEN}
```

## Key Metrics

- `xiaomaolv_code_mode_attempts_total`
- `xiaomaolv_code_mode_fallback_total`
- `xiaomaolv_code_mode_timed_out_calls_total`
- `xiaomaolv_code_mode_circuit_open_total`
- `xiaomaolv_code_mode_circuit_open` (gauge: 1 open, 0 closed)
- `xiaomaolv_code_mode_timeout_warn_ratio` (configured threshold)
- `xiaomaolv_code_mode_timeout_auto_shadow_probe_every`

## Recording/Alert Rule Example

```yaml
groups:
  - name: xiaomaolv-code-mode
    rules:
      - record: job:xiaomaolv_code_mode_fallback_ratio_5m
        expr: |
          sum(rate(xiaomaolv_code_mode_fallback_total[5m]))
          /
          clamp_min(sum(rate(xiaomaolv_code_mode_attempts_total[5m])), 1e-6)

      - record: job:xiaomaolv_code_mode_timeout_ratio_5m
        expr: |
          sum(rate(xiaomaolv_code_mode_timed_out_calls_total[5m]))
          /
          clamp_min(sum(rate(xiaomaolv_code_mode_attempts_total[5m])), 1e-6)

      - alert: XiaomaolvCodeModeCircuitOpen
        expr: max_over_time(xiaomaolv_code_mode_circuit_open[5m]) >= 1
        for: 5m
        labels:
          severity: warning
        annotations:
          summary: "code mode timeout circuit is open"
          description: "Code mode is in fallback mode due to timeout pressure."

      - alert: XiaomaolvCodeModeHighFallbackRatio
        expr: job:xiaomaolv_code_mode_fallback_ratio_5m > 0.5
        for: 10m
        labels:
          severity: warning
        annotations:
          summary: "code mode fallback ratio is high"
          description: "Fallback ratio over 50% in the last 10 minutes."
```

## Operational Notes

- Keep `diag_bearer_token` in secret storage, not in plaintext config.
- If `xiaomaolv_code_mode_circuit_open` is persistently `1`, inspect tool latency first.
- Tune `timeout_warn_ratio`, `timeout_auto_shadow_streak`, and `timeout_auto_shadow_probe_every` together.
