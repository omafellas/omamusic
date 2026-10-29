use crate::config::AppConfig;
use crate::library::{Library, LibrarySnapshot, SongSource};
use crate::liked::Liked;
use crate::models::{
    Album, AlbumSort, AlbumSummary, AlbumView, ArtistEntry, GenreEntry, PlayerState, Section,
    SongColumn, SongSort, Track,
};
use crate::player::Player;
use crate::playlists::Playlists;
use crate::search::{highlight_ranges, top_result, TopResult};
use crate::{DecreaseFontSize, FocusSearch, GoBack, GoForward, IncreaseFontSize, ResetFontSize};
use gpui_kit::base::input::{InputEvent, InputState};
use gpui_kit::base::motion::{transition, Transition};
use gpui_kit::base::{Popover, Selectable};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::TestSupportExt;
use gpui_kit::{
    div, ease_out_quint, img, linear_color_stop, linear_gradient, px, rems, uniform_list,
    Animation, AnimationExt as _, AnyElement, App, AppContext, BoxShadow, ClickEvent, Context, Div,
    ElementId, Entity, EntityInputHandler, FocusHandle, Focusable, FontWeight, HighlightStyle,
    Hsla, ImageSource, InteractiveElement, IntoElement, MouseButton, NavigationDirection,
    ObjectFit, ParentElement, PathPromptOptions, Pixels, Render, Rgba, SharedString, Stateful,
    StatefulInteractiveElement, StyleRefinement, Styled, StyledImage as _, StyledText, Window,
};
use gpui_omarchy::{
    avatar, avatar_image, button, empty_state, focus_scope, icon, icon_button, input, keycap,
    keycaps, menu, panel, separator, switch, with_tooltip, ActiveTheme, Button, ButtonVariant,
    IconName, MenuItem,
};
use rand::Rng;
use std::cell::Cell;
use std::collections::BTreeSet;
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
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
    /// The Up Next panel beside the page.
    show_queue: bool,
    liked: Liked,
    /// Bumped each time the Up Next panel opens so it slides in afresh.
    queue_seq: u64,
    lib_version: u64,
    prefs: AppConfig,
    new_folder: Entity<InputState>,
    rescan_notify: Option<(String, f32)>,
    /// Bumped for every toast; makes an older fade task ignore a replaced toast.
    notify_seq: u64,
    cover_poll: CoverPoll,
    /// Bumped on every navigation so the content area fades in afresh.
    nav_seq: u64,
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
    /// Focus for the view itself. Actions dispatch along the focus path, so the
    /// root element holds focus whenever no text field does; otherwise the
    /// window shortcuts (Ctrl+=, Ctrl+P, Alt+Left) have nothing to reach.
    focus_handle: FocusHandle,
    /// Refocus the root when one of our handles loses focus to nothing.
    _focus_guards: Vec<gpui_kit::Subscription>,
    search_scope: SearchScope,
    forward: Vec<Page>,
    playlists: Playlists,
    playlist_name: Entity<InputState>,
    playlist_edit: Option<PlaylistEdit>,
    confirm_delete: Option<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PlaylistEdit {
    New,
    Rename(u64),
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum SearchScope {
    #[default]
    All,
    Songs,
    Albums,
    Artists,
}

#[derive(Clone, PartialEq)]
enum Page {
    Album(String),
    Artist(String),
    Genre(String),
}

/// Where a row's playback list comes from. Rows themselves are paged from
/// SQLite, so actions resolve the full list lazily instead of holding it.
#[derive(Clone)]
enum TrackList {
    Source(SongSource),
    Playlist(u64, Arc<[PathBuf]>),
    Album(String),
    Artist(String),
    /// A list already in memory (album detail, play queue).
    Loaded(Arc<[Track]>),
}

impl TrackList {
    fn tracks(&self, library: &Library) -> Vec<Track> {
        match self {
            TrackList::Source(source) => library.songs(source),
            TrackList::Playlist(_, paths) => library.songs(&SongSource::Paths(paths.clone())),
            TrackList::Album(key) => library.album_tracks(key),
            TrackList::Artist(name) => library.artist_tracks(name),
            TrackList::Loaded(tracks) => tracks.to_vec(),
        }
    }

    fn get(&self, library: &Library, index: usize) -> Option<Track> {
        match self {
            TrackList::Source(source) => library.song_page(source, index, 1).into_iter().next(),
            TrackList::Playlist(_, paths) => library
                .song_page(&SongSource::Paths(paths.clone()), index, 1)
                .into_iter()
                .next(),
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
    AddToPlaylistMenu,
    AddToPlaylist(u64),
    NewPlaylistWith,
    RemoveFromPlaylist,
}

const TRACK_ACTIONS: &[RowAction] = &[
    RowAction::Play,
    RowAction::PlayNext,
    RowAction::AddToQueue,
    RowAction::AddToPlaylistMenu,
    RowAction::GoToAlbum,
    RowAction::GoToArtist,
    RowAction::ExportFile,
];
const PLAYLIST_TRACK_ACTIONS: &[RowAction] = &[
    RowAction::Play,
    RowAction::PlayNext,
    RowAction::AddToQueue,
    RowAction::AddToPlaylistMenu,
    RowAction::GoToAlbum,
    RowAction::GoToArtist,
    RowAction::RemoveFromPlaylist,
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
    RowAction::AddToPlaylistMenu,
    RowAction::GoToArtist,
];
const ARTIST_ACTIONS: &[RowAction] = &[
    RowAction::Play,
    RowAction::PlayNext,
    RowAction::AddToQueue,
    RowAction::AddToPlaylistMenu,
];
const ALBUM_MORE_ACTIONS: &[RowAction] = &[
    RowAction::PlayNext,
    RowAction::AddToQueue,
    RowAction::AddToPlaylistMenu,
];
const SIDEBAR_REMS: f32 = 13.75;
const QUEUE_REMS: f32 = 17.5;
const SONG_COLUMNS: [(SongColumn, &str); 5] = [
    (SongColumn::Title, "Title"),
    (SongColumn::Artist, "Artist"),
    (SongColumn::Album, "Album"),
    (SongColumn::Genre, "Genre"),
    (SongColumn::Time, "Time"),
];

const VARIOUS_ARTISTS: &str = "Various Artists";
const SEARCH_PREVIEW_SONGS: usize = 6;
const SEARCH_PREVIEW_ARTISTS: usize = 4;
const SEARCH_TOP_CANDIDATES: usize = 50;
const ALBUM_CARD_REMS: f32 = 10.;

/// Default rem size in pixels; the UI scale multiplies it.
const BASE_REM_SIZE: f32 = 16.0;
/// UI scale step per Ctrl+= / Ctrl+- press.
const UI_SCALE_STEP: f64 = 0.1;
/// Allowed UI scale range.
const UI_SCALE_MIN: f64 = 0.75;
const UI_SCALE_MAX: f64 = 2.0;

/// Window rem size for a UI scale factor.
fn rem_for_scale(scale: f64) -> gpui_kit::Pixels {
    px((BASE_REM_SIZE as f64 * scale) as f32)
}

/// Step the UI scale up or down, rounded to one decimal and clamped.
fn stepped_scale(current: f64, steps: i32) -> f64 {
    (((current * 10.0).round() + steps as f64 * UI_SCALE_STEP * 10.0) / 10.0)
        .clamp(UI_SCALE_MIN, UI_SCALE_MAX)
}

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
            RowAction::Play
            | RowAction::PlayNext
            | RowAction::AddToQueue
            | RowAction::AddToPlaylistMenu
            | RowAction::AddToPlaylist(_)
            | RowAction::NewPlaylistWith => 0,
            RowAction::GoToAlbum | RowAction::GoToArtist => 1,
            RowAction::Remove | RowAction::RemoveFromPlaylist => 2,
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
            RowAction::AddToPlaylistMenu | RowAction::AddToPlaylist(_) => {
                ("Add to Playlist", IconName::ListPlus)
            }
            RowAction::NewPlaylistWith => ("New Playlist", IconName::Plus),
            RowAction::RemoveFromPlaylist => ("Remove from Playlist", IconName::ListX),
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

const PAGE_ENTER: Duration = Duration::from_millis(220);
const COVER_ENTER: Duration = Duration::from_millis(240);
const PANEL_ENTER: Duration = Duration::from_millis(200);
pub(crate) const ROW_FILL: Duration = Duration::from_millis(120);
pub(crate) const LIFT: Duration = Duration::from_millis(120);
pub(crate) const TINT_ENTER: Duration = Duration::from_millis(400);

/// A cover tint as a translucent wash.
pub(crate) fn tint_color(tint: [u8; 3], alpha: f32) -> Hsla {
    Rgba {
        r: tint[0] as f32 / 255.,
        g: tint[1] as f32 / 255.,
        b: tint[2] as f32 / 255.,
        a: alpha,
    }
    .into()
}

/// The marker for the current track in a list.
fn now_playing(playing: bool, color: Hsla) -> AnyElement {
    icon(if playing {
        IconName::Volume2
    } else {
        IconName::Volume
    })
    .size(rems(0.875))
    .text_color(color)
    .into_any_element()
}

/// How far the pointer's arrival over `key` has eased in, from 0 at rest to
/// 1 while hovered, plus the listener that feeds it. The element using it
/// must pass the listener to `on_hover`.
pub(crate) fn hover_progress(
    key: impl Into<ElementId>,
    policy: Transition,
    window: &mut Window,
    cx: &mut App,
) -> (f32, impl Fn(&bool, &mut Window, &mut App) + 'static) {
    let key = key.into();
    let hovered = window.use_keyed_state((key.clone(), "hovered"), cx, |_, _| false);
    let target = if *hovered.read(cx) { 1. } else { 0. };
    let progress = transition((key, "hover"), target, policy, window, cx);
    let listener = move |over: &bool, _: &mut Window, cx: &mut App| {
        hovered.update(cx, |state, cx| {
            if *state != *over {
                *state = *over;
                cx.notify();
            }
        })
    };
    (progress, listener)
}

/// A translucent row fill: a faint stripe on alternate rows that eases up to
/// the full hover color, so whatever is behind the list shows through.
pub(crate) fn row_fill(hover: Hsla, stripe: bool, progress: f32) -> Hsla {
    let rest = if stripe { 0.5 } else { 0. };
    hover.opacity(rest + (1. - rest) * progress)
}

/// Corner radius for a cover of `size` pixels: tighter on small thumbnails.
pub fn cover_radius(size: f32) -> Pixels {
    px(if size >= 180. {
        8.
    } else if size >= 96. {
        6.
    } else {
        4.
    })
}
/// Re-renders a view while cover thumbnails it asked for are still being
/// decoded. Polling a timer keeps the wake-up on the foreground executor,
/// where GPUI's deterministic test scheduler can see it.
#[derive(Clone, Default)]
pub(crate) struct CoverPoll(Rc<Cell<bool>>);

const COVER_POLL: Duration = Duration::from_millis(40);

impl CoverPoll {
    pub(crate) fn tick<V: 'static>(&self, library: &Library, cx: &mut Context<V>) {
        if self.0.get() || library.covers().pending_count() == 0 {
            return;
        }
        self.0.set(true);
        let flag = self.0.clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COVER_POLL).await;
            flag.set(false);
            let _ = this.update(cx, |_, cx| cx.notify());
        })
        .detach();
    }
}

pub fn cover(
    library: &Library,
    album_id: Option<&str>,
    size: f32,
    name: &str,
    cx: &App,
) -> AnyElement {
    rounded_cover(library, album_id, size, name, cover_radius(size), cx)
}

pub fn rounded_cover(
    library: &Library,
    album_id: Option<&str>,
    size: f32,
    name: &str,
    radius: Pixels,
    cx: &App,
) -> AnyElement {
    let cover = avatar(initials(name), cx)
        .size(px(size))
        .rounded(radius)
        .border_0();
    match album_id.and_then(|id| library.covers().cached(id, size as u32 * 2)) {
        Some(thumb) if thumb.fresh => cover
            .image(avatar_image(ImageSource::Render(thumb.image)).rounded(radius))
            .with_animation(
                ElementId::Name(format!("cover-in-{}", album_id.unwrap_or_default()).into()),
                Animation::new(COVER_ENTER).with_easing(ease_out_quint()),
                |cover, t| cover.opacity(t),
            )
            .into_any_element(),
        Some(thumb) => cover
            .image(avatar_image(ImageSource::Render(thumb.image)).rounded(radius))
            .into_any_element(),
        None => cover.into_any_element(),
    }
}

