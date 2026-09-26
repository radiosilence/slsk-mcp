//! The shared-file index.
//!
//! Every search on the distributed network reaches every node in its branch,
//! so the index is on the hottest path this client has. It is an inverted
//! index — word to sorted list of file ids — and a query is an intersection
//! starting from the rarest word, which touches a few postings rather than
//! every path. A browse response is compressed once per scan and served as
//! the same bytes to every peer that asks.
//!
//! Scanning reads audio properties from each file's headers, which is the
//! slow part on a large library, so they are cached by path, size and mtime:
//! a rescan after an import probes only what changed.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use bytes::Bytes;
use rayon::prelude::*;
use slsk_proto::RawStr;
use slsk_proto::peer::{Directory, FileEntry, shared_file_list_frame};
use slsk_proto::wire::{Reader, Writer};

pub struct SharedFile {
    /// `root\sub\file.flac`, as peers see it.
    pub virtual_path: RawStr,
    pub path: PathBuf,
    pub size: u64,
    pub extension: String,
    pub attrs: Vec<(u32, u32)>,
}

impl SharedFile {
    fn entry(&self, name: RawStr) -> FileEntry {
        FileEntry {
            name,
            size: self.size,
            extension: self.extension.clone(),
            attrs: self.attrs.clone(),
        }
    }

    pub fn search_entry(&self) -> FileEntry {
        self.entry(self.virtual_path.clone())
    }
}

#[derive(Default)]
pub struct ShareIndex {
    files: Vec<SharedFile>,
    by_virtual: HashMap<Bytes, u32>,
    postings: HashMap<Box<str>, Vec<u32>>,
    /// Directory virtual path to its files, for folder-contents requests.
    dirs: BTreeMap<RawStr, Vec<u32>>,
    browse: Bytes,
}

