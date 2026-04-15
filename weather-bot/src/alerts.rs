use crate::config::Config;
use reqwest::Client;

/// Send a Telegram alert (non-blocking, fire-and-forget)
pub async fn send_alert(config: &Config, message: &str) {
    let token = match &config.telegram_bot_token {
        Some(t) => t.clone(),
        None => return,
    };
    let chat_id = match &config.telegram_chat_id {
        Some(c) => c.clone(),
        None => return,
    };

    let client = Client::new();
    let url = format!("https://api.telegram.org/bot{}/sendMessage", token);

    let _ = client
        .post(&url)
        .json(&serde_json::json!({
            "chat_id": chat_id,
            "text": message,
            "parse_mode": "HTML",
            "disable_web_page_preview": true,
        }))
        .send()
        .await;
}

/// Format a summary dashboard message
pub fn format_dashboard(
    active_count: usize,
    total_deployed: f64,
    current_value: f64,
    realized_pnl: f64,
    total_trades: u64,
    win_rate: f64,
    markets_scanned: usize,
) -> String {
    format!(
        r#"<b>🌡️ Weather Bot Dashboard</b>

<b>Active Positions:</b> {}
<b>Deployed:</b> ${:.2}
<b>Current Value:</b> ${:.2}
<b>Unrealized PnL:</b> ${:.2}
<b>Realized PnL:</b> ${:.2}
<b>Total Trades:</b> {}
<b>Win Rate:</b> {:.1}%
<b>Markets Scanned:</b> {}"#,
        active_count,
        total_deployed,
        current_value,
        current_value - total_deployed,
        realized_pnl,
        total_trades,
        win_rate,
        markets_scanned,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[tokio::test]
    async fn test_send_alert_no_telegram_config() {
        // Should silently return when no Telegram config is set
        let config = Config::default();
        send_alert(&config, "test message").await;
        // No panic = pass
    }

    #[tokio::test]
    async fn test_send_alert_missing_chat_id() {
        let mut config = Config::default();
        config.telegram_bot_token = Some("fake_token".to_string());
        // chat_id is None — should return early
        send_alert(&config, "test message").await;
    }

    #[tokio::test]
    async fn test_send_alert_missing_token() {
        let mut config = Config::default();
        config.telegram_chat_id = Some("-100123".to_string());
        // token is None — should return early
        send_alert(&config, "test message").await;
    }

    #[test]
    fn test_format_dashboard_basic() {
        let dashboard = format_dashboard(5, 25.0, 30.0, 10.0, 20, 55.0, 100);
        assert!(dashboard.contains("Active Positions:</b> 5"));
        assert!(dashboard.contains("Deployed:</b> $25.00"));
        assert!(dashboard.contains("Current Value:</b> $30.00"));
        assert!(dashboard.contains("Unrealized PnL:</b> $5.00"));
        assert!(dashboard.contains("Realized PnL:</b> $10.00"));
        assert!(dashboard.contains("Total Trades:</b> 20"));
        assert!(dashboard.contains("Win Rate:</b> 55.0%"));
        assert!(dashboard.contains("Markets Scanned:</b> 100"));
    }

    #[test]
    fn test_format_dashboard_zero_values() {
        let dashboard = format_dashboard(0, 0.0, 0.0, 0.0, 0, 0.0, 0);
        assert!(dashboard.contains("Active Positions:</b> 0"));
        assert!(dashboard.contains("Total Trades:</b> 0"));
    }

    #[test]
    fn test_format_dashboard_negative_unrealized() {
        let dashboard = format_dashboard(3, 15.0, 10.0, -2.0, 5, 20.0, 50);
        // Unrealized = current - deployed = 10 - 15 = -5
        assert!(dashboard.contains("Unrealized PnL:</b> $-5.00"));
    }

    #[test]
    fn test_format_dashboard_contains_html_tags() {
        let dashboard = format_dashboard(1, 1.0, 1.0, 1.0, 1, 100.0, 1);
        assert!(dashboard.contains("<b>"));
        assert!(dashboard.contains("</b>"));
    }
}
