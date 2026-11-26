//! Library actions applied in the background, one at a time in the order
//! they were asked for, each album saved as it finishes so a restart takes
//! up where it stopped.

use std::collections::HashMap;
use std::sync::Arc;

use askama::Template;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::response::sse::Event;
use axum::response::{IntoResponse, Response};
use koan_kit_web as kit;

use super::plan::{Action, ArtChoice};
use super::{ArtStash, Closed, key, rel, slot_in, stashed};
use crate::library::{AlbumEdit, TagEdits, by_path};
use crate::live::{Topic, Watched};
use crate::web::{Draw, UiState, failed, field, fields, signals};

/// The library's runs: the one going or the last, the slot each takes in
/// turn, and the lock held while one joins the queue.
#[derive(Clone)]
pub(in crate::web) struct Runs {
    pub(in crate::web) run: Arc<Watched<LibraryRun>>,
    /// Held by the run going; those waiting take it in turn.
    slot: Arc<tokio::sync::Mutex<()>>,
    /// Held while a run takes its ticket and joins the queue.
    queueing: Arc<tokio::sync::Mutex<()>>,
}

impl Runs {
    pub(in crate::web) fn new() -> Self {
        Self {
            run: Arc::new(Watched::new(Topic::Library, LibraryRun::starting())),
            slot: Default::default(),
            queueing: Default::default(),
        }
    }
}

/// Take up the runs a restart interrupted, before the server answers anyone,
/// so nothing asked for since queues ahead of them.
pub(in crate::web) fn resume(s: &UiState) {
    let queueing = s
        .library
        .queueing
        .clone()
        .try_lock_owned()
        .expect("nothing else holds a new lock");
    tokio::spawn(resume_queued(s.clone(), queueing));
}

/// A background run of one action over albums, and how far it has got.
/// Each album takes from a moment (a move) to seconds (a MusicBrainz
/// lookup, decoding for gain), so it runs apart from the request that
/// started it, and the page follows it.
#[derive(Default)]
pub(in crate::web) struct LibraryRun {
    /// Counts runs, so a page following one knows when another starts.
    id: u64,
    action: Option<Action>,
    total: usize,
    done: usize,
    current: Option<String>,
    running: bool,
    results: Vec<Outcome>,
    error: Option<String>,
    /// Runs asked for while this one works, in the order they start.
    waiting: Vec<Queued>,
    tickets: u64,
}

impl LibraryRun {
    /// Before any run, numbered from when the service started, so a page
    /// from before a restart is behind every run after it.
    fn starting() -> Self {
        Self {
            id: chrono::Utc::now().timestamp_millis().max(0) as u64,
            ..Default::default()
        }
    }
}

struct Queued {
    ticket: u64,
    action: Action,
    paths: Vec<String>,
    with: HashMap<String, String>,
    split: String,
    /// The page that asked for it, waiting to hear how it ended; none for
    /// one taken up again after a restart.
    ended: Option<tokio::sync::oneshot::Sender<Ended>>,
}

/// How a run ended: where its albums moved, how many it did and how many
/// of those failed.
type Ended = (HashMap<String, String>, usize, usize);

impl Queued {
    /// Points the run at where the run before it moved its albums.
    fn follow(&mut self, moved: &HashMap<String, String>) {
        for path in &mut self.paths {
            if let Some(to) = moved.get(path) {
                if let Some(mut w) = self.with.remove(&key(path)) {
                    // An edit names the files it changes one by one: they
                    // moved with the album.
                    if self.action == Action::Edit
                        && let Ok(mut edits) = serde_json::from_str::<TagEdits>(&w)
                    {
                        for (file, _) in &mut edits.tracks {
                            if let Ok(rest) = file.strip_prefix(path.as_str()) {
                                *file = std::path::Path::new(to).join(rest);
                            }
                        }
                        w = serde_json::to_string(&edits).unwrap_or(w);
                    }
                    self.with.insert(key(to), w);
                }
                *path = to.clone();
            }
        }
    }
    fn label(&self) -> String {
        let n = self.paths.len();
        format!(
            "{} {n} album{}",
            self.action.doing(),
            if n == 1 { "" } else { "s" }
        )
    }
}

pub(in crate::web) struct Outcome {
    path: String,
    /// Where the album is now, when the action moved it.
    to: Option<String>,
    ok: bool,
    detail: String,
    problems: Vec<String>,
}

