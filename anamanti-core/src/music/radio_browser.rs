//! Station lookup against the free, keyless **Radio Browser** directory
//! (<https://api.radio-browser.info>) — the "Look up" button on the config page's
//! Radio stations card. Given a stream URL and/or a name it returns candidate stations
//! (name, logo, homepage…) for the user to pick from; picking one fills the station's
//! name + logo URL, which stay editable (the override).
//!
//! Lookup order (results merged, de-duplicated by station uuid, best first):
//! 1. exact stream URL (`/json/stations/byurl`);
//! 2. the typed name, if any;
//! 3. a call sign / slug derived from the URL's last path segment
//!    (`…/CJDVFM-MP3` → `CJDV`), since the directory often lists the same station
//!    under a different host (`live.` vs `corus.leanstream.co`).
//!
//! Only the stream URL / name typed into the page leave the house (to Radio Browser).

use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// The round-robin DNS name every Radio Browser mirror answers under.
pub const DEFAULT_BASE: &str = "https://all.api.radio-browser.info";
const TIMEOUT: Duration = Duration::from_secs(8);
const MAX_RESULTS: usize = 8;
const COMPONENT: &str = "radio-browser";
/// Score of an exact stream-URL match (outranks slug match 2 + logo 1).
const EXACT: i32 = 4;

/// One directory hit, as the config page renders it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct StationMatch {
    pub name: String,
    /// The directory's (resolved) stream URL.
    pub stream_url: String,
    /// Station logo URL (Radio Browser's `favicon`), or empty.
    pub logo_url: String,
    pub homepage: String,
    pub country: String,
    pub state: String,
    pub codec: String,
    pub bitrate: u32,
    /// A suggested station id: the call sign when one can be found, else a slug.
    pub suggested_id: String,
    #[serde(skip)]
    uuid: String,
    #[serde(skip)]
    score: i32,
}

/// The subset of a Radio Browser station record we use.
#[derive(Debug, Deserialize)]
struct RbStation {
    #[serde(default)]
    stationuuid: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    url_resolved: String,
    #[serde(default)]
    favicon: String,
    #[serde(default)]
    homepage: String,
    #[serde(default)]
    countrycode: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    codec: String,
    #[serde(default)]
    bitrate: u32,
}

/// Look up `stream_url` / `name` on the public directory.
pub async fn lookup(stream_url: &str, name: &str) -> Result<Vec<StationMatch>> {
    lookup_at(DEFAULT_BASE, stream_url, name).await
}

/// [`lookup`] against an explicit API base (tests).
pub async fn lookup_at(base: &str, stream_url: &str, name: &str) -> Result<Vec<StationMatch>> {
    let stream_url = stream_url.trim();
    let name = name.trim();
    let slug = url_slug(stream_url);
    let call = slug.as_deref().and_then(call_sign);

    let mut queries: Vec<(&str, Vec<(&str, String)>)> = Vec::new();
    if !stream_url.is_empty() {
        queries.push((
            "/json/stations/byurl",
            vec![("url", stream_url.to_string())],
        ));
    }
    let mut names: Vec<String> = Vec::new();
    for n in [Some(name.to_string()), call.clone(), slug.clone()]
        .into_iter()
        .flatten()
        .filter(|n| n.len() >= 3)
    {
        if !names.iter().any(|m| m.eq_ignore_ascii_case(&n)) {
            names.push(n);
        }
    }
    for n in &names {
        queries.push((
            "/json/stations/search",
            vec![
                ("name", n.clone()),
                ("limit", "10".into()),
                ("order", "votes".into()),
                ("reverse", "true".into()),
                ("hidebroken", "true".into()),
            ],
        ));
    }
    anyhow::ensure!(
        !queries.is_empty(),
        "enter a stream URL or a station name to look up"
    );

    let mut out: Vec<StationMatch> = Vec::new();
    let mut last_err = None;
    for (path, params) in queries {
        let req = crate::http::get(format!("{base}{path}"), COMPONENT).query(&params);
        match crate::http::send_json::<Vec<RbStation>>(req, TIMEOUT, "Radio Browser").await {
            Ok(hits) => {
                for hit in hits {
                    if out.iter().any(|m| m.uuid == hit.stationuuid) {
                        continue;
                    }
                    out.push(to_match(hit, stream_url, slug.as_deref(), call.as_deref()));
                }
            }
            Err(e) => last_err = Some(e),
        }
        // Stop early once an exact URL match is in hand.
        if out.iter().any(|m| m.score >= EXACT) {
            break;
        }
    }
    if out.is_empty() {
        if let Some(e) = last_err {
            return Err(e);
        }
    }
    out.sort_by_key(|m| std::cmp::Reverse(m.score)); // stable: keeps query order on ties
    out.truncate(MAX_RESULTS);
    Ok(out)
}

fn to_match(
    s: RbStation,
    stream_url: &str,
    slug: Option<&str>,
    call: Option<&str>,
) -> StationMatch {
    let stream = if s.url_resolved.is_empty() {
        s.url.clone()
    } else {
        s.url_resolved.clone()
    };
    let mut score = 0;
    if !stream_url.is_empty()
        && (s.url.eq_ignore_ascii_case(stream_url)
            || s.url_resolved.eq_ignore_ascii_case(stream_url))
    {
        score += EXACT;
    } else if let Some(slug) = slug {
        if [&s.url, &s.url_resolved]
            .iter()
            .any(|u| url_slug(u).is_some_and(|x| x.eq_ignore_ascii_case(slug)))
        {
            score += 2;
        }
    }
    if !s.favicon.trim().is_empty() {
        score += 1;
    }
    let suggested_id = call
        .map(str::to_string)
        .or_else(|| s.name.split_whitespace().find_map(call_sign))
        .unwrap_or_else(|| {
            s.name
                .chars()
                .filter(char::is_ascii_alphanumeric)
                .take(8)
                .collect::<String>()
                .to_uppercase()
        });
    StationMatch {
        name: s.name.trim().to_string(),
        stream_url: stream,
        logo_url: s.favicon.trim().to_string(),
        homepage: s.homepage.trim().to_string(),
        country: s.countrycode,
        state: s.state,
        codec: s.codec,
        bitrate: s.bitrate,
        suggested_id,
        uuid: s.stationuuid,
        score,
    }
}

