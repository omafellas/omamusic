use crate::desktop::{Command, NowPlaying};
use crate::models::PlayerState;
use crate::player::Player;
use async_channel::Sender;
use gpui_kit::AssetSource;
use ksni::menu::StandardItem;
use ksni::{Icon, MenuItem, Orientation, ToolTip};
use resvg::{tiny_skia, usvg};
use std::sync::Arc;

const MENU_ICON_SIZE: u32 = 32;
const TRAY_ICON_SIZE: u32 = 64;
const DEFAULT_FOREGROUND: &str = "#cacccc";

struct Icons {
    color: String,
    play: Vec<u8>,
    pause: Vec<u8>,
    previous: Vec<u8>,
    next: Vec<u8>,
    show: Vec<u8>,
    quit: Vec<u8>,
    idle: Option<Icon>,
}

fn theme_foreground() -> String {
    let state = dirs::state_dir().unwrap_or_default();
    std::fs::read_to_string(state.join("omarchy/current/theme/colors.toml"))
        .ok()
        .and_then(|colors| {
            colors.lines().find_map(|line| {
                let (key, value) = line.split_once('=')?;
                (key.trim() == "foreground").then(|| value.trim().trim_matches('"').to_string())
            })
        })
        .unwrap_or_else(|| DEFAULT_FOREGROUND.into())
}

fn render(name: &str, color: &str, size: u32) -> Option<tiny_skia::Pixmap> {
    let svg = gpui_kit::assets::AllAssets
        .load(&format!("icons/{name}.svg"))
        .ok()??;
    let svg = String::from_utf8_lossy(&svg).replace("currentColor", color);
    let tree = usvg::Tree::from_str(&svg, &usvg::Options::default()).ok()?;
    let mut pixmap = tiny_skia::Pixmap::new(size, size)?;
    let scale = size as f32 / tree.size().width();
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Some(pixmap)
}

fn png(name: &str, color: &str) -> Vec<u8> {
    render(name, color, MENU_ICON_SIZE)
        .and_then(|pixmap| pixmap.encode_png().ok())
        .unwrap_or_default()
}

impl Icons {
    fn load() -> Self {
        let color = theme_foreground();
        let idle = render("music", &color, TRAY_ICON_SIZE).map(|pixmap| Icon {
            width: pixmap.width() as i32,
            height: pixmap.height() as i32,
            data: pixmap
                .pixels()
                .iter()
                .flat_map(|p| {
                    let c = p.demultiply();
                    [c.alpha(), c.red(), c.green(), c.blue()]
                })
                .collect(),
        });
        Self {
            play: png("play", &color),
            pause: png("pause", &color),
            previous: png("skip-back", &color),
            next: png("skip-forward", &color),
            show: png("app-window", &color),
            quit: png("power", &color),
            idle,
            color,
        }
    }
}

pub struct Tray {
    pub now: NowPlaying,
    player: Arc<Player>,
    commands: Sender<Command>,
    icons: Icons,
}

impl Tray {
    pub fn new(player: Arc<Player>, commands: Sender<Command>) -> Self {
        Self {
            now: NowPlaying::default(),
            player,
            commands,
            icons: Icons::load(),
        }
    }

    fn send(&self, command: Command) {
        let _ = self.commands.send_blocking(command);
    }
}

fn item(label: &str, icon: &[u8], activate: impl Fn(&mut Tray) + Send + 'static) -> MenuItem<Tray> {
    StandardItem {
        label: label.into(),
        icon_data: icon.to_vec(),
        activate: Box::new(activate),
        ..Default::default()
    }
    .into()
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "omamusic".into()
    }

    fn title(&self) -> String {
        "omamusic".into()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        self.icons.idle.as_ref().cloned().into_iter().collect()
    }

    fn menu_about_to_show(&mut self) {
        if theme_foreground() != self.icons.color {
            self.icons = Icons::load();
        }
    }

    fn tool_tip(&self) -> ToolTip {
        let Some(track) = &self.now.track else {
            return ToolTip {
                title: "omamusic".into(),
                ..Default::default()
            };
        };
        let paused = if self.now.state == PlayerState::Paused {
            " · Paused"
        } else {
            ""
        };
        ToolTip {
            title: track.title.clone(),
            description: format!("{} — {}{paused}", track.artist, track.album),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(Command::ToggleWindow);
    }

    fn secondary_activate(&mut self, _x: i32, _y: i32) {
        self.player.toggle();
    }

    fn scroll(&mut self, delta: i32, orientation: Orientation) {
        if orientation != Orientation::Vertical {
            return;
        }
        if delta > 0 {
            self.player.previous();
        } else {
            self.player.next();
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let has_track = self.now.track.is_some();
        let mut menu = Vec::new();
        if let Some(track) = &self.now.track {
            menu.push(
                StandardItem {
                    label: format!("{} — {}", track.title, track.artist).replace('_', "__"),
                    activate: Box::new(|tray: &mut Self| tray.send(Command::ShowWindow)),
                    icon_data: self
                        .now
                        .cover
                        .as_ref()
                        .map(|c| c.menu_png.clone())
                        .unwrap_or_default(),
                    ..Default::default()
                }
                .into(),
            );
            menu.push(MenuItem::Separator);
        }
        let icons = &self.icons;
        let (label, icon) = if self.now.state == PlayerState::Playing {
            ("Pause", &icons.pause)
        } else {
            ("Play", &icons.play)
        };
        let controls = [
            item(label, icon, |tray| tray.player.toggle()),
            item("Previous", &icons.previous, |tray| tray.player.previous()),
            item("Next", &icons.next, |tray| tray.player.next()),
        ];
        menu.extend(controls.into_iter().map(|mut entry| {
            if let MenuItem::Standard(standard) = &mut entry {
                standard.enabled = has_track;
            }
            entry
        }));
        menu.extend([
            MenuItem::Separator,
            item("Show omamusic", &icons.show, |tray| {
                tray.send(Command::ShowWindow)
            }),
            MenuItem::Separator,
            item("Quit omamusic", &icons.quit, |tray| {
                tray.send(Command::Quit)
            }),
        ]);
        menu
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_icons_render_in_the_theme_color() {
        let icon = png("pause", "#ff0000");
        let decoded = image::load_from_memory(&icon).unwrap().to_rgba8();
        assert_eq!(decoded.dimensions(), (MENU_ICON_SIZE, MENU_ICON_SIZE));
        assert!(decoded.pixels().any(|p| p.0 == [255, 0, 0, 255]));
        let idle = Icons::load().idle.unwrap();
        assert_eq!(
            idle.data.len(),
            (TRAY_ICON_SIZE * TRAY_ICON_SIZE * 4) as usize
        );
    }
}
