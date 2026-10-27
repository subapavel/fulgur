//! border-color, border-radius, border-style extraction.
//!
//! border_radii basis is CSS px (Stylo length-percentage operates in CSS px),
//! converted to pt via `.as_px().in_pt()` before storage. See coordinate-system.md.

use super::{StyleContext, absolute_to_rgba};
use crate::draw_primitives::{BlockStyle, BorderStyleValue};
use crate::units::F32Units;

pub(super) fn apply_to(style: &mut BlockStyle, ctx: &StyleContext<'_>) {
    // Border colours: each side has its own (css-backgrounds-3 section 4.1); `currentColor` resolves per side.
    style.border_colors = [
        side_color(&ctx.styles.clone_border_top_color(), ctx),
        side_color(&ctx.styles.clone_border_right_color(), ctx),
        side_color(&ctx.styles.clone_border_bottom_color(), ctx),
        side_color(&ctx.styles.clone_border_left_color(), ctx),
    ];

    // Border radii. Stylo evaluates length-percentage values in CSS px
    // space, so we feed it the CSS-px border-box basis and convert the
    // returned radius to pt. border_radii is consumed downstream alongside
    // pt-space widths/heights (see `compute_padding_box_inner_radii`).
    let width = ctx.layout.size.width;
    let height = ctx.layout.size.height;

    let tl = ctx.styles.clone_border_top_left_radius();
    let tr = ctx.styles.clone_border_top_right_radius();
    let br = ctx.styles.clone_border_bottom_right_radius();
    let bl = ctx.styles.clone_border_bottom_left_radius();

    style.border_radii = [
        [
            resolve_radius(&tl.0.width, width),
            resolve_radius(&tl.0.height, height),
        ],
        [
            resolve_radius(&tr.0.width, width),
            resolve_radius(&tr.0.height, height),
        ],
        [
            resolve_radius(&br.0.width, width),
            resolve_radius(&br.0.height, height),
        ],
        [
            resolve_radius(&bl.0.width, width),
            resolve_radius(&bl.0.height, height),
        ],
    ];

    style.border_styles = [
        map_border_style(ctx.styles.clone_border_top_style()),
        map_border_style(ctx.styles.clone_border_right_style()),
        map_border_style(ctx.styles.clone_border_bottom_style()),
        map_border_style(ctx.styles.clone_border_left_style()),
    ];
}

/// One side's computed border colour as RGBA, `currentColor` resolved against the element's `color`.
fn side_color(c: &style::values::computed::Color, ctx: &StyleContext<'_>) -> [u8; 4] {
    absolute_to_rgba(c.resolve_to_absolute(ctx.current_color))
}

/// Resolve one border-radius corner component to PDF pt. Stylo resolves the
/// length-percentage in CSS px (`Length::px()` -> f32 CSS px); `F32Units`
/// (`f32 -> Px`) then `.in_pt()` converts to `Pt`. Byte-neutral.
fn resolve_radius(
    r: &style::values::computed::length_percentage::NonNegativeLengthPercentage,
    basis: f32,
) -> crate::units::Pt {
    let radius_css_px: f32 =
        r.0.resolve(style::values::computed::Length::new(basis))
            .px();
    radius_css_px.as_px().in_pt()
}

pub(in crate::convert) fn map_border_style(
    bs: style::values::specified::BorderStyle,
) -> BorderStyleValue {
    use style::values::specified::BorderStyle as BS;
    match bs {
        BS::None | BS::Hidden => BorderStyleValue::None,
        BS::Dashed => BorderStyleValue::Dashed,
        BS::Dotted => BorderStyleValue::Dotted,
        BS::Double => BorderStyleValue::Double,
        BS::Groove => BorderStyleValue::Groove,
        BS::Ridge => BorderStyleValue::Ridge,
        BS::Inset => BorderStyleValue::Inset,
        BS::Outset => BorderStyleValue::Outset,
        BS::Solid => BorderStyleValue::Solid,
    }
}

#[cfg(test)]
mod tests {
    use super::{map_border_style, resolve_radius};
    use crate::units::F32Units;

    /// `extract_block_style` of the `<div>` in `<body>` with `style` as its inline style.
    fn div_style(style: &str) -> crate::draw_primitives::BlockStyle {
        let doc = crate::blitz_adapter::parse_and_layout(
            &format!(
                "<!doctype html><html><body style=\"margin:0\"><div style=\"{style}\"></div></body></html>"
            ),
            595.0_f32.as_px(),
            842.0_f32.as_px(),
            &[],
            false,
        );
        let body = doc.get_node(doc.root_element().children[1]).unwrap();
        let div = doc.get_node(body.children[0]).unwrap();
        super::super::extract_block_style(div, None, None)
    }

