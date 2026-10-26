use crate::config::AppConfig;
use crate::library::{Library, LibrarySnapshot};
use crate::models::{Album, AlbumSummary, ArtistEntry, PlayerState, Section, Track};
use crate::player::Player;
use crate::{FocusSearch, GoBack};
use gpui_kit::base::input::InputState;
use gpui_kit::base::{Popover, Selectable};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    div, px, rems, uniform_list, Animation, AnimationExt as _, AnyElement, App, AppContext,
    ClickEvent, Context, Div, ElementId, Entity, EntityInputHandler, Focusable, ImageSource,
    InteractiveElement, IntoElement, MouseButton, NavigationDirection, ParentElement,
    PathPromptOptions, Render, Stateful, StatefulInteractiveElement, StyleRefinement, Styled,
    Window,
};
use gpui_omarchy::{
    avatar, avatar_image, button, empty_state, focus_scope, icon, icon_button, input, menu, panel,
    separator, switch, with_tooltip, ActiveTheme, ButtonVariant, IconName, MenuItem,
};
use rand::Rng;
use std::collections::BTreeSet;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub struct Omamusic {
    pub library: Arc<Library>,
    pub player: Arc<Player>,
    section: Section,
    query: Entity<InputState>,
    pages: Vec<Page>,
    selection: BTreeSet<usize>,
    anchor: Option<usize>,
    show_settings: bool,
    lib_version: u64,
    prefs: AppConfig,
    new_folder: Entity<InputState>,
    rescan_notify: Option<(String, f32)>,
    /// Bumped for every toast; makes an older fade task ignore a replaced toast.
    notify_seq: u64,
    prev_scanning: bool,
    /// Music-reactive bottom strip; see `Visualizer`.
    visualizer: Entity<Visualizer>,
    /// Latest text typed into the search field, applied after the debounce.
    pending_query: String,
    /// The in-flight debounce task; replacing it cancels the previous one.
    search_task: Option<gpui_kit::Task<()>>,
    /// True while a debounced search is waiting to run (shows the spinner).
    searching: bool,
    /// Keeps bare character keystrokes reaching the text fields; see `new`.
    _typing_guard: gpui_kit::Subscription,
}

#[derive(Clone, PartialEq)]
enum Page {
    Album(String),
    Artist(String),
}

/// Where a row's playback list comes from. Rows themselves are paged from
/// SQLite, so actions resolve the full list lazily instead of holding it.
#[derive(Clone)]
enum TrackList {
    /// Every track in the current Songs view (filtered or not).
    Songs,
    Album(String),
    Artist(String),
    /// A list already in memory (album detail, play queue).
    Loaded(Arc<[Track]>),
}

impl TrackList {
    fn tracks(&self, library: &Library) -> Vec<Track> {
        match self {
            TrackList::Songs => library.tracks_for_play(),
            TrackList::Album(key) => library.album_tracks(key),
            TrackList::Artist(name) => library.artist_tracks(name),
            TrackList::Loaded(tracks) => tracks.to_vec(),
        }
    }

    fn get(&self, library: &Library, index: usize) -> Option<Track> {
        match self {
            TrackList::Songs => library.track_at(index),
            TrackList::Album(key) => library.album_tracks(key).get(index).cloned(),
            TrackList::Artist(name) => library.artist_tracks(name).get(index).cloned(),
            TrackList::Loaded(tracks) => tracks.get(index).cloned(),
        }
    }
}

fn selection_tracks(library: &Library, list: &TrackList, rows: &[usize]) -> Vec<Track> {
    rows.iter().filter_map(|&i| list.get(library, i)).collect()
}

#[derive(Clone, Copy)]
enum RowAction {
    Play,
    PlayNext,
    AddToQueue,
    GoToAlbum,
    GoToArtist,
    Remove,
    ExportFile,
}

const TRACK_ACTIONS: &[RowAction] = &[
    RowAction::Play,
    RowAction::PlayNext,
    RowAction::AddToQueue,
    RowAction::GoToAlbum,
    RowAction::GoToArtist,
    RowAction::ExportFile,
];
const QUEUE_ACTIONS: &[RowAction] = &[
    RowAction::Play,
    RowAction::GoToAlbum,
    RowAction::GoToArtist,
    RowAction::Remove,
];
const ALBUM_ACTIONS: &[RowAction] = &[
    RowAction::Play,
    RowAction::PlayNext,
    RowAction::AddToQueue,
    RowAction::GoToArtist,
];
const ARTIST_ACTIONS: &[RowAction] = &[RowAction::Play, RowAction::PlayNext, RowAction::AddToQueue];

/// Wait this long after the last keystroke before running the search, so a
/// burst of typing results in a single BM25 query.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(180);

/// The scan-completion toast stays fully visible this long, then fades over
/// `NOTIFY_FADE` (one step every `NOTIFY_FADE_STEP`), gone after 3s total.
const NOTIFY_HOLD: Duration = Duration::from_millis(2600);
const NOTIFY_FADE: Duration = Duration::from_millis(400);
const NOTIFY_FADE_STEP: Duration = Duration::from_millis(50);

/// Number of equalizer bars in the "Visual Effects" background animation.
const VISUALIZER_BARS: usize = 40;

/// Song-list layout priority: the title flexes at this rate while the artist
/// column flexes at 1 and caps at `SECONDARY_MAX_W` rems, so a narrow window
/// crops the artist rather than the song name.
const TITLE_GROW: f32 = 3.;
const SECONDARY_MAX_W: f32 = 10.;

/// Fraction of the song row's flexible space the title claims when
/// `secondary` columns compete for it at grow factor 1.
#[cfg(test)]
fn title_flex_share(secondary: usize) -> f32 {
    TITLE_GROW / (TITLE_GROW + secondary as f32)
}

impl RowAction {
    fn group(self) -> u8 {
        match self {
            RowAction::Play | RowAction::PlayNext | RowAction::AddToQueue => 0,
            RowAction::GoToAlbum | RowAction::GoToArtist => 1,
            RowAction::Remove => 2,
            RowAction::ExportFile => 3,
        }
    }

    fn item(self) -> MenuItem {
        let (label, icon_name) = match self {
            RowAction::Play => ("Play", IconName::Play),
            RowAction::PlayNext => ("Play Next", IconName::ListStart),
            RowAction::AddToQueue => ("Add to Queue", IconName::ListEnd),
            RowAction::GoToAlbum => ("Go to Album", IconName::Disc3),
            RowAction::GoToArtist => ("Go to Artist", IconName::Mic),
            RowAction::Remove => ("Remove from Queue", IconName::ListX),
            RowAction::ExportFile => ("Export File…", IconName::Download),
        };
        MenuItem::new(label).icon(icon_name)
    }
}

struct MenuTrigger(AnyElement);

impl IntoElement for MenuTrigger {
    type Element = AnyElement;

    fn into_element(self) -> AnyElement {
        self.0
    }
}

impl Selectable for MenuTrigger {
    fn selected(self, _: bool) -> Self {
        self
    }

    fn is_selected(&self) -> bool {
        false
    }
}

pub fn cover(
    library: &Library,
    album_id: Option<&str>,
    size: f32,
    name: &str,
    cx: &App,
) -> impl IntoElement {
    let cover = avatar(initials(name), cx).size(px(size));
    match album_id.and_then(|id| library.covers().cached(id, size as u32 * 2)) {
        Some(image) => cover.image(avatar_image(ImageSource::Image(image))),
        None => cover,
    }
}

fn initials(text: &str) -> String {
    text.chars().take(2).collect()
}

