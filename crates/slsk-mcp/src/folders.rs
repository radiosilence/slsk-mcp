//! Search results as albums.
//!
//! The network answers file by file, but what anyone downloads is a folder:
//! an album from one user. Grouping by user and directory, then ranking, is
//! what turns four hundred responses into "this one".

use std::collections::HashMap;

use slsk_engine::slsk_proto::RawStr;
use slsk_engine::slsk_proto::peer::{FileEntry, SearchResponse};

pub(crate) const LOSSLESS: &[&str] = &["flac", "wav", "aiff", "aif", "ape", "wv", "alac"];
const UNCOMPRESSED: &[&str] = &["wav", "aiff", "aif"];
pub(crate) const AUDIO: &[&str] = &[
    "flac", "wav", "aiff", "aif", "ape", "wv", "alac", "mp3", "m4a", "aac", "ogg", "opus", "wma",
];

#[derive(Debug, Clone, async_graphql::SimpleObject)]
pub struct File {
    /// The full path on the peer, as it must be requested.
    #[graphql(skip)]
    pub remote: RawStr,
    pub name: String,
    pub size: u64,
    pub extension: String,
    pub bitrate: Option<u32>,
    pub duration: Option<u32>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u32>,
}

#[derive(Debug, Clone, async_graphql::SimpleObject)]
pub struct Folder {
    pub username: String,
    /// The folder's path on the peer.
    pub path: String,
    #[graphql(skip)]
    pub remote_path: RawStr,
    pub files: Vec<File>,
    pub audio_files: usize,
    pub total_size: u64,
    /// Every audio file is lossless.
    pub lossless: bool,
    /// Lowest bit depth and sample rate across the audio files.
    pub bit_depth: Option<u32>,
    pub sample_rate: Option<u32>,
    /// Lowest bitrate, for lossy folders.
    pub bitrate: Option<u32>,
    pub free_slot: bool,
    /// The peer's average upload speed, bytes per second.
    pub speed: u32,
    pub queue_length: u32,
    pub score: f64,
}

#[derive(Debug, Clone, Default, async_graphql::InputObject)]
pub struct Filter {
    /// Only folders where every audio file is lossless.
    #[graphql(default)]
    pub lossless: bool,
    pub min_bit_depth: Option<u32>,
    pub min_sample_rate: Option<u32>,
    /// For lossy folders.
    pub min_bitrate: Option<u32>,
    pub min_tracks: Option<usize>,
    #[graphql(default)]
    pub free_slot_only: bool,
}

fn ext(entry: &FileEntry, name: &str) -> String {
    if !entry.extension.is_empty() {
        return entry.extension.to_lowercase();
    }
    name.rsplit_once('.')
        .map(|(_, e)| e.to_lowercase())
        .unwrap_or_default()
}