impl ShareIndex {
    pub fn empty() -> Self {
        Self {
            browse: shared_file_list_frame(&[], &[]),
            ..Self::default()
        }
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    pub fn dir_count(&self) -> usize {
        self.dirs.len()
    }

    /// The complete SharedFileListResponse frame.
    pub fn browse_frame(&self) -> Bytes {
        self.browse.clone()
    }

    pub fn get(&self, virtual_path: &[u8]) -> Option<&SharedFile> {
        self.by_virtual
            .get(virtual_path)
            .map(|&i| &self.files[i as usize])
    }

    /// A folder and everything under it, as FolderContentsResponse wants.
    pub fn folder(&self, folder: &RawStr) -> Vec<Directory> {
        let prefix = folder.as_bytes();
        self.dirs
            .range(folder.clone()..)
            .take_while(|(dir, _)| dir.as_bytes().starts_with(prefix))
            .filter(|(dir, _)| {
                dir.as_bytes().len() == prefix.len() || dir.as_bytes()[prefix.len()] == b'\\'
            })
            .map(|(dir, ids)| Directory {
                name: dir.clone(),
                files: ids.iter().map(|&i| self.basename_entry(i)).collect(),
            })
            .collect()
    }

    fn basename_entry(&self, id: u32) -> FileEntry {
        let f = &self.files[id as usize];
        let path = f.virtual_path.as_bytes();
        let start = path.iter().rposition(|&b| b == b'\\').map_or(0, |i| i + 1);
        f.entry(RawStr(f.virtual_path.0.slice(start..)))
    }

    /// Files matching every term of `query`, at most `limit` of them.
    ///
    /// Terms are whole words, as other clients match them. `-word` excludes,
    /// and `*tail` matches any word ending in `tail`. Paths containing any of
    /// `excluded` — the phrases the server bans from search results — are
    /// dropped.
    pub fn search(&self, query: &str, limit: usize, excluded: &[String]) -> Vec<&SharedFile> {
        let mut include: Vec<&[u32]> = Vec::new();
        let mut partial: Vec<Vec<u32>> = Vec::new();
        let mut exclude: Vec<&[u32]> = Vec::new();
        for term in query.split_whitespace() {
            let (negate, term) = match term.strip_prefix('-') {
                Some(t) => (true, t),
                None => (false, term),
            };
            if let Some(tail) = term.strip_prefix('*') {
                let tail = tail.to_lowercase();
                if tail.is_empty() || negate {
                    continue;
                }
                let mut ids: Vec<u32> = self
                    .postings
                    .iter()
                    .filter(|(w, _)| w.ends_with(&tail))
                    .flat_map(|(_, ids)| ids.iter().copied())
                    .collect();
                ids.sort_unstable();
                ids.dedup();
                partial.push(ids);
                continue;
            }
            // A term with punctuation in it is several words to the index.
            for word in words(term) {
                match (negate, self.postings.get(word.as_str())) {
                    (false, Some(ids)) => include.push(ids),
                    (false, None) => return Vec::new(),
                    (true, Some(ids)) => exclude.push(ids),
                    (true, None) => {}
                }
            }
        }
        let mut sets: Vec<&[u32]> = include;
        sets.extend(partial.iter().map(Vec::as_slice));
        if sets.is_empty() {
            return Vec::new();
        }
        sets.sort_by_key(|s| s.len());
        let excluded: Vec<String> = excluded.iter().map(|p| p.to_lowercase()).collect();
        sets[0]
            .iter()
            .copied()
            .filter(|id| sets[1..].iter().all(|s| s.binary_search(id).is_ok()))
            .filter(|id| exclude.iter().all(|s| s.binary_search(id).is_err()))
            .map(|id| &self.files[id as usize])
            .filter(|f| {
                excluded.is_empty() || {
                    let path = f.virtual_path.to_string_lossy().to_lowercase();
                    !excluded.iter().any(|p| path.contains(p.as_str()))
                }
            })
            .take(limit)
            .collect()
    }

    fn build(files: Vec<SharedFile>) -> Self {
        let mut by_virtual = HashMap::with_capacity(files.len());
        let mut postings: HashMap<Box<str>, Vec<u32>> = HashMap::new();
        let mut dirs: BTreeMap<RawStr, Vec<u32>> = BTreeMap::new();
        for (i, f) in files.iter().enumerate() {
            let id = i as u32;
            by_virtual.insert(f.virtual_path.0.clone(), id);
            let path = f.virtual_path.to_string_lossy();
            let mut seen: Vec<String> = words(&path).collect();
            seen.sort_unstable();
            seen.dedup();
            for w in seen {
                postings.entry(w.into_boxed_str()).or_default().push(id);
            }
            let bytes = f.virtual_path.as_bytes();
            let dir_end = bytes.iter().rposition(|&b| b == b'\\').unwrap_or(0);
            dirs.entry(RawStr(f.virtual_path.0.slice(..dir_end)))
                .or_default()
                .push(id);
        }
        // Ids were pushed in ascending order, so every list is already sorted.
        let mut index = Self {
            files,
            by_virtual,
            postings,
            dirs,
            browse: Bytes::new(),
        };
        let listing: Vec<Directory> = index
            .dirs
            .iter()
            .map(|(dir, ids)| Directory {
                name: dir.clone(),
                files: ids.iter().map(|&i| index.basename_entry(i)).collect(),
            })
            .collect();
        index.browse = shared_file_list_frame(&listing, &[]);
        index
    }
}

/// Lower-cased alphanumeric runs. Unicode-aware, so "Björk" is one word.
fn words(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
}

const AUDIO: &[&str] = &[
    "flac", "mp3", "m4a", "aac", "ogg", "opus", "wav", "aiff", "aif", "ape", "wv", "alac", "wma",
    "dsf",
];

#[derive(Clone)]
struct Probed {
    size: u64,
    mtime: u64,
    attrs: Vec<(u32, u32)>,
}

/// Audio properties by path, persisted between scans.
#[derive(Default)]
pub struct ProbeCache(HashMap<PathBuf, Probed>);

impl ProbeCache {
    pub fn load(path: &Path) -> Self {
        let Ok(bytes) = std::fs::read(path) else {
            return Self::default();
        };
        let mut r = Reader::new(Bytes::from(bytes));
        let mut map = HashMap::new();
        let parse = |r: &mut Reader| -> slsk_proto::wire::Result<(PathBuf, Probed)> {
            let p = PathBuf::from(r.string()?);
            let size = r.u64()?;
            let mtime = r.u64()?;
            let attrs = r.list(8, |r| Ok((r.u32()?, r.u32()?)))?;
            Ok((p, Probed { size, mtime, attrs }))
        };
        while !r.is_empty() {
            match parse(&mut r) {
                Ok((p, v)) => {
                    map.insert(p, v);
                }
                Err(_) => break,
            }
        }
        Self(map)
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let mut w = Writer::new();
        for (p, v) in &self.0 {
            w.str(&p.to_string_lossy())
                .u64(v.size)
                .u64(v.mtime)
                .u32(v.attrs.len() as u32);
            for (c, a) in &v.attrs {
                w.u32(*c).u32(*a);
            }
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, w.finish())?;
        std::fs::rename(tmp, path)
    }
}

/// Walk `roots` and build an index. Blocking and CPU-heavy on a cold cache;
/// run it on a blocking thread.
pub fn scan(roots: &[PathBuf], cache: &mut ProbeCache) -> ShareIndex {
    struct Found {
        path: PathBuf,
        virtual_path: String,
        size: u64,
        mtime: u64,
        extension: String,
    }
    let mut found = Vec::new();
    let mut root_names: HashMap<String, u32> = HashMap::new();
    for root in roots {
        let base = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "share".into());
        let uses = root_names.entry(base.clone()).or_insert(0);
        *uses += 1;
        let name = if *uses == 1 {
            base
        } else {
            format!("{base} ({uses})")
        };
        for entry in walkdir::WalkDir::new(root)
            .follow_links(true)
            .into_iter()
            .filter_entry(|e| e.depth() == 0 || !e.file_name().to_string_lossy().starts_with('.'))
        {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_file() {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
            let mut virtual_path = name.clone();
            for part in rel.components() {
                virtual_path.push('\\');
                virtual_path.push_str(&part.as_os_str().to_string_lossy());
            }
            let extension = entry
                .path()
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            found.push(Found {
                path: entry.into_path(),
                virtual_path,
                size: meta.len(),
                mtime,
                extension,
            });
        }
    }

    let fresh: Vec<(PathBuf, Probed)> = found
        .par_iter()
        .filter(|f| AUDIO.contains(&f.extension.as_str()))
        .filter(
            |f| !matches!(cache.0.get(&f.path), Some(p) if p.size == f.size && p.mtime == f.mtime),
        )
        .map(|f| {
            (
                f.path.clone(),
                Probed {
                    size: f.size,
                    mtime: f.mtime,
                    attrs: probe(&f.path),
                },
            )
        })
        .collect();
    cache.0.extend(fresh);
    let live: std::collections::HashSet<&Path> = found.iter().map(|f| f.path.as_path()).collect();
    cache.0.retain(|p, _| live.contains(p.as_path()));

    let files = found
        .into_iter()
        .map(|f| SharedFile {
            attrs: cache
                .0
                .get(&f.path)
                .map(|p| p.attrs.clone())
                .unwrap_or_default(),
            virtual_path: RawStr::from(f.virtual_path),
            path: f.path,
            size: f.size,
            extension: f.extension,
        })
        .collect();
    ShareIndex::build(files)
}

/// Attributes in the combinations the network expects: lossless files carry
/// duration, sample rate and bit depth; lossy ones bitrate and duration.
fn probe(path: &Path) -> Vec<(u32, u32)> {
    use lofty::config::ParseOptions;
    use lofty::file::AudioFile;
    let Ok(probe) = lofty::probe::Probe::open(path) else {
        return Vec::new();
    };
    let Ok(file) = probe
        .options(ParseOptions::new().read_tags(false).read_cover_art(false))
        .read()
    else {
        return Vec::new();
    };
    let p = file.properties();
    let duration = p.duration().as_secs() as u32;
    match p.bit_depth() {
        Some(depth) => {
            let mut attrs = vec![(FileEntry::DURATION, duration)];
            if let Some(rate) = p.sample_rate() {
                attrs.push((FileEntry::SAMPLE_RATE, rate));
            }
            attrs.push((FileEntry::BIT_DEPTH, u32::from(depth)));
            attrs
        }
        None => {
            let mut attrs = Vec::new();
            if let Some(kbps) = p.audio_bitrate() {
                attrs.push((FileEntry::BITRATE, kbps));
            }
            attrs.push((FileEntry::DURATION, duration));
            attrs
        }
    }
}

pub type Shares = Arc<arc_swap::ArcSwap<ShareIndex>>;

#[cfg(test)]
mod tests {
    use super::*;

