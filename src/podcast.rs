//! Podcasts: feeds fetched and parsed here, episodes downloaded beside the
//! server's state, played like any other file.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{header, Request};
use quick_xml::events::Event;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use tokio::io::AsyncWriteExt;

use tokio::sync::watch;

use super::fetch;
use elbert_subsonic_core::api::{Answer, Params};
use elbert_subsonic_core::auth::Who;
use elbert_subsonic_core::respond::{iso8601, ApiError, ERR_GENERIC};
use elbert_subsonic_core::state::{Channel, Episode};
use elbert_subsonic_core::{wall_ms, Server};

/// A feed bigger than this is not a feed.
const MAX_FEED: usize = 32 * 1024 * 1024;
const REFRESH_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
const KEEP_EPISODES: usize = 300;

fn hashed(prefix: &str, text: &str) -> String {
    let digest = Sha1::digest(text.as_bytes());
    let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("{prefix}{hex}")
}

fn episode_json(channel: &Channel, e: &Episode) -> Value {
    let mut value = json!({
        "id": &e.id,
        "channelId": &channel.id,
        "parent": &channel.id,
        "isDir": false,
        "title": &e.title,
        "album": &channel.title,
        "artist": &channel.title,
        "description": &e.description,
        "status": &e.status,
        "type": "podcast",
        "isVideo": false,
        "duration": e.duration_s,
        "size": e.size,
        "contentType": &e.mime,
    });
    if e.status == "completed" {
        value["streamId"] = e.id.clone().into();
    }
    if let Some(when) = e.published_ms {
        value["publishDate"] = iso8601(when).into();
    }
    if channel.image_url.is_some() {
        value["coverArt"] = channel.id.clone().into();
    }
    if let Some(error) = &e.error {
        value["errorMessage"] = error.clone().into();
    }
    value
}

fn channel_json(channel: &Channel, with_episodes: bool) -> Value {
    let mut value = json!({
        "id": &channel.id,
        "url": &channel.url,
        "title": &channel.title,
        "description": &channel.description,
        "status": &channel.status,
    });
    if let Some(image) = &channel.image_url {
        value["originalImageUrl"] = image.clone().into();
        value["coverArt"] = channel.id.clone().into();
    }
    if let Some(error) = &channel.error {
        value["errorMessage"] = error.clone().into();
    }
    if with_episodes {
        value["episode"] = channel
            .episodes
            .iter()
            .map(|e| episode_json(channel, e))
            .collect::<Vec<_>>()
            .into();
    }
    value
}

/// `getPodcasts`.
pub fn get_podcasts(srv: &Arc<Server>, p: &Params) -> Answer {
    let with_episodes = p.truthy("includeEpisodes").unwrap_or(true);
    let state = srv.state.lock().unwrap();
    let id = p.get("id");
    if let Some(id) = id {
        if !state.podcasts.iter().any(|c| c.id == id) {
            return Err(ApiError::not_found("Podcast channel"));
        }
    }
    let channels: Vec<Value> = state
        .podcasts
        .iter()
        .filter(|c| id.is_none_or(|i| c.id == i))
        .map(|c| channel_json(c, with_episodes))
        .collect();
    Ok(json!({ "podcasts": { "channel": channels } }))
}

/// `getNewestPodcasts`.
pub fn newest(srv: &Arc<Server>, p: &Params) -> Answer {
    let count = p.count("count", 20, 500);
    let state = srv.state.lock().unwrap();
    let mut all: Vec<(&Channel, &Episode)> = state
        .podcasts
        .iter()
        .flat_map(|c| c.episodes.iter().map(move |e| (c, e)))
        .collect();
    all.sort_by_key(|(_, e)| std::cmp::Reverse(e.published_ms.unwrap_or(0)));
    let list: Vec<Value> = all
        .into_iter()
        .take(count)
        .map(|(c, e)| episode_json(c, e))
        .collect();
    Ok(json!({ "newestPodcasts": { "episode": list } }))
}

