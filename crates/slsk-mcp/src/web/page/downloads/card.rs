//! One job's card, on the Downloads list or heading the job's own page, and
//! the disclosures on it that read their content when opened.

use askama::Template;
use axum::response::Response;
use axum::response::sse::Event;
use koan_kit_web as kit;
use uuid::Uuid;

use crate::App;
use crate::graphql::{self, Job};
use crate::web::{human, percent, place_url, status_label};

#[derive(Template)]
#[template(path = "downloads/card.html")]
pub(in crate::web) struct Card {
    pub(in crate::web) j: Job,
    /// Drawn among those that need a decision.
    pub(in crate::web) warn: bool,
    /// On the job's own page, which shows everything known about it rather
    /// than linking to it.
    full: bool,
    /// The job's place in its peer's queue, while it waits there.
    place: Option<u32>,
    /// Whether a Discogs release can be applied as well as a MusicBrainz one.
    discogs: bool,
    /// The list of files is open, so it is drawn.
    open: bool,
    /// The job's peer is banned.
    banned: bool,
}

/// A suggested next step for a job, with the reason in a person's terms.
pub(super) struct Advice {
    /// The path under `/jobs/{id}/` that does it.
    action: String,
    label: &'static str,
    why: String,
}

impl Card {
    pub(in crate::web) fn of(app: &App, j: Job, full: bool, open: bool) -> Self {
        let warn = matches!(j.status.as_str(), "review" | "suspect" | "failed");
        let place = Uuid::parse_str(j.id.as_str())
            .ok()
            .and_then(|id| app.jobs.place(id));
        let banned = j
            .username
            .as_deref()
            .zip(app.session.engine())
            .is_some_and(|(u, e)| e.is_banned(u));
        Self {
            j,
            warn,
            full,
            place,
            discogs: app.jobs.discogs(),
            open: full || open,
            banned,
        }
    }

    /// What was fetched, as the peer named it: the folder and the one above,
    /// which is usually the artist. The query that found it otherwise.
    pub(super) fn heading(&self) -> String {
        self.j
            .folder
            .as_deref()
            .and_then(crate::web::folder_heading)
            .unwrap_or_else(|| self.j.title.clone())
    }

    /// The album's page in the library, once the job filed it.
    fn filed(&self) -> Option<String> {
        self.j
            .library_path
            .as_ref()
            .map(|path| place_url("/library", &[("album", path)]))
    }

