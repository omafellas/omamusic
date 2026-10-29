//! Pixel smoke tests. The production `Root` view is drawn with GPUI's headless
//! WGPU renderer and the captured image is checked, so a wrong or missing paint
//! is caught even when layout and state look right. Linux only; it needs a
//! Vulkan adapter (Mesa lavapipe is enough). Set `RENDERING_REQUIRED=1` to fail
//! instead of skipping when no renderer is available, and `RENDERING_DUMP_DIR`
//! to save every capture as PNG.
fn main() {
    #[cfg(target_os = "linux")]
    pixels::run();
    #[cfg(not(target_os = "linux"))]
    println!("rendering: skipped; only Linux is supported");
}

#[cfg(target_os = "linux")]
mod pixels {
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{
        px, size, AnyWindowHandle, AppContext, Bounds, Entity, HeadlessAppContext, Pixels,
    };
    use omamusic::models::AlbumSummary;
    use omamusic::{AppConfig, Library, Omamusic, Player, Root};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    const WINDOW: (f32, f32) = (1020., 680.);

    fn renderer_available() -> Result<bool, String> {
        match gpui_kit::platform::current_headless_renderer() {
            Ok(Some(_)) => Ok(true),
            Ok(None) => Ok(false),
            Err(err) => Err(format!("{err:#}")),
        }
    }

