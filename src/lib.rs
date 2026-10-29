pub mod app;
pub mod artwork;
pub mod config;
pub mod db;
pub mod desktop;
pub mod index;
pub mod library;
pub mod liked;
pub mod lookup;
pub mod metadata;
pub mod models;
pub mod mpris;
pub mod player;
pub mod player_bar;
pub mod playlists;
pub mod search;
pub mod tray;

use gpui_kit::{
    actions, div, AppContext, Context, Entity, IntoElement, ParentElement, Render, StyleRefinement,
    Styled, Window,
};
use gpui_omarchy::{separator, ActiveTheme};
use std::sync::Arc;

pub use app::Omamusic;
pub use config::AppConfig;
pub use library::Library;
pub use player::Player;
pub use player_bar::PlayerBar;

actions!(
    omamusic,
    [
        TogglePlay,
        NextTrack,
        PrevTrack,
        Rescan,
        FocusSearch,
        GoBack,
        GoForward,
        IncreaseFontSize,
        DecreaseFontSize,
        ResetFontSize,
        Quit
    ]
);

pub fn key_bindings() -> Vec<gpui_kit::KeyBinding> {
    vec![
        gpui_kit::KeyBinding::new("space", TogglePlay, None),
        gpui_kit::KeyBinding::new("right", NextTrack, None),
        gpui_kit::KeyBinding::new("left", PrevTrack, None),
        gpui_kit::KeyBinding::new("ctrl-r", Rescan, None),
        gpui_kit::KeyBinding::new("ctrl-f", FocusSearch, None),
        gpui_kit::KeyBinding::new("alt-left", GoBack, None),
        gpui_kit::KeyBinding::new("alt-right", GoForward, None),
        gpui_kit::KeyBinding::new("ctrl-=", IncreaseFontSize, None),
        gpui_kit::KeyBinding::new("ctrl--", DecreaseFontSize, None),
        gpui_kit::KeyBinding::new("ctrl-0", ResetFontSize, None),
        gpui_kit::KeyBinding::new("ctrl-q", Quit, None),
    ]
}

/// Install the key bindings and the app-wide action handlers. The binary and
/// the UI smoke tests both go through here so they share one wiring.
pub fn init_actions(library: &Arc<Library>, player: &Arc<Player>, cx: &mut gpui_kit::App) {
    cx.bind_keys(key_bindings());
    cx.on_action(|_: &Quit, cx| cx.quit());
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
}

/// The window content: the main view above the player bar.
pub struct Root {
    main: Entity<Omamusic>,
    bar: Entity<PlayerBar>,
}

impl Root {
    pub fn new(
        library: Arc<Library>,
        player: Arc<Player>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let main = cx.new(|cx| Omamusic::new(library.clone(), player.clone(), window, cx));
        let bar = cx.new(|cx| PlayerBar::new(library, player, main.downgrade(), window, cx));
        Self { main, bar }
    }

    pub fn main(&self) -> &Entity<Omamusic> {
        &self.main
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_bindings_parse() {
        // `KeyBinding::new` panics on an invalid keystroke, so building the
        // real list validates every shortcut, including ctrl-= / ctrl--.
        assert_eq!(key_bindings().len(), 11);
    }
}