impl LibraryRun {
    fn verb(&self) -> &'static str {
        match self.action {
            Some(a) if self.running => a.doing(),
            Some(a) => a.done(),
            None => "",
        }
    }
    fn failed(&self) -> usize {
        self.results.iter().filter(|r| !r.ok).count()
    }
}

/// The run panel: the run going or the last one, and those waiting.
#[derive(Template)]
#[template(path = "library/run.html")]
pub(in crate::web) struct RunPanel<'a> {
    pub(in crate::web) run: &'a LibraryRun,
    pub(in crate::web) root: &'a str,
}

/// The panel's head alone: what the run is doing and how far it has got.
#[derive(Template)]
#[template(path = "library/run_head.html")]
pub(in crate::web) struct RunHead<'a> {
    pub(in crate::web) run: &'a LibraryRun,
    pub(in crate::web) root: &'a str,
}

/// One album the run has done.
#[derive(Template)]
#[template(path = "library/run_row.html")]
pub(in crate::web) struct RunRow<'a> {
    pub(in crate::web) r: &'a Outcome,
    pub(in crate::web) root: &'a str,
}

impl LibraryRun {
    pub(in crate::web) fn id(&self) -> u64 {
        self.id
    }
    pub(in crate::web) fn running(&self) -> bool {
        self.running
    }
    pub(in crate::web) fn results(&self) -> &[Outcome] {
        &self.results
    }
}

/// Apply an action to the albums ticked in its preview, in the background.
/// The response follows the run and, when it ends, says how it went; the
/// page's stream draws the run and, once it ends, the list as it now is.
pub(in crate::web) async fn apply(
    State(s): State<UiState>,
    Path(action): Path<String>,
    body: Bytes,
) -> Response {
    let Some(action) = Action::parse(&action) else {
        return failed(&anyhow::anyhow!("no such action"));
    };
    let form = fields(&body);
    let paths: Vec<String> = form
        .iter()
        .filter(|(k, v)| k == "path" && !v.is_empty())
        .map(|(_, v)| v.clone())
        .collect();
    if paths.is_empty() {
        return failed(&anyhow::anyhow!("nothing ticked"));
    }
    let from = slot_in(&form);
    // What the preview attached to each album, by its key.
    let with: HashMap<String, String> = form
        .iter()
        .filter_map(|(k, v)| Some((k.strip_prefix("with-")?.to_string(), v.clone())))
        .collect();
    let split = field(&form, "split").unwrap_or_default().to_string();
    // Held from the ticket to the queue, so actions queue in ticket order.
    let queueing = s.library.queueing.lock().await;
    let ticket = match crate::db::queue_library_action(
        &s.app.db,
        action.slug(),
        &paths,
        &with,
        &split,
    )
    .await
    {
        Ok(t) => t,
        Err(e) => return failed(&e.into()),
    };
    let (tx, rx) = tokio::sync::oneshot::channel();
    {
        let mut run = s.library.run.lock();
        run.tickets = run.tickets.max(ticket);
        let at = run.waiting.partition_point(|q| q.ticket < ticket);
        run.waiting.insert(
            at,
            Queued {
                ticket,
                action,
                paths,
                with,
                split,
                ended: Some(tx),
            },
        );
    }
    drop(queueing);
    run_next(&s);
    askama_datastar::stream(async_stream::stream! {
        // What it was applied from goes back to how it reads, every form on
        // it closed.
        let closed = |part| Event::from(Closed { slot: &from, part }.draw().patch());
        yield closed("preview");
        if from != "lib" {
            yield closed("edit");
            yield closed("retag");
            yield signals(serde_json::json!({ "_artform": "" }));
        }
        let Ok((_, done, failed)) = rx.await else {
            return;
        };
        let msg = format!(
            "{} {} albums{}",
            action.done(),
            done - failed,
            if failed > 0 { format!(", {failed} failed") } else { String::new() }
        );
        yield kit::Flash::ok(&msg).patch().into();
    })
    .into_response()
}

