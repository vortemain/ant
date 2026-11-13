//! The coordinator's static dashboard (served at `/` and `/ui`). The assets
//! contain no data: the page asks for a viewer or client token and reads the
//! coordinator's read-model API from the browser.

pub const INDEX_HTML: &str = include_str!("../ui/index.html");
pub const APP_JS: &str = include_str!("../ui/app.js");
pub const APP_CSS: &str = include_str!("../ui/app.css");
