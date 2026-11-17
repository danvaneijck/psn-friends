//! The handful of PSN userProfile endpoints we need.

use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use reqwest::blocking::Client;
use reqwest::header::ACCEPT_LANGUAGE;
use reqwest::StatusCode;
use serde::de::DeserializeOwned;
use serde::Deserialize;

use crate::auth::NeedsLogin;
use crate::par::par_map;

const V1: &str = "https://m.np.playstation.com/api/userProfile/v1/internal/users";
const V2: &str = "https://m.np.playstation.com/api/userProfile/v2/internal/users";
const PRESENCE_BATCH: usize = 100;
/// The bulk profile endpoint's real limit is undocumented; 50 is conservative,
/// and a rejected batch falls back to one request per friend.
const PROFILE_BATCH: usize = 50;

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub online_id: String,
    #[serde(default)]
    pub avatars: Vec<Avatar>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Avatar {
    pub size: String,
    pub url: String,
}

impl Profile {
    /// Medium avatar is plenty for a 36px widget; fall back to whatever exists.
    pub fn avatar_url(&self) -> Option<&str> {
        ["m", "l", "xl", "s"]
            .iter()
            .find_map(|size| self.avatars.iter().find(|a| a.size == *size))
            .or_else(|| self.avatars.first())
            .map(|a| a.url.as_str())
    }
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct Presence {
    pub account_id: Option<String>,
    pub availability: Option<String>,
    pub last_available_date: Option<String>,
    pub primary_platform_info: Option<PlatformInfo>,
    #[serde(default)]
    pub game_title_info_list: Vec<GameTitleInfo>,
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct PlatformInfo {
    pub online_status: Option<String>,
    pub platform: Option<String>,
    pub last_online_date: Option<String>,
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct GameTitleInfo {
    pub np_title_id: Option<String>,
    pub title_name: Option<String>,
    pub format: Option<String>,
    pub launch_platform: Option<String>,
    pub np_title_icon_url: Option<String>,
    pub concept_icon_url: Option<String>,
    /// Rich presence text ("In a match", ...) when the game provides it.
    pub game_status: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BatchPresences {
    #[serde(default)]
    basic_presences: Vec<Presence>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SinglePresence {
    basic_presence: Presence,
}

#[derive(Deserialize)]
struct BulkProfiles {
    #[serde(default)]
    profiles: Vec<Profile>,
}

#[derive(Deserialize)]
struct FriendsList {
    #[serde(default)]
    friends: Vec<String>,
}

pub struct Api {
    client: Client,
    token: String,
}

impl Api {
    pub fn new(client: Client, access_token: String) -> Self {
        Self {
            client,
            token: access_token,
        }
    }

    fn get<T: DeserializeOwned>(&self, url: &str, query: &[(&str, &str)]) -> Result<T> {
        let resp = self
            .client
            .get(url)
            .bearer_auth(&self.token)
            .header(ACCEPT_LANGUAGE, "en-US")
            .query(query)
            .send()
            .with_context(|| format!("requesting {url}"))?;
        let status = resp.status();
        if status == StatusCode::UNAUTHORIZED {
            return Err(NeedsLogin(
                "PSN rejected the access token. Run `psn-friends login`.".into(),
            )
            .into());
        }
        if !status.is_success() {
            let body = resp.text().unwrap_or_default();
            bail!(HttpError {
                status,
                url: url.to_string(),
                body: truncate(&body, 300)
            });
        }
        resp.json()
            .with_context(|| format!("unexpected response from {url}"))
    }

    pub fn friend_ids(&self) -> Result<Vec<String>> {
        let list: FriendsList = self.get(&format!("{V1}/me/friends"), &[("limit", "1000")])?;
        Ok(list.friends)
    }

    /// Profiles for `ids`, in batches. Friends whose profile can't be fetched
    /// are absent from the result.
    pub fn profiles(&self, ids: &[String]) -> Result<Vec<(String, Profile)>> {
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(PROFILE_BATCH) {
            match self.profile_batch(chunk) {
                // The bulk response carries no account ids; it's in request order.
                Ok(list) if list.len() == chunk.len() => {
                    out.extend(chunk.iter().cloned().zip(list))
                }
                Err(e) if e.is::<NeedsLogin>() => return Err(e),
                _ => {
                    for (id, r) in par_map(chunk, 8, |id| (id.clone(), self.profile(id))) {
                        match r {
                            Ok(p) => out.push((id, p)),
                            Err(e) if e.is::<NeedsLogin>() => return Err(e),
                            Err(_) => {}
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    fn profile_batch(&self, ids: &[String]) -> Result<Vec<Profile>> {
        let joined = ids.join(",");
        let resp: BulkProfiles = self.get(&format!("{V1}/profiles"), &[("accountIds", &joined)])?;
        Ok(resp.profiles)
    }

    fn profile(&self, account_id: &str) -> Result<Profile> {
        self.get(&format!("{V1}/{account_id}/profiles"), &[])
    }

    /// Presence for many friends, keyed by account id. Friends who hide their
    /// presence are simply absent from the map.
    pub fn presences(&self, ids: &[String]) -> Result<HashMap<String, Presence>> {
        let mut out = HashMap::new();
        for chunk in ids.chunks(PRESENCE_BATCH) {
            match self.presence_batch(chunk) {
                Ok(list) => {
                    for p in list {
                        if let Some(id) = p.account_id.clone() {
                            out.insert(id, p);
                        }
                    }
                }
                Err(e) if e.is::<NeedsLogin>() => return Err(e),
                // Batch endpoint unavailable or picky: fall back to one call per friend.
                Err(_) => {
                    let results = par_map(chunk, 8, |id| (id.clone(), self.presence_one(id)));
                    for (id, r) in results {
                        match r {
                            Ok(p) => {
                                out.insert(id, p);
                            }
                            Err(e) if e.is::<NeedsLogin>() => return Err(e),
                            // 403/404: presence hidden by the friend's privacy settings.
                            Err(_) => {}
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    fn presence_batch(&self, ids: &[String]) -> Result<Vec<Presence>> {
        let joined = ids.join(",");
        let resp: BatchPresences = self.get(
            &format!("{V2}/basicPresences"),
            &[
                ("type", "primary"),
                ("platforms", "PS4,PS5,MOBILE_APP,PSPC"),
                ("withOwnGameTitleInfo", "true"),
                ("accountIds", &joined),
            ],
        )?;
        Ok(resp.basic_presences)
    }

    fn presence_one(&self, id: &str) -> Result<Presence> {
        let resp: SinglePresence = self.get(
            &format!("{V1}/{id}/basicPresences"),
            &[
                ("type", "primary"),
                ("platforms", "PS4,PS5,MOBILE_APP,PSPC"),
            ],
        )?;
        let mut p = resp.basic_presence;
        p.account_id.get_or_insert_with(|| id.to_string());
        Ok(p)
    }
}

#[derive(Debug)]
pub struct HttpError {
    pub status: StatusCode,
    pub url: String,
    pub body: String,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PSN returned HTTP {} for {}", self.status, self.url)?;
        if !self.body.is_empty() {
            write!(f, ": {}", self.body)?;
        }
        Ok(())
    }
}

impl std::error::Error for HttpError {}

fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_batch_presence_playing_and_offline() {
        let raw = r#"{
          "basicPresences": [
            {
              "accountId": "111",
              "availability": "availableToPlay",
              "primaryPlatformInfo": {"onlineStatus": "online", "platform": "PS5", "lastOnlineDate": "2026-11-17T09:00:00.000Z"},
              "gameTitleInfoList": [{
                "npTitleId": "PPSA01234_00", "titleName": "Some Game", "format": "PS5",
                "launchPlatform": "PS5", "conceptIconUrl": "https://image.api.playstation.com/a.png"
              }]
            },
            {
              "accountId": "222",
              "availability": "unavailable",
              "lastAvailableDate": "2026-11-16T22:10:00.000Z",
              "primaryPlatformInfo": {"onlineStatus": "offline", "platform": "ps4", "lastOnlineDate": "2026-11-16T22:10:00.000Z"}
            }
          ]
        }"#;
        let parsed: BatchPresences = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.basic_presences.len(), 2);
        let playing = &parsed.basic_presences[0];
        assert_eq!(
            playing.game_title_info_list[0].title_name.as_deref(),
            Some("Some Game")
        );
        let offline = &parsed.basic_presences[1];
        assert!(offline.game_title_info_list.is_empty());
        assert_eq!(
            offline
                .primary_platform_info
                .as_ref()
                .unwrap()
                .online_status
                .as_deref(),
            Some("offline")
        );
    }

    #[test]
    fn parses_single_presence_and_profile() {
        let raw = r#"{"basicPresence": {"availability": "unavailable",
            "primaryPlatformInfo": {"onlineStatus": "offline", "platform": "PS5", "lastOnlineDate": "2026-11-01T00:00:00Z"}}}"#;
        let parsed: SinglePresence = serde_json::from_str(raw).unwrap();
        assert!(parsed.basic_presence.account_id.is_none());

        let raw = r#"{"onlineId": "Hakoom", "aboutMe": "", "avatars": [
            {"size": "s", "url": "https://x/s.png"}, {"size": "m", "url": "https://x/m.png"}],
            "languages": ["en-US"], "isPlus": false, "isOfficiallyVerified": false, "isMe": false}"#;
        let p: Profile = serde_json::from_str(raw).unwrap();
        assert_eq!(p.online_id, "Hakoom");
        assert_eq!(p.avatar_url(), Some("https://x/m.png"));
    }

    #[test]
    fn parses_bulk_profiles() {
        let raw = r#"{"profiles": [
            {"onlineId": "first", "aboutMe": "", "avatars": [{"size": "l", "url": "https://x/l.png"}],
             "isPlus": true, "isOfficiallyVerified": false, "languages": [], "personalDetail": {}},
            {"onlineId": "second", "aboutMe": "", "avatars": []}]}"#;
        let parsed: BulkProfiles = serde_json::from_str(raw).unwrap();
        let names: Vec<_> = parsed
            .profiles
            .iter()
            .map(|p| p.online_id.as_str())
            .collect();
        assert_eq!(names, ["first", "second"]);
        assert_eq!(parsed.profiles[0].avatar_url(), Some("https://x/l.png"));
        assert_eq!(parsed.profiles[1].avatar_url(), None);
    }
}
