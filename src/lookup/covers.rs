use crate::lookup::http::{Http, Throttle};
use crate::lookup::text::same_name;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::Value;

const MIN_SIDE: u32 = 300;
const MAX_ASPECT_DEVIATION: f64 = 0.10;
const JPEG_QUALITY: u8 = 90;

pub fn caa_release_url(release: &str) -> String {
    format!("https://coverartarchive.org/release/{release}/front-500")
}

pub fn caa_release_group_url(release_group: &str) -> String {
    format!("https://coverartarchive.org/release-group/{release_group}/front-500")
}

pub fn itunes_search_url(artist: &str, album: &str) -> String {
    let term = format!("{artist} {album}");
    format!(
        "https://itunes.apple.com/search?entity=album&limit=5&term={}",
        utf8_percent_encode(&term, NON_ALPHANUMERIC)
    )
}

pub fn itunes_pick(body: &[u8], artist: &str, album: &str) -> Option<String> {
    let json: Value = serde_json::from_slice(body).ok()?;
    json["results"].as_array()?.iter().find_map(|result| {
        let close = names_match(
            result["artistName"].as_str()?,
            result["collectionName"].as_str()?,
            artist,
            album,
        );
        close.then(|| {
            result["artworkUrl100"]
                .as_str()
                .map(|url| url.replace("100x100bb", "1200x1200bb"))
        })?
    })
}

pub fn deezer_search_url(artist: &str, album: &str) -> String {
    let query = format!("artist:\"{artist}\" album:\"{album}\"");
    format!(
        "https://api.deezer.com/search/album?q={}",
        utf8_percent_encode(&query, NON_ALPHANUMERIC)
    )
}

pub fn deezer_pick(body: &[u8], artist: &str, album: &str) -> Option<String> {
    let json: Value = serde_json::from_slice(body).ok()?;
    json["data"].as_array()?.iter().find_map(|result| {
        let close = names_match(
            result["artist"]["name"].as_str()?,
            result["title"].as_str()?,
            artist,
            album,
        );
        close.then(|| result["cover_xl"].as_str().map(str::to_string))?
    })
}

fn names_match(found_artist: &str, found_album: &str, artist: &str, album: &str) -> bool {
    same_name(found_artist, artist) && same_name(found_album, album)
}

pub fn validate_image(bytes: &[u8]) -> Option<Vec<u8>> {
    let format = image::guess_format(bytes).ok()?;
    let decoded = image::load_from_memory(bytes).ok()?;
    let (w, h) = (decoded.width(), decoded.height());
    if w.min(h) < MIN_SIDE {
        return None;
    }
    let deviation = (f64::from(w) - f64::from(h)).abs() / f64::from(w.max(h));
    if deviation > MAX_ASPECT_DEVIATION {
        return None;
    }
    if format == image::ImageFormat::Jpeg {
        return Some(bytes.to_vec());
    }
    let mut out = std::io::Cursor::new(Vec::new());
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY);
    decoded.to_rgb8().write_with_encoder(encoder).ok()?;
    Some(out.into_inner())
}

pub fn fetch_cover(
    http: &dyn Http,
    throttle: &Throttle,
    release: Option<&str>,
    release_group: Option<&str>,
    artist: &str,
    album: &str,
) -> Option<Vec<u8>> {
    let caa_urls = release
        .map(caa_release_url)
        .into_iter()
        .chain(release_group.map(caa_release_group_url));
    for url in caa_urls {
        throttle.wait();
        if let Some(cover) = download(http, &url) {
            return Some(cover);
        }
    }
    let by_name = [
        (
            itunes_search_url(artist, album),
            itunes_pick as fn(&[u8], &str, &str) -> Option<String>,
        ),
        (deezer_search_url(artist, album), deezer_pick),
    ];
    for (search, pick) in by_name {
        let Some(url) = ok_body(http, &search).and_then(|body| pick(&body, artist, album)) else {
            continue;
        };
        if let Some(cover) = download(http, &url) {
            return Some(cover);
        }
    }
    None
}

