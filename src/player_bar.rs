use crate::app::{cover, format_time, hover_progress, CoverPoll, Omamusic, LIFT};
use crate::config::AppConfig;
use crate::library::Library;
use crate::models::{PlayerState, RepeatMode};
use crate::player::Player;
use gpui_kit::base::motion::Transition;
use gpui_kit::base::slider::{SliderEvent, SliderState};
use gpui_kit::base::{Slider, SliderIndicator, SliderThumb, SliderTrack};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::TestSupportExt;
use gpui_kit::{
    div, ease_out_quint, px, relative, rems, Animation, AnimationExt as _, App, AppContext,
    Context, ElementId, Entity, FontWeight, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, WeakEntity, Window,
};
use gpui_omarchy::{
    button, icon, icon_button, with_tooltip, ActiveTheme, Button, ButtonVariant, IconName,
};
use std::sync::Arc;
use std::time::Duration;

const PROGRESS_STEPS: f32 = 1000.;
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const IDLE_INTERVAL: Duration = Duration::from_millis(250);
const LANE_REMS: f32 = 16.;
const WIDE_REMS: f32 = 56.;

pub struct PlayerBar {
    library: Arc<Library>,
    player: Arc<Player>,
    main: WeakEntity<Omamusic>,
    volume: Entity<SliderState>,
    progress: Entity<SliderState>,
    seeking: bool,
    cover_poll: CoverPoll,
}

