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

mod library {
    use axum::extract::Query;

    use crate::library::{AlbumMove, by_path, release_id};
    use crate::ui::library::*;

    #[test]
    fn new_art_goes_where_the_form_ticks() {
        let form = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        assert_eq!(
            ArtChoice::from_form(&form(&[("art", "t"), ("embed", "1")])),
            Some(ArtChoice {
                token: "t".into(),
                embed: true,
                file: false,
            })
        );
        let both = ArtChoice::from_form(&form(&[("art", "t"), ("embed", "1"), ("file", "1")]));
        assert_eq!(both.map(|c| (c.embed, c.file)), Some((true, true)));
        assert_eq!(ArtChoice::from_form(&form(&[("embed", "1")])), None);
    }

    #[test]
    fn only_the_latest_previewed_images_are_held() {
        let art = ArtStash::default();
        let image = || crate::art::Image {
            bytes: vec![0xFF, 0xD8, 0xFF],
            mime: "image/jpeg",
            width: 1,
            height: 1,
            resized: false,
        };
        let first = stash(&art, image());
        assert!(stashed(&art, &first).is_some());
        let rest: Vec<String> = (0..ART_HELD).map(|_| stash(&art, image())).collect();
        assert!(stashed(&art, &first).is_none());
        assert!(rest.iter().all(|t| stashed(&art, t).is_some()));
        assert!(stashed(&art, "made-up").is_none());
    }

    #[test]
    fn an_address_that_does_not_parse_is_the_default_view() {
        let f: Filter = crate::ui::query(&"/library?q=a&q=b".parse().unwrap());
        assert_eq!(f, Filter::default());
        let f: Filter = crate::ui::query(&"/library?q=+a+&datastar=%7B%7D".parse().unwrap());
        assert_eq!(f.q, "a");
    }

    #[test]
    fn albums_held_in_more_than_one_format_are_found_across_editions() {
        let album =
            |path: &str, artist: &str, title: &str, format: &str| crate::library::LibraryAlbum {
                path: path.into(),
                album_artist: Some(artist.into()),
                album: Some(title.into()),
                year: None,
                format: format.into(),
                tracks: 10,
                added: None,
            };
        let found = in_formats(&[
            album("/a", "The Dø", "Both Ways Open Jaws", "MP3"),
            album(
                "/b",
                "the dø",
                "Both Ways Open Jaws (Deluxe Edition)",
                "FLAC",
            ),
            album(
                "/c",
                "The Dø",
                "Both Ways Open Jaws Extended: Live Sessions",
                "FLAC",
            ),
            album("/d", "Burial", "Untrue", "FLAC"),
            album("/e", "Burial", "Untrue", "FLAC"),
        ]);
        let mut found: Vec<_> = found.into_iter().collect();
        found.sort();
        assert_eq!(found, ["/a", "/b"]);
    }

    fn change(from: &str, to: Option<&str>) -> Change {
        Change::of(&AlbumMove {
            from: from.into(),
            to: to.map(Into::into),
            refused: to.is_none().then(|| "collides".into()),
            files: 1,
        })
    }

    #[test]
    fn a_move_is_named_for_the_least_it_changes() {
        let from = "/m/Burial/(2007) Untrue [FLAC]";
        assert_eq!(change(from, Some(from)), Change::FileNames);
        assert_eq!(
            change(
                "/m/Burial/(2007) Untrue_ Remixes [FLAC]",
                Some("/m/Burial/(2007) Untrue- Remixes [FLAC]")
            ),
            Change::Spelling
        );
        assert_eq!(
            change(from, Some("/m/Burial/(2008) Untrue [FLAC]")),
            Change::Year
        );
        assert_eq!(
            change(
                "/m/Burial_/(2007) Untrue [FLAC]",
                Some("/m/Burial-/(2008) Untrue [FLAC]")
            ),
            Change::Year
        );
        assert_eq!(
            change(from, Some("/m/Kode9 & Burial/(2007) Untrue [FLAC]")),
            Change::Tags
        );
        assert_eq!(change(from, None), Change::Refused);
    }

    #[test]
    fn only_four_digit_runs_count_as_years() {
        assert_eq!(without_years("(1999) 101 Hits 12345"), "(#) 101 Hits 12345");
        assert_ne!(
            without_years("/m/A/(2001) X"),
            without_years("/m/A/(2001) X 2")
        );
    }

