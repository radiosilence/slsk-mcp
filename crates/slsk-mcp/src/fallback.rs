//! What a job is of, read from where its files came from, so that looking for
//! another copy searches for the album and not for the name of the folder
//! that held it.
//!
//! Sharers file albums in trees: `music\Recoil\Singles & EP's\2000 Strange
//! Hours (CD)\Covers`. The leaf is often a scans or disc folder whose name
//! means nothing on its own ("Covers", "CD", "DVD"), and searching for it
//! fetches whatever else on the network shares the word. So the album is the
//! deepest folder whose name is not one of those, cleaned of years, formats
//! and catalogue numbers, and the artist the nearest meaningful folder above
//! it. Whether a name is generic is judged after folding letters that look
//! Latin but are not ("Сovers" with a Cyrillic С) onto the Latin ones; the
//! words searched for keep their own script, so a Cyrillic title is searched
//! as written.

/// Words that alone name what a folder holds rather than what it is: scans,
/// discs, formats, and the categories sharers sort albums into.
const GENERIC: &[&str] = &[
    // Art and extras.
    "covers",
    "cover",
    "scans",
    "scan",
    "artwork",
    "artworks",
    "art",
    "pictures",
    "picture",
    "pics",
    "images",
    "image",
    "img",
    "booklet",
    "booklets",
    "inlay",
    "assets",
    "xtras",
    "extras",
    "extra",
    "bonus",
    "cdplus",
    "enhanced",
    "data",
    "video",
    "videos",
    "lyrics",
    "log",
    "logs",
    "nfo",
    "misc",
    "other",
    "others",
    "samples",
    "sample",
    "proof",
    "and",
    // Media.
    "cd",
    "cds",
    "disc",
    "discs",
    "disk",
    "dvd",
    "dvds",
    "bd",
    "bluray",
    "vinyl",
    "lp",
    "side",
    "tape",
    "cassette",
    "web",
    "sacd",
    // Formats.
    "flac",
    "mp3",
    "aac",
    "ogg",
    "wav",
    "alac",
    "lossless",
    "hires",
    "16bit",
    "24bit",
    "320",
    "v0",
    "khz",
    // How sharers sort an artist's work.
    "albums",
    "album",
    "singles",
    "single",
    "eps",
    "ep",
    "s",
    "compilations",
    "compilation",
    "discography",
    "bootlegs",
    "promos",
    "promo",
    // Share roots.
    "music",
    "media",
    "library",
    "complete",
    "completed",
    "downloads",
    "download",
    "shared",
    "share",
    "uploads",
    "incoming",
];

/// Media a disc folder is named for: "CD2", "Disc 1", "DVD 3 Promotional
/// Films".
const MEDIA: &[&str] = &["cd", "disc", "disk", "dvd", "bd", "lp", "side", "vinyl"];

/// Map letters from other scripts that render as Latin ones onto those, so
/// a folder named with a lookalike reads as the Latin name.
pub(crate) fn fold_confusables(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'а' | 'А' | 'α' | 'Α' => 'a',
            'в' | 'В' | 'β' | 'Β' => 'b',
            'с' | 'С' | 'ϲ' | 'Ϲ' => 'c',
            'ԁ' => 'd',
            'е' | 'Е' | 'ё' | 'Ё' | 'ε' | 'Ε' => 'e',
            'һ' | 'Һ' | 'н' | 'Н' | 'Η' => 'h',
            'і' | 'І' | 'ι' | 'Ι' | 'ӏ' | 'Ӏ' => 'i',
            'ј' | 'Ј' => 'j',
            'к' | 'К' | 'κ' | 'Κ' => 'k',
            'м' | 'М' | 'Μ' => 'm',
            'п' | 'ո' | 'Ν' | 'η' => 'n',
            'о' | 'О' | 'ο' | 'Ο' | 'σ' => 'o',
            'р' | 'Р' | 'ρ' | 'Ρ' => 'p',
            'ԛ' => 'q',
            'ѕ' | 'Ѕ' => 's',
            'т' | 'Т' | 'τ' | 'Τ' => 't',
            'υ' | 'ս' => 'u',
            'ν' | 'ѵ' => 'v',
            'ԝ' | 'ω' => 'w',
            'х' | 'Х' | 'χ' | 'Χ' => 'x',
            'у' | 'У' | 'γ' | 'Υ' => 'y',
            'ζ' | 'Ζ' => 'z',
            c => c,
        })
        .collect()
}

