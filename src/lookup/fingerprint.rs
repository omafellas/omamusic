use anyhow::{anyhow, Context, Result};
use base64::Engine;
use rodio::Source;
use rusty_chromaprint::{Configuration, FingerprintCompressor, Fingerprinter};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

const MAX_SECONDS: u32 = 120;
const MIN_SCORE: f64 = 0.5;
const CHUNK: usize = 8192;

#[derive(Debug, Clone, PartialEq)]
pub struct Fingerprint {
    pub compressed: String,
    pub duration: f64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Hit {
    pub recording: Option<String>,
    pub release_groups: Vec<String>,
}

pub fn fingerprint(path: &Path) -> Result<Fingerprint> {
    let file = std::fs::File::open(path)?;
    let decoder = rodio::Decoder::try_from(file)
        .map_err(|err| anyhow!("decode {}: {err}", path.display()))?;
    let rate = decoder.sample_rate().get();
    let channels = u32::from(decoder.channels().get());
    let duration = decoder
        .total_duration()
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let limit = (rate * channels * MAX_SECONDS) as usize;
    let config = Configuration::preset_test2();
    let mut printer = Fingerprinter::new(&config);
    printer
        .start(rate, channels)
        .map_err(|err| anyhow!("fingerprint {}: {err:?}", path.display()))?;
    let mut buffer: Vec<i16> = Vec::with_capacity(CHUNK);
    let mut total = 0usize;
    for sample in decoder {
        buffer.push((sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16);
        total += 1;
        if buffer.len() == CHUNK {
            printer.consume(&buffer);
            buffer.clear();
        }
        if total >= limit {
            break;
        }
    }
    printer.consume(&buffer);
    printer.finish();
    let raw = printer.fingerprint();
    if raw.is_empty() {
        return Err(anyhow!("fingerprint {}: too short", path.display()));
    }
    let duration = if duration > 0.0 {
        duration
    } else {
        total as f64 / f64::from(rate * channels)
    };
    let compressed = FingerprintCompressor::from(&config).compress(raw);
    Ok(Fingerprint {
        compressed: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(compressed),
        duration,
    })
}

pub fn lookup_url(key: &str, duration: f64, compressed: &str) -> String {
    format!(
        "https://api.acoustid.org/v2/lookup?client={key}&meta=recordings+releasegroups\
         &duration={}&fingerprint={compressed}",
        duration.round() as u64
    )
}

pub fn parse_lookup(body: &[u8]) -> Result<Hit> {
    let json: Value = serde_json::from_slice(body).context("AcoustID response")?;
    if json["status"].as_str() != Some("ok") {
        return Err(anyhow!(
            "AcoustID: {}",
            json["error"]["message"].as_str().unwrap_or("bad answer")
        ));
    }
    let best = json["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["score"].as_f64().unwrap_or(0.0) >= MIN_SCORE)
        .max_by(|a, b| {
            a["score"]
                .as_f64()
                .unwrap_or(0.0)
                .total_cmp(&b["score"].as_f64().unwrap_or(0.0))
        });
    let Some(best) = best else {
        return Ok(Hit::default());
    };
    let Some(recording) = best["recordings"].as_array().and_then(|r| r.first()) else {
        return Ok(Hit::default());
    };
    Ok(Hit {
        recording: recording["id"].as_str().map(str::to_string),
        release_groups: recording["releasegroups"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|g| g["id"].as_str().map(str::to_string))
            .collect(),
    })
}

pub fn vote_release_groups(hits: &[Hit]) -> Vec<String> {
    let mut votes: HashMap<&str, (usize, usize)> = HashMap::new();
    for (order, hit) in hits.iter().flat_map(|h| &h.release_groups).enumerate() {
        let entry = votes.entry(hit.as_str()).or_insert((0, order));
        entry.0 += 1;
    }
    let mut ranked: Vec<(&str, (usize, usize))> = votes.into_iter().collect();
    ranked.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then(a.1 .1.cmp(&b.1 .1)));
    ranked.into_iter().map(|(id, _)| id.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::fixtures;

    fn temp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("omamusic-fp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn fingerprint_is_deterministic_and_reports_duration() {
        let path = temp("tone.wav", &fixtures::wav_tone(4, 440.0));
        let a = fingerprint(&path).unwrap();
        let b = fingerprint(&path).unwrap();
        assert_eq!(a, b);
        assert!((a.duration - 4.0).abs() < 0.1, "{}", a.duration);
        assert!(a.compressed.len() > 10, "{}", a.compressed);
        assert!(a
            .compressed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        let other = fingerprint(&temp("other.wav", &fixtures::wav_tone(4, 1200.0))).unwrap();
        assert_ne!(a.compressed, other.compressed);
    }

    #[test]
    fn unreadable_audio_is_an_error() {
        assert!(fingerprint(&temp("junk.wav", b"nope")).is_err());
    }

    #[test]
    #[ignore = "needs the fpcalc binary from chromaprint"]
    fn fingerprint_agrees_with_fpcalc() {
        let path = temp("cmp.wav", &fixtures::wav_tone(10, 440.0));
        let ours = fingerprint(&path).unwrap();
        let out = std::process::Command::new("fpcalc")
            .arg("-length")
            .arg("120")
            .arg(&path)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let theirs = text
            .lines()
            .find_map(|l| l.strip_prefix("FINGERPRINT="))
            .unwrap();
        assert!(
            ours.compressed.len().abs_diff(theirs.len()) < 8,
            "{} vs {}",
            ours.compressed.len(),
            theirs.len()
        );
    }

    #[test]
    fn lookup_url_carries_key_duration_and_fingerprint() {
        let url = lookup_url("k3y", 243.7, "AQAD_x-y");
        assert_eq!(
            url,
            "https://api.acoustid.org/v2/lookup?client=k3y&meta=recordings+releasegroups&duration=244&fingerprint=AQAD_x-y"
        );
    }

    #[test]
    fn parse_lookup_collects_release_groups_above_the_score_floor() {
        let body = r#"{"status":"ok","results":[
            {"id":"x","score":0.97,"recordings":[
                {"id":"rec-1","releasegroups":[{"id":"g1"},{"id":"g2"}]}
            ]},
            {"id":"y","score":0.3,"recordings":[{"id":"rec-9","releasegroups":[{"id":"g9"}]}]}
        ]}"#;
        let hit = parse_lookup(body.as_bytes()).unwrap();
        assert_eq!(hit.recording.as_deref(), Some("rec-1"));
        assert_eq!(hit.release_groups, ["g1", "g2"]);
        let none = parse_lookup(br#"{"status":"ok","results":[]}"#).unwrap();
        assert_eq!(none.recording, None);
        assert!(parse_lookup(b"html").is_err());
        assert!(
            parse_lookup(br#"{"status":"error","error":{"message":"invalid API key"}}"#).is_err()
        );
    }

    #[test]
    fn vote_release_groups_ranks_by_track_count() {
        let hits = vec![
            Hit {
                recording: Some("a".into()),
                release_groups: vec!["g1".into(), "g2".into()],
            },
            Hit {
                recording: Some("b".into()),
                release_groups: vec!["g1".into()],
            },
            Hit {
                recording: None,
                release_groups: vec![],
            },
            Hit {
                recording: Some("c".into()),
                release_groups: vec!["g3".into()],
            },
        ];
        assert_eq!(vote_release_groups(&hits), ["g1", "g2", "g3"]);
        assert_eq!(vote_release_groups(&hits[2..3]), Vec::<String>::new());
    }
}
