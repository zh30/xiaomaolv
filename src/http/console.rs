use axum::response::Html;

/// Operator control-plane console. Single-file page that consumes the
/// harness collection/detail/SSE contract with the same bearer key as the
/// API; nothing here is a new backend surface.
const CONSOLE_PAGE_HTML: &str = include_str!("console.html");

pub(super) async fn console_page() -> Html<&'static str> {
    Html(CONSOLE_PAGE_HTML)
}
