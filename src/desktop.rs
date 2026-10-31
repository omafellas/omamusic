use crate::library::Library;
use crate::models::{PlayerState, Track};
use crate::player::Player;
use crate::{mpris, tray};
use async_channel::Sender;
use ksni::blocking::TrayMethods;
use mpris_server::zbus;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

const BUS_NAME: &str = "org.mpris.MediaPlayer2.omamusic";
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";

pub enum Command {
    ToggleWindow,
    ShowWindow,
    Quit,
}

pub struct Cover {
    pub url: String,
    pub menu_png: Vec<u8>,
    pub icon: ksni::Icon,
    pub dimmed: ksni::Icon,
}

#[derive(Clone, Default)]
pub struct NowPlaying {
    pub track: Option<Track>,
    pub state: PlayerState,
    pub cover: Option<Arc<Cover>>,
}

pub fn hash(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

pub fn raise_running() -> bool {
    async_io::block_on(async {
        let Ok(connection) = zbus::Connection::session().await else {
            return false;
        };
        connection
            .call_method(
                Some(BUS_NAME),
                MPRIS_PATH,
                Some("org.mpris.MediaPlayer2"),
                "Raise",
                &(),
            )
            .await
            .is_ok()
    })
}

pub fn notify(summary: &str, body: &str) {
    let (summary, body) = (summary.to_string(), body.to_string());
    std::thread::spawn(move || {
        async_io::block_on(async {
            let connection = zbus::Connection::session().await?;
            connection
                .call_method(
                    Some("org.freedesktop.Notifications"),
                    "/org/freedesktop/Notifications",
                    Some("org.freedesktop.Notifications"),
                    "Notify",
                    &(
                        "omamusic",
                        0u32,
                        "audio-x-generic",
                        summary.as_str(),
                        body.as_str(),
                        Vec::<&str>::new(),
                        std::collections::HashMap::<&str, zbus::zvariant::Value>::new(),
                        -1i32,
                    ),
                )
                .await
                .map(|_| ())
        })
        .unwrap_or_else(|err: zbus::Error| eprintln!("notify: {err}"));
    });
}

pub fn start(player: Arc<Player>, library: Arc<Library>, commands: Sender<Command>) -> bool {
    let tray = tray::Tray::new(player.clone(), commands.clone()).spawn();
    let tray = match tray {
        Ok(handle) => Some(handle),
        Err(err) => {
            eprintln!("tray: {err}");
            None
        }
    };
    let has_tray = tray.is_some();
    std::thread::spawn(move || async_io::block_on(sync(player, library, commands, tray)));
    has_tray
}

async fn sync(
    player: Arc<Player>,
    library: Arc<Library>,
    commands: Sender<Command>,
    tray: Option<ksni::blocking::Handle<tray::Tray>>,
) {
    let server = match mpris::serve(player.clone(), commands).await {
        Ok(server) => Some(server),
        Err(err) => {
            eprintln!("mpris: {err}");
            None
        }
    };
    let changes = player.subscribe();
    let mut now = NowPlaying::default();
    let mut position = 0.0;
    let mut checked = Instant::now();
    loop {
        let track = player.current();
        let state = player.state();
        let track_changed = track.as_ref().map(|t| &t.path) != now.track.as_ref().map(|t| &t.path);
        let album_changed =
            track.as_ref().map(|t| &t.album_id) != now.track.as_ref().map(|t| &t.album_id);
        let changed = track_changed || state != now.state;

        let previous = position;
        position = player.position();
        let expected = previous
            + if now.state == PlayerState::Playing {
                checked.elapsed().as_secs_f64()
            } else {
                0.0
            };
        checked = Instant::now();
        if let Some(server) = &server {
            if !track_changed && (position - expected).abs() > 1.0 {
                server.seeked(position).await;
            }
        }

        if changed {
            if album_changed {
                now.cover = track
                    .as_ref()
                    .and_then(|t| cover(&library, &t.album_id))
                    .map(Arc::new);
            }
            now.track = track;
            now.state = state;
            if let Some(tray) = &tray {
                let update = now.clone();
                tray.update(move |tray| tray.now = update);
            }
            if let Some(server) = &server {
                server
                    .imp()
                    .set_art(now.cover.as_ref().map(|c| c.url.clone()));
                server.changed(track_changed).await;
            }
        }

        let woke = changes.recv().await;
        if woke.is_err() {
            break;
        }
    }
}

fn cover(library: &Library, album_id: &str) -> Option<Cover> {
    let covers = library.covers();
    let dir = dirs::cache_dir()?.join("omamusic").join("covers");
    std::fs::create_dir_all(&dir).ok()?;
    let file: PathBuf = dir.join(format!("{:016x}.png", hash(album_id)));
    if !file.exists() {
        std::fs::write(&file, png_bytes(&covers.thumbnail(album_id, 512)?)?).ok()?;
    }
    let menu_png = png_bytes(&covers.thumbnail(album_id, 32)?)?;
    let small = covers.thumbnail(album_id, 64)?;
    Some(Cover {
        url: format!("file://{}", file.display()),
        menu_png,
        icon: argb(&small, 1.0),
        dimmed: argb(&small, 0.45),
    })
}

fn png_bytes(image: &image::RgbaImage) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    image
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .ok()?;
    Some(bytes)
}

fn argb(image: &image::RgbaImage, alpha: f32) -> ksni::Icon {
    let data = image
        .pixels()
        .flat_map(|p| {
            let [r, g, b, a] = p.0;
            [(a as f32 * alpha) as u8, r, g, b]
        })
        .collect();
    ksni::Icon {
        width: image.width() as i32,
        height: image.height() as i32,
        data,
    }
}
