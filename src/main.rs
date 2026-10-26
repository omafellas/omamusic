mod app;
mod artwork;
mod config;
mod db;
mod desktop;
mod index;
mod library;
mod metadata;
mod models;
mod mpris;
mod player;
mod player_bar;
mod search;
mod tray;

use app::Omamusic;
use desktop::Command;
use gpui_kit::{
    actions, div, px, size, App, AppContext, Bounds, Context, Entity, IntoElement, ParentElement,
    QuitMode, Render, StyleRefinement, Styled, Window, WindowBounds, WindowOptions,
};
use gpui_omarchy::{separator, ActiveTheme};
use player_bar::PlayerBar;
use std::sync::Arc;

use crate::config::AppConfig;
use crate::library::Library;
use crate::player::Player;

actions!(
    omamusic,
    [
        TogglePlay,
        NextTrack,
        PrevTrack,
        Rescan,
        FocusSearch,
        GoBack,
        Quit
    ]
);

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

            cx.bind_keys([
                gpui_kit::KeyBinding::new("space", TogglePlay, None),
                gpui_kit::KeyBinding::new("right", NextTrack, None),
                gpui_kit::KeyBinding::new("left", PrevTrack, None),
                gpui_kit::KeyBinding::new("ctrl-r", Rescan, None),
                gpui_kit::KeyBinding::new("ctrl-f", FocusSearch, None),
                gpui_kit::KeyBinding::new("alt-left", GoBack, None),
                gpui_kit::KeyBinding::new("ctrl-q", Quit, None),
            ]);

            let library = Arc::new(Library::new().expect("library"));
            let config = AppConfig::load();
            let player = Arc::new(Player::new(config.volume).expect("player"));
            desktop::set_cover_icon(!config.plain_tray_icon);
            let (commands, received) = async_channel::unbounded();
            let has_tray = desktop::start(player.clone(), library.clone(), commands);

            cx.on_action(|_: &Quit, cx| cx.quit());

            // Ctrl+F must work from a cold window: nothing inside Omamusic is on the
            // keystroke dispatch path until the first click focuses an element, so
            // focus is forwarded at the application level as well as from the view.
            bind_focus_search(cx);

            cx.on_action({
                let player = player.clone();
                move |_: &TogglePlay, cx| {
                    player.toggle();
                    cx.refresh_windows();
                }
            });
            cx.on_action({
                let player = player.clone();
                move |_: &NextTrack, cx| {
                    player.next();
                    cx.refresh_windows();
                }
            });
            cx.on_action({
                let player = player.clone();
                move |_: &PrevTrack, cx| {
                    player.previous();
                    cx.refresh_windows();
                }
            });
            cx.on_action({
                let library = library.clone();
                move |_: &Rescan, cx| {
                    library.request_rescan();
                    cx.refresh_windows();
                }
            });

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

/// Forwards `FocusSearch` from any window to its `Omamusic` view. Keystrokes
/// only reach element handlers below the window root once something has been
/// clicked, so this application-level path makes Ctrl+F work from a cold
/// window too.
/// Forwards `FocusSearch` to every window's search field. Keystrokes reach
/// element handlers below the window root only once something inside has been
/// focused, so for a cold window this application-level path catches the
/// action. The update is deferred because during dispatch the window is
/// checked out of the app and cannot be re-entered.
fn bind_focus_search(cx: &mut App) {
    cx.on_action(move |_: &FocusSearch, cx| {
        let windows = cx.windows();
        cx.defer(move |cx| {
            for window in windows {
                let _ = window.update(cx, |view, window, cx| {
                    let Ok(root) = view.downcast::<Root>() else {
                        return;
                    };
                    root.update(cx, |root, cx| {
                        root.main
                            .update(cx, |main, cx| main.focus_search(window, cx));
                    });
                });
            }
        });
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
        |window, cx| {
            cx.new(|cx| {
                let main = cx.new(|cx| Omamusic::new(library.clone(), player.clone(), window, cx));
                let bar =
                    cx.new(|cx| PlayerBar::new(library, player, main.downgrade(), window, cx));
                Root { main, bar }
            })
        },
    );
    if let Err(err) = opened {
        eprintln!("open window: {err:#}");
    }
}

struct Root {
    main: Entity<Omamusic>,
    bar: Entity<PlayerBar>,
}

impl Render for Root {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(cx.omarchy().background)
            .child(
                self.main
                    .clone()
                    .cached(StyleRefinement::default().flex_1().min_h_0().w_full()),
            )
            .child(separator(cx))
            .child(self.bar.clone())
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
    for album in lib.album_page(0, 1_000_000) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{Focusable, Keystroke, TestAppContext};

    /// Ctrl+F focuses the search field through the real window tree (Root above
    /// Omamusic) while nothing has been clicked yet — the case that made the
    /// keystroke never reach the field when only the view handled it.
    #[gpui_kit::test]
    async fn ctrl_f_finds_search_from_a_cold_window(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_omarchy::init(cx);
            cx.bind_keys([gpui_kit::KeyBinding::new("ctrl-f", FocusSearch, None)]);
            bind_focus_search(cx);
        });
        let (root, view) = cx.add_window_view(|window, cx| {
            let library = Arc::new(Library::empty());
            let player = Arc::new(Player::new_test());
            let main = cx.new(|cx| Omamusic::new(library.clone(), player.clone(), window, cx));
            let bar = cx.new(|cx| PlayerBar::new(library, player, main.downgrade(), window, cx));
            Root { main, bar }
        });

        view.update(|window, cx| {
            window.dispatch_keystroke(Keystroke::parse("ctrl-f").unwrap(), cx)
        });
        view.run_until_parked();

        root.update_in(view, |root, window, cx| {
            root.main.update(cx, |main, cx| {
                assert!(
                    main.query().read(cx).focus_handle(cx).is_focused(window),
                    "Ctrl+F reaches the search field before any click"
                );
            });
        });
    }
}