/// `getPodcastEpisode`.
pub fn episode(srv: &Arc<Server>, p: &Params) -> Answer {
    let id = p.need("id")?;
    let state = srv.state.lock().unwrap();
    for channel in &state.podcasts {
        if let Some(e) = channel.episodes.iter().find(|e| e.id == id) {
            return Ok(json!({ "podcastEpisode": episode_json(channel, e) }));
        }
    }
    Err(ApiError::not_found("Podcast episode"))
}

/// The artwork URL for a channel, or an episode of it.
pub fn cover_url(srv: &Server, id: &str) -> Option<String> {
    let state = srv.state.lock().unwrap();
    state
        .podcasts
        .iter()
        .find(|c| c.id == id || c.episodes.iter().any(|e| e.id == id))
        .and_then(|c| c.image_url.clone())
}

/// The podcast calls, which core's `api::dispatch` hands to the app's `Link` (the `podcasts` feature).
pub async fn call(srv: &Arc<Server>, who: &Who, call: &str, p: &Params) -> Answer {
    match call {
        "getPodcasts" => get_podcasts(srv, p),
        "getNewestPodcasts" => newest(srv, p),
        "getPodcastEpisode" => episode(srv, p),
        "refreshPodcasts" => refresh(srv, who),
        "createPodcastChannel" => create_channel(srv, p, who).await,
        "deletePodcastChannel" => delete_channel(srv, p, who),
        "deletePodcastEpisode" => delete_episode(srv, p, who),
        "downloadPodcastEpisode" => download_episode(srv, p, who),
        _ => Err(ApiError(ERR_GENERIC, format!("No such call: {call}"))),
    }
}

/// `refreshPodcasts`.
pub fn refresh(srv: &Arc<Server>, who: &Who) -> Answer {
    who.require(
        who.account.podcast || who.account.admin,
        "Refreshing podcasts",
    )?;
    let server = srv.clone();
    tokio::spawn(async move { refresh_all(&server).await });
    Ok(json!({}))
}

async fn refresh_all(srv: &Arc<Server>) {
    let urls: Vec<String> = srv
        .state
        .lock()
        .unwrap()
        .podcasts
        .iter()
        .map(|c| c.url.clone())
        .collect();
    for url in urls {
        fetch_channel(srv, &url).await;
    }
}

/// Keeps feeds current while the server runs.
pub fn keep_fresh(srv: Arc<Server>, mut stop: watch::Receiver<bool>) {
    tokio::spawn(async move {
        loop {
            let oldest = srv
                .state
                .lock()
                .unwrap()
                .podcasts
                .iter()
                .map(|c| c.refreshed_ms)
                .min();
            let wait = match oldest {
                Some(when) if wall_ms() - when >= REFRESH_EVERY.as_millis() as i64 => {
                    Duration::ZERO
                }
                _ => REFRESH_EVERY,
            };
            if wait.is_zero() {
                refresh_all(&srv).await;
            }
            tokio::select! {
                _ = tokio::time::sleep(if wait.is_zero() { REFRESH_EVERY } else { wait }) => {}
                _ = stop.changed() => return,
            }
        }
    });
}

/// `createPodcastChannel`.
pub async fn create_channel(srv: &Arc<Server>, p: &Params, who: &Who) -> Answer {
    who.require(who.account.podcast || who.account.admin, "Adding podcasts")?;
    let url = p.need("url")?;
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(ApiError(
            ERR_GENERIC,
            "A feed's address must be an http(s) URL".into(),
        ));
    }
    let id = hashed("pc-", url);
    {
        let mut state = srv.state.lock().unwrap();
        if !state.podcasts.iter().any(|c| c.id == id) {
            state.podcasts.push(Channel {
                id,
                url: url.to_string(),
                status: "downloading".into(),
                ..Default::default()
            });
        }
    }
    srv.save_soon();
    fetch_channel(srv, url).await;
    Ok(json!({}))
}

/// `deletePodcastChannel`.
pub fn delete_channel(srv: &Arc<Server>, p: &Params, who: &Who) -> Answer {
    who.require(
        who.account.podcast || who.account.admin,
        "Removing podcasts",
    )?;
    let id = p.need("id")?;
    let removed = {
        let mut state = srv.state.lock().unwrap();
        let at = state
            .podcasts
            .iter()
            .position(|c| c.id == id)
            .ok_or_else(|| ApiError::not_found("Podcast channel"))?;
        state.podcasts.remove(at)
    };
    for e in &removed.episodes {
        if let Some(file) = &e.file {
            let _ = std::fs::remove_file(file);
        }
    }
    srv.save_soon();
    Ok(json!({}))
}

