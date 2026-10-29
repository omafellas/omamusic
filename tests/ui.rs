//! UI smoke tests. Each test drives the production `Root` view in a headless
//! window with the real key bindings and asserts the visible outcome.
use gpui_kit::test::TestWindowExt;
use gpui_kit::{px, Entity, Pixels, TestAppContext, VisualTestContext};
use omamusic::lookup::http::FakeHttp;
use omamusic::models::AlbumSummary;
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
        omamusic::init_actions(&library, &player, cx);
    });
    let (root, cx) = cx.add_window_view(|window, cx| Root::new(library, player, window, cx));
    cx.run_until_parked();
    let view = root.read_with(cx, |root, _| root.main().clone());
    assert_eq!(view.read_with(cx, |view, _| view.ui_scale()), 1.5);
    assert_eq!(cx.update(|window, _| window.rem_size()), px(24.0));
}
