//! Favicon fetching and disk caching for URL domains via Google's favicon service.
//! Non-critical: all failures are silent and callers fall back to chain icon.
//!
//! Every fetch runs on a background worker; a cache file is published
//! atomically so a renderer can never observe a partial PNG.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::core::paths::images_dir;

/// Set when a favicon cache file is published for the first time. A card reads
/// its icon straight from that file, so the UI poll loop consumes this flag to
/// repaint the lists that may show the newly available favicon.
static FAVICON_READY: AtomicBool = AtomicBool::new(false);

/// Consume the "a favicon cache file just appeared" signal.
///
/// Returns `true` at most once per published favicon, even when several calls
/// race: the repaint it asks for is idempotent, so a lost wakeup is harmless.
pub(crate) fn take_favicon_ready() -> bool {
    FAVICON_READY.swap(false, Ordering::SeqCst)
}

/// Sanitize a domain string for use as a filename (remove port and invalid chars).
fn sanitize_domain(domain: &str) -> String {
    let without_port = domain.split(':').next().unwrap_or(domain);
    without_port
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
}

fn cache_path(domain_or_url: &str) -> Option<PathBuf> {
    let host = crate::core::secret::url_clean_host(domain_or_url)?;
    Some(
        images_dir()
            .join("icons")
            .join(format!("favicon_{}.png", sanitize_domain(&host))),
    )
}

/// Check if a favicon is cached locally for the given domain.
/// Returns the file path if it exists — no network request.
pub fn favicon_cache_path(domain: &str) -> Option<String> {
    let path = cache_path(domain)?;
    if path.exists() {
        Some(path.to_string_lossy().to_string())
    } else {
        None
    }
}

/// Try to fetch a favicon from Google's service and cache it to disk.
/// Returns the disk path if already cached or successfully fetched.
/// Returns `None` on any failure (network, non-200, disk write).
pub fn ensure_favicon_cached(domain: &str) -> Option<String> {
    let host = crate::core::secret::url_clean_host(domain)?;
    let path = cache_path(&host)?;

    // --- Already cached ---
    if path.exists() {
        return Some(path.to_string_lossy().to_string());
    }

    // --- Ensure icons directory exists ---
    let _ = std::fs::create_dir_all(path.parent()?);

    let mut service_url = url::Url::parse("https://www.google.com/s2/favicons").ok()?;
    service_url
        .query_pairs_mut()
        .append_pair("domain", &host)
        .append_pair("sz", "32");

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(3))
        .timeout_read(Duration::from_secs(5))
        .build();

    match agent.get(service_url.as_str()).call() {
        Ok(response) => {
            let mut body = Vec::new();
            if response.into_reader().read_to_end(&mut body).is_err() {
                return None;
            }
            // --- Quick PNG validation: check magic bytes ---
            if body.len() < 8 || &body[..4] != b"\x89PNG" {
                return None;
            }
            publish_favicon(&path, &body)
        }
        Err(_) => None,
    }
}

/// Publish fetched favicon bytes so the cache file appears complete or not at
/// all.
///
/// The bytes go to a unique sibling temp file and are moved into place in one
/// step. This matters for more than tidiness: the UI image cache keys on the
/// file path and remembers a failed decode for the lifetime of the entry, so a
/// renderer that read a half-written PNG would keep showing the fallback icon
/// long after the fetch completed.
///
/// Returns the cache path, or `None` when the bytes could not be published.
fn publish_favicon(path: &std::path::Path, body: &[u8]) -> Option<String> {
    let file_name = path.file_name()?.to_string_lossy().to_string();
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let tmp_path = path.with_file_name(format!(".{file_name}.{unique}.tmp"));

    if std::fs::write(&tmp_path, body).is_err() {
        let _ = std::fs::remove_file(&tmp_path);
        return None;
    }
    if let Err(error) = crate::services::file_ops::replace_file(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        log::warn!(
            "favicon cache publish failed for {}: {error}",
            path.display()
        );
        return None;
    }

    FAVICON_READY.store(true, Ordering::SeqCst);
    Some(path.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "clippi-favicon-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entries(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn publishes_favicon_in_one_step_and_leaves_no_temp_file() {
        let dir = temp_dir("publish");
        let target = dir.join("favicon_example.com.png");
        let body = b"\x89PNG\r\n\x1a\nrest-of-the-icon".to_vec();

        let published = publish_favicon(&target, &body).expect("publish must succeed");

        assert_eq!(published, target.to_string_lossy().to_string());
        assert_eq!(std::fs::read(&target).unwrap(), body);
        assert_eq!(
            entries(&dir),
            vec!["favicon_example.com.png".to_string()],
            "the temp file must not survive a successful publish"
        );
    }

    #[test]
    fn replacing_an_existing_favicon_keeps_the_file_complete() {
        let dir = temp_dir("replace");
        let target = dir.join("favicon_example.com.png");
        std::fs::write(&target, b"\x89PNG\r\n\x1a\nold").unwrap();
        let body = b"\x89PNG\r\n\x1a\nnew".to_vec();

        publish_favicon(&target, &body).expect("publish must succeed");

        assert_eq!(std::fs::read(&target).unwrap(), body);
        assert_eq!(entries(&dir), vec!["favicon_example.com.png".to_string()]);
    }

    #[test]
    fn publishing_a_favicon_asks_for_a_repaint() {
        let dir = temp_dir("ready");
        // Any signal left by an earlier test is irrelevant: the flag is only
        // ever raised, so the assertion below holds for this publish alone.
        let _ = take_favicon_ready();

        publish_favicon(
            &dir.join("favicon_example.com.png"),
            b"\x89PNG\r\n\x1a\nicon",
        )
        .expect("publish must succeed");

        assert!(take_favicon_ready(), "publishing must ask for a repaint");
    }

    #[test]
    fn an_unpublishable_favicon_leaves_no_cache_file() {
        let dir = temp_dir("missing-parent");
        // The parent directory does not exist, so the write must fail.
        let target = dir.join("nested").join("favicon_example.com.png");

        assert!(publish_favicon(&target, b"\x89PNG\r\n\x1a\nicon").is_none());
        assert!(!target.exists(), "a failed publish must not leave a file");
    }
}