fn ok_body(http: &dyn Http, url: &str) -> Option<Vec<u8>> {
    let response = http.get(url).ok()?;
    (response.status == 200).then_some(response.body)
}

fn download(http: &dyn Http, url: &str) -> Option<Vec<u8>> {
    if !url.starts_with("https://") {
        return None;
    }
    validate_image(&ok_body(http, url)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lookup::http::{FakeHttp, Throttle};
    use std::time::Duration;

    pub(crate) fn png(width: u32, height: u32) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(width, height, image::Rgba([200, 30, 30, 255]))
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    #[test]
    fn itunes_pick_rewrites_size_and_requires_close_match() {
        let body = r#"{"results":[
            {"artistName":"Exxótica","collectionName":"Capítulo II","artworkUrl100":"https://x/cap/100x100bb.jpg"},
            {"artistName":"Exxótica","collectionName":"Estranhos no Ninho","artworkUrl100":"https://x/ninho/100x100bb.jpg"}
        ]}"#;
        assert_eq!(
            itunes_pick(body.as_bytes(), "Exxótica", "Estranhos no Ninho").as_deref(),
            Some("https://x/ninho/1200x1200bb.jpg")
        );
        assert_eq!(itunes_pick(body.as_bytes(), "Exxótica", "Arquivo"), None);
        let numbered = r#"{"results":[{"artistName":"Led Zeppelin","collectionName":"Led Zeppelin III","artworkUrl100":"https://x/iii/100x100bb.jpg"}]}"#;
        assert_eq!(
            itunes_pick(numbered.as_bytes(), "Led Zeppelin", "Led Zeppelin II"),
            None
        );
        let live = r#"{"results":[{"artistName":"A","collectionName":"Unplugged (Live)","artworkUrl100":"https://x/u/100x100bb.jpg"}]}"#;
        assert_eq!(itunes_pick(live.as_bytes(), "A", "Unplugged"), None);
        assert_eq!(
            itunes_pick(body.as_bytes(), "Someone Else", "Estranhos no Ninho"),
            None
        );
        assert_eq!(itunes_pick(b"garbage", "a", "b"), None);
    }

    #[test]
    fn deezer_pick_reads_cover_xl() {
        let body = r#"{"data":[
            {"title":"Estranhos no Ninho","artist":{"name":"Exxótica"},"cover_xl":"https://d/xl.jpg","cover_big":"https://d/big.jpg"}
        ]}"#;
        assert_eq!(
            deezer_pick(body.as_bytes(), "Exxótica", "Estranhos no Ninho").as_deref(),
            Some("https://d/xl.jpg")
        );
        assert_eq!(deezer_pick(body.as_bytes(), "Exxótica", "IV"), None);
    }

    #[test]
    fn search_urls_percent_encode_names() {
        assert_eq!(
            itunes_search_url("Exxótica", "Estranhos no Ninho"),
            "https://itunes.apple.com/search?entity=album&limit=5&term=Exx%C3%B3tica%20Estranhos%20no%20Ninho"
        );
        assert_eq!(
            deezer_search_url("Exxótica", "IV"),
            "https://api.deezer.com/search/album?q=artist%3A%22Exx%C3%B3tica%22%20album%3A%22IV%22"
        );
        assert_eq!(
            caa_release_url("r1"),
            "https://coverartarchive.org/release/r1/front-500"
        );
        assert_eq!(
            caa_release_group_url("g1"),
            "https://coverartarchive.org/release-group/g1/front-500"
        );
    }

    #[test]
    fn validate_image_rejects_small_and_non_square() {
        assert!(validate_image(&png(100, 100)).is_none());
        assert!(validate_image(&png(400, 200)).is_none());
        assert!(
            validate_image(&png(400, 440)).is_some(),
            "within 10% of square"
        );
        assert!(validate_image(b"not an image").is_none());
        let ok = validate_image(&png(400, 400)).unwrap();
        assert_eq!(
            image::guess_format(&ok).unwrap(),
            image::ImageFormat::Jpeg,
            "stored as JPEG"
        );
    }

    #[test]
    fn validate_image_keeps_jpeg_bytes_as_is() {
        let mut jpeg = std::io::Cursor::new(Vec::new());
        image::RgbImage::from_pixel(400, 400, image::Rgb([1, 2, 3]))
            .write_to(&mut jpeg, image::ImageFormat::Jpeg)
            .unwrap();
        let jpeg = jpeg.into_inner();
        assert_eq!(validate_image(&jpeg).unwrap(), jpeg);
    }

    #[test]
    fn fetch_cover_falls_through_sources() {
        let http = FakeHttp::new()
            .on("https://coverartarchive.org/release/r1", 404, b"".to_vec())
            .on(
                "https://coverartarchive.org/release-group/g1",
                200,
                png(400, 400),
            );
        let throttle = Throttle::new(Duration::ZERO);
        let cover = fetch_cover(&http, &throttle, Some("r1"), Some("g1"), "Exxótica", "IV");
        assert!(cover.is_some());
        assert_eq!(
            http.requests(),
            [
                "https://coverartarchive.org/release/r1/front-500",
                "https://coverartarchive.org/release-group/g1/front-500"
            ]
        );
    }

    #[test]
    fn fetch_cover_uses_itunes_when_caa_has_nothing() {
        let body = r#"{"results":[{"artistName":"Exxótica","collectionName":"IV","artworkUrl100":"https://x/iv/100x100bb.jpg"}]}"#;
        let http = FakeHttp::new()
            .on(
                "https://itunes.apple.com/search",
                200,
                body.as_bytes().to_vec(),
            )
            .on("https://x/iv/1200x1200bb.jpg", 200, png(1200, 1200));
        let throttle = Throttle::new(Duration::ZERO);
        let cover = fetch_cover(&http, &throttle, Some("r1"), None, "Exxótica", "IV");
        assert!(cover.is_some());
        let requests = http.requests();
        assert_eq!(
            requests[0],
            "https://coverartarchive.org/release/r1/front-500"
        );
        assert!(requests[1].starts_with("https://itunes.apple.com/search"));
        assert_eq!(requests[2], "https://x/iv/1200x1200bb.jpg");
        assert_eq!(
            requests.len(),
            3,
            "Deezer is not asked once iTunes delivered"
        );
    }

    #[test]
    fn fetch_cover_only_downloads_over_https() {
        let itunes = r#"{"results":[{"artistName":"A","collectionName":"B","artworkUrl100":"http://x/100x100bb.jpg"}]}"#;
        let http = FakeHttp::new()
            .on(
                "https://itunes.apple.com/search",
                200,
                itunes.as_bytes().to_vec(),
            )
            .on("http://x/1200x1200bb.jpg", 200, png(600, 600));
        let throttle = Throttle::new(Duration::ZERO);
        assert!(fetch_cover(&http, &throttle, None, None, "A", "B").is_none());
        assert!(
            http.requests().iter().all(|u| u.starts_with("https://")),
            "{:?}",
            http.requests()
        );
    }

    #[test]
    fn fetch_cover_rejects_bad_image_and_continues_to_deezer() {
        let itunes = r#"{"results":[{"artistName":"A","collectionName":"B","artworkUrl100":"https://x/100x100bb.jpg"}]}"#;
        let deezer =
            r#"{"data":[{"title":"B","artist":{"name":"A"},"cover_xl":"https://d/xl.jpg"}]}"#;
        let http = FakeHttp::new()
            .on(
                "https://itunes.apple.com/search",
                200,
                itunes.as_bytes().to_vec(),
            )
            .on("https://x/1200x1200bb.jpg", 200, png(100, 100))
            .on(
                "https://api.deezer.com/search/album",
                200,
                deezer.as_bytes().to_vec(),
            )
            .on("https://d/xl.jpg", 200, png(600, 600));
        let throttle = Throttle::new(Duration::ZERO);
        let cover = fetch_cover(&http, &throttle, None, None, "A", "B").unwrap();
        let decoded = image::load_from_memory(&cover).unwrap();
        assert_eq!(decoded.width(), 600);
        assert!(http
            .requests()
            .last()
            .unwrap()
            .starts_with("https://d/xl.jpg"));
    }
}