impl PlayerBar {
    pub fn new(
        library: Arc<Library>,
        player: Arc<Player>,
        main: WeakEntity<Omamusic>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let vol = player.volume() as f32;
        let volume = cx.new(|_| {
            SliderState::new()
                .min(0.)
                .max(1.)
                .step(0.01)
                .default_value(vol)
        });
        let progress = cx.new(|_| {
            SliderState::new()
                .min(0.)
                .max(PROGRESS_STEPS)
                .step(1.)
                .default_value(0.)
        });

        cx.subscribe(&volume, |this, _, event: &SliderEvent, cx| match event {
            SliderEvent::Change(value) => {
                this.player.set_volume(value.end() as f64);
                cx.notify();
            }
            SliderEvent::Release(_) => {
                let volume = this.player.volume();
                AppConfig::update(|c| c.volume = volume);
                cx.notify();
            }
        })
        .detach();

        cx.subscribe(&progress, |this, _, event: &SliderEvent, _| match event {
            SliderEvent::Change(_) => this.seeking = true,
            SliderEvent::Release(value) => {
                this.seeking = false;
                let dur = this.player.duration().max(1.0);
                this.player
                    .seek(value.end() as f64 / PROGRESS_STEPS as f64 * dur);
            }
        })
        .detach();

        cx.spawn_in(window, async move |this, cx| loop {
            let Ok(delay) = this.update_in(cx, |this, window, cx| this.advance(window, cx)) else {
                break;
            };
            cx.background_executor().timer(delay).await;
        })
        .detach();

        if let Some(main) = main.upgrade() {
            cx.observe(&main, |_, _, cx| cx.notify()).detach();
        }

        let changes = player.subscribe();
        cx.spawn_in(window, async move |this, cx| {
            while changes.recv().await.is_ok() {
                if this
                    .update_in(cx, |this, window, cx| this.sync(window, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        Self {
            library,
            player,
            main,
            volume,
            progress,
            seeking: false,
            cover_poll: CoverPoll::default(),
        }
    }

    fn advance(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Duration {
        if self.player.state() != PlayerState::Playing {
            return IDLE_INTERVAL;
        }
        self.sync(window, cx);
        let pixels = f32::from(window.viewport_size().width) * window.scale_factor();
        Duration::from_secs_f64(self.player.duration() / pixels.max(1.) as f64)
            .clamp(FRAME_INTERVAL, IDLE_INTERVAL)
    }

    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.seeking {
            let dur = self.player.duration().max(1.0) as f32;
            let value =
                (self.player.position() as f32 / dur * PROGRESS_STEPS).clamp(0., PROGRESS_STEPS);
            self.progress
                .update(cx, |s, cx| s.set_value(value, window, cx));
        }
        cx.notify();
    }

    fn control_button(
        id: &'static str,
        icon_name: IconName,
        label: &'static str,
        disabled: bool,
        action: fn(&Player),
        cx: &mut Context<Self>,
    ) -> Button {
        icon_button(id, icon_name, label, ButtonVariant::Secondary, cx)
            .rounded(rems(0.375))
            .disabled(disabled)
            .on_click(cx.listener(move |this, _, _, cx| {
                action(&this.player);
                cx.notify();
            }))
    }
}

fn line_slider(state: &Entity<SliderState>, disabled: bool, hover: f32, cx: &App) -> Slider {
    let t = cx.omarchy().clone();
    let percentage = state.read(cx).percentage().end;
    let line = 3. + 2. * hover;
    let knob = 11. * hover;
    Slider::new(state)
        .disabled(disabled)
        .w_full()
        .h(rems(1.25))
        .px(rems(0.375))
        .when(disabled, |s| s.opacity(0.45))
        .child(
            SliderTrack::new(state)
                .disabled(disabled)
                .relative()
                .size_full()
                .child(
                    SliderIndicator::new(state)
                        .absolute()
                        .top(relative(0.5))
                        .mt(px(-line / 2.))
                        .w_full()
                        .h(px(line))
                        .rounded_full()
                        .bg(t.border)
                        .child(
                            div()
                                .absolute()
                                .h_full()
                                .left_0()
                                .right(relative(1. - percentage))
                                .rounded_full()
                                .bg(if hover > 0. { t.accent } else { t.foreground }),
                        ),
                )
                .child(
                    SliderThumb::new(state)
                        .disabled(disabled)
                        .absolute()
                        .top(relative(0.5))
                        .mt(px(-knob / 2.))
                        .left(relative(percentage))
                        .ml(px(-knob / 2.))
                        .size(px(knob))
                        .rounded_full()
                        .bg(t.foreground)
                        .shadow_sm(),
                ),
        )
}

impl Render for PlayerBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.cover_poll.tick(&self.library, cx);
        let theme = cx.omarchy().clone();
        let current = self.player.current();
        let current = current.as_ref();
        let playing = self.player.state() == PlayerState::Playing;
        let has_track = current.is_some();
        let pos = if has_track {
            self.player.position()
        } else {
            0.0
        };
        let dur = if has_track {
            self.player.duration().max(1.0)
        } else {
            0.0
        };
        let title = current
            .map(|t| t.title.clone())
            .unwrap_or_else(|| "Not Playing".into());
        let sub = current
            .map(|t| format!("{} — {}", t.artist, t.album))
            .unwrap_or_else(|| "Pick a song to start".into());
        let shuffle = self.player.shuffle();
        let repeat = self.player.repeat();
        let muted = self.player.is_muted();
        let queue_open = self
            .main
            .upgrade()
            .is_some_and(|main| main.read(cx).queue_open());
        let wide = window.viewport_size().width >= window.rem_size() * WIDE_REMS;
        let (seek_hover, on_seek_hover) =
            hover_progress("progress", Transition::new(LIFT), window, cx);
        let seek_hover = if has_track { seek_hover } else { 0. };
        let (volume_hover, on_volume_hover) =
            hover_progress("volume", Transition::new(LIFT), window, cx);
        let toggle =
            |button: Button, on: bool| button.selected(on).when(on, |b| b.text_color(theme.accent));
        let lane = || {
            div()
                .flex()
                .items_center()
                .w(rems(LANE_REMS))
                .min_w(rems(3.))
        };
        let time = |secs: f64| {
            div()
                .flex_none()
                .min_w(rems(2.5))
                .text_size(rems(0.625))
                .text_color(theme.secondary)
                .child(format_time(secs))
        };

        let now_playing = div()
            .id("now-playing")
            .flex()
            .items_center()
            .gap(rems(0.625))
            .min_w_0()
            .when(has_track, |d| d.cursor_pointer())
            .on_click(cx.listener(|this, _, _, cx| {
                if let Some(track) = this.player.current() {
                    let _ = this
                        .main
                        .update(cx, |main, cx| main.show_album(track.album_id, cx));
                }
            }))
            .child(div().flex_none().rounded(px(4.)).shadow_sm().child(cover(
                &self.library,
                current.map(|t| t.album_id.as_str()),
                40.,
                current.map_or("♪", |t| t.title.as_str()),
                cx,
            )))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(rems(0.75))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.foreground)
                            .truncate()
                            .child(title),
                    )
                    .child(
                        div()
                            .text_size(rems(0.6875))
                            .text_color(theme.secondary)
                            .truncate()
                            .child(sub),
                    ),
            );

        let play_glyph = icon(if playing {
            IconName::Pause
        } else {
            IconName::Play
        })
        .size(rems(0.875))
        .with_animation(
            ElementId::Name(format!("play-glyph-{playing}").into()),
            Animation::new(LIFT).with_easing(ease_out_quint()),
            |glyph, t| glyph.opacity(t),
        );
        let play = button("play", "", ButtonVariant::Secondary, cx)
            .accessibility_label(if playing { "Pause" } else { "Play" })
            .child(play_glyph)
            .size(rems(2.))
            .p_0()
            .justify_center()
            .rounded_full()
            .border_color(theme.accent.opacity(0.))
            .bg(theme.accent)
            .text_color(theme.on_accent)
            .hover(|s| s.bg(theme.accent.opacity(0.85)))
            .active(|s| s.bg(theme.accent.opacity(0.7)))
            .disabled(!has_track)
            .on_click(cx.listener(|this, _, _, cx| {
                this.player.toggle();
                cx.notify();
            }));

        let transport = div()
            .flex()
            .items_center()
            .gap(rems(0.5))
            .child(with_tooltip(
                toggle(
                    Self::control_button(
                        "shuffle",
                        IconName::Shuffle,
                        "Shuffle",
                        !has_track,
                        |p| p.set_shuffle(!p.shuffle()),
                        cx,
                    ),
                    shuffle,
                ),
                "Shuffle",
            ))
            .child(with_tooltip(
                Self::control_button(
                    "prev",
                    IconName::SkipBack,
                    "Previous",
                    !has_track,
                    Player::previous,
                    cx,
                ),
                "Previous",
            ))
            .child(with_tooltip(play, "Play/Pause (Space)"))
            .child(with_tooltip(
                Self::control_button(
                    "next",
                    IconName::SkipForward,
                    "Next",
                    !has_track,
                    Player::next,
                    cx,
                ),
                "Next",
            ))
            .child(with_tooltip(
                toggle(
                    Self::control_button(
                        "repeat",
                        if repeat == RepeatMode::One {
                            IconName::Repeat1
                        } else {
                            IconName::Repeat
                        },
                        "Repeat",
                        !has_track,
                        |p| p.set_repeat(p.repeat().next()),
                        cx,
                    ),
                    repeat != RepeatMode::Off,
                ),
                "Repeat",
            ));

        let progress = div()
            .id("progress")
            .test_support()
            .flex()
            .items_center()
            .gap(rems(0.25))
            .w_full()
            .max_w(rems(36.))
            .on_hover(on_seek_hover)
            .child(time(pos).text_right())
            .child(div().flex_1().min_w_0().child(line_slider(
                &self.progress,
                !has_track,
                seek_hover,
                cx,
            )))
            .child(time(dur));

        let mute = with_tooltip(
            icon_button(
                "mute",
                if muted {
                    IconName::VolumeX
                } else {
                    IconName::Volume2
                },
                if muted { "Unmute" } else { "Mute" },
                ButtonVariant::Secondary,
                cx,
            )
            .rounded(rems(0.375))
            .when(muted, |b| b.text_color(theme.danger))
            .on_click(cx.listener(|this, _, window, cx| {
                this.player.toggle_mute();
                let value = this.player.volume() as f32;
                this.volume
                    .update(cx, |s, cx| s.set_value(value, window, cx));
                cx.notify();
            })),
            if muted { "Unmute" } else { "Mute" },
        );

        div()
            .flex()
            .items_center()
            .gap(rems(0.75))
            .h(rems(4.))
            .px(rems(0.75))
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(lane().child(now_playing))
            .child(
                div()
                    .flex_1()
                    .min_w(rems(14.))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(rems(0.125))
                    .child(transport)
                    .child(progress),
            )
            .child(
                lane()
                    .justify_end()
                    .gap(rems(0.25))
                    .child(with_tooltip(
                        icon_button(
                            "bar-queue",
                            IconName::ListMusic,
                            "Up Next",
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .rounded(rems(0.375))
                        .selected(queue_open)
                        .on_click(cx.listener(|this, _, _, cx| {
                            let _ = this.main.update(cx, |main, cx| main.toggle_queue(cx));
                            cx.notify();
                        })),
                        "Up Next",
                    ))
                    .child(mute)
                    .when(wide, |d| {
                        d.child(
                            div()
                                .id("volume")
                                .w(rems(5.5))
                                .flex_shrink_0()
                                .on_hover(on_volume_hover)
                                .child(line_slider(&self.volume, false, volume_hover, cx)),
                        )
                    }),
            )
    }
}