pub fn format_time(secs: f64) -> String {
    if !secs.is_finite() || secs <= 0.0 {
        return "0:00".into();
    }
    let t = secs as u64;
    let m = t / 60;
    let s = t % 60;
    if m >= 60 {
        format!("{}:{:02}:{:02}", m / 60, m % 60, s)
    } else {
        format!("{m}:{s:02}")
    }
}

/// The library size line shown in the sidebar and the scan-completion toast,
/// with the song count leading so it reads well at small widths.
fn library_counts_text(albums: usize, tracks: usize) -> String {
    format!("{} songs · {} albums", tracks, albums)
}

/// Equalizer bar shape over one animation cycle, 0.0..=1.0. The two mixed
/// sine waves use whole cycles per animation loop, so repeating never jumps.
fn visualizer_shape(bar: usize, delta: f32) -> f32 {
    let tau = std::f32::consts::TAU;
    let fast = (delta * tau * (1 + (bar % 4)) as f32 + bar as f32 * 0.7).sin() * 0.5 + 0.5;
    let slow = (delta * tau * (2 + (bar % 3)) as f32 + bar as f32 * 1.3).sin() * 0.5 + 0.5;
    ((0.6 * fast + 0.4 * slow) * (0.55 + 0.45 * (bar % 5) as f32 / 4.)).clamp(0.0, 1.0)
}

/// Bar height in rems for a loudness level. Silence keeps a thin baseline;
/// bars grow vertically as the music gets louder.
fn visualizer_height(level: f32, delta: f32, bar: usize) -> f32 {
    0.12 + level * visualizer_shape(bar, delta) * 2.4
}

/// Low alpha so the strip never blocks the text behind it.
fn visualizer_opacity(level: f32, delta: f32, bar: usize) -> f32 {
    0.08 + 0.34 * level * visualizer_shape(bar, delta)
}

/// Music-reactive strip of bars pinned to the bottom of the main area, just
/// above the player bar. Only rendered while "Visual Effects" is enabled;
/// heights follow the audio level meter of the current output.
struct Visualizer {
    player: Arc<Player>,
}

impl Visualizer {
    fn new(player: Arc<Player>, cx: &mut Context<Self>) -> Self {
        // Repaint often enough to follow the meter while playing, and idle
        // otherwise so a paused track costs nothing.
        cx.spawn(async move |this, cx| loop {
            let delay = this
                .update(cx, |this, _| {
                    if this.player.state() == PlayerState::Playing {
                        Duration::from_millis(120)
                    } else {
                        Duration::from_millis(500)
                    }
                })
                .unwrap_or(Duration::from_millis(500));
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |_, cx| cx.notify());
        })
        .detach();
        Self { player }
    }
}

impl Render for Visualizer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.omarchy().clone();
        let level = self.player.level();
        div()
            .size_full()
            .flex()
            .items_end()
            .gap(px(3.))
            .px(rems(1.5))
            .children((0..VISUALIZER_BARS).map(|bar| {
                div()
                    .flex_1()
                    .min_w_0()
                    .h(rems(visualizer_height(level, 0.0, bar)))
                    .rounded_t(px(2.))
                    .with_animation(
                        ElementId::Name(format!("visualizer-bar-{bar}").into()),
                        Animation::new(Duration::from_millis(900 + (bar % 5) as u64 * 210))
                            .repeat(),
                        move |el, delta| {
                            el.h(rems(visualizer_height(level, delta, bar)))
                                .bg(theme.accent.opacity(visualizer_opacity(level, delta, bar)))
                        },
                    )
            }))
    }
}

impl Omamusic {
    pub fn new(
        library: Arc<Library>,
        player: Arc<Player>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("Search… (Ctrl+F)"));
        let new_folder = cx.new(|cx| InputState::new(window, cx).placeholder("Enter folder path"));

        // On Linux the platform only hands a printable character to the focused field once the
        // keystroke survives key-binding dispatch, and `prefer_character_input` is never set there.
        // A bare space otherwise matches the global `TogglePlay` binding and is consumed before it
        // can be typed. Feed such keystrokes straight to whichever of our fields has focus — the
        // same `replace_text_in_range` call the platform would have made — and stop dispatch so the
        // binding stays out of the way while typing.
        let search = query.clone();
        let folder = new_folder.clone();
        let search_focus = query.read(cx).focus_handle(cx);
        let folder_focus = new_folder.read(cx).focus_handle(cx);
        let _typing_guard = cx.intercept_keystrokes(move |event, window, cx| {
            let keystroke = &event.keystroke;
            let modifiers = &keystroke.modifiers;
            if modifiers.control || modifiers.alt || modifiers.platform {
                return;
            }
            let Some(text) = keystroke.key_char.clone() else {
                return;
            };
            let field = if search_focus.is_focused(window) {
                &search
            } else if folder_focus.is_focused(window) {
                &folder
            } else {
                return;
            };
            cx.stop_propagation();
            field.update(cx, |input, cx| {
                input.replace_text_in_range(None, &text, window, cx);
            });
        });

