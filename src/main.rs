use gpui_kit::{px, size, App, AppContext, Bounds, QuitMode, WindowBounds, WindowOptions};
use omamusic::desktop::{self, Command};
use omamusic::{AppConfig, Library, Player, Root};
use std::sync::Arc;

fn main() {
    if std::env::args().any(|a| a == "--dump-library") {
        dump_library();
        return;
    }
    if desktop::raise_running() {
        return;
    }
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(|cx: &mut App| {
            gpui_omarchy::init(cx);
            cx.set_quit_mode(QuitMode::Explicit);

            let library = Arc::new(Library::new().expect("library"));
            let config = AppConfig::load();
            let player = Arc::new(Player::new(config.volume).expect("player"));
            let (commands, received) = async_channel::unbounded();
            let has_tray = desktop::start(player.clone(), library.clone(), commands);

            omamusic::init_actions(&library, &player, cx);

            cx.on_window_closed(move |cx, _| {
                if !cx.windows().is_empty() {
                    return;
                }
                let config = AppConfig::load();
                if !has_tray || config.quit_on_close {
                    cx.quit();
                } else if !config.tray_hint_shown {
                    AppConfig::update(|c| c.tray_hint_shown = true);
                    desktop::notify(
                        "omamusic is still running",
                        "Find it in the tray. To stop playback and exit, choose Quit from the tray menu.",
                    );
                }
            })
            .detach();

            cx.spawn({
                let library = library.clone();
                let player = player.clone();
                async move |cx| {
                    while let Ok(command) = received.recv().await {
                        cx.update(|cx| match command {
                            Command::Quit => cx.quit(),
                            Command::ShowWindow => show_window(&library, &player, cx),
                            Command::ToggleWindow => match cx.windows().first() {
                                Some(window) => {
                                    let _ = window.update(cx, |_, window, _| window.remove_window());
                                }
                                None => show_window(&library, &player, cx),
                            },
                        });
                    }
                }
            })
            .detach();

            if !(has_tray && config.start_hidden) {
                show_window(&library, &player, cx);
            }
            cx.activate(true);
        });
}

fn show_window(library: &Arc<Library>, player: &Arc<Player>, cx: &mut App) {
    if let Some(window) = cx.windows().first() {
        let _ = window.update(cx, |_, window, _| window.activate_window());
        return;
    }
    let (library, player) = (library.clone(), player.clone());
    let opened = cx.open_window(
        WindowOptions {
            titlebar: None,
            window_min_size: Some(size(px(560.), px(420.))),
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1020.), px(680.)),
                cx,
            ))),
            ..Default::default()
        },
        |window, cx| cx.new(|cx| Root::new(library, player, window, cx)),
    );
    if let Err(err) = opened {
        eprintln!("open window: {err:#}");
    }
}

fn dump_library() {
    let lib = Library::new().expect("library");
    for _ in 0..150 {
        std::thread::sleep(std::time::Duration::from_millis(200));
        if !lib.snapshot().scanning {
            break;
        }
    }
    let s = lib.snapshot();
    println!("albums={} tracks={}", s.album_count, s.track_count);
    for album in lib.album_page(0, 1_000_000, omamusic::models::AlbumSort::Artist) {
        println!(
            "ALBUM {:?} artist={:?} year={} n={}",
            album.title, album.artist, album.year, album.track_count
        );
        for t in lib.album_tracks(&album.id) {
            println!(
                "  TRK {:?} artist={:?} album={:?} no={} dur={:.1} file={}",
                t.title,
                t.artist,
                t.album,
                t.track_number,
                t.duration,
                t.path.display()
            );
        }
    }
    for q in [
        "neon",
        "test artist",
        "song",
        "midnight drive",
        "zzz-no-match",
    ] {
        lib.set_query(q.to_string());
        let s = lib.snapshot();
        let albums = s
            .filter
            .as_ref()
            .map_or(s.album_count, |f| f.album_keys.len());
        let tracks = s
            .filter
            .as_ref()
            .map_or(s.track_count, |f| f.track_ids.len());
        println!(
            "query={q:?} albums={} tracks={} artists={}",
            albums,
            tracks,
            lib.artist_total()
        );
    }
}