pub fn group(responses: &[SearchResponse], filter: &Filter) -> Vec<Folder> {
    let mut map: HashMap<(String, Vec<u8>), Folder> = HashMap::new();
    for r in responses {
        for entry in &r.files {
            let bytes = entry.name.as_bytes();
            let mut split = bytes.iter().rposition(|&b| b == b'\\').unwrap_or(0);
            // "Album\CD 1" and "Album\CD 2" are one album; the job's listing
            // of the parent brings every disc, each in its own subdirectory.
            if let Some(parent) = bytes[..split].iter().rposition(|&b| b == b'\\')
                && is_disc(&String::from_utf8_lossy(&bytes[parent + 1..split]))
            {
                split = parent;
            }
            let dir = RawStr(entry.name.0.slice(..split));
            let full = entry.name.to_string_lossy();
            let name = full.rsplit('\\').next().unwrap_or(&full).to_string();
            let file = File {
                remote: entry.name.clone(),
                extension: ext(entry, &name),
                name,
                size: entry.size,
                bitrate: entry.attr(FileEntry::BITRATE),
                duration: entry.attr(FileEntry::DURATION),
                sample_rate: entry.attr(FileEntry::SAMPLE_RATE),
                bit_depth: entry.attr(FileEntry::BIT_DEPTH),
            };
            let folder = map
                .entry((r.username.clone(), dir.0.to_vec()))
                .or_insert_with(|| Folder {
                    username: r.username.clone(),
                    path: dir.to_string_lossy(),
                    remote_path: dir.clone(),
                    files: Vec::new(),
                    audio_files: 0,
                    total_size: 0,
                    lossless: true,
                    bit_depth: None,
                    sample_rate: None,
                    bitrate: None,
                    free_slot: r.slot_free,
                    speed: r.avg_speed,
                    queue_length: r.queue_length,
                    score: 0.0,
                });
            folder.files.push(file);
        }
    }
    let mut folders: Vec<Folder> = map
        .into_values()
        .map(|mut f| {
            f.files.sort_by(|a, b| a.name.cmp(&b.name));
            f.files.dedup_by(|a, b| a.remote == b.remote);
            let audio: Vec<&File> = f
                .files
                .iter()
                .filter(|x| AUDIO.contains(&x.extension.as_str()))
                .collect();
            f.audio_files = audio.len();
            f.total_size = f.files.iter().map(|x| x.size).sum();
            f.lossless = !audio.is_empty()
                && audio
                    .iter()
                    .all(|x| LOSSLESS.contains(&x.extension.as_str()));
            f.bit_depth = audio.iter().filter_map(|x| x.bit_depth).min();
            f.sample_rate = audio.iter().filter_map(|x| x.sample_rate).min();
            f.bitrate = audio.iter().filter_map(|x| x.bitrate).min();
            f.score = score(&f);
            f
        })
        .filter(|f| f.audio_files > 0)
        .filter(|f| !filter.lossless || f.lossless)
        .filter(|f| {
            filter
                .min_bit_depth
                .is_none_or(|m| f.bit_depth.is_some_and(|d| d >= m))
        })
        .filter(|f| {
            filter
                .min_sample_rate
                .is_none_or(|m| f.sample_rate.is_some_and(|r| r >= m))
        })
        .filter(|f| {
            filter
                .min_bitrate
                .is_none_or(|m| f.lossless || f.bitrate.is_some_and(|b| b >= m))
        })
        .filter(|f| filter.min_tracks.is_none_or(|m| f.audio_files >= m))
        .filter(|f| !filter.free_slot_only || f.free_slot)
        .collect();
    folders.sort_by(|a, b| b.score.total_cmp(&a.score));
    folders
}

/// "CD 1", "Disc2", "cd1 - Mezzanine": one disc of an album split into
/// folders.
pub(crate) fn is_disc(name: &str) -> bool {
    let norm = sift::matching::normalise(name);
    let mut words = norm.split(' ');
    let Some(first) = words.next() else {
        return false;
    };
    ["cd", "disc", "disk"].iter().any(|p| {
        first.strip_prefix(p).is_some_and(|n| match n {
            "" => words
                .next()
                .is_some_and(|w| w.chars().all(|c| c.is_ascii_digit())),
            n => n.chars().all(|c| c.is_ascii_digit()),
        })
    })
}

/// Words in a folder's own name that mark it as something other than the
/// album: asked for by name, they are what was wanted; otherwise the plain
/// release comes first.
const VARIANTS: &[&str] = &[
    "live",
    "demo",
    "demos",
    "session",
    "sessions",
    "bsides",
    "sides",
    "peel",
    "remix",
    "remixes",
    "remixed",
    "instrumental",
    "instrumentals",
    "karaoke",
    "acoustic",
    "bootleg",
    "rehearsal",
    "rehearsals",
    "outtakes",
    "unplugged",
    "tribute",
    "covers",
    "commentary",
    "interview",
    // DJ mixes: an artist's name appears twice in "Artist - fabric 91: Artist",
    // which otherwise passes for the self-titled album.
    "mix",
    "mixed",
    "mixes",
    "fabric",
    "podcast",
];

/// How many unasked-for words a folder's name may carry before it ranks
/// behind folders named closer to the query. A threshold, not a count to
/// minimise: a lossless copy named `Artist - Album [WEB FLAC 24-44.1]` should
/// not lose to a lossy one named `Album`.
const MAX_EXTRA_WORDS: usize = 3;

