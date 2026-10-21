//! Colour changes eased rather than snapped. A tint arriving or leaving — a
//! finished program's green giving way when the user is back, a card's header
//! going amber — reads as a jolt when it lands in one frame; carried over a
//! tenth of a second it reads as the window responding.
//!
//! This is a pass over the scene a window is about to draw, not a property of
//! any one model: whatever decides a colour goes on saying what it should be,
//! and [`ColorFades`] remembers what was last on screen under each item and
//! eases from there. An item is known by its [`SceneId`] and how many items
//! with that id came before it, so it must keep being drawn — in the colour it
//! settles on — for a change to it to ease: one that appears or disappears
//! just does.

use std::collections::HashMap;

use ghost_render::scene::{Rgba, Scene, SceneId, SceneItem};

/// How long a colour takes to reach a new one.
pub const FADE_MS: u64 = 200;

/// What each item of a window's scene was last drawn in, and where it is
/// heading. One per window: the scenes it eases must be that window's frames,
/// in order.
#[derive(Debug, Default)]
pub struct ColorFades {
    slots: HashMap<(SceneId, usize), Fade>,
}

#[derive(Clone, Copy, Debug)]
struct Fade {
    from: Rgba,
    to: Rgba,
    t0: u64,
}

impl Fade {
    fn at(&self, now_ms: u64) -> Rgba {
        let elapsed = now_ms.saturating_sub(self.t0);
        if elapsed >= FADE_MS {
            return self.to;
        }
        let p = crate::root::ease_in_out(elapsed as f32 / FADE_MS as f32);
        std::array::from_fn(|i| self.from[i] + (self.to[i] - self.from[i]) * p)
    }

    fn settled(&self, now_ms: u64) -> bool {
        self.from == self.to || now_ms.saturating_sub(self.t0) >= FADE_MS
    }
}

impl ColorFades {
    /// Draw `scene` as it looks `now_ms`: each colour that changed since the
    /// last frame starts from what was shown and eases to what the scene says.
    /// An item seen for the first time is drawn as it is. Whether any colour is
    /// still on its way, and so wants another frame.
    pub fn apply(&mut self, scene: &mut Scene, now_ms: u64) -> bool {
        let mut seen: HashMap<SceneId, usize> = HashMap::new();
        let mut slots = HashMap::with_capacity(self.slots.len());
        let mut moving = false;
        for item in scene.layers.iter_mut().flat_map(|l| &mut l.items) {
            let (id, color) = match item {
                SceneItem::Rect { id, color, .. }
                | SceneItem::ChromeText { id, color, .. }
                | SceneItem::Text { id, color, .. }
                | SceneItem::Border { id, color, .. } => (*id, color),
                SceneItem::Terminal { .. } | SceneItem::Badge { .. } => continue,
            };
            let nth = seen.entry(id).or_default();
            let key = (id, *nth);
            *nth += 1;
            let fade = match self.slots.remove(&key) {
                Some(f) if f.to == *color => f,
                Some(f) => Fade {
                    from: f.at(now_ms),
                    to: *color,
                    t0: now_ms,
                },
                None => Fade {
                    from: *color,
                    to: *color,
                    t0: now_ms,
                },
            };
            *color = fade.at(now_ms);
            moving |= !fade.settled(now_ms);
            slots.insert(key, fade);
        }
        self.slots = slots;
        moving
    }

