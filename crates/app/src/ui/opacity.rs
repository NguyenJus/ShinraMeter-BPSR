//! The overlay's window opacity as a type, not a bare `f32`.
//!
//! Every painted surface fades with the opacity slider
//! (`Settings::opacity`), so the value used to travel down the paint paths as
//! a bare `f32` — indistinguishable, at a call site, from the half-dozen
//! other `f32` arguments those functions take (alphas, insets, fractions,
//! heights). `Opacity` makes the fade its own type, so the compiler catches
//! a swapped argument, and makes the clamp a property of the value rather
//! than something each caller has to remember: whatever a settings file, a
//! slider drag or an arithmetic slip produces, an `Opacity` is always a
//! finite number in `0.0..=1.0`.

use eframe::egui;

/// A window opacity: a finite fraction in `0.0..=1.0`, where `0.0` paints
/// nothing and `1.0` paints the color untouched.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Opacity(f32);

impl Opacity {
    /// Fully opaque — the identity for [`Opacity::apply`].
    pub(crate) const OPAQUE: Self = Self(1.0);

    /// Clamps `value` into `0.0..=1.0`.
    ///
    /// A non-finite input (a corrupt settings file, a division that went
    /// wrong) becomes [`Opacity::OPAQUE`], not transparent: a fully visible
    /// overlay is recoverable by the user, an invisible one looks like the
    /// meter failed to start.
    pub(crate) fn new(value: f32) -> Self {
        if value.is_nan() {
            return Self::OPAQUE;
        }
        Self(value.clamp(0.0, 1.0))
    }

    /// The raw fraction, for the egui APIs that still take an `f32`.
    pub(crate) fn as_f32(self) -> f32 {
        self.0
    }

    /// Fades `color` by this opacity — the one place the paint paths turn an
    /// `Opacity` back into a color.
    pub(crate) fn apply(self, color: egui::Color32) -> egui::Color32 {
        color.gamma_multiply(self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_keeps_a_value_already_inside_the_range() {
        assert_eq!(Opacity::new(0.25).as_f32(), 0.25);
        assert_eq!(Opacity::new(0.0).as_f32(), 0.0);
        assert_eq!(Opacity::new(1.0).as_f32(), 1.0);
    }

    #[test]
    fn new_clamps_out_of_range_values_to_the_unit_interval() {
        assert_eq!(Opacity::new(1.5).as_f32(), 1.0);
        assert_eq!(Opacity::new(-0.25).as_f32(), 0.0);
        assert_eq!(Opacity::new(f32::INFINITY).as_f32(), 1.0);
        assert_eq!(Opacity::new(f32::NEG_INFINITY).as_f32(), 0.0);
    }

    #[test]
    fn a_nan_setting_paints_the_overlay_opaque_rather_than_invisible() {
        assert_eq!(Opacity::new(f32::NAN).as_f32(), 1.0);
    }

    #[test]
    fn applying_full_opacity_leaves_a_color_untouched() {
        let color = egui::Color32::from_rgba_unmultiplied(0x10, 0x20, 0x30, 0x80);
        assert_eq!(Opacity::OPAQUE.apply(color), color);
    }

    #[test]
    fn applying_zero_opacity_paints_nothing() {
        let color = egui::Color32::from_rgb(0x10, 0x20, 0x30);
        assert_eq!(Opacity::new(0.0).apply(color), egui::Color32::TRANSPARENT);
    }

    #[test]
    fn apply_matches_the_raw_gamma_multiply_it_replaces() {
        let color = egui::Color32::from_rgba_unmultiplied(0x70, 0x80, 0x90, 0x50);
        for raw in [0.0_f32, 0.2, 0.5, 0.78, 1.0] {
            assert_eq!(
                Opacity::new(raw).apply(color),
                color.gamma_multiply(raw),
                "apply must stay a pure rename of gamma_multiply for {raw}"
            );
        }
    }
}