/// Words folder names carry that say nothing about which release it is:
/// numbers (years, disc and track counts), formats and sources, bit depths
/// and rates, catalogue numbers mixing letters and digits, and joining words.
fn is_clutter(w: &str) -> bool {
    const WORDS: &[&str] = &[
        "flac",
        "mp3",
        "wav",
        "aiff",
        "alac",
        "ogg",
        "opus",
        "aac",
        "m4a",
        "web",
        "cd",
        "vinyl",
        "lp",
        "ep",
        "single",
        "album",
        "bit",
        "bits",
        "khz",
        "hz",
        "hi",
        "res",
        "hires",
        "lossless",
        "24bit",
        "16bit",
        "the",
        "and",
        "a",
        "of",
        "by",
        "va",
        "various",
        "artists",
        "remaster",
        "remastered",
    ];
    w.is_empty()
        || w.chars().all(|c| c.is_ascii_digit())
        || (w.chars().any(|c| c.is_ascii_digit()) && w.chars().any(|c| c.is_alphabetic()))
        || WORDS.contains(&w)
}

/// Folders that answer the query, best first, keeping each tier in the
/// order it came in. Peers match words anywhere in a path, so a search for
/// an album also returns every other folder under the artist's directory:
/// every word of the query must appear, as often as the query repeats it
/// ("portishead portishead" is the album in the artist's folder, not any
/// folder under it), and a live or demos folder waits behind the album
/// unless the query asks for one. With nothing that qualifies, everything
/// is returned as it came.
pub fn relevant(folders: Vec<Folder>, query: &str) -> Vec<Folder> {
    tiered(folders, query).into_iter().map(|(_, f)| f).collect()
}

/// An album this short is taken at whatever length anyone has it: a single
/// or an EP shared as two tracks is not a fragment of anything.
const SHORT_ALBUM: usize = 4;

/// Far short of the fullest copy: under four-fifths of its tracks, the same
/// bar searching again after a stall sets.
fn is_fragment(f: &Folder, fullest: usize) -> bool {
    fullest > SHORT_ALBUM && f.audio_files * 5 < fullest * 4
}

/// [`relevant`], each folder marked when it holds only part of the album.
/// The album's length is taken as the most tracks any folder in the best
/// relevance tier holds: those answer the query most closely, where a live
/// or deluxe folder further down may well be longer than the album.
fn marked(folders: Vec<Folder>, query: &str) -> Vec<(Option<u8>, bool, Folder)> {
    let ranked = tiered(folders, query);
    let best = ranked.first().map(|(t, _)| *t);
    let most = ranked
        .iter()
        .filter(|(t, _)| Some(*t) == best)
        .map(|(_, f)| f.audio_files)
        .max()
        .unwrap_or(0);
    ranked
        .into_iter()
        .map(|(t, f)| (t, is_fragment(&f, most), f))
        .collect()
}

/// [`relevant`], with folders holding only part of the album behind the
/// complete copies within each relevance tier. They are kept, last, for
/// when nothing else will come.
pub fn relevant_complete_first(folders: Vec<Folder>, query: &str) -> Vec<Folder> {
    let mut ranked = marked(folders, query);
    ranked.sort_by_key(|(t, fragment, _)| (*t, *fragment));
    ranked.into_iter().map(|(.., f)| f).collect()
}

/// [`relevant`], without folders holding only part of the album: for a wish,
/// which should wait for a complete copy rather than take a fragment.
pub fn relevant_complete(folders: Vec<Folder>, query: &str) -> Vec<Folder> {
    marked(folders, query)
        .into_iter()
        .filter(|(_, fragment, _)| !fragment)
        .map(|(.., f)| f)
        .collect()
}