    fn index(paths: &[&str]) -> ShareIndex {
        ShareIndex::build(
            paths
                .iter()
                .map(|p| SharedFile {
                    virtual_path: RawStr::from(*p),
                    path: PathBuf::from(p),
                    size: 1,
                    extension: "flac".into(),
                    attrs: vec![],
                })
                .collect(),
        )
    }

    fn names(hits: Vec<&SharedFile>) -> Vec<String> {
        hits.into_iter()
            .map(|f| f.virtual_path.to_string_lossy())
            .collect()
    }

    #[test]
    fn matches_whole_words_across_the_path() {
        let idx = index(&[
            "music\\Aphex Twin\\Drukqs\\01 Jynweythek.flac",
            "music\\Autechre\\Tri Repetae\\01 Dael.flac",
        ]);
        assert_eq!(
            names(idx.search("aphex drukqs", 10, &[])),
            ["music\\Aphex Twin\\Drukqs\\01 Jynweythek.flac"]
        );
        assert!(
            idx.search("aphe", 10, &[]).is_empty(),
            "partial words do not match"
        );
        assert_eq!(idx.search("music 01", 10, &[]).len(), 2);
    }

    #[test]
    fn exclusions_wildcards_and_banned_phrases() {
        let idx = index(&[
            "m\\Boards of Canada\\Geogaddi\\a.flac",
            "m\\Boards of Canada\\Geogaddi\\a.mp3",
            "m\\Canada\\x.flac",
        ]);
        assert_eq!(idx.search("canada -mp3", 10, &[]).len(), 2);
        assert_eq!(idx.search("*gaddi", 10, &[]).len(), 2);
        assert_eq!(idx.search("canada", 10, &["boards of".into()]).len(), 1);
        assert_eq!(idx.search("canada", 1, &[]).len(), 1);
    }