    #[test]
    fn paths_become_exact_alternatives() {
        assert_eq!(
            by_path(["/m/a", "/m/b"]),
            vec!["path:=/m/a", ",", "path:=/m/b"]
        );
        let q = sift::library::Query::parse(&by_path(["/m/a, b+", "/m/-c"])).unwrap();
        assert_eq!(q.sort.len(), 0);
    }

    fn filter(since: &str, from: &str) -> Filter {
        Filter {
            since: since.into(),
            from: from.into(),
            ..Default::default()
        }
    }

    #[test]
    fn an_import_date_becomes_a_lower_bound() {
        // A Sunday afternoon.
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-18T15:30:00Z")
            .unwrap()
            .to_utc();
        let midnight = now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();
        assert_eq!(
            filter("today", "").since_secs(now),
            Some(midnight.timestamp())
        );
        assert_eq!(
            filter("week", "").since_secs(now),
            Some(1_791_763_200),
            "Monday 12 October"
        );
        assert_eq!(
            filter("month", "").since_secs(now),
            Some(1_790_812_800),
            "1 October"
        );
        assert_eq!(
            filter("date", "2026-09-01").since_secs(now),
            Some(1_788_220_800)
        );
        assert_eq!(filter("date", "not a date").since_secs(now), None);
        assert_eq!(filter("", "").since_secs(now), None);
    }

    #[test]
    fn the_list_is_newest_first_after_any_sort_asked_for() {
        let f = Filter {
            q: "artist:burial year-".into(),
            ..Default::default()
        };
        assert_eq!(
            f.terms(),
            ["artist:burial", "year-", "added-", "albumartist+", "album+"]
        );
        let q = sift::library::Query::parse(&f.terms()).unwrap();
        assert_eq!(q.sort[0], ("year".to_string(), false));
        assert_eq!(q.sort[1], ("added".to_string(), false));
    }

    #[test]
    fn the_address_round_trips_the_filter() {
        let f = Filter {
            q: "burial".into(),
            since: "date".into(),
            from: "2026-09-01".into(),
            show: "dups".into(),
            album: "/m/Burial/(2007) Untrue".into(),
        };
        let url = f.url();
        let Query(back) = Query::<Filter>::try_from_uri(&url.parse().unwrap()).unwrap();
        assert_eq!(back, f);
        let Query(AlbumQuery { filter, path }) =
            Query::try_from_uri(&format!("{url}&path=%2Fm%2FX").parse().unwrap()).unwrap();
        assert_eq!((filter, path.as_str()), (f, "/m/X"));
    }

    #[test]
    fn a_form_carries_the_filter_beside_its_other_fields() {
        let form: Vec<(String, String)> = [("path", "/a"), ("q", " burial "), ("path", "/b")]
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .into();
        let f = Filter::from_fields(&form);
        assert_eq!(f.q, "burial");
        assert_eq!(f.album, "");
    }

    #[test]
    fn lyrics_are_summarised_without_their_timestamps() {
        let l = Lyric::of(&sift::PlannedLyrics {
            path: "/m/a/01 x.flac".into(),
            synced: true,
            text: "[00:01.00] First line\n[00:02.00]\n[00:03.00][00:09.00] Second".into(),
        });
        assert_eq!((l.lines, l.first.as_str()), (2, "First line"));
        assert_eq!(l.file, "01 x.flac");
    }

    fn track(path: &str, title: &str, album: &str, genre: Option<&str>) -> sift::meta::Track {
        sift::meta::Track {
            path: path.into(),
            title: Some(title.into()),
            album: Some(album.into()),
            genre: genre.map(Into::into),
            ..Default::default()
        }
    }