/// The art slot of a grid tile, raised and shadowed as `lift` eases to 1.
fn lifted(size: f32, lift: f32) -> Div {
    div()
        .relative()
        .mb(rems(0.375))
        .top(px(-2. * lift))
        .rounded(cover_radius(size))
        .when(lift > 0., |d| {
            d.shadow(vec![BoxShadow {
                color: gpui_kit::black().opacity(0.35 * lift),
                offset: gpui_kit::point(px(0.), px(6. * lift)),
                blur_radius: px(16. * lift),
                spread_radius: px(0.),
                inset: false,
            }])
        })
}

/// A rounded Play or Shuffle action: filled with the accent when `primary`.
fn pill(id: &'static str, glyph: IconName, label: &'static str, primary: bool, cx: &App) -> Button {
    let t = cx.omarchy();
    let (rest, hover, ink) = if primary {
        (t.accent, t.accent.opacity(0.85), t.on_accent)
    } else {
        (t.hover_fill(), t.selected_fill(), t.foreground)
    };
    button(id, "", ButtonVariant::Secondary, cx)
        .accessibility_label(label)
        .child(icon(glyph).size(rems(0.75)))
        .child(label)
        .rounded_full()
        .px(rems(1.))
        .border_color(rest.opacity(0.))
        .font_weight(FontWeight::MEDIUM)
        .bg(rest)
        .text_color(ink)
        .hover(|s| s.bg(hover))
}

fn highlighted(text: &str, query: &str, color: gpui_kit::Hsla) -> StyledText {
    let style = HighlightStyle {
        color: Some(color),
        font_weight: Some(FontWeight::SEMIBOLD),
        ..Default::default()
    };
    StyledText::new(text.to_string()).with_highlights(
        highlight_ranges(text, query)
            .into_iter()
            .map(|range| (range, style)),
    )
}

fn initials(text: &str) -> String {
    text.chars().take(2).collect()
}

fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

