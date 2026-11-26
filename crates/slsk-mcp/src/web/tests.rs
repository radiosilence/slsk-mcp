use koan_kit_web as kit;

use super::stream::unnamed;

/// Datastar's `data-show` acts only once Datastar has loaded, so an element
/// that starts hidden flashes until then; such elements carry `hidden` and
/// `data-attr:hidden`. Nothing else may use the name either, since Datastar
/// would run its value as script.
#[test]
fn nothing_waits_for_datastar_to_be_hidden() {
    let attr = concat!("data-", "show");
    let mut files = vec![std::path::PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/assets/live.js"
    ))];
    let mut dirs = vec![std::path::PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/templates"
    ))];
    dirs.push(concat!(env!("CARGO_MANIFEST_DIR"), "/src").into());
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else {
                files.push(path);
            }
        }
    }
    for path in files {
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains(&format!("{attr}=")) && !text.contains(&format!("{attr}:")),
            "{} uses {attr}",
            path.display()
        );
    }
}

mod library {
    use axum::extract::Query;

    use crate::library::{AlbumMove, by_path, release_id};
    use crate::web::page::library::plan::*;
    use crate::web::page::library::*;

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
        let f: Filter = crate::web::query(&"/library?q=a&q=b".parse().unwrap());
        assert_eq!(f, Filter::default());
        let f: Filter = crate::web::query(&"/library?q=+a+&datastar=%7B%7D".parse().unwrap());
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
    }

    #[test]
    fn the_address_lists_every_open_album() {
        let uri: http::Uri = "/library?q=x&album=%2Fm%2FA&album=&album=%2Fm%2FB%20%26%20C"
            .parse()
            .unwrap();
        assert_eq!(open_albums(&uri), ["/m/A", "/m/B & C"]);
        // An album's slot is named after its path, and only a plain name is
        // taken from a form, since it is spliced into selectors.
        let slot = slot_of("/m/A");
        assert!(slot.starts_with('a') && slot.chars().all(|c| c.is_ascii_alphanumeric()));
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
    fn a_release_type_is_a_primary_type_and_its_secondary_types() {
        let typed = |types: Option<&str>, pairs: &[(&str, &str)]| {
            let t = sift::meta::Track {
                release_types: types
                    .map(|t| t.split("; ").map(Into::into).collect())
                    .unwrap_or_default(),
                ..track("/m/a/1.flac", "One", "Monstre", None)
            };
            edits_from(&[t], &form(pairs)).unwrap().album
        };
        let set = |v: &str| vec![("albumtypes".to_string(), Some(v.to_string()))];
        assert_eq!(
            typed(
                None,
                &[
                    ("type-primary", "album"),
                    ("type-secondary", "dj-mix"),
                    ("type-secondary", "compilation"),
                ]
            ),
            set("album; compilation; dj-mix"),
            "secondary types follow the primary in MusicBrainz's order"
        );
        assert!(
            typed(
                Some("compilation; album"),
                &[("type-primary", "album"), ("type-secondary", "compilation")]
            )
            .is_empty(),
            "the same types in another order are no change"
        );
        assert_eq!(
            typed(Some("ep; remix"), &[("type-primary", "none")]),
            [("albumtypes".to_string(), None)],
            "none chosen clears the type"
        );
        assert!(
            typed(Some("ep"), &[]).is_empty(),
            "a form without the type controls leaves it alone"
        );

        let tracks = [
            sift::meta::Track {
                release_types: vec!["ep".into()],
                ..track("/m/a/1.flac", "One", "Monstre", None)
            },
            sift::meta::Track {
                release_types: vec!["ep".into(), "remix".into()],
                ..track("/m/a/2.flac", "Two", "Monstre", None)
            },
        ];
        let batch = |pairs: &[(&str, &str)]| batch_edits_from(&tracks, &form(pairs));
        assert!(
            batch(&[("type-primary", "keep")]).is_err(),
            "the type left as is asks nothing"
        );
        assert_eq!(
            batch(&[("type-primary", "keep"), ("type-secondary", "live")])
                .unwrap()
                .album,
            set("ep; live"),
            "secondary types chosen alone go beside the album's own primary type"
        );
        assert_eq!(
            batch(&[("type-primary", "album"), ("clear-albumtypes", "1")])
                .unwrap()
                .album,
            [("albumtypes".to_string(), None)],
            "Clear wins over a type chosen"
        );
    }

    #[test]
    fn a_release_type_is_not_lost_to_the_form() {
        let typed = |types: &[&str]| sift::meta::Track {
            release_types: types.iter().map(|t| t.to_string()).collect(),
            ..track("/m/a/1.flac", "One", "Monstre", None)
        };
        // The form as drawn for "album; ep": the second primary type is a
        // toggle, on.
        assert!(
            edits_from(
                &[typed(&["album", "ep"])],
                &form(&[("type-primary", "album"), ("type-secondary", "ep")])
            )
            .unwrap()
            .is_empty(),
            "a second primary type is offered, so saving the form keeps it"
        );
        let differ = [
            typed(&["album"]),
            sift::meta::Track {
                path: "/m/a/2.flac".into(),
                ..typed(&["ep"])
            },
        ];
        let keep_live = form(&[("type-primary", "keep"), ("type-secondary", "live")]);
        assert!(
            edits_from(&differ, &keep_live).is_err(),
            "one file's primary type is not given to the rest"
        );
        assert!(batch_edits_from(&differ, &keep_live).is_err());
        let untyped = [
            typed(&[]),
            sift::meta::Track {
                path: "/m/a/2.flac".into(),
                ..typed(&["ep"])
            },
        ];
        assert_eq!(
            batch_edits_from(&untyped, &keep_live).unwrap().album,
            [("albumtypes".to_string(), Some("ep; live".to_string()))],
            "a file with no type takes the others' primary type"
        );
        assert_eq!(
            batch_edits_from(
                &differ,
                &form(&[
                    ("type-primary", "keep"),
                    ("type-secondary", "live"),
                    ("clear-albumtypes", "1")
                ])
            )
            .unwrap()
            .album,
            [("albumtypes".to_string(), None)],
            "Clear wins over the type's other controls"
        );
        assert!(
            edits_from(&differ, &form(&[("type-primary", "keep")]))
                .unwrap()
                .is_empty(),
            "kept, the type is left alone"
        );
        let reordered = [
            typed(&["album", "live"]),
            sift::meta::Track {
                path: "/m/a/2.flac".into(),
                ..typed(&["Live", "Album"])
            },
        ];
        assert!(
            edits_from(
                &reordered,
                &form(&[("type-primary", "album"), ("type-secondary", "live")])
            )
            .unwrap()
            .album
            .is_empty(),
            "the same types in another order and case are one type"
        );
    }

    #[test]
    fn an_artist_is_renamed_only_where_named_exactly() {
        let t = |path: &str, artist: &str, album_artist: &str| sift::meta::Track {
            artist: Some(artist.into()),
            album_artist: Some(album_artist.into()),
            ..track(path, "One", "Monstre", None)
        };
        let tracks = [
            t("/m/a/1.flac", "Jay Z", "Jay Z"),
            t("/m/a/2.flac", "Jay Z & Kanye West", "Jay Z"),
            t("/m/a/3.flac", "Kanye West", "Jay Z"),
        ];
        let edits = batch_edits_from(
            &tracks,
            &form(&[("rename-from", " Jay Z "), ("rename-to", "JAY-Z")]),
        )
        .unwrap();
        let set = |f: &str| (f.to_string(), Some("JAY-Z".to_string()));
        assert_eq!(
            edits.tracks,
            [
                (
                    "/m/a/1.flac".into(),
                    vec![set("artist"), set("albumartist")]
                ),
                ("/m/a/2.flac".into(), vec![set("albumartist")]),
                ("/m/a/3.flac".into(), vec![set("albumartist")]),
            ],
            "a collaboration naming the artist is left alone"
        );
        assert_eq!(
            batch_edits_from(
                &tracks,
                &form(&[("rename-from", "jay z"), ("rename-to", "JAY-Z")])
            )
            .unwrap()
            .tracks
            .len(),
            3,
            "the name is found in any case"
        );
        assert!(
            batch_edits_from(
                &tracks,
                &form(&[("rename-from", "Jay Z"), ("rename-to", "Jay Z")])
            )
            .unwrap()
            .is_empty(),
            "renamed to itself, nothing changes"
        );
        let credited = sift::meta::Track {
            artists: vec!["Jay Z".into()],
            ..t("/m/a/1.flac", "Jay Z", "Someone")
        };
        assert_eq!(
            batch_edits_from(
                &[credited],
                &form(&[("rename-from", "Jay Z"), ("rename-to", "JAY-Z")])
            )
            .unwrap()
            .tracks[0]
                .1,
            [set("artist"), set("artists")],
            "the credit is renamed rather than cleared, so its MusicBrainz id stays"
        );
        assert!(batch_edits_from(&tracks, &form(&[("rename-from", "Jay Z")])).is_err());
        assert!(batch_edits_from(&tracks, &form(&[("rename-to", "JAY-Z")])).is_err());
        assert!(
            batch_edits_from(
                &tracks,
                &form(&[("rename-from", "  "), ("rename-to", "Foo")])
            )
            .is_err(),
            "a blank old name is no name"
        );
        assert!(
            batch_edits_from(
                &tracks,
                &form(&[
                    ("rename-from", "Jay Z"),
                    ("rename-to", "JAY-Z"),
                    ("a-albumartist", "Someone")
                ])
            )
            .is_err()
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
    use axum_extra::extract::Query;
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use slsk_engine::slsk_proto::RawStr;
    use slsk_engine::slsk_proto::peer::{Directory, FileEntry};

    use crate::web::page::browse::*;

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

    fn scans(name: &str) -> Directory {
        Directory {
            name: RawStr::from(name.to_string()),
            files: ["front.jpg", "back.jpg"]
                .iter()
                .map(|f| FileEntry {
                    name: RawStr::from(f.to_string()),
                    size: 1,
                    extension: "jpg".into(),
                    attrs: Vec::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn folders_without_music_go_with_the_album_above_them() {
        let root = "music\\Recoil (FLAC)\\Albums";
        let dirs = vec![
            dir(&format!("{root}\\2000 Liquid (CD)"), 2),
            scans(&format!("{root}\\2000 Liquid (CD)\\Covers")),
            scans(&format!("{root}\\2000 Liquid (CD)\\CD-Extra\\Assets")),
            dir(&format!("{root}\\2007 subHuman (CD)\\CD1"), 3),
            scans(&format!("{root}\\2007 subHuman (CD)\\CD1\\Scans")),
            scans(&format!("{root}\\2007 subHuman (CD)\\PICTURES")),
            // Nothing above it holds music.
            scans("music\\Recoil (FLAC)\\Xtras"),
        ];
        let a = albums(&dirs, b"music");
        let summary: Vec<(String, Vec<String>)> = a
            .iter()
            .map(|(k, v)| {
                let mut subs: Vec<String> = v.iter().map(|f| f.2.clone()).collect();
                subs.dedup();
                (String::from_utf8_lossy(k).into_owned(), subs)
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                (
                    format!("{root}\\2000 Liquid (CD)"),
                    vec!["".into(), "Covers".into(), "CD-Extra/Assets".into()]
                ),
                (
                    format!("{root}\\2007 subHuman (CD)"),
                    vec!["CD1".into(), "CD1/Scans".into(), "PICTURES".into()]
                ),
            ]
        );
        // A scans folder opened on its own is no album.
        assert!(
            albums(
                &dirs,
                format!("{root}\\2000 Liquid (CD)\\Covers").as_bytes()
            )
            .is_empty()
        );
    }

    fn place(at: &str, open: &[&str]) -> Place {
        Place {
            username: "u".into(),
            at: at.as_bytes().to_vec(),
            open: open.iter().map(|o| o.as_bytes().to_vec()).collect(),
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
        let p = place("@@music\\A", &["@@music\\A\\One", "@@music\\A\\One\\CD1"]);
        let q = query(&p.href());
        assert_eq!(q.open, vec!["One", "One/CD1"]);
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
    fn a_long_folder_is_drawn_whole() {
        let dirs: Vec<Directory> = (0..2000)
            .map(|i| dir(&format!("@@m\\{i:05}"), 1))
            .chain([dir("@@m", 3000)])
            .collect();
        let html = tree(&place("@@m", &[]), &dirs, b"@@m", false);
        assert_eq!(html.matches("aria-expanded=").count(), 2000);
        assert_eq!(html.matches("type=\"checkbox\"").count(), 3000);
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

mod profile {
    use std::sync::Arc;

    use slsk_engine::slsk_proto::peer::UserInfo;
    use slsk_engine::slsk_proto::server::{UserStats, UserStatus};

    use crate::web::Draw;
    use crate::web::page::profile::*;

    /// A peer's profile, as its card draws it.
    fn peer(
        username: &str,
        banned: bool,
        server: &Server,
        info: Option<&UserInfo>,
        note: Option<String>,
        asking: bool,
    ) -> String {
        crate::web::page::profile::peer(username, banned, server, info, note, asking)
            .draw()
            .to_string()
    }

    fn info(picture: Option<&'static [u8]>) -> UserInfo {
        UserInfo {
            description: "<b>dub</b> techno".into(),
            picture: picture.map(bytes::Bytes::from_static),
            total_uploads: 42,
            queue_size: 3,
            slots_free: true,
            upload_permitted: None,
        }
    }

    #[test]
    fn a_peers_profile_shows_what_they_and_the_server_said() {
        let server = Server {
            status: Some(UserStatus::Away),
            stats: Some(UserStats {
                avg_speed: 2048,
                upload_num: 7,
                files: 1000,
                dirs: 90,
            }),
            interests: Some((vec!["dub".into()], vec!["noise".into()])),
        };
        let html = peer(
            "ann",
            false,
            &server,
            Some(&info(Some(b"\x89PNG\r\n\x1a\nxx"))),
            None,
            false,
        );
        assert!(html.contains("&#60;b&#62;dub&#60;/b&#62; techno"), "{html}");
        assert!(
            html.contains("/profile/picture?username=ann&#38;v="),
            "{html}"
        );
        assert!(html.contains("1000 files in 90 folders"));
        assert!(html.contains("2.0 KB/s"));
        assert!(html.contains(">42<"));
        assert!(
            html.contains(r#"<span class="status warn">away</span>"#),
            "{html}"
        );
        assert!(html.contains("noise"));
        assert!(html.contains("/browse?username=ann"));
    }

    #[test]
    fn a_picture_that_is_not_an_image_is_not_shown() {
        let html = peer(
            "ann",
            false,
            &Server::default(),
            Some(&info(Some(b"<svg/>"))),
            None,
            false,
        );
        assert!(!html.contains("/profile/picture"), "{html}");
    }

    #[test]
    fn without_the_peer_the_servers_part_still_shows() {
        let server = Server {
            stats: Some(UserStats {
                upload_num: 7,
                ..Default::default()
            }),
            ..Default::default()
        };
        let html = peer(
            "ann",
            false,
            &server,
            None,
            Some("ann did not send their profile".into()),
            false,
        );
        assert!(html.contains("did not send their profile"));
        assert!(html.contains(">7<"));
        assert!(!html.contains("No description."));
    }

    #[test]
    fn only_the_latest_answers_are_kept() {
        let cache = Cache::default();
        cache.put("first", Arc::new(info(None)));
        for n in 0..KEEP {
            cache.put(&format!("u{n}"), Arc::new(info(None)));
        }
        assert!(cache.get("first").is_none());
        assert!(cache.get(&format!("u{}", KEEP - 1)).is_some());
    }
}

/// The committed stylesheet asks for the font beside it by the hash of the
/// one koan-kit-web serves there, so a kit bump without `just css` fails.
#[test]
fn the_stylesheet_names_the_font_the_kit_serves() {
    assert!(include_str!("../../assets/app.css").contains(&kit::font::url("app.css")));
}

/// A fragment drawn twice from the same state is not sent again, though the kit names its buttons' own busy signals
/// afresh each time.
#[test]
fn a_redraw_from_the_same_state_compares_equal() {
    let draw = |url: &'static str| {
        kit::Button::bordered("Retry")
            .busy_key("job")
            .post(url)
            .to_string()
    };
    assert_eq!(
        unnamed(&draw("/jobs/1/retry")),
        unnamed(&draw("/jobs/1/retry"))
    );
    assert_ne!(
        unnamed(&draw("/jobs/1/retry")),
        unnamed(&draw("/jobs/2/retry"))
    );
}