    fn form(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn an_edit_is_only_what_was_typed_differently() {
        let tracks = [
            track("/m/a/1.flac", "One", "Monstre", Some("Rock")),
            track("/m/a/2.flac", "Two", "Monstre", None),
        ];
        let edits = edits_from(
            &tracks,
            &form(&[
                ("a-album", "Monster"),
                // Differs between the files and left blank: untouched.
                ("a-genre", ""),
                ("a-label", ""),
                ("t0-file", "/m/a/1.flac"),
                ("t0-title", "One"),
                ("t1-file", "/m/a/2.flac"),
                ("t1-title", "Two (Live)"),
            ]),
        )
        .unwrap();
        assert_eq!(
            edits.album,
            [("album".to_string(), Some("Monster".to_string()))]
        );
        assert_eq!(
            edits.tracks,
            [(
                std::path::PathBuf::from("/m/a/2.flac"),
                vec![("title".to_string(), Some("Two (Live)".to_string()))]
            )]
        );
        let cleared = edits_from(&tracks, &form(&[("a-album", "")])).unwrap();
        assert_eq!(cleared.album, [("album".to_string(), None)]);

        let mut credited = tracks[0].clone();
        credited.artist = Some("A × B".into());
        credited.artists = vec!["A".into(), "B".into()];
        let renamed = edits_from(
            &[credited],
            &form(&[
                ("t0-file", "/m/a/1.flac"),
                ("t0-artist", "A & B"),
                ("t0-title", " One "),
            ]),
        )
        .unwrap();
        assert_eq!(
            renamed.tracks[0].1,
            [
                ("artist".to_string(), Some("A & B".to_string())),
                ("artists".to_string(), None)
            ],
            "the list of artists goes with the credit; a title typed with spaces is the same title"
        );
    }

    #[test]
    fn an_edit_across_albums_sets_what_was_typed_and_clears_what_was_ticked() {
        let tracks = [
            sift::meta::Track {
                album_artist: Some("A × B".into()),
                album_artists: vec!["A".into(), "B".into()],
                label: Some("Warp".into()),
                ..track("/m/a/1.flac", "One", "Monstre", Some("Rock"))
            },
            track("/m/a/2.flac", "Two", "Monstre", None),
        ];
        let edits = batch_edits_from(
            &tracks,
            &form(&[
                ("a-albumartist", " A & B "),
                ("a-genre", "Rock"),
                ("a-date", ""),
                ("a-label", ""),
                ("clear-label", "1"),
                ("clear-catalognum", "1"),
            ]),
        )
        .unwrap();
        assert_eq!(
            edits.album,
            [
                ("albumartist".to_string(), Some("A & B".to_string())),
                ("genre".to_string(), Some("Rock".to_string())),
                ("label".to_string(), None),
            ],
            "a genre one file lacks is set; a blank date is left; a catalogue number no file has needs no clearing"
        );
        assert_eq!(
            edits.tracks,
            [(
                std::path::PathBuf::from("/m/a/1.flac"),
                vec![("albumartists".to_string(), None)]
            )],
            "the list of album artists goes with the retyped album artist"
        );

        let same = batch_edits_from(&tracks, &form(&[("a-album", "Other"), ("a-genre", "")]));
        assert!(
            same.is_err(),
            "the album title is not offered, so nothing was asked"
        );
        assert!(
            batch_edits_from(
                &tracks,
                &form(&[("a-label", "Ninja"), ("clear-label", "1")])
            )
            .is_err()
        );
        let agreed = sift::meta::Track {
            genre: Some("Rock".into()),
            ..tracks[1].clone()
        };
        assert!(
            batch_edits_from(&[agreed], &form(&[("a-genre", "Rock")]))
                .unwrap()
                .is_empty(),
            "a value every file has already is no edit"
        );
    }

    #[test]
    fn a_release_is_its_id_or_a_link_to_it() {
        let id = "4c3e0b3c-2f8a-4a4b-9d6e-2a1f3b5c7d9e";
        assert_eq!(release_id(id).as_deref(), Some(id));
        assert_eq!(
            release_id(&format!("https://musicbrainz.org/release/{id}/")).as_deref(),
            Some(id)
        );
        assert_eq!(
            release_id(&format!(
                "https://musicbrainz.org/release/{}?tab=x",
                id.to_uppercase()
            ))
            .as_deref(),
            Some(id)
        );
        assert_eq!(
            release_id(&format!("https://musicbrainz.org/release/{id}/cover-art")).as_deref(),
            Some(id)
        );
        assert_eq!(
            release_id(&format!("https://musicbrainz.org/release-group/{id}")),
            None
        );
        assert_eq!(
            release_id(&format!("https://musicbrainz.org/recording/{id}")),
            None
        );
        assert_eq!(release_id("monster"), None);
    }

    #[test]
    fn a_discogs_release_is_its_id_or_a_link_to_it() {
        let ok = Some("discogs:123");
        assert_eq!(release_id(" discogs:123 ").as_deref(), ok);
        assert_eq!(
            release_id("https://www.discogs.com/release/123-Artist-Album").as_deref(),
            ok
        );
        assert_eq!(
            release_id("https://www.discogs.com/fr/release/123-Artist-Album?ev=rr").as_deref(),
            ok
        );
        assert_eq!(release_id("discogs.com/release/123").as_deref(), ok);
        assert_eq!(
            release_id("https://www.discogs.com/Artist-Album/release/123").as_deref(),
            ok
        );
        assert_eq!(
            release_id("https://www.discogs.com/master/123-Artist-Album"),
            None
        );
        assert_eq!(release_id("discogs:12a"), None);
        assert_eq!(release_id("discogs:"), None);
        assert_eq!(release_id("https://musicbrainz.org/release/123"), None);
    }

    #[test]
    fn a_number_typed_as_letters_is_refused() {
        let tracks = [sift::meta::Track {
            track: Some(1),
            ..track("/m/a/1.flac", "One", "Monstre", None)
        }];
        let typed = |n: &str| {
            edits_from(
                &tracks,
                &form(&[("t0-file", "/m/a/1.flac"), ("t0-track", n)]),
            )
        };
        assert!(typed("A1").is_err());
        assert!(typed("1/2").is_err());
        assert_eq!(
            typed("2").unwrap().tracks[0].1,
            [("track".to_string(), Some("2".to_string()))]
        );
        assert_eq!(
            typed("").unwrap().tracks[0].1,
            [("track".to_string(), None)],
            "a number left blank is cleared"
        );
    }
}

mod browse {
    use std::collections::BTreeMap;