    const RED: [u8; 4] = [204, 0, 0, 255];
    const GREEN: [u8; 4] = [0, 170, 0, 255];
    const BLUE: [u8; 4] = [0, 0, 204, 255];
    const ORANGE: [u8; 4] = [255, 170, 0, 255];
    const BLACK: [u8; 4] = [0, 0, 0, 255];

    #[test]
    fn four_colours_are_read_per_side_in_trbl_order() {
        let s = div_style("border: 4px solid; border-color: #c00 #0a0 #00c #fa0");
        assert_eq!(s.border_colors, [RED, GREEN, BLUE, ORANGE]);
        assert_eq!(s.uniform_border_color(), None);
    }

    #[test]
    fn border_color_shorthand_expands_one_two_and_three_values() {
        let one = div_style("border: 4px solid; border-color: #c00");
        assert_eq!(one.border_colors, [RED; 4]);
        assert_eq!(one.uniform_border_color(), Some(RED));
        let two = div_style("border: 4px solid; border-color: #c00 #0a0");
        assert_eq!(two.border_colors, [RED, GREEN, RED, GREEN]);
        let three = div_style("border: 4px solid; border-color: #c00 #0a0 #00c");
        assert_eq!(three.border_colors, [RED, GREEN, BLUE, GREEN]);
    }

    #[test]
    fn a_side_shorthand_sets_only_its_own_colour_and_the_rest_stay_current_color() {
        // The unset sides keep the initial `currentColor`, here the (black) `color` of the element.
        let s = div_style("border-bottom: 4px solid #c00");
        assert_eq!(s.border_colors, [BLACK, BLACK, RED, BLACK]);
        let s = div_style("color: #00c; border-left: 4px solid #fa0");
        assert_eq!(s.border_colors, [BLUE, BLUE, BLUE, ORANGE]);
    }

    #[test]
    fn current_color_resolves_per_side_against_the_element_color() {
        let s = div_style(
            "color: #0a0; border: 2px solid; border-top-color: #c00; border-right-color: currentColor",
        );
        assert_eq!(s.border_colors, [RED, GREEN, GREEN, GREEN]);
        assert_eq!(s.uniform_border_color(), None);
        let all = div_style("color: #0a0; border: 2px solid");
        assert_eq!(all.uniform_border_color(), Some(GREEN));
    }

    #[test]
    fn a_translucent_side_keeps_its_alpha() {
        let s = div_style("border: 2px solid #c00; border-left-color: rgba(255, 170, 0, 0.5)");
        assert_eq!(s.border_colors[3], [255, 170, 0, 128]);
        assert_eq!(s.border_colors[0], RED);
    }

    use crate::draw_primitives::BorderStyleValue;
    use style::values::computed::{Length, LengthPercentage, Percentage};
    use style::values::generics::NonNegative;
    use style::values::specified::BorderStyle as BS;

    #[test]
    fn resolve_radius_absolute_via_in_pt() {
        // An absolute 8 CSS px radius converts to 8 * 0.75 = 6 pt, and the
        // basis is irrelevant for an absolute length (pass a nonzero basis to
        // prove it is ignored).
        let r = NonNegative(LengthPercentage::new_length(Length::new(8.0)));
        assert_eq!(resolve_radius(&r, 200.0).to_f32(), 6.0);
    }

    #[test]
    fn resolve_radius_percentage_of_basis_to_pt() {
        // 50% of a 100 CSS px basis is 50 CSS px, which converts to
        // 50 * 0.75 = 37.5 pt.
        let r = NonNegative(LengthPercentage::new_percent(Percentage(0.5)));
        assert_eq!(resolve_radius(&r, 100.0).to_f32(), 37.5);
    }

    #[test]
    fn none_and_hidden_collapse_to_none() {
        assert_eq!(map_border_style(BS::None), BorderStyleValue::None);
        assert_eq!(map_border_style(BS::Hidden), BorderStyleValue::None);
    }

    #[test]
    fn dashed_dotted_double() {
        assert_eq!(map_border_style(BS::Dashed), BorderStyleValue::Dashed);
        assert_eq!(map_border_style(BS::Dotted), BorderStyleValue::Dotted);
        assert_eq!(map_border_style(BS::Double), BorderStyleValue::Double);
    }

    #[test]
    fn groove_ridge_inset_outset() {
        assert_eq!(map_border_style(BS::Groove), BorderStyleValue::Groove);
        assert_eq!(map_border_style(BS::Ridge), BorderStyleValue::Ridge);
        assert_eq!(map_border_style(BS::Inset), BorderStyleValue::Inset);
        assert_eq!(map_border_style(BS::Outset), BorderStyleValue::Outset);
    }

    #[test]
    fn solid() {
        assert_eq!(map_border_style(BS::Solid), BorderStyleValue::Solid);
    }
}