        // Debounce the search so a burst of keystrokes runs a single query.
        cx.observe(&query, |this, _, cx| this.schedule_search(cx))
            .detach();

        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(500))
                .await;
            let alive = this.update(cx, |this, cx| this.tick(false, cx)).is_ok();
            if !alive {
                break;
            }
        })
        .detach();

        let changes = player.subscribe();
        cx.spawn(async move |this, cx| {
            while changes.recv().await.is_ok() {
                let alive = this.update(cx, |this, cx| this.tick(true, cx)).is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();

        let covers = library.covers().subscribe();
        cx.spawn(async move |this, cx| {
            while covers.recv().await.is_ok() {
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        })
        .detach();

        let visualizer = cx.new(|cx| Visualizer::new(player.clone(), cx));

        let view = std::env::var("OMADRIVE_VIEW").ok();
        let (section, pages, show_settings) = match view.as_deref() {
            Some("artists") => (Section::Artists, Vec::new(), false),
            Some("songs") => (Section::Songs, Vec::new(), false),
            Some("queue") => (Section::Queue, Vec::new(), false),
            Some("settings") => (Section::Albums, Vec::new(), true),
            Some("album") => {
                let first = library.first_album_key();
                (
                    Section::Albums,
                    first.map(Page::Album).into_iter().collect(),
                    false,
                )
            }
            _ => (Section::Songs, Vec::new(), false),
        };

        Self {
            library,
            player,
            section,
            query,
            pages,
            selection: BTreeSet::new(),
            anchor: None,
            show_settings,
            lib_version: 0,
            prefs: AppConfig::load(),
            new_folder,
            rescan_notify: None,
            notify_seq: 0,
            prev_scanning: false,
            visualizer,
            pending_query: String::new(),
            search_task: None,
            searching: false,
            _typing_guard,
        }
    }

    fn tick(&mut self, force: bool, cx: &mut Context<Self>) {
        let version = self.library.version();
        if !force && version == self.lib_version {
            return;
        }
        if version != self.lib_version {
            self.clear_selection();
        }

        // Detect scan completion: scanning went from true to false
        let snap = self.library.snapshot();
        let scanning_now = snap.scanning;
        let notify_msg = if self.prev_scanning && !scanning_now {
            Some(library_counts_text(snap.album_count, snap.track_count))
        } else {
            None
        };
        drop(snap);
        self.prev_scanning = scanning_now;
        if let Some(msg) = notify_msg {
            self.show_notify(msg, cx);
        }

        self.lib_version = version;
        cx.notify();
    }

    fn show_notify(&mut self, msg: String, cx: &mut Context<Self>) {
        self.notify_seq += 1;
        let seq = self.notify_seq;
        self.rescan_notify = Some((msg, 1.0));
        cx.notify();
        // Hold at full opacity, then fade out step by step: gpui repaints
        // `opacity` immediately, so the steps themselves are the animation.
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NOTIFY_HOLD).await;
            let steps = (NOTIFY_FADE.as_millis() / NOTIFY_FADE_STEP.as_millis()).max(1);
            for step in 1..=steps {
                let _ = this.update(cx, |this, cx| {
                    if this.notify_seq == seq {
                        if let Some(entry) = this.rescan_notify.as_mut() {
                            entry.1 = 1. - step as f32 / steps as f32;
                        }
                        cx.notify();
                    }
                });
                cx.background_executor().timer(NOTIFY_FADE_STEP).await;
            }
            let _ = this.update(cx, |this, cx| {
                if this.notify_seq == seq {
                    this.rescan_notify = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub fn show_album(&mut self, album_id: String, cx: &mut Context<Self>) {
        self.navigate(Page::Album(album_id), cx);
    }

    fn cover(&self, album_id: Option<&str>, size: f32, name: &str, cx: &App) -> impl IntoElement {
        cover(&self.library, album_id, size, name, cx)
    }

    fn navigate(&mut self, page: Page, cx: &mut Context<Self>) {
        if self.pages.last() != Some(&page) {
            self.pages.push(page);
        }
        self.clear_selection();
        cx.notify();
    }

    fn back(&mut self, cx: &mut Context<Self>) {
        self.pages.pop();
        self.clear_selection();
        cx.notify();
    }

    fn set_section(&mut self, section: Section, cx: &mut Context<Self>) {
        self.section = section;
        self.pages.clear();
        self.clear_selection();
        cx.notify();
    }

    fn clear_selection(&mut self) {
        self.selection.clear();
        self.anchor = None;
    }

    fn select_row(&mut self, row: usize, event: &ClickEvent) -> bool {
        let m = event.modifiers();
        let toggle = m.control || m.platform;
        match self.anchor {
            Some(anchor) if m.shift => {
                if !toggle {
                    self.selection.clear();
                }
                self.selection.extend(anchor.min(row)..=anchor.max(row));
                return false;
            }
            _ if toggle => {
                if !self.selection.remove(&row) {
                    self.selection.insert(row);
                }
            }
            _ => {
                self.selection = BTreeSet::from([row]);
            }
        }
        self.anchor = Some(row);
        event.click_count() >= 2 && !toggle
    }

    fn track_action(
        &mut self,
        action: RowAction,
        list: &TrackList,
        row: usize,
        queue: bool,
        cx: &mut Context<Self>,
    ) {
        let rows: Vec<usize> = if self.selection.contains(&row) {
            self.selection.iter().copied().collect()
        } else {
            vec![row]
        };
        let tracks = selection_tracks(&self.library, list, &rows);
        match action {
            RowAction::Play if queue => self.player.jump(row),
            RowAction::Play if tracks.len() > 1 => self.player.play(tracks, 0),
            RowAction::Play => {
                let all = list.tracks(&self.library);
                self.player.play(all, row);
            }
            RowAction::PlayNext => self.player.play_next(tracks),
            RowAction::AddToQueue => self.player.enqueue(tracks),
            RowAction::GoToAlbum => {
                if let Some(track) = list.get(&self.library, row) {
                    self.navigate(Page::Album(track.album_id), cx);
                }
            }
            RowAction::GoToArtist => {
                if let Some(track) = list.get(&self.library, row) {
                    self.navigate(Page::Artist(track.artist), cx);
                }
            }
            RowAction::Remove => {
                self.player.remove(&rows);
                self.clear_selection();
            }
            RowAction::ExportFile => {
                if tracks.len() == 1 {
                    let track = tracks[0].clone();
                    let this = cx.entity().downgrade();
                    cx.spawn(async move |_, cx| {
                        let _ = this.update(cx, |this, cx| this.export_track(track, cx));
                    })
                    .detach();
                }
            }
        }
        cx.notify();
    }

    fn export_track(&mut self, track: Track, cx: &mut Context<Self>) {
        let path = track.path.clone();
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("track");
        let download_dir =
            dirs::download_dir().unwrap_or_else(|| dirs::home_dir().unwrap_or_default());
        let _default_path = download_dir.join(file_name);
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Export File".into()),
        });
        cx.spawn(async move |_, _cx| {
            if let Ok(Ok(Some(mut paths))) = picked.await {
                if let Some(dest) = paths.pop() {
                    if let Err(e) = std::fs::copy(&path, &dest) {
                        eprintln!("export file: {e}");
                    }
                }
            }
        })
        .detach();
    }

    fn tracks_action(&mut self, action: RowAction, list: &TrackList, cx: &mut Context<Self>) {
        let tracks = list.tracks(&self.library);
        match action {
            RowAction::Play => self.player.play(tracks, 0),
            RowAction::PlayNext => self.player.play_next(tracks),
            RowAction::AddToQueue => self.player.enqueue(tracks),
            RowAction::GoToAlbum
            | RowAction::GoToArtist
            | RowAction::Remove
            | RowAction::ExportFile => {}
        }
        cx.notify();
    }

    fn context_menu(
        id: impl Into<ElementId>,
        trigger: impl IntoElement,
        actions: &'static [RowAction],
        on_select: impl Fn(&mut Self, RowAction, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Popover {
        let this = cx.entity().downgrade();
        let items = actions
            .iter()
            .enumerate()
            .map(|(i, action)| {
                let item = action.item();
                if i > 0 && actions[i - 1].group() != action.group() {
                    item.separator_before()
                } else {
                    item
                }
            })
            .collect();
        menu(
            id,
            MenuTrigger(trigger.into_any_element()),
            items,
            move |index, _, cx| {
                let _ = this.update(cx, |this, cx| on_select(this, actions[index], cx));
            },
        )
        .mouse_button(MouseButton::Right)
    }

    fn track_row(
        &self,
        id: &'static str,
        row: usize,
        list: TrackList,
        queue: bool,
        content: Stateful<Div>,
        cx: &mut Context<Self>,
    ) -> Popover {
        let theme = cx.omarchy().clone();
        let click_list = list.clone();
        let content = content
            .when(self.selection.contains(&row), |d| {
                d.bg(theme.selected_fill())
            })
            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                if this.select_row(row, event) {
                    this.track_action(RowAction::Play, &click_list, row, queue, cx);
                }
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, _, cx| {
                    if !this.selection.contains(&row) {
                        this.selection = BTreeSet::from([row]);
                        this.anchor = Some(row);
                        cx.notify();
                    }
                }),
            );
        let actions = if queue { QUEUE_ACTIONS } else { TRACK_ACTIONS };
        Self::context_menu(
            (id, row),
            content,
            actions,
            move |this, action, cx| this.track_action(action, &list, row, queue, cx),
            cx,
        )
        .w_full()
    }

    /// Apply `text` to the library. Shared by the immediate path used in tests
    /// and the debounced path used by the view.
    fn apply_text(&mut self, text: String, cx: &mut Context<Self>) {
        if text != self.library.query() {
            self.library.set_query(text.clone());
            self.pages.clear();
            self.clear_selection();
            // Search always filters all songs, switch to Songs section.
            if !text.trim().is_empty() && self.section != Section::Songs {
                self.section = Section::Songs;
            }
        }
        cx.notify();
    }

    /// Push the field's current text into the library immediately. Used by the
    /// view tests; the running view debounces through [`Self::schedule_search`].
    #[cfg(test)]
    fn apply_query(&mut self, cx: &mut Context<Self>) {
        let text = self.query.read(cx).text().to_string();
        self.apply_text(text, cx);
    }

    /// Schedule a search after the debounce interval. Replacing the stored task
    /// cancels the previous one, so only the last keystroke is searched.
    fn schedule_search(&mut self, cx: &mut Context<Self>) {
        let text = self.query.read(cx).text().to_string();
        if text == self.pending_query {
            return;
        }
        self.pending_query = text;
        self.searching = true;
        cx.notify();
        self.search_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| this.apply_pending_query(cx));
        }));
    }

    fn apply_pending_query(&mut self, cx: &mut Context<Self>) {
        let text = self.pending_query.clone();
        self.searching = false;
        self.apply_text(text, cx);
    }

    pub(crate) fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.query.update(cx, |q, cx| {
            q.set_value("", window, cx);
            q.focus(window, cx);
        });
    }

    #[cfg(test)]
    pub(crate) fn query(&self) -> &Entity<InputState> {
        &self.query
    }

    fn pref_switch(
        &self,
        id: &'static str,
        label: &'static str,
        checked: bool,
        change: fn(&mut AppConfig, bool),
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let this = cx.entity().downgrade();
        switch(id, label, checked, cx).on_change(move |on, _, _, cx| {
            let _ = this.update(cx, |this, cx| {
                change(&mut this.prefs, on);
                AppConfig::update(|c| change(c, on));
                crate::desktop::set_cover_icon(!this.prefs.plain_tray_icon);
                cx.notify();
            });
        })
    }

    fn browse_folder(&mut self, cx: &mut Context<Self>) {
        let path = self.new_folder.read(cx).text().to_string();
        if !path.is_empty() {
            self.library.add_folder(PathBuf::from(path));
            cx.notify();
            return;
        }
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Select Music Folder".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = picked.await {
                let _ = this.update(cx, |this, cx| {
                    for path in paths {
                        this.library.add_folder(path);
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }
}

fn sidebar_btn(
    id: impl Into<ElementId>,
    label: &str,
    icon_name: IconName,
    selected: bool,
    cx: &App,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let t = cx.omarchy().clone();
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(rems(0.5))
        .px(rems(0.625))
        .py(rems(0.4375))
        .rounded_none()
        .cursor_pointer()
        .text_size(rems(0.75))
        .font_family(t.font.clone())
        .text_color(if selected { t.foreground } else { t.secondary })
        .bg(if selected {
            t.selected_fill()
        } else {
            t.background
        })
        .hover(|s| s.bg(t.hover_fill()))
        .child(icon(icon_name).size(rems(0.875)))
        .child(label.to_string())
        .on_click(on_click)
}

impl Render for Omamusic {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snap = self.library.snapshot();
        let theme = cx.omarchy().clone();
        let nav = [
            ("nav-songs", "Songs", IconName::Music, Section::Songs),
            ("nav-albums", "Albums", IconName::Disc3, Section::Albums),
            ("nav-artists", "Artists", IconName::Mic, Section::Artists),
            ("nav-queue", "Up Next", IconName::ListMusic, Section::Queue),
        ];

        focus_scope("omamusic")
            .on_action(
                cx.listener(|this, _: &FocusSearch, window, cx| this.focus_search(window, cx)),
            )
            .on_action(cx.listener(|this, _: &GoBack, _, cx| this.back(cx)))
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Back),
                cx.listener(|this, _, _, cx| this.back(cx)),
            )
            .size_full()
            .bg(theme.background)
            .child(
                div()
                    .flex()
                    .size_full()
                    .min_w(px(480.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .p(rems(0.5))
                            .w(rems(11.))
                            .min_w(rems(8.))
                            .flex_shrink_0()
                            .border_r_1()
                            .border_color(theme.border)
                            .bg(theme.background)
                            .children(nav.map(|(id, label, icon_name, section)| {
                                sidebar_btn(
                                    id,
                                    label,
                                    icon_name,
                                    self.section == section,
                                    cx,
                                    cx.listener(move |this, _, window, cx| {
                                        this.query.update(cx, |q, cx| {
                                            q.set_value("", window, cx);
                                        });
                                        this.set_section(section, cx)
                                    }),
                                )
                            }))
                            .child(separator(cx))
                            .child(
                                div()
                                    .mt(rems(0.5))
                                    .text_size(rems(0.6875))
                                    .text_color(theme.secondary)
                                    .text_center()
                                    .child(library_counts_text(snap.album_count, snap.track_count)),
                            )
                            .when(snap.scanning, |d| {
                                d.child(
                                    div()
                                        .text_size(rems(0.6875))
                                        .text_color(theme.secondary)
                                        .child("Scanning…"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(rems(0.5))
                                    .py(rems(0.5))
                                    .border_b_1()
                                    .border_color(theme.border)
                                    .bg(theme.background)
                                    .pl(rems(0.75))
                                    .pr(px(0.))
                                    .child(div().flex_1().min_w_0().max_w(rems(28.)).child({
                                        let field = input("search", &self.query, window, cx);
                                        if self.searching {
                                            field.suffix(
                                                icon(IconName::Loader)
                                                    .size(rems(0.875))
                                                    .text_color(theme.secondary)
                                                    .with_animation(
                                                        "search-spinner",
                                                        Animation::new(Duration::from_millis(700))
                                                            .repeat(),
                                                        |this, delta| {
                                                            this.opacity(0.4 + 0.6 * delta)
                                                        },
                                                    ),
                                            )
                                        } else {
                                            field
                                        }
                                    }))
                                    .child(with_tooltip(
                                        icon_button(
                                            "rescan",
                                            IconName::RefreshCw,
                                            "Rescan library",
                                            ButtonVariant::Secondary,
                                            cx,
                                        )
                                        .on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.library.request_rescan();
                                                cx.notify();
                                            }),
                                        ),
                                        "Rescan library (Ctrl+R)",
                                    ))
                                    .child(with_tooltip(
                                        icon_button(
                                            "settings",
                                            IconName::Settings,
                                            "Settings",
                                            ButtonVariant::Secondary,
                                            cx,
                                        )
                                        .on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.show_settings = !this.show_settings;
                                                cx.notify();
                                            }),
                                        ),
                                        "Settings",
                                    )),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .min_h_0()
                                    .flex()
                                    .flex_col()
                                    .bg(theme.background)
                                    .child(
                                        if snap.folders.is_empty() && self.section != Section::Queue
                                        {
                                            empty_state("No Music", "Add a folder with music.", cx)
                                                .child(
                                                    button(
                                                        "empty-add-folder",
                                                        "Add Folder…",
                                                        ButtonVariant::Primary,
                                                        cx,
                                                    )
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.browse_folder(cx)
                                                    })),
                                                )
                                                .into_any_element()
                                        } else if snap.album_count == 0
                                            && snap.scanning
                                            && self.section != Section::Queue
                                        {
                                            empty_state(
                                                "Scanning…",
                                                "Reading the music folders.",
                                                cx,
                                            )
                                            .into_any_element()
                                        } else {
                                            self.render_content(window, cx)
                                        },
                                    ),
                            ),
                    )
                    .when(self.show_settings, |d| {
                        d.child(self.render_settings(&snap, window, cx))
                    })
                    .when(self.rescan_notify.is_some(), |d| {
                        let (msg, opacity) = self
                            .rescan_notify
                            .as_ref()
                            .map(|(m, o)| (m.clone(), *o))
                            .unwrap_or_default();
                        d.child(
                            div()
                                .absolute()
                                .top(rems(1.))
                                .left_0()
                                .right_0()
                                .flex()
                                .justify_center()
                                .opacity(opacity)
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(rems(0.5))
                                        .px(rems(1.))
                                        .py(rems(0.5))
                                        .bg(theme.background)
                                        .border_1()
                                        .border_color(theme.success.opacity(0.5))
                                        .child(
                                            icon(IconName::Check)
                                                .size(rems(0.75))
                                                .text_color(theme.success),
                                        )
                                        .child(
                                            div()
                                                .text_size(rems(0.75))
                                                .text_color(theme.foreground)
                                                .child(msg),
                                        ),
                                ),
                        )
                    })
                    .when(
                        self.prefs.visual_effects && self.player.state() == PlayerState::Playing,
                        |d| {
                            d.child(
                                self.visualizer.clone().cached(
                                    StyleRefinement::default()
                                        .absolute()
                                        .bottom_0()
                                        .left_0()
                                        .right_0()
                                        .h(rems(3.5)),
                                ),
                            )
                        },
                    ),
            )
    }
}

