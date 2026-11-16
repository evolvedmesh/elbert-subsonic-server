//! Sonic similarity: songs compared by how they sound. A print is how an analyser hears a
//! file; prints are kept in the server's state, since making one means decoding the file.
//! What a print is made of is the embedding app's business ([`SonicIndex`]): Elbert's comes
//! from the aurora's analyser and its blend profiles, and a host without one has no sonic
//! call group (`Capabilities::sonic`).

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use elbert_subsonic_core::api::{Answer, Params};
use elbert_subsonic_core::browse::{Shuffler, View};
use elbert_subsonic_core::catalog::Song;
use elbert_subsonic_core::respond::ApiError;
use elbert_subsonic_core::state::SonicPrint;
use elbert_subsonic_core::Server;

/// How many numbers make a print.
pub const FEATURES: usize = 7;
/// How long a request may spend hearing songs it has no print of.
const BUDGET: Duration = Duration::from_secs(6);

/// Hears a file. Implemented by the app that embeds the server.
pub trait SonicIndex: Send + Sync + 'static {
    /// [FEATURES] numbers, each roughly 0 to 1 and none able to outvote the rest, that put
    /// songs that sound alike close together; `None` when the file can't be heard. Decodes
    /// the file, so it blocks.
    fn print(&self, path: &Path) -> Option<Vec<f32>>;
}

fn known(srv: &Server, song: &Song) -> Option<Vec<f32>> {
    let state = srv.state.lock().unwrap();
    let print = state.sonic.get(&*song.id)?;
    (print.len == song.size && print.features.len() == FEATURES).then(|| print.features.clone())
}

/// Hears `song` if it hasn't yet.
async fn print_of(srv: &Arc<Server>, index: &Arc<dyn SonicIndex>, song: &Song) -> Option<Vec<f32>> {
    if let Some(features) = known(srv, song) {
        return Some(features);
    }
    let path = song.path.clone();
    let index = index.clone();
    let features = tokio::task::spawn_blocking(move || index.print(&path))
        .await
        .ok()??;
    srv.state.lock().unwrap().sonic.insert(
        song.id.to_string(),
        SonicPrint {
            len: song.size,
            features: features.clone(),
        },
    );
    srv.save_soon();
    Some(features)
}

/// 1 for the same sound, toward 0 the further apart.
fn similarity(a: &[f32], b: &[f32]) -> f64 {
    let d: f64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| ((x - y) as f64).powi(2))
        .sum::<f64>()
        .sqrt();
    (1.0 - d / (FEATURES as f64).sqrt()).clamp(0.0, 1.0)
}

/// Prints for as many candidates as the time allows: already-heard ones first,
/// then a random sample, so repeated asking covers the whole library.
async fn prints<'a>(
    srv: &Arc<Server>,
    index: &Arc<dyn SonicIndex>,
    view: &'a View<'_>,
    skip: &[&str],
) -> Vec<(&'a Song, Vec<f32>)> {
    let mut heard = Vec::new();
    let mut unheard: Vec<&Song> = Vec::new();
    for song in &view.cat.songs {
        if !view.song_visible(song) || skip.contains(&&*song.id) {
            continue;
        }
        match known(srv, song) {
            Some(features) => heard.push((song, features)),
            None => unheard.push(song),
        }
    }
    let wanted = unheard.len().min(64);
    Shuffler::new().sample(&mut unheard, wanted);
    let started = Instant::now();
    for song in unheard.into_iter().take(wanted) {
        if started.elapsed() > BUDGET {
            break;
        }
        if let Some(features) = print_of(srv, index, song).await {
            heard.push((song, features));
        }
    }
    heard
}

fn match_json(view: &View, song: &Song, score: f64) -> Value {
    json!({ "entry": view.song_json(song), "similarity": score })
}

/// `getSonicSimilarTracks`, `getSimilarSongs` and `getSimilarSongs2`: the songs that sound most like the one asked about.
pub async fn similar(
    srv: &Arc<Server>,
    index: &Arc<dyn SonicIndex>,
    view: &View<'_>,
    p: &Params,
) -> Answer {
    let id = p.need("id")?;
    let seed = view
        .visible_song(id)
        .ok_or_else(|| ApiError::not_found("Song"))?;
    let count = p.count("count", 10, 500);
    let Some(print) = print_of(srv, index, seed).await else {
        return Ok(json!({ "sonicMatch": [] }));
    };
    let mut scored: Vec<(&Song, f64)> = prints(srv, index, view, &[id])
        .await
        .into_iter()
        .map(|(s, f)| (s, similarity(&print, &f)))
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    let matches: Vec<Value> = scored
        .into_iter()
        .take(count)
        .map(|(s, score)| match_json(view, s, score))
        .collect();
    Ok(json!({ "sonicMatch": matches }))
}

/// A walk from one song to the other: each step the nearest song to where a
/// straight line says this step should be, used once.
pub async fn path(
    srv: &Arc<Server>,
    index: &Arc<dyn SonicIndex>,
    view: &View<'_>,
    p: &Params,
) -> Answer {
    let (start_id, end_id) = (p.need("startSongId")?, p.need("endSongId")?);
    let start = view
        .visible_song(start_id)
        .ok_or_else(|| ApiError::not_found("Song"))?;
    let end = view
        .visible_song(end_id)
        .ok_or_else(|| ApiError::not_found("Song"))?;
    let count = p.count("count", 25, 500).max(2);
    let (Some(from), Some(to)) = (
        print_of(srv, index, start).await,
        print_of(srv, index, end).await,
    ) else {
        return Ok(json!({ "sonicMatch": [] }));
    };
    let mut pool = prints(srv, index, view, &[start_id, end_id]).await;
    let mut matches = vec![match_json(view, start, 1.0)];
    let steps = (count - 2).min(pool.len());
    for step in 1..=steps {
        let t = step as f32 / (steps + 1) as f32;
        let aim: Vec<f32> = from.iter().zip(&to).map(|(a, b)| a + (b - a) * t).collect();
        let mut best: Option<(usize, f64)> = None;
        for (i, (_, features)) in pool.iter().enumerate() {
            let score = similarity(&aim, features);
            if best.is_none_or(|(_, top)| score > top) {
                best = Some((i, score));
            }
        }
        let Some((at, score)) = best else { break };
        let (song, _) = pool.swap_remove(at);
        matches.push(match_json(view, song, score));
    }
    matches.push(match_json(view, end, similarity(&from, &to)));
    Ok(json!({ "sonicMatch": matches }))
}
