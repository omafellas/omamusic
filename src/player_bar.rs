use crate::app::{cover, format_time, Omamusic};
use crate::config::AppConfig;
use crate::library::Library;
use crate::models::{PlayerState, RepeatMode};
use crate::player::Player;
use gpui_kit::base::slider::{SliderEvent, SliderState};
use gpui_kit::base::{Slider, SliderIndicator, SliderThumb, SliderTrack};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    div, relative, rems, App, AppContext, Context, Entity, InteractiveElement, IntoElement,
    ParentElement, Render, StatefulInteractiveElement, Styled, WeakEntity, Window,
};
use gpui_omarchy::{
    icon, icon_button, slider, with_tooltip, ActiveTheme, Button, ButtonVariant, IconName,
};
use std::sync::Arc;
use std::time::Duration;

const PROGRESS_STEPS: f32 = 1000.;
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const IDLE_INTERVAL: Duration = Duration::from_millis(250);

pub struct PlayerBar {
    library: Arc<Library>,
    player: Arc<Player>,
    main: WeakEntity<Omamusic>,
    volume: Entity<SliderState>,
    progress: Entity<SliderState>,
    seeking: bool,
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

        let covers = library.covers().subscribe();
        cx.spawn(async move |this, cx| {
            while covers.recv().await.is_ok() {
                if this.update(cx, |_, cx| cx.notify()).is_err() {
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
        variant: ButtonVariant,
        disabled: bool,
        action: fn(&Player),
        cx: &mut Context<Self>,
    ) -> Button {
        icon_button(id, icon_name, label, variant, cx)
            .disabled(disabled)
            .on_click(cx.listener(move |this, _, _, cx| {
                action(&this.player);
                cx.notify();
            }))
    }
}

fn progress_slider(state: &Entity<SliderState>, disabled: bool, cx: &App) -> Slider {
    let t = cx.omarchy().clone();
    let percentage = state.read(cx).percentage().end;
    Slider::new(state)
        .disabled(disabled)
        .w_full()
        .h(rems(1.75))
        .px(rems(0.5))
        .when(disabled, |s| s.opacity(0.45))
        .child(
            SliderTrack::new(state)
                .disabled(disabled)
                .relative()
                .size_full()
                .child(
                    SliderIndicator::new(state)
                        .absolute()
                        .top(rems(0.75))
                        .w_full()
                        .h(rems(0.25))
                        .bg(t.border)
                        .child(
                            div()
                                .absolute()
                                .h_full()
                                .left_0()
                                .right(relative(1. - percentage))
                                .bg(t.accent),
                        ),
                )
                .child(
                    SliderThumb::new(state)
                        .disabled(disabled)
                        .absolute()
                        .top(rems(0.375))
                        .left(relative(percentage))
                        .ml(rems(-0.5))
                        .size(rems(1.))
                        .border_2()
                        .border_color(t.accent)
                        .bg(t.background)
                        .when(!disabled, |thumb| thumb.hover(|s| s.bg(t.selection))),
                ),
        )
}

impl Render for PlayerBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            .unwrap_or_default();
        let shuffle = self.player.shuffle();
        let repeat = self.player.repeat();
        let muted = self.player.is_muted();
        let toggled = |on: bool| {
            if on {
                ButtonVariant::Primary
            } else {
                ButtonVariant::Secondary
            }
        };

        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(rems(0.75))
            .px(rems(0.75))
            .py(rems(0.5))
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(rems(0.75))
                    .child(
                        div()
                            .id("now-playing")
                            .flex()
                            .items_center()
                            .gap(rems(0.75))
                            .when(has_track, |d| d.cursor_pointer())
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(track) = this.player.current() {
                                    let _ = this
                                        .main
                                        .update(cx, |main, cx| main.show_album(track.album_id, cx));
                                }
                            }))
                            .child(cover(
                                &self.library,
                                current.map(|t| t.album_id.as_str()),
                                40.,
                                current.map_or("♪", |t| t.title.as_str()),
                                cx,
                            ))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .w(rems(10.))
                                    .min_w(rems(4.))
                                    .child(
                                        div()
                                            .text_size(rems(0.75))
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
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(rems(0.375))
                            .child(with_tooltip(
                                Self::control_button(
                                    "shuffle",
                                    IconName::Shuffle,
                                    "Shuffle",
                                    toggled(shuffle),
                                    !has_track,
                                    |p| p.set_shuffle(!p.shuffle()),
                                    cx,
                                ),
                                "Shuffle",
                            ))
                            .child(with_tooltip(
                                Self::control_button(
                                    "prev",
                                    IconName::SkipBack,
                                    "Previous",
                                    ButtonVariant::Secondary,
                                    !has_track,
                                    Player::previous,
                                    cx,
                                ),
                                "Previous",
                            ))
                            .child(with_tooltip(
                                Self::control_button(
                                    "play",
                                    if playing {
                                        IconName::Pause
                                    } else {
                                        IconName::Play
                                    },
                                    if playing { "Pause" } else { "Play" },
                                    ButtonVariant::Primary,
                                    !has_track,
                                    Player::toggle,
                                    cx,
                                ),
                                "Play/Pause (Space)",
                            ))
                            .child(with_tooltip(
                                Self::control_button(
                                    "next",
                                    IconName::SkipForward,
                                    "Next",
                                    ButtonVariant::Secondary,
                                    !has_track,
                                    Player::next,
                                    cx,
                                ),
                                "Next",
                            ))
                            .child(with_tooltip(
                                Self::control_button(
                                    "repeat",
                                    if repeat == RepeatMode::One {
                                        IconName::Repeat1
                                    } else {
                                        IconName::Repeat
                                    },
                                    "Repeat",
                                    toggled(repeat != RepeatMode::Off),
                                    !has_track,
                                    |p| p.set_repeat(p.repeat().next()),
                                    cx,
                                ),
                                "Repeat",
                            )),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w(rems(16.))
                    .items_center()
                    .gap(rems(0.375))
                    .child(
                        div()
                            .min_w(rems(3.5))
                            .max_w(rems(7.))
                            .text_right()
                            .text_size(rems(0.625))
                            .text_color(theme.secondary)
                            .child(format_time(pos)),
                    )
                    .child(div().flex_1().min_w_0().child(progress_slider(
                        &self.progress,
                        !has_track,
                        cx,
                    )))
                    .child(
                        div()
                            .min_w(rems(3.5))
                            .max_w(rems(7.))
                            .text_size(rems(0.625))
                            .text_color(theme.secondary)
                            .child(format_time(dur)),
                    )
                    .child(with_tooltip(
                        div()
                            .id("mute")
                            .flex_shrink_0()
                            .cursor_pointer()
                            .child(
                                icon(if muted {
                                    IconName::VolumeX
                                } else {
                                    IconName::Volume2
                                })
                                .size(rems(0.875))
                                .text_color(if muted {
                                    theme.danger
                                } else {
                                    theme.foreground
                                }),
                            )
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.player.toggle_mute();
                                let value = this.player.volume() as f32;
                                this.volume
                                    .update(cx, |s, cx| s.set_value(value, window, cx));
                                cx.notify();
                            })),
                        if muted { "Unmute" } else { "Mute" },
                    ))
                    .child(
                        div()
                            .w(rems(5.))
                            .min_w(rems(3.))
                            .flex_shrink_0()
                            .child(slider(&self.volume, false, window, cx)),
                    ),
            )
    }
}
