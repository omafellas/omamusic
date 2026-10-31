//! UI smoke tests. Each test drives the production `Root` view in a headless
//! window with the real key bindings and asserts the visible outcome.
use gpui_kit::test::TestWindowExt;
use gpui_kit::{px, ElementId, Entity, Pixels, TestAppContext, VisualTestContext};
use omamusic::lookup::http::FakeHttp;
use omamusic::models::{AlbumSummary, Track};
use omamusic::{AppConfig, Library, Omamusic, Player, Root};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

static NEXT_CONFIG_DIR: AtomicUsize = AtomicUsize::new(0);

/// Fresh config dir for this test's thread so nothing touches the user's file.
fn isolated_config_dir() -> PathBuf {
    let n = NEXT_CONFIG_DIR.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("omamusic-ui-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create config dir");
    AppConfig::use_dir(&dir);
    dir
}

struct Smoke<'a> {
    view: Entity<Omamusic>,
    cx: &'a mut VisualTestContext,
    config_dir: PathBuf,
}

impl Smoke<'_> {
    fn scale(&mut self) -> f64 {
        self.view.read_with(self.cx, |view, _| view.ui_scale())
    }

    fn rem(&mut self) -> Pixels {
        self.cx.update(|window, _| window.rem_size())
    }

    fn saved_scale(&self) -> f64 {
        let text = std::fs::read_to_string(self.config_dir.join("config.json")).unwrap_or_default();
        serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("ui_scale")?.as_f64())
            .unwrap_or(1.0)
    }

    fn press(&mut self, key: &str) {
        self.cx.update(|window, cx| window.press(key, cx));
        self.cx.run_until_parked();
    }

    fn click(&mut self, id: &'static str) {
        self.cx.update(|window, cx| window.click(id, cx));
        self.cx.run_until_parked();
    }

    fn label(&mut self, id: &'static str) -> Option<String> {
        self.cx.update(|window, cx| {
            window.render_frame(cx);
            window.find(id).label().map(str::to_owned)
        })
    }

    fn exists(&mut self, id: &str) -> bool {
        let id = id.to_string();
        self.cx.update(|window, cx| {
            window.render_frame(cx);
            window.try_find(id).is_some()
        })
    }

    fn label_of(&mut self, id: impl Into<ElementId>) -> Option<String> {
        let id = id.into();
        self.cx.update(|window, cx| {
            window.render_frame(cx);
            window.try_find(id)?.label().map(str::to_owned)
        })
    }

    fn has(&mut self, id: impl Into<ElementId>) -> bool {
        let id = id.into();
        self.cx.update(|window, cx| {
            window.render_frame(cx);
            window.try_find(id).is_some()
        })
    }

    fn click_on(&mut self, id: impl Into<ElementId>) {
        let id = id.into();
        self.cx.update(|window, cx| {
            window.render_frame(cx);
            window.click(id, cx)
        });
        self.cx.run_until_parked();
    }

    fn play_with(&mut self, act: impl FnOnce(&mut gpui_kit::Window, &mut gpui_kit::App)) {
        self.cx.update(|window, cx| {
            window.render_frame(cx);
            act(window, cx)
        });
        self.view
            .read_with(self.cx, |view, _| view.player.clone())
            .settle();
        self.cx.run_until_parked();
    }

    fn right_click_on(&mut self, id: impl Into<ElementId>) {
        let id = id.into();
        self.cx.update(|window, cx| {
            window.render_frame(cx);
            window.right_click(id, cx)
        });
        self.cx.run_until_parked();
    }

    fn typing(&mut self, text: &str) {
        self.cx.update(|window, cx| window.input(text, cx));
        self.cx.run_until_parked();
    }

    fn playlists_file(&self) -> serde_json::Value {
        let text =
            std::fs::read_to_string(self.config_dir.join("playlists.json")).unwrap_or_default();
        serde_json::from_str(&text).unwrap_or_default()
    }

    fn hover_on(&mut self, id: impl Into<ElementId>) {
        let id = id.into();
        self.cx.update(|window, cx| {
            window.render_frame(cx);
            window.hover(id, cx)
        });
        self.cx.run_until_parked();
        self.cx.executor().advance_clock(Duration::from_millis(300));
        self.cx.run_until_parked();
    }

    fn bounds(&mut self, id: &'static str) -> gpui_kit::Bounds<Pixels> {
        self.cx.update(|window, cx| {
            window.render_frame(cx);
            window.find(id).bounds()
        })
    }

    fn tooltip_visible(&mut self) -> bool {
        self.cx.update(|window, cx| window.render_frame(cx));
        self.cx.debug_bounds("omarchy-tooltip-surface").is_some()
    }

    fn liked_file(&self) -> serde_json::Value {
        let text = std::fs::read_to_string(self.config_dir.join("liked.json")).unwrap_or_default();
        serde_json::from_str(&text).unwrap_or_default()
    }

    fn search(&mut self, text: &str) {
        self.press("ctrl-f");
        self.cx.update(|window, cx| window.input(text, cx));
        self.cx.executor().advance_clock(Duration::from_millis(400));
        self.cx.run_until_parked();
    }

    fn queued(&mut self) -> Vec<(String, String)> {
        self.view.read_with(self.cx, |view, _| {
            view.player
                .queue()
                .0
                .iter()
                .map(|t| (t.album_id.clone(), t.artist.clone()))
                .collect()
        })
    }

    fn saved(&self, key: &str) -> serde_json::Value {
        let text = std::fs::read_to_string(self.config_dir.join("config.json")).unwrap_or_default();
        serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get(key).cloned())
            .unwrap_or_default()
    }

    fn search_text(&mut self) -> String {
        self.view
            .read_with(self.cx, |view, cx| view.search_text(cx))
    }
}