fn tiered(folders: Vec<Folder>, query: &str) -> Vec<(Option<u8>, Folder)> {
    let norm = sift::matching::normalise(query);
    let wanted: Vec<&str> = norm.split(' ').filter(|w| w.len() > 1).collect();
    let tier = |f: &Folder| -> Option<u8> {
        let path = sift::matching::normalise(&f.path);
        let mut have: Vec<&str> = path.split(' ').collect();
        for w in &wanted {
            let i = have.iter().position(|p| p == w)?;
            have.swap_remove(i);
        }
        let leaf = f.path.rsplit('\\').next().unwrap_or(&f.path);
        let leaf = sift::matching::normalise(leaf);
        let variant = leaf
            .split(' ')
            .any(|w| VARIANTS.contains(&w) && !wanted.contains(&w));
        // Words in the folder's own name that the query did not ask for and
        // that are not the usual clutter: many of them mean something else
        // by the artist ("Artist (2021) Cyberpunk 2077 - Radio, Vol. 4"),
        // not the album asked for.
        let extra = leaf
            .split(' ')
            .filter(|w| !wanted.contains(w) && !is_clutter(w))
            .count();
        Some(u8::from(variant) * 2 + u8::from(extra > MAX_EXTRA_WORDS))
    };
    let mut ranked: Vec<(Option<u8>, Folder)> =
        folders.into_iter().map(|f| (tier(&f), f)).collect();
    if ranked.iter().any(|(t, _)| t.is_some()) {
        ranked.retain(|(t, _)| t.is_some());
        ranked.sort_by_key(|(t, _)| *t);
    }
    ranked
}