/// `deletePodcastEpisode`.
pub fn delete_episode(srv: &Arc<Server>, p: &Params, who: &Who) -> Answer {
    who.require(
        who.account.podcast || who.account.admin,
        "Removing podcasts",
    )?;
    let id = p.need("id")?;
    let file = {
        let mut state = srv.state.lock().unwrap();
        let e = state
            .podcasts
            .iter_mut()
            .flat_map(|c| c.episodes.iter_mut())
            .find(|e| e.id == id)
            .ok_or_else(|| ApiError::not_found("Podcast episode"))?;
        e.status = "deleted".into();
        e.file.take()
    };
    if let Some(file) = file {
        let _ = std::fs::remove_file(file);
    }
    srv.save_soon();
    Ok(json!({}))
}

/// `downloadPodcastEpisode`.
pub fn download_episode(srv: &Arc<Server>, p: &Params, who: &Who) -> Answer {
    who.require(
        who.account.podcast || who.account.admin,
        "Downloading podcasts",
    )?;
    let id = p.need("id")?.to_string();
    let url = {
        let mut state = srv.state.lock().unwrap();
        let e = state
            .podcasts
            .iter_mut()
            .flat_map(|c| c.episodes.iter_mut())
            .find(|e| e.id == id)
            .ok_or_else(|| ApiError::not_found("Podcast episode"))?;
        e.status = "downloading".into();
        e.error = None;
        e.url.clone()
    };
    let server = srv.clone();
    tokio::spawn(async move {
        let result = download(&server, &id, &url).await;
        {
            let mut state = server.state.lock().unwrap();
            if let Some(e) = state
                .podcasts
                .iter_mut()
                .flat_map(|c| c.episodes.iter_mut())
                .find(|e| e.id == id)
            {
                match result {
                    Ok((file, size)) => {
                        e.status = "completed".into();
                        e.file = Some(file);
                        e.size = size;
                    }
                    Err(error) => {
                        e.status = "error".into();
                        e.error = Some(error);
                    }
                }
            }
        }
        server.save_soon();
    });
    Ok(json!({}))
}

async fn download(srv: &Arc<Server>, id: &str, url: &str) -> Result<(String, u64), String> {
    let dir = srv
        .link
        .data_dir()
        .ok_or("no folder to keep episodes in")?
        .join("podcasts");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| e.to_string())?;
    let response = get(url).await?;
    let ext = url
        .split('?')
        .next()
        .and_then(|u| u.rsplit('.').next())
        .filter(|e| e.len() <= 4 && e.chars().all(char::is_alphanumeric))
        .unwrap_or("mp3")
        .to_string();
    let path = dir.join(format!("{id}.{ext}"));
    let tmp = dir.join(format!("{id}.part"));
    let mut file = tokio::fs::File::create(&tmp)
        .await
        .map_err(|e| e.to_string())?;
    let mut body = response.into_body();
    let mut size = 0u64;
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| e.to_string())?;
        if let Some(data) = frame.data_ref() {
            size += data.len() as u64;
            file.write_all(data).await.map_err(|e| e.to_string())?;
        }
    }
    file.flush().await.map_err(|e| e.to_string())?;
    drop(file);
    tokio::fs::rename(&tmp, &path)
        .await
        .map_err(|e| e.to_string())?;
    Ok((path.to_string_lossy().into_owned(), size))
}

async fn get(url: &str) -> Result<hyper::Response<hyper::body::Incoming>, String> {
    let response = fetch::send_following(url, |u| {
        Request::get(u)
            .header(header::USER_AGENT, "Elbert")
            .body(Full::new(Bytes::new()))
            .map_err(|e| e.to_string())
    })
    .await?;
    if !response.status().is_success() {
        return Err(format!("the server answered {}", response.status()));
    }
    Ok(response)
}