/// Take the next queued action once the one before it has finished. Each
/// action queued spawns one of these, so each is run once.
fn run_next(s: &UiState) {
    let (app, state, art, slot) = (
        s.app.clone(),
        s.library.run.clone(),
        s.art.clone(),
        s.library.slot.clone(),
    );
    tokio::spawn(async move {
        use futures_util::FutureExt;
        let _slot = slot.lock().await;
        let Some(q) = ({
            let mut run = state.lock();
            (!run.waiting.is_empty()).then(|| {
                let q = run.waiting.remove(0);
                *run = LibraryRun {
                    id: run.id + 1,
                    action: Some(q.action),
                    total: q.paths.len(),
                    running: true,
                    waiting: std::mem::take(&mut run.waiting),
                    tickets: run.tickets,
                    ..Default::default()
                };
                q
            })
        }) else {
            return;
        };
        let Queued {
            ticket,
            action,
            paths,
            with,
            split,
            ended,
        } = q;
        // A panic in the run still frees the slot.
        match std::panic::AssertUnwindSafe(run(
            &app, &state, &art, ticket, action, &paths, &with, &split,
        ))
        .catch_unwind()
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => state.lock().error = Some(format!("{e:#}")),
            Err(_) => state.lock().error = Some("stopped by an internal error".into()),
        }
        let (result, waiting) = {
            let mut run = state.lock();
            run.current = None;
            run.running = false;
            let moved: HashMap<String, String> = run
                .results
                .iter()
                .filter_map(|r| Some((r.path.clone(), r.to.clone()?)))
                .collect();
            for q in &mut run.waiting {
                q.follow(&moved);
            }
            let waiting: Vec<_> = run
                .waiting
                .iter()
                .map(|q| (q.ticket, q.paths.clone(), q.with.clone()))
                .collect();
            ((moved, run.done, run.failed()), waiting)
        };
        if let Err(e) = crate::db::finish_library_action(&app.db, ticket, waiting).await {
            tracing::warn!(error = %e, "could not save the library queue");
        }
        if let Some(tx) = ended {
            let _ = tx.send(result);
        }
    });
}

/// Take up the library actions a restart interrupted, each from the first
/// album it had not finished, and those queued behind them. `queueing` is
/// held from before the server answers, so nothing asked for since queues
/// ahead of them.
async fn resume_queued(s: UiState, queueing: tokio::sync::OwnedMutexGuard<()>) {
    let queued = match crate::db::library_queue(&s.app.db).await {
        Ok(q) => q,
        Err(e) => {
            tracing::warn!(error = %e, "could not read the library queue");
            return;
        }
    };
    let mut moved: HashMap<String, String> = HashMap::new();
    for q in queued {
        let end = |t| crate::db::finish_library_action(&s.app.db, t, Vec::new());
        let Some(action) = Action::parse(&q.action) else {
            let _ = end(q.ticket).await;
            continue;
        };
        let mut next = Queued {
            ticket: q.ticket,
            action,
            paths: q
                .paths
                .into_iter()
                .filter(|p| !q.done.contains_key(p))
                .collect(),
            with: q.attached,
            split: q.split,
            ended: None,
        };
        next.follow(&moved);
        moved.extend(
            q.done
                .into_iter()
                .filter_map(|(from, to)| Some((from, to?))),
        );
        if next.paths.is_empty() {
            let _ = end(q.ticket).await;
            continue;
        }
        if action == Action::SetArt {
            // The image chosen was held in memory only.
            let _ = end(q.ticket).await;
            let mut run = s.library.run.lock();
            run.id += 1;
            run.action = Some(action);
            run.total = next.paths.len();
            run.error =
                Some("A restart stopped this before it finished; choose the image again.".into());
            continue;
        }
        tracing::info!(
            action = action.slug(),
            albums = next.paths.len(),
            "resuming a library action"
        );
        {
            let mut run = s.library.run.lock();
            run.tickets = run.tickets.max(next.ticket);
            let at = run.waiting.partition_point(|q| q.ticket < next.ticket);
            run.waiting.insert(at, next);
        }
        run_next(&s);
    }
    drop(queueing);
}

impl LibraryRun {
    /// The last run that ended: the one before the run going, if one is.
    pub(in crate::web) fn seen(&self) -> u64 {
        if self.running {
            self.id.saturating_sub(1)
        } else {
            self.id
        }
    }
}

/// An album whose tags were written where it is.
fn edited(path: String) -> Outcome {
    Outcome {
        path,
        to: None,
        ok: true,
        detail: "tags written in place".into(),
        problems: Vec::new(),
    }
}

