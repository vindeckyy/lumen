//! The dashboard the control plane serves.
//!
//! The page is one file with its styles and its script inside it, so the
//! server hands it over without a second request and the browser needs
//! nothing from the network but the host itself.

/// The dashboard itself.
pub const DASHBOARD: &str = include_str!("../assets/index.html");

/// The content type the dashboard is served with.
pub const CONTENT_TYPE: &str = "text/html; charset=utf-8";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dashboard_is_a_complete_page() {
        assert!(DASHBOARD.starts_with("<!doctype html>"));
        assert!(DASHBOARD.contains("</html>"));
        assert!(DASHBOARD.contains("<style>"));
        assert!(DASHBOARD.contains("<script>"));
    }

    #[test]
    fn the_dashboard_asks_for_no_thing_from_the_network() {
        // A control plane on a machine with no internet must still work, so
        // the page may not pull anything from a third party.
        assert!(!DASHBOARD.contains("http://cdn"));
        assert!(!DASHBOARD.contains("https://cdn"));
        assert!(!DASHBOARD.contains("//unpkg"));
        assert!(!DASHBOARD.contains("googleapis"));
    }

    #[test]
    fn the_dashboard_carries_the_token_in_the_fragment() {
        assert!(DASHBOARD.contains("location.hash"));
    }
}