impl Omamusic {
    fn render_content(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        match self.pages.last().cloned() {
            Some(Page::Album(id)) => {
                if let Some(album) = self.library.album_detail(&id) {
                    return self.render_album_detail(&album, cx);
                }
            }
            Some(Page::Artist(name)) => return self.render_artist_albums(&name, window, cx),
            None => {}
        }
        match self.section {
            Section::Albums => self.render_album_grid("albums", window, cx),
            Section::Artists => self.render_artist_list(cx),
            Section::Songs => self.render_song_list(cx),
            Section::Queue => self.render_queue(cx),
        }
    }

    fn render_artist_albums(
        &mut self,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.omarchy().clone();
        let albums = self.library.artist_albums(name);
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(rems(0.75))
                    .px(rems(0.75))
                    .pt(rems(0.75))
                    .child(
                        button("artist-back", "Back", ButtonVariant::Secondary, cx)
                            .on_click(cx.listener(|this, _, _, cx| this.back(cx))),
                    )
                    .child(
                        div()
                            .text_size(rems(1.125))
                            .text_color(theme.foreground)
                            .child(name.to_string()),
                    ),
            )
            .child(self.render_album_list("artist-albums", albums, window, cx))
            .into_any_element()
    }

    fn grid_columns(window: &Window) -> (usize, gpui_kit::Pixels) {
        let rem = window.rem_size();
        let gap = rem * 0.75;
        let width = window.viewport_size().width - rem * 11. - px(1.) - gap * 2.;
        let columns = (((width + gap) / (rem * 10. + gap)).floor() as usize).max(1);
        (columns, gap)
    }

    fn render_album_grid(
        &mut self,
        id: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (columns, gap) = Self::grid_columns(window);
        let total = self.library.album_total();
        let rows = total.div_ceil(columns);
        uniform_list(
            id,
            rows,
            cx.processor(move |this, range: Range<usize>, _, cx| {
                range
                    .map(|row| {
                        let start = row * columns;
                        let end = (start + columns).min(total);
                        let albums = this.library.album_page(start, end - start);
                        div()
                            .flex()
                            .gap(gap)
                            .px(gap)
                            .pt(gap)
                            .children((start..end).map(|idx| {
                                let album = albums.get(idx - start).cloned().unwrap_or_default();
                                this.album_card(&album, idx, cx)
                            }))
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .flex_1()
        .min_h_0()
        .pb(gap)
        .into_any_element()
    }

    fn render_album_list(
        &mut self,
        id: &'static str,
        albums: Vec<AlbumSummary>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (columns, gap) = Self::grid_columns(window);
        let albums: Arc<[AlbumSummary]> = albums.into();
        let rows = albums.len().div_ceil(columns);
        uniform_list(
            id,
            rows,
            cx.processor(move |this, range: Range<usize>, _, cx| {
                range
                    .map(|row| {
                        let start = row * columns;
                        let end = (start + columns).min(albums.len());
                        div().flex().gap(gap).px(gap).pt(gap).children(
                            (start..end).map(|idx| this.album_card(&albums[idx], idx, cx)),
                        )
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .flex_1()
        .min_h_0()
        .pb(gap)
        .into_any_element()
    }

    fn album_card(&self, a: &AlbumSummary, idx: usize, cx: &mut Context<Self>) -> Popover {
        let theme = cx.omarchy().clone();
        let id = a.id.clone();
        let menu_id = a.id.clone();
        let n = a.track_count;
        let count = format!("{n} song{}", if n == 1 { "" } else { "s" });
        let artist = a.artist.clone();
        let card = div()
            .id(("album", idx))
            .w(rems(10.))
            .flex()
            .flex_col()
            .gap(rems(0.375))
            .p(rems(0.5))
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .hover(|s| s.bg(theme.hover_fill()))
            .cursor_pointer()
            .child(self.cover(Some(&a.id), 128., &a.title, cx))
            .on_click(cx.listener(move |this, _, _, cx| this.navigate(Page::Album(id.clone()), cx)))
            .child(
                div()
                    .text_size(rems(0.75))
                    .text_color(theme.foreground)
                    .truncate()
                    .child(a.title.clone()),
            )
            .child(
                div()
                    .text_size(rems(0.6875))
                    .text_color(theme.secondary)
                    .truncate()
                    .child(a.artist.clone()),
            )
            .child(
                div()
                    .text_size(rems(0.6875))
                    .text_color(theme.secondary)
                    .child(count),
            );
        Self::context_menu(
            ("album-menu", idx),
            card,
            ALBUM_ACTIONS,
            move |this, action, cx| match action {
                RowAction::GoToArtist => this.navigate(Page::Artist(artist.clone()), cx),
                _ => this.tracks_action(action, &TrackList::Album(menu_id.clone()), cx),
            },
            cx,
        )
    }

    fn render_artist_list(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let total = self.library.artist_total();
        uniform_list(
            "artists",
            total,
            cx.processor(move |this, range: Range<usize>, _, cx| {
                let start = range.start;
                let artists = this.library.artist_page(start, range.len());
                range
                    .map(|idx| {
                        let entry = artists.get(idx - start).cloned().unwrap_or_default();
                        this.artist_row(idx, &entry, cx)
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .flex_1()
        .min_h_0()
        .into_any_element()
    }

    fn artist_row(&self, idx: usize, entry: &ArtistEntry, cx: &mut Context<Self>) -> Popover {
        let theme = cx.omarchy().clone();
        let an = entry.album_count;
        let label = format!(
            "{} · {an} album{} · {} songs",
            entry.name,
            if an == 1 { "" } else { "s" },
            entry.song_count
        );
        let aname = entry.name.clone();
        let name = entry.name.clone();
        let row = div()
            .id(("artist", idx))
            .flex()
            .items_center()
            .gap(rems(0.5))
            .px(rems(0.75))
            .py(rems(0.5))
            .border_b_1()
            .border_color(theme.border)
            .hover(|s| s.bg(theme.hover_fill()))
            .cursor_pointer()
            .child(self.cover(entry.cover_album_id.as_deref(), 40., &entry.name, cx))
            .on_click(
                cx.listener(move |this, _, _, cx| this.navigate(Page::Artist(aname.clone()), cx)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(rems(0.75))
                    .text_color(theme.foreground)
                    .truncate()
                    .child(label),
            );
        Self::context_menu(
            ("artist-menu", idx),
            row,
            ARTIST_ACTIONS,
            move |this, action, cx| {
                this.tracks_action(action, &TrackList::Artist(name.clone()), cx)
            },
            cx,
        )
        .w_full()
    }

    fn render_song_list(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let total = self.library.track_total();
        uniform_list(
            "songs",
            total,
            cx.processor(move |this, range: Range<usize>, _, cx| {
                let start = range.start;
                let tracks = this.library.track_page(start, range.len());
                range
                    .map(|i| {
                        let track = tracks.get(i - start).cloned().unwrap_or_default();
                        this.song_row(i, &track, cx)
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .flex_1()
        .min_h_0()
        .into_any_element()
    }

    fn song_row(&self, i: usize, t: &Track, cx: &mut Context<Self>) -> Popover {
        let theme = cx.omarchy().clone();
        let bg = if i.is_multiple_of(2) {
            theme.background
        } else {
            theme.hover_fill()
        };
        // Column layout priority: the title dominates and keeps its share on
        // narrow screens, while the artist column grows from nothing and caps,
        // so it truncates before the song name does.
        let cell = |width: f32, text: String| {
            div()
                .w(rems(width))
                .flex_shrink_0()
                .text_size(rems(0.6875))
                .text_color(theme.secondary)
                .truncate()
                .child(text)
        };
        let side_cell = |text: String| {
            div()
                .flex_1()
                .max_w(rems(SECONDARY_MAX_W))
                .min_w_0()
                .text_size(rems(0.6875))
                .text_color(theme.secondary)
                .truncate()
                .child(text)
        };
        let row = div()
            .id(("song", i))
            .flex()
            .items_center()
            .gap(rems(0.75))
            .px(rems(0.75))
            .py(rems(0.375))
            .bg(bg)
            .hover(|s| s.bg(theme.hover_fill()))
            .cursor_pointer()
            .child(cell(2., format!("{}", i + 1)))
            .child(
                div()
                    .flex_1()
                    .flex_grow(TITLE_GROW)
                    .min_w_0()
                    .text_size(rems(0.75))
                    .text_color(theme.foreground)
                    .truncate()
                    .child(t.title.clone()),
            )
            .child(side_cell(t.artist.clone()))
            .child(cell(3., format_time(t.duration)));
        self.track_row("song", i, TrackList::Songs, false, row, cx)
    }

    fn render_album_detail(&mut self, album: &Album, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.omarchy().clone();
        let total = album.tracks.iter().map(|t| t.duration).sum::<f64>();
        let n = album.tracks.len();
        let subtitle = [
            album.artist.clone(),
            if album.year > 0 {
                album.year.to_string()
            } else {
                String::new()
            },
            format!("{n} song{}", if n == 1 { "" } else { "s" }),
            format_time(total),
        ]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
        let current = self.player.current();
        let playing = self.player.state() == PlayerState::Playing;
        let play = |shuffle: bool, tracks: Arc<[Track]>| {
            move |this: &mut Omamusic, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>| {
                let start = if shuffle {
                    rand::thread_rng().gen_range(0..tracks.len())
                } else {
                    0
                };
                this.player.set_shuffle(shuffle);
                this.player.play(tracks.to_vec(), start);
                cx.notify();
            }
        };
        div()
            .id("album-detail-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p(rems(1.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(rems(0.75))
                    .child(
                        div()
                            .flex()
                            .items_end()
                            .gap(rems(1.))
                            .child(self.cover(Some(&album.id), 176., &album.title, cx))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(rems(0.25))
                                    .child(
                                        div()
                                            .text_size(rems(1.125))
                                            .text_color(theme.foreground)
                                            .child(album.title.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_size(rems(0.75))
                                            .text_color(theme.secondary)
                                            .child(subtitle),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .gap(rems(0.5))
                                            .child(
                                                button(
                                                    "play-album",
                                                    "Play",
                                                    ButtonVariant::Primary,
                                                    cx,
                                                )
                                                .on_click(
                                                    cx.listener(play(false, album.tracks.clone())),
                                                ),
                                            )
                                            .child(
                                                button(
                                                    "shuffle-album",
                                                    "Shuffle",
                                                    ButtonVariant::Secondary,
                                                    cx,
                                                )
                                                .on_click(
                                                    cx.listener(play(true, album.tracks.clone())),
                                                ),
                                            )
                                            .child(
                                                button(
                                                    "back",
                                                    "Back",
                                                    ButtonVariant::Secondary,
                                                    cx,
                                                )
                                                .on_click(
                                                    cx.listener(|this, _, _, cx| this.back(cx)),
                                                ),
                                            ),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .children(album.tracks.iter().enumerate().map(|(i, t)| {
                                let is_current = current.as_ref().is_some_and(|c| c.path == t.path);
                                let number = if is_current {
                                    icon(if playing {
                                        IconName::Volume2
                                    } else {
                                        IconName::Volume
                                    })
                                    .size(rems(0.875))
                                    .text_color(theme.accent)
                                    .into_any_element()
                                } else if t.track_number > 0 {
                                    t.track_number.to_string().into_any_element()
                                } else {
                                    "".into_any_element()
                                };
                                let row = div()
                                    .id(("album-track", i))
                                    .flex()
                                    .items_center()
                                    .gap(rems(0.75))
                                    .px(rems(0.5))
                                    .py(rems(0.375))
                                    .hover(|s| s.bg(theme.hover_fill()))
                                    .cursor_pointer()
                                    .child(
                                        div()
                                            .w(rems(2.))
                                            .flex()
                                            .justify_end()
                                            .text_color(theme.secondary)
                                            .child(number),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .flex()
                                            .gap(rems(0.5))
                                            .text_color(theme.foreground)
                                            .child(t.title.clone())
                                            .when(t.artist != album.artist, |d| {
                                                d.child(
                                                    div()
                                                        .text_color(theme.secondary)
                                                        .child(t.artist.clone()),
                                                )
                                            }),
                                    )
                                    .child(
                                        div()
                                            .text_color(theme.secondary)
                                            .child(format_time(t.duration)),
                                    );
                                self.track_row(
                                    "album-track-menu",
                                    i,
                                    TrackList::Loaded(album.tracks.clone()),
                                    false,
                                    row,
                                    cx,
                                )
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_queue(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.omarchy().clone();
        let (queue, index) = self.player.queue();
        if queue.is_empty() {
            return empty_state(
                "Queue Empty",
                "Play something or add songs to the queue.",
                cx,
            )
            .into_any_element();
        }
        let playing = self.player.state() == PlayerState::Playing;
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(rems(0.75))
                    .px(rems(0.75))
                    .py(rems(0.5))
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex_1()
                            .text_size(rems(0.75))
                            .text_color(theme.secondary)
                            .child(format!(
                                "{} songs · {}",
                                queue.len(),
                                format_time(queue.iter().map(|t| t.duration).sum())
                            )),
                    )
                    .child(
                        button("clear-queue", "Clear", ButtonVariant::Secondary, cx).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.player.stop();
                                this.clear_selection();
                                cx.notify();
                            }),
                        ),
                    ),
            )
            .child(
                uniform_list(
                    "queue",
                    queue.len(),
                    cx.processor(move |this, range: Range<usize>, _, cx| {
                        range
                            .map(|i| this.queue_row(i, &queue, index, playing, cx))
                            .collect::<Vec<_>>()
                    }),
                )
                .flex_1()
                .min_h_0(),
            )
            .into_any_element()
    }

    fn queue_row(
        &self,
        i: usize,
        queue: &Arc<[Track]>,
        index: Option<usize>,
        playing: bool,
        cx: &mut Context<Self>,
    ) -> Popover {
        let theme = cx.omarchy().clone();
        let t = &queue[i];
        let is_current = index == Some(i);
        let past = index.is_some_and(|current| i < current);
        let number = if is_current {
            icon(if playing {
                IconName::Volume2
            } else {
                IconName::Volume
            })
            .size(rems(0.875))
            .text_color(theme.accent)
            .into_any_element()
        } else {
            (i + 1).to_string().into_any_element()
        };
        let row = div()
            .id(("queue-row", i))
            .flex()
            .items_center()
            .gap(rems(0.75))
            .px(rems(0.75))
            .py(rems(0.375))
            .text_size(rems(0.75))
            .text_color(if past {
                theme.secondary
            } else {
                theme.foreground
            })
            .hover(|s| s.bg(theme.hover_fill()))
            .cursor_pointer()
            .child(
                div()
                    .w(rems(2.))
                    .flex()
                    .justify_end()
                    .text_color(theme.secondary)
                    .child(number),
            )
            .child(self.cover(Some(&t.album_id), 28., &t.album, cx))
            .child(div().flex_1().min_w_0().truncate().child(t.title.clone()))
            .child(
                div()
                    .w(rems(10.))
                    .text_color(theme.secondary)
                    .truncate()
                    .child(t.artist.clone()),
            )
            .child(
                div()
                    .w(rems(3.))
                    .text_color(theme.secondary)
                    .child(format_time(t.duration)),
            );
        self.track_row(
            "queue-menu",
            i,
            TrackList::Loaded(queue.clone()),
            true,
            row,
            cx,
        )
    }

    fn render_settings(
        &mut self,
        snap: &LibrarySnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.omarchy();
        // Overlay backdrop
        div()
            .absolute()
            .size_full()
            .bg(theme.background.opacity(0.6))
            .flex()
            .items_center()
            .justify_center()
            .p(rems(1.))
            .child(
                panel("Settings", cx)
                    .id("settings-panel")
                    .w(rems(32.))
                    .max_h_full()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(rems(0.5))
                    .child(
                        div().flex().items_center().justify_end().child(
                            icon_button(
                                "close-settings-x",
                                IconName::Close,
                                "Close settings",
                                ButtonVariant::Secondary,
                                cx,
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_settings = false;
                                cx.notify();
                            })),
                        ),
                    )
                    .child(separator(cx))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(rems(0.25))
                            .child(self.pref_switch(
                                "pref-close-to-tray",
                                "Close window to tray",
                                !self.prefs.quit_on_close,
                                |c, on| c.quit_on_close = !on,
                                cx,
                            ))
                            .child(self.pref_switch(
                                "pref-tray-cover",
                                "Use album cover as tray icon",
                                !self.prefs.plain_tray_icon,
                                |c, on| c.plain_tray_icon = !on,
                                cx,
                            ))
                            .child(self.pref_switch(
                                "pref-start-hidden",
                                "Start hidden in tray",
                                self.prefs.start_hidden,
                                |c, on| c.start_hidden = on,
                                cx,
                            ))
                            .child(self.pref_switch(
                                "pref-visual-effects",
                                "Visual Effects",
                                self.prefs.visual_effects,
                                |c, on| c.visual_effects = on,
                                cx,
                            )),
                    )
                    .child(separator(cx))
                    .child(div().flex().flex_col().gap(rems(0.5)).children(
                        snap.folders.iter().map(|f| {
                            let p = f.clone();
                            div()
                                .flex()
                                .items_center()
                                .gap(rems(0.5))
                                .child(
                                    div()
                                        .flex_1()
                                        .text_size(rems(0.6875))
                                        .child(f.to_string_lossy().to_string()),
                                )
                                .child(
                                    button("rm-folder", "Remove", ButtonVariant::Secondary, cx)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.library.remove_folder(&p);
                                            cx.notify();
                                        })),
                                )
                        }),
                    ))
                    .child(
                        div()
                            .flex()
                            .gap(rems(0.5))
                            .child(div().flex_1().child(input(
                                "new-folder",
                                &self.new_folder,
                                window,
                                cx,
                            )))
                            .child(
                                button("browse-folder", "Browse…", ButtonVariant::Secondary, cx)
                                    .on_click(cx.listener(|this, _, _, cx| this.browse_folder(cx))),
                            ),
                    )
                    .child(
                        button("add-folder", "Add ~/Music", ButtonVariant::Primary, cx).on_click(
                            cx.listener(|this, _, _, cx| {
                                if let Some(music) = dirs::audio_dir() {
                                    this.library.add_folder(music);
                                }
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        button("close-settings", "Close", ButtonVariant::Secondary, cx).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.show_settings = false;
                                cx.notify();
                            }),
                        ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FocusSearch, TogglePlay};
    use gpui_kit::{KeyBinding, Keystroke, TestAppContext, VisualTestContext};
    use std::cell::Cell;
    use std::rc::Rc;

    fn setup<'a>(
        cx: &'a mut TestAppContext,
        toggles: &Rc<Cell<usize>>,
    ) -> (Entity<Omamusic>, &'a mut VisualTestContext) {
        cx.update(|cx| {
            gpui_omarchy::init(cx);
            cx.bind_keys([
                KeyBinding::new("space", TogglePlay, None),
                KeyBinding::new("ctrl-f", FocusSearch, None),
            ]);
            let toggles = toggles.clone();
            cx.on_action(move |_: &TogglePlay, cx| {
                toggles.set(toggles.get() + 1);
                cx.refresh_windows();
            });
        });
        cx.add_window_view(|window, cx| {
            Omamusic::new(
                Arc::new(Library::empty()),
                Arc::new(Player::new_test()),
                window,
                cx,
            )
        })
    }

    fn focus_search(view: &Entity<Omamusic>, cx: &mut VisualTestContext) {
        view.update_in(cx, |view, window, cx| {
            view.query.update(cx, |query, cx| query.focus(window, cx));
        });
    }

    fn blur_focus(cx: &mut VisualTestContext) {
        cx.update(|window, cx| window.disable_focus(cx));
    }

    fn type_stroke(cx: &mut VisualTestContext, stroke: Keystroke) {
        cx.update(|window, cx| {
            window.dispatch_keystroke(stroke, cx);
        });
        cx.run_until_parked();
    }

    /// Field text and the query currently applied to the library, after letting
    /// the render-time sync run.
    fn search_state(view: &Entity<Omamusic>, cx: &mut VisualTestContext) -> (String, String) {
        view.update(cx, |view, cx| {
            view.apply_query(cx);
            (view.query.read(cx).text().to_string(), view.library.query())
        })
    }

    fn space() -> Keystroke {
        Keystroke::parse("space").unwrap()
    }

    fn character(ch: char) -> Keystroke {
        let mut stroke = space();
        if ch != ' ' {
            stroke.key = ch.to_string();
        }
        stroke.key_char = Some(ch.to_string());
        stroke
    }

    #[gpui_kit::test]
    async fn space_typed_into_the_search_field_is_not_play_pause(cx: &mut TestAppContext) {
        let toggles = Rc::new(Cell::new(0));
        let (view, cx) = setup(cx, &toggles);
        focus_search(&view, cx);
        type_stroke(cx, space());
        let (text, applied) = search_state(&view, cx);
        assert_eq!(text, " ", "space should be typed into the field");
        assert_eq!(applied, " ", "the field's content reaches the search");
        assert_eq!(toggles.get(), 0, "typing a space must not toggle playback");
    }

    #[gpui_kit::test]
    async fn search_accepts_spaces_and_unicode(cx: &mut TestAppContext) {
        let toggles = Rc::new(Cell::new(0));
        let (view, cx) = setup(cx, &toggles);
        focus_search(&view, cx);
        for ch in "moon 藍".chars() {
            type_stroke(cx, character(ch));
        }
        let (text, applied) = search_state(&view, cx);
        assert_eq!(text, "moon 藍");
        assert_eq!(applied, "moon 藍");
        assert_eq!(toggles.get(), 0);
    }

    #[gpui_kit::test]
    async fn space_with_no_field_focused_still_toggles_playback(cx: &mut TestAppContext) {
        let toggles = Rc::new(Cell::new(0));
        let (view, cx) = setup(cx, &toggles);
        // Focus then blur the search field so the window has focus history but no field.
        focus_search(&view, cx);
        blur_focus(cx);
        type_stroke(cx, space());
        let (text, _) = search_state(&view, cx);
        assert_eq!(text, "");
        assert_eq!(
            toggles.get(),
            1,
            "bare space outside a field still toggles playback"
        );
    }

    #[gpui_kit::test]
    async fn tab_click_clears_the_search(cx: &mut TestAppContext) {
        let toggles = Rc::new(Cell::new(0));
        let (view, cx) = setup(cx, &toggles);
        focus_search(&view, cx);
        type_stroke(cx, character('m'));
        let (text, applied) = search_state(&view, cx);
        assert_eq!((text.as_str(), applied.as_str()), ("m", "m"));
        // The nav buttons are plain divs (not registered for observation), so
        // run the click handler's exact body: clearing the field from inside the
        // view's own update — the nesting that used to panic with the old
        // change observer.
        view.update_in(cx, |view, window, cx| {
            view.query
                .update(cx, |query, cx| query.set_value("", window, cx));
            view.set_section(Section::Albums, cx);
        });
        cx.run_until_parked();
        let (text, applied) = search_state(&view, cx);
        assert_eq!(text, "", "clicking a tab clears the search field");
        assert_eq!(applied, "", "and the library filter");
        assert_eq!(view.read_with(cx, |view, _| view.section), Section::Albums);
    }

    #[gpui_kit::test]
    async fn search_is_debounced_until_typing_pauses(cx: &mut TestAppContext) {
        let toggles = Rc::new(Cell::new(0));
        let (view, cx) = setup(cx, &toggles);
        focus_search(&view, cx);

        type_stroke(cx, character('m'));
        type_stroke(cx, character('o'));

        // Inside the debounce window: the field has both characters, but the
        // library has not been queried yet and the spinner is showing.
        view.read_with(cx, |view, cx| {
            assert_eq!(view.query.read(cx).text(), "mo");
            assert_eq!(view.library.query(), "");
            assert!(view.searching);
        });

        cx.executor()
            .advance_clock(SEARCH_DEBOUNCE + Duration::from_millis(10));
        cx.run_until_parked();

        view.read_with(cx, |view, _| {
            assert_eq!(view.library.query(), "mo");
            assert!(!view.searching);
        });
    }

    #[gpui_kit::test]
    async fn ctrl_f_clears_and_focuses_the_search_field(cx: &mut TestAppContext) {
        let toggles = Rc::new(Cell::new(0));
        let (view, cx) = setup(cx, &toggles);
        focus_search(&view, cx);
        type_stroke(cx, character('m'));
        assert_eq!(search_state(&view, cx).0, "m");

        type_stroke(cx, Keystroke::parse("ctrl-f").unwrap());

        let (text, applied) = search_state(&view, cx);
        assert_eq!(text, "", "Ctrl+F clears the field as it focuses it");
        assert_eq!(applied, "", "and the library filter with it");
        view.update_in(cx, |view, window, cx| {
            assert!(
                view.query.read(cx).focus_handle(cx).is_focused(window),
                "Ctrl+F moves the caret into the search field"
            );
        });
    }

    #[gpui_kit::test]
    async fn the_scan_toast_holds_then_fades_out_by_three_seconds(cx: &mut TestAppContext) {
        let toggles = Rc::new(Cell::new(0));
        let (view, cx) = setup(cx, &toggles);
        view.update(cx, |view, cx| {
            view.show_notify("5 songs · 2 albums".into(), cx)
        });
        view.read_with(cx, |view, _| {
            let (msg, opacity) = view.rescan_notify.clone().unwrap();
            assert_eq!(msg, "5 songs · 2 albums");
            assert_eq!(opacity, 1.0, "freshly shown toast is solid");
        });

        cx.executor().advance_clock(NOTIFY_HOLD);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            let (_, opacity) = view.rescan_notify.clone().unwrap();
            assert!(
                opacity < 1.0 && opacity > 0.0,
                "after the hold the toast fades gradually, got {opacity}"
            );
        });

        cx.executor()
            .advance_clock(NOTIFY_FADE + NOTIFY_FADE_STEP * 2);
        cx.run_until_parked();
        assert!(
            view.read_with(cx, |view, _| view.rescan_notify.is_none()),
            "the toast is gone within three seconds"
        );
        assert!(
            NOTIFY_HOLD + NOTIFY_FADE <= Duration::from_millis(3000),
            "total lifetime must fit the promised 3s"
        );
    }

    #[test]
    fn visualizer_shape_stays_in_range_and_loops_seamlessly() {
        for bar in 0..VISUALIZER_BARS {
            for step in 0..=20 {
                let delta = step as f32 / 20.;
                let shape = visualizer_shape(bar, delta);
                assert!((0.0..=1.0).contains(&shape), "bar {bar} @ {delta}: {shape}");
            }
            assert!(
                (visualizer_shape(bar, 0.) - visualizer_shape(bar, 1.)).abs() < 1e-5,
                "bar {bar} jumps when its animation repeats"
            );
        }
    }

    #[test]
    fn visualizer_bars_rise_with_the_level_and_collapse_without_it() {
        for bar in 0..VISUALIZER_BARS {
            assert_eq!(
                visualizer_height(0.0, 0.5, bar),
                0.12,
                "silence leaves only the thin baseline"
            );
            assert!(
                visualizer_height(1.0, 0.5, bar) >= visualizer_height(0.3, 0.5, bar),
                "louder must not shrink bar {bar}"
            );
            assert!(
                visualizer_height(1.0, 0.5, bar) <= 2.6,
                "bar {bar} may not overrun its strip"
            );
            assert!(visualizer_opacity(1.0, 0.5, bar) <= 0.5, "stay see-through");
        }
        let loud = (0..VISUALIZER_BARS)
            .map(|b| visualizer_height(1.0, 0.4, b))
            .sum::<f32>();
        let quiet = (0..VISUALIZER_BARS)
            .map(|b| visualizer_height(0.2, 0.4, b))
            .sum::<f32>();
        assert!(loud > quiet, "the strip as a whole reacts to the level");
    }

    #[test]
    fn the_count_line_leads_with_songs_then_albums() {
        assert_eq!(library_counts_text(1, 2), "2 songs · 1 albums");
        assert_eq!(library_counts_text(12, 340), "340 songs · 12 albums");
    }

    #[test]
    fn dropping_the_source_column_gives_the_title_room() {
        // Three competing columns (title + artist + source) squeezed the title
        // on small screens; with the source column gone the title claims the
        // clear majority of the flexible width.
        let before = title_flex_share(2);
        let after = title_flex_share(1);
        assert!(
            after > before,
            "fewer secondary columns grow the title's share"
        );
        assert!(
            after > 0.7,
            "the title keeps the bulk of the flexible space"
        );
    }
}
