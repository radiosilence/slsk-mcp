//! Album art: the pictures an album's files carry and the cover images
//! beside them, and a replacement fetched or uploaded, made ready to write
//! as an import writes the art it finds.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_graphql::SimpleObject;

/// The names a cover image beside an album goes by, in the order players
/// such as Navidrome prefer them.
const COVER_STEMS: &[&str] = &["cover", "folder", "front", "album"];
const COVER_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png"];

/// The most an image fetched or uploaded may be: well above any scan of a
/// sleeve, well below what would strain memory, since it is held whole.
pub const MAX_BYTES: usize = 32 << 20;
/// The most art written as it came may be. A larger image within the
/// width imports embed is re-encoded as JPEG, and refused if that is still
/// larger: every file carries its own copy.
pub const MAX_KEPT: usize = 5 << 20;
/// How long fetching an image may take in all.
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// A picture embedded in a file's tags.
#[derive(SimpleObject, Clone, Debug, PartialEq)]
pub struct Picture {
    /// What it is for, as the tag says: `CoverFront`, `Other`, …
    pub kind: String,
    pub mime: Option<String>,
    pub bytes: usize,
    /// Absent when the picture cannot be decoded.
    pub width: Option<u32>,
    pub height: Option<u32>,
}

impl From<sift::meta::Embedded> for Picture {
    fn from(e: sift::meta::Embedded) -> Self {
        Self {
            kind: e.kind,
            mime: e.mime,
            bytes: e.bytes,
            width: e.width,
            height: e.height,
        }
    }
}

/// One file of an album and the pictures embedded in it.
#[derive(SimpleObject, Clone, Debug)]
pub struct FileArt {
    /// The file's path within its album.
    pub file: String,
    pub pictures: Vec<Picture>,
    /// Why its pictures could not be read.
    pub error: Option<String>,
}

/// A cover image in an album's folder.
#[derive(SimpleObject, Clone, Debug)]
pub struct CoverFile {
    pub name: String,
    pub bytes: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// The art an album has: in each file, and beside them.
#[derive(SimpleObject, Clone, Debug)]
pub struct AlbumArt {
    pub path: String,
    pub files: Vec<FileArt>,
    /// Cover images in the folder, the one players show first leading.
    pub cover_files: Vec<CoverFile>,
}

/// What removing an album's embedded art did, or with `preview`, would do.
#[derive(SimpleObject, Clone, Debug)]
pub struct ArtRemoved {
    pub path: String,
    /// The files that carry (or carried) pictures, within the album.
    pub files: Vec<String>,
    /// Cover images in the folder, left as they are: players show one of
    /// these before embedded art.
    pub cover_files: Vec<String>,
}

/// What setting an album's art wrote, or with `preview`, would write.
#[derive(SimpleObject, Clone, Debug)]
pub struct ArtSet {
    pub path: String,
    pub mime: String,
    pub width: u32,
    pub height: u32,
    pub bytes: usize,
    /// Scaled down to the largest width imports embed, and re-encoded.
    pub resized: bool,
    /// The files given it as their only embedded picture, within the album.
    pub embedded: Vec<String>,
    /// The cover image written beside them.
    pub cover_file: Option<String>,
    /// Cover images already there, moved to the bin to make way for it.
    pub binned: Vec<String>,
    /// Cover images left in the folder: players show one of these before
    /// embedded art.
    pub kept: Vec<String>,
}

/// Where new art goes: into the files' tags, as imports put it, a cover
/// image beside them, or both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub embed: bool,
    pub file: bool,
}

/// Whether `path` is inside `root` once links are followed, so a link
/// planted in an album's folder cannot reach anything outside the library.
pub fn inside(root: &Path, path: &Path) -> bool {
    match (root.canonicalize(), path.canonicalize()) {
        (Ok(root), Ok(path)) => path.starts_with(root),
        _ => false,
    }
}

/// The cover images in `dir`, the one players show first leading. Only
/// regular files inside `root` count.
pub fn cover_files(root: &Path, dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(usize, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let path = e.path();
            let stem = path.file_stem()?.to_str()?.to_ascii_lowercase();
            let ext = path.extension()?.to_str()?.to_ascii_lowercase();
            let rank = COVER_STEMS.iter().position(|s| *s == stem)?;
            COVER_EXTENSIONS
                .contains(&ext.as_str())
                .then_some((rank, path))
        })
        .filter(|(_, p)| p.is_file() && inside(root, p))
        .collect();
    found.sort();
    found.into_iter().map(|(_, p)| p).collect()
}

/// An image's type, from its first bytes.
pub fn mime_of(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"GIF8") {
        Some("image/gif")
    } else if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// An image's width and height, when it decodes.
