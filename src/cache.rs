//! On-disk caches under ~/.cache/psn-friends: profiles (names + avatar URLs
//! barely change, so we refetch them daily) and downloaded images.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

use crate::api::Api;
use crate::par::par_map;

const PROFILE_TTL_SECS: i64 = 24 * 3600;

#[derive(Serialize, Deserialize, Clone)]
pub struct CachedProfile {
    pub online_id: String,
    pub avatar_url: Option<String>,
    pub fetched_at: i64,
}

pub fn cache_dir() -> Result<PathBuf> {
    Ok(dirs::cache_dir()
        .context("no cache directory")?
        .join("psn-friends"))
}

/// Profiles for `ids`, from cache where fresh, otherwise fetched in bulk.
/// A friend whose profile can't be fetched (and isn't cached) is left out.
pub fn profiles(api: &Api, ids: &[String]) -> Result<HashMap<String, CachedProfile>> {
    let path = cache_dir()?.join("profiles.json");
    let mut cached: HashMap<String, CachedProfile> = fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();

    let now = chrono::Utc::now().timestamp();
    let stale: Vec<String> = ids
        .iter()
        .filter(|id| {
            cached
                .get(*id)
                .is_none_or(|p| now - p.fetched_at > PROFILE_TTL_SECS)
        })
        .cloned()
        .collect();

    if !stale.is_empty() {
        // Friends missing from the result keep their stale cache entry, if any.
        for (id, p) in api.profiles(&stale)? {
            let avatar_url = p.avatar_url().map(str::to_string);
            cached.insert(
                id,
                CachedProfile {
                    online_id: p.online_id,
                    avatar_url,
                    fetched_at: now,
                },
            );
        }
        // Drop people who are no longer friends.
        cached.retain(|id, _| ids.contains(id));
        write_atomic(&path, serde_json::to_string(&cached)?.as_bytes())?;
    }
    Ok(cached)
}

/// Download each URL once into the image cache; returns url -> local path.
/// Failed downloads are just missing from the result.
pub fn images(client: &Client, urls: Vec<String>) -> Result<HashMap<String, String>> {
    let dir = cache_dir()?.join("img");
    fs::create_dir_all(&dir)?;
    let results = par_map(&urls, 8, |url| {
        let path = dir.join(image_file_name(url));
        if path.exists() || download(client, url, &path).is_ok() {
            Some((url.clone(), path.to_string_lossy().into_owned()))
        } else {
            None
        }
    });
    Ok(results.into_iter().flatten().collect())
}

fn download(client: &Client, url: &str, path: &Path) -> Result<()> {
    let bytes = client.get(url).send()?.error_for_status()?.bytes()?;
    write_atomic(path, &bytes)
}

/// Stable file name per URL: FNV-1a hash plus the URL's extension.
fn image_file_name(url: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in url.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let path_part = url.split(['?', '#']).next().unwrap_or(url);
    let ext = path_part
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .filter(|e| matches!(e.as_str(), "png" | "jpg" | "jpeg" | "webp"))
        .unwrap_or_else(|| "img".into());
    format!("{hash:016x}.{ext}")
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::image_file_name;

    #[test]
    fn image_names_are_stable_and_keep_extension() {
        let a = image_file_name("https://psn-rsc.prod.dl.playstation.net/avatar/abc_m.png");
        assert_eq!(
            a,
            image_file_name("https://psn-rsc.prod.dl.playstation.net/avatar/abc_m.png")
        );
        assert!(a.ends_with(".png"));
        assert!(image_file_name("https://x/y.JPG?w=64").ends_with(".jpg"));
        assert!(image_file_name("https://x/noext").ends_with(".img"));
        assert_ne!(
            a,
            image_file_name("https://psn-rsc.prod.dl.playstation.net/avatar/abd_m.png")
        );
    }
}
