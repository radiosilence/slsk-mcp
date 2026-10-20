use super::*;

#[test]
fn asset_urls_change_with_their_contents() {
    assert_eq!(versioned("/a.css", "x"), versioned("/a.css", "x"));
    assert_ne!(versioned("/a.css", "x"), versioned("/a.css", "y"));
    assert!(CSS_URL.starts_with("/assets/app.css?v="));
    assert!(DATASTAR_URL.starts_with("/assets/datastar.js?v="));
}

#[test]
fn only_hashed_asset_urls_are_cached_for_good() {
    let cache = |q: Option<&str>| {
        asset(APP_CSS, "text/css", RawQuery(q.map(Into::into))).headers()[header::CACHE_CONTROL]
            .to_str()
            .unwrap()
            .to_owned()
    };
    assert!(cache(Some("v=0123456789abcdef")).contains("immutable"));
    assert!(!cache(None).contains("immutable"));
}