pub fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// A replacement ready to write.
#[derive(Clone, Debug)]
pub struct Image {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
    pub resized: bool,
}

impl Image {
    /// What a cover image of it is called.
    pub fn file_name(&self) -> &'static str {
        if self.mime == "image/png" {
            "cover.png"
        } else {
            "cover.jpg"
        }
    }
}

/// `bytes` as art to write, as an import writes what it finds: a JPEG or
/// PNG, scaled down past the largest width imports embed. A person chose
/// it, so it is not held to the smallest width or the shape an import
/// would refuse.
pub fn prepare(bytes: &[u8], cfg: &sift::Config) -> anyhow::Result<Image> {
    let rules = sift::art::ArtRules {
        min_width: 0,
        max_width: cfg.art_max_width,
        quality: cfg.art_quality,
        ratio: None,
    };
    let prepared = sift::art::prepare(bytes, &rules)
        .ok_or_else(|| anyhow::anyhow!("not a JPEG or PNG image that can be read"))?;
    let mime = mime_of(&prepared.bytes)
        .filter(|m| matches!(*m, "image/jpeg" | "image/png"))
        .ok_or_else(|| anyhow::anyhow!("not a JPEG or PNG image"))?;
    let (width, height) =
        dimensions(&prepared.bytes).ok_or_else(|| anyhow::anyhow!("the image cannot be read"))?;
    if prepared.bytes.len() <= MAX_KEPT {
        return Ok(Image {
            bytes: prepared.bytes,
            mime,
            width,
            height,
            resized: prepared.resized,
        });
    }
    let mut out = Vec::new();
    image::load_from_memory(&prepared.bytes)?
        .to_rgb8()
        .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
            &mut out,
            cfg.art_quality.clamp(1, 100),
        ))?;
    anyhow::ensure!(
        out.len() <= MAX_KEPT,
        "the image is {} bytes even as a JPEG, more than the {MAX_KEPT} embedded art may be; use a smaller one",
        out.len()
    );
    Ok(Image {
        bytes: out,
        mime: "image/jpeg",
        width,
        height,
        resized: true,
    })
}

/// The longest side a profile picture is kept at. Clients show it small,
/// and it travels whole in every answer to a peer asking about this user.
const AVATAR_SIDE: u32 = 512;

/// `bytes` as a profile picture: a JPEG or PNG, scaled down to fit
/// [`AVATAR_SIDE`] and kept as a JPEG.
pub fn avatar(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    let img = image::load_from_memory(bytes)
        .map_err(|_| anyhow::anyhow!("not a JPEG or PNG image that can be read"))?;
    let img = if img.width() > AVATAR_SIDE || img.height() > AVATAR_SIDE {
        img.thumbnail(AVATAR_SIDE, AVATAR_SIDE)
    } else {
        img
    };
    let mut out = Vec::new();
    img.to_rgb8()
        .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
            &mut out, 85,
        ))?;
    Ok(out)
}

/// Where a cover image replaced in the album at `dir` goes: under the
/// bin's `.art`, by the album's path and the time, so it never takes the
/// place binning the album itself would use, and a second replacement
/// never meets the first.
pub fn binned_cover(root: &Path, bin: &Path, cover: &Path, at: u64) -> anyhow::Result<PathBuf> {
    let rel = cover
        .parent()
        .and_then(|d| d.strip_prefix(root).ok())
        .ok_or_else(|| anyhow::anyhow!("{} is not under the library", cover.display()))?;
    let dir = bin.join(".art").join(rel);
    let stem = cover.file_stem().unwrap_or_default().to_string_lossy();
    let ext = cover.extension().unwrap_or_default().to_string_lossy();
    (0..)
        .map(|n| match n {
            0 => dir.join(format!("{stem}.{at}.{ext}")),
            n => dir.join(format!("{stem}.{at}-{n}.{ext}")),
        })
        .find(|p| !p.exists())
        .ok_or_else(|| anyhow::anyhow!("no free name in the bin"))
}