/// A folder name's words: lower case, punctuation dropped, and anything in
/// brackets left out, since that is a year, a format or a catalogue number
/// ("2000 Strange Hours (CD)", "Recoil (FLAC)").
fn words(name: &str) -> Vec<String> {
    let mut depth = 0u32;
    let unbracketed: String = name
        .chars()
        .filter(|c| match c {
            '(' | '[' | '{' => {
                depth += 1;
                false
            }
            ')' | ']' | '}' => {
                depth = depth.saturating_sub(1);
                false
            }
            _ => depth == 0,
        })
        .collect();
    sift::matching::normalise(&unbracketed)
        .split(' ')
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

fn is_year(w: &str) -> bool {
    w.len() == 4 && w.parse::<u16>().is_ok_and(|y| (1900..=2099).contains(&y))
}

/// "cd2", "dvd1": a medium and its number in one word.
fn is_numbered_medium(w: &str) -> bool {
    MEDIA.iter().any(|m| {
        w.strip_prefix(m)
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
    })
}

fn is_generic_word(w: &str) -> bool {
    GENERIC.contains(&w)
        || w.chars().all(|c| c.is_ascii_digit())
        || is_numbered_medium(w)
        // "2cd", "24bit", "96khz"
        || (w.starts_with(|c: char| c.is_ascii_digit())
            && GENERIC.contains(&w.trim_start_matches(|c: char| c.is_ascii_digit())))
}

/// Whether a folder's name says nothing about which album it is: "Covers",
/// "CD1", "Disc 2", "CD-Extra", "Сovers" in Cyrillic, "Singles & EP's".
/// A name of numbers alone is a title ("21", "1989"), not a disc number.
pub fn is_generic(name: &str) -> bool {
    let words = words(&fold_confusables(name));
    words.iter().all(|w| is_generic_word(w))
        && (words.is_empty() || !words.iter().all(|w| w.chars().all(|c| c.is_ascii_digit())))
}

/// Whether a folder is one disc or medium of an album: its name starts with
/// a medium and its number ("CD2", "Disc 1", "DVD 3 Promotional Films"), or
/// is a medium alone ("CD", "DVD").
fn is_medium(name: &str) -> bool {
    match words(&fold_confusables(name)).as_slice() {
        // A disc folder numbered alone ("2"); a longer number is a title.
        [n] if n.len() == 1 && n.chars().all(|c| c.is_ascii_digit()) => true,
        [first, ..] if is_numbered_medium(first) => true,
        [first] => MEDIA.contains(&first.as_str()),
        [first, n, ..] => MEDIA.contains(&first.as_str()) && n.chars().all(|c| c.is_ascii_digit()),
        [] => false,
    }
}

/// An album's name with what sharers add around it dropped: years and
/// format and media words, unless nothing else is left ("1989").
fn album_words(name: &str) -> Vec<String> {
    let all = words(name);
    let kept: Vec<String> = all
        .iter()
        .filter(|w| !is_year(w) && !GENERIC_FORMAT.contains(&w.as_str()))
        .cloned()
        .collect();
    if kept.is_empty() { all } else { kept }
}

/// Generic words that can be dropped from inside an album's name without
/// changing which album it is.
const GENERIC_FORMAT: &[&str] = &[
    "flac", "mp3", "aac", "ogg", "wav", "alac", "lossless", "web", "cd", "vinyl", "16bit", "24bit",
    "320", "v0", "hires",
];

/// The album a folder holds and the artist above it, as words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wanted {
    pub artist: Vec<String>,
    pub album: Vec<String>,
}

impl Wanted {
    /// From a peer's folder path (`\`-separated). The first component is the
    /// share's own root and never names the album unless it is all there is.
    pub fn from_path(path: &str) -> Option<Self> {
        let parts: Vec<&str> = path.split(['\\', '/']).filter(|p| !p.is_empty()).collect();
        let roots = usize::from(parts.len() > 1);
        let mut above = parts[roots..]
            .iter()
            .rev()
            .copied()
            .filter(|p| !is_generic(p) && !is_medium(p));
        let album = album_words(above.next()?);
        if album.is_empty() {
            return None;
        }
        // The nearest meaningful folder above, unless the album's own name
        // already holds it ("Recoil - Strange Hours" under "Recoil").
        let artist = above
            .next()
            .map(album_words)
            .filter(|a| !a.iter().all(|w| album.contains(w)))
            .unwrap_or_default();
        Some(Self { artist, album })
    }

    /// From the tags of what arrived: its album artist (or artist) and album.
    pub fn from_tags(artist: &str, album_name: &str) -> Option<Self> {
        let album = album_words(album_name);
        if album.is_empty() || is_generic(album_name) {
            return None;
        }
        Some(Self {
            artist: words(artist),
            album,
        })
    }