    fn verdict(&self, t: &crate::analysis::TrackAnalysis) -> &'static str {
        use crate::analysis::Verdict as V;
        match t.verdict {
            V::Lossless => "lossless",
            V::Lossy => "lossy",
            V::Upsampled => "upsampled",
            V::Uncertain => "uncertain",
            V::Unknown => "unknown",
        }
    }

    /// Where the download stands, in the terms that explain a wait: nothing
    /// yet, a place in the peer's queue, or how much has arrived.
    fn progress(&self) -> String {
        let j = &self.j;
        let pct = percent(j.downloaded_bytes, j.total_bytes);
        if let Some(host) = &j.host {
            return match (j.downloaded_bytes, j.total_bytes) {
                (0, _) => format!("waiting for {host}"),
                (got, 0) => format!("{} so far", human(got)),
                _ => format!(
                    "{} of {} ({pct}%)",
                    human(j.downloaded_bytes),
                    human(j.total_bytes),
                ),
            };
        }
        let peer = j.username.as_deref().unwrap_or("the peer");
        match self.place {
            Some(p) if j.downloaded_bytes == 0 => format!("place {p} in {peer}'s queue"),
            Some(p) => format!(
                "{} of {}, rest at place {p} in {peer}'s queue",
                human(j.downloaded_bytes),
                human(j.total_bytes)
            ),
            None if j.downloaded_bytes == 0 => "waiting for the peer".to_string(),
            None => format!(
                "{} of {} ({pct}%)",
                human(j.downloaded_bytes),
                human(j.total_bytes),
            ),
        }
    }

    /// The address of one of the job's actions.
    fn url(&self, action: impl std::fmt::Display) -> String {
        format!("/jobs/{}/{action}", self.j.id.as_str())
    }

    /// When a measurement was taken, for `live.js` to move it on from; 0
    /// while it does not move, so an idle card reads the same from one
    /// redraw to the next and is not sent again.
    fn stamp(&self, rate: &u64) -> i64 {
        if *rate > 0 {
            self.j.measured_at.timestamp_millis()
        } else {
            0
        }
    }

    /// Progress toward `total` bytes, carrying the measurement `live.js`
    /// moves it from until the next.
    fn meter(&self, bytes: &u64, total: &u64, rate: &u64) -> kit::Meter<'static> {
        kit::Meter::new(bytes, (*total).max(1))
            .label("download")
            .data("bytes", bytes)
            .data("total", total)
            .data("rate", rate)
            .data("at", self.stamp(rate))
    }

    /// The files' toggle, as drawn before Datastar takes it over.
    fn files_label(&self) -> String {
        let arrow = if self.open { "▾" } else { "▸" };
        format!("{arrow} {} files", self.j.files.len())
    }

    fn release_hint(&self) -> &'static str {
        if self.discogs {
            "MusicBrainz or Discogs release id or link"
        } else {
            "MusicBrainz release id or link"
        }
    }

    /// How much of an import's current stage is done, when it counts.
    fn stage(&self) -> Option<kit::Meter<'static>> {
        let total = self.j.stage_total?;
        let done = self.j.stage_done.unwrap_or_default();
        Some(
            kit::Meter::new(done, total.max(1))
                .label(format!("{} of {} checked", human(done), human(total)))
                .class("mt-2"),
        )
    }

    fn label(&self) -> &'static str {
        status_label(&self.j.status)
    }

    fn file_progress(&self, f: &graphql::JobFile) -> String {
        if let Some(e) = &f.error {
            return format!("{} · {e}", f.state);
        }
        match f.state.as_str() {
            "completed" => human(f.size),
            _ if f.bytes == 0 => f.state.clone(),
            _ => format!(
                "{} of {} ({}%)",
                human(f.bytes),
                human(f.size),
                percent(f.bytes, f.size)
            ),
        }
    }

    /// The query that started the job, when the heading shows something else.
    fn asked(&self) -> Option<&str> {
        self.j.folder.as_ref().map(|_| self.j.title.as_str())
    }

    /// Why an import as-is would be refused, when it has been checked and
    /// would be.
    fn as_is_blocked(&self) -> Option<&str> {
        self.j.as_is_blocker.as_deref().filter(|b| !b.is_empty())
    }

    /// The blocker, unless the job's error already says the same.
    fn as_is_blocker_line(&self) -> Option<&str> {
        self.as_is_blocked()
            .filter(|b| self.j.error.as_deref() != Some(*b))
    }

    /// Whether bytes are arriving, so the line shows the rate and time left.
    fn moving(&self) -> bool {
        self.j.bytes_per_sec > 0 && self.j.downloaded_bytes > 0
    }

    fn at(&self, at: &chrono::DateTime<chrono::Utc>) -> i64 {
        at.timestamp_millis()
    }

    fn ms(&self, at: &Option<chrono::DateTime<chrono::Utc>>) -> i64 {
        at.map_or(0, |t| t.timestamp_millis())
    }

    fn size(&self, b: &u64) -> String {
        human(*b)
    }

    fn cause(&self, c: &str) -> String {
        crate::web::page::triage::cause_label(c).to_lowercase()
    }

    /// The one action most likely right, and why: the judgement a person
    /// would otherwise make from the evidence on the card.
    pub(super) fn advice(&self) -> Option<Advice> {
        let j = &self.j;
        let next = |why: String| Advice {
            action: "next".into(),
            label: "Try another copy",
            why,
        };
        match j.status.as_str() {
            "review" => {
                let best = j.candidates.first();
                if let Some(c) = best
                    && c.missing == 0
                    && c.extra == 0
                    && c.distance <= 0.12
                {
                    return Some(Advice {
                        action: format!("resolve/{}", c.release_id),
                        label: "Use this release",
                        why: format!(
                            "{} — {} lines up track for track; only details such as the credit differ.",
                            c.artist, c.title
                        ),
                    });
                }
                // Only once checked and found clean: an unchecked job may be
                // refused, and a suggestion has to be one that works.
                if best.is_none() && j.as_is_blocker.as_deref() == Some("") {
                    return Some(Advice {
                        action: "as-is".into(),
                        label: "Import as-is",
                        why: "MusicBrainz doesn't have this release, and the files are properly tagged.".into(),
                    });
                }
                if j.alternates > 0 {
                    return Some(next(match best {
                        None => "MusicBrainz doesn't have it and these files aren't tagged well enough to file; another copy may be.".into(),
                        Some(c) if c.missing > 0 => format!("This copy is missing {} tracks.", c.missing),
                        Some(_) => "No release fits these files closely.".into(),
                    }));
                }
                best.map(|c| Advice {
                    action: format!("resolve/{}", c.release_id),
                    label: "Use the closest release",
                    why: "No other copy to try.".into(),
                })
            }
            "suspect" => {
                let flagged: Vec<_> = j
                    .analysis
                    .iter()
                    .filter(|t| {
                        !matches!(
                            t.verdict,
                            crate::analysis::Verdict::Lossless
                                | crate::analysis::Verdict::Uncertain
                        )
                    })
                    .collect();
                let padded = !flagged.is_empty()
                    && flagged.iter().all(|t| {
                        t.estimate
                            .as_deref()
                            .is_some_and(|e| e.contains("16-bit content"))
                    });
                if padded {
                    return Some(Advice {
                        action: "approve".into(),
                        label: "Import anyway",
                        why:
                            "Only padded: CD-quality audio in a 24-bit container. Nothing is lost."
                                .into(),
                    });
                }
                if j.alternates > 0 {
                    return Some(next(
                        "The audio may not be true lossless; another copy may be clean.".into(),
                    ));
                }
                Some(Advice {
                    action: "approve".into(),
                    label: "Import anyway",
                    why: "No other copy; the audio may not be true lossless.".into(),
                })
            }
            "failed" if j.alternates > 0 => Some(next("This copy couldn't be fetched.".into())),
            "failed" => Some(Advice {
                action: "retry".into(),
                label: "Retry",
                why: "No other copy was found.".into(),
            }),
            _ => None,
        }
    }

    /// Whether to show `action` among the other choices: it applies, and
    /// is not already the suggestion.
    pub(super) fn offer(&self, action: &str) -> bool {
        let j = &self.j;
        let applies = match action {
            "approve" => j.status == "suspect",
            "next" => {
                matches!(j.status.as_str(), "review" | "suspect" | "failed") && j.alternates > 0
            }
            "as-is" => {
                matches!(j.status.as_str(), "review" | "waiting") && self.as_is_blocked().is_none()
            }
            "retry" => matches!(j.status.as_str(), "failed" | "cancelled"),
            "match" => j.status == "review",
            "cancel" => matches!(j.status.as_str(), "downloading" | "waiting"),
            "finish" => j.finishable && j.username.is_some(),
            "scam" => {
                j.username.is_some() && !matches!(j.status.as_str(), "importing" | "imported")
            }
            _ => false,
        };
        applies && self.advice().is_none_or(|a| a.action != action)
    }

    /// What the job needs from the person, in their terms; the reason
    /// underneath is the machine's.
    fn ask(&self) -> Option<&'static str> {
        let j = &self.j;
        match j.status.as_str() {
            "review" if j.candidates.is_empty() && self.as_is_blocked().is_some() => Some(
                "MusicBrainz has nothing that fits these files, and their tags are not enough to file them as they are. Correct the tags or try another copy.",
            ),
            "review" if j.candidates.is_empty() => Some(
                "MusicBrainz has nothing that fits these files. Import them as they are tagged, or try another copy.",
            ),
            "review" => Some(
                "Not sure which release this is. Pick the one that matches, or try another copy.",
            ),
            "suspect" => Some(
                "Some tracks may not be true lossless. Check the spectrograms, then import anyway or try another copy.",
            ),
            "failed" => Some("This one could not finish."),
            _ => None,
        }
    }

    fn sources(&self) -> Lazy {
        Lazy::sources(self.j.id.as_str(), self.j.alternates)
    }

    fn pick(&self) -> Lazy {
        Lazy::pick(self.j.id.as_str())
    }

    fn files(&self) -> Lazy {
        Lazy::files(self.j.id.as_str())
    }

    fn compare(&self, release: &str) -> Lazy {
        Lazy::compare(self.j.id.as_str(), release)
    }
}