/// Put the new cover written at `part` in place at `dest`, and take the
/// old covers out of the album into the bin, each `(from, to)` in `moves`.
/// Each old cover is linked into the bin before the new one takes its
/// place, so none is lost whichever step fails, and the error says where
/// things stand.
pub async fn put_cover(
    part: &Path,
    dest: &Path,
    moves: &[(PathBuf, PathBuf)],
) -> anyhow::Result<()> {
    for (from, to) in moves {
        let kept = async {
            if let Some(parent) = to.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            if tokio::fs::hard_link(from, to).await.is_err() {
                tokio::fs::copy(from, to).await?;
            }
            anyhow::Ok(())
        }
        .await;
        if let Err(e) = kept {
            return Err(e.context(format!(
                "keeping {} in the bin; the new cover waits at {} and the old ones are in place",
                from.display(),
                part.display()
            )));
        }
    }
    if let Err(e) = tokio::fs::rename(part, dest).await {
        return Err(anyhow::Error::new(e).context(format!(
            "putting the new cover in place; it waits at {} and the old ones are in place, with copies in the bin",
            part.display()
        )));
    }
    let new = tokio::fs::metadata(dest).await?;
    for (from, _) in moves {
        // An old cover the rename replaced (the same name, or the same file
        // under another case of it) is gone already.
        let Ok(old) = tokio::fs::metadata(from).await else {
            continue;
        };
        if same_file(&old, &new) {
            continue;
        }
        if let Err(e) = tokio::fs::remove_file(from).await {
            return Err(anyhow::Error::new(e).context(format!(
                "taking {} out of the album; the new cover is in place and a copy of the old one is in the bin",
                from.display()
            )));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    (a.dev(), a.ino()) == (b.dev(), b.ino())
}

/// Fetch an image from an http or https URL, refusing a web page or
/// anything over [`MAX_BYTES`]. Only the host is ever named: links to
/// images can carry access tokens.
pub async fn fetch(http: &reqwest::Client, url: &str) -> anyhow::Result<Vec<u8>> {
    let url = crate::jobs::check_url(url)?;
    let host = url.host_str().unwrap_or_default().to_string();
    tracing::info!(%host, "fetching album art");
    let mut resp = http
        .get(url)
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| anyhow::anyhow!("could not fetch from {host}: {}", e.without_url()))?;
    if resp
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| t.starts_with("text/html"))
    {
        anyhow::bail!("{host} answered with a web page, not an image: use the image's own address");
    }
    if resp.content_length().is_some_and(|n| n > MAX_BYTES as u64) {
        anyhow::bail!("{host} offers more than the {MAX_BYTES} bytes an image may be");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| anyhow::anyhow!("the fetch from {host} broke off: {}", e.without_url()))?
    {
        if bytes.len() + chunk.len() > MAX_BYTES {
            anyhow::bail!("{host} sent more than the {MAX_BYTES} bytes an image may be");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("slsk-art-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut out = Vec::new();
        image::DynamicImage::new_rgb8(width, height)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn an_avatar_is_scaled_to_fit_and_kept_as_a_jpeg() {
        let out = avatar(&png(2000, 1000)).unwrap();
        assert_eq!(mime_of(&out), Some("image/jpeg"));
        assert_eq!(dimensions(&out), Some((512, 256)));
        assert_eq!(dimensions(&avatar(&png(64, 64)).unwrap()), Some((64, 64)));
        assert!(avatar(b"not an image").is_err());
    }

    #[test]
    fn cover_images_are_found_in_the_order_players_prefer() {
        let root = scratch();
        let album = root.join("Artist/Album");
        std::fs::create_dir_all(&album).unwrap();
        for name in [
            "Folder.JPG",
            "cover.png",
            "back.jpg",
            "cover.txt",
            "01.flac",
        ] {
            std::fs::write(album.join(name), b"x").unwrap();
        }
        std::fs::create_dir(album.join("front.jpg")).unwrap();
        let names: Vec<_> = cover_files(&root, &album)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["cover.png", "Folder.JPG"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_link_out_of_the_library_is_not_a_cover() {
        let outside = scratch();
        std::fs::write(outside.join("secret.jpg"), b"x").unwrap();
        let root = scratch();
        let album = root.join("Album");
        std::fs::create_dir_all(&album).unwrap();
        std::os::unix::fs::symlink(outside.join("secret.jpg"), album.join("cover.jpg")).unwrap();
        assert!(cover_files(&root, &album).is_empty());
        assert!(!inside(
            &root,
            &album.join("../../").join(outside.file_name().unwrap())
        ));
        assert!(!inside(&root, &album.join("missing.jpg")));
        assert!(inside(&root, &album));
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn an_image_is_kept_as_it_is_unless_wider_than_imports_embed() {
        let cfg = sift::Config {
            art_max_width: 100,
            ..Default::default()
        };
        let small = prepare(&png(40, 30), &cfg).unwrap();
        assert_eq!((small.width, small.height, small.resized), (40, 30, false));
        assert_eq!((small.mime, small.file_name()), ("image/png", "cover.png"));
        let large = prepare(&png(400, 400), &cfg).unwrap();
        assert_eq!((large.width, large.height, large.resized), (100, 100, true));
        assert_eq!((large.mime, large.file_name()), ("image/jpeg", "cover.jpg"));
    }

    #[test]
    fn a_large_image_within_the_width_is_re_encoded() {
        // Noise, so PNG cannot compress it below the cap.
        let mut seed = 1u32;
        let img = image::RgbImage::from_fn(1500, 1500, |_, _| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            let b = seed.to_le_bytes();
            image::Rgb([b[1], b[2], b[3]])
        });
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        assert!(bytes.len() > MAX_KEPT);
        let cfg = sift::Config {
            art_max_width: 0,
            art_quality: 50,
            ..Default::default()
        };
        let image = prepare(&bytes, &cfg).unwrap();
        assert_eq!(
            (image.mime, image.width, image.resized),
            ("image/jpeg", 1500, true)
        );
        assert!(image.bytes.len() <= MAX_KEPT);
    }

    #[test]
    fn replaced_covers_never_meet_each_other_or_the_album_in_the_bin() {
        let root = scratch();
        let bin = scratch();
        let album = root.join("Artist/Album");
        std::fs::create_dir_all(&album).unwrap();
        let mut binned = Vec::new();
        for _ in 0..2 {
            let cover = album.join("cover.jpg");
            std::fs::write(&cover, b"old").unwrap();
            let to = binned_cover(&root, &bin, &cover, 1_700_000_000).unwrap();
            std::fs::create_dir_all(to.parent().unwrap()).unwrap();
            std::fs::rename(&cover, &to).unwrap();
            binned.push(to);
        }
        assert_ne!(binned[0], binned[1]);
        assert!(binned.iter().all(|p| p.exists()));
        assert!(binned[0].starts_with(bin.join(".art/Artist/Album")));
        // Binning the album itself goes to `<bin>/Artist/Album`, still free.
        assert!(!bin.join("Artist/Album").exists());
        assert!(binned_cover(&root, &bin, Path::new("/elsewhere/cover.jpg"), 1).is_err());
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(bin).unwrap();
    }

    #[tokio::test]
    async fn the_new_cover_leaves_no_old_one_beside_it() {
        let root = scratch();
        let bin = scratch();
        let album = root.join("Album");
        std::fs::create_dir_all(&album).unwrap();
        let old = album.join("Cover.JPG");
        std::fs::write(&old, b"old").unwrap();
        // Whether this filesystem tells names apart by case.
        let folds_case = album.join("cover.jpg").exists();
        let part = album.join(".cover.jpg.part");
        std::fs::write(&part, b"new").unwrap();
        let moves = vec![(old.clone(), binned_cover(&root, &bin, &old, 1).unwrap())];
        put_cover(&part, &album.join("cover.jpg"), &moves)
            .await
            .unwrap();
        let mut left: Vec<String> = std::fs::read_dir(&album)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        // A case-folding filesystem may keep the old spelling of the name.
        if folds_case {
            assert_eq!(left.len(), 1);
        } else {
            assert_eq!(left, ["cover.jpg"]);
        }
        assert_eq!(std::fs::read(album.join("cover.jpg")).unwrap(), b"new");
        assert_eq!(std::fs::read(&moves[0].1).unwrap(), b"old");
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(bin).unwrap();
    }

    #[tokio::test]
    async fn a_cover_of_another_name_goes_to_the_bin() {
        let root = scratch();
        let bin = scratch();
        let album = root.join("Album");
        std::fs::create_dir_all(&album).unwrap();
        let old = album.join("folder.jpg");
        std::fs::write(&old, b"old").unwrap();
        let part = album.join(".cover.jpg.part");
        std::fs::write(&part, b"new").unwrap();
        let moves = vec![(old.clone(), binned_cover(&root, &bin, &old, 1).unwrap())];
        put_cover(&part, &album.join("cover.jpg"), &moves)
            .await
            .unwrap();
        assert!(!old.exists() && !part.exists());
        assert_eq!(std::fs::read(album.join("cover.jpg")).unwrap(), b"new");
        assert_eq!(std::fs::read(&moves[0].1).unwrap(), b"old");
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(bin).unwrap();
    }

    #[test]
    fn what_is_not_an_image_is_refused() {
        let cfg = sift::Config::default();
        assert!(prepare(b"<html>not a picture</html>", &cfg).is_err());
        assert!(prepare(&png(10, 10)[..20], &cfg).is_err());
    }

    #[tokio::test]
    async fn only_http_urls_are_fetched() {
        let http = reqwest::Client::new();
        for url in [
            "file:///etc/passwd",
            "ftp://example.com/a.jpg",
            "not a url",
            "http://",
        ] {
            assert!(fetch(&http, url).await.is_err(), "{url}");
        }
    }
}