    use axum_extra::extract::Query;
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use slsk_engine::slsk_proto::RawStr;
    use slsk_engine::slsk_proto::peer::{Directory, FileEntry};

    use crate::ui::browse::*;

    fn dir(name: &str, files: usize) -> Directory {
        Directory {
            name: RawStr::from(name.to_string()),
            files: (0..files)
                .map(|i| FileEntry {
                    name: RawStr::from(format!("{i}.flac")),
                    size: 1,
                    extension: "flac".into(),
                    attrs: Vec::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn a_level_lists_folders_below_it_including_unlisted_ones() {
        let dirs = vec![
            dir("@@music\\A\\One", 2),
            dir("@@music\\A\\Two", 3),
            dir("@@music\\B", 1),
            dir("@@other\\C\\D", 4),
        ];
        let (top, here) = level(&dirs, b"");
        assert!(here.is_none());
        assert_eq!(
            top.iter()
                .map(|c| (c.name.as_str(), c.files))
                .collect::<Vec<_>>(),
            vec![("@@music", 6), ("@@other", 4)]
        );
        let (music, _) = level(&dirs, b"@@music");
        assert_eq!(
            music.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["A", "B"]
        );
        let (below_b, here) = level(&dirs, b"@@music\\B");
        assert!(below_b.is_empty());
        assert_eq!(here.map(|d| d.files.len()), Some(1));
        // A sibling whose name starts the same is not beneath it.
        let (a, _) = level(
            &[dir("@@music\\A", 1), dir("@@music\\AB\\x", 1)],
            b"@@music\\A",
        );
        assert!(a.is_empty());
    }

    #[test]
    fn a_tree_downloads_as_albums_with_discs_kept_together() {
        let dirs = vec![
            dir("@@music\\A\\One", 2),
            dir("@@music\\A\\Two\\CD1", 3),
            dir("@@music\\A\\Two\\CD2", 3),
            dir("@@music\\AB", 1),
        ];
        let a = albums(&dirs, b"@@music\\A");
        assert_eq!(
            a.iter()
                .map(|(k, v)| (String::from_utf8_lossy(k).into_owned(), v.len()))
                .collect::<Vec<_>>(),
            vec![
                ("@@music\\A\\One".to_string(), 2),
                ("@@music\\A\\Two".to_string(), 6)
            ]
        );
        let two = &a[b"@@music\\A\\Two".as_slice()];
        assert_eq!(two[0].2, "CD1");
        assert_eq!(two[5].2, "CD2");
        // A disc folder opened directly is its own album.
        let cd = albums(&dirs, b"@@music\\A\\Two\\CD1");
        assert_eq!(cd.len(), 1);
        assert_eq!(cd.values().next().unwrap()[0].2, "");
        assert_eq!(albums(&dirs, b"").len(), 3);
    }

    fn place(at: &str, open: &[&str]) -> Place {
        Place {
            username: "u".into(),
            at: at.as_bytes().to_vec(),
            open: open.iter().map(|o| o.as_bytes().to_vec()).collect(),
            more: BTreeMap::new(),
        }
    }

    fn query(href: &str) -> BrowseQuery {
        Query::try_from_uri(&href.parse().unwrap()).unwrap().0
    }

    fn reread(href: &str) -> Place {
        Place::from_query(&query(href)).unwrap()
    }

    #[test]
    fn the_address_names_expanded_folders_relative_to_the_open_one() {
        let mut p = place("@@music\\A", &["@@music\\A\\One", "@@music\\A\\One\\CD1"]);
        p.more.insert(b"@@music\\A".to_vec(), 2);
        let q = query(&p.href());
        assert_eq!(q.open, vec!["One", "One/CD1"]);
        assert_eq!(q.more, vec![".", "."]);
        assert_eq!(Place::from_query(&q).unwrap(), p);
    }

    #[test]
    fn a_name_the_address_cannot_spell_goes_as_its_bytes() {
        let mut p = place("@@m", &["@@m\\a/b"]);
        p.open.insert(b"@@m\\Bj\xf6rk".to_vec());
        let q = query(&p.href());
        assert!(q.open.iter().all(|o| o.starts_with(RAW)));
        assert_eq!(Place::from_query(&q).unwrap(), p);
    }

    #[test]
    fn expanded_folders_share_one_budget_of_rows() {
        let page = crate::ui::PAGE as usize;
        let dirs: Vec<Directory> = (0..3)
            .flat_map(|a| (0..page).map(move |i| dir(&format!("{a}\\{i:05}"), 1)))
            .collect();
        let p = place("", &["0", "1", "2"]);
        let mut left = 3 + 2 * page;
        let html = tree(&p, &dirs, b"", false, &mut left);
        assert_eq!(left, 0);
        assert_eq!(html.matches("aria-expanded=").count(), 3 + 2 * page);
        assert!(html.contains("Collapse other folders"));
    }

    #[test]
    fn collapsing_a_folder_forgets_what_was_open_under_it() {
        let p = place("", &["A", "A\\B", "AB"]);
        let q = reread(&p.toggle(b"A"));
        assert_eq!(q, place("", &["AB"]));
        let q = reread(&p.toggle(b"C"));
        assert!(q.open.contains(b"C".as_slice()));
    }

    #[test]
    fn moving_keeps_what_is_open_below_and_the_way_back_down() {
        let p = place("A\\B", &["A\\B\\C", "A\\B\\C\\D"]);
        // Down: what was open below the new folder stays open.
        let down = reread(&p.go(b"A\\B\\C"));
        assert_eq!(down, place("A\\B\\C", &["A\\B\\C\\D"]));
        // Up: the folder left, and everything open in it, stays expanded.
        let up = reread(&p.go(b""));
        assert_eq!(up, place("", &["A", "A\\B", "A\\B\\C", "A\\B\\C\\D"]));
        // Sideways: nothing open elsewhere follows.
        let side = reread(&p.go(b"X"));
        assert_eq!(side, place("X", &[]));
    }

    #[test]
    fn a_long_folder_is_drawn_a_page_at_a_time() {
        let page = crate::ui::PAGE as usize;
        let dirs: Vec<Directory> = (0..page + 3)
            .map(|i| dir(&format!("@@m\\{i:05}"), 1))
            .collect();
        let p = place("@@m", &[]);
        let html = tree(&p, &dirs, b"@@m", false, &mut { MOST_ROWS });
        assert_eq!(html.matches("aria-expanded=").count(), page);
        assert!(html.contains("Show more"));
        let mut p = p;
        p.more.insert(b"@@m".to_vec(), 1);
        let html = tree(&p, &dirs, b"@@m", false, &mut { MOST_ROWS });
        assert_eq!(html.matches("aria-expanded=").count(), page + 3);
        assert!(!html.contains("Show more"));
    }

    #[test]
    fn crumbs_lead_back_up() {
        let c = crumbs(b"@@music\\A\\One");
        assert_eq!(
            c.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["@@music", "A", "One"]
        );
        assert_eq!(
            URL_SAFE_NO_PAD.decode(&c[1].key).unwrap(),
            b"@@music\\A".to_vec()
        );
    }
}