/// One of a job card's disclosures, read from `url` when it opens and kept
/// open across the stream's redraws of the card.
pub(super) struct Lazy {
    id: String,
    url: String,
    summary: String,
    placeholder: &'static str,
    class: &'static str,
}

impl Lazy {
    pub(super) fn sources(job: &str, alternates: usize) -> Self {
        let summary = match alternates {
            0 => "sources and history".into(),
            1 => "sources and history · 1 other copy".into(),
            n => format!("sources and history · {n} other copies"),
        };
        Self {
            id: format!("sources-{job}"),
            url: format!("/jobs/{job}/sources"),
            summary,
            placeholder: "Reading history…",
            class: "mt-2",
        }
    }

    pub(super) fn pick(job: &str) -> Self {
        Self {
            id: format!("pick-{job}"),
            url: format!("/jobs/{job}/pick"),
            summary: "choose files".into(),
            placeholder: "Reading files…",
            class: "mt-2",
        }
    }

    pub(super) fn files(job: &str) -> Self {
        Self {
            id: format!("files-{job}"),
            url: format!("/jobs/{job}/files"),
            summary: "files and their tags".into(),
            placeholder: "Reading tags…",
            class: "mt-2",
        }
    }

    /// Discogs ids hold a colon, which a selector would read as a pseudo-class.
    pub(super) fn compare(job: &str, release: &str) -> Self {
        Self {
            id: format!("cmp-{job}-{}", release.replace(':', "-")),
            url: format!("/jobs/{job}/compare/{release}"),
            summary: "compare track by track".into(),
            placeholder: "Fetching the release…",
            class: "basis-full",
        }
    }

    /// As the card draws it: closed, reading `url` each time it opens.
    fn closed(&self) -> kit::Disclosure<'_> {
        kit::Disclosure::new(&self.summary)
            .id(&self.id)
            .ignore_morph()
            .class(self.class)
            .loads(&self.url, self.placeholder)
    }

    /// The handler's answer: `body` in place of the disclosure's own, which
    /// keeps its element and reads `url` again when next opened.
    pub(super) fn fill(&self, body: kit::Markup) -> Event {
        kit::Disclosure::fill(&self.id, body).into()
    }

    /// [`Lazy::fill`] as the whole answer.
    pub(super) fn answer(&self, body: kit::Markup) -> Response {
        askama_datastar::events([self.fill(body)])
    }
}
