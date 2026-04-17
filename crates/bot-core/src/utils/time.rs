use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::types::MarketWindow;

pub async fn sleep_ms(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

pub fn now_sec() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn get_current_window_start_sec(at_sec: Option<u64>) -> u64 {
    let n = at_sec.unwrap_or_else(now_sec);
    (n / 300) * 300
}

pub fn get_market_slug(window_start_sec: u64) -> String {
    format!("btc-updown-5m-{window_start_sec}")
}

pub fn build_window(at_sec: Option<u64>) -> MarketWindow {
    let window_start_sec = get_current_window_start_sec(at_sec);
    MarketWindow {
        window_start_sec,
        close_time_sec: window_start_sec + 300,
        slug: get_market_slug(window_start_sec),
    }
}

pub async fn sleep_until(target_sec: u64) {
    while now_sec() < target_sec {
        let remaining_ms = ((target_sec - now_sec()) * 1000).max(50);
        sleep_ms(remaining_ms.min(1000)).await;
    }
}
