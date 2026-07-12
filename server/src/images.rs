//! Crawl-time image capture: fetch, transcode, content-address, and rewrite.
//!
//! Blogs hotlink images at whatever size and quality their CMS emits — often a
//! multi-megabyte PNG of a screenshot. We can't reach those from an offline
//! Kindle, and even in the reader they cost bandwidth on every view. So at crawl
//! time we pull each remote `<img src>` through the sidecar (the one place
//! allowed to touch the network), transcode it to a Kindle-sized JPEG, and store
//! it keyed by the hash of the *transcoded* bytes. The stored HTML's `src` is
//! rewritten to `/img/<hash>`, which [`crate::api`] serves back from the DB.
//! Identical images across posts collapse to a single row, and a re-crawl of the
//! same URL skips the fetch entirely.

use crate::db::{self, Pool};
use crate::fetcher::Fetcher;
use base64::Engine;
use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::sync::LazyLock;
use std::time::Duration;

/// Kindle e-ink panels top out around 1264×1680. Anything larger is detail the
/// device can't show, and bytes the reader (and MOBI's per-image record) can't
/// afford.
pub const MAX_IMG_W: u32 = 1264;
pub const MAX_IMG_H: u32 = 1680;
/// MOBI7 silently drops an image whose record exceeds ~127 KB; staying under it
/// keeps every captured image embeddable and the DB lean.
pub const MAX_IMG_BYTES: usize = 120 * 1024;

