//! Turn raw PSN data into the "who's doing what" list the CLI and the GNOME
//! extension show. The JSON shape here is the contract with the extension.

use std::collections::HashMap;

use serde::Serialize;

use crate::api::Presence;
use crate::cache::CachedProfile;

#[derive(Serialize, Debug)]
pub struct Snapshot {
    pub fetched_at: String,
    pub friends: Vec<Friend>,
}

#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Playing,
    Online,
    Offline,
}

#[derive(Serialize, Debug)]
pub struct Friend {
    pub account_id: String,
    pub online_id: String,
    /// Local path to the cached avatar image.
    pub avatar: Option<String>,
    pub state: State,
    pub platform: Option<String>,
    pub game: Option<Game>,
    pub last_online: Option<String>,
}

#[derive(Serialize, Debug)]
pub struct Game {
    pub title: String,
    pub title_id: Option<String>,
    /// Local path to the cached game art.
    pub icon: Option<String>,
    pub status: Option<String>,
}

/// URLs of every image the snapshot will want (avatars + game art).
pub fn image_urls(
    ids: &[String],
    profiles: &HashMap<String, CachedProfile>,
    presences: &HashMap<String, Presence>,
) -> Vec<String> {
    let mut urls: Vec<String> = ids
        .iter()
        .filter_map(|id| profiles.get(id)?.avatar_url.clone())
        .chain(
            ids.iter()
                .filter_map(|id| game_icon_url(presences.get(id)?)),
        )
        .collect();
    urls.sort();
    urls.dedup();
    urls
}

fn game_icon_url(p: &Presence) -> Option<String> {
    let g = p.game_title_info_list.first()?;
    g.concept_icon_url
        .clone()
        .or_else(|| g.np_title_icon_url.clone())
}

pub fn build(
    ids: &[String],
    profiles: &HashMap<String, CachedProfile>,
    presences: &HashMap<String, Presence>,
    images: &HashMap<String, String>,
) -> Snapshot {
    let local = |url: Option<&String>| url.and_then(|u| images.get(u)).cloned();

    let mut friends: Vec<Friend> = ids
        .iter()
        .filter_map(|id| {
            let profile = profiles.get(id)?;
            let presence = presences.get(id).cloned().unwrap_or_default();
            let info = presence.primary_platform_info.clone().unwrap_or_default();
            let game_info = presence.game_title_info_list.first();

            let online = info.online_status.as_deref() == Some("online")
                || presence.availability.as_deref() == Some("availableToPlay");
            let state = if game_info.is_some() {
                State::Playing
            } else if online {
                State::Online
            } else {
                State::Offline
            };

            let platform = game_info
                .and_then(|g| g.launch_platform.as_deref().or(g.format.as_deref()))
                .or(info.platform.as_deref())
                .map(platform_name);

            let game = game_info.map(|g| Game {
                title: g
                    .title_name
                    .clone()
                    .unwrap_or_else(|| "Unknown game".into()),
                title_id: g.np_title_id.clone(),
                icon: local(game_icon_url(&presence).as_ref()),
                status: g.game_status.clone().filter(|s| !s.trim().is_empty()),
            });

            Some(Friend {
                account_id: id.clone(),
                online_id: profile.online_id.clone(),
                avatar: local(profile.avatar_url.as_ref()),
                state,
                platform,
                game,
                last_online: info.last_online_date.or(presence.last_available_date),
            })
        })
        .collect();

    // Playing, then online (alphabetical), then offline (most recently seen first).
    friends.sort_by(|a, b| {
        a.state.cmp(&b.state).then_with(|| match a.state {
            State::Offline => b.last_online.cmp(&a.last_online),
            _ => a.online_id.to_lowercase().cmp(&b.online_id.to_lowercase()),
        })
    });

    Snapshot {
        fetched_at: chrono::Utc::now().to_rfc3339(),
        friends,
    }
}

fn platform_name(raw: &str) -> String {
    match raw.to_ascii_uppercase().as_str() {
        "MOBILE_APP" => "PS App".into(),
        "PSPC" => "PC".into(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{GameTitleInfo, PlatformInfo};

    fn profile(name: &str) -> CachedProfile {
        CachedProfile {
            online_id: name.into(),
            avatar_url: Some(format!("https://a/{name}.png")),
            fetched_at: 0,
        }
    }

    fn presence(status: &str, platform: &str, last: &str, game: Option<&str>) -> Presence {
        Presence {
            primary_platform_info: Some(PlatformInfo {
                online_status: Some(status.into()),
                platform: Some(platform.into()),
                last_online_date: Some(last.into()),
            }),
            game_title_info_list: game
                .map(|t| {
                    vec![GameTitleInfo {
                        title_name: Some(t.into()),
                        launch_platform: Some("PS5".into()),
                        concept_icon_url: Some("https://g/icon.png".into()),
                        ..Default::default()
                    }]
                })
                .unwrap_or_default(),
            ..Default::default()
        }
    }

    #[test]
    fn sorts_and_classifies() {
        let ids: Vec<String> = ["1", "2", "3", "4", "5"].map(String::from).to_vec();
        let profiles: HashMap<_, _> = [
            ("1", "zed"),
            ("2", "amy"),
            ("3", "old"),
            ("4", "recent"),
            ("5", "player"),
        ]
        .map(|(id, n)| (id.to_string(), profile(n)))
        .into();
        let presences: HashMap<_, _> = [
            ("1", presence("online", "ps4", "2026-11-17T10:00:00Z", None)),
            (
                "2",
                presence("online", "MOBILE_APP", "2026-11-17T10:00:00Z", None),
            ),
            (
                "3",
                presence("offline", "PS5", "2026-10-01T10:00:00Z", None),
            ),
            (
                "4",
                presence("offline", "PS5", "2026-11-16T10:00:00Z", None),
            ),
            (
                "5",
                presence("online", "PS5", "2026-11-17T10:00:00Z", Some("Some Game")),
            ),
        ]
        .map(|(id, p)| (id.to_string(), p))
        .into();
        let images: HashMap<_, _> = [(
            "https://g/icon.png".to_string(),
            "/cache/icon.png".to_string(),
        )]
        .into();

        let snap = build(&ids, &profiles, &presences, &images);
        let names: Vec<_> = snap.friends.iter().map(|f| f.online_id.as_str()).collect();
        assert_eq!(names, ["player", "amy", "zed", "recent", "old"]);

        let player = &snap.friends[0];
        assert_eq!(player.state, State::Playing);
        assert_eq!(
            player.game.as_ref().unwrap().icon.as_deref(),
            Some("/cache/icon.png")
        );
        assert_eq!(player.avatar, None, "avatar not downloaded in this test");
        assert_eq!(snap.friends[1].platform.as_deref(), Some("PS App"));
        assert_eq!(snap.friends[2].platform.as_deref(), Some("PS4"));

        let urls = image_urls(&ids, &profiles, &presences);
        assert_eq!(urls.len(), 6, "5 avatars + 1 game icon: {urls:?}");
    }

    #[test]
    fn friend_without_presence_is_offline() {
        let ids = vec!["9".to_string()];
        let profiles: HashMap<_, _> = [("9".to_string(), profile("ghost"))].into();
        let snap = build(&ids, &profiles, &HashMap::new(), &HashMap::new());
        assert_eq!(snap.friends[0].state, State::Offline);
        assert_eq!(snap.friends[0].last_online, None);
    }
}