async fn fetch_channel(srv: &Arc<Server>, url: &str) {
    let parsed = async {
        let response = get(url).await?;
        let collected = http_body_util::Limited::new(response.into_body(), MAX_FEED)
            .collect()
            .await
            .map_err(|e| e.to_string())?;
        parse_feed(&collected.to_bytes())
    }
    .await;
    {
        let mut state = srv.state.lock().unwrap();
        if let Some(channel) = state.podcasts.iter_mut().find(|c| c.url == url) {
            channel.refreshed_ms = wall_ms();
            match parsed {
                Ok(feed) => merge(channel, feed),
                Err(error) => {
                    channel.status = "error".into();
                    channel.error = Some(error);
                }
            }
        }
    }
    srv.save_soon();
}

/// New episodes come in as `new`; what a known one's status and file are stays.
fn merge(channel: &mut Channel, feed: Feed) {
    channel.title = feed.title;
    channel.description = feed.description;
    channel.image_url = feed.image;
    channel.status = "completed".into();
    channel.error = None;
    let mut episodes: Vec<Episode> = Vec::with_capacity(feed.episodes.len());
    for mut e in feed.episodes {
        e.id = hashed("pe-", &format!("{}\0{}", channel.id, e.guid));
        if let Some(known) = channel.episodes.iter().find(|k| k.id == e.id) {
            e.status = known.status.clone();
            e.file = known.file.clone();
            e.error = known.error.clone();
            if known.size > 0 && known.file.is_some() {
                e.size = known.size;
            }
        } else {
            e.status = "new".into();
        }
        episodes.push(e);
    }
    episodes.sort_by_key(|e| std::cmp::Reverse(e.published_ms.unwrap_or(0)));
    episodes.truncate(KEEP_EPISODES);
    // A downloaded episode that fell off the feed is still on disk.
    for old in &channel.episodes {
        if old.file.is_some() && !episodes.iter().any(|e| e.id == old.id) {
            episodes.push(old.clone());
        }
    }
    channel.episodes = episodes;
}

#[derive(Default)]
struct Feed {
    title: String,
    description: String,
    image: Option<String>,
    episodes: Vec<Episode>,
}

