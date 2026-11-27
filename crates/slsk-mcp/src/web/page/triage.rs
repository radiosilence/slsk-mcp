//! The Triage tab: why albums did not land cleanly, by cause, with every
//! example, over a period chosen in the address.

use askama::Template;
use koan_kit_web as kit;

use super::{Body, Page, Tab, View};
use crate::graphql::TriageCause;
use crate::web::UiState;

/// The newest events drawn for each cause; the count covers the rest.
const EXAMPLES: usize = 20;

pub(in crate::web) struct Triage;

#[derive(serde::Deserialize, Default)]
pub(in crate::web) struct Days {
    days: Option<i64>,
}

#[derive(Template)]
#[template(path = "triage/page.html")]
struct TriagePage {
    days: i64,
    causes: Vec<TriageCause>,
    error: Option<String>,
}

impl TriagePage {
    fn periods(&self) -> &'static [i64] {
        &[7, 30, 90]
    }
    fn label(&self, cause: &str) -> &'static str {
        cause_label(cause)
    }
    fn ago(&self, at: &chrono::DateTime<chrono::Utc>) -> kit::Time<'static> {
        crate::web::ago(*at)
    }
}

impl Page for Triage {
    const TAB: Tab = Tab::Triage;
    const STREAM: &'static str = "/stream/triage";
    type Args = View<Days>;

    async fn body(s: &UiState, View(q): View<Days>) -> anyhow::Result<Body> {
        let days = q.days.unwrap_or(7).clamp(1, 365);
        let (causes, error) = match crate::graphql::triage(&s.app, days, EXAMPLES).await {
            Ok(causes) => (causes, None),
            Err(e) => (Vec::new(), Some(e.message)),
        };
        Ok(Body::of(&TriagePage {
            days,
            causes,
            error,
        }))
    }
}

/// Why an outcome happened, from the fixed set of causes, as a person would say it.
pub(in crate::web) fn cause_label(cause: &str) -> &'static str {
    match cause {
        "peer_failed" => "The peer failed every file",
        "stalled_peer" => "The peer stalled",
        "corrupt_copy" => "Files that are not valid audio",
        "no_audio" => "No audio in the folder",
        "requested" => "Another copy was asked for",
        "untagged" => "Tags too poor to import as-is",
        "no_candidates" => "MusicBrainz had nothing",
        "incomplete" => "Tracks missing against the release",
        "extra_files" => "Files the release does not have",
        "weak_match" => "Match too weak to apply unasked",
        "lossy_source" => "Lossy audio",
        "upsampled" => "Upsampled audio",
        "mb_unavailable" => "MusicBrainz unavailable",
        "discogs_unavailable" => "Discogs unavailable",
        "import_error" => "Import error",
        _ => "Other",
    }
}