    /// From a title a person or a grab gave the job, when it names
    /// something. "Artist - Album" is read as the two; a grab's query does
    /// not say which of its words are which.
    pub fn from_title(title: &str) -> Option<Self> {
        if is_generic(title) {
            return None;
        }
        if let Some((artist, album)) = title.split_once(" - ")
            && !is_generic(album)
        {
            return Some(Self {
                artist: words(artist),
                album: album_words(album),
            });
        }
        let album = album_words(title);
        (!album.is_empty()).then_some(Self {
            artist: Vec::new(),
            album,
        })
    }

    /// A title that reads back as this: "artist - album".
    pub fn title(&self) -> String {
        match self.artist.is_empty() {
            true => self.album.join(" "),
            false => format!("{} - {}", self.artist.join(" "), self.album.join(" ")),
        }
    }

    /// The words to search the network for.
    pub fn query(&self) -> String {
        self.artist
            .iter()
            .chain(&self.album)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Whether a folder found by searching holds this album rather than
    /// something sharing a word with it: every word of the album in the
    /// folder's own album name or the artist above it, and every word of the
    /// artist somewhere in its path. A title ("recoil liquid") does not say
    /// which words are the artist, so each of its words need only be
    /// somewhere in the path, with at least one in the folder's album name.
    pub fn matches(&self, path: &str) -> bool {
        let Some(found) = Self::from_path(path) else {
            return false;
        };
        let near: Vec<&String> = found.album.iter().chain(&found.artist).collect();
        let anywhere: Vec<String> = path.split(['\\', '/']).flat_map(words).collect();
        if self.artist.is_empty() {
            return self.album.iter().all(|w| anywhere.contains(w))
                && self.album.iter().any(|w| found.album.contains(w));
        }
        self.album.iter().all(|w| near.contains(&w))
            && self.artist.iter().all(|w| anywhere.contains(w))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_disc_and_category_folders_are_generic() {
        for name in [
            "Covers",
            "covers",
            "COVERS",
            "Scans",
            "Artwork",
            "PICTURES",
            "CD",
            "CD1",
            "CD 2",
            "Disc 1",
            "Disk 3",
            "DVD",
            "DVD 2",
            "Assets",
            "Xtras",
            "Extras",
            "CDPLUS",
            "CD-Extra",
            "Covers & Scans",
            "Singles & EP's",
            "Albums",
            "FLAC",
            "[FLAC]",
            "2CD",
            "",
        ] {
            assert!(is_generic(name), "{name:?} should be generic");
        }
        for name in [
            "Strange Hours",
            "2000 Strange Hours (CD)",
            "Under the Covers, Volume IV",
            "DVD 3 Promotional Films+Bonus",
            "Selected",
            "Recoil",
        ] {
            assert!(!is_generic(name), "{name:?} names something");
        }
    }

    #[test]
    fn a_cyrillic_lookalike_reads_as_latin() {
        // С is U+0421 CYRILLIC CAPITAL LETTER ES.
        assert_eq!(fold_confusables("Сovers"), "covers");
        assert!(is_generic("Сovers"));
        assert!(is_generic("СD1"));
        assert!(is_generic("Ехтrаs"));
        assert_eq!(Wanted::from_title("Сovers"), None);
        assert_eq!(
            Wanted::from_title("Кино").map(|w| w.query()).as_deref(),
            Some("кино"),
            "a Cyrillic title is searched as written"
        );
    }

    #[test]
    fn the_album_is_the_deepest_folder_that_names_one() {
        let w = |p: &str| Wanted::from_path(p).map(|w| w.query());
        let root = r"music\Recoil (FLAC)\Recoil (FLAC)";
        assert_eq!(
            w(&format!(r"{root}\Singles & EP's\2000 Strange Hours (CD)")).as_deref(),
            Some("recoil strange hours")
        );
        assert_eq!(
            w(&format!(
                r"{root}\Singles & EP's\2000 Strange Hours (CD)\Covers"
            ))
            .as_deref(),
            Some("recoil strange hours")
        );
        assert_eq!(
            w(&format!(r"{root}\Compilations\2010 Selected (CD)")).as_deref(),
            Some("recoil selected")
        );
        assert_eq!(
            w(&format!(
                r"{root}\Albums\2007 subHuman (CD)\DVD\DVD 3 Promotional Films+Bonus"
            ))
            .as_deref(),
            Some("recoil subhuman")
        );
        assert_eq!(
            w(&format!(r"{root}\Albums\2000 Liquid (CD)\CD-Extra")).as_deref(),
            Some("recoil liquid")
        );
        assert_eq!(
            w(r"FLAC\Recoil - Strange Hours").as_deref(),
            Some("recoil strange hours")
        );
        assert_eq!(
            w(r"Nate\Recoil\Liquid (2000)").as_deref(),
            Some("recoil liquid")
        );
        assert_eq!(
            w(r"Music\Foo Fighters\In Your Honor (2005)\CD1").as_deref(),
            Some("foo fighters in your honor")
        );
    }

    #[test]
    fn a_path_of_nothing_but_generic_folders_names_no_album() {
        assert_eq!(Wanted::from_path("Covers"), None);
        assert_eq!(Wanted::from_path(r"music\Covers"), None);
        assert_eq!(Wanted::from_path(r"complete\CD\Scans"), None);
        assert_eq!(Wanted::from_path(r"@@ttayo\complete\Сovers"), None);
        assert_eq!(Wanted::from_path(""), None);
    }

    #[test]
    fn a_title_names_an_album_only_when_it_is_not_generic() {
        assert_eq!(Wanted::from_title("Covers"), None);
        assert_eq!(Wanted::from_title("CD"), None);
        assert_eq!(Wanted::from_title("Disc 2"), None);
        assert_eq!(
            Wanted::from_title("recoil hydrology plus")
                .map(|w| w.query())
                .as_deref(),
            Some("recoil hydrology plus")
        );
    }

    #[test]
    fn tags_name_the_album_and_its_artist() {
        assert_eq!(
            Wanted::from_tags("Recoil", "Strange Hours")
                .map(|w| w.query())
                .as_deref(),
            Some("recoil strange hours")
        );
        assert_eq!(Wanted::from_tags("Recoil", ""), None);
        assert_eq!(Wanted::from_tags("Recoil", "CD 1"), None);
    }

    #[test]
    fn a_title_written_back_keeps_its_artist_apart() {
        let w = Wanted::from_path(r"music\Recoil (FLAC)\Albums\2000 Liquid (CD)").unwrap();
        let again = Wanted::from_title(&w.title()).unwrap();
        assert_eq!(again, w);
        assert!(!again.matches(r"Liquid Records\Recoil - Bloodline"));
        assert!(again.matches(r"FLAC\Recoil - Liquid"));
    }

    #[test]
    fn a_disc_folder_numbered_alone_is_not_the_album() {
        assert_eq!(
            Wanted::from_path(r"Music\Recoil\Liquid\2")
                .map(|w| w.query())
                .as_deref(),
            Some("recoil liquid")
        );
    }

    #[test]
    fn a_title_of_numbers_is_a_title() {
        assert!(!is_generic("21"));
        assert!(!is_generic("1989"));
        assert!(is_generic("CD 2"));
        let adele = Wanted::from_path(r"Music\Adele\21").unwrap();
        assert_eq!(adele.query(), "adele 21");
        assert!(!adele.matches(r"Music\Adele\25"));
        assert!(adele.matches(r"FLAC\Adele - 21 (2011)"));
        assert_eq!(
            Wanted::from_tags("Taylor Swift", "1989")
                .map(|w| w.query())
                .as_deref(),
            Some("taylor swift 1989")
        );
    }

    #[test]
    fn a_grab_title_matches_a_copy_at_the_top_of_a_share() {
        let grab = Wanted::from_title("recoil liquid").unwrap();
        assert!(grab.matches(r"Recoil\Liquid"));
        assert!(grab.matches(r"Nate\Recoil\Liquid (2000)"));
        assert!(!grab.matches(r"Nate\Recoil\Bloodline"));
        assert!(!grab.matches(r"Liquid\Recoil\Bloodline"));
    }

    #[test]
    fn a_copy_found_must_be_the_same_album() {
        let strange =
            Wanted::from_path(r"music\Recoil (FLAC)\Singles & EP's\2000 Strange Hours (CD)")
                .unwrap();
        assert!(strange.matches(r"FLAC\Recoil - Strange Hours"));
        assert!(strange.matches(r"Music\Recoil\Strange Hours [2000]\CD1"));
        assert!(!strange.matches(r"Music\Recoil\Liquid"));
        // Every word somewhere in the path, but not the album's own name.
        assert!(!strange.matches(r"Strange Hours Mixes\Recoil\Liquid"));

        let selected =
            Wanted::from_path(r"music\Recoil (FLAC)\Compilations\2010 Selected (CD)").unwrap();
        assert!(!selected.matches(
            r"Complete\djlightningx\[BELGIAN LABELS]\Mostiko\(22 23203-2) VA-The_Sound_Of_The_Oh_Vol.1_(Selected_and_Mixed_By_W4CKO)-CD-FLAC-2010-HBFD"
        ));
        assert!(selected.matches(r"media\Recoil\Recoil - Selected (2010)"));

        let covers = Wanted::from_title("Under the Covers Volume IV").unwrap();
        assert!(!covers.matches(r"Library\Recoil\Covers"));
    }
}