fn format_minutes(secs: f64) -> String {
    let minutes = (secs.max(0.0) / 60.0).round() as u64;
    if minutes >= 60 {
        format!("{} hr {} min", minutes / 60, minutes % 60)
    } else {
        format!("{minutes} min")
    }
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
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
        let new_folder = cx.new(|cx| InputState::new(window, cx).placeholder("Enter folder path"));
        let playlist_name = cx.new(|cx| InputState::new(window, cx).placeholder("Playlist name"));
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        let _focus_guards = [
            &focus_handle,
            &query.read(cx).focus_handle(cx),
            &new_folder.read(cx).focus_handle(cx),
            &playlist_name.read(cx).focus_handle(cx),
        ]
        .into_iter()
        .map(|handle| {
            cx.on_blur(handle, window, |this, window, cx| {
                this.reclaim_focus(window, cx)
            })
        })
        .collect();

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
        let name_field = playlist_name.clone();
        let name_focus = playlist_name.read(cx).focus_handle(cx);
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
            } else if name_focus.is_focused(window) {
                &name_field
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
        cx.subscribe(&query, |this, _, event: &InputEvent, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.play_top_result(cx);
            }
        })
        .detach();
        cx.subscribe(&playlist_name, |this, _, event: &InputEvent, cx| {
            if let InputEvent::PressEnter { .. } | InputEvent::Blur = event {
                this.commit_playlist_name(cx);
            }
        })
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

        let visualizer = cx.new(|cx| Visualizer::new(player.clone(), cx));

        let view = std::env::var("OMADRIVE_VIEW").ok();
        let show_queue = view.as_deref() == Some("queue");
        let (section, pages, show_settings) = match view.as_deref() {
            Some("artists") => (Section::Artists, Vec::new(), false),
            Some("genres") => (Section::Genres, Vec::new(), false),
            Some("songs") => (Section::Songs, Vec::new(), false),
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
            show_queue,
            liked: Liked::load(),
            queue_seq: 0,
            lib_version: 0,
            prefs: AppConfig::load(),
            new_folder,
            rescan_notify: None,
            notify_seq: 0,
            cover_poll: CoverPoll::default(),
            nav_seq: 0,
            prev_scanning: false,
            visualizer,
            pending_query: String::new(),
            search_task: None,
            searching: false,
            _typing_guard,
            focus_handle,
            _focus_guards,
            search_scope: SearchScope::All,
            forward: Vec::new(),
            playlists: Playlists::load(),
            playlist_name,
            playlist_edit: None,
            confirm_delete: None,
        }
    }

    fn start_playlist_edit(
        &mut self,
        edit: PlaylistEdit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = match edit {
            PlaylistEdit::New => String::new(),
            PlaylistEdit::Rename(id) => self
                .playlists
                .get(id)
                .map(|p| p.name.clone())
                .unwrap_or_default(),
        };
        self.playlist_edit = Some(edit);
        self.playlist_name.update(cx, |field, cx| {
            field.set_value(name, window, cx);
            field.focus(window, cx);
        });
        cx.notify();
    }

    fn commit_playlist_name(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.playlist_edit.take() else {
            return;
        };
        let name = self
            .playlist_name
            .read(cx)
            .text()
            .to_string()
            .trim()
            .to_string();
        match edit {
            PlaylistEdit::New if !name.is_empty() => {
                let id = self.playlists.create(&name, Vec::new());
                self.playlists.save();
                self.set_section(Section::Playlist(id), cx);
            }
            PlaylistEdit::Rename(id) => {
                self.playlists.rename(id, &name);
                self.playlists.save();
            }
            PlaylistEdit::New => {}
        }
        cx.notify();
    }

    fn cancel_playlist_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.playlist_edit = None;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn delete_playlist(&mut self, id: u64, cx: &mut Context<Self>) {
        self.playlists.delete(id);
        self.playlists.save();
        self.confirm_delete = None;
        if self.section == Section::Playlist(id) {
            self.set_section(Section::Songs, cx);
        }
        cx.notify();
    }

    fn set_song_sort(&mut self, column: SongColumn, cx: &mut Context<Self>) {
        let sort = self.prefs.song_sort.toggled(column);
        self.prefs.song_sort = sort;
        AppConfig::update(|c| c.song_sort = sort);
        self.clear_selection();
        cx.notify();
    }

    pub fn playlist_names(&self) -> Vec<String> {
        self.playlists
            .items
            .iter()
            .map(|p| p.name.clone())
            .collect()
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

    pub fn toggle_queue(&mut self, cx: &mut Context<Self>) {
        self.show_queue = !self.show_queue;
        if self.show_queue {
            self.queue_seq += 1;
        }
        cx.notify();
    }

    pub fn is_liked(&self, path: &std::path::Path) -> bool {
        self.liked.contains(path)
    }

    pub fn toggle_like(&mut self, path: &std::path::Path, cx: &mut Context<Self>) {
        self.liked.toggle(path);
        self.liked.save();
        if self.section == Section::Liked {
            self.clear_selection();
        }
        cx.notify();
    }

    fn liked_source(&self) -> SongSource {
        SongSource::Paths(self.liked.paths().into())
    }

    pub fn queue_open(&self) -> bool {
        self.show_queue
    }

    pub fn show_album(&mut self, album_id: String, cx: &mut Context<Self>) {
        self.navigate(Page::Album(album_id), cx);
    }

    fn cover(&self, album_id: Option<&str>, size: f32, name: &str, cx: &App) -> impl IntoElement {
        cover(&self.library, album_id, size, name, cx)
    }

    fn badged_cover(
        &self,
        album_id: &str,
        unmatched: bool,
        size: f32,
        name: &str,
        cx: &App,
    ) -> impl IntoElement {
        let theme = cx.omarchy().clone();
        div()
            .relative()
            .flex_none()
            .child(self.cover(Some(album_id), size, name, cx))
            .when(unmatched, |d| {
                d.child(with_tooltip(
                    div()
                        .id(ElementId::Name(
                            format!("album-unmatched-{album_id}").into(),
                        ))
                        .test_support()
                        .absolute()
                        .top(px(-4.))
                        .right(px(-4.))
                        .size(rems(1.125))
                        .rounded_full()
                        .bg(theme.background)
                        .border_1()
                        .border_color(theme.border)
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(theme.accent)
                        .child(icon(IconName::CircleAlert).size(rems(0.875))),
                    "Could not be identified online",
                ))
            })
    }

    fn lookup_status(&self, snap: &LibrarySnapshot, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = cx.omarchy().clone();
        if snap.lookup_total > 0 && snap.lookup_pending > 0 {
            let text = format!(
                "Looking up albums… {} of {}",
                snap.lookup_total - snap.lookup_pending,
                snap.lookup_total
            );
            return Some(
                div()
                    .id("lookup-status")
                    .test_support()
                    .aria_label(text.clone())
                    .px(rems(0.3125))
                    .text_size(rems(0.6875))
                    .text_color(theme.secondary)
                    .child(text)
                    .into_any_element(),
            );
        }
        if snap.lookup_unmatched.is_empty() {
            return None;
        }
        let n = snap.lookup_unmatched.len();
        let text = format!(
            "{n} album{} could not be identified",
            if n == 1 { "" } else { "s" }
        );
        Some(
            div()
                .flex()
                .flex_col()
                .gap(rems(0.25))
                .px(rems(0.3125))
                .child(
                    div()
                        .id("lookup-status")
                        .test_support()
                        .aria_label(text.clone())
                        .text_size(rems(0.75))
                        .text_color(theme.foreground)
                        .child(text),
                )
                .child(
                    div()
                        .id("lookup-unmatched-list")
                        .max_h(rems(8.))
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .text_size(rems(0.6875))
                        .text_color(theme.secondary)
                        .children(snap.lookup_unmatched.iter().map(|a| {
                            div()
                                .truncate()
                                .child(format!("{} – {}", a.artist, a.title))
                        })),
                )
                .child(div().child(
                    button("lookup-retry", "Retry all", ButtonVariant::Secondary, cx).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.library.retry_unmatched();
                            this.tick(true, cx);
                            cx.notify();
                        }),
                    ),
                ))
                .into_any_element(),
        )
    }

    fn navigate(&mut self, page: Page, cx: &mut Context<Self>) {
        if self.pages.last() != Some(&page) {
            self.pages.push(page);
            self.forward.clear();
            self.nav_seq += 1;
        }
        self.clear_selection();
        cx.notify();
    }

    fn back(&mut self, cx: &mut Context<Self>) {
        if let Some(page) = self.pages.pop() {
            self.forward.push(page);
            self.nav_seq += 1;
        }
        self.clear_selection();
        cx.notify();
    }

    fn go_forward(&mut self, cx: &mut Context<Self>) {
        if let Some(page) = self.forward.pop() {
            self.pages.push(page);
            self.nav_seq += 1;
        }
        self.clear_selection();
        cx.notify();
    }

    fn set_section(&mut self, section: Section, cx: &mut Context<Self>) {
        if self.section != section || !self.pages.is_empty() {
            self.nav_seq += 1;
        }
        self.section = section;
        self.confirm_delete = None;
        self.pages.clear();
        self.forward.clear();
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
            RowAction::AddToPlaylistMenu => {}
            RowAction::AddToPlaylist(id) => self.add_to_playlist(Some(id), &tracks, cx),
            RowAction::NewPlaylistWith => self.add_to_playlist(None, &tracks, cx),
            RowAction::RemoveFromPlaylist => {
                if let TrackList::Playlist(id, _) = list {
                    let paths: Vec<PathBuf> = tracks.iter().map(|t| t.path.clone()).collect();
                    self.playlists.remove(*id, &paths);
                    self.playlists.save();
                    self.clear_selection();
                }
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
        let picked = cx.prompt_for_new_path(&download_dir, Some(file_name));
        cx.spawn(async move |_, _cx| {
            if let Ok(Ok(Some(dest))) = picked.await {
                if let Err(e) = std::fs::copy(&path, &dest) {
                    eprintln!("export file: {e}");
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
            RowAction::AddToPlaylist(id) => self.add_to_playlist(Some(id), &tracks, cx),
            RowAction::NewPlaylistWith => self.add_to_playlist(None, &tracks, cx),
            RowAction::GoToAlbum
            | RowAction::GoToArtist
            | RowAction::Remove
            | RowAction::ExportFile
            | RowAction::AddToPlaylistMenu
            | RowAction::RemoveFromPlaylist => {}
        }
        cx.notify();
    }

    fn add_to_playlist(&mut self, id: Option<u64>, tracks: &[Track], cx: &mut Context<Self>) {
        if tracks.is_empty() {
            return;
        }
        let paths: Vec<PathBuf> = tracks.iter().map(|t| t.path.clone()).collect();
        let id = match id {
            Some(id) => {
                self.playlists.add(id, paths);
                id
            }
            None => {
                let name = self.playlists.untitled_name();
                self.playlists.create(&name, paths)
            }
        };
        self.playlists.save();
        if let Some(playlist) = self.playlists.get(id) {
            let msg = format!(
                "Added {} to “{}”",
                plural(tracks.len(), "song"),
                playlist.name
            );
            self.show_notify(msg, cx);
        }
    }

    fn context_menu(
        &self,
        id: impl Into<ElementId>,
        trigger: impl IntoElement,
        actions: &'static [RowAction],
        on_select: impl Fn(&mut Self, RowAction, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Popover {
        self.action_menu(id, trigger, actions, on_select, cx)
            .mouse_button(MouseButton::Right)
    }

    fn action_menu(
        &self,
        id: impl Into<ElementId>,
        trigger: impl IntoElement,
        actions: &'static [RowAction],
        on_select: impl Fn(&mut Self, RowAction, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Popover {
        let this = cx.entity().downgrade();
        let mut extra = vec![RowAction::NewPlaylistWith];
        extra.extend(
            self.playlists
                .items
                .iter()
                .map(|p| RowAction::AddToPlaylist(p.id)),
        );
        let names: Vec<String> = self
            .playlists
            .items
            .iter()
            .map(|p| p.name.clone())
            .collect();
        let items = actions
            .iter()
            .enumerate()
            .map(|(i, action)| {
                let mut item = action.item();
                if let RowAction::AddToPlaylistMenu = action {
                    let base = actions.len();
                    let mut children = vec![(base, RowAction::NewPlaylistWith.item())];
                    children.extend(names.iter().enumerate().map(|(j, name)| {
                        (
                            base + 1 + j,
                            MenuItem::new(name.clone()).icon(IconName::ListMusic),
                        )
                    }));
                    item = item.submenu(children);
                }
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
                let action = actions
                    .get(index)
                    .copied()
                    .or_else(|| extra.get(index - actions.len()).copied());
                if let Some(action) = action {
                    let _ = this.update(cx, |this, cx| on_select(this, action, cx));
                }
            },
        )
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
            )
            .test_support();
        let actions = match &list {
            _ if queue => QUEUE_ACTIONS,
            TrackList::Playlist(..) => PLAYLIST_TRACK_ACTIONS,
            _ => TRACK_ACTIONS,
        };
        self.context_menu(
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
            if self.library.query().trim().is_empty() {
                self.search_scope = SearchScope::All;
            }
            self.library.set_query(text.clone());
            self.pages.clear();
            self.clear_selection();
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

    fn clear_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.query.update(cx, |q, cx| q.set_value("", window, cx));
        self.pending_query.clear();
        self.search_task = None;
        self.searching = false;
        self.apply_text(String::new(), cx);
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

    /// Persist a new UI scale and repaint. The window rem size itself is
    /// applied in [`Self::render`] so every frame follows the setting.
    fn set_scale(&mut self, scale: f64, cx: &mut Context<Self>) {
        let scale = scale.clamp(UI_SCALE_MIN, UI_SCALE_MAX);
        if scale == self.prefs.ui_scale {
            return;
        }
        self.prefs.ui_scale = scale;
        AppConfig::update(|c| c.ui_scale = scale);
        cx.notify();
    }

    fn bump_scale(&mut self, steps: i32, cx: &mut Context<Self>) {
        self.set_scale(stepped_scale(self.prefs.ui_scale, steps), cx);
    }

    /// Current UI scale factor; 1.0 is the default text size.
    pub fn ui_scale(&self) -> f64 {
        self.prefs.ui_scale
    }

    /// Text currently in the search field.
    pub fn search_text(&self, cx: &App) -> String {
        self.query.read(cx).text().to_string()
    }

    /// Take focus back when nothing in the window has it, so shortcuts keep working.
    fn reclaim_focus(&self, window: &mut Window, cx: &mut App) {
        if window.focused(cx).is_none() {
            window.focus(&self.focus_handle, cx);
        }
    }

    fn text_size_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.omarchy().clone();
        let pct = format!("{}%", (self.prefs.ui_scale * 100.0).round() as i32);
        div()
            .flex()
            .items_center()
            .gap(rems(0.5))
            .py(rems(0.3125))
            .px(rems(0.3125))
            .child(
                div()
                    .flex_1()
                    .text_size(rems(0.75))
                    .text_color(theme.foreground)
                    .child("Text size"),
            )
            .child(
                button("font-smaller", "−", ButtonVariant::Secondary, cx)
                    .on_click(cx.listener(|this, _, _, cx| this.bump_scale(-1, cx))),
            )
            .child(with_tooltip(
                button("font-reset", pct, ButtonVariant::Secondary, cx)
                    .on_click(cx.listener(|this, _, _, cx| this.set_scale(1.0, cx))),
                "Reset to 100% (Ctrl+0)",
            ))
            .child(
                button("font-larger", "+", ButtonVariant::Secondary, cx)
                    .on_click(cx.listener(|this, _, _, cx| this.bump_scale(1, cx))),
            )
    }

    /// Sync the window rem size with the saved UI scale. Called at the top of
    /// every render; a no-op once they agree.
    fn apply_scale(&self, window: &mut Window) {
        let rem = rem_for_scale(self.prefs.ui_scale);
        if window.rem_size() != rem {
            window.set_rem_size(rem);
        }
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
                let lookup_before = this.prefs.online_lookup;
                change(&mut this.prefs, on);
                AppConfig::update(|c| change(c, on));
                crate::desktop::set_cover_icon(!this.prefs.plain_tray_icon);
                if this.prefs.online_lookup != lookup_before {
                    this.library.set_online_lookup(this.prefs.online_lookup);
                }
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
        .test_support()
        .flex()
        .items_center()
        .gap(rems(0.5))
        .px(rems(0.625))
        .py(rems(0.25))
        .rounded(rems(0.375))
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
        .child(icon(icon_name).size(rems(0.875)).flex_none())
        .child(div().min_w_0().truncate().child(label.to_string()))
        .on_click(on_click)
}

impl Render for Omamusic {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.apply_scale(window);
        self.reclaim_focus(window, cx);
        self.cover_poll.tick(&self.library, cx);
        let snap = self.library.snapshot();
        let theme = cx.omarchy().clone();

        focus_scope("omamusic")
            .track_focus(&self.focus_handle)
            .on_action(
                cx.listener(|this, _: &FocusSearch, window, cx| this.focus_search(window, cx)),
            )
            .on_action(cx.listener(|this, _: &GoBack, _, cx| this.back(cx)))
            .on_action(cx.listener(|this, _: &GoForward, _, cx| this.go_forward(cx)))
            .on_action(cx.listener(|this, _: &IncreaseFontSize, _, cx| this.bump_scale(1, cx)))
            .on_action(cx.listener(|this, _: &DecreaseFontSize, _, cx| this.bump_scale(-1, cx)))
            .on_action(cx.listener(|this, _: &ResetFontSize, _, cx| this.set_scale(1.0, cx)))
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Back),
                cx.listener(|this, _, _, cx| this.back(cx)),
            )
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Forward),
                cx.listener(|this, _, _, cx| this.go_forward(cx)),
            )
            .size_full()
            .bg(theme.background)
            .child(
                div()
                    .flex()
                    .size_full()
                    .min_w(px(480.))
                    .child(self.render_sidebar(&snap, window, cx))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(self.render_toolbar(window, cx))
                            .child(
                                div()
                                    .flex()
                                    .flex_1()
                                    .min_h_0()
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .min_h_0()
                                            .flex()
                                            .flex_col()
                                            .bg(theme.background)
                                            .child(if snap.folders.is_empty() {
                                                empty_state(
                                                    "No Music",
                                                    "Add a folder with music.",
                                                    cx,
                                                )
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
                                            } else if snap.album_count == 0 && snap.scanning {
                                                empty_state(
                                                    "Scanning…",
                                                    "Reading the music folders.",
                                                    cx,
                                                )
                                                .into_any_element()
                                            } else {
                                                {
                                                    let content = self.render_content(window, cx);
                                                    self.page_enter(content)
                                                }
                                            }),
                                    )
                                    .when(self.show_queue, |d| {
                                        d.child(self.render_queue_panel(cx))
                                    }),
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
    fn render_sidebar(
        &mut self,
        snap: &LibrarySnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.omarchy().clone();
        let heading = |label: &'static str| {
            div()
                .flex_1()
                .px(rems(0.625))
                .text_size(rems(0.625))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.secondary)
                .child(label)
        };
        let library_nav = [
            (
                "nav-recent",
                "Recently Added",
                IconName::Clock,
                Section::RecentlyAdded,
            ),
            ("nav-liked", "Liked Songs", IconName::Heart, Section::Liked),
            ("nav-songs", "Songs", IconName::Music, Section::Songs),
            ("nav-albums", "Albums", IconName::Disc3, Section::Albums),
            ("nav-artists", "Artists", IconName::Mic, Section::Artists),
            ("nav-genres", "Genres", IconName::Tag, Section::Genres),
        ];
        let library_items: Vec<AnyElement> = library_nav
            .into_iter()
            .map(|(id, label, icon_name, section)| {
                sidebar_btn(
                    id,
                    label,
                    icon_name,
                    self.section == section,
                    cx,
                    cx.listener(move |this, _, window, cx| {
                        this.clear_search(window, cx);
                        this.set_section(section, cx)
                    }),
                )
                .into_any_element()
            })
            .collect();
        let mut playlist_items: Vec<AnyElement> = Vec::new();
        for playlist in self.playlists.items.clone() {
            let id = playlist.id;
            if self.playlist_edit == Some(PlaylistEdit::Rename(id)) {
                playlist_items.push(self.playlist_name_field(window, cx));
                continue;
            }
            let item = sidebar_btn(
                ElementId::Name(format!("playlist-{id}").into()),
                &playlist.name,
                IconName::ListMusic,
                self.section == Section::Playlist(id),
                cx,
                cx.listener(move |this, _, window, cx| {
                    this.clear_search(window, cx);
                    this.set_section(Section::Playlist(id), cx)
                }),
            );
            let this = cx.entity().downgrade();
            playlist_items.push(
                menu(
                    ElementId::Name(format!("playlist-menu-{id}").into()),
                    MenuTrigger(item.into_any_element()),
                    vec![
                        MenuItem::new("Rename").icon(IconName::Pencil),
                        MenuItem::new("Delete Playlist…")
                            .icon(IconName::Trash)
                            .separator_before(),
                    ],
                    move |index, window, cx| {
                        let _ = this.update(cx, |this, cx| {
                            if index == 0 {
                                this.start_playlist_edit(PlaylistEdit::Rename(id), window, cx);
                            } else {
                                this.clear_search(window, cx);
                                this.set_section(Section::Playlist(id), cx);
                                this.confirm_delete = Some(id);
                            }
                        });
                    },
                )
                .mouse_button(MouseButton::Right)
                .w_full()
                .into_any_element(),
            );
        }
        if self.playlist_edit == Some(PlaylistEdit::New) {
            playlist_items.push(self.playlist_name_field(window, cx));
        } else if playlist_items.is_empty() {
            playlist_items.push(
                div()
                    .px(rems(0.625))
                    .py(rems(0.25))
                    .text_size(rems(0.6875))
                    .text_color(theme.secondary)
                    .child("No playlists yet")
                    .into_any_element(),
            );
        }
        let status = if snap.scanning {
            "Scanning…"
        } else {
            "Up to date"
        };
        div()
            .flex()
            .flex_col()
            .w(rems(SIDEBAR_REMS))
            .min_w(rems(8.))
            .flex_shrink_0()
            .border_r_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(
                div()
                    .id("sidebar")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p(rems(0.5))
                    .child(
                        div()
                            .flex()
                            .pt(rems(0.25))
                            .pb(rems(0.25))
                            .child(heading("LIBRARY")),
                    )
                    .children(library_items)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .pt(rems(0.75))
                            .child(heading("PLAYLISTS"))
                            .child(with_tooltip(
                                icon_button(
                                    "playlist-new",
                                    IconName::Plus,
                                    "New Playlist",
                                    ButtonVariant::Secondary,
                                    cx,
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.start_playlist_edit(PlaylistEdit::New, window, cx)
                                    },
                                )),
                                "New Playlist",
                            )),
                    )
                    .children(playlist_items),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(rems(0.125))
                    .px(rems(0.875))
                    .py(rems(0.625))
                    .border_t_1()
                    .border_color(theme.border)
                    .text_size(rems(0.6875))
                    .text_color(theme.secondary)
                    .child(library_counts_text(snap.album_count, snap.track_count))
                    .child(
                        div()
                            .id("library-status")
                            .test_support()
                            .aria_label(status)
                            .child(status),
                    ),
            )
            .into_any_element()
    }

    fn playlist_name_field(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("playlist-name-row")
            .px(rems(0.25))
            .test_support()
            .on_key_down(
                cx.listener(|this, event: &gpui_kit::KeyDownEvent, window, cx| {
                    if event.keystroke.key == "escape" {
                        cx.stop_propagation();
                        this.cancel_playlist_edit(window, cx);
                    }
                }),
            )
            .child(input("playlist-name", &self.playlist_name, window, cx))
            .into_any_element()
    }

    fn view_info(&self) -> Option<(SharedString, String)> {
        match self.pages.last() {
            Some(Page::Album(_)) => return None,
            Some(Page::Artist(name)) => {
                let albums = self.library.artist_albums(name).len();
                return Some((name.clone().into(), plural(albums, "album")));
            }
            Some(Page::Genre(name)) => {
                let source = SongSource::Genre(name.clone(), self.prefs.song_sort);
                return Some((name.clone().into(), self.songs_detail(&source)));
            }
            None => {}
        }
        if self.library.snapshot().filter.is_some() {
            return None;
        }
        Some(match self.section {
            Section::RecentlyAdded => (
                "Recently Added".into(),
                self.songs_detail(&SongSource::Library(SongSort::RECENT)),
            ),
            Section::Songs => (
                "Songs".into(),
                self.songs_detail(&SongSource::Library(self.prefs.song_sort)),
            ),
            Section::Liked => (
                "Liked Songs".into(),
                self.songs_detail(&self.liked_source()),
            ),
            Section::Albums => (
                "Albums".into(),
                count_text(self.library.album_total(), "album"),
            ),
            Section::Artists => (
                "Artists".into(),
                count_text(self.library.artist_total(), "artist"),
            ),
            Section::Genres => (
                "Genres".into(),
                count_text(self.library.genres().len(), "genre"),
            ),
            Section::Playlist(id) => {
                let playlist = self.playlists.get(id)?;
                let source = SongSource::Paths(playlist.tracks.clone().into());
                (playlist.name.clone().into(), self.songs_detail(&source))
            }
        })
    }

    fn songs_detail(&self, source: &SongSource) -> String {
        let (count, duration) = self.library.song_stats(source);
        span_text(count, duration)
    }

    fn render_toolbar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.omarchy().clone();
        let info = self.view_info();
        let controls = self.toolbar_controls(cx);
        let search_empty = self.query.read(cx).text().to_string().is_empty();
        let search_focused = self.query.read(cx).focus_handle(cx).is_focused(window);
        let field = input("search", &self.query, window, cx);
        let field = if self.searching {
            field.suffix(
                icon(IconName::Loader)
                    .size(rems(0.875))
                    .text_color(theme.secondary)
                    .with_animation(
                        "search-spinner",
                        Animation::new(Duration::from_millis(700)).repeat(),
                        |this, delta| this.opacity(0.4 + 0.6 * delta),
                    ),
            )
        } else if search_empty && !search_focused {
            field.suffix(keycaps(["Ctrl", "F"], cx))
        } else {
            field
        };
        div()
            .flex()
            .items_center()
            .gap(rems(0.5))
            .py(rems(0.5))
            .px(rems(0.5))
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(
                div()
                    .flex()
                    .flex_none()
                    .gap(rems(0.125))
                    .child(with_tooltip(
                        icon_button(
                            "nav-back",
                            IconName::ChevronLeft,
                            "Back",
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .disabled(self.pages.is_empty())
                        .on_click(cx.listener(|this, _, _, cx| this.back(cx))),
                        "Back (Alt+Left)",
                    ))
                    .child(with_tooltip(
                        icon_button(
                            "nav-forward",
                            IconName::ChevronRight,
                            "Forward",
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .disabled(self.forward.is_empty())
                        .on_click(cx.listener(|this, _, _, cx| this.go_forward(cx))),
                        "Forward (Alt+Right)",
                    )),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_baseline()
                    .gap(rems(0.5))
                    .when_some(info, |d, (title, detail)| {
                        d.child(
                            div()
                                .id("view-title")
                                .test_support()
                                .aria_label(title.clone())
                                .min_w_0()
                                .truncate()
                                .text_size(rems(0.9375))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme.foreground)
                                .child(title),
                        )
                        .child(
                            div()
                                .id("view-detail")
                                .test_support()
                                .aria_label(detail.clone())
                                .flex_none()
                                .text_size(rems(0.6875))
                                .text_color(theme.secondary)
                                .child(detail),
                        )
                    }),
            )
            .children(controls)
            .child(with_tooltip(
                icon_button(
                    "queue-toggle",
                    IconName::ListMusic,
                    "Up Next",
                    ButtonVariant::Secondary,
                    cx,
                )
                .selected(self.show_queue)
                .on_click(cx.listener(|this, _, _, cx| this.toggle_queue(cx))),
                "Up Next",
            ))
            .child(
                div()
                    .w(rems(15.))
                    .flex_shrink_1()
                    .min_w(rems(8.))
                    .child(field),
            )
            .child(with_tooltip(
                icon_button(
                    "rescan",
                    IconName::RefreshCw,
                    "Rescan library",
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.library.request_rescan();
                    cx.notify();
                })),
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
                .on_click(cx.listener(|this, _, _, cx| {
                    this.show_settings = !this.show_settings;
                    cx.notify();
                })),
                "Settings",
            ))
            .into_any_element()
    }

    fn toolbar_controls(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        if !self.pages.is_empty() || self.library.snapshot().filter.is_some() {
            return Vec::new();
        }
        match self.section {
            Section::Albums => {
                let view = self.prefs.album_view;
                vec![
                    self.album_sort_menu(self.prefs.album_sort, cx)
                        .into_any_element(),
                    div()
                        .flex()
                        .flex_none()
                        .gap(rems(0.125))
                        .child(with_tooltip(
                            icon_button(
                                "albums-view-grid",
                                IconName::LayoutGrid,
                                "Grid",
                                ButtonVariant::Secondary,
                                cx,
                            )
                            .selected(view == AlbumView::Grid)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_album_view(AlbumView::Grid, cx)
                            })),
                            "Grid",
                        ))
                        .child(with_tooltip(
                            icon_button(
                                "albums-view-list",
                                IconName::List,
                                "List",
                                ButtonVariant::Secondary,
                                cx,
                            )
                            .selected(view == AlbumView::List)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_album_view(AlbumView::List, cx)
                            })),
                            "List",
                        ))
                        .into_any_element(),
                ]
            }
            Section::Playlist(id) => {
                let Some(playlist) = self.playlists.get(id) else {
                    return Vec::new();
                };
                let paths: Arc<[PathBuf]> = playlist.tracks.clone().into();
                let empty = paths.is_empty();
                let play = |shuffle: bool, paths: Arc<[PathBuf]>| {
                    move |this: &mut Omamusic,
                          _: &ClickEvent,
                          _: &mut Window,
                          cx: &mut Context<Self>| {
                        let tracks = this.library.songs(&SongSource::Paths(paths.clone()));
                        if tracks.is_empty() {
                            return;
                        }
                        let start = if shuffle {
                            rand::thread_rng().gen_range(0..tracks.len())
                        } else {
                            0
                        };
                        this.player.set_shuffle(shuffle);
                        this.player.play(tracks, start);
                        cx.notify();
                    }
                };
                vec![
                    pill("playlist-play", IconName::Play, "Play", true, cx)
                        .disabled(empty)
                        .on_click(cx.listener(play(false, paths.clone())))
                        .into_any_element(),
                    pill("playlist-shuffle", IconName::Shuffle, "Shuffle", false, cx)
                        .disabled(empty)
                        .on_click(cx.listener(play(true, paths)))
                        .into_any_element(),
                    with_tooltip(
                        icon_button(
                            "playlist-rename",
                            IconName::Pencil,
                            "Rename Playlist",
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.start_playlist_edit(PlaylistEdit::Rename(id), window, cx)
                            },
                        )),
                        "Rename Playlist",
                    )
                    .into_any_element(),
                    with_tooltip(
                        icon_button(
                            "playlist-delete",
                            IconName::Trash,
                            "Delete Playlist",
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.confirm_delete = Some(id);
                            cx.notify();
                        })),
                        "Delete Playlist",
                    )
                    .into_any_element(),
                ]
            }
            _ => Vec::new(),
        }
    }

    fn render_playlist(&mut self, id: u64, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.omarchy().clone();
        let Some(playlist) = self.playlists.get(id).cloned() else {
            return empty_state("Playlist Not Found", "It may have been deleted.", cx)
                .into_any_element();
        };
        let confirm = (self.confirm_delete == Some(id)).then(|| {
            div()
                .id("playlist-confirm")
                .test_support()
                .aria_label(format!("Delete “{}”?", playlist.name))
                .flex()
                .items_center()
                .gap(rems(0.5))
                .px(rems(0.75))
                .py(rems(0.5))
                .border_b_1()
                .border_color(theme.border)
                .bg(theme.danger.opacity(0.08))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .items_baseline()
                        .gap(rems(0.5))
                        .child(
                            div()
                                .text_size(rems(0.75))
                                .text_color(theme.foreground)
                                .child(format!("Delete “{}”?", playlist.name)),
                        )
                        .child(
                            div()
                                .text_size(rems(0.6875))
                                .text_color(theme.secondary)
                                .child("The songs stay in your library."),
                        ),
                )
                .child(
                    button(
                        "playlist-delete-confirm",
                        "Delete",
                        ButtonVariant::Danger,
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.delete_playlist(id, cx))),
                )
                .child(
                    button(
                        "playlist-delete-cancel",
                        "Cancel",
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.confirm_delete = None;
                        cx.notify();
                    })),
                )
        });
        let body = if playlist.tracks.is_empty() {
            empty_state(
                "Empty Playlist",
                "Right-click a song and choose Add to Playlist.",
                cx,
            )
            .into_any_element()
        } else {
            self.render_song_table(
                "playlist-songs",
                TrackList::Playlist(id, playlist.tracks.into()),
                None,
                cx,
            )
        };
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .children(confirm)
            .child(body)
            .into_any_element()
    }

    fn render_genre_list(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let genres: Arc<[GenreEntry]> = self.library.genres().into();
        if genres.is_empty() {
            return empty_state("No Genres", "Songs with a genre tag show up here.", cx)
                .into_any_element();
        }
        let (columns, gap, card) = self.grid_columns(window);
        let rows = genres.len().div_ceil(columns);
        uniform_list(
            "genres",
            rows,
            cx.processor(move |this, range: Range<usize>, window, cx| {
                range
                    .map(|row| {
                        let start = row * columns;
                        let end = (start + columns).min(genres.len());
                        div().flex().gap(gap).px(gap).pt(gap).children(
                            (start..end)
                                .map(|idx| this.genre_tile(idx, &genres[idx], card, window, cx)),
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

    fn genre_tile(
        &self,
        idx: usize,
        genre: &GenreEntry,
        size: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Popover {
        let theme = cx.omarchy().clone();
        let (lift, on_hover) = hover_progress(
            ElementId::NamedInteger("genre-lift".into(), idx as u64),
            Transition::new(LIFT),
            window,
            cx,
        );
        let name = genre.name.clone();
        let open_name = genre.name.clone();
        let play_name = genre.name.clone();
        let menu_name = genre.name.clone();
        let sort = self.prefs.song_sort;
        let count = format!(
            "{} · {}",
            plural(genre.song_count, "song"),
            plural(genre.album_count, "album")
        );
        let tile = div()
            .id(("genre", idx))
            .test_support()
            .aria_label(name.clone())
            .w(px(size))
            .flex_none()
            .flex()
            .flex_col()
            .gap(rems(0.125))
            .cursor_pointer()
            .on_hover(on_hover)
            .on_click(
                cx.listener(move |this, _, _, cx| {
                    this.navigate(Page::Genre(open_name.clone()), cx)
                }),
            )
            .child(
                lifted(size, lift)
                    .child(self.genre_art(idx, genre, size, cx))
                    .child(
                        div()
                            .id(ElementId::Name(format!("genre-play-{name}").into()))
                            .test_support()
                            .aria_label(format!("Play {name}"))
                            .absolute()
                            .right(rems(0.5))
                            .bottom(px(4. + 4. * lift))
                            .size(rems(2.))
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(theme.accent)
                            .text_color(theme.on_accent)
                            .shadow_md()
                            .opacity(lift)
                            .child(icon(IconName::Play).size(rems(0.875)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.tracks_action(
                                    RowAction::Play,
                                    &TrackList::Source(SongSource::Genre(play_name.clone(), sort)),
                                    cx,
                                );
                            })),
                    ),
            )
            .child(
                div()
                    .text_size(rems(0.75))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.foreground)
                    .truncate()
                    .child(genre.name.clone()),
            )
            .child(
                div()
                    .id(("genre-count", idx))
                    .test_support()
                    .aria_label(count.clone())
                    .text_size(rems(0.6875))
                    .text_color(theme.secondary)
                    .truncate()
                    .child(count),
            );
        self.context_menu(
            ("genre-menu", idx),
            tile,
            ARTIST_ACTIONS,
            move |this, action, cx| {
                this.tracks_action(
                    action,
                    &TrackList::Source(SongSource::Genre(menu_name.clone(), sort)),
                    cx,
                )
            },
            cx,
        )
    }

    /// A square of album covers from the genre: four in a mosaic when the
    /// genre has that many covered albums, one filling the square otherwise,
    /// and a tag glyph when none of its albums has art.
    fn genre_art(&self, idx: usize, genre: &GenreEntry, size: f32, cx: &App) -> AnyElement {
        let theme = cx.omarchy().clone();
        let radius = cover_radius(size);
        let art = div()
            .id(("genre-art", idx))
            .test_support()
            .size(px(size))
            .flex_none()
            .rounded(radius)
            .bg(theme.hover_fill());
        let ids = &genre.cover_album_ids;
        if ids.len() >= crate::db::GENRE_COVERS {
            let cell = size / 2.;
            return art
                .flex()
                .flex_wrap()
                .overflow_hidden()
                .children(
                    ids.iter()
                        .take(crate::db::GENRE_COVERS)
                        .enumerate()
                        .map(|(i, id)| {
                            let corner = |d: Div| match i {
                                0 => d.rounded_tl(radius),
                                1 => d.rounded_tr(radius),
                                2 => d.rounded_bl(radius),
                                _ => d.rounded_br(radius),
                            };
                            corner(self.genre_cell(id, cell))
                        }),
                )
                .into_any_element();
        }
        match ids.first() {
            Some(id) => art
                .overflow_hidden()
                .child(self.genre_cell(id, size).rounded(radius))
                .into_any_element(),
            None => art
                .flex()
                .items_center()
                .justify_center()
                .child(
                    icon(IconName::Tag)
                        .size(px(size * 0.3))
                        .text_color(theme.secondary),
                )
                .into_any_element(),
        }
    }

    fn genre_cell(&self, album_id: &str, size: f32) -> Div {
        let cell = div().size(px(size)).flex_none().overflow_hidden();
        match self.library.covers().cached(album_id, size as u32 * 2) {
            Some(thumb) => cell.child(
                img(ImageSource::Render(thumb.image))
                    .size_full()
                    .object_fit(ObjectFit::Cover),
            ),
            None => cell,
        }
    }

    fn render_song_table(
        &mut self,
        id: &'static str,
        list: TrackList,
        sort: Option<SongSort>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let source = list_source(&list);
        let total = self.library.song_total(&source);
        let current = self.player.current().map(|t| t.path);
        let playing = self.player.state() == PlayerState::Playing;
        let highlight = matches!(source, SongSource::Search);
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(self.song_header(sort, cx))
            .child(
                uniform_list(
                    id,
                    total,
                    cx.processor(move |this, range: Range<usize>, window, cx| {
                        let start = range.start;
                        let tracks = this.library.song_page(&source, start, range.len());
                        let query = if highlight {
                            this.library.query()
                        } else {
                            String::new()
                        };
                        range
                            .map(|i| {
                                let track = tracks.get(i - start).cloned().unwrap_or_default();
                                let row = SongRow {
                                    index: i,
                                    list: &list,
                                    query: &query,
                                    current: current.as_deref(),
                                    playing,
                                };
                                this.song_row(&track, row, window, cx)
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .flex_1()
                .min_h_0(),
            )
            .into_any_element()
    }

    fn song_header(&self, sort: Option<SongSort>, cx: &mut Context<Self>) -> Div {
        let theme = cx.omarchy().clone();
        let cells: Vec<AnyElement> = SONG_COLUMNS
            .iter()
            .map(|&(column, label)| {
                let active = sort.is_some_and(|s| s.column == column);
                let descending = sort.is_some_and(|s| s.descending);
                let cell = song_cell(column, div())
                    .id(ElementId::Name(
                        format!("song-col-{}", label.to_lowercase()).into(),
                    ))
                    .flex()
                    .items_center()
                    .gap(rems(0.25))
                    .text_color(if active {
                        theme.foreground
                    } else {
                        theme.secondary
                    })
                    .when(column == SongColumn::Time, |d| d.justify_end())
                    .child(div().min_w_0().truncate().child(label.to_uppercase()))
                    .when(active, |d| {
                        d.child(
                            icon(if descending {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronUp
                            })
                            .size(rems(0.75)),
                        )
                    })
                    .when(sort.is_some(), |d| {
                        d.cursor_pointer()
                            .hover(|s| s.text_color(theme.foreground))
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.set_song_sort(column, cx)),
                            )
                    })
                    .test_support()
                    .aria_label(format!("Sort by {label}"));
                cell.into_any_element()
            })
            .collect();
        let mut cells = cells.into_iter();
        div()
            .flex()
            .items_center()
            .gap(rems(0.75))
            .mx(rems(0.375))
            .px(rems(0.5))
            .py(rems(0.375))
            .border_b_1()
            .border_color(theme.border)
            .text_size(rems(0.625))
            .font_weight(FontWeight::MEDIUM)
            .text_color(theme.secondary)
            .child(div().w(rems(2.)).flex_shrink_0().child("#"))
            .children(cells.by_ref().take(4))
            .child(div().w(rems(1.25)).flex_shrink_0())
            .child(div().w(rems(1.5)).flex_shrink_0())
            .children(cells)
    }

    fn song_row(
        &self,
        t: &Track,
        row: SongRow<'_>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Popover {
        let theme = cx.omarchy().clone();
        let i = row.index;
        let is_current = row.current.is_some_and(|p| p == t.path.as_path());
        let group: SharedString = format!("song-row-{i}").into();
        let (hover, on_hover) = hover_progress(
            ("song-hover", i),
            Transition::new(ROW_FILL).ease(gpui_kit::linear),
            window,
            cx,
        );
        let (bg, ink, muted) = if is_current {
            (theme.accent, theme.on_accent, theme.on_accent.opacity(0.8))
        } else {
            (
                row_fill(theme.hover_fill(), !i.is_multiple_of(2), hover),
                theme.foreground,
                theme.secondary,
            )
        };
        let number = if is_current {
            now_playing(row.playing, ink)
        } else {
            (i + 1).to_string().into_any_element()
        };
        let play_list = row.list.clone();
        let more_list = row.list.clone();
        let liked = self.liked.contains(&t.path);
        let like_path = t.path.clone();
        let heart = div()
            .id(("song-like", i))
            .test_support()
            .aria_label(if liked { "Unlike" } else { "Like" })
            .w(rems(1.25))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .text_color(if is_current {
                ink
            } else if liked {
                theme.accent
            } else {
                muted
            })
            .when(!liked, |d| {
                d.opacity(0.).group_hover(group.clone(), |s| s.opacity(1.))
            })
            .hover(|s| s.text_color(if is_current { ink } else { theme.accent }))
            .cursor_pointer()
            .child(icon(IconName::Heart).size(rems(0.8125)))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.toggle_like(&like_path, cx);
            }));
        let actions = match row.list {
            TrackList::Playlist(..) => PLAYLIST_TRACK_ACTIONS,
            _ => TRACK_ACTIONS,
        };
        let more = self.action_menu(
            ("song-more-menu", i),
            div()
                .id(("song-more", i))
                .size(rems(1.5))
                .flex()
                .items_center()
                .justify_center()
                .text_color(muted)
                .opacity(0.)
                .group_hover(group.clone(), |s| s.opacity(1.))
                .hover(|s| s.text_color(ink))
                .child(icon(IconName::Ellipsis).size(rems(0.875)))
                .test_support()
                .aria_label("More"),
            actions,
            move |this, action, cx| this.track_action(action, &more_list, i, false, cx),
            cx,
        );
        let text_cell = |column: SongColumn, text: String| {
            song_cell(column, div())
                .truncate()
                .text_size(rems(0.6875))
                .text_color(muted)
                .when(column == SongColumn::Time, |d| d.text_right())
                .child(text)
        };
        let content = div()
            .id(("song", i))
            .group(group.clone())
            .flex()
            .items_center()
            .gap(rems(0.75))
            .mx(rems(0.375))
            .px(rems(0.5))
            .py(rems(0.25))
            .rounded(rems(0.375))
            .relative()
            .bg(bg)
            .on_hover(on_hover)
            .cursor_pointer()
            .child(
                div()
                    .relative()
                    .w(rems(2.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .text_size(rems(0.6875))
                    .text_color(muted)
                    .child(
                        div()
                            .group_hover(group.clone(), |s| s.opacity(0.))
                            .child(number),
                    )
                    .child(
                        div()
                            .id(("song-play", i))
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left_0()
                            .flex()
                            .items_center()
                            .text_color(ink)
                            .opacity(0.)
                            .group_hover(group, |s| s.opacity(1.))
                            .child(icon(IconName::Play).size(rems(0.875)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.clear_selection();
                                this.track_action(RowAction::Play, &play_list, i, false, cx);
                            }))
                            .test_support()
                            .aria_label(format!("Play {}", t.title)),
                    ),
            )
            .child(
                song_cell(SongColumn::Title, div())
                    .truncate()
                    .text_size(rems(0.75))
                    .text_color(ink)
                    .child(highlighted(
                        &t.title,
                        row.query,
                        if is_current { ink } else { theme.accent },
                    )),
            )
            .child(text_cell(SongColumn::Artist, t.artist.clone()))
            .child(text_cell(SongColumn::Album, t.album.clone()))
            .child(text_cell(SongColumn::Genre, t.genre.clone()))
            .child(heart)
            .child(div().w(rems(1.5)).flex_shrink_0().child(more))
            .child(text_cell(SongColumn::Time, format_time(t.duration)));
        self.track_row("song", i, row.list.clone(), false, content, cx)
    }
}

struct SongRow<'a> {
    index: usize,
    list: &'a TrackList,
    query: &'a str,
    current: Option<&'a std::path::Path>,
    playing: bool,
}

fn list_source(list: &TrackList) -> SongSource {
    match list {
        TrackList::Source(source) => source.clone(),
        TrackList::Playlist(_, paths) => SongSource::Paths(paths.clone()),
        TrackList::Loaded(tracks) => {
            SongSource::Paths(tracks.iter().map(|t| t.path.clone()).collect())
        }
        TrackList::Album(_) | TrackList::Artist(_) => SongSource::Search,
    }
}

fn song_cell(column: SongColumn, cell: Div) -> Div {
    match column {
        SongColumn::Title => cell.flex_1().flex_grow(TITLE_GROW).min_w_0(),
        SongColumn::Artist | SongColumn::Album => {
            cell.flex_1().max_w(rems(SECONDARY_MAX_W)).min_w_0()
        }
        SongColumn::Genre => cell.flex_1().max_w(rems(7.)).min_w_0(),
        SongColumn::Time | SongColumn::Added => cell.w(rems(3.)).flex_shrink_0(),
    }
}

fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn count_text(n: usize, word: &str) -> String {
    format!("{} {word}{}", thousands(n), if n == 1 { "" } else { "s" })
}

fn format_span(secs: f64) -> String {
    let minutes = (secs.max(0.0) / 60.0).round() as u64;
    let (days, hours, mins) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    let unit = |n: u64, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    if days > 0 {
        format!("{} {}", unit(days, "day"), unit(hours, "hour"))
    } else if hours > 0 {
        format!("{} {mins} min", unit(hours, "hour"))
    } else {
        format!("{mins} min")
    }
}

fn span_text(count: usize, secs: f64) -> String {
    if count == 0 {
        return "No songs".into();
    }
    format!("{} · {}", count_text(count, "song"), format_span(secs))
}

impl Omamusic {
    fn render_content(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        match self.pages.last().cloned() {
            Some(Page::Album(id)) => {
                if let Some(album) = self.library.album_detail(&id) {
                    return self.render_album_detail(&album, window, cx);
                }
            }
            Some(Page::Artist(name)) => return self.render_artist_albums(&name, window, cx),
            Some(Page::Genre(name)) => {
                let sort = self.prefs.song_sort;
                return self.render_song_table(
                    "genre-songs",
                    TrackList::Source(SongSource::Genre(name, sort)),
                    Some(sort),
                    cx,
                );
            }
            None => {}
        }
        if self.library.snapshot().filter.is_some() {
            return self.render_search(window, cx);
        }
        match self.section {
            Section::RecentlyAdded => self.render_song_table(
                "recent-songs",
                TrackList::Source(SongSource::Library(SongSort::RECENT)),
                None,
                cx,
            ),
            Section::Songs => {
                let sort = self.prefs.song_sort;
                self.render_song_table(
                    "songs",
                    TrackList::Source(SongSource::Library(sort)),
                    Some(sort),
                    cx,
                )
            }
            Section::Liked => self.render_song_table(
                "liked-songs",
                TrackList::Source(self.liked_source()),
                None,
                cx,
            ),
            Section::Albums => self.render_albums_section(window, cx),
            Section::Artists => self.render_artist_list(cx),
            Section::Genres => self.render_genre_list(window, cx),
            Section::Playlist(id) => self.render_playlist(id, cx),
        }
    }

    /// Fades and settles freshly navigated content into place.
    fn page_enter(&self, content: AnyElement) -> AnyElement {
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(content)
            .with_animation(
                ElementId::NamedInteger("page".into(), self.nav_seq),
                Animation::new(PAGE_ENTER).with_easing(ease_out_quint()),
                |page, t| page.opacity(t).mt(px(6. * (1. - t))),
            )
            .into_any_element()
    }

    fn section_heading(title: impl Into<SharedString>, see_all: Option<Button>, cx: &App) -> Div {
        let theme = cx.omarchy().clone();
        div()
            .flex()
            .items_center()
            .gap(rems(0.5))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(rems(0.875))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.foreground)
                    .truncate()
                    .child(title.into()),
            )
            .children(see_all)
    }

    fn render_artist_albums(
        &mut self,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let albums = self.library.artist_albums(name);
        self.render_album_list("artist-albums", albums, window, cx)
    }

    fn grid_columns(&self, window: &Window) -> (usize, gpui_kit::Pixels, f32) {
        let rem = window.rem_size();
        let gap = rem * 0.75;
        let queue = if self.show_queue {
            rem * QUEUE_REMS + px(1.)
        } else {
            px(0.)
        };
        let width = window.viewport_size().width - rem * SIDEBAR_REMS - px(1.) - queue - gap * 2.;
        let card = rem * ALBUM_CARD_REMS;
        let columns = (((width + gap) / (card + gap)).floor() as usize).max(1);
        (columns, gap, card / px(1.))
    }

    fn render_albums_section(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let sort = self.prefs.album_sort;
        match self.prefs.album_view {
            AlbumView::Grid => self.render_album_grid("albums", sort, window, cx),
            AlbumView::List => self.render_album_rows(sort, cx),
        }
    }

    fn album_sort_menu(&self, sort: AlbumSort, cx: &mut Context<Self>) -> Popover {
        let this = cx.entity().downgrade();
        let trigger = button("album-sort", "", ButtonVariant::Secondary, cx)
            .accessibility_label(format!("Sort by {}", sort.label()))
            .child(format!("Sort: {}", sort.label()))
            .child(icon(IconName::ChevronDown).size(rems(0.75)));
        let items = AlbumSort::ALL
            .iter()
            .map(|&option| MenuItem::new(option.label()).checked(option == sort))
            .collect();
        menu(
            "album-sort-menu",
            MenuTrigger(trigger.into_any_element()),
            items,
            move |index, _, cx| {
                let _ = this.update(cx, |this, cx| {
                    this.set_album_sort(AlbumSort::ALL[index], cx)
                });
            },
        )
    }

    fn set_album_sort(&mut self, sort: AlbumSort, cx: &mut Context<Self>) {
        self.prefs.album_sort = sort;
        AppConfig::update(|c| c.album_sort = sort);
        cx.notify();
    }

    fn set_album_view(&mut self, view: AlbumView, cx: &mut Context<Self>) {
        self.prefs.album_view = view;
        AppConfig::update(|c| c.album_view = view);
        cx.notify();
    }

    fn render_album_grid(
        &mut self,
        id: &'static str,
        sort: AlbumSort,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (columns, gap, card) = self.grid_columns(window);
        let total = self.library.album_total();
        let rows = total.div_ceil(columns);
        uniform_list(
            id,
            rows,
            cx.processor(move |this, range: Range<usize>, window, cx| {
                range
                    .map(|row| {
                        let start = row * columns;
                        let end = (start + columns).min(total);
                        let albums = this.library.album_page(start, end - start, sort);
                        div()
                            .flex()
                            .gap(gap)
                            .px(gap)
                            .pt(gap)
                            .children((start..end).map(|idx| {
                                let album = albums.get(idx - start).cloned().unwrap_or_default();
                                this.album_card(&album, "album", idx, card, window, cx)
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
        let (columns, gap, card) = self.grid_columns(window);
        let albums: Arc<[AlbumSummary]> = albums.into();
        let rows = albums.len().div_ceil(columns);
        uniform_list(
            id,
            rows,
            cx.processor(move |this, range: Range<usize>, window, cx| {
                range
                    .map(|row| {
                        let start = row * columns;
                        let end = (start + columns).min(albums.len());
                        div()
                            .flex()
                            .gap(gap)
                            .px(gap)
                            .pt(gap)
                            .children((start..end).map(|idx| {
                                this.album_card(&albums[idx], "album", idx, card, window, cx)
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

    fn album_menu(
        &self,
        menu_id: ElementId,
        trigger: impl IntoElement,
        album: &AlbumSummary,
        cx: &mut Context<Self>,
    ) -> Popover {
        let key = album.id.clone();
        let artist = album.artist.clone();
        self.context_menu(
            menu_id,
            trigger,
            ALBUM_ACTIONS,
            move |this, action, cx| match action {
                RowAction::GoToArtist => this.navigate(Page::Artist(artist.clone()), cx),
                _ => this.tracks_action(action, &TrackList::Album(key.clone()), cx),
            },
            cx,
        )
    }

    fn album_card(
        &self,
        a: &AlbumSummary,
        id: &'static str,
        idx: usize,
        size: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Popover {
        let theme = cx.omarchy().clone();
        let key = a.id.clone();
        let play_key = a.id.clone();
        let (lift, on_hover) = hover_progress(
            ElementId::Name(format!("{id}-lift-{idx}").into()),
            Transition::new(LIFT),
            window,
            cx,
        );
        let subtitle = if a.year > 0 {
            format!("{} · {}", a.artist, a.year)
        } else {
            a.artist.clone()
        };
        let card = div()
            .id((id, idx))
            .test_support()
            .aria_label(a.title.clone())
            .w(px(size))
            .flex_none()
            .flex()
            .flex_col()
            .gap(rems(0.125))
            .cursor_pointer()
            .on_hover(on_hover)
            .on_click(
                cx.listener(move |this, _, _, cx| this.navigate(Page::Album(key.clone()), cx)),
            )
            .child(
                lifted(size, lift)
                    .child(self.badged_cover(&a.id, a.unmatched, size, &a.title, cx))
                    .child(
                        div()
                            .id(ElementId::Name(format!("album-play-{}", a.id).into()))
                            .test_support()
                            .aria_label(format!("Play {}", a.title))
                            .absolute()
                            .right(rems(0.5))
                            .bottom(px(4. + 4. * lift))
                            .size(rems(2.))
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(theme.accent)
                            .text_color(theme.on_accent)
                            .shadow_md()
                            .opacity(lift)
                            .child(icon(IconName::Play).size(rems(0.875)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.tracks_action(
                                    RowAction::Play,
                                    &TrackList::Album(play_key.clone()),
                                    cx,
                                );
                            })),
                    ),
            )
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
                    .child(subtitle),
            );
        self.album_menu(
            ElementId::Name(format!("{id}-menu-{idx}").into()),
            card,
            a,
            cx,
        )
    }

    fn render_album_rows(&mut self, sort: AlbumSort, cx: &mut Context<Self>) -> AnyElement {
        let total = self.library.album_total();
        uniform_list(
            "album-rows",
            total,
            cx.processor(move |this, range: Range<usize>, window, cx| {
                let start = range.start;
                let albums = this.library.album_page(start, range.len(), sort);
                range
                    .map(|idx| {
                        let album = albums.get(idx - start).cloned().unwrap_or_default();
                        this.album_row(&album, idx, window, cx)
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .flex_1()
        .min_h_0()
        .into_any_element()
    }

    fn album_row(
        &self,
        a: &AlbumSummary,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Popover {
        let theme = cx.omarchy().clone();
        let bg = self.list_row_fill(("album-row-hover", idx), idx, window, cx);
        let key = a.id.clone();
        let year = if a.year > 0 {
            a.year.to_string()
        } else {
            String::new()
        };
        let cell = |width: f32, text: String| {
            div()
                .w(rems(width))
                .flex_shrink_0()
                .text_size(rems(0.6875))
                .text_color(theme.secondary)
                .truncate()
                .child(text)
        };
        let row = div()
            .id(("album-row", idx))
            .test_support()
            .aria_label(a.title.clone())
            .flex()
            .items_center()
            .gap(rems(0.75))
            .mx(rems(0.375))
            .px(rems(0.5))
            .py(rems(0.3125))
            .rounded(rems(0.375))
            .bg(bg.0)
            .on_hover(bg.1)
            .cursor_pointer()
            .child(self.badged_cover(&a.id, a.unmatched, 36., &a.title, cx))
            .child(
                div()
                    .flex_1()
                    .flex_grow(TITLE_GROW)
                    .min_w_0()
                    .text_size(rems(0.75))
                    .text_color(theme.foreground)
                    .truncate()
                    .child(a.title.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .max_w(rems(SECONDARY_MAX_W))
                    .min_w_0()
                    .text_size(rems(0.6875))
                    .text_color(theme.secondary)
                    .truncate()
                    .child(a.artist.clone()),
            )
            .child(cell(3., year))
            .child(cell(4.5, plural(a.track_count, "song")))
            .on_click(
                cx.listener(move |this, _, _, cx| this.navigate(Page::Album(key.clone()), cx)),
            );
        self.album_menu(
            ElementId::Name(format!("album-row-menu-{idx}").into()),
            row,
            a,
            cx,
        )
        .w_full()
    }

    /// The striped, hover-eased fill of a plain list row and its hover listener.
    fn list_row_fill(
        &self,
        key: impl Into<ElementId>,
        idx: usize,
        window: &mut Window,
        cx: &mut App,
    ) -> (Hsla, impl Fn(&bool, &mut Window, &mut App) + 'static) {
        let theme = cx.omarchy().clone();
        let (hover, on_hover) = hover_progress(
            key,
            Transition::new(ROW_FILL).ease(gpui_kit::linear),
            window,
            cx,
        );
        (
            row_fill(theme.hover_fill(), !idx.is_multiple_of(2), hover),
            on_hover,
        )
    }

    fn render_artist_list(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let total = self.library.artist_total();
        uniform_list(
            "artists",
            total,
            cx.processor(move |this, range: Range<usize>, window, cx| {
                let start = range.start;
                let artists = this.library.artist_page(start, range.len());
                range
                    .map(|idx| {
                        let entry = artists.get(idx - start).cloned().unwrap_or_default();
                        this.artist_row(idx, &entry, window, cx)
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .flex_1()
        .min_h_0()
        .into_any_element()
    }
    fn artist_row(
        &self,
        idx: usize,
        entry: &ArtistEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Popover {
        let theme = cx.omarchy().clone();
        let bg = self.list_row_fill(("artist-hover", idx), idx, window, cx);
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
            .gap(rems(0.75))
            .mx(rems(0.375))
            .px(rems(0.5))
            .py(rems(0.3125))
            .rounded(rems(0.375))
            .bg(bg.0)
            .on_hover(bg.1)
            .cursor_pointer()
            .child(rounded_cover(
                &self.library,
                entry.cover_album_id.as_deref(),
                40.,
                &entry.name,
                px(20.),
                cx,
            ))
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
        self.context_menu(
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

    fn render_album_detail(
        &mut self,
        album: &Album,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.omarchy().clone();
        let total = album.tracks.iter().map(|t| t.duration).sum::<f64>();
        let meta = [
            if album.year > 0 {
                album.year.to_string()
            } else {
                String::new()
            },
            album.genre.clone(),
            plural(album.tracks.len(), "song"),
            format_minutes(total),
        ]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
        let current = self.player.current();
        let playing = self.player.state() == PlayerState::Playing;
        let tint = self
            .library
            .covers()
            .cached(&album.id, 400)
            .map(|thumb| thumb.tint);
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
        let linkable = !album.artist.is_empty() && album.artist != VARIOUS_ARTISTS;
        let more: Vec<AlbumSummary> = if linkable {
            self.library
                .artist_albums(&album.artist)
                .into_iter()
                .filter(|other| other.id != album.id)
                .collect()
        } else {
            Vec::new()
        };
        let (columns, gap, card) = self.grid_columns(window);
        let artist_label: AnyElement =
            if linkable {
                let name = album.artist.clone();
                button("album-artist", "", ButtonVariant::Secondary, cx)
                    .accessibility_label(album.artist.clone())
                    .child(album.artist.to_uppercase())
                    .px(rems(0.25))
                    .py(rems(0.125))
                    .ml(rems(-0.25))
                    .rounded(rems(0.25))
                    .text_size(rems(0.6875))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.accent)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.navigate(Page::Artist(name.clone()), cx)
                    }))
                    .into_any_element()
            } else {
                div()
                    .text_size(rems(0.6875))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.secondary)
                    .child(album.artist.to_uppercase())
                    .into_any_element()
            };
        let more_tracks = album.tracks.clone();
        let more_menu = self.action_menu(
            "album-more-menu",
            with_tooltip(
                icon_button(
                    "album-more",
                    IconName::Ellipsis,
                    "More",
                    ButtonVariant::Secondary,
                    cx,
                )
                .rounded_full(),
                "More",
            ),
            ALBUM_MORE_ACTIONS,
            move |this, action, cx| {
                this.tracks_action(action, &TrackList::Loaded(more_tracks.clone()), cx)
            },
            cx,
        );
        let header = div()
            .flex()
            .items_end()
            .gap(rems(1.5))
            .child(
                div()
                    .flex_none()
                    .rounded(cover_radius(200.))
                    .shadow_lg()
                    .child(self.badged_cover(&album.id, album.unmatched, 200., &album.title, cx)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(rems(0.25))
                    .child(div().flex().child(artist_label))
                    .child(
                        div()
                            .id("album-title")
                            .test_support()
                            .aria_label(album.title.clone())
                            .text_size(rems(1.75))
                            .font_weight(FontWeight::BOLD)
                            .line_height(rems(2.125))
                            .text_color(theme.foreground)
                            .child(album.title.clone()),
                    )
                    .child(
                        div()
                            .id("album-meta")
                            .test_support()
                            .aria_label(meta.clone())
                            .text_size(rems(0.75))
                            .text_color(theme.secondary)
                            .child(meta),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(rems(0.5))
                            .pt(rems(0.75))
                            .child(
                                pill("play-album", IconName::Play, "Play", true, cx)
                                    .on_click(cx.listener(play(false, album.tracks.clone()))),
                            )
                            .child(
                                pill("shuffle-album", IconName::Shuffle, "Shuffle", false, cx)
                                    .on_click(cx.listener(play(true, album.tracks.clone()))),
                            )
                            .child(more_menu),
                    ),
            );
        let rows = album.tracks.iter().enumerate().map(|(i, t)| {
            let is_current = current.as_ref().is_some_and(|c| c.path == t.path);
            let (hover, on_hover) = hover_progress(
                ("album-track-hover", i),
                Transition::new(ROW_FILL).ease(gpui_kit::linear),
                window,
                cx,
            );
            let (bg, ink, muted) = if is_current {
                (theme.accent, theme.on_accent, theme.on_accent.opacity(0.8))
            } else {
                (
                    row_fill(theme.hover_fill(), !i.is_multiple_of(2), hover),
                    theme.foreground,
                    theme.secondary,
                )
            };
            let number = if is_current {
                now_playing(playing, ink)
            } else if t.track_number > 0 {
                t.track_number.to_string().into_any_element()
            } else {
                "".into_any_element()
            };
            let group: SharedString = format!("album-track-{i}").into();
            let tracks = album.tracks.clone();
            let row = div()
                .id(("album-track", i))
                .group(group.clone())
                .flex()
                .items_center()
                .gap(rems(0.75))
                .px(rems(0.5))
                .py(rems(0.4375))
                .rounded(rems(0.375))
                .bg(bg)
                .on_hover(on_hover)
                .cursor_pointer()
                .child(
                    div()
                        .relative()
                        .w(rems(2.))
                        .flex()
                        .justify_end()
                        .text_color(muted)
                        .child(
                            div()
                                .group_hover(group.clone(), |s| s.opacity(0.))
                                .child(number),
                        )
                        .child(
                            div()
                                .id(("album-track-play", i))
                                .test_support()
                                .aria_label(format!("Play {}", t.title))
                                .absolute()
                                .top_0()
                                .bottom_0()
                                .right_0()
                                .flex()
                                .items_center()
                                .text_color(ink)
                                .opacity(0.)
                                .group_hover(group, |s| s.opacity(1.))
                                .child(icon(IconName::Play).size(rems(0.875)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.player.play(tracks.to_vec(), i);
                                    cx.notify();
                                })),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .gap(rems(0.5))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_color(ink)
                                .child(t.title.clone()),
                        )
                        .when(t.artist != album.artist, |d| {
                            d.child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(muted)
                                    .child(t.artist.clone()),
                            )
                        }),
                )
                .child(div().text_color(muted).child(format_time(t.duration)));
            self.track_row(
                "album-track-menu",
                i,
                TrackList::Loaded(album.tracks.clone()),
                false,
                row,
                cx,
            )
        });
        let rows: Vec<Popover> = rows.collect();
        let more_section =
            (!more.is_empty()).then(|| {
                let name = album.artist.clone();
                let see_all = button("album-more-by-all", "See All", ButtonVariant::Secondary, cx)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.navigate(Page::Artist(name.clone()), cx)
                    }));
                div()
                    .flex()
                    .flex_col()
                    .gap(rems(0.5))
                    .pt(rems(0.5))
                    .child(Self::section_heading(
                        format!("More by {}", album.artist),
                        Some(see_all),
                        cx,
                    ))
                    .child(
                        div().flex().gap(gap).overflow_hidden().children(
                            more.iter()
                                .take(columns)
                                .enumerate()
                                .map(|(i, a)| self.album_card(a, "more-by", i, card, window, cx))
                                .collect::<Vec<_>>(),
                        ),
                    )
            });
        div()
            .id("album-detail-scroll")
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p(rems(1.5))
            .when_some(tint, |d, tint| {
                d.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(rems(22.))
                        .bg(linear_gradient(
                            180.,
                            linear_color_stop(tint_color(tint, 0.3), 0.),
                            linear_color_stop(tint_color(tint, 0.), 1.),
                        ))
                        .with_animation(
                            ElementId::Name(format!("album-tint-{}", album.id).into()),
                            Animation::new(TINT_ENTER).with_easing(ease_out_quint()),
                            |wash, t| wash.opacity(t),
                        ),
                )
            })
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .gap(rems(1.5))
                    .child(header)
                    .child(div().flex().flex_col().text_size(rems(0.75)).children(rows))
                    .children(more_section),
            )
            .into_any_element()
    }

    fn current_top_result(&self) -> Option<TopResult> {
        let query = self.library.query();
        let artists = self.library.artist_page(0, SEARCH_TOP_CANDIDATES);
        let albums = self
            .library
            .album_page(0, SEARCH_TOP_CANDIDATES, self.prefs.album_sort);
        let tracks = self.library.track_page(0, SEARCH_TOP_CANDIDATES);
        top_result(&query, &artists, &albums, &tracks)
    }

    fn play_top_result(&mut self, cx: &mut Context<Self>) {
        if self.searching {
            self.apply_pending_query(cx);
        }
        if self.library.snapshot().filter.is_none() {
            return;
        }
        match self.current_top_result() {
            Some(TopResult::Artist(artist)) => {
                self.tracks_action(RowAction::Play, &TrackList::Artist(artist.name), cx)
            }
            Some(TopResult::Album(album)) => {
                self.tracks_action(RowAction::Play, &TrackList::Album(album.id), cx)
            }
            Some(TopResult::Track(index, _)) => {
                self.player.play(self.library.tracks_for_play(), index);
                cx.notify();
            }
            None => {}
        }
    }

    fn open_top_result(&mut self, top: &TopResult, cx: &mut Context<Self>) {
        match top {
            TopResult::Artist(artist) => self.navigate(Page::Artist(artist.name.clone()), cx),
            TopResult::Album(album) => self.navigate(Page::Album(album.id.clone()), cx),
            TopResult::Track(index, _) => {
                self.player.play(self.library.tracks_for_play(), *index);
                cx.notify();
            }
        }
    }

    fn set_search_scope(&mut self, scope: SearchScope, cx: &mut Context<Self>) {
        self.search_scope = scope;
        self.clear_selection();
        cx.notify();
    }

    fn render_search(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.omarchy().clone();
        let query = self.library.query().trim().to_string();
        let songs = self.library.track_total();
        let albums = self.library.album_total();
        let artists = self.library.artist_total();
        let total = songs + albums + artists;
        if total == 0 {
            return empty_state("No Results", format!("Nothing matches “{query}”."), cx)
                .into_any_element();
        }
        let scope = self.search_scope;
        let chips: Vec<Button> = [
            (
                "search-scope-all",
                "All".to_string(),
                SearchScope::All,
                total,
            ),
            (
                "search-scope-songs",
                format!("Songs · {songs}"),
                SearchScope::Songs,
                songs,
            ),
            (
                "search-scope-albums",
                format!("Albums · {albums}"),
                SearchScope::Albums,
                albums,
            ),
            (
                "search-scope-artists",
                format!("Artists · {artists}"),
                SearchScope::Artists,
                artists,
            ),
        ]
        .into_iter()
        .map(|(id, label, option, count)| {
            button(id, label, ButtonVariant::Secondary, cx)
                .selected(scope == option)
                .disabled(count == 0)
                .on_click(cx.listener(move |this, _, _, cx| this.set_search_scope(option, cx)))
        })
        .collect();
        let summary = format!("{} for “{query}”", plural(total, "result"));
        let header = div()
            .flex()
            .flex_col()
            .gap(rems(0.5))
            .px(rems(0.75))
            .pt(rems(0.75))
            .pb(rems(0.5))
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .gap(rems(0.5))
                    .child(
                        div()
                            .text_size(rems(1.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.foreground)
                            .child("Search"),
                    )
                    .child(
                        div()
                            .id("search-summary")
                            .test_support()
                            .aria_label(summary.clone())
                            .min_w_0()
                            .truncate()
                            .text_size(rems(0.6875))
                            .text_color(theme.secondary)
                            .child(summary),
                    ),
            )
            .child(div().flex().flex_wrap().gap(rems(0.25)).children(chips));
        let body = match scope {
            SearchScope::All => self.render_search_overview(&query, window, cx),
            SearchScope::Songs => self.render_song_table(
                "search-songs",
                TrackList::Source(SongSource::Search),
                None,
                cx,
            ),
            SearchScope::Albums => {
                self.render_album_grid("search-albums", self.prefs.album_sort, window, cx)
            }
            SearchScope::Artists => self.render_artist_list(cx),
        };
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn render_search_overview(
        &mut self,
        query: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (columns, gap, card) = self.grid_columns(window);
        let song_total = self.library.track_total();
        let album_total = self.library.album_total();
        let artist_total = self.library.artist_total();
        let tracks = self.library.track_page(0, SEARCH_PREVIEW_SONGS);
        let albums = self.library.album_page(0, columns, self.prefs.album_sort);
        let artists = self.library.artist_page(0, SEARCH_PREVIEW_ARTISTS);
        let top = self.current_top_result();
        let see_all = |id: &'static str, scope: SearchScope, cx: &mut Context<Self>| {
            button(id, "See All", ButtonVariant::Secondary, cx)
                .on_click(cx.listener(move |this, _, _, cx| this.set_search_scope(scope, cx)))
        };
        let mut sections: Vec<AnyElement> = Vec::new();
        if let Some(top) = top {
            sections.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(rems(0.5))
                    .px(rems(0.75))
                    .child(Self::section_heading("Top Result", None, cx))
                    .child(self.top_result_card(top, query, cx))
                    .into_any_element(),
            );
        }
        if !tracks.is_empty() {
            let more = (song_total > tracks.len())
                .then(|| see_all("search-songs-all", SearchScope::Songs, cx));
            let list = TrackList::Source(SongSource::Search);
            let current = self.player.current().map(|t| t.path);
            let playing = self.player.state() == PlayerState::Playing;
            let rows: Vec<Popover> = tracks
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let row = SongRow {
                        index: i,
                        list: &list,
                        query,
                        current: current.as_deref(),
                        playing,
                    };
                    self.song_row(t, row, window, cx)
                })
                .collect();
            sections.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(rems(0.25))
                    .child(
                        div()
                            .px(rems(0.75))
                            .child(Self::section_heading("Songs", more, cx)),
                    )
                    .child(div().flex().flex_col().children(rows))
                    .into_any_element(),
            );
        }
        if !albums.is_empty() {
            let more = (album_total > albums.len())
                .then(|| see_all("search-albums-all", SearchScope::Albums, cx));
            let cards: Vec<Popover> = albums
                .iter()
                .enumerate()
                .map(|(i, a)| self.album_card(a, "search-album", i, card, window, cx))
                .collect();
            sections.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(rems(0.5))
                    .child(
                        div()
                            .px(rems(0.75))
                            .child(Self::section_heading("Albums", more, cx)),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(gap)
                            .px(gap)
                            .overflow_hidden()
                            .children(cards),
                    )
                    .into_any_element(),
            );
        }
        if !artists.is_empty() {
            let more = (artist_total > artists.len())
                .then(|| see_all("search-artists-all", SearchScope::Artists, cx));
            let rows: Vec<Popover> = artists
                .iter()
                .enumerate()
                .map(|(i, entry)| self.artist_row(i, entry, window, cx))
                .collect();
            sections.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(rems(0.25))
                    .child(
                        div()
                            .px(rems(0.75))
                            .child(Self::section_heading("Artists", more, cx)),
                    )
                    .child(div().flex().flex_col().children(rows))
                    .into_any_element(),
            );
        }
        div()
            .id("search-overview")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .py(rems(0.75))
            .child(div().flex().flex_col().gap(rems(1.25)).children(sections))
            .into_any_element()
    }

    fn top_result_card(&self, top: TopResult, query: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.omarchy().clone();
        let (kind, name, detail, cover_id) = match &top {
            TopResult::Artist(artist) => (
                "ARTIST",
                artist.name.clone(),
                format!(
                    "{} · {}",
                    plural(artist.album_count, "album"),
                    plural(artist.song_count, "song")
                ),
                artist.cover_album_id.clone(),
            ),
            TopResult::Album(album) => (
                "ALBUM",
                album.title.clone(),
                if album.year > 0 {
                    format!("{} · {}", album.artist, album.year)
                } else {
                    album.artist.clone()
                },
                Some(album.id.clone()),
            ),
            TopResult::Track(_, track) => (
                "SONG",
                track.title.clone(),
                format!("{} · {}", track.artist, track.album),
                Some(track.album_id.clone()),
            ),
        };
        let open = top.clone();
        div()
            .id("search-top-result")
            .test_support()
            .aria_label(name.clone())
            .flex()
            .items_center()
            .gap(rems(1.))
            .p(rems(0.875))
            .w(rems(28.))
            .max_w_full()
            .border_1()
            .border_color(theme.border)
            .hover(|s| s.bg(theme.hover_fill()))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| this.open_top_result(&open, cx)))
            .child(self.cover(cover_id.as_deref(), 88., &name, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(rems(0.25))
                    .child(
                        div()
                            .text_size(rems(0.625))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.secondary)
                            .child(kind),
                    )
                    .child(
                        div()
                            .text_size(rems(1.125))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.foreground)
                            .truncate()
                            .child(highlighted(&name, query, theme.accent)),
                    )
                    .child(
                        div()
                            .text_size(rems(0.75))
                            .text_color(theme.secondary)
                            .truncate()
                            .child(detail),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap(rems(0.375))
                    .child(
                        button("search-top-play", "", ButtonVariant::Primary, cx)
                            .accessibility_label(format!("Play {name}"))
                            .child(icon(IconName::Play).size(rems(0.75)))
                            .child("Play")
                            .on_click(cx.listener(|this, _, _, cx| {
                                cx.stop_propagation();
                                this.play_top_result(cx);
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(rems(0.25))
                            .text_size(rems(0.625))
                            .text_color(theme.secondary)
                            .child("or press")
                            .child(keycap("Enter", cx)),
                    ),
            )
            .into_any_element()
    }

    fn render_queue_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.omarchy().clone();
        let (queue, index) = self.player.queue();
        let playing = self.player.state() == PlayerState::Playing;
        let summary = span_text(queue.len(), queue.iter().map(|t| t.duration).sum());
        let header = div()
            .flex()
            .items_center()
            .gap(rems(0.5))
            .px(rems(0.75))
            .py(rems(0.5))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(rems(0.8125))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.foreground)
                            .child("Up Next"),
                    )
                    .child(
                        div()
                            .id("queue-summary")
                            .test_support()
                            .aria_label(summary.clone())
                            .text_size(rems(0.6875))
                            .text_color(theme.secondary)
                            .truncate()
                            .child(summary),
                    ),
            )
            .when(!queue.is_empty(), |d| {
                d.child(
                    button("clear-queue", "Clear", ButtonVariant::Secondary, cx).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.player.stop();
                            this.clear_selection();
                            cx.notify();
                        }),
                    ),
                )
            });
        let body = if queue.is_empty() {
            div()
                .id("queue-empty")
                .test_support()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .px(rems(1.))
                .text_size(rems(0.75))
                .text_color(theme.secondary)
                .text_center()
                .child("Songs you play or queue show up here.")
                .into_any_element()
        } else {
            uniform_list(
                "queue",
                queue.len(),
                cx.processor(move |this, range: Range<usize>, window, cx| {
                    range
                        .map(|i| this.queue_row(i, &queue, index, playing, window, cx))
                        .collect::<Vec<_>>()
                }),
            )
            .flex_1()
            .min_h_0()
            .px(rems(0.375))
            .pb(rems(0.375))
            .into_any_element()
        };
        div()
            .id("queue-panel")
            .test_support()
            .aria_label("Up Next")
            .relative()
            .flex()
            .flex_col()
            .w(rems(QUEUE_REMS))
            .flex_shrink_0()
            .border_l_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(header)
            .child(body)
            .with_animation(
                ElementId::NamedInteger("queue-panel".into(), self.queue_seq),
                Animation::new(PANEL_ENTER).with_easing(ease_out_quint()),
                |panel, t| panel.opacity(t).left(px(16. * (1. - t))),
            )
            .into_any_element()
    }

    fn queue_row(
        &self,
        i: usize,
        queue: &Arc<[Track]>,
        index: Option<usize>,
        playing: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Popover {
        let theme = cx.omarchy().clone();
        let t = &queue[i];
        let is_current = index == Some(i);
        let past = index.is_some_and(|current| i < current);
        let (hover, on_hover) = hover_progress(
            ("queue-row-hover", i),
            Transition::new(ROW_FILL).ease(gpui_kit::linear),
            window,
            cx,
        );
        let (rest, title_color, sub_color) = if is_current {
            (theme.accent, theme.on_accent, theme.on_accent.opacity(0.8))
        } else if past {
            (theme.background, theme.secondary, theme.secondary)
        } else {
            (theme.background, theme.foreground, theme.secondary)
        };
        let row = div()
            .id(("queue-row", i))
            .flex()
            .items_center()
            .gap(rems(0.5))
            .px(rems(0.375))
            .py(rems(0.25))
            .rounded(rems(0.375))
            .text_size(rems(0.75))
            .bg(if is_current {
                rest
            } else {
                row_fill(theme.hover_fill(), false, hover)
            })
            .on_hover(on_hover)
            .cursor_pointer()
            .child(div().relative().flex_none().child(self.cover(
                Some(&t.album_id),
                32.,
                &t.album,
                cx,
            )))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(rems(0.25))
                            .when(is_current, |d| d.child(now_playing(playing, title_color)))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(title_color)
                                    .child(t.title.clone()),
                            ),
                    )
                    .child(
                        div()
                            .text_size(rems(0.6875))
                            .text_color(sub_color)
                            .truncate()
                            .child(t.artist.clone()),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(rems(0.6875))
                    .text_color(sub_color)
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
        let theme = cx.omarchy().clone();
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
                            ))
                            .child(self.pref_switch(
                                "pref-online-lookup",
                                "Complete missing tags and covers online",
                                self.prefs.online_lookup,
                                |c, on| c.online_lookup = on,
                                cx,
                            ))
                            .child(
                                div()
                                    .px(rems(0.3125))
                                    .text_size(rems(0.6875))
                                    .text_color(theme.secondary)
                                    .child(
                                        "Sends album and artist names to MusicBrainz, Apple and \
                                         Deezer, and audio fingerprints to AcoustID when a key is \
                                         set. Writes covers and missing tags into your music files.",
                                    ),
                            )
                            .when(self.prefs.acoustid_key.is_empty(), |d| {
                                d.child(
                                    div()
                                        .id("lookup-acoustid-hint")
                                        .test_support()
                                        .px(rems(0.3125))
                                        .text_size(rems(0.6875))
                                        .text_color(theme.secondary)
                                        .child(
                                            "Untagged files can also be identified by sound: get a \
                                             free AcoustID application key, put it in config.json \
                                             as \"acoustid_key\" and restart.",
                                        ),
                                )
                            })
                            .children(self.lookup_status(snap, cx))
                            .child(self.text_size_row(cx)),
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
    use crate::TogglePlay;
    use gpui_kit::{Keystroke, TestAppContext, VisualTestContext};
    use std::cell::Cell;
    use std::rc::Rc;

    fn setup<'a>(
        cx: &'a mut TestAppContext,
        toggles: &Rc<Cell<usize>>,
    ) -> (Entity<Omamusic>, &'a mut VisualTestContext) {
        cx.update(|cx| {
            gpui_omarchy::init(cx);
            gpui_omarchy::Theme::tokyo_night().apply(cx);
            cx.bind_keys(crate::key_bindings());
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

    #[test]
    fn ui_scale_steps_round_and_clamp() {
        assert_eq!(stepped_scale(1.0, 1), 1.1);
        assert_eq!(stepped_scale(1.0, -1), 0.9);
        assert_eq!(stepped_scale(1.0, 0), 1.0);
        // No float drift from repeated steps: 1.1 + 0.1 is exactly 1.2.
        assert_eq!(stepped_scale(stepped_scale(1.0, 1), 1), 1.2);
        assert_eq!(stepped_scale(2.0, 1), UI_SCALE_MAX);
        assert_eq!(stepped_scale(0.75, -5), UI_SCALE_MIN);
        assert_eq!(rem_for_scale(1.0), px(16.0));
        assert_eq!(rem_for_scale(1.5), px(24.0));
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
    fn library_spans_read_in_days_hours_and_minutes() {
        assert_eq!(thousands(1284), "1,284");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000_000), "1,000,000");
        assert_eq!(format_span(15.0 * 60.0), "15 min");
        assert_eq!(format_span(75.0 * 60.0), "1 hour 15 min");
        assert_eq!(format_span((3.0 * 24.0 + 4.0) * 3600.0), "3 days 4 hours");
        assert_eq!(span_text(0, 0.0), "No songs");
        assert_eq!(span_text(1284, 3600.0), "1,284 songs · 1 hour 0 min");
    }

    #[test]
    fn album_length_reads_in_minutes_then_hours() {
        assert_eq!(format_minutes(0.0), "0 min");
        assert_eq!(format_minutes(200.0), "3 min");
        assert_eq!(format_minutes(42.0 * 60.0 + 20.0), "42 min");
        assert_eq!(format_minutes(72.0 * 60.0), "1 hr 12 min");
        assert_eq!(plural(1, "song"), "1 song");
        assert_eq!(plural(2, "song"), "2 songs");
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