/// Codec/format tokens that are never part of a station's name in a stream path.
const NOISE: &[&str] = &[
    "MP3", "AAC", "AACP", "HE", "OGG", "OPUS", "FLAC", "SC", "HLS", "STREAM", "LIVE", "HI", "LO",
    "LOW", "HIGH", "WEB", "M3U8", "PLS", "M3U", "ICY", "MOBILE",
];

/// The meaningful part of a stream URL's last path segment: `…/CJDVFM-MP3` → `CJDVFM`,
/// `…/stream/kexp-160.aac` → `KEXP`. `None` for an unparseable/empty URL.
fn url_slug(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let seg = parsed.path_segments()?.rev().find(|s| !s.is_empty())?;
    let seg = seg.split('.').next().unwrap_or(seg); // drop an extension
    let tokens: Vec<String> = seg
        .split(['-', '_', '.'])
        .map(str::to_uppercase)
        .filter(|t| {
            !t.is_empty() && !NOISE.contains(&t.as_str()) && !t.chars().all(|c| c.is_ascii_digit())
        })
        .collect();
    let slug = tokens.join(" ");
    (!slug.is_empty()).then_some(slug)
}

/// A North American call sign in `token` (`CJDVFM` / `CJDV` / `KEXP-FM` → `CJDV` /
/// `CJDV` / `KEXP`): 3–4 letters starting C/K/W/X, optionally suffixed FM/AM/TV.
fn call_sign(token: &str) -> Option<String> {
    let t: String = token
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_uppercase();
    let core = ["FM", "AM", "TV"]
        .iter()
        .find_map(|suf| t.strip_suffix(suf).filter(|c| c.len() >= 3))
        .unwrap_or(&t);
    let ok = (3..=4).contains(&core.len())
        && core.chars().all(|c| c.is_ascii_uppercase())
        && matches!(core.chars().next(), Some('C' | 'K' | 'W' | 'X'));
    ok.then(|| core.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::test_support::serve_sequence;
    use serde_json::json;

    #[test]
    fn slugs_and_call_signs_come_from_the_stream_path() {
        assert_eq!(
            url_slug("https://corus.leanstream.co/CJDVFM-MP3").as_deref(),
            Some("CJDVFM")
        );
        assert_eq!(
            url_slug("https://x.org/stream/kexp-160.aac").as_deref(),
            Some("KEXP")
        );
        assert_eq!(url_slug("not a url"), None);
        assert_eq!(call_sign("CJDVFM").as_deref(), Some("CJDV"));
        assert_eq!(call_sign("KEXP-FM").as_deref(), Some("KEXP"));
        assert_eq!(call_sign("CBC"), Some("CBC".into()));
        assert_eq!(call_sign("Dave"), None);
        assert_eq!(call_sign("ROCK"), None); // doesn't start C/K/W/X
    }

    fn rb(uuid: &str, name: &str, url: &str, favicon: &str) -> serde_json::Value {
        json!({
            "stationuuid": uuid, "name": name, "url": url, "url_resolved": url,
            "favicon": favicon, "homepage": "", "countrycode": "CA", "state": "Ontario",
            "codec": "MP3", "bitrate": 128
        })
    }

    #[tokio::test]
    async fn falls_back_to_the_call_sign_and_ranks_slug_matches_first() {
        // 1: byurl → nothing; 2: search "CJDV" → two hits (the second shares the slug);
        // 3: search "CJDVFM" → a duplicate of hit 2 (de-duplicated).
        let (base, _h) = serve_sequence(vec![
            "[]".into(),
            json!([
                rb("a", "Some Other CJDV Thing", "http://other/x", ""),
                rb(
                    "b",
                    "CJDV 107.5 \"Dave Rocks\" Kitchener, ON",
                    "http://live.leanstream.co/CJDVFM-MP3",
                    "https://logo/dave.png"
                ),
            ])
            .to_string(),
            json!([rb("b", "dup", "http://live.leanstream.co/CJDVFM-MP3", "")]).to_string(),
        ]);
        let hits = lookup_at(&base, "https://corus.leanstream.co/CJDVFM-MP3", "")
            .await
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].logo_url, "https://logo/dave.png");
        assert_eq!(hits[0].suggested_id, "CJDV");
        assert_eq!(hits[0].country, "CA");
        assert!(hits[0].name.starts_with("CJDV 107.5"));
    }

    #[tokio::test]
    async fn an_exact_url_hit_stops_the_search() {
        let url = "https://corus.leanstream.co/CJDVFM-MP3";
        let (base, _h) = serve_sequence(vec![json!([rb("x", "Dave", url, "")]).to_string()]);
        let hits = lookup_at(&base, url, "Dave Rocks").await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].stream_url, url);
    }

    #[tokio::test]
    async fn needs_a_url_or_a_name() {
        let err = lookup_at("http://127.0.0.1:1", "", "").await.unwrap_err();
        assert!(err.to_string().contains("enter a stream URL"));
    }
}