/// Quality first, then how soon it will actually arrive. A free slot matters
/// more than raw speed: a fast peer with forty people queued is hours away.
fn score(f: &Folder) -> f64 {
    let mut s = 0.0;
    if f.lossless {
        s += 1000.0;
        s += f64::from(f.bit_depth.unwrap_or(16).min(24)) * 2.0;
        // Uncompressed audio carries its tags poorly or not at all and is
        // twice the size: a FLAC copy is preferred unless the WAV is much
        // sooner to arrive.
        if f.files
            .iter()
            .any(|x| UNCOMPRESSED.contains(&x.extension.as_str()))
        {
            s -= 250.0;
        }
    } else {
        s += f64::from(f.bitrate.unwrap_or(0).min(320));
    }
    if f.free_slot {
        s += 300.0;
    }
    s -= f64::from(f.queue_length.min(100)) * 3.0;
    s += (f64::from(f.speed) + 1.0).log2() * 10.0;
    s += (f.audio_files as f64).min(30.0);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, ext: &str, attrs: Vec<(u32, u32)>) -> FileEntry {
        FileEntry {
            name: name.into(),
            size: 1000,
            extension: ext.into(),
            attrs,
        }
    }

    fn response(user: &str, slot_free: bool, files: Vec<FileEntry>) -> SearchResponse {
        SearchResponse {
            username: user.into(),
            token: 1,
            files,
            slot_free,
            avg_speed: 1_000_000,
            queue_length: 0,
            private_files: vec![],
        }
    }

    #[test]
    fn groups_by_user_and_folder_and_prefers_lossless_with_a_free_slot() {
        let flac = |n: &str| entry(n, "flac", vec![(1, 200), (4, 44100), (5, 16)]);
        let rs = vec![
            response(
                "a",
                true,
                vec![
                    flac("m\\X\\1.flac"),
                    flac("m\\X\\2.flac"),
                    entry("m\\X\\cover.jpg", "jpg", vec![]),
                ],
            ),
            response("b", true, vec![entry("m\\X\\1.mp3", "mp3", vec![(0, 320)])]),
            response("c", false, vec![flac("x\\X\\1.flac"), flac("x\\X\\2.flac")]),
        ];
        let all = group(&rs, &Filter::default());
        assert_eq!(
            all.iter().map(|f| f.username.as_str()).collect::<Vec<_>>(),
            ["a", "c", "b"]
        );
        assert_eq!(all[0].files.len(), 3);
        assert_eq!(all[0].audio_files, 2);
        let lossless = group(
            &rs,
            &Filter {
                lossless: true,
                ..Default::default()
            },
        );
        assert_eq!(lossless.len(), 2);
    }

    fn folder(user: &str, path: &str) -> Folder {
        let flac = |n: &str| entry(n, "flac", vec![(4, 44100), (5, 16)]);
        group(
            &[response(user, true, vec![flac(&format!("{path}\\1.flac"))])],
            &Filter::default(),
        )
        .remove(0)
    }

    /// A folder holding `tracks` FLAC files, from a peer with a free slot.
    fn album(user: &str, path: &str, tracks: usize) -> Folder {
        let flac = |n: String| entry(&n, "flac", vec![(4, 44100), (5, 16)]);
        group(
            &[response(
                user,
                true,
                (1..=tracks)
                    .map(|i| flac(format!("{path}\\{i}.flac")))
                    .collect(),
            )],
            &Filter::default(),
        )
        .remove(0)
    }

    #[test]
    fn a_fragment_ranks_behind_a_complete_copy() {
        // The fragment comes first on quality alone (24-bit beats 16).
        let mut fragment = album("a", "m\\Mac Declos\\Nothing Stands Still", 2);
        fragment.bit_depth = Some(24);
        let complete = album("b", "x\\Mac Declos - Nothing Stands Still", 12);
        let found = relevant_complete_first(
            vec![fragment, complete.clone()],
            "mac declos nothing stands still",
        );
        assert_eq!(
            found
                .iter()
                .map(|f| f.username.as_str())
                .collect::<Vec<_>>(),
            ["b", "a"]
        );
    }

    #[test]
    fn completeness_ranks_within_relevance_not_above_it() {
        let live_complete = album("a", "m\\Pixies\\Doolittle Live", 15);
        let album_short = album("b", "m\\Pixies\\Doolittle", 10);
        let fragment = album("c", "x\\Pixies\\Doolittle", 3);
        let found = relevant_complete_first(
            vec![live_complete, fragment, album_short],
            "pixies doolittle",
        );
        // 10 of 15 is a fragment by the live copy's count, but the album
        // tier still comes first; within it, 10 tracks beats 3.
        assert_eq!(
            found
                .iter()
                .map(|f| f.username.as_str())
                .collect::<Vec<_>>(),
            ["b", "c", "a"]
        );
    }

    #[test]
    fn grab_keeps_a_fragment_as_a_fallback_and_a_wish_drops_it() {
        let q = "mac declos nothing stands still";
        let copies = || {
            vec![
                album("a", "m\\Mac Declos\\Nothing Stands Still", 2),
                album("b", "x\\Mac Declos - Nothing Stands Still", 12),
            ]
        };
        assert_eq!(relevant_complete_first(copies(), q).len(), 2);
        let wished = relevant_complete(copies(), q);
        assert_eq!(
            wished
                .iter()
                .map(|f| f.username.as_str())
                .collect::<Vec<_>>(),
            ["b"]
        );
        // Alone, a short copy is its own measure: nothing says it is short.
        let alone = relevant_complete(
            vec![album("a", "m\\Mac Declos\\Nothing Stands Still", 2)],
            q,
        );
        assert_eq!(alone.len(), 1);
    }

    #[test]
    fn singles_and_eps_are_not_fragments() {
        let q = "burial street halo";
        let found = relevant_complete(
            vec![
                album("a", "m\\Burial\\Street Halo", 1),
                album("b", "x\\Burial - Street Halo", 3),
            ],
            q,
        );
        assert_eq!(found.len(), 2);
    }

    fn paths(folders: Vec<Folder>) -> Vec<String> {
        folders.into_iter().map(|f| f.path).collect()
    }

    #[test]
    fn a_self_titled_album_needs_its_name_twice() {
        let found = relevant(
            vec![
                folder("a", "music\\Portishead\\Roseland NYC Live (1998)"),
                folder("b", "music\\Portishead\\Portishead (1997)"),
            ],
            "portishead portishead",
        );
        assert_eq!(paths(found), ["music\\Portishead\\Portishead (1997)"]);
    }

    #[test]
    fn variants_wait_behind_the_album_unless_asked_for() {
        let folders = || {
            vec![
                folder(
                    "a",
                    "m\\Pixies-Doolittle_25_B_Sides_Peel_Sessions_And_Demos-2014",
                ),
                folder("b", "m\\Pixies\\Doolittle"),
            ]
        };
        assert_eq!(
            paths(relevant(folders(), "pixies doolittle"))[0],
            "m\\Pixies\\Doolittle"
        );
        assert_eq!(
            paths(relevant(folders(), "pixies doolittle peel sessions"))[0],
            "m\\Pixies-Doolittle_25_B_Sides_Peel_Sessions_And_Demos-2014"
        );
    }

    #[test]
    fn nothing_relevant_returns_everything() {
        let found = relevant(vec![folder("a", "m\\Other")], "portishead dummy");
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn disc_folders_are_one_album() {
        let flac = |n: &str| entry(n, "flac", vec![(4, 44100), (5, 16)]);
        let rs = vec![response(
            "a",
            true,
            vec![
                flac("m\\Massive Attack\\Mezzanine\\CD 1\\01.flac"),
                flac("m\\Massive Attack\\Mezzanine\\CD 2\\01.flac"),
            ],
        )];
        let all = group(&rs, &Filter::default());
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].path, "m\\Massive Attack\\Mezzanine");
        assert_eq!(all[0].audio_files, 2);
    }

    #[test]
    fn recognises_disc_folders() {
        for yes in ["CD 1", "CD1", "Disc 2", "disk3", "cd1 - Mezzanine", "CD 01"] {
            assert!(is_disc(yes), "{yes}");
        }
        for no in ["CDs", "Discovery", "Disco Inferno", "Mezzanine", ""] {
            assert!(!is_disc(no), "{no}");
        }
    }

    #[test]
    fn flac_is_preferred_to_wav() {
        let rs = vec![
            response(
                "wav",
                true,
                vec![entry("m\\A\\1.wav", "wav", vec![(4, 44100), (5, 16)])],
            ),
            response(
                "flac",
                true,
                vec![entry("m\\A\\1.flac", "flac", vec![(4, 44100), (5, 16)])],
            ),
        ];
        let all = group(&rs, &Filter::default());
        assert_eq!(all[0].username, "flac");
    }

    #[test]
    fn a_dj_mix_waits_behind_the_self_titled_album() {
        let folders = || {
            vec![
                folder("a", "m\\Nina Kraviz - Fabric 91_ Nina Kraviz"),
                folder("b", "m\\Nina Kraviz\\Nina Kraviz (2012)"),
            ]
        };
        assert_eq!(
            paths(relevant(folders(), "nina kraviz nina kraviz"))[0],
            "m\\Nina Kraviz\\Nina Kraviz (2012)"
        );
        assert_eq!(
            paths(relevant(folders(), "nina kraviz fabric"))[0],
            "m\\Nina Kraviz - Fabric 91_ Nina Kraviz"
        );
    }

    #[test]
    fn a_folder_named_like_the_query_beats_one_with_much_else_in_it() {
        let folders = || {
            vec![
                folder(
                    "a",
                    "m\\Nina Kraviz (Russian DJ)\\Nina Kraviz (2021) Cyberpunk 2077 - Radio, Vol. 4 - Original Soundtrack",
                ),
                folder("b", "m\\Nina Kraviz\\Nina Kraviz (2012)"),
            ]
        };
        assert_eq!(
            paths(relevant(folders(), "nina kraviz nina kraviz"))[0],
            "m\\Nina Kraviz\\Nina Kraviz (2012)"
        );
        // Asking for the soundtrack makes its words wanted.
        assert_eq!(
            paths(relevant(
                folders(),
                "nina kraviz cyberpunk radio original soundtrack"
            ))
            .len(),
            1
        );
    }

    #[test]
    fn scene_release_names_are_mostly_clutter() {
        let found = relevant(
            vec![folder(
                "a",
                "m\\Techno\\202609\\Amelie_Lens-AURA-EXH022-24BIT-WEB-FLAC-2026-WAVED",
            )],
            "amelie lens aura",
        );
        assert_eq!(found.len(), 1);
        for w in ["exh022", "24bit", "web", "flac", "2026", "dc215", "b2b001"] {
            assert!(is_clutter(w), "{w}");
        }
        for w in ["cyberpunk", "soundtrack", "radio", "waved"] {
            assert!(!is_clutter(w), "{w}");
        }
    }
}