static IMG_SRC: LazyLock<regex::Regex> =
    // Ammonia guarantees double-quoted attributes, so this is enough to find them.
    LazyLock::new(|| regex::Regex::new(r#"<img\b[^>]*?\bsrc="([^"]+)""#).unwrap());

/// The distinct `src` values in an article's HTML, in first-seen order.
pub fn image_srcs(html: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    IMG_SRC
        .captures_iter(html)
        .filter_map(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .filter(|u| seen.insert(u.clone()))
        .collect()
}

/// A remote source we'd try to capture, as opposed to an already-captured
/// `/img/<hash>` ref or an inline `data:` URI.
pub fn is_remote(src: &str) -> bool {
    src.starts_with("http://") || src.starts_with("https://")
}

/// The hash in a captured `/img/<hash>` src, if that's what this is.
pub fn local_hash(src: &str) -> Option<&str> {
    src.strip_prefix("/img/").filter(|h| is_hash(h))
}

/// Whether a string is a bare content-address — hex is all we ever mint, so the
/// route and every lookup can hard-reject anything else.
pub fn is_hash(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Content-address: the first 128 bits of the SHA-256 of the transcoded bytes.
/// Half the length of a full digest in every URL, and still collision-proof for
/// a personal library.
fn hash_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// Decode any web image and re-encode it as a JPEG that fits the byte budget,
/// shrinking until it does. Returns None for bytes we can't decode.
pub fn transcode(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut img = image::load_from_memory(bytes).ok()?;
    // `resize` fits to the box in both directions and will happily *upscale* a
    // thumbnail into a bigger, blurrier, heavier file — so only ever shrink.
    if img.width() > MAX_IMG_W || img.height() > MAX_IMG_H {
        img = img.resize(MAX_IMG_W, MAX_IMG_H, image::imageops::FilterType::Lanczos3);
    }
    for attempt in 0..4 {
        let mut out = Cursor::new(Vec::new());
        // JPEG quality steps down each retry; a screenshot survives 60 fine.
        let quality = 82 - attempt * 8;
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
        if img.to_rgb8().write_with_encoder(encoder).is_err() {
            return None;
        }
        let out = out.into_inner();
        if out.len() <= MAX_IMG_BYTES || attempt == 3 {
            return Some(out);
        }
        // Still too big: shrink the pixels and try again at lower quality.
        img = img.resize(
            img.width() * 7 / 10,
            img.height() * 7 / 10,
            image::imageops::FilterType::Triangle,
        );
    }
    None
}

/// Fetch, transcode, store, and rewrite every remote `<img src>` in `html` to a
/// local `/img/<hash>`. Already-captured URLs are reused without a re-fetch;
/// images that fail to fetch or decode keep their original remote `src` (the
/// reader still shows their alt text). Returns the rewritten HTML — unchanged if
/// there are no remote images or the sidecar batch fails outright.
pub async fn capture_html(pool: &Pool, fetch: &Fetcher, html: &str, referer: &str) -> String {
    let remote: Vec<String> = image_srcs(html).into_iter().filter(|s| is_remote(s)).collect();
    if remote.is_empty() {
        return html.to_string();
    }

    // What we've already captured (memoized url → hash), so neither a re-crawl
    // nor the backfill re-fetches the same image.
    let known = {
        let urls = remote.clone();
        db::call(pool, move |c| db::image_hashes_for_urls(c, &urls))
            .await
            .unwrap_or_default()
    };
    let missing: Vec<String> = remote
        .iter()
        .filter(|u| !known.contains_key(*u))
        .cloned()
        .collect();

    let mut fresh: HashMap<String, String> = HashMap::new();
    if !missing.is_empty() {
        match fetch.images(&missing, referer).await {
            Ok(results) => {
                let mut to_store: Vec<(String, String, Vec<u8>)> = Vec::new();
                for r in results {
                    let Some(b64) = r.data_b64.as_deref().filter(|_| r.ok) else {
                        // A blocked or dead host shows up here at debug.
                        tracing::debug!(
                            "image {} unavailable: {}",
                            r.url,
                            r.error.as_deref().unwrap_or("no data")
                        );
                        continue;
                    };
                    let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(b64) else {
                        continue;
                    };
                    // Decode + resize + re-encode is CPU-bound; keep it off the
                    // async runtime so other fetches keep flowing.
                    let jpeg = match tokio::task::spawn_blocking(move || transcode(&raw)).await {
                        Ok(Some(jpeg)) => jpeg,
                        _ => continue,
                    };
                    let hash = hash_bytes(&jpeg);
                    fresh.insert(r.url.clone(), hash.clone());
                    to_store.push((r.url, hash, jpeg));
                }
                if !to_store.is_empty()
                    && let Err(e) = db::call(pool, move |c| db::store_images(c, &to_store)).await
                {
                    tracing::warn!("storing captured images: {e:#}");
                }
            }
            Err(e) => tracing::warn!("fetching images for {referer}: {e:#}"),
        }
    }

    // Rewrite every remote src we now have a hash for. iepub and the browser
    // both key on the exact `src` string, so replace it verbatim.
    let mut out = html.to_string();
    for url in &remote {
        if let Some(hash) = known.get(url).or_else(|| fresh.get(url)) {
            out = out.replace(&format!("src=\"{url}\""), &format!("src=\"/img/{hash}\""));
        }
    }
    out
}

/// The bytes of a captured image, by hash. `None` for an unknown or malformed
/// hash.
pub async fn read(pool: &Pool, hash: &str) -> Option<Vec<u8>> {
    if !is_hash(hash) {
        return None;
    }
    let hash = hash.to_string();
    db::call(pool, move |c| db::image_bytes(c, &hash))
        .await
        .ok()
        .flatten()
}

/// One-time migration for articles crawled before capture existed: rewrite each
/// one's stored HTML, pulling its images local as we go. Runs in the background
/// at a gentle pace — the reader and exports work without it (they fall back to
/// capturing on demand); this just lets the whole backlog benefit up front.
pub fn spawn_backfill(pool: Pool, fetch: Fetcher) {
    tokio::spawn(async move {
        let items = match db::call(&pool, db::articles_with_remote_images).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("image backfill: listing articles failed: {e:#}");
                return;
            }
        };
        if items.is_empty() {
            return;
        }
        tracing::info!("image backfill: {} articles with remote images", items.len());

        let (mut migrated, mut failed) = (0usize, 0usize);
        for (id, url) in items {
            let html = match db::call(&pool, move |c| db::article_content(c, id)).await {
                Ok(Some(h)) => h,
                _ => continue,
            };
            let rewritten = capture_html(&pool, &fetch, &html, &url).await;
            if rewritten != html {
                let saved = rewritten.clone();
                match db::call(&pool, move |c| db::update_article_html(c, id, &saved)).await {
                    Ok(()) => migrated += 1,
                    Err(e) => {
                        failed += 1;
                        tracing::debug!("backfill update of article {id} failed: {e:#}");
                    }
                }
            }
            // One article a second: unhurried enough to leave a single blog's
            // CDN and the sidecar free for live crawls.
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        tracing::info!("image backfill done: {migrated} migrated, {failed} failed");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_distinct_srcs_in_order() {
        let html = r#"<img src="https://a/1.png"><p>x</p><img src="https://b/2.jpg" alt="y"><img src="https://a/1.png">"#;
        assert_eq!(
            image_srcs(html),
            vec!["https://a/1.png".to_string(), "https://b/2.jpg".to_string()]
        );
    }

    #[test]
    fn ignores_html_without_images() {
        assert!(image_srcs("<p>no pictures here</p>").is_empty());
    }

    #[test]
    fn classifies_remote_local_and_inline_srcs() {
        assert!(is_remote("https://a/1.png"));
        assert!(is_remote("http://a/1.png"));
        assert!(!is_remote("/img/deadbeef"));
        assert!(!is_remote("data:image/png;base64,AAAA"));

        assert_eq!(local_hash("/img/deadbeef01"), Some("deadbeef01"));
        assert_eq!(local_hash("/img/not-hex!"), None, "only hex is a valid id");
        assert_eq!(local_hash("https://a/1.png"), None);
    }

    #[test]
    fn hash_is_hex_stable_and_content_dependent() {
        let h = hash_bytes(b"hello");
        assert_eq!(h.len(), 32);
        assert!(is_hash(&h));
        assert_eq!(h, hash_bytes(b"hello"), "same bytes, same hash");
        assert_ne!(h, hash_bytes(b"world"), "different bytes, different hash");
    }

    #[test]
    fn transcode_shrinks_a_big_png_to_a_bounded_jpeg() {
        // A 2000×2000 image is over the pixel cap; the result must be a JPEG
        // (FF D8 FF) under the byte budget.
        let big = image::RgbImage::from_fn(2000, 2000, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        let mut buf = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(big)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        let out = transcode(&buf.into_inner()).expect("should transcode");
        assert_eq!(&out[..3], &[0xFF, 0xD8, 0xFF], "not a jpeg");
        assert!(out.len() <= MAX_IMG_BYTES, "over budget: {}", out.len());
    }

    #[test]
    fn transcode_rejects_non_images() {
        assert!(transcode(b"this is not an image").is_none());
    }
}
