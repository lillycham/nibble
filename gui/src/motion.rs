//! Short eases where the window would otherwise jump, and none at all when
//! the system asks for less motion.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gpui::{Animation, AnimationExt, AnyElement, ElementId, Hsla, IntoElement, Rgba, Styled};

/// How long things take to come and go, and to change.
pub const IN: Duration = Duration::from_millis(200);
pub const OUT: Duration = Duration::from_millis(150);
pub const CHANGE: Duration = Duration::from_millis(250);

static REDUCED: AtomicBool = AtomicBool::new(false);

/// Whether the system asks for less motion, as of the last `check`.
pub fn reduced() -> bool {
    REDUCED.load(Ordering::Relaxed)
}

/// Ask the system again, off the main thread, since it means running a
/// command. Done at launch and whenever the window comes to the front.
pub fn check() {
    std::thread::spawn(|| REDUCED.store(ask_system(), Ordering::Relaxed));
}

fn ask_system() -> bool {
    let run = |program: &str, args: &[&str]| {
        std::process::Command::new(program)
            .args(args)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    if cfg!(target_os = "macos") {
        run("defaults", &["read", "com.apple.universalaccess", "reduceMotion"]).is_some_and(|value| value == "1")
    } else {
        run("gsettings", &["get", "org.gnome.desktop.interface", "enable-animations"]).is_some_and(|value| value == "false")
    }
}

/// Quick at first, settling at the end.
pub fn ease_out(t: f32) -> f32 {
    1. - (1. - t).powi(3)
}

/// Fade in, rising a few pixels if `rise` is given. Runs once for each id
/// while the element stays on screen.
pub fn fade_in<E: IntoElement + Styled + 'static>(element: E, id: impl Into<ElementId>, rise: f32) -> AnyElement {
    if reduced() {
        return element.into_any_element();
    }
    element
        .with_animation(id, Animation::new(IN).with_easing(ease_out), move |element, t| {
            let element = element.opacity(t);
            if rise > 0. { element.relative().top(gpui::px(rise * (1. - t))) } else { element }
        })
        .into_any_element()
}

/// Fade out, for something already on its way off screen.
pub fn fade_out<E: IntoElement + Styled + 'static>(element: E, id: impl Into<ElementId>) -> AnyElement {
    if reduced() {
        return element.opacity(0.).into_any_element();
    }
    element
        .with_animation(id, Animation::new(OUT).with_easing(ease_out), |element, t| element.opacity(1. - t))
        .into_any_element()
}

/// A colour part of the way to another, mixed in RGB so that the hue takes
/// the short way round.
pub fn mix(from: Hsla, to: Hsla, t: f32) -> Hsla {
    let (a, b) = (Rgba::from(from), Rgba::from(to));
    let at = |x: f32, y: f32| x + (y - x) * t;
    Rgba { r: at(a.r, b.r), g: at(a.g, b.g), b: at(a.b, b.b), a: at(a.a, b.a) }.into()
}

/// A value easing from where it was to where it is now. `step` goes up each
/// time it changes, which starts the ease again.
#[derive(Clone, Copy, Default)]
pub struct Eased<T> {
    pub from: T,
    pub to: T,
    pub step: usize,
}

impl<T: Copy + PartialEq> Eased<T> {
    pub fn new(value: T) -> Self {
        Eased { from: value, to: value, step: 0 }
    }

    /// Head for `value`, from where the last ease was heading.
    pub fn set(&mut self, value: T) {
        if value != self.to {
            self.from = self.to;
            self.to = value;
            self.step += 1;
        }
    }

    /// Jump to `value` without an ease.
    pub fn reset(&mut self, value: T) {
        self.from = value;
        self.to = value;
        self.step += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ease_starts_and_ends_where_it_should() {
        assert_eq!(ease_out(0.), 0.);
        assert_eq!(ease_out(1.), 1.);
        assert!(ease_out(0.5) > 0.5);
    }

    #[test]
    fn a_colour_mixes_from_one_end_to_the_other() {
        let (red, blue) = (gpui::red(), gpui::blue());
        let start = Rgba::from(mix(red, blue, 0.));
        let end = Rgba::from(mix(red, blue, 1.));
        assert!((start.r - 1.).abs() < 1e-3 && start.b.abs() < 1e-3);
        assert!(end.r.abs() < 1e-3 && (end.b - 1.).abs() < 1e-3);
    }

    #[test]
    fn an_eased_value_only_restarts_when_it_changes() {
        let mut value = Eased::new(0.);
        value.set(0.);
        assert_eq!(value.step, 0);
        value.set(0.5);
        value.set(0.8);
        assert_eq!((value.from, value.to, value.step), (0.5, 0.8, 2));
    }
}