    /// Forget what was shown, so the next frame is drawn as it is — for frames
    /// whose colours something else is already moving, like a zoom fading the
    /// chrome, which an easing lagging behind would only smear.
    pub fn clear(&mut self) {
        self.slots.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{BarStatus, ButtonLayout, Titlebar, notice_bg, with_titlebar};

    fn bar(status: Option<BarStatus>) -> Titlebar {
        Titlebar {
            height_px: 30,
            bg: [0.2, 0.2, 0.2, 1.0],
            fg: [0.9, 0.9, 0.9, 1.0],
            title: "t".into(),
            notice: None,
            status,
            attention: None,
            font_px: 14.0,
            buttons: ButtonLayout::parse(":close"),
            hovered: None,
            pressed: None,
            maximized: false,
            scale: 1.0,
            controls_px: 0.0,
        }
    }

    fn drawn(fades: &mut ColorFades, bar: &Titlebar, now_ms: u64) -> (Rgba, bool) {
        let mut scene = with_titlebar(Scene::new((200, 100)), bar);
        let moving = fades.apply(&mut scene, now_ms);
        let color = scene
            .layers
            .iter()
            .flat_map(|l| &l.items)
            .find_map(|i| match i {
                SceneItem::Rect {
                    id: SceneId::Titlebar,
                    color,
                    ..
                } => Some(*color),
                _ => None,
            })
            .expect("a titlebar");
        (color, moving)
    }

    fn strictly_between(c: Rgba, a: Rgba, b: Rgba) -> bool {
        (0..3).all(|i| a[i] == b[i] || (c[i] - a[i]) * (c[i] - b[i]) < 0.0)
            && (0..3).any(|i| a[i] != b[i])
    }

    /// The green of a finished program leaves the bar over a tenth of a second
    /// once the user is back, rather than in one frame — and the bar asks for
    /// frames until it has.
    #[test]
    fn a_tint_leaving_the_bar_eases_away() {
        let done = bar(Some(BarStatus::Done));
        let plain = bar(None);
        let green = notice_bg(&done);
        let mut fades = ColorFades::default();
        assert_eq!(drawn(&mut fades, &done, 0), (green, false));

        assert_eq!(drawn(&mut fades, &plain, 1_000), (green, true));
        let (mid, moving) = drawn(&mut fades, &plain, 1_000 + FADE_MS / 2);
        assert!(strictly_between(mid, green, plain.bg), "{mid:?}");
        assert!(moving);
        assert_eq!(
            drawn(&mut fades, &plain, 1_000 + FADE_MS),
            (plain.bg, false)
        );
    }

    /// Eased, not linear: a quarter of the way in, less than a quarter of the
    /// change has happened.
    #[test]
    fn a_tint_starts_gently() {
        let mut fades = ColorFades::default();
        drawn(&mut fades, &bar(None), 0);
        let working = bar(Some(BarStatus::Working { progress: None }));
        let to = notice_bg(&working);
        drawn(&mut fades, &working, 0);
        let (c, _) = drawn(&mut fades, &working, FADE_MS / 4);
        let from = bar(None).bg;
        let travelled = (c[0] - from[0]) / (to[0] - from[0]);
        assert!(travelled > 0.0 && travelled < 0.25, "{travelled}");
    }

    /// A window's first frame, and anything newly drawn, shows as it is.
    #[test]
    fn what_is_drawn_for_the_first_time_is_not_eased() {
        let done = bar(Some(BarStatus::Done));
        let mut fades = ColorFades::default();
        assert_eq!(drawn(&mut fades, &done, 0), (notice_bg(&done), false));
    }

    /// A change of mind mid-way turns back from where the colour is, not from
    /// where it started.
    #[test]
    fn a_change_mid_fade_starts_from_what_is_shown() {
        let done = bar(Some(BarStatus::Done));
        let plain = bar(None);
        let mut fades = ColorFades::default();
        drawn(&mut fades, &done, 0);
        drawn(&mut fades, &plain, 0);
        let (mid, _) = drawn(&mut fades, &plain, FADE_MS / 2);
        assert_eq!(drawn(&mut fades, &done, FADE_MS / 2).0, mid);
        let (back, _) = drawn(&mut fades, &done, FADE_MS / 2 + FADE_MS);
        assert_eq!(back, notice_bg(&done));
    }

    /// After a clear, the next frame is drawn as it is.
    #[test]
    fn a_cleared_window_draws_its_next_frame_as_it_is() {
        let plain = bar(None);
        let done = bar(Some(BarStatus::Done));
        let mut fades = ColorFades::default();
        drawn(&mut fades, &plain, 0);
        fades.clear();
        assert_eq!(drawn(&mut fades, &done, 10), (notice_bg(&done), false));
    }
}