fn open(cx: &mut TestAppContext) -> Smoke<'_> {
    open_with(cx, Arc::new(Library::empty()))
}

fn open_with(cx: &mut TestAppContext, library: Arc<Library>) -> Smoke<'_> {
    let config_dir = isolated_config_dir();
    let player = Arc::new(Player::new_test());
    cx.update(|cx| {
        gpui_omarchy::init(cx);
        gpui_omarchy::Theme::tokyo_night().apply(cx);
        omamusic::init_actions(&library, &player, cx);
    });
    let (root, cx) = cx.add_window_view(|window, cx| Root::new(library, player, window, cx));
    let view = root.read_with(cx, |root, _| root.main().clone());
    cx.run_until_parked();
    Smoke {
        view,
        cx,
        config_dir,
    }
}

#[gpui_kit::test]
async fn ctrl_plus_minus_and_zero_change_text_size_with_nothing_focused(cx: &mut TestAppContext) {
    let mut app = open(cx);
    assert_eq!(app.scale(), 1.0);
    assert_eq!(app.rem(), px(16.0));

    app.press("ctrl-=");
    app.press("ctrl-=");
    assert_eq!(app.scale(), 1.2);
    assert_eq!(app.rem(), px(19.2));
    assert_eq!(app.saved_scale(), 1.2, "the new size is persisted");

    app.press("ctrl--");
    assert_eq!(app.scale(), 1.1);
    assert_eq!(app.rem(), px(17.6));

    app.press("ctrl-0");
    assert_eq!(app.scale(), 1.0);
    assert_eq!(app.rem(), px(16.0));
    assert_eq!(app.saved_scale(), 1.0);
}

#[gpui_kit::test]
async fn text_size_shortcuts_work_while_typing_in_search(cx: &mut TestAppContext) {
    let mut app = open(cx);
    app.press("ctrl-f");
    app.cx.update(|window, cx| window.input("ab", cx));
    app.cx.run_until_parked();
    assert_eq!(app.search_text(), "ab", "Ctrl+P focused the search field");

    app.press("ctrl-=");
    assert_eq!(app.scale(), 1.1);
    assert_eq!(
        app.search_text(),
        "ab",
        "the shortcut is not typed into the field"
    );

    app.press("ctrl--");
    app.press("ctrl--");
    assert_eq!(app.scale(), 0.9);
    assert_eq!(app.search_text(), "ab");
}

#[gpui_kit::test]
async fn shortcuts_recover_after_focus_is_cleared(cx: &mut TestAppContext) {
    let mut app = open(cx);
    app.press("ctrl-f");
    app.cx.update(|window, cx| window.blur(cx));
    app.cx.run_until_parked();
    app.press("ctrl-=");
    assert_eq!(
        app.scale(),
        1.1,
        "focus returns to the view after the search field blurs"
    );

    app.cx.update(|window, cx| window.blur(cx));
    app.cx.run_until_parked();
    app.press("ctrl-=");
    assert_eq!(
        app.scale(),
        1.2,
        "focus returns to the view after the view itself blurs"
    );
}

#[gpui_kit::test]
async fn text_size_is_clamped(cx: &mut TestAppContext) {
    let mut app = open(cx);
    for _ in 0..15 {
        app.press("ctrl-=");
    }
    assert_eq!(app.scale(), 2.0);
    assert_eq!(app.rem(), px(32.0));
    for _ in 0..20 {
        app.press("ctrl--");
    }
    assert_eq!(app.scale(), 0.75);
    assert_eq!(app.rem(), px(12.0));
}

#[gpui_kit::test]
async fn settings_buttons_change_text_size(cx: &mut TestAppContext) {
    let mut app = open(cx);
    assert!(
        app.cx.update(|window, cx| {
            window.render_frame(cx);
            window.try_find("font-larger").is_none()
        }),
        "text size controls live in Settings"
    );

    app.click("settings");
    assert_eq!(app.label("font-reset").as_deref(), Some("100%"));

    app.click("font-larger");
    assert_eq!(app.scale(), 1.1);
    assert_eq!(app.rem(), px(17.6));
    assert_eq!(app.label("font-reset").as_deref(), Some("110%"));
    assert_eq!(app.saved_scale(), 1.1);

    app.click("font-smaller");
    app.click("font-smaller");
    assert_eq!(app.scale(), 0.9);
    assert_eq!(app.label("font-reset").as_deref(), Some("90%"));

    app.click("font-reset");
    assert_eq!(app.scale(), 1.0);
    assert_eq!(app.label("font-reset").as_deref(), Some("100%"));
    assert_eq!(app.saved_scale(), 1.0);
}

#[gpui_kit::test]
async fn settings_close_button_is_aligned_with_title(cx: &mut TestAppContext) {
    let mut app = open(cx);
    app.click("settings");

    let title = app.bounds("settings-title");
    let close = app.bounds("close-settings-x");
    let title_center = title.origin.y.as_f32() + title.size.height.as_f32() / 2.0;
    let close_center = close.origin.y.as_f32() + close.size.height.as_f32() / 2.0;
    assert!(
        (title_center - close_center).abs() <= 1.0,
        "the close button is vertically centered with the Settings title"
    );

    app.click("close-settings-x");
    assert!(
        !app.has("settings-panel"),
        "clicking the close button closes Settings"
    );
}

#[gpui_kit::test]
async fn settings_shows_version_and_github_link(cx: &mut TestAppContext) {
    let mut app = open(cx);
    app.click("settings");
    assert!(
        app.has("settings-version"),
        "settings shows the app version element"
    );
    assert!(app.has("github"), "settings links to GitHub");
    // The title names the file the settings live in.
    let title = app.label("settings-title").unwrap_or_default();
    assert!(
        title.starts_with("Settings (") && title.ends_with(')'),
        "the title includes the config path, got {title:?}"
    );
    assert!(
        title.contains("config.json"),
        "the title names the config file, got {title:?}"
    );
}

