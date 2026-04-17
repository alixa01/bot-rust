use bot_core::utils::time::{build_window, get_current_window_start_sec};

#[test]
fn computes_window_start_correctly() {
    assert_eq!(get_current_window_start_sec(Some(1713359999)), 1713359700);
    assert_eq!(get_current_window_start_sec(Some(1713360000)), 1713360000);
}

#[test]
fn builds_slug_for_window() {
    let window = build_window(Some(1713360001));
    assert_eq!(window.window_start_sec, 1713360000);
    assert_eq!(window.close_time_sec, 1713360300);
    assert_eq!(window.slug, "btc-updown-5m-1713360000");
}