    #[test]
    fn folder_contents_include_subfolders_but_not_siblings() {
        let idx = index(&[
            "m\\A\\CD1\\1.flac",
            "m\\A\\CD2\\1.flac",
            "m\\AB\\1.flac",
            "m\\A\\cover.jpg",
        ]);
        let dirs: Vec<String> = idx
            .folder(&"m\\A".into())
            .into_iter()
            .map(|d| d.name.to_string_lossy())
            .collect();
        assert_eq!(dirs, ["m\\A", "m\\A\\CD1", "m\\A\\CD2"]);
    }

    #[test]
    fn browse_frame_decodes_to_the_same_tree() {
        use bytes::BytesMut;
        let idx = index(&["m\\A\\1.flac", "m\\A\\2.flac", "m\\B\\1.flac"]);
        let frame = idx.browse_frame();
        let mut buf = BytesMut::from(&frame[..]);
        let f = slsk_proto::frame::decode(&mut buf, slsk_proto::CodeWidth::U32, 1 << 20)
            .unwrap()
            .unwrap();
        match slsk_proto::peer::PeerMessage::decode(f.code, f.body).unwrap() {
            slsk_proto::peer::PeerMessage::SharedFileList { dirs, .. } => {
                assert_eq!(dirs.len(), 2);
                assert_eq!(dirs[0].files[1].name.to_string_lossy(), "2.flac");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn scans_a_tree_and_caches_probes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("music");
        std::fs::create_dir_all(root.join("Artist/Album")).unwrap();
        std::fs::write(root.join("Artist/Album/01 Song.flac"), b"not really flac").unwrap();
        std::fs::write(root.join(".hidden"), b"x").unwrap();
        let mut cache = ProbeCache::default();
        let idx = scan(std::slice::from_ref(&root), &mut cache);
        assert_eq!(idx.file_count(), 1);
        assert!(idx.get(b"music\\Artist\\Album\\01 Song.flac").is_some());
        let cache_file = dir.path().join("cache.bin");
        cache.save(&cache_file).unwrap();
        assert_eq!(ProbeCache::load(&cache_file).0.len(), 1);
    }
}