#[gpui_kit::test]
async fn settings_folder_button_toggles_add_and_browse(cx: &mut TestAppContext) {
    let mut app = open(cx);
    app.click("settings");

    // With an empty field the button is "Browse…" (opens the folder picker).
    assert_eq!(
        app.label("browse-folder").as_deref(),
        Some("Browse…"),
        "empty folder field shows Browse"
    );

    // Typing a path switches the button to "Add".
    app.click_on("new-folder");
    app.typing("/tmp/omamusic-settings-test-folder");
    assert_eq!(
        app.label("browse-folder").as_deref(),
        Some("Add"),
        "non-empty folder field shows Add"
    );
}

#[gpui_kit::test]
async fn rescan_completion_shows_a_toast(cx: &mut TestAppContext) {
    let library = Arc::new(Library::empty());
    let mut app = open_with(cx, library.clone());
    app.click("rescan");
    assert!(
        app.view
            .read_with(app.cx, |view, _| view.notify_text().is_none()),
        "rescan click alone does not show the toast"
    );

    // The scan finished (bumps scan_id and version).
    library.finish_scan_for_test(3, 12);
    app.cx.executor().advance_clock(Duration::from_millis(600));
    app.cx.run_until_parked();

    let text = app
        .view
        .read_with(app.cx, |view, _| view.notify_text())
        .expect("a toast is shown when a scan completes");
    assert!(
        text.contains("Library rescanned")
            && text.contains("12 songs")
            && text.contains("3 albums"),
        "toast describes the completed scan, got {text:?}"
    );
}