    fn isolated_config_dir() -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("omamusic-px-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create config dir");
        AppConfig::use_dir(&dir);
        dir
    }

    struct Scene {
        cx: HeadlessAppContext,
        handle: AnyWindowHandle,
        view: Entity<Omamusic>,
    }

    impl Scene {
        fn open() -> Self {
            Self::open_with(Arc::new(Library::empty()))
        }

        fn open_with(library: Arc<Library>) -> Self {
            isolated_config_dir();
            let mut cx = HeadlessAppContext::with_platform(
                gpui_kit::platform::current_platform(true).text_system(),
                Arc::new(gpui_kit::assets::AllAssets),
                gpui_kit::platform::current_headless_renderer,
            );
            let player = Arc::new(Player::new_test());
            cx.update(|cx| {
                gpui_omarchy::init(cx);
                omamusic::init_actions(&library, &player, cx);
            });
            let root = cx
                .open_window(size(px(WINDOW.0), px(WINDOW.1)), |window, cx| {
                    cx.new(|cx| Root::new(library, player, window, cx))
                })
                .expect("open headless window");
            let handle: AnyWindowHandle = root.into();
            let view = cx.update(|cx| root.read(cx).map(|r| r.main().clone()).unwrap());
            cx.run_until_parked();
            cx.update_window(handle, |_, window, cx| window.render_frame(cx))
                .unwrap();
            Self { cx, handle, view }
        }

        fn act(&mut self, f: impl FnOnce(&mut gpui_kit::Window, &mut gpui_kit::App)) {
            self.cx
                .update_window(self.handle, |_, window, cx| f(window, cx))
                .unwrap();
            self.cx.run_until_parked();
            self.cx
                .update_window(self.handle, |_, window, cx| window.render_frame(cx))
                .unwrap();
        }

        fn bounds(&mut self, id: &'static str) -> Bounds<Pixels> {
            self.cx
                .update_window(self.handle, |_, window, _| window.find(id).bounds())
                .unwrap()
        }

        fn scale(&mut self) -> f64 {
            self.cx.update(|cx| self.view.read(cx).ui_scale())
        }

        fn capture(&mut self) -> Capture {
            Capture::take(&mut self.cx, self.handle)
        }
    }

    /// A window capture as raw RGBA rows.
    struct Capture {
        width: u32,
        height: u32,
        scale: f32,
        raw: Vec<u8>,
    }

    impl Capture {
        fn take(cx: &mut HeadlessAppContext, handle: AnyWindowHandle) -> Self {
            let scale = cx
                .update_window(handle, |_, window, _| window.scale_factor())
                .unwrap();
            let image = cx.capture_screenshot(handle).expect("capture screenshot");
            if let Ok(dir) = std::env::var("RENDERING_DUMP_DIR") {
                static SHOT: AtomicUsize = AtomicUsize::new(0);
                let n = SHOT.fetch_add(1, Ordering::Relaxed);
                std::fs::create_dir_all(&dir).expect("create RENDERING_DUMP_DIR");
                image.save(format!("{dir}/shot-{n:02}.png")).unwrap();
            }
            let (width, height) = image.dimensions();
            Self {
                width,
                height,
                scale,
                raw: image.into_raw(),
            }
        }

        fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
            let i = ((y * self.width + x) * 4) as usize;
            [
                self.raw[i],
                self.raw[i + 1],
                self.raw[i + 2],
                self.raw[i + 3],
            ]
        }

        fn background(&self) -> [u8; 4] {
            self.pixel(0, 0)
        }

        fn device(&self, v: Pixels) -> u32 {
            (f32::from(v) * self.scale).round() as u32
        }

        /// Pixels inside `bounds` that differ from the window background.
        fn ink_in(&self, bounds: Bounds<Pixels>) -> usize {
            let bg = self.background();
            let (x0, y0) = (self.device(bounds.left()), self.device(bounds.top()));
            let (x1, y1) = (
                self.device(bounds.right()).min(self.width),
                self.device(bounds.bottom()).min(self.height),
            );
            let mut count = 0;
            for y in y0..y1 {
                for x in x0..x1 {
                    if self.pixel(x, y) != bg {
                        count += 1;
                    }
                }
            }
            count
        }

        fn changed_in(&self, other: &Capture, bounds: Bounds<Pixels>) -> usize {
            let (x0, y0) = (self.device(bounds.left()), self.device(bounds.top()));
            let (x1, y1) = (
                self.device(bounds.right()).min(self.width),
                self.device(bounds.bottom()).min(self.height),
            );
            let mut count = 0;
            for y in y0..y1 {
                for x in x0..x1 {
                    if self.pixel(x, y) != other.pixel(x, y) {
                        count += 1;
                    }
                }
            }
            count
        }

        fn ink(&self) -> usize {
            self.ink_in(Bounds::new(
                Default::default(),
                size(
                    px(self.width as f32 / self.scale),
                    px(self.height as f32 / self.scale),
                ),
            ))
        }
    }

    fn window_is_drawn_and_deterministic() {
        let first = Scene::open().capture();
        let second = Scene::open().capture();
        assert_eq!((first.width, first.height), (second.width, second.height));
        assert!(first.ink() > 0, "the window must not be blank");
        assert!(
            first.raw == second.raw,
            "the same view must render identically twice"
        );
    }

    fn ctrl_equal_draws_larger_text() {
        let mut scene = Scene::open();
        let before = scene.capture();
        let label_before = scene.bounds("nav-songs");
        scene.act(|window, cx| window.press("ctrl-=", cx));
        scene.act(|window, cx| window.press("ctrl-=", cx));
        assert_eq!(scene.scale(), 1.2);
        let after = scene.capture();
        let label_after = scene.bounds("nav-songs");
        assert!(before.raw != after.raw, "a larger text size must repaint");
        assert!(
            label_after.size.height > label_before.size.height,
            "navigation rows grow with the text: {label_before:?} -> {label_after:?}"
        );
        assert!(
            after.ink_in(label_after) > before.ink_in(label_before),
            "larger text must put more ink in its row"
        );
        scene.act(|window, cx| window.press("ctrl-0", cx));
        let reset = scene.capture();
        assert!(
            reset.raw == before.raw,
            "Ctrl+0 must restore the original frame"
        );
    }

    fn settings_draws_the_text_size_row() {
        let mut scene = Scene::open();
        scene.act(|window, cx| window.click("settings", cx));
        let capture = scene.capture();
        for id in ["font-smaller", "font-reset", "font-larger"] {
            let bounds = scene.bounds(id);
            assert!(
                capture.ink_in(bounds) > 0,
                "`{id}` has bounds {bounds:?} but nothing painted inside them"
            );
        }
        let reset = scene.bounds("font-reset");
        let before = capture.ink_in(reset);
        scene.act(|window, cx| window.click("font-larger", cx));
        assert_eq!(scene.scale(), 1.1);
        let changed = scene.capture();
        assert!(
            changed.ink_in(scene.bounds("font-reset")) != before,
            "the percentage label must repaint when the size changes"
        );
    }

    fn unmatched_badge_paints_ink() {
        let summary = |unmatched: bool| AlbumSummary {
            id: "k".to_string(),
            title: "Estranhos no Ninho".to_string(),
            artist: "Exxótica".to_string(),
            year: 2002,
            track_count: 1,
            unmatched,
        };
        let library = Arc::new(Library::empty());
        library.insert_album(&summary(true));
        let mut scene = Scene::open_with(library);
        scene.act(|window, cx| window.click("nav-albums", cx));
        let capture = scene.capture();
        let bounds = scene.bounds("album-unmatched-k");

        let library = Arc::new(Library::empty());
        library.insert_album(&summary(false));
        let mut plain = Scene::open_with(library);
        plain.act(|window, cx| window.click("nav-albums", cx));
        let plain = plain.capture();
        assert!(
            capture.changed_in(&plain, bounds) > 0,
            "the badge must paint over the cover at {bounds:?}"
        );
    }

    fn hovering_an_album_paints_its_play_button() {
        let library = Arc::new(Library::empty());
        library.insert_album(&AlbumSummary {
            id: "k".to_string(),
            title: "Estranhos no Ninho".to_string(),
            artist: "Exxótica".to_string(),
            year: 2002,
            track_count: 1,
            unmatched: false,
        });
        let mut scene = Scene::open_with(library);
        scene.act(|window, cx| window.click("nav-albums", cx));
        let resting = scene.capture();
        let button = scene.bounds("album-play-k");
        scene.act(|window, cx| window.hover(("album", 0usize), cx));
        let hovered = scene.capture();
        assert!(
            hovered.changed_in(&resting, button) > 0,
            "hovering the card must reveal the play button at {button:?}"
        );
        let elsewhere = scene.bounds("nav-songs");
        assert_eq!(
            hovered.changed_in(&resting, elsewhere),
            0,
            "the hover must not repaint the sidebar"
        );
    }

    pub fn run() {
        match renderer_available() {
            Ok(true) => {}
            Ok(false) | Err(_) if std::env::var_os("RENDERING_REQUIRED").is_none() => {
                let why = renderer_available().err().unwrap_or_default();
                println!("rendering: skipped; no headless renderer available {why}");
                return;
            }
            Ok(false) => panic!("rendering: RENDERING_REQUIRED is set but no renderer exists"),
            Err(err) => {
                panic!("rendering: RENDERING_REQUIRED is set but the renderer failed: {err}")
            }
        }
        let tests: [(&str, fn()); 5] = [
            (
                "window_is_drawn_and_deterministic",
                window_is_drawn_and_deterministic,
            ),
            ("ctrl_equal_draws_larger_text", ctrl_equal_draws_larger_text),
            (
                "settings_draws_the_text_size_row",
                settings_draws_the_text_size_row,
            ),
            ("unmatched_badge_paints_ink", unmatched_badge_paints_ink),
            (
                "hovering_an_album_paints_its_play_button",
                hovering_an_album_paints_its_play_button,
            ),
        ];
        for (name, test) in tests {
            println!("running {name}");
            test();
            println!("passed {name}");
        }
        println!("rendering: {} passed", tests.len());
    }
}