fn parse_feed(bytes: &[u8]) -> Result<Feed, String> {
    let mut reader = quick_xml::Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);
    let mut feed = Feed::default();
    let mut path: Vec<String> = Vec::new();
    let mut item: Option<Episode> = None;
    let mut saw_channel = false;
    let mut buf = Vec::new();
    loop {
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|e| format!("that isn't a feed: {e}"))?;
        match &event {
            Event::Start(tag) | Event::Empty(tag) => {
                let name = String::from_utf8_lossy(tag.name().as_ref()).into_owned();
                let attr = |key: &[u8]| {
                    tag.attributes()
                        .flatten()
                        .find(|a| a.key.as_ref() == key)
                        .and_then(|a| a.unescape_value().ok())
                        .map(|v| v.into_owned())
                };
                saw_channel |= name == "channel";
                match name.as_str() {
                    "item" => item = Some(Episode::default()),
                    "enclosure" => {
                        if let Some(e) = &mut item {
                            e.url = attr(b"url").unwrap_or_default();
                            e.mime = attr(b"type").unwrap_or_else(|| "audio/mpeg".into());
                            e.size = attr(b"length").and_then(|l| l.parse().ok()).unwrap_or(0);
                        }
                    }
                    "itunes:image" if item.is_none() => {
                        if let Some(href) = attr(b"href") {
                            feed.image = Some(href);
                        }
                    }
                    _ => {}
                }
                if matches!(event, Event::Start(_)) {
                    path.push(name);
                }
            }
            Event::End(tag) => {
                if tag.name().as_ref() == b"item" {
                    if let Some(mut e) = item.take() {
                        if !e.url.is_empty() {
                            if e.guid.is_empty() {
                                e.guid = e.url.clone();
                            }
                            feed.episodes.push(e);
                        }
                    }
                }
                path.pop();
            }
            Event::Text(_) | Event::CData(_) => {
                let text = match &event {
                    Event::Text(t) => t.unescape().map(|c| c.into_owned()).unwrap_or_default(),
                    Event::CData(c) => String::from_utf8_lossy(c.as_ref()).into_owned(),
                    _ => unreachable!(),
                };
                let field = path.last().map(String::as_str).unwrap_or("");
                let parent = path
                    .len()
                    .checked_sub(2)
                    .map(|i| path[i].as_str())
                    .unwrap_or("");
                if let Some(e) = &mut item {
                    match field {
                        "title" => e.title.push_str(&text),
                        "guid" => e.guid.push_str(&text),
                        "description" | "itunes:summary" if e.description.is_empty() => {
                            e.description.push_str(&text)
                        }
                        "pubDate" => e.published_ms = parse_rfc2822(&text),
                        "itunes:duration" => e.duration_s = parse_duration(&text),
                        _ => {}
                    }
                } else if parent == "channel" {
                    match field {
                        "title" => feed.title.push_str(&text),
                        "description" => feed.description.push_str(&text),
                        _ => {}
                    }
                } else if parent == "image" && field == "url" && feed.image.is_none() {
                    feed.image = Some(text);
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    if !saw_channel {
        return Err("that isn't a podcast feed".into());
    }
    Ok(feed)
}

/// `1:02:03`, `02:03` or plain seconds.
fn parse_duration(text: &str) -> u64 {
    text.trim().split(':').fold(0u64, |total, part| {
        total * 60 + part.trim().parse::<u64>().unwrap_or(0)
    })
}

/// `Wed, 02 Oct 2024 13:00:00 +0000`, as a feed writes a date.
fn parse_rfc2822(text: &str) -> Option<i64> {
    let text = text.split_once(',').map(|(_, rest)| rest).unwrap_or(text);
    let mut parts = text.split_whitespace();
    let day: i64 = parts.next()?.parse().ok()?;
    let month = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ]
    .iter()
    .position(|m| {
        parts_month_eq(
            m,
            &text
                .split_whitespace()
                .nth(1)
                .unwrap_or("")
                .to_ascii_lowercase(),
        )
    })? as i64
        + 1;
    parts.next()?;
    let year: i64 = parts.next()?.parse().ok()?;
    let mut clock = parts.next()?.split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    let second: i64 = clock.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let zone = parts.next().unwrap_or("+0000");
    let offset = match zone.as_bytes().first() {
        Some(b'+') | Some(b'-') if zone.len() == 5 => {
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let hours: i64 = zone[1..3].parse().ok()?;
            let minutes: i64 = zone[3..5].parse().ok()?;
            sign * (hours * 3600 + minutes * 60)
        }
        _ => 0,
    };
    // Howard Hinnant's days_from_civil.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + hour * 3600 + minute * 60 + second - offset) * 1000)
}

fn parts_month_eq(month: &str, text: &str) -> bool {
    text.starts_with(month)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_feed_parses() {
        let feed = parse_feed(
            br#"<?xml version="1.0"?><rss xmlns:itunes="x"><channel>
            <title>Show &amp; Tell</title><description><![CDATA[About <b>it</b>]]></description>
            <image><url>http://x/cover.jpg</url></image>
            <item><title>One</title><guid>g1</guid><pubDate>Wed, 02 Oct 2024 13:00:00 +0000</pubDate>
            <itunes:duration>1:02:03</itunes:duration>
            <enclosure url="http://x/1.mp3" length="123" type="audio/mpeg"/></item>
            <item><title>No audio</title></item>
            </channel></rss>"#,
        )
        .unwrap();
        assert_eq!(feed.title, "Show & Tell");
        assert_eq!(feed.description, "About <b>it</b>");
        assert_eq!(feed.image.as_deref(), Some("http://x/cover.jpg"));
        assert_eq!(feed.episodes.len(), 1);
        let e = &feed.episodes[0];
        assert_eq!(
            (e.title.as_str(), e.guid.as_str(), e.size, e.duration_s),
            ("One", "g1", 123, 3723)
        );
        assert_eq!(iso8601(e.published_ms.unwrap()), "2024-10-02T13:00:00.000Z");
        assert!(parse_feed(b"<html><body>nope</body></html>").is_err());
    }
}