/// Run an action, saving each album as it finishes, in order, and every
/// one saved before the run is over.
#[allow(clippy::too_many_arguments)]
async fn run(
    app: &crate::App,
    state: &crate::live::Watched<LibraryRun>,
    art: &ArtStash,
    ticket: u64,
    action: Action,
    paths: &[String],
    with: &HashMap<String, String>,
    split: &str,
) -> anyhow::Result<()> {
    let (saves, mut saving) = tokio::sync::mpsc::unbounded_channel::<(String, Option<String>)>();
    let db = app.db.clone();
    let writer = tokio::spawn(async move {
        while let Some((path, to)) = saving.recv().await {
            if let Err(e) = crate::db::library_action_did(&db, ticket, &path, to.as_deref()).await {
                tracing::warn!(error = %e, "could not save the library queue");
            }
        }
    });
    let ran = run_albums(app, state, art, &saves, action, paths, with, split).await;
    drop(saves);
    let _ = writer.await;
    ran
}

#[allow(clippy::too_many_arguments)]
async fn run_albums(
    app: &crate::App,
    state: &crate::live::Watched<LibraryRun>,
    art: &ArtStash,
    saves: &tokio::sync::mpsc::UnboundedSender<(String, Option<String>)>,
    action: Action,
    paths: &[String],
    with: &HashMap<String, String>,
    split: &str,
) -> anyhow::Result<()> {
    let library = &app.library;
    let root = library.root().to_string_lossy().into_owned();
    let record = |o: Outcome| {
        let _ = saves.send((o.path.clone(), o.to.clone()));
        let mut run = state.lock();
        run.results.push(o);
        run.done += 1;
    };
    if action == Action::Refile {
        // One pass under the index's writer, each album planned again as it
        // moves.
        let dirs: Vec<&str> = paths.iter().map(String::as_str).collect();
        return library
            .refile_each(&by_path(dirs.iter().copied()), &dirs, |m| {
                record(Outcome {
                    ok: m.to.is_some() || m.refused.is_none(),
                    detail: match (&m.to, &m.refused) {
                        (Some(to), _) => format!("→ {}", rel(&root, to)),
                        (None, Some(why)) => why.replace(&format!("{root}/"), ""),
                        (None, None) => "Already where the rules file it.".into(),
                    },
                    to: m.to.clone().filter(|to| *to != m.from),
                    path: m.from,
                    problems: Vec::new(),
                });
            })
            .await;
    }
    if action == Action::Edit {
        // One call, so every album is checked before any is written and a
        // failed write puts back what was already written.
        let edits = paths
            .iter()
            .map(|path| {
                let edits: TagEdits = with
                    .get(&key(path))
                    .and_then(|e| serde_json::from_str(e).ok())
                    .ok_or_else(|| anyhow::anyhow!("no edits given for {path}"))?;
                Ok(AlbumEdit {
                    dir: path.clone(),
                    edits,
                    read: None,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>();
        state.lock().current = Some(match paths {
            [one] => one.clone(),
            _ => "all of them together".into(),
        });
        let edited_all = match edits {
            Ok(edits) => library.edit_many(&edits).await,
            Err(e) => Err(e),
        };
        match edited_all {
            Ok(moves) => {
                for m in moves {
                    record(match (m.to, m.refused) {
                        (Some(to), _) if to != m.from => Outcome {
                            detail: format!("→ {}", rel(&root, &to)),
                            to: Some(to),
                            ..edited(m.from)
                        },
                        (_, Some(why)) => Outcome {
                            detail: "tags written; left where it is".into(),
                            problems: vec![why.replace(&format!("{root}/"), "")],
                            ..edited(m.from)
                        },
                        _ => edited(m.from),
                    });
                }
            }
            Err(e) => {
                for path in paths {
                    record(Outcome {
                        path: path.clone(),
                        to: None,
                        ok: false,
                        detail: format!("{e:#}"),
                        problems: Vec::new(),
                    });
                }
            }
        }
        return Ok(());
    }
    let known = match action {
        Action::Credit | Action::Enrich => library.known_artists().await?.splitting_on(split),
        _ => Default::default(),
    };
    for path in paths {
        state.lock().current = Some(path.clone());
        let attached = with.get(&key(path)).map(String::as_str);
        let query = by_path([path.as_str()]);
        // What happened, problems on the way, and where the album went.
        let outcome: anyhow::Result<(String, Vec<String>, Option<String>)> = async {
            Ok(match action {
                Action::Refile => unreachable!("re-filed above"),
                Action::Credit => match library.credit_with(&query, &known).await?.pop() {
                    Some(c) => (
                        format!("{} files: {}", c.files, c.artists.join(", ")),
                        c.problems,
                        None,
                    ),
                    None => ("nothing to credit".into(), Vec::new(), None),
                },
                Action::Enrich => match library.enrich_with(&query, &known).await?.pop() {
                    Some(e) => {
                        let mut parts = vec![
                            e.gain_db
                                .map_or_else(|| "no gain".to_string(), |g| format!("{g:+.1} dB")),
                        ];
                        if !e.genres.is_empty() {
                            parts.push(e.genres.join(", "));
                        }
                        parts.push(format!("lyrics for {}", e.lyrics));
                        if !e.artists.is_empty() {
                            parts.push(format!("credited {}", e.artists.join(", ")));
                        }
                        (parts.join(" · "), e.problems, None)
                    }
                    None => anyhow::bail!("no longer in the library"),
                },
                Action::Reimport => {
                    let Some(release) = attached else {
                        anyhow::bail!("no release chosen");
                    };
                    let r = library.reimport(path, Some(release)).await?;
                    let release = r
                        .release
                        .map(|m| format!("{} — {}", m.artist, m.title))
                        .unwrap_or_default();
                    match (r.to, r.review) {
                        (Some(to), _) if to == *path => {
                            (format!("{release}, retagged in place"), r.problems, None)
                        }
                        (Some(to), _) => (
                            format!("{release} → {}", rel(&root, &to)),
                            r.problems,
                            Some(to),
                        ),
                        (None, Some(why)) => anyhow::bail!("left for review: {why}"),
                        (None, None) => (release, r.problems, None),
                    }
                }
                Action::Edit => unreachable!("edited above"),
                Action::RemoveArt => {
                    let r = library
                        .remove_art(std::slice::from_ref(path), true)
                        .await?
                        .pop()
                        .ok_or_else(|| anyhow::anyhow!("no longer in the library"))?;
                    (
                        format!("removed from {} files", r.files.len()),
                        Vec::new(),
                        None,
                    )
                }
                Action::SetArt => {
                    let choice: ArtChoice = attached
                        .and_then(|c| serde_json::from_str(c).ok())
                        .ok_or_else(|| anyhow::anyhow!("no image chosen"))?;
                    let image = stashed(art, &choice.token).ok_or_else(|| {
                        anyhow::anyhow!("that image is no longer held; choose it again")
                    })?;
                    let set = library.set_art(path, &image, choice.target(), true).await?;
                    let mut parts = Vec::new();
                    if !set.embedded.is_empty() {
                        parts.push(format!("embedded in {} files", set.embedded.len()));
                    }
                    if let Some(f) = &set.cover_file {
                        parts.push(format!("written as {f}"));
                    }
                    if !set.binned.is_empty() {
                        parts.push(format!("binned {}", set.binned.join(", ")));
                    }
                    (parts.join(" · "), Vec::new(), None)
                }
                Action::Bin => {
                    let Some(keep) = attached else {
                        let to = library.bin_album(path).await?;
                        return Ok((format!("→ {}", to.display()), Vec::new(), None));
                    };
                    let sets = library
                        .duplicates(&by_path([keep, path.as_str()]), true)
                        .await?;
                    match sets
                        .iter()
                        .flat_map(|d| &d.spares)
                        .find(|sp| sp.album.path == *path)
                        .and_then(|sp| sp.binned_to.clone())
                    {
                        Some(to) => (format!("→ {to}"), Vec::new(), None),
                        None => anyhow::bail!("no longer a spare copy; left where it is"),
                    }
                }
            })
        }
        .await;
        record(match outcome {
            Ok((detail, problems, to)) => Outcome {
                path: path.clone(),
                to,
                ok: true,
                detail,
                problems,
            },
            Err(e) => Outcome {
                path: path.clone(),
                to: None,
                ok: false,
                detail: format!("{e:#}"),
                problems: Vec::new(),
            },
        });
    }
    Ok(())
}