#[gpui_kit::test]
async fn online_lookup_switch_persists_to_config(cx: &mut TestAppContext) {
    let mut app = open(cx);
    app.click("settings");
    app.click("pref-online-lookup");
    let text = std::fs::read_to_string(app.config_dir.join("config.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["online_lookup"], serde_json::Value::Bool(true));
    app.click("pref-online-lookup");
    let text = std::fs::read_to_string(app.config_dir.join("config.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["online_lookup"], serde_json::Value::Bool(false));
}

#[gpui_kit::test]
async fn online_lookup_details_live_behind_the_info_icon_tooltip(cx: &mut TestAppContext) {
    let mut app = open(cx);
    app.click("settings");

    assert!(
        app.has("lookup-info-hint"),
        "the settings page keeps a hover target for the online-lookup details"
    );

    // The helper paragraph is gone; hovering the info icon surfaces it.
    app.hover_on("lookup-info-hint");
    app.cx.executor().advance_clock(Duration::from_millis(500));
    app.cx.run_until_parked();
    assert!(
        app.tooltip_visible(),
        "hovering the info icon shows the online-lookup details"
    );
}

fn album(key: &str, title: &str, unmatched: bool) -> AlbumSummary {
    AlbumSummary {
        id: key.to_string(),
        title: title.to_string(),
        artist: "Exxótica".to_string(),
        year: 2002,
        track_count: 1,
        unmatched,
    }
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[gpui_kit::test]
async fn unmatched_album_shows_badge_and_settings_count(cx: &mut TestAppContext) {
    let library = Arc::new(Library::empty());
    library.insert_album(&album("k", "Estranhos no Ninho", true));
    let mut app = open_with(cx, library);
    app.click("nav-albums");
    assert!(
        app.exists("album-unmatched-k"),
        "the album card carries a badge"
    );

    app.click("settings");
    assert_eq!(
        app.label("lookup-status").as_deref(),
        Some("1 album could not be identified")
    );

    app.click("lookup-retry");
    assert!(
        !app.exists("lookup-status"),
        "the list empties once retried"
    );
    assert!(!app.exists("album-unmatched-k"), "the badge goes with it");
}

#[gpui_kit::test]
async fn switching_lookup_on_runs_the_worker_through_the_real_wiring(cx: &mut TestAppContext) {
    let config_dir = isolated_config_dir();
    let http = Arc::new(FakeHttp::new().on(
        "https://musicbrainz.org/ws/2/release/?",
        200,
        br#"{"releases":[]}"#.to_vec(),
    ));
    let library = Arc::new(Library::with_lookup(&config_dir, http.clone()).unwrap());
    library.insert_album(&album("k", "Estranhos no Ninho", false));
    let mut app = open_with(cx, library.clone());
    app.click("nav-albums");
    assert!(!app.exists("album-unmatched-k"));

    app.click("settings");
    app.click("pref-online-lookup");
    wait_until("the first pass", || {
        library.snapshot().lookup_unmatched.len() == 1
    });
    assert_eq!(
        app.label("lookup-status").as_deref(),
        Some("1 album could not be identified")
    );
    assert!(app.exists("album-unmatched-k"));
    assert!(
        http.requests()[0].starts_with("https://musicbrainz.org/ws/2/release/?"),
        "{:?}",
        http.requests()
    );

    let asked = http.requests().len();
    app.click("lookup-retry");
    wait_until("the retry pass", || http.requests().len() > asked);
    wait_until("the retry result", || {
        library.snapshot().lookup_unmatched.len() == 1
    });
    assert!(
        app.exists("album-unmatched-k"),
        "still unidentified after the retry"
    );

    app.click("pref-online-lookup");
    wait_until("the switch to settle", || {
        !library.snapshot().folders.is_empty()
    });
}

#[gpui_kit::test]
async fn lookup_progress_is_shown_while_working(cx: &mut TestAppContext) {
    let config_dir = isolated_config_dir();
    let http = Arc::new(FakeHttp::new().with_delay(Duration::from_millis(200)).on(
        "https://musicbrainz.org/ws/2/release/?",
        200,
        br#"{"releases":[]}"#.to_vec(),
    ));
    let library = Arc::new(Library::with_lookup(&config_dir, http).unwrap());
    for (i, title) in ["Arquivo", "Capítulo II", "IV"].into_iter().enumerate() {
        library.insert_album(&album(&format!("k{i}"), title, false));
    }
    let mut app = open_with(cx, library.clone());
    app.click("settings");
    app.click("pref-online-lookup");
    wait_until("the pass to start", || {
        let snap = library.snapshot();
        snap.lookup_total == 3 && snap.lookup_pending < 3 && snap.lookup_pending > 0
    });
    let label = app.label("lookup-status").unwrap_or_default();
    assert!(
        label.starts_with("Looking up albums… ") && label.ends_with(" of 3"),
        "{label:?}"
    );
    wait_until("the pass to finish", || {
        library.snapshot().lookup_total == 0
    });
    assert_eq!(
        app.label("lookup-status").as_deref(),
        Some("3 albums could not be identified")
    );
}

#[gpui_kit::test]
async fn settings_hint_at_missing_acoustid_key(cx: &mut TestAppContext) {
    let dir = isolated_config_dir();
    std::fs::write(dir.join("config.json"), r#"{"acoustid_key": ""}"#).unwrap();
    let library = Arc::new(Library::empty());
    let player = Arc::new(Player::new_test());
    cx.update(|cx| {
        gpui_omarchy::init(cx);
        gpui_omarchy::Theme::tokyo_night().apply(cx);
        omamusic::init_actions(&library, &player, cx);
    });
    let (root, cx) = cx.add_window_view(|window, cx| Root::new(library, player, window, cx));
    cx.run_until_parked();
    let view = root.read_with(cx, |root, _| root.main().clone());
    let mut app = Smoke {
        view,
        cx,
        config_dir: dir,
    };
    app.click("settings");
    assert!(
        app.exists("lookup-acoustid-hint"),
        "without a key the settings explain how to enable fingerprinting"
    );
}

#[gpui_kit::test]
async fn no_acoustid_hint_once_a_key_is_configured(cx: &mut TestAppContext) {
    let dir = isolated_config_dir();
    std::fs::write(dir.join("config.json"), r#"{"acoustid_key": "k3y"}"#).unwrap();
    let library = Arc::new(Library::empty());
    let player = Arc::new(Player::new_test());
    cx.update(|cx| {
        gpui_omarchy::init(cx);
        gpui_omarchy::Theme::tokyo_night().apply(cx);
        omamusic::init_actions(&library, &player, cx);
    });
    let (root, cx) = cx.add_window_view(|window, cx| Root::new(library, player, window, cx));
    cx.run_until_parked();
    let view = root.read_with(cx, |root, _| root.main().clone());
    let mut app = Smoke {
        view,
        cx,
        config_dir: dir,
    };
    app.click("settings");
    assert!(!app.exists("lookup-acoustid-hint"));
}

#[gpui_kit::test]
async fn saved_text_size_is_applied_on_start(cx: &mut TestAppContext) {
    let dir = isolated_config_dir();
    std::fs::write(dir.join("config.json"), r#"{"ui_scale": 1.5}"#).unwrap();
    let library = Arc::new(Library::empty());
    let player = Arc::new(Player::new_test());
    cx.update(|cx| {
        gpui_omarchy::init(cx);
        gpui_omarchy::Theme::tokyo_night().apply(cx);
        omamusic::init_actions(&library, &player, cx);
    });
    let (root, cx) = cx.add_window_view(|window, cx| Root::new(library, player, window, cx));
    cx.run_until_parked();
    let view = root.read_with(cx, |root, _| root.main().clone());
    assert_eq!(view.read_with(cx, |view, _| view.ui_scale()), 1.5);
    assert_eq!(cx.update(|window, _| window.rem_size()), px(24.0));
}

fn release(key: &str, title: &str, artist: &str, year: u32) -> AlbumSummary {
    AlbumSummary {
        id: key.to_string(),
        title: title.to_string(),
        artist: artist.to_string(),
        year,
        track_count: 1,
        unmatched: false,
    }
}

fn library_of(albums: &[AlbumSummary]) -> Arc<Library> {
    let library = Arc::new(Library::empty());
    for album in albums {
        library.insert_album(album);
    }
    library
}

#[gpui_kit::test]
async fn album_sort_menu_reorders_the_grid_and_persists(cx: &mut TestAppContext) {
    let library = library_of(&[
        release("z", "Zebra", "Exxótica", 1990),
        release("a", "Alpha", "Exxótica", 2005),
    ]);
    let mut app = open_with(cx, library);
    app.click("nav-albums");
    assert_eq!(app.label_of(("album", 0usize)).as_deref(), Some("Zebra"));
    assert_eq!(app.label("album-sort").as_deref(), Some("Sort by Artist"));

    app.click("album-sort");
    app.press("down");
    app.press("enter");
    assert_eq!(app.label("album-sort").as_deref(), Some("Sort by Title"));
    assert_eq!(app.label_of(("album", 0usize)).as_deref(), Some("Alpha"));
    assert_eq!(app.saved("album_sort"), "title");
}

#[gpui_kit::test]
async fn album_view_toggle_switches_to_rows_and_persists(cx: &mut TestAppContext) {
    let library = library_of(&[release("k", "Estranhos no Ninho", "Exxótica", 2002)]);
    let mut app = open_with(cx, library);
    app.click("nav-albums");
    assert!(app.has(("album", 0usize)));
    assert!(!app.has(("album-row", 0usize)));

    app.click("albums-view-list");
    assert_eq!(
        app.label_of(("album-row", 0usize)).as_deref(),
        Some("Estranhos no Ninho")
    );
    assert!(!app.has(("album", 0usize)));
    assert_eq!(app.saved("album_view"), "list");

    app.click("albums-view-grid");
    assert!(app.has(("album", 0usize)));
    assert_eq!(app.saved("album_view"), "grid");
}

#[gpui_kit::test]
async fn album_card_play_button_plays_without_opening_the_album(cx: &mut TestAppContext) {
    let library = library_of(&[release("k", "Estranhos no Ninho", "Exxótica", 2002)]);
    let mut app = open_with(cx, library);
    app.click("nav-albums");
    app.hover_on(("album", 0usize));
    app.play_with(|window, cx| window.click("album-play-k", cx));
    assert_eq!(
        app.queued(),
        [("k".to_string(), "Exxótica".to_string())],
        "the hover button queues the album"
    );
    assert!(!app.has("album-title"), "the click stays on the grid");
}

#[gpui_kit::test]
async fn album_page_links_to_the_artist_and_other_albums(cx: &mut TestAppContext) {
    let library = library_of(&[
        release("k", "Estranhos no Ninho", "Exxótica", 2002),
        release("k2", "Arquivo", "Exxótica", 2002),
    ]);
    let mut app = open_with(cx, library);
    app.click("nav-albums");
    assert_eq!(app.label("view-title").as_deref(), Some("Albums"));
    app.click("nav-back");
    assert_eq!(
        app.label("view-title").as_deref(),
        Some("Albums"),
        "no history on a top-level view"
    );

    app.click_on(("album", 0usize));
    assert_eq!(app.label("album-title").as_deref(), Some("Arquivo"));
    assert_eq!(
        app.label("album-meta").as_deref(),
        Some("2002 · 1 song · 3 min")
    );
    assert_eq!(
        app.label_of(("more-by", 0usize)).as_deref(),
        Some("Estranhos no Ninho")
    );

    app.click_on(("more-by", 0usize));
    assert_eq!(
        app.label("album-title").as_deref(),
        Some("Estranhos no Ninho")
    );

    app.click("album-artist");
    assert_eq!(app.label("view-title").as_deref(), Some("Exxótica"));

    app.press("alt-left");
    assert_eq!(
        app.label("album-title").as_deref(),
        Some("Estranhos no Ninho")
    );

    app.click("nav-back");
    assert_eq!(app.label("album-title").as_deref(), Some("Arquivo"));
    app.hover_on(("album-track", 0usize));
    app.play_with(|window, cx| window.click(("album-track-play", 0usize), cx));
    assert_eq!(app.queued(), [("k2".to_string(), "Exxótica".to_string())]);

    app.click("nav-back");
    assert_eq!(app.label("view-title").as_deref(), Some("Albums"));
    assert!(app.has(("album", 0usize)), "back lands on the grid");
}

#[gpui_kit::test]
async fn search_groups_results_and_enter_plays_the_top_result(cx: &mut TestAppContext) {
    let library = library_of(&[
        release("k", "Estranhos no Ninho", "Exxótica", 2002),
        release("k2", "Arquivo", "Exxótica", 2002),
        release("o", "Outro", "Banda", 2010),
    ]);
    let mut app = open_with(cx, library);
    app.click("nav-albums");
    app.search("exx");
    assert_eq!(app.search_text(), "exx");
    assert_eq!(
        app.label("search-summary").as_deref(),
        Some("5 results for “exx”")
    );
    assert_eq!(app.label("search-top-result").as_deref(), Some("Exxótica"));
    assert_eq!(
        app.label("search-scope-songs").as_deref(),
        Some("Songs · 2")
    );
    assert_eq!(
        app.label("search-scope-albums").as_deref(),
        Some("Albums · 2")
    );
    assert_eq!(
        app.label("search-scope-artists").as_deref(),
        Some("Artists · 1")
    );
    assert!(
        !app.has(("album", 0usize)),
        "the Albums view gives way to search"
    );

    app.play_with(|window, cx| window.press("enter", cx));
    let queued = app.queued();
    assert_eq!(queued.len(), 2, "{queued:?}");
    assert!(queued.iter().all(|(_, artist)| artist == "Exxótica"));
}

#[gpui_kit::test]
async fn search_scopes_and_top_result_navigate_and_come_back(cx: &mut TestAppContext) {
    let library = library_of(&[
        release("k", "Estranhos no Ninho", "Exxótica", 2002),
        release("o", "Outro", "Banda", 2010),
    ]);
    let mut app = open_with(cx, library);
    app.search("exx");

    app.click("search-scope-albums");
    assert!(app.has(("album", 0usize)));
    assert!(!app.has("search-top-result"));

    app.click("search-scope-all");
    app.click("search-top-result");
    assert_eq!(app.label("view-title").as_deref(), Some("Exxótica"));

    app.click("nav-back");
    assert!(app.has("search-top-result"), "back returns to the results");

    app.click("nav-songs");
    assert_eq!(app.search_text(), "");
    assert!(
        !app.has("search-summary"),
        "a section click ends the search"
    );
}

fn song(title: &str, artist: &str, album: &str, genre: &str) -> Track {
    Track {
        path: PathBuf::from(format!("/music/{album}/{title}.flac")),
        title: title.to_string(),
        artist: artist.to_string(),
        album: album.to_string(),
        track_number: 1,
        year: 2001,
        duration: 200.0,
        album_id: format!("{album}-key"),
        genre: genre.to_string(),
    }
}

fn songs_library() -> Arc<Library> {
    let library = Arc::new(Library::empty());
    library.insert_song(&song("Alpha", "Zeta", "Zed", "Jazz"), 10);
    library.insert_song(&song("Bravo", "Mid", "Mod", "Rock"), 30);
    library.insert_song(&song("Charlie", "Mid", "Mod", "Rock"), 20);
    library
}

#[gpui_kit::test]
async fn sidebar_sections_show_their_title_and_counts(cx: &mut TestAppContext) {
    let mut app = open_with(cx, songs_library());
    assert_eq!(app.label("view-title").as_deref(), Some("Songs"));
    assert_eq!(
        app.label("view-detail").as_deref(),
        Some("3 songs · 10 min")
    );

    app.click("nav-recent");
    assert_eq!(app.label("view-title").as_deref(), Some("Recently Added"));
    assert_eq!(
        app.label_of(("song-play", 0usize)).as_deref(),
        Some("Play Bravo"),
        "newest file first"
    );

    app.click("nav-genres");
    assert_eq!(app.label("view-detail").as_deref(), Some("2 genres"));
    assert_eq!(app.label_of(("genre", 1usize)).as_deref(), Some("Rock"));
    app.click_on(("genre", 1usize));
    assert_eq!(app.label("view-title").as_deref(), Some("Rock"));
    assert_eq!(app.label("view-detail").as_deref(), Some("2 songs · 7 min"));
    assert_eq!(
        app.label_of(("song-play", 0usize)).as_deref(),
        Some("Play Bravo")
    );
    assert!(!app.has(("song-play", 2usize)), "only the genre's songs");
    assert!(
        !app.has("library-status"),
        "the up-to-date status line is removed"
    );
}

#[gpui_kit::test]
async fn genre_tiles_show_counts_and_play_the_genre(cx: &mut TestAppContext) {
    let mut app = open_with(cx, songs_library());
    app.click("nav-genres");
    assert_eq!(app.label_of(("genre", 0usize)).as_deref(), Some("Jazz"));
    assert_eq!(
        app.label_of(("genre-count", 0usize)).as_deref(),
        Some("1 song · 1 album")
    );
    assert_eq!(
        app.label_of(("genre-count", 1usize)).as_deref(),
        Some("2 songs · 1 album")
    );
    assert!(app.has(("genre-art", 1usize)), "a tile always has art");
    app.hover_on(("genre", 1usize));
    assert_eq!(app.label("genre-play-Rock").as_deref(), Some("Play Rock"));
    app.click("genre-play-Rock");
    assert_eq!(
        app.queued(),
        [
            ("Mod-key".to_string(), "Mid".to_string()),
            ("Mod-key".to_string(), "Mid".to_string())
        ]
    );
    assert_eq!(
        app.label("view-title").as_deref(),
        Some("Genres"),
        "the play button must not open the genre"
    );
}

#[gpui_kit::test]
async fn song_column_headers_sort_and_persist(cx: &mut TestAppContext) {
    let mut app = open_with(cx, songs_library());
    assert_eq!(
        app.label_of(("song-play", 0usize)).as_deref(),
        Some("Play Bravo"),
        "album order by default"
    );

    app.click("song-col-title");
    assert_eq!(
        app.label_of(("song-play", 0usize)).as_deref(),
        Some("Play Alpha")
    );
    assert_eq!(app.saved("song_sort")["column"], "title");
    assert_eq!(app.saved("song_sort")["descending"], false);

    app.click("song-col-title");
    assert_eq!(
        app.label_of(("song-play", 0usize)).as_deref(),
        Some("Play Charlie")
    );
    assert_eq!(app.saved("song_sort")["descending"], true);

    app.click("song-col-time");
    assert_eq!(app.saved("song_sort")["column"], "time");
}

#[gpui_kit::test]
async fn song_row_play_button_plays_the_list_from_that_song(cx: &mut TestAppContext) {
    let mut app = open_with(cx, songs_library());
    app.click("song-col-title");
    app.hover_on(("song", 1usize));
    app.play_with(|window, cx| window.click(("song-play", 1usize), cx));
    let player = app.view.read_with(app.cx, |view, _| view.player.clone());
    let titles: Vec<String> = player.queue().0.iter().map(|t| t.title.clone()).collect();
    assert_eq!(titles, ["Alpha", "Bravo", "Charlie"]);
    assert_eq!(
        player.last_loaded().map(|t| t.title).as_deref(),
        Some("Bravo"),
        "playback starts at the clicked song"
    );
}

#[gpui_kit::test]
async fn playlists_are_created_filled_renamed_and_deleted(cx: &mut TestAppContext) {
    let mut app = open_with(cx, songs_library());
    app.click("playlist-new");
    app.typing("Road");
    app.press("space");
    app.typing("Trip");
    app.press("enter");
    assert_eq!(app.label("view-title").as_deref(), Some("Road Trip"));
    assert_eq!(app.label("view-detail").as_deref(), Some("No songs"));
    assert_eq!(app.playlists_file()["items"][0]["name"], "Road Trip");
    let id = app.playlists_file()["items"][0]["id"].as_u64().unwrap();
    let sidebar_item = format!("playlist-{id}");

    app.click("nav-songs");
    app.right_click_on(("song", 0usize));
    for _ in 0..3 {
        app.press("down");
    }
    app.press("enter");
    app.press("down");
    app.press("enter");
    let tracks = &app.playlists_file()["items"][0]["tracks"];
    assert_eq!(tracks.as_array().map(Vec::len), Some(1), "{tracks}");

    app.click_on(ElementId::Name(sidebar_item.clone().into()));
    assert_eq!(app.label("view-title").as_deref(), Some("Road Trip"));
    assert_eq!(app.label("view-detail").as_deref(), Some("1 song · 3 min"));

    app.click("playlist-rename");
    app.press("ctrl-a");
    app.typing("Chill");
    app.press("enter");
    assert_eq!(app.label("view-title").as_deref(), Some("Chill"));
    assert_eq!(app.playlists_file()["items"][0]["name"], "Chill");

    app.right_click_on(("song", 0usize));
    for _ in 0..6 {
        app.press("down");
    }
    app.press("enter");
    assert_eq!(app.label("view-detail").as_deref(), Some("No songs"));

    app.click("playlist-delete");
    assert_eq!(
        app.label("playlist-confirm").as_deref(),
        Some("Delete “Chill”?")
    );
    app.click("playlist-delete-cancel");
    assert!(!app.has("playlist-confirm"));
    assert!(app.has(ElementId::Name(sidebar_item.clone().into())));

    app.click("playlist-delete");
    app.click("playlist-delete-confirm");
    assert_eq!(app.label("view-title").as_deref(), Some("Songs"));
    assert!(!app.has(ElementId::Name(sidebar_item.clone().into())));
    assert_eq!(app.playlists_file()["items"], serde_json::json!([]));
}

#[gpui_kit::test]
async fn escape_cancels_a_new_playlist(cx: &mut TestAppContext) {
    let mut app = open_with(cx, songs_library());
    app.click("playlist-new");
    assert!(app.has("playlist-name-row"));
    app.typing("Nope");
    app.press("escape");
    assert!(!app.has("playlist-name-row"));
    assert_eq!(app.label("view-title").as_deref(), Some("Songs"));
    assert_eq!(
        app.playlists_file()["items"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0),
        0
    );
}

#[gpui_kit::test]
async fn forward_returns_to_the_page_left_with_back(cx: &mut TestAppContext) {
    let library = library_of(&[release("k", "Estranhos no Ninho", "Exxótica", 2002)]);
    let mut app = open_with(cx, library);
    app.click("nav-albums");
    app.click_on(("album", 0usize));
    assert!(app.has("album-title"));

    app.press("alt-left");
    assert_eq!(app.label("view-title").as_deref(), Some("Albums"));
    app.press("alt-right");
    assert!(app.has("album-title"), "Alt+Right goes forward");

    app.click("nav-back");
    app.click("nav-forward");
    assert!(app.has("album-title"), "the toolbar button goes forward");

    app.click("nav-back");
    app.click("nav-songs");
    app.click("nav-forward");
    assert_eq!(
        app.label("view-title").as_deref(),
        Some("Songs"),
        "switching sections drops the forward history"
    );
}

#[gpui_kit::test]
async fn up_next_opens_beside_the_page_from_the_toolbar_and_the_bar(cx: &mut TestAppContext) {
    let mut app = open_with(cx, songs_library());
    assert!(!app.has("nav-queue"), "Up Next is a panel, not a section");
    assert!(!app.has("queue-panel"));

    app.click("queue-toggle");
    assert!(app.has("queue-panel"));
    assert_eq!(
        app.label("view-title").as_deref(),
        Some("Songs"),
        "the panel opens beside the page"
    );
    assert!(app.has("queue-empty"), "nothing is queued yet");

    app.hover_on(("song", 0usize));
    app.play_with(|window, cx| window.click(("song-play", 0usize), cx));
    assert!(app.has(("queue-row", 2usize)), "the panel lists the queue");
    assert!(!app.has("queue-empty"));

    app.click("bar-queue");
    assert!(!app.has("queue-panel"), "the bar button closes it");
}

#[gpui_kit::test]
async fn transport_is_centered_above_the_progress_line(cx: &mut TestAppContext) {
    let mut app = open(cx);
    let play = app.bounds("play");
    let progress = app.bounds("progress");
    assert!(
        play.bottom() <= progress.top(),
        "the buttons sit above the line: {play:?} vs {progress:?}"
    );
    let width = app.cx.update(|window, _| window.viewport_size().width);
    let offset = (play.center().x - width / 2.).abs();
    let rem = app.rem();
    assert!(
        offset < rem * 0.5,
        "play is centered in the bar: {play:?} in a window {width:?} wide"
    );
}

fn silent_wav(path: &std::path::Path) {
    let samples = 44_100u32;
    let mut wav = Vec::with_capacity(44 + samples as usize * 2);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + samples * 2).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&samples.to_le_bytes());
    wav.extend_from_slice(&(samples * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(samples * 2).to_le_bytes());
    wav.resize(wav.len() + samples as usize * 2, 0);
    std::fs::write(path, wav).unwrap();
}

#[gpui_kit::test]
async fn liked_songs_fill_their_section_and_persist(cx: &mut TestAppContext) {
    let mut app = open_with(cx, songs_library());
    app.hover_on(("song", 0usize));
    app.click_on(("song-like", 0usize));
    assert_eq!(
        app.liked_file(),
        serde_json::json!(["/music/Mod/Bravo.flac"])
    );

    app.click("nav-liked");
    assert_eq!(app.label("view-title").as_deref(), Some("Liked Songs"));
    assert_eq!(app.label("view-detail").as_deref(), Some("1 song · 3 min"));
    assert_eq!(
        app.label_of(("song-play", 0usize)).as_deref(),
        Some("Play Bravo")
    );

    app.hover_on(("song", 0usize));
    app.click_on(("song-like", 0usize));
    assert_eq!(app.label("view-detail").as_deref(), Some("No songs"));
    assert_eq!(app.liked_file(), serde_json::json!([]));
}

#[gpui_kit::test]
async fn the_bar_heart_likes_the_playing_song(cx: &mut TestAppContext) {
    let dir = std::env::temp_dir().join(format!(
        "omamusic-like-{}-{}",
        std::process::id(),
        NEXT_CONFIG_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("Low Tide.wav");
    silent_wav(&path);
    let library = Arc::new(Library::empty());
    library.insert_song(
        &Track {
            path: path.clone(),
            title: "Low Tide".into(),
            artist: "Neon Harbor".into(),
            album: "Afterglow".into(),
            track_number: 1,
            year: 2020,
            duration: 1.0,
            album_id: "afterglow".into(),
            genre: "Synth-pop".into(),
        },
        1,
    );
    let mut app = open_with(cx, library);
    app.hover_on(("song", 0usize));
    app.play_with(|window, cx| window.click(("song-play", 0usize), cx));
    let player = app.view.read_with(app.cx, |view, _| view.player.clone());
    wait_until("the song to load", || player.current().is_some());
    player.pause();
    app.click_on("like");
    assert_eq!(
        app.liked_file(),
        serde_json::json!([path.to_string_lossy()])
    );
    app.click_on("like");
    assert_eq!(app.liked_file(), serde_json::json!([]));
}

#[gpui_kit::test]
async fn the_playing_album_is_marked_in_the_grid_and_the_list(cx: &mut TestAppContext) {
    let dir = std::env::temp_dir().join(format!(
        "omamusic-now-{}-{}",
        std::process::id(),
        NEXT_CONFIG_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("Low Tide.wav");
    silent_wav(&path);
    let library = Arc::new(Library::empty());
    for (path, album) in [
        (path.clone(), "Afterglow"),
        (dir.join("x.wav"), "Backwater"),
    ] {
        library.insert_song(
            &Track {
                path,
                title: "Low Tide".into(),
                artist: "Neon Harbor".into(),
                album: album.into(),
                track_number: 1,
                year: 2020,
                duration: 1.0,
                album_id: album.to_lowercase(),
                genre: "Synth-pop".into(),
            },
            1,
        );
    }
    let mut app = open_with(cx, library);
    app.click("nav-albums");
    assert!(!app.has("album-now-afterglow"));

    app.hover_on(("album", 0usize));
    app.play_with(|window, cx| window.click("album-play-afterglow", cx));
    let player = app.view.read_with(app.cx, |view, _| view.player.clone());
    wait_until("the album to play", || player.current().is_some());
    app.hover_on("nav-songs");
    assert_eq!(
        app.label("album-now-afterglow").as_deref(),
        Some("Now Playing")
    );
    assert!(!app.has("album-now-backwater"));

    player.pause();
    player.settle();
    assert_eq!(app.label("album-now-afterglow").as_deref(), Some("Paused"));

    app.click("albums-view-list");
    assert!(app.has("album-row-now-afterglow"));
    assert!(!app.has("album-row-now-backwater"));
}

#[gpui_kit::test]
async fn a_narrow_toolbar_never_overlaps_its_title_and_controls(cx: &mut TestAppContext) {
    let library = library_of(&[release("k", "Estranhos no Ninho", "Exxótica", 2002)]);
    let mut app = open_with(cx, library);
    app.click("nav-albums");
    app.cx.simulate_resize(gpui_kit::size(px(820.), px(600.)));
    app.cx.run_until_parked();
    let title = app.bounds("view-title");
    let detail = app.bounds("view-detail");
    let sort = app.bounds("album-sort");
    assert!(
        title.right() <= detail.left() + px(0.5),
        "the count sits after the title: {title:?} {detail:?}"
    );
    assert!(
        detail.right() <= sort.left(),
        "the count must not run under the sort button: {detail:?} vs {sort:?}"
    );
    assert!(
        title.size.width > px(0.),
        "the title keeps some room: {title:?}"
    );
}

#[gpui_kit::test]
async fn the_sidebar_collapses_to_icons_and_remembers_it(cx: &mut TestAppContext) {
    let library = library_of(&[release("k", "Estranhos no Ninho", "Exxótica", 2002)]);
    let mut app = open_with(cx, library);
    let rem = app.rem();
    let wide = app.bounds("nav-albums");
    assert!(wide.size.width > rem * 8., "labels show: {wide:?}");

    app.click("sidebar-toggle");
    app.cx.executor().advance_clock(Duration::from_millis(400));
    app.cx.run_until_parked();
    let narrow = app.bounds("nav-albums");
    assert!(
        narrow.size.width < rem * 3.,
        "only the icon is left: {narrow:?}"
    );
    assert_eq!(app.label("nav-albums").as_deref(), Some("Albums"));
    assert_eq!(app.saved("sidebar_collapsed"), true);

    app.click("nav-albums");
    assert_eq!(
        app.label("view-title").as_deref(),
        Some("Albums"),
        "an icon still navigates"
    );

    app.click("sidebar-toggle");
    app.cx.executor().advance_clock(Duration::from_millis(400));
    app.cx.run_until_parked();
    assert!(app.bounds("nav-albums").size.width > rem * 8.);
    assert_eq!(app.saved("sidebar_collapsed"), false);
}

#[gpui_kit::test]
async fn search_shrinks_with_the_window_then_folds_into_a_button(cx: &mut TestAppContext) {
    let mut app = open_with(cx, songs_library());
    let rem = app.rem();
    assert!(app.bounds("search").size.width >= rem * 15. - px(1.));

    app.cx.simulate_resize(gpui_kit::size(px(820.), px(600.)));
    app.cx.run_until_parked();
    let search = app.bounds("search");
    let rescan = app.bounds("rescan");
    assert!(
        search.size.width < rem * 15. && search.size.width >= rem * 6. - px(1.),
        "the field gives up width first: {search:?}"
    );
    assert!(search.right() <= rescan.left(), "{search:?} vs {rescan:?}");

    app.cx.simulate_resize(gpui_kit::size(px(640.), px(600.)));
    app.cx.run_until_parked();
    assert!(!app.has("search"), "too narrow for a field");
    app.click("search-open");
    assert!(app.has("search"), "the button opens the field");
    app.typing("Bra");
    assert_eq!(app.search_text(), "Bra");
}
