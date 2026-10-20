//! Minimal CSS sniffer for the multicol properties that stylo 0.8.0 gates to
//! its `gecko` engine. Blitz ships the `servo` feature only, so
//! `column-rule-{width,style,color}`, `column-fill`, and `break-inside` never
//! reach the `ComputedValues` we inspect elsewhere in the pipeline.
//!
//! Phase A of fulgur-v7a needs `column-rule-*` and `column-fill` to render
//! rules between adjacent columns and switch the multicol layout hook between
//! "balance" and "auto" (greedy fill). The follow-up (`fulgur-ftp`) extends
//! this module with `break-inside` (see [`ColumnStyleProps::break_inside`]).
//!
//! Scope and trade-offs:
//!
//! - Only inline `style="..."` attributes and top-level `<style>` blocks are
//!   scanned. External stylesheets loaded via `<link rel=stylesheet>` reach
//!   stylo/blitz for the properties they support but do **not** currently
//!   populate this side-table — tracked as fulgur-s5ro.
//! - `<style media="...">` blocks whose media excludes `all` / `print` are
//!   skipped by the harvester (`extract_column_style_table` in
//!   `blitz_adapter.rs`). Full CSS media-query evaluation is deferred.
//! - `@layer` and `@media` blocks are read (F87): Tailwind v4 puts every
//!   utility in `@layer utilities { … }`. Rules are folded in cascade order,
//!   which is layer order first (CSS Cascade 5 §6.4: a later layer beats an
//!   earlier one, a layer's own rules beat its sub-layers, unlayered rules
//!   beat every layer) and source order inside a layer. `@media` is read when
//!   the query is `[only|not]? all|screen` (see [`READER_MEDIA_TYPE`]); a query
//!   with a feature does not match. `@supports` and every other at-rule are
//!   skipped, and so are blocks nested deeper than `MAX_AT_RULE_DEPTH`.
//! - The selector grammar is intentionally tiny: type (`div`), class
//!   (`.foo`), id (`#bar`), universal (`*`), compound (`div.foo#bar`) and
//!   comma-separated lists. Unsupported combinators or pseudo-classes cause
//!   the whole rule to be dropped silently.
//! - Layer order, then source order wins (no specificity, no `!important`).
//!   Inline style is folded last so it beats stylesheet rules in every layer
//!   (Cascade 5 §6.1: element-attached styles come before layers).
//! - Length units: `pt` passes through, `px` is converted to `pt` using the
//!   canonical CSS factor `72 / 96`. `em` and `rem` resolve against a fixed
//!   `DEFAULT_EM_PX = 16.0` basis (Phase B will replace this with the
//!   container's computed font-size). `%`, `vh`, `vw`, `mm`, `cm`, `in`
//!   remain invalid — none of them make sense as a sub-point border width
//!   without consulting the surrounding box.

use std::collections::BTreeMap;

use selectors::SelectorList;
use selectors::matching::{
    MatchingContext, MatchingForInvalidation, MatchingMode, NeedsSelectorFlags, QuirksMode,
    SelectorCaches, matches_selector,
};
use style::selector_parser::{SelectorImpl, SelectorParser};
use style::stylesheets::UrlExtraData;

use cssparser::{
    AtRuleParser, BasicParseErrorKind, CowRcStr, DeclarationParser, ParseError, Parser,
    ParserInput, QualifiedRuleParser, RuleBodyItemParser, RuleBodyParser, StyleSheetParser, Token,
    color::{parse_hash_color, parse_named_color},
};

use crate::draw_primitives::{BreakAfter, BreakBefore, BreakInside};
use crate::units::F32Units;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// `column-rule-style` value. Only the three Phase A styles are modelled —
/// `double`, `groove`, `ridge`, `inset`, and `outset` are Phase C and fall
/// through to [`ColumnRuleStyle::None`] (rule is not drawn).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColumnRuleStyle {
    /// No rule — either the author set `none` or an unsupported style was
    /// provided. The renderer skips drawing.
    #[default]
    None,
    Solid,
    Dashed,
    Dotted,
}

/// A fully-resolved `column-rule` specification. Width is in PDF points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColumnRuleSpec {
    pub width: crate::units::Pt,
    pub style: ColumnRuleStyle,
    /// RGBA colour, matching the `[u8; 4]` convention used by
    /// `paragraph::TextRun` and `draw_primitives::BlockStyle::border_color`.
    pub color: [u8; 4],
}

impl Default for ColumnRuleSpec {
    fn default() -> Self {
        Self {
            width: 1.0_f32.as_pt(),
            style: ColumnRuleStyle::None,
            color: [0, 0, 0, 255],
        }
    }
}

/// `column-fill` value. Defaults to `balance` per CSS Multi-column Layout
/// Level 1.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColumnFill {
    #[default]
    Balance,
    Auto,
}

/// `page` value (CSS Page 3, fulgur-uebl). `None` means the author did not
/// declare the property; `Some(PageName::Auto)` means the author wrote
/// `page: auto` explicitly (which is also the initial value, but we
/// distinguish "set" from "absent" for the cascade); `Some(PageName::Named(_))`
/// names a specific page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PageName {
    Auto,
    Named(String),
}

/// Resolved properties for a single element. Fields remain `None` when the
/// author did not set them, so the consumer can tell "absent" from "set to
/// default" (important for the cascade — a later rule overrides only the
/// fields it declares).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ColumnStyleProps {
    pub rule: Option<ColumnRuleSpec>,
    pub fill: Option<ColumnFill>,
    /// `break-inside` as resolved by the fulgur-ftp sniffer. `None` means the
    /// author did not set the property — consumers should treat that as the
    /// initial value (`BreakInside::Auto`). Stored as `Option` for the same
    /// reason as the other fields: a later rule overwrites only the
    /// properties it declares (see `merge`).
    pub break_inside: Option<BreakInside>,
    /// `break-after` / `page-break-after` resolved by the sniffer.
    pub break_after: Option<BreakAfter>,
    /// `break-before` / `page-break-before` resolved by the sniffer.
    pub break_before: Option<BreakBefore>,
    /// `page: auto | <custom-ident>` (CSS Page 3). Drives implicit forced
    /// page breaks where adjacent boxes have different used page-names —
    /// resolution happens later in `compute_used_page_names`, this field
    /// stores only the author's declared value.
    pub page: Option<PageName>,
}

impl ColumnStyleProps {
    /// Fold `other` on top of `self`, with `Some(x)` in `other` overwriting
    /// the same field in `self` (last-wins, no specificity).
    fn merge(&mut self, other: ColumnStyleProps) {
        if other.rule.is_some() {
            self.rule = other.rule;
        }
        if other.fill.is_some() {
            self.fill = other.fill;
        }
        if other.break_inside.is_some() {
            self.break_inside = other.break_inside;
        }
        if other.break_after.is_some() {
            self.break_after = other.break_after;
        }
        if other.break_before.is_some() {
            self.break_before = other.break_before;
        }
        if other.page.is_some() {
            self.page = other.page;
        }
    }

    fn is_empty(&self) -> bool {
        self.rule.is_none()
            && self.fill.is_none()
            && self.break_inside.is_none()
            && self.break_after.is_none()
            && self.break_before.is_none()
            && self.page.is_none()
    }
}

/// Side-table keyed by the blitz DOM node id (`usize`). `BTreeMap` is chosen
/// over `HashMap` so iteration order is deterministic — even though this
/// table is only consulted during draw setup (never serialised directly),
/// determinism is a project-wide invariant (see `CLAUDE.md`).
pub(crate) type ColumnStyleTable = BTreeMap<usize, ColumnStyleProps>;

/// F144: true if some node of `table` declares a forced page break (`break-before: page` or `break-after: page`).
/// When none does, no walk for "a forced break below" can find one.
pub(crate) fn has_forced_page_break(table: &ColumnStyleTable) -> bool {
    table.values().any(|p| {
        matches!(p.break_before, Some(BreakBefore::Page))
            || matches!(p.break_after, Some(BreakAfter::Page))
    })
}

/// F144: true if some node of `table` names a page (`page: <custom-ident>`). With none, every used page-name is the
/// unnamed page, so there is nothing to compute or compare.
pub(crate) fn has_named_page(table: &ColumnStyleTable) -> bool {
    table
        .values()
        .any(|p| matches!(p.page, Some(PageName::Named(_))))
}

/// A single parsed stylesheet rule: one or more selectors and the properties
/// that apply when any of them matches.
#[derive(Clone, Debug)]
pub(crate) struct StyleRule {
    /// The selector list, parsed by Stylo's own parser (the one Blitz uses), so the reader supports exactly the
    /// selectors the engine does: combinators, attributes, `:not()`, `:is()`, `:where()`, structural pseudo-classes.
    pub selectors: SelectorList<SelectorImpl>,
    /// The normal declarations.
    pub props: ColumnStyleProps,
    /// The `!important` declarations (Cascade 5 §6.1: a separate, higher tier of the cascade).
    pub important: ColumnStyleProps,
    /// Cascade layer the rule was written in; `0` for unlayered rules.
    pub layer: LayerId,
    /// Position of `layer` in the cascade ([`LayerTree::ranks`]): higher beats lower for normal declarations,
    /// unlayered rules are highest.
    pub rank: usize,
}

// ---------------------------------------------------------------------------
// Length and colour helpers
// ---------------------------------------------------------------------------

/// Default font-size basis used to resolve `em` / `rem` lengths when we
/// don't have a live cascade to consult. CSS default is `1rem == 16px`;
/// Phase A does not thread a real computed basis through this parser, so
/// accepting the default keeps common stylesheets (`column-rule-width:
/// 0.25rem`) from silently dropping. Phase B will replace this with the
/// container's resolved font-size.
const DEFAULT_EM_PX: f32 = 16.0;

/// Convert a CSS length token to PDF points. Accepts `pt` and `px` natively;
/// `em` and `rem` are resolved against a fixed 16px basis so rule widths
/// authored in ems don't silently drop. `%`, `mm`, `cm`, `in`, `vh`, `vw`
/// remain rejected — none of them make sense as a sub-point border width
/// without consulting the surrounding box, which Phase A does not expose.
fn length_to_pt(value: f32, unit: &str) -> Option<f32> {
    if unit.eq_ignore_ascii_case("pt") {
        Some(value)
    } else if unit.eq_ignore_ascii_case("px") {
        Some(value * 72.0 / 96.0)
    } else if unit.eq_ignore_ascii_case("em") || unit.eq_ignore_ascii_case("rem") {
        // em/rem → px via the default basis, then px → pt.
        Some(value * DEFAULT_EM_PX * 72.0 / 96.0)
    } else {
        None
    }
}

/// Parse a single `<length>` from `input`. Unlike the GCPM variant we reject
/// `mm`/`cm`/`in` too — the column-rule spec is sub-point and designers use
/// `px`/`pt`/`em` exclusively in practice; folding more units here would
/// just give inconsistent renders between the multicol rule and the rest
/// of the border model.
fn parse_length<'i>(input: &mut Parser<'i, '_>) -> Result<f32, ParseError<'i, ()>> {
    let token = input.next()?.clone();
    match token {
        Token::Dimension { value, unit, .. } => length_to_pt(value, &unit)
            .ok_or_else(|| input.new_error(BasicParseErrorKind::QualifiedRuleInvalid)),
        Token::Number { value: 0.0, .. } => Ok(0.0),
        _ => Err(input.new_error(BasicParseErrorKind::QualifiedRuleInvalid)),
    }
}

/// Parse a `<color>` from `input`. Supports `#rgb` / `#rgba` / `#rrggbb` /
/// `#rrggbbaa` hashes, CSS named colours, and the functional notations
/// `rgb()` / `rgba()` / `hsl()` / `hsla()` (legacy comma syntax; CSS Color
/// 3 – modern `rgb(r g b / a)` space-separated form is not modelled).
/// Deliberate returns of `Err` for unsupported keywords (`currentcolor`,
/// `inherit`, system colours): the declaration drops silently while
/// siblings in the same block continue to populate.
///
/// **Why not `cssparser::Color::parse`?** The Phase A plan referenced that
/// API, but cssparser 0.35 does not ship one — the full colour parser
/// lives in a separate `cssparser-color` crate. Adding that dependency is
/// out of scope for this targeted fix (hard-constrained to this file), so
/// we implement the two functional forms directly on top of the tokenizer.
fn parse_color<'i>(input: &mut Parser<'i, '_>) -> Result<[u8; 4], ParseError<'i, ()>> {
    let token = input.next()?.clone();
    let err = || input.new_error::<()>(BasicParseErrorKind::QualifiedRuleInvalid);
    match token {
        Token::IDHash(ref s) | Token::Hash(ref s) => {
            let (r, g, b, a) = parse_hash_color(s.as_bytes()).map_err(|_| err())?;
            let alpha_u8 = (a * 255.0_f32).round().clamp(0.0, 255.0) as u8;
            Ok([r, g, b, alpha_u8])
        }
        Token::Ident(ref name) => {
            if name.eq_ignore_ascii_case("transparent") {
                Ok([0, 0, 0, 0])
            } else if let Ok((r, g, b)) = parse_named_color(&name.to_ascii_lowercase()) {
                Ok([r, g, b, 255])
            } else {
                // `currentcolor`, `inherit`, system colours, typos — not
                // representable as an `[u8; 4]`. Bail so the containing
                // declaration drops.
                Err(err())
            }
        }
        Token::Function(ref name) => {
            let fn_name = name.to_ascii_lowercase();
            // Build the fallback error eagerly so we release the immutable
            // borrow of `input` before calling `parse_nested_block`, which
            // takes `&mut self`.
            let nested = match fn_name.as_str() {
                "rgb" | "rgba" => input.parse_nested_block(parse_rgb_args),
                "hsl" | "hsla" => input.parse_nested_block(parse_hsl_args),
                _ => return Err(err()),
            };
            nested.map_err(|_| input.new_error::<()>(BasicParseErrorKind::QualifiedRuleInvalid))
        }
        _ => Err(err()),
    }
}

/// Parse a single rgb/rgba channel as either an integer 0..255 number or a
/// percentage. Clamps to `[0, 255]` on overflow, matching the CSS Color
/// spec's "clamp on compute" rule.
fn parse_rgb_channel<'i>(input: &mut Parser<'i, '_>) -> Result<u8, ParseError<'i, ()>> {
    let token = input.next()?.clone();
    match token {
        Token::Number { value, .. } => Ok(value.round().clamp(0.0, 255.0) as u8),
        Token::Percentage { unit_value, .. } => {
            Ok((unit_value * 255.0).round().clamp(0.0, 255.0) as u8)
        }
        _ => Err(input.new_error(BasicParseErrorKind::QualifiedRuleInvalid)),
    }
}

/// Parse an alpha value as either a 0..1 number or a percentage. Scales to
/// 0..255 and rounds. Clamps to `[0, 255]`.
fn parse_alpha_value<'i>(input: &mut Parser<'i, '_>) -> Result<u8, ParseError<'i, ()>> {
    let token = input.next()?.clone();
    match token {
        Token::Number { value, .. } => Ok((value * 255.0).round().clamp(0.0, 255.0) as u8),
        Token::Percentage { unit_value, .. } => {
            Ok((unit_value * 255.0).round().clamp(0.0, 255.0) as u8)
        }
        _ => Err(input.new_error(BasicParseErrorKind::QualifiedRuleInvalid)),
    }
}

/// Parse the arguments of `rgb(...)` / `rgba(...)` — legacy comma syntax
/// only. Accepts `rgb(r, g, b)` or `rgb(r, g, b, a)` (alpha optional; the
/// name distinction between `rgb` and `rgba` has not mattered since CSS
/// Color 4).
fn parse_rgb_args<'i>(input: &mut Parser<'i, '_>) -> Result<[u8; 4], ParseError<'i, ()>> {
    let r = parse_rgb_channel(input)?;
    input.expect_comma()?;
    let g = parse_rgb_channel(input)?;
    input.expect_comma()?;
    let b = parse_rgb_channel(input)?;
    let a = if input.try_parse(|i| i.expect_comma()).is_ok() {
        parse_alpha_value(input)?
    } else {
        255
    };
    input.expect_exhausted()?;
    Ok([r, g, b, a])
}

/// Parse the arguments of `hsl(...)` / `hsla(...)` — legacy comma syntax
/// only. Hue is a number in degrees; saturation and lightness are required
/// percentages. Converts to sRGB using the CSS Color 3 formula.
fn parse_hsl_args<'i>(input: &mut Parser<'i, '_>) -> Result<[u8; 4], ParseError<'i, ()>> {
    let hue_deg = match input.next()?.clone() {
        Token::Number { value, .. } => value,
        _ => return Err(input.new_error(BasicParseErrorKind::QualifiedRuleInvalid)),
    };
    input.expect_comma()?;
    let s = match input.next()?.clone() {
        Token::Percentage { unit_value, .. } => unit_value.clamp(0.0, 1.0),
        _ => return Err(input.new_error(BasicParseErrorKind::QualifiedRuleInvalid)),
    };
    input.expect_comma()?;
    let l = match input.next()?.clone() {
        Token::Percentage { unit_value, .. } => unit_value.clamp(0.0, 1.0),
        _ => return Err(input.new_error(BasicParseErrorKind::QualifiedRuleInvalid)),
    };
    let a = if input.try_parse(|i| i.expect_comma()).is_ok() {
        parse_alpha_value(input)?
    } else {
        255
    };
    input.expect_exhausted()?;

    let (r, g, b) = hsl_to_rgb(hue_deg, s, l);
    Ok([r, g, b, a])
}

/// CSS Color 3 HSL → sRGB conversion. Hue is in degrees (any real number;
/// wrapped via modulo). Saturation and lightness are normalised to
/// `[0, 1]`. Returns 8-bit sRGB channels with standard rounding.
fn hsl_to_rgb(hue_deg: f32, s: f32, l: f32) -> (u8, u8, u8) {
    // Normalise hue to [0, 1) — rem_euclid keeps negatives positive.
    let h = (hue_deg.rem_euclid(360.0)) / 360.0;

    let hue_to_rgb = |p: f32, q: f32, mut t: f32| -> f32 {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };

    let (r_f, g_f, b_f) = if s == 0.0 {
        (l, l, l)
    } else {
        let q = if l < 0.5 {
            l * (1.0 + s)
        } else {
            l + s - l * s
        };
        let p = 2.0 * l - q;
        (
            hue_to_rgb(p, q, h + 1.0 / 3.0),
            hue_to_rgb(p, q, h),
            hue_to_rgb(p, q, h - 1.0 / 3.0),
        )
    };

    (
        (r_f * 255.0).round().clamp(0.0, 255.0) as u8,
        (g_f * 255.0).round().clamp(0.0, 255.0) as u8,
        (b_f * 255.0).round().clamp(0.0, 255.0) as u8,
    )
}

fn parse_rule_style_ident(ident: &str) -> Option<ColumnRuleStyle> {
    match ident.to_ascii_lowercase().as_str() {
        "none" => Some(ColumnRuleStyle::None),
        "solid" => Some(ColumnRuleStyle::Solid),
        "dashed" => Some(ColumnRuleStyle::Dashed),
        "dotted" => Some(ColumnRuleStyle::Dotted),
        // `double | groove | ridge | inset | outset` are Phase C — drop the
        // whole shorthand rather than falsely render as `None`.
        _ => None,
    }
}

fn parse_column_fill_value<'i>(
    input: &mut Parser<'i, '_>,
) -> Result<ColumnFill, ParseError<'i, ()>> {
    let ident = input.expect_ident()?.clone();
    if ident.eq_ignore_ascii_case("auto") {
        Ok(ColumnFill::Auto)
    } else if ident.eq_ignore_ascii_case("balance") {
        Ok(ColumnFill::Balance)
    } else {
        Err(input.new_error(BasicParseErrorKind::QualifiedRuleInvalid))
    }
}

/// Parse the value side of `break-inside`. Accepts the three avoid flavours
/// (`avoid`, `avoid-page`, `avoid-column`) as `BreakInside::Avoid`, and
/// `auto` as `BreakInside::Auto`. Other idents (`avoid-region`,
/// `left`, `right`, typos …) drop the declaration so siblings still apply.
fn parse_break_inside_value<'i>(
    input: &mut Parser<'i, '_>,
) -> Result<BreakInside, ParseError<'i, ()>> {
    let ident = input.expect_ident()?.clone();
    match ident.as_ref().to_ascii_lowercase().as_str() {
        "avoid" | "avoid-page" | "avoid-column" => Ok(BreakInside::Avoid),
        "auto" => Ok(BreakInside::Auto),
        _ => Err(input.new_error(BasicParseErrorKind::QualifiedRuleInvalid)),
    }
}

/// Value of `break-before` / `break-after`, as the side table models it: `true` is a forced page break.
///
/// css-break-3 §3.1: `break-*` takes `auto | avoid | avoid-page | page | left | right | recto | verso | avoid-column |
/// column` (and region values, which are dropped). `avoid*` and `column` force no page break (`column` only breaks
/// columns, and the table has no column breaks); what matters is that a later one overrides an earlier `page`.
/// §3.4: the legacy `page-break-*` takes `auto | always | avoid | left | right`; `always` is not a value of
/// `break-*` (Chrome ignores it there), and `page`, `recto`, `verso`, `column`, `avoid-page` are not legacy values.
fn parse_break_forced<'i>(
    input: &mut Parser<'i, '_>,
    legacy: bool,
) -> Result<bool, ParseError<'i, ()>> {
    let ident = input.expect_ident()?.clone();
    let lower = ident.as_ref().to_ascii_lowercase();
    let forced = match (lower.as_str(), legacy) {
        ("auto" | "avoid", _) => false,
        ("left" | "right", _) => true,
        ("always", true) => true,
        ("avoid-page" | "avoid-column" | "column", false) => false,
        ("page" | "recto" | "verso", false) => true,
        _ => return Err(input.new_error(BasicParseErrorKind::QualifiedRuleInvalid)),
    };
    Ok(forced)
}

fn parse_break_after_value<'i>(
    input: &mut Parser<'i, '_>,
    legacy: bool,
) -> Result<BreakAfter, ParseError<'i, ()>> {
    parse_break_forced(input, legacy).map(|f| {
        if f {
            BreakAfter::Page
        } else {
            BreakAfter::Auto
        }
    })
}

fn parse_break_before_value<'i>(
    input: &mut Parser<'i, '_>,
    legacy: bool,
) -> Result<BreakBefore, ParseError<'i, ()>> {
    parse_break_forced(input, legacy).map(|f| {
        if f {
            BreakBefore::Page
        } else {
            BreakBefore::Auto
        }
    })
}

/// Parse a `page: auto | <custom-ident>` value. CSS Page 3 §5.3 defines
/// the page-name grammar; we match `auto` (case-insensitive) as the
/// "unnamed" sentinel and accept any other ident as a named page.
/// Anything else (numbers, function calls, multiple tokens) drops the
/// declaration silently per the project-wide "no panic on bad CSS"
/// invariant.
fn parse_page_value<'i>(input: &mut Parser<'i, '_>) -> Result<PageName, ParseError<'i, ()>> {
    let ident = input.expect_ident()?.clone();
    let s = ident.as_ref();
    if s.eq_ignore_ascii_case("auto") {
        Ok(PageName::Auto)
    } else {
        // Reserved CSS-wide keywords (`inherit`, `initial`, `unset`,
        // `revert`) are dropped — we don't model the cascade richness
        // they imply. Numeric idents are also rejected by `expect_ident`
        // before reaching here.
        let lower = s.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "inherit" | "initial" | "unset" | "revert" | "revert-layer"
        ) {
            Err(input.new_error(BasicParseErrorKind::QualifiedRuleInvalid))
        } else {
            // Bound the retained name length: a CSS `<custom-ident>` is
            // unbounded and this string is cloned into the column style table
            // and once per node while resolving used page names
            // (O(nodes × name_len)). Truncate an over-long name at a UTF-8
            // char boundary (clamp-and-warn); realistic names are far shorter.
            let name = if s.len() > crate::MAX_PAGE_NAME_BYTES {
                let mut end = crate::MAX_PAGE_NAME_BYTES;
                while end > 0 && !s.is_char_boundary(end) {
                    end -= 1;
                }
                log::warn!(
                    "page: name exceeds {} bytes; truncated (defensive bound)",
                    crate::MAX_PAGE_NAME_BYTES
                );
                s[..end].to_string()
            } else {
                s.to_string()
            };
            Ok(PageName::Named(name))
        }
    }
}

// ---------------------------------------------------------------------------
// Declaration block parser
// ---------------------------------------------------------------------------

/// The declarations of one block, split by importance (Cascade 5 §6.1).
#[derive(Clone, Debug, Default)]
pub(crate) struct Declared {
    pub normal: ColumnStyleProps,
    pub important: ColumnStyleProps,
}

/// Parse a CSS declaration block (no surrounding braces) into [`Declared`]. Invalid declarations drop silently;
/// valid ones fill their field, in the `normal` or the `important` half. Last-declaration-wins within a block and
/// half.
pub(crate) fn parse_declarations(css: &str) -> Declared {
    let mut declared = Declared::default();
    let mut input = ParserInput::new(css);
    let mut parser = Parser::new(&mut input);

    let mut decl_parser = ColumnDeclParser {
        normal: &mut declared.normal,
        important: &mut declared.important,
    };
    // `RuleBodyParser` walks `decl; decl; decl;` and feeds each declaration
    // through `DeclarationParser::parse_value`. Errors from one declaration
    // do not abort the block — they surface as `Err` items in the iterator
    // which we discard.
    let iter = RuleBodyParser::new(&mut parser, &mut decl_parser);
    for item in iter {
        let _ = item;
    }
    declared
}

/// The normal declarations of a block (tests of the value grammar).
#[cfg(test)]
pub(crate) fn parse_declaration_block(css: &str) -> ColumnStyleProps {
    parse_declarations(css).normal
}

struct ColumnDeclParser<'a> {
    normal: &'a mut ColumnStyleProps,
    important: &'a mut ColumnStyleProps,
}

/// What follows a declaration's value.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tail {
    /// Nothing.
    Normal,
    /// `!important` (Syntax 3 §5.4.6: `!`, optional whitespace, the ident `important`, any case).
    Important,
    /// Anything else after a `!`: the declaration is invalid.
    Invalid,
}

/// Look at what follows the value, without consuming anything.
fn scan_tail(input: &mut Parser<'_, '_>) -> Tail {
    let state = input.state();
    let _ = input.parse_until_before(cssparser::Delimiter::Bang, |i| {
        while i.next().is_ok() {}
        Ok::<(), ParseError<'_, ()>>(())
    });
    let tail = if input.is_exhausted() {
        Tail::Normal
    } else if input.try_parse(cssparser::parse_important).is_ok() && input.is_exhausted() {
        Tail::Important
    } else {
        Tail::Invalid
    };
    input.reset(&state);
    tail
}

impl<'i, 'a> DeclarationParser<'i> for ColumnDeclParser<'a> {
    type Declaration = ();
    type Error = ();

    fn parse_value<'t>(
        &mut self,
        name: CowRcStr<'i>,
        input: &mut Parser<'i, 't>,
        _decl_start: &cssparser::ParserState,
    ) -> Result<(), ParseError<'i, ()>> {
        let props = match scan_tail(input) {
            Tail::Normal => &mut *self.normal,
            Tail::Important => &mut *self.important,
            Tail::Invalid => {
                // `a: b !foo` is invalid as a whole.
                while input.next().is_ok() {}
                return Ok(());
            }
        };
        // The value stops before the `!`; `parse_until_before` hands the value parsers an input that ends there.
        let _ = input.parse_until_before(cssparser::Delimiter::Bang, |input| {
            parse_value_into(props, &name, input);
            Ok::<(), ParseError<'i, ()>>(())
        });
        // Consume `!important` so that the caller's `parse_entirely` is satisfied.
        let _ = input.try_parse(cssparser::parse_important);
        Ok(())
    }
}

fn parse_value_into(props: &mut ColumnStyleProps, name: &str, input: &mut Parser<'_, '_>) {
    if name.eq_ignore_ascii_case("column-rule") {
        if let Some(spec) = parse_column_rule_shorthand(input) {
            // A shorthand with `style: None` is equivalent to "don't
            // draw" — we store it anyway so a later longhand can read
            // the width/colour the author still wanted.
            props.rule = Some(spec);
        }
    } else if name.eq_ignore_ascii_case("column-rule-width") {
        if let Ok(w) = input.parse_entirely(parse_length) {
            let mut spec = props.rule.unwrap_or_default();
            spec.width = w.as_pt();
            props.rule = Some(spec);
        }
    } else if name.eq_ignore_ascii_case("column-rule-style") {
        let result = input.parse_entirely(|input| {
            let ident = input.expect_ident()?.clone();
            parse_rule_style_ident(&ident)
                .ok_or_else(|| input.new_error::<()>(BasicParseErrorKind::QualifiedRuleInvalid))
        });
        if let Ok(style) = result {
            let mut spec = props.rule.unwrap_or_default();
            spec.style = style;
            props.rule = Some(spec);
        }
    } else if name.eq_ignore_ascii_case("column-rule-color") {
        if let Ok(color) = input.parse_entirely(parse_color) {
            let mut spec = props.rule.unwrap_or_default();
            spec.color = color;
            props.rule = Some(spec);
        }
    } else if name.eq_ignore_ascii_case("column-fill") {
        if let Ok(fill) = input.parse_entirely(parse_column_fill_value) {
            props.fill = Some(fill);
        }
    } else if name.eq_ignore_ascii_case("break-inside") {
        // `break-inside` (CSS Fragmentation Level 3) maps
        // `avoid-page` / `avoid-column` / `avoid` all onto
        // [`BreakInside::Avoid`]: fulgur's fragmentation model only
        // distinguishes "may split" from "atomic", so the three avoid
        // flavours collapse into a single lattice point. `auto` is the
        // initial value; unknown idents drop the declaration silently
        // so siblings keep applying.
        if let Ok(bi) = input.parse_entirely(parse_break_inside_value) {
            props.break_inside = Some(bi);
        }
    } else if name.eq_ignore_ascii_case("break-after")
        || name.eq_ignore_ascii_case("page-break-after")
    {
        let legacy = name.eq_ignore_ascii_case("page-break-after");
        if let Ok(v) = input.parse_entirely(|i| parse_break_after_value(i, legacy)) {
            props.break_after = Some(v);
        }
    } else if name.eq_ignore_ascii_case("break-before")
        || name.eq_ignore_ascii_case("page-break-before")
    {
        let legacy = name.eq_ignore_ascii_case("page-break-before");
        if let Ok(v) = input.parse_entirely(|i| parse_break_before_value(i, legacy)) {
            props.break_before = Some(v);
        }
    } else if name.eq_ignore_ascii_case("page") {
        if let Ok(v) = input.parse_entirely(parse_page_value) {
            props.page = Some(v);
        }
    } else {
        // Unknown property — discard its value tokens silently.
        while input.next().is_ok() {}
    }
}

impl<'i, 'a> AtRuleParser<'i> for ColumnDeclParser<'a> {
    type Prelude = ();
    type AtRule = ();
    type Error = ();
}

impl<'i, 'a> QualifiedRuleParser<'i> for ColumnDeclParser<'a> {
    type Prelude = ();
    type QualifiedRule = ();
    type Error = ();
}

impl<'i, 'a> RuleBodyItemParser<'i, (), ()> for ColumnDeclParser<'a> {
    fn parse_declarations(&self) -> bool {
        true
    }
    fn parse_qualified(&self) -> bool {
        false
    }
}

/// Parse the value side of `column-rule: <width> || <style> || <color>`.
/// Returns `None` if any component is invalid so the whole shorthand drops
/// — per CSS rules a malformed shorthand is "ignored entirely" and we
/// mirror that strict behaviour. Missing components fall back to the
/// defaults in [`ColumnRuleSpec::default`].
fn parse_column_rule_shorthand(input: &mut Parser<'_, '_>) -> Option<ColumnRuleSpec> {
    let mut width: Option<f32> = None;
    let mut style: Option<ColumnRuleStyle> = None;
    let mut color: Option<[u8; 4]> = None;

    // The shorthand accepts the three components in any order but at most
    // one of each. Loop up to three times and try each slot in turn.
    for _ in 0..3 {
        if input.is_exhausted() {
            break;
        }

        // Try width.
        if width.is_none() {
            let parsed = input.try_parse(parse_length);
            if let Ok(w) = parsed {
                width = Some(w);
                continue;
            }
        }

        // Try style.
        if style.is_none() {
            let parsed = input.try_parse(|input| {
                let ident = input.expect_ident()?.clone();
                parse_rule_style_ident(&ident)
                    .ok_or_else(|| input.new_error::<()>(BasicParseErrorKind::QualifiedRuleInvalid))
            });
            if let Ok(s) = parsed {
                style = Some(s);
                continue;
            }
        }

        // Try color.
        if color.is_none() {
            let parsed = input.try_parse(parse_color);
            if let Ok(c) = parsed {
                color = Some(c);
                continue;
            }
        }

        // No component accepted the next token — malformed shorthand.
        return None;
    }

    // Any trailing tokens → invalid.
    if !input.is_exhausted() {
        return None;
    }

    let mut spec = ColumnRuleSpec::default();
    if let Some(w) = width {
        spec.width = w.as_pt();
    }
    if let Some(s) = style {
        spec.style = s;
    }
    if let Some(c) = color {
        spec.color = c;
    }
    Some(spec)
}

// ---------------------------------------------------------------------------
// Stylesheet parser
// ---------------------------------------------------------------------------

/// Media type of the device Stylo is given, as the reader sees it in the CSS text it receives.
///
/// Blitz builds its Stylo device as `MediaType::screen()` (`blitz-dom` `document.rs`, `make_device`), so the
/// `@media` blocks Stylo applies are `all` and `screen`. The embedding crate (`htmltopdf`) turns the author's
/// `print` into `screen` and back in every `@media` prelude before this crate sees the CSS, so the author's `print`
/// rules reach it as `screen` ones. The reader must read exactly the blocks Stylo applies, otherwise a break rule and
/// the other declarations of its `@media` block would switch separately. Evaluating `print` here would apply the
/// author's screen rules for that embedder.
///
/// When Blitz gets a print device, and the swap goes away, this constant flips with it.
/// `blitz_adapter::tests::column_reader_media_type_agrees_with_stylo_device` fails the day that happens.
pub(crate) const READER_MEDIA_TYPE: &str = "screen";

/// Deepest nesting of `@layer` / `@media` blocks the reader descends into; deeper blocks are skipped like any other
/// unknown at-rule. Descending recurses, and a stack overflow aborts the process (a panic guard cannot catch it).
/// Real documents nest 3 deep at most (Tailwind: `@layer utilities { @media print { … } }`).
const MAX_AT_RULE_DEPTH: usize = 32;

/// Index of a cascade layer in a [`LayerTree`]; `0` is the root (unlayered styles).
pub(crate) type LayerId = usize;

/// The cascade layers of one stylesheet, in order of first appearance (CSS Cascade 5 §6.4.1–§6.4.2).
///
/// Node `0` is the root: the unlayered styles. A layer's children are its sub-layers in the order they were first
/// named. [`LayerTree::ranks`] flattens the tree into the order in which layers are sorted for normal declarations.
#[derive(Debug)]
pub(crate) struct LayerTree {
    nodes: Vec<LayerNode>,
}

#[derive(Debug, Default)]
struct LayerNode {
    /// Sub-layers in order of first appearance, anonymous ones included.
    children: Vec<LayerId>,
    /// Named sub-layers. `BTreeMap`, not `HashMap`: determinism is a project-wide invariant.
    named: BTreeMap<String, LayerId>,
}

impl LayerTree {
    pub(crate) fn new() -> Self {
        Self {
            nodes: vec![LayerNode::default()],
        }
    }

    /// The sub-layer of `parent` called `name`, created on first use. `None` is an anonymous layer, and every
    /// anonymous layer is a new one (§6.4.1).
    pub(crate) fn child(&mut self, parent: LayerId, name: Option<&str>) -> LayerId {
        if let Some(&id) = name.and_then(|n| self.nodes[parent].named.get(n)) {
            return id;
        }
        let id = self.nodes.len();
        self.nodes.push(LayerNode::default());
        self.nodes[parent].children.push(id);
        if let Some(name) = name {
            self.nodes[parent].named.insert(name.to_string(), id);
        }
        id
    }

    /// `rank[layer]`: the position of the layer in the cascade, ascending, so that a normal declaration of a higher
    /// rank beats a lower one. Post-order: a layer's sub-layers come first, then its own rules (§6.4.2, "implicit
    /// final sub-layer"), and the root, the unlayered styles, last (§6.4.3). Iterative: a dotted name can be long.
    pub(crate) fn ranks(&self) -> Vec<usize> {
        let mut rank = vec![0; self.nodes.len()];
        let mut next = 0;
        // (layer, index of the next child to visit)
        let mut stack: Vec<(LayerId, usize)> = vec![(0, 0)];
        while let Some(&mut (id, ref mut at)) = stack.last_mut() {
            if let Some(&child) = self.nodes[id].children.get(*at) {
                *at += 1;
                stack.push((child, 0));
            } else {
                rank[id] = next;
                next += 1;
                stack.pop();
            }
        }
        rank
    }
}

/// Does a media query list match the device? CSS Conditional 3 §5, Media Queries 4 §2.1–§2.3.
///
/// A query matches when it is `[only | not]? <media-type>` with nothing after the type and the type is `all` or
/// [`READER_MEDIA_TYPE`]; `not` negates. A query with a feature (`and (hover: hover)`, `(width >= 40rem)`), a
/// deprecated type, or any syntax the reader does not know does not match: the block is skipped, which leaves today's
/// value. A comma separates queries, any one of which may match. An empty list matches (§2.1).
pub(crate) fn media_query_list_matches(input: &mut Parser<'_, '_>) -> bool {
    media_query_list_matches_on(input, READER_MEDIA_TYPE, false)
}

/// [`media_query_list_matches`] for a device of type `device`. With `features_match`, `<type> and <features>` is
/// taken to match when the type does (the features cannot be evaluated here): the `media` attribute of a `<style>`
/// applied or not as a whole, so a sheet for `print and (min-width: 1px)` must stay on for a print device.
pub(crate) fn media_query_list_matches_on(
    input: &mut Parser<'_, '_>,
    device: &str,
    features_match: bool,
) -> bool {
    use cssparser::Delimiter;
    let mut any = input.is_exhausted();
    while !input.is_exhausted() {
        let query = input.parse_until_before(Delimiter::Comma, |i| {
            Ok::<bool, ParseError<'_, ()>>(media_query_matches(i, device, features_match))
        });
        any |= query.unwrap_or(false);
        // Consume the comma; at the end of the input there is none.
        let _ = input.next();
    }
    any
}

fn media_query_matches(input: &mut Parser<'_, '_>, device: &str, features_match: bool) -> bool {
    let Ok(first) = input.expect_ident_cloned() else {
        return false;
    };
    let (negated, media_type) = if first.eq_ignore_ascii_case("not") {
        match input.expect_ident_cloned() {
            Ok(t) => (true, t),
            Err(_) => return false,
        }
    } else if first.eq_ignore_ascii_case("only") {
        match input.expect_ident_cloned() {
            Ok(t) => (false, t),
            Err(_) => return false,
        }
    } else {
        (false, first)
    };
    let is_all = media_type.eq_ignore_ascii_case("all");
    let is_device = media_type.eq_ignore_ascii_case(device);
    // `not` negates a known type only: `not speech` is not "matches everything".
    let is_other_known =
        media_type.eq_ignore_ascii_case("print") || media_type.eq_ignore_ascii_case("screen");
    if !(is_all || is_device || is_other_known) {
        return false;
    }
    let type_matches = is_all || is_device;
    if !input.is_exhausted() {
        // `<type> and <features>`: only with the optimistic reading, and never under `not` (a negated feature
        // list is not the negated type).
        let has_and = input.expect_ident_matching("and").is_ok();
        // The segment must be consumed whole for `parse_until_before`.
        while input.next().is_ok() {}
        return features_match && has_and && !negated && type_matches;
    }
    type_matches != negated
}

/// Parse a top-level CSS source into [`StyleRule`]s, in ascending cascade order. Qualified rules whose prelude
/// contains an unsupported selector are dropped. Empty property blocks are kept out of the result to save memory.
///
/// At-rules:
///
/// - `@layer` (block) is descended into with the rules assigned to that layer; `@layer a, b;` only names layers;
/// - `@media` is descended into at the same layer when [`media_query_list_matches`];
/// - every other at-rule (`@supports`, `@font-face`, `@property`, `@page`, `@import`, …) is skipped, and so is a
///   block nested deeper than [`MAX_AT_RULE_DEPTH`]: returning `Err` from `parse_prelude` makes cssparser consume
///   and discard the block.
///
/// The result is sorted (stably) by layer rank ([`LayerTree::ranks`], unlayered rules last), so source order holds
/// inside a layer. [`build_column_style_table`] finishes the cascade per element: importance, then layer rank
/// (reversed for important declarations), then specificity, then source order.
pub(crate) fn parse_stylesheet(source: &str) -> Vec<StyleRule> {
    let mut rules = Vec::new();
    let mut layers = LayerTree::new();
    let mut input = ParserInput::new(source);
    let mut parser = Parser::new(&mut input);

    // Stylo's selector parser wants the URL the sheet came from; it only asks whether the scheme is `chrome`.
    let url_data =
        UrlExtraData::from(url::Url::parse("about:blank").expect("a constant, valid URL"));
    let mut sheet = SheetParser {
        rules: &mut rules,
        layers: &mut layers,
        url_data: &url_data,
        layer: 0,
        depth: 0,
    };
    let iter = StyleSheetParser::new(&mut parser, &mut sheet);
    for item in iter {
        let _ = item;
    }
    let ranks = layers.ranks();
    for rule in &mut rules {
        rule.rank = ranks[rule.layer];
    }
    rules.sort_by_key(|r| r.rank);
    rules
}

struct SheetParser<'a> {
    rules: &'a mut Vec<StyleRule>,
    layers: &'a mut LayerTree,
    url_data: &'a UrlExtraData,
    /// Layer the rules of this (sub-)sheet belong to.
    layer: LayerId,
    /// Nesting depth of this (sub-)sheet; the top level is `0`.
    depth: usize,
}

/// What an at-rule prelude parsed to.
enum AtPrelude {
    /// `@layer` with its comma-separated names; each name is its dotted path. An empty list is an anonymous layer.
    Layer(Vec<Vec<String>>),
    /// A matching `@media`.
    Media,
}

/// `<layer-name> = <ident> [ '.' <ident> ]*` (Cascade 5 §6.4.2).
fn parse_layer_name(input: &mut Parser<'_, '_>) -> Option<Vec<String>> {
    let mut path = vec![input.expect_ident().ok()?.as_ref().to_string()];
    // `a.b` tokenises as an ident, a `.` and an ident. Whitespace around the dot is invalid (`a .b` is two
    // tokens of a prelude that is not a name list, so the whole at-rule is invalid).
    while input
        .try_parse(|i| match i.next_including_whitespace() {
            Ok(Token::Delim('.')) => Ok(()),
            _ => Err(i.new_custom_error::<(), ()>(())),
        })
        .is_ok()
    {
        path.push(
            input
                .next_including_whitespace()
                .ok()
                .and_then(|t| match t {
                    Token::Ident(n) => Some(n.as_ref().to_string()),
                    _ => None,
                })?,
        );
    }
    Some(path)
}

impl SheetParser<'_> {
    /// Rules of `block` belong to `layer`, one level deeper.
    fn parse_nested<'i>(
        &mut self,
        layer: LayerId,
        input: &mut Parser<'i, '_>,
    ) -> Result<(), ParseError<'i, ()>> {
        if self.depth >= MAX_AT_RULE_DEPTH {
            return Err(input.new_error(BasicParseErrorKind::AtRuleBodyInvalid));
        }
        let mut nested = SheetParser {
            rules: &mut *self.rules,
            layers: &mut *self.layers,
            url_data: self.url_data,
            layer,
            depth: self.depth + 1,
        };
        for item in StyleSheetParser::new(input, &mut nested) {
            let _ = item;
        }
        Ok(())
    }
}

impl<'i, 'a> AtRuleParser<'i> for SheetParser<'a> {
    type Prelude = AtPrelude;
    type AtRule = ();
    type Error = ();

    fn parse_prelude<'t>(
        &mut self,
        name: CowRcStr<'i>,
        input: &mut Parser<'i, 't>,
    ) -> Result<Self::Prelude, ParseError<'i, ()>> {
        // Anything but `@layer` and `@media` ends at the prelude with `Err`: `StyleSheetParser` then skips the
        // block (or the statement) without calling `parse_block`.
        if name.eq_ignore_ascii_case("layer") {
            // An empty prelude is an anonymous layer (a block) or an error (a statement, see `rule_without_block`).
            let names = if input.is_exhausted() {
                Vec::new()
            } else {
                input.parse_comma_separated(|i| {
                    parse_layer_name(i)
                        .ok_or_else(|| i.new_error(BasicParseErrorKind::QualifiedRuleInvalid))
                })?
            };
            Ok(AtPrelude::Layer(names))
        } else if name.eq_ignore_ascii_case("media") {
            if media_query_list_matches(input) {
                Ok(AtPrelude::Media)
            } else {
                Err(input.new_error(BasicParseErrorKind::AtRuleInvalid(name)))
            }
        } else {
            Err(input.new_error(BasicParseErrorKind::AtRuleInvalid(name)))
        }
    }

    fn rule_without_block(
        &mut self,
        prelude: Self::Prelude,
        _start: &cssparser::ParserState,
    ) -> Result<Self::AtRule, ()> {
        // `@layer a, b.c;` names layers without rules, which fixes their order. A statement with no name is invalid.
        match prelude {
            AtPrelude::Layer(names) if !names.is_empty() => {
                for path in &names {
                    let mut at = self.layer;
                    for part in path {
                        at = self.layers.child(at, Some(part));
                    }
                }
                Ok(())
            }
            _ => Err(()),
        }
    }

    fn parse_block<'t>(
        &mut self,
        prelude: Self::Prelude,
        _start: &cssparser::ParserState,
        input: &mut Parser<'i, 't>,
    ) -> Result<Self::AtRule, ParseError<'i, ()>> {
        match prelude {
            AtPrelude::Layer(names) => {
                // A block takes at most one name: `@layer a, b { … }` is invalid.
                let layer = match names.as_slice() {
                    [] => self.layers.child(self.layer, None),
                    [path] => {
                        let mut at = self.layer;
                        for part in path {
                            at = self.layers.child(at, Some(part));
                        }
                        at
                    }
                    _ => return Err(input.new_error(BasicParseErrorKind::AtRuleBodyInvalid)),
                };
                self.parse_nested(layer, input)
            }
            AtPrelude::Media => self.parse_nested(self.layer, input),
        }
    }
}

impl<'i, 'a> QualifiedRuleParser<'i> for SheetParser<'a> {
    type Prelude = SelectorList<SelectorImpl>;
    type QualifiedRule = ();
    type Error = ();

    fn parse_prelude<'t>(
        &mut self,
        input: &mut Parser<'i, 't>,
    ) -> Result<Self::Prelude, ParseError<'i, ()>> {
        // Slice out the raw selector text and hand it to Stylo's selector parser, the one Blitz styles with. A list
        // with any invalid selector is invalid as a whole (Selectors 4 §3.1), and so is the rule.
        let start = input.position();
        while input.next_including_whitespace().is_ok() {}
        let raw = input.slice_from(start);
        SelectorParser::parse_author_origin_no_namespace(raw, self.url_data)
            .map_err(|_| input.new_error(BasicParseErrorKind::QualifiedRuleInvalid))
    }

    fn parse_block<'t>(
        &mut self,
        selectors: Self::Prelude,
        _start: &cssparser::ParserState,
        input: &mut Parser<'i, 't>,
    ) -> Result<Self::QualifiedRule, ParseError<'i, ()>> {
        let mut declared = Declared::default();
        let mut decl_parser = ColumnDeclParser {
            normal: &mut declared.normal,
            important: &mut declared.important,
        };
        let iter = RuleBodyParser::new(input, &mut decl_parser);
        for item in iter {
            let _ = item;
        }
        if !declared.normal.is_empty() || !declared.important.is_empty() {
            self.rules.push(StyleRule {
                selectors,
                props: declared.normal,
                important: declared.important,
                layer: self.layer,
                // Set by `parse_stylesheet` once the layer tree is complete.
                rank: 0,
            });
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Cascade driver
// ---------------------------------------------------------------------------

/// Build the per-document side-table: the stylesheet rules and the `style` attributes folded into each element's
/// entry by the cascade of CSS Cascade 5 §6.1 (see [`walk`]). Elements with no matching rule and no inline
/// declaration are skipped entirely.
pub(crate) fn build_column_style_table(
    doc: &blitz_html::HtmlDocument,
    stylesheet_rules: &[StyleRule],
) -> ColumnStyleTable {
    let mut table = ColumnStyleTable::new();
    let root = doc.root_element();
    let mut caches = SelectorCaches::default();
    walk(doc, root.id, stylesheet_rules, &mut caches, &mut table, 0);
    table
}

/// Recurse into `node_id`, folding matching rules into `table`.
///
/// Rules are matched with Stylo's own matcher (`selectors::matching`) against Blitz's `selectors::Element`
/// implementation, the one the engine's cascade uses: no bloom filter, quirks mode as Blitz builds its `Stylist`
/// (`NoQuirks`), links unvisited, no selector flags. Pseudo-classes of interactive state (`:hover`, `:focus`) are
/// false, and a selector with a pseudo-element matches no element.
fn walk(
    doc: &blitz_html::HtmlDocument,
    node_id: usize,
    rules: &[StyleRule],
    caches: &mut SelectorCaches,
    table: &mut ColumnStyleTable,
    depth: usize,
) {
    if depth >= crate::MAX_DOM_DEPTH {
        return;
    }
    let Some(node) = doc.get_node(node_id) else {
        return;
    };

    if node.element_data().is_some() {
        let mut props = ColumnStyleProps::default();
        // The rules that match, with what orders them. Specificity is that of the selector that matched: the
        // highest one of a selector list (Selectors 4 §17).
        let mut hits: Vec<(usize, u32, usize)> = Vec::new();
        let mut context = MatchingContext::new(
            MatchingMode::Normal,
            None,
            caches,
            QuirksMode::NoQuirks,
            NeedsSelectorFlags::No,
            MatchingForInvalidation::No,
        );
        for (index, rule) in rules.iter().enumerate() {
            let matched = rule
                .selectors
                .slice()
                .iter()
                .filter(|sel| matches_selector(sel, 0, None, &node, &mut context))
                .map(|sel| sel.specificity())
                .max();
            if let Some(spec) = matched {
                hits.push((rule.rank, spec, index));
            }
        }
        let inline = node
            .element_data()
            .and_then(|elem| crate::blitz_adapter::get_attr(elem, "style"))
            .map(parse_declarations)
            .unwrap_or_default();
        // Cascade 5 §6.1, lowest to highest: normal declarations by layer, then specificity, then order
        // (unlayered rules are the last layer); the `style` attribute's normal ones; important ones by layer in
        // *reverse* (§6.4.4: the unlayered ones lose, an earlier layer beats a later one), specificity, order; the
        // `style` attribute's important ones.
        hits.sort_by_key(|&(rank, spec, index)| (rank, spec, index));
        for &(_, _, index) in &hits {
            props.merge(rules[index].props.clone());
        }
        props.merge(inline.normal);
        hits.sort_by_key(|&(rank, spec, index)| (std::cmp::Reverse(rank), spec, index));
        for &(_, _, index) in &hits {
            props.merge(rules[index].important.clone());
        }
        props.merge(inline.important);
        if !props.is_empty() {
            table.insert(node_id, props);
        }
    }

    // Recurse in document order.
    for &child_id in &node.children {
        walk(doc, child_id, rules, caches, table, depth + 1);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -------- declaration parsing --------

    #[test]
    fn parses_column_rule_longhand_triplet() {
        let css = "column-rule-width: 2pt; column-rule-style: solid; column-rule-color: red;";
        let props = parse_declaration_block(css);
        let rule = props.rule.expect("rule");
        assert!((rule.width.to_f32() - 2.0).abs() < 1e-3);
        assert_eq!(rule.style, ColumnRuleStyle::Solid);
        assert_eq!(rule.color, [255, 0, 0, 255]);
    }

    #[test]
    fn parses_column_rule_shorthand() {
        let props = parse_declaration_block("column-rule: 1px dashed #0a0;");
        let rule = props.rule.expect("rule");
        assert!((rule.width.to_f32() - 0.75).abs() < 1e-2); // 1px → 0.75pt
        assert_eq!(rule.style, ColumnRuleStyle::Dashed);
        assert_eq!(rule.color, [0x00, 0xAA, 0x00, 0xFF]);
    }

    #[test]
    fn parses_column_rule_shorthand_any_order() {
        let props = parse_declaration_block("column-rule: red 3pt dotted;");
        let rule = props.rule.expect("rule");
        assert!((rule.width.to_f32() - 3.0).abs() < 1e-3);
        assert_eq!(rule.style, ColumnRuleStyle::Dotted);
        assert_eq!(rule.color, [255, 0, 0, 255]);
    }

    #[test]
    fn column_rule_shorthand_partial_uses_defaults() {
        let props = parse_declaration_block("column-rule: dashed;");
        let rule = props.rule.expect("rule");
        assert_eq!(rule.style, ColumnRuleStyle::Dashed);
        // Defaults: 1pt, black.
        assert!((rule.width.to_f32() - 1.0).abs() < 1e-3);
        assert_eq!(rule.color, [0, 0, 0, 255]);
    }

    #[test]
    fn parses_column_fill_auto() {
        let props = parse_declaration_block("column-fill: auto;");
        assert_eq!(props.fill, Some(ColumnFill::Auto));
    }

    #[test]
    fn parses_column_fill_balance() {
        let props = parse_declaration_block("column-fill: balance;");
        assert_eq!(props.fill, Some(ColumnFill::Balance));
    }

    // -------- break-inside (fulgur-ftp) --------

    #[test]
    fn parse_break_inside_avoid_inline() {
        let props = parse_declaration_block("break-inside: avoid;");
        assert_eq!(props.break_inside, Some(BreakInside::Avoid));
    }

    #[test]
    fn parse_break_inside_avoid_page_and_column_collapse_to_avoid() {
        let p1 = parse_declaration_block("break-inside: avoid-page;");
        let p2 = parse_declaration_block("break-inside: avoid-column;");
        assert_eq!(p1.break_inside, Some(BreakInside::Avoid));
        assert_eq!(p2.break_inside, Some(BreakInside::Avoid));
    }

    #[test]
    fn parse_break_inside_auto_is_auto_variant() {
        let props = parse_declaration_block("break-inside: auto;");
        assert_eq!(props.break_inside, Some(BreakInside::Auto));
    }

    #[test]
    fn parse_break_inside_invalid_value_is_silently_dropped() {
        let props = parse_declaration_block("break-inside: banana;");
        assert_eq!(props.break_inside, None);
    }

    #[test]
    fn parse_break_inside_via_selector() {
        let rules = parse_stylesheet(".keep { break-inside: avoid; }");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].props.break_inside, Some(BreakInside::Avoid));
    }

    #[test]
    fn ignores_unsupported_rule_style() {
        // `double` is Phase C — the whole shorthand must drop.
        let props = parse_declaration_block("column-rule: 1pt double red;");
        assert!(props.rule.is_none());
    }

    #[test]
    fn px_length_converts_to_pt() {
        let props = parse_declaration_block("column-rule-width: 4px;");
        let w = props.rule.expect("rule").width.to_f32();
        assert!((w - 3.0).abs() < 1e-3); // 4px * 72/96 = 3pt
    }

    #[test]
    fn em_length_resolves_against_default_basis() {
        // `1em` at the 16px fallback basis == 12pt.
        let props = parse_declaration_block("column-rule-width: 1em;");
        let rule = props.rule.expect("rule");
        assert!(
            (rule.width.to_f32() - 12.0).abs() < 1e-3,
            "1em expected 12pt (16px × 72/96), got {}",
            rule.width.to_f32()
        );
    }

    #[test]
    fn rem_length_resolves_against_default_basis() {
        // `0.25rem` == 4px at the 16px basis == 3pt.
        let props = parse_declaration_block("column-rule-width: 0.25rem;");
        let rule = props.rule.expect("rule");
        assert!(
            (rule.width.to_f32() - 3.0).abs() < 1e-3,
            "0.25rem expected 3pt (4px × 72/96), got {}",
            rule.width.to_f32()
        );
    }

    #[test]
    fn em_shorthand_resolves_against_default_basis() {
        let props = parse_declaration_block("column-rule: 0.1em solid red;");
        let rule = props.rule.expect("rule");
        // 0.1em × 16 px × 72/96 = 1.2pt
        assert!(
            (rule.width.to_f32() - 1.2).abs() < 1e-3,
            "0.1em expected 1.2pt, got {}",
            rule.width.to_f32()
        );
        assert_eq!(rule.style, ColumnRuleStyle::Solid);
        assert_eq!(rule.color, [255, 0, 0, 255]);
    }

    #[test]
    fn percent_length_still_drops() {
        // `%` requires a containing-block basis we don't have here.
        let props = parse_declaration_block("column-rule-width: 50%;");
        assert!(props.rule.is_none());
    }

    #[test]
    fn longhands_compose_across_declarations() {
        let css = "column-rule-width: 5pt; column-rule-color: blue;";
        let props = parse_declaration_block(css);
        let rule = props.rule.expect("rule");
        assert!((rule.width.to_f32() - 5.0).abs() < 1e-3);
        assert_eq!(rule.color, [0, 0, 255, 255]);
        // No `column-rule-style` — default `None` applies.
        assert_eq!(rule.style, ColumnRuleStyle::None);
    }

    #[test]
    fn invalid_declaration_does_not_poison_siblings() {
        // `1vh` has no containing-block basis at parse time → drops the
        // width declaration. Sibling `column-fill: auto` must still apply.
        let css = "column-rule-width: 1vh; column-fill: auto;";
        let props = parse_declaration_block(css);
        assert!(props.rule.is_none());
        assert_eq!(props.fill, Some(ColumnFill::Auto));
    }

    #[test]
    fn currentcolor_drops_color_longhand() {
        let props = parse_declaration_block("column-rule-color: currentcolor;");
        // cssparser doesn't know `currentcolor` as a named colour; we
        // return `None` which drops this declaration silently.
        assert!(props.rule.is_none());
    }

    #[test]
    fn parses_rgb_functional_notation() {
        let props = parse_declaration_block("column-rule-color: rgb(255, 0, 0);");
        assert_eq!(props.rule.expect("rule").color, [255, 0, 0, 255]);
    }

    #[test]
    fn parses_rgba_with_alpha() {
        let props = parse_declaration_block("column-rule-color: rgba(0, 0, 0, 0.5);");
        let c = props.rule.expect("rule").color;
        assert_eq!(&c[0..3], &[0, 0, 0]);
        // Allow ±1 rounding slack.
        assert!((c[3] as i32 - 128).abs() <= 1, "alpha was {}", c[3]);
    }

    #[test]
    fn parses_hsl_notation() {
        let props = parse_declaration_block("column-rule-color: hsl(0, 100%, 50%);");
        let c = props.rule.expect("rule").color;
        // pure red; allow ±2 slack on rounding
        assert!(c[0] >= 253 && c[1] <= 2 && c[2] <= 2, "got {:?}", c);
    }

    #[test]
    fn currentcolor_is_rejected() {
        let props = parse_declaration_block("column-rule-color: currentcolor;");
        assert!(props.rule.is_none() || props.rule.unwrap().style == ColumnRuleStyle::None);
    }

    // -------- selector parsing --------

    // -------- end-to-end matcher (via blitz_adapter::parse) --------

    fn build_doc(html: &str) -> blitz_html::HtmlDocument {
        crate::blitz_adapter::parse(html, 400.0, &[])
    }

    fn find_node_with<F>(doc: &blitz_html::HtmlDocument, pred: F) -> Option<usize>
    where
        F: Fn(&blitz_dom::Node) -> bool + Copy,
    {
        let root = doc.root_element();
        fn rec<F: Fn(&blitz_dom::Node) -> bool + Copy>(
            doc: &blitz_html::HtmlDocument,
            id: usize,
            depth: usize,
            pred: F,
        ) -> Option<usize> {
            if depth > 64 {
                return None;
            }
            let node = doc.get_node(id)?;
            if pred(node) {
                return Some(id);
            }
            for &c in &node.children {
                if let Some(r) = rec(doc, c, depth + 1, pred) {
                    return Some(r);
                }
            }
            None
        }
        rec(doc, root.id, 0, pred)
    }

    /// Collect every node id (in document order) whose node satisfies `pred`.
    fn collect_in_document_order<F>(doc: &blitz_html::HtmlDocument, pred: F) -> Vec<usize>
    where
        F: Fn(&blitz_dom::Node) -> bool + Copy,
    {
        fn rec<F: Fn(&blitz_dom::Node) -> bool + Copy>(
            doc: &blitz_html::HtmlDocument,
            id: usize,
            pred: F,
            out: &mut Vec<usize>,
        ) {
            let Some(node) = doc.get_node(id) else {
                return;
            };
            if pred(node) {
                out.push(id);
            }
            for &c in &node.children {
                rec(doc, c, pred, out);
            }
        }
        let mut out = Vec::new();
        rec(doc, doc.root_element().id, pred, &mut out);
        out
    }

    // -------- stylesheet parsing --------

    #[test]
    fn parses_stylesheet_with_multiple_rules() {
        let css = r#"
            .mc { column-rule: 1pt solid red; }
            #x { column-fill: auto; }
        "#;
        let rules = parse_stylesheet(css);
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].selectors.slice().len(), 1);
        assert!(rules[0].props.rule.is_some());
        assert_eq!(rules[1].props.fill, Some(ColumnFill::Auto));
    }

    #[test]
    fn stylesheet_skips_unmatched_at_rules() {
        // `@media print` does not match the device (`READER_MEDIA_TYPE` is `screen`; the embedder swaps the two),
        // `@supports` is never evaluated: only the bare `.mc` rule remains.
        let css = r#"
            @media print { .mc { column-rule: 1pt solid red; } }
            @supports (display: grid) { .mc { column-rule: 1pt solid red; } }
            .mc { column-fill: auto; }
        "#;
        let rules = parse_stylesheet(css);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].props.fill, Some(ColumnFill::Auto));
        assert_eq!(rules[0].layer, 0);
    }

    #[test]
    fn stylesheet_reads_matching_media_and_layers_in_cascade_order() {
        // The rule's `column-rule-width` (in pt) names it.
        let css = r#"
            .z { column-rule-width: 1pt; }
            @layer late { .y { column-rule-width: 2pt; } }
            @media screen { .x { column-rule-width: 3pt; } }
            @layer early { .w { column-rule-width: 4pt; } }
        "#;
        let rules = parse_stylesheet(css);
        let order: Vec<f32> = rules
            .iter()
            .map(|r| r.props.rule.expect("rule").width.to_f32())
            .collect();
        // Layers by first appearance (`late` < `early`), then unlayered rules (`.z`, `.x`) in source order.
        assert_eq!(order, [2.0, 4.0, 1.0, 3.0]);
    }

    #[test]
    fn layer_ranks_are_post_order() {
        let mut t = LayerTree::new();
        let a = t.child(0, Some("a"));
        let ax = t.child(a, Some("x"));
        let b = t.child(0, Some("b"));
        let anon1 = t.child(0, None);
        let anon2 = t.child(0, None);
        assert_ne!(anon1, anon2);
        assert_eq!(t.child(0, Some("a")), a, "a name is the same layer");
        assert_eq!(t.child(a, Some("x")), ax);
        let r = t.ranks();
        // a.x < a < b < anon1 < anon2 < root.
        let order = [ax, a, b, anon1, anon2, 0];
        assert!(order.windows(2).all(|w| r[w[0]] < r[w[1]]), "{r:?}");
        // The root is last, however many layers there are.
        assert_eq!(r[0], r.len() - 1);
    }

    #[test]
    fn stylesheet_drops_rule_with_empty_properties() {
        // No column-* property — should not be returned.
        let css = ".mc { color: red; }";
        let rules = parse_stylesheet(css);
        assert!(rules.is_empty());
    }

    // -------- cascade --------

    #[test]
    fn inline_style_beats_stylesheet() {
        let html = r#"<html><head><style>
            .mc { column-rule: 1pt solid blue; }
        </style></head><body>
            <div class="mc" id="a"></div>
            <div class="mc" id="b" style="column-rule: 2pt dashed red"></div>
        </body></html>"#;
        let doc = build_doc(html);

        // Extract the <style> content manually so this test doesn't depend
        // on Task 2's harvester.
        let rules = parse_stylesheet(".mc { column-rule: 1pt solid blue; }");
        let table = build_column_style_table(&doc, &rules);

        let a_id = find_node_with(&doc, |n| {
            n.element_data()
                .and_then(|e| crate::blitz_adapter::get_attr(e, "id"))
                == Some("a")
        })
        .expect("a");
        let b_id = find_node_with(&doc, |n| {
            n.element_data()
                .and_then(|e| crate::blitz_adapter::get_attr(e, "id"))
                == Some("b")
        })
        .expect("b");

        let a = table.get(&a_id).expect("a entry");
        let a_rule = a.rule.expect("a rule");
        assert_eq!(a_rule.color, [0, 0, 255, 255]);
        assert_eq!(a_rule.style, ColumnRuleStyle::Solid);

        let b = table.get(&b_id).expect("b entry");
        let b_rule = b.rule.expect("b rule");
        assert!((b_rule.width.to_f32() - 2.0).abs() < 1e-3);
        assert_eq!(b_rule.style, ColumnRuleStyle::Dashed);
        assert_eq!(b_rule.color, [255, 0, 0, 255]);
    }

    #[test]
    fn later_stylesheet_rule_wins_on_conflict() {
        let css = r#"
            .mc { column-fill: balance; }
            .mc { column-fill: auto; }
        "#;
        let rules = parse_stylesheet(css);
        let html = r#"<html><body><div class="mc" id="a"></div></body></html>"#;
        let doc = build_doc(html);
        let table = build_column_style_table(&doc, &rules);
        let a_id = find_node_with(&doc, |n| {
            n.element_data()
                .and_then(|e| crate::blitz_adapter::get_attr(e, "id"))
                == Some("a")
        })
        .expect("a");
        assert_eq!(table.get(&a_id).expect("a").fill, Some(ColumnFill::Auto));
    }

    #[test]
    fn nodes_without_matching_rule_are_absent_from_table() {
        let css = ".mc { column-fill: auto; }";
        let rules = parse_stylesheet(css);
        let html = r#"<html><body><div id="nope"></div></body></html>"#;
        let doc = build_doc(html);
        let table = build_column_style_table(&doc, &rules);
        // Nothing matches `.mc`, so the table should be empty.
        assert!(table.is_empty());
    }

    /// Regression guard for the linear-time adjacent-sibling cascade.
    ///
    /// A flat document of many `<div>` siblings with `* + * { column-fill:
    /// auto }` must assign the property to exactly the elements that have a
    /// preceding element sibling — every div except the first — and to no
    /// other div. The pre-refactor matcher recomputed each node's position
    /// inside its parent's child list on every rule check (`siblings.iter()
    /// .position(...)`), making a flat sibling list O(n^2); the cascade walk
    /// now threads the previous element sibling in O(1). This pins the
    /// *result set* so the linear rewrite cannot silently change which nodes
    /// match. Timing is deliberately not asserted — the complexity guarantee
    /// is structural (there is no longer a per-node sibling scan), and a
    /// wall-clock threshold would be flaky.
    #[test]
    fn adjacent_sibling_cascade_matches_every_element_after_the_first() {
        const N: usize = 400;
        let mut body = String::with_capacity(N * "<div></div>".len());
        for _ in 0..N {
            body.push_str("<div></div>");
        }
        let html = format!("<html><body>{body}</body></html>");
        let doc = build_doc(&html);

        let rules = parse_stylesheet("* + * { column-fill: auto; }");
        let table = build_column_style_table(&doc, &rules);

        let divs = collect_in_document_order(&doc, |n| {
            n.element_data()
                .is_some_and(|e| e.name.local.as_ref() == "div")
        });
        assert_eq!(divs.len(), N, "all {N} divs must be present in the DOM");

        // The first div has no preceding element sibling → no match.
        assert!(
            !table.contains_key(&divs[0]),
            "first div must not match `* + *`"
        );
        // Every subsequent div has a preceding element sibling → matches.
        for (i, id) in divs.iter().enumerate().skip(1) {
            let props = table
                .get(id)
                .unwrap_or_else(|| panic!("div #{i} missing from column style table"));
            assert_eq!(
                props.fill,
                Some(ColumnFill::Auto),
                "div #{i} should match `* + *`"
            );
        }
    }

    /// End-to-end guard for the threaded text-skip in the cascade walk.
    ///
    /// `walk` advances its previous-element-sibling marker only on element
    /// children, so text and comment nodes between two elements must not break
    /// an `E + F` match. The unit test `matches_complex_adjacent_sibling_skips
    /// _text_nodes` covers this only through the test-only backward scan; this
    /// exercises the *production* threading (`build_column_style_table`) with
    /// both a text node and a comment interspersed among the siblings.
    #[test]
    fn adjacent_sibling_cascade_skips_text_and_comment_siblings() {
        let html = r#"<html><body>
            <p id="a"></p>
            some text
            <p id="b"></p>
            <!-- a comment -->
            <p id="c"></p>
        </body></html>"#;
        let doc = build_doc(html);

        let rules = parse_stylesheet("p + p { column-fill: auto; }");
        let table = build_column_style_table(&doc, &rules);

        let id_of = |want: &str| {
            find_node_with(&doc, |n| {
                n.element_data()
                    .and_then(|e| crate::blitz_adapter::get_attr(e, "id"))
                    == Some(want)
            })
            .unwrap_or_else(|| panic!("#{want}"))
        };

        // #a has no preceding element sibling → no match.
        assert!(
            !table.contains_key(&id_of("a")),
            "#a must not match `p + p`"
        );
        // #b's previous element sibling is #a across the text node → match.
        assert_eq!(
            table.get(&id_of("b")).and_then(|p| p.fill),
            Some(ColumnFill::Auto),
            "#b must match `p + p` across the text node"
        );
        // #c's previous element sibling is #b across the comment node → match.
        assert_eq!(
            table.get(&id_of("c")).and_then(|p| p.fill),
            Some(ColumnFill::Auto),
            "#c must match `p + p` across the comment node"
        );
    }

    // -------- inline-style helpers used by break-after / break-before tests ----

    fn parse_inline_style(css: &str) -> ColumnStyleProps {
        parse_declaration_block(css)
    }

    // -------- break-after / page-break-after --------

    #[test]
    fn parse_break_after_always_inline() {
        // `always` is a value of the legacy `page-break-after` only (css-break-3 §3.4; F87 review).
        let props = parse_inline_style("break-after: always");
        assert_eq!(props.break_after, None);
    }

    #[test]
    fn parse_page_break_after_always_inline() {
        let props = parse_inline_style("page-break-after: always");
        assert_eq!(props.break_after, Some(BreakAfter::Page));
    }

    #[test]
    fn parse_break_before_always_inline() {
        let props = parse_inline_style("break-before: always");
        assert_eq!(props.break_before, None);
    }

    #[test]
    fn parse_page_break_before_always_inline() {
        let props = parse_inline_style("page-break-before: always");
        assert_eq!(props.break_before, Some(BreakBefore::Page));
    }

    #[test]
    fn parse_break_after_auto_is_auto() {
        let props = parse_inline_style("break-after: auto");
        assert_eq!(props.break_after, Some(BreakAfter::Auto));
    }

    #[test]
    fn parse_break_after_invalid_is_silently_dropped() {
        let props = parse_inline_style("break-after: banana");
        assert_eq!(props.break_after, None);
    }

    #[test]
    fn parse_break_after_via_selector() {
        let rules = parse_stylesheet(".forced { break-after: page; }");
        assert_eq!(rules[0].props.break_after, Some(BreakAfter::Page));
    }

    // -------- break-before / page-break-before --------

    #[test]
    fn parse_break_before_via_selector() {
        let rules = parse_stylesheet(".forced { break-before: page; }");
        assert_eq!(rules[0].props.break_before, Some(BreakBefore::Page));
    }

    #[test]
    fn parse_break_after_left_and_right_collapse_to_page() {
        let p1 = parse_inline_style("break-after: left");
        let p2 = parse_inline_style("break-after: right");
        assert_eq!(p1.break_after, Some(BreakAfter::Page));
        assert_eq!(p2.break_after, Some(BreakAfter::Page));
    }

    #[test]
    fn comma_selector_list_matches_any_compound() {
        let css = "div, #a { column-fill: auto; }";
        let rules = parse_stylesheet(css);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].selectors.len(), 2);

        let html = r#"<html><body>
            <div></div>
            <p id="a"></p>
            <p id="b"></p>
        </body></html>"#;
        let doc = build_doc(html);
        let table = build_column_style_table(&doc, &rules);

        let div_id = find_node_with(&doc, |n| {
            n.element_data()
                .is_some_and(|e| e.name.local.as_ref() == "div")
        })
        .expect("div");
        let a_id = find_node_with(&doc, |n| {
            n.element_data()
                .and_then(|e| crate::blitz_adapter::get_attr(e, "id"))
                == Some("a")
        })
        .expect("a");
        let b_id = find_node_with(&doc, |n| {
            n.element_data()
                .and_then(|e| crate::blitz_adapter::get_attr(e, "id"))
                == Some("b")
        })
        .expect("b");

        assert_eq!(table.get(&div_id).unwrap().fill, Some(ColumnFill::Auto));
        assert_eq!(table.get(&a_id).unwrap().fill, Some(ColumnFill::Auto));
        assert!(!table.contains_key(&b_id));
    }

    // -------- matches_complex DOM behaviour --------

    // -------- break-before --------

    #[test]
    fn parses_break_before_page() {
        let props = parse_declaration_block("break-before: page;");
        assert_eq!(
            props.break_before,
            Some(crate::draw_primitives::BreakBefore::Page)
        );
    }

    #[test]
    fn parses_break_before_always() {
        // Not a value of `break-before` (F87 review); `page-break-before: always` is.
        let props = parse_declaration_block("break-before: always;");
        assert_eq!(props.break_before, None);
        let props = parse_declaration_block("page-break-before: always;");
        assert_eq!(
            props.break_before,
            Some(crate::draw_primitives::BreakBefore::Page)
        );
    }

    #[test]
    fn parses_break_before_auto() {
        let props = parse_declaration_block("break-before: auto;");
        assert_eq!(
            props.break_before,
            Some(crate::draw_primitives::BreakBefore::Auto)
        );
    }

    #[test]
    fn break_before_propagates_to_pagination() {
        // Test that the stylesheet parser correctly captures break_before.
        let rules = parse_stylesheet("div { break-before: page; }");
        assert_eq!(rules.len(), 1);
        assert_eq!(
            rules[0].props.break_before,
            Some(crate::draw_primitives::BreakBefore::Page)
        );
    }

    // -------- page (CSS Page 3 §5.3, fulgur-uebl) --------

    #[test]
    fn parses_page_named_ident() {
        let props = parse_declaration_block("page: chapter;");
        assert_eq!(props.page, Some(PageName::Named("chapter".into())));
    }

    #[test]
    fn parses_page_auto() {
        let props = parse_declaration_block("page: auto;");
        assert_eq!(props.page, Some(PageName::Auto));
    }

    #[test]
    fn page_name_length_is_defensively_capped() {
        // Security regression: a CSS `<custom-ident>` is length-unbounded and
        // the retained name is cloned into the column style table and once per
        // node while walking used page names (O(nodes × name_len)). A very
        // long name is truncated at its single construction site so retention
        // stays bounded; realistic page names are far shorter and unaffected.
        let long = "a".repeat(10_000);
        let props = parse_declaration_block(&format!("page: {long};"));
        match props.page {
            Some(PageName::Named(s)) => assert!(
                s.len() <= crate::MAX_PAGE_NAME_BYTES,
                "page name must be capped, got {}",
                s.len()
            ),
            other => panic!("expected Named page, got {other:?}"),
        }
    }

    #[test]
    fn page_is_case_sensitive_for_named_idents() {
        // CSS custom-idents are case-sensitive — `Chapter` and `chapter`
        // are distinct page names.
        let lower = parse_declaration_block("page: chapter;");
        let upper = parse_declaration_block("page: Chapter;");
        assert_ne!(lower.page, upper.page);
    }

    #[test]
    fn page_auto_keyword_is_case_insensitive() {
        // CSS keywords are ASCII case-insensitive — `AUTO` resolves to
        // the same `PageName::Auto` as `auto`.
        let lower = parse_declaration_block("page: auto;");
        let upper = parse_declaration_block("page: AUTO;");
        assert_eq!(lower.page, upper.page);
    }

    #[test]
    fn page_rejects_css_wide_keywords() {
        // `inherit` / `initial` / `unset` / `revert` / `revert-layer`
        // should drop the
        // declaration entirely — we don't model the cascade richness
        // they imply, and accepting them as named pages would silently
        // create pages named "inherit" etc.
        for kw in ["inherit", "initial", "unset", "revert", "revert-layer"] {
            let css = format!("page: {kw};");
            let props = parse_declaration_block(&css);
            assert_eq!(props.page, None, "{kw} should be rejected");
        }
    }

    #[test]
    fn page_rejects_invalid_value_types() {
        // Numbers, percentages, strings — none of these are valid
        // <custom-ident> grammar, so the declaration drops silently
        // (matching the project-wide "no panic on bad CSS" invariant).
        for v in ["123", "10%", "\"chapter\""] {
            let css = format!("page: {v};");
            let props = parse_declaration_block(&css);
            assert_eq!(props.page, None, "{v} should be rejected");
        }
    }

    #[test]
    fn page_in_stylesheet_via_selector() {
        let rules = parse_stylesheet(".chapter { page: chapter; }");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].props.page, Some(PageName::Named("chapter".into())));
    }

    #[test]
    fn page_in_inline_style_overrides_stylesheet() {
        // Cascade rule: inline style applies last, so it beats a
        // matching stylesheet rule — same as for `column-rule-*` and
        // `break-*` props.
        let mut a = parse_declaration_block("page: a;");
        let b = parse_declaration_block("page: b;");
        a.merge(b);
        assert_eq!(a.page, Some(PageName::Named("b".into())));
    }

    // -------- parse_length: zero without a dimension unit --------

    #[test]
    fn parse_length_zero_without_unit() {
        // CSS allows `0` as a valid `<length>` with no unit. The `parse_length`
        // function matches `Token::Number { value: 0.0, .. }` for this case.
        let props = parse_declaration_block("column-rule-width: 0;");
        let rule = props.rule.expect("rule");
        assert_eq!(
            rule.width.to_f32(),
            0.0,
            "zero length must round-trip as 0.0pt"
        );
    }

    // -------- parse_color: transparent keyword --------

    #[test]
    fn parse_color_transparent_keyword() {
        // `transparent` resolves to [0, 0, 0, 0] — the `Token::Ident` arm in
        // `parse_color` handles this before falling through to `parse_named_color`.
        let props = parse_declaration_block("column-rule-color: transparent;");
        let color = props.rule.expect("rule").color;
        assert_eq!(
            color,
            [0, 0, 0, 0],
            "transparent must be fully-transparent black"
        );
    }

    // -------- parse_rgb_channel: percentage variant --------

    #[test]
    fn parse_rgb_channel_as_percentage() {
        // `rgb(r%, g%, b%)` exercises `Token::Percentage` in `parse_rgb_channel`.
        // 50% × 255 ≈ 128, 25% × 255 ≈ 64, 75% × 255 ≈ 191.
        let props = parse_declaration_block("column-rule-color: rgb(50%, 25%, 75%);");
        let color = props.rule.expect("rule").color;
        assert!((color[0] as i32 - 128).abs() <= 1, "R: {}", color[0]);
        assert!((color[1] as i32 - 64).abs() <= 1, "G: {}", color[1]);
        assert!((color[2] as i32 - 191).abs() <= 1, "B: {}", color[2]);
        assert_eq!(color[3], 255, "alpha must be fully opaque");
    }

    // -------- parse_alpha_value: percentage variant --------

    #[test]
    fn parse_alpha_as_percentage() {
        // `rgba(r, g, b, p%)` exercises `Token::Percentage` in `parse_alpha_value`.
        // 50% × 255 ≈ 128.
        let props = parse_declaration_block("column-rule-color: rgba(0, 0, 0, 50%);");
        let color = props.rule.expect("rule").color;
        assert_eq!(&color[..3], &[0, 0, 0]);
        assert!((color[3] as i32 - 128).abs() <= 1, "alpha: {}", color[3]);
    }

    // -------- hsl_to_rgb: achromatic path (s == 0) --------

    #[test]
    fn hsl_to_rgb_achromatic_returns_gray() {
        // When saturation is 0 the achromatic branch `(l, l, l)` is taken.
        // hsl(0, 0%, 50%) → neutral gray ≈ [128, 128, 128].
        let props = parse_declaration_block("column-rule-color: hsl(0, 0%, 50%);");
        let color = props.rule.expect("rule").color;
        assert!((color[0] as i32 - 128).abs() <= 1, "R: {}", color[0]);
        assert!((color[1] as i32 - 128).abs() <= 1, "G: {}", color[1]);
        assert!((color[2] as i32 - 128).abs() <= 1, "B: {}", color[2]);
        assert_eq!(color[3], 255);
    }

    // -------- hsl_to_rgb: l < 0.5 branch --------

    #[test]
    fn hsl_to_rgb_dark_l_below_half() {
        // l < 0.5 selects `q = l * (1 + s)` instead of `l + s - l*s`.
        // hsl(120, 100%, 25%) → dark green ≈ [0, 128, 0].
        let props = parse_declaration_block("column-rule-color: hsl(120, 100%, 25%);");
        let color = props.rule.expect("rule").color;
        assert!((color[0] as i32).abs() <= 2, "R: {}", color[0]);
        assert!((color[1] as i32 - 128).abs() <= 2, "G: {}", color[1]);
        assert!((color[2] as i32).abs() <= 2, "B: {}", color[2]);
        assert_eq!(color[3], 255);
    }

    // -------- hsl_to_rgb: t > 1.0 wrap in hue_to_rgb closure --------

    #[test]
    fn hsl_to_rgb_hue_wrap_above_one() {
        // For hsl(300, 100%, 50%) the red channel's intermediate t = h + 1/3 = 7/6 > 1.0
        // and must wrap down by 1 before the piecewise interpolation. Result is magenta.
        let props = parse_declaration_block("column-rule-color: hsl(300, 100%, 50%);");
        let color = props.rule.expect("rule").color;
        // Magenta: R=255, G=0, B=255.
        assert!((color[0] as i32 - 255).abs() <= 2, "R: {}", color[0]);
        assert!((color[1] as i32).abs() <= 2, "G: {}", color[1]);
        assert!((color[2] as i32 - 255).abs() <= 2, "B: {}", color[2]);
        assert_eq!(color[3], 255);
    }

    // -------- parse_hsl_args: alpha branch --------

    #[test]
    fn parse_hsla_with_number_alpha() {
        // `hsla(h, s%, l%, a)` exercises the alpha branch in `parse_hsl_args`.
        // hsl(120, 100%, 50%) = pure green; alpha 0.5 ≈ 128.
        let props = parse_declaration_block("column-rule-color: hsla(120, 100%, 50%, 0.5);");
        let color = props.rule.expect("rule").color;
        assert!((color[0] as i32).abs() <= 2, "R: {}", color[0]);
        assert!((color[1] as i32 - 255).abs() <= 2, "G: {}", color[1]);
        assert!((color[2] as i32).abs() <= 2, "B: {}", color[2]);
        assert!((color[3] as i32 - 128).abs() <= 1, "alpha: {}", color[3]);
    }

    #[test]
    fn parse_hsla_with_percentage_alpha() {
        // `hsla(h, s%, l%, p%)` — `parse_alpha_value` receives a percentage token.
        // 50% × 255 ≈ 128.
        let props = parse_declaration_block("column-rule-color: hsla(120, 100%, 50%, 50%);");
        let color = props.rule.expect("rule").color;
        assert!((color[0] as i32).abs() <= 2, "R: {}", color[0]);
        assert!((color[1] as i32 - 255).abs() <= 2, "G: {}", color[1]);
        assert!((color[2] as i32).abs() <= 2, "B: {}", color[2]);
        assert!((color[3] as i32 - 128).abs() <= 1, "alpha: {}", color[3]);
    }

    // -------- selector parser edge cases --------

    // -------- cascade layers and conditional rules (F87) --------

    /// `break-inside` the cascade gives the element `id="k"` of `<p id="k" class="k">` for the stylesheet `css`.
    fn break_inside_of(css: &str, inline: &str) -> Option<crate::draw_primitives::BreakInside> {
        let html =
            format!(r#"<html><body><p id="k" class="k" style="{inline}">x</p></body></html>"#);
        let doc = build_doc(&html);
        let table = build_column_style_table(&doc, &parse_stylesheet(css));
        let id = find_node_with(&doc, |n| {
            n.element_data()
                .and_then(|e| crate::blitz_adapter::get_attr(e, "id"))
                == Some("k")
        })
        .expect("k");
        table.get(&id).and_then(|p| p.break_inside)
    }

    use crate::draw_primitives::BreakInside::{Auto, Avoid};

    #[test]
    fn a_rule_inside_a_layer_is_read() {
        assert_eq!(
            break_inside_of("@layer utilities{.k{break-inside:avoid}}", ""),
            Some(Avoid)
        );
    }

    #[test]
    fn unlayered_rule_beats_a_layer_whatever_the_order() {
        // Case A. Cascade 5 §6.4: unlayered styles form an implicit final layer.
        let layered = "@layer utilities{.k{break-inside:avoid}}";
        let unlayered = "p{break-inside:auto}";
        assert_eq!(
            break_inside_of(&format!("{unlayered}{layered}"), ""),
            Some(Auto)
        );
        assert_eq!(
            break_inside_of(&format!("{layered}{unlayered}"), ""),
            Some(Auto)
        );
    }

    #[test]
    fn an_earlier_layer_loses_to_a_later_one_whatever_the_source_order() {
        // Case B: `base` is declared before `utilities` by the statement, so it loses although its rule comes last.
        assert_eq!(
            break_inside_of(
                "@layer base, utilities;@layer utilities{.k{break-inside:avoid}}@layer base{p{break-inside:auto}}",
                ""
            ),
            Some(Avoid)
        );
        // Case C: `overrides` is declared after `utilities` and wins although its rule comes first.
        assert_eq!(
            break_inside_of(
                "@layer utilities, overrides;@layer overrides{.k{break-inside:auto}}@layer utilities{.k{break-inside:avoid}}",
                ""
            ),
            Some(Auto)
        );
    }

    #[test]
    fn a_layer_beats_its_own_sublayers() {
        // Case D. Cascade 5 §6.4: a layer's own rules are an implicit final sub-layer.
        assert_eq!(
            break_inside_of(
                "@layer a{.k{break-inside:auto}@layer b{.k{break-inside:avoid}}}",
                ""
            ),
            Some(Auto)
        );
        assert_eq!(
            break_inside_of(
                "@layer a{@layer b{.k{break-inside:avoid}}.k{break-inside:auto}}",
                ""
            ),
            Some(Auto)
        );
    }

    #[test]
    fn a_dotted_layer_name_is_the_nested_layer() {
        let dotted = "@layer a.b{.k{break-inside:avoid}}@layer a{.k{break-inside:auto}}";
        let nested = "@layer a{@layer b{.k{break-inside:avoid}}}@layer a{.k{break-inside:auto}}";
        assert_eq!(break_inside_of(dotted, ""), Some(Auto));
        assert_eq!(break_inside_of(nested, ""), Some(Auto));
        // `a.b` after `a.c` ranks above it; both below `a`'s own rules.
        assert_eq!(
            break_inside_of(
                "@layer a.c{.k{break-inside:auto}}@layer a.b{.k{break-inside:avoid}}",
                ""
            ),
            Some(Avoid)
        );
    }

    #[test]
    fn anonymous_layers_are_distinct_and_ordered() {
        assert_eq!(
            break_inside_of(
                "@layer{.k{break-inside:avoid}}@layer{.k{break-inside:auto}}",
                ""
            ),
            Some(Auto)
        );
        assert_eq!(
            break_inside_of(
                "@layer{.k{break-inside:auto}}@layer{.k{break-inside:avoid}}",
                ""
            ),
            Some(Avoid)
        );
    }

    #[test]
    fn a_layer_statement_fixes_the_order_without_rules() {
        let rules = "@layer a{.k{break-inside:auto}}@layer b{.k{break-inside:avoid}}";
        assert_eq!(break_inside_of(rules, ""), Some(Avoid));
        assert_eq!(
            break_inside_of(&format!("@layer b, a;{rules}"), ""),
            Some(Auto)
        );
    }

    #[test]
    fn the_style_attribute_beats_layered_and_unlayered_rules() {
        assert_eq!(
            break_inside_of("@layer u{.k{break-inside:avoid}}", "break-inside:auto"),
            Some(Auto)
        );
        assert_eq!(
            break_inside_of(".k{break-inside:avoid}", "break-inside:auto"),
            Some(Auto)
        );
        assert_eq!(
            break_inside_of("@layer u{.k{break-inside:auto}}", "break-inside:avoid"),
            Some(Avoid)
        );
    }

    #[test]
    fn media_blocks_are_read_when_the_query_matches_the_device() {
        // The device is a screen (Blitz `make_device`, see `READER_MEDIA_TYPE`).
        let read = |q: &str| {
            break_inside_of(&format!("@media {q}{{.k{{break-inside:avoid}}}}"), "") == Some(Avoid)
        };
        assert!(read("screen"));
        assert!(read("all"));
        assert!(read("SCREEN"));
        assert!(read("only screen"));
        assert!(read("not print"));
        assert!(read("print, screen"));
        assert!(!read("print"));
        assert!(!read("not screen"));
        assert!(!read("only print"));
        assert!(!read("screen and (min-width:1px)"));
        assert!(!read("(hover:hover)"));
        assert!(!read("(width >= 40rem)"));
        assert!(!read("speech"));
    }

    #[test]
    fn a_media_block_nests_in_a_layer_and_in_a_media_block() {
        assert_eq!(
            break_inside_of("@layer u{@media screen{.k{break-inside:avoid}}}", ""),
            Some(Avoid)
        );
        assert_eq!(
            break_inside_of(
                "@media screen{@layer u{@media all{.k{break-inside:avoid}}}}",
                ""
            ),
            Some(Avoid)
        );
        assert_eq!(
            break_inside_of("@layer u{@media print{.k{break-inside:avoid}}}", ""),
            None
        );
    }

    #[test]
    fn supports_blocks_stay_skipped() {
        // Tailwind puts only custom properties and colour fallbacks there; evaluating conditions is out of scope.
        assert_eq!(
            break_inside_of("@supports (display:grid){.k{break-inside:avoid}}", ""),
            None
        );
        assert_eq!(
            break_inside_of(
                "@layer u{@supports not (display:grid){.k{break-inside:avoid}}}",
                ""
            ),
            None
        );
    }

    #[test]
    fn other_at_rules_stay_skipped_and_do_not_swallow_the_next_rule() {
        let css = "@font-face{font-family:x}@property --a{syntax:'*';inherits:false}@import 'x.css';\
                   @page{margin:0}@keyframes k{from{opacity:0}}.k{break-inside:avoid}";
        assert_eq!(break_inside_of(css, ""), Some(Avoid));
    }

    #[test]
    fn an_escaped_class_selector_matches_the_class() {
        let html = r#"<html><body><p id="k" class="print:break-before-page">x</p></body></html>"#;
        let doc = build_doc(html);
        let rules = parse_stylesheet(r"@layer u{.print\:break-before-page{break-before:page}}");
        let table = build_column_style_table(&doc, &rules);
        let id = find_node_with(&doc, |n| {
            n.element_data()
                .and_then(|e| crate::blitz_adapter::get_attr(e, "id"))
                == Some("k")
        })
        .expect("k");
        assert_eq!(
            table.get(&id).and_then(|p| p.break_before),
            Some(BreakBefore::Page)
        );
    }

    #[test]
    fn deeply_nested_blocks_do_not_overflow_the_stack() {
        // A stack overflow aborts the process, `catch_unwind` cannot catch it.
        for open in ["@layer{", "@media screen{", "@layer a.b.c{@media all{"] {
            let css = format!("{}.k{{break-inside:avoid}}", open.repeat(10_000));
            assert_eq!(break_inside_of(&css, ""), None, "{open}");
        }
        let dotted = format!(
            "@layer {}{{.k{{break-inside:avoid}}}}",
            vec!["a"; 10_000].join(".")
        );
        assert_eq!(break_inside_of(&dotted, ""), Some(Avoid));
    }

    #[test]
    fn nesting_up_to_the_cap_is_read() {
        let n = MAX_AT_RULE_DEPTH;
        let css = format!(
            "{}.k{{break-inside:avoid}}{}",
            "@layer{".repeat(n),
            "}".repeat(n)
        );
        assert_eq!(break_inside_of(&css, ""), Some(Avoid));
        let css = format!(
            "{}.k{{break-inside:avoid}}{}",
            "@layer{".repeat(n + 1),
            "}".repeat(n + 1)
        );
        assert_eq!(break_inside_of(&css, ""), None);
    }
    // -------- importance and specificity (F87 review) --------

    #[test]
    fn important_declarations_are_read() {
        for css in [
            ".k{break-inside:avoid!important}",
            ".k{break-inside:avoid !important}",
            ".k{break-inside:avoid ! important}",
            ".k{break-inside:avoid !IMPORTANT}",
            "@layer u{.k{break-inside:avoid!important}}",
        ] {
            assert_eq!(break_inside_of(css, ""), Some(Avoid), "{css}");
        }
        // Only a well-formed flag counts, and a malformed tail drops the whole declaration.
        for css in [
            ".k{break-inside:avoid !importantx}",
            ".k{break-inside:avoid !important foo}",
            ".k{break-inside:avoid !}",
            ".k{break-inside:avoid ! foo}",
        ] {
            assert_eq!(break_inside_of(css, ""), None, "{css}");
        }
        // The flag belongs to its own declaration.
        let html = r#"<html><body><p id="k" class="k">x</p></body></html>"#;
        let doc = build_doc(html);
        let table = build_column_style_table(
            &doc,
            &parse_stylesheet(".k{break-inside:avoid!important;break-before:page}"),
        );
        let id = find_node_with(&doc, |n| {
            n.element_data()
                .and_then(|e| crate::blitz_adapter::get_attr(e, "id"))
                == Some("k")
        })
        .expect("k");
        assert_eq!(table[&id].break_before, Some(BreakBefore::Page));
        assert_eq!(table[&id].break_inside, Some(Avoid));
    }

    #[test]
    fn important_beats_normal_whatever_the_layer_or_the_style_attribute() {
        // Cascade 5 §6.1: importance first. An important rule beats the `style` attribute's normal declaration.
        for css in [
            ".k{break-inside:avoid!important}",
            "@layer u{.k{break-inside:avoid!important}}",
        ] {
            assert_eq!(
                break_inside_of(css, "break-inside:auto"),
                Some(Avoid),
                "{css}"
            );
        }
        // A layered important beats an unlayered normal, in either source order.
        for css in [
            "@layer u{.k{break-inside:auto!important}}.k{break-inside:avoid}",
            ".k{break-inside:avoid}@layer u{.k{break-inside:auto!important}}",
        ] {
            assert_eq!(break_inside_of(css, ""), Some(Auto), "{css}");
        }
    }

    #[test]
    fn important_reverses_the_layer_order() {
        // Cascade 5 §6.4.4: among important declarations an earlier layer beats a later one, and the unlayered
        // styles lose to every layer.
        assert_eq!(
            break_inside_of(
                "@layer a,b;@layer a{.k{break-inside:avoid!important}}@layer b{.k{break-inside:auto!important}}",
                ""
            ),
            Some(Avoid)
        );
        assert_eq!(
            break_inside_of(
                "@layer a,b;@layer b{.k{break-inside:auto!important}}@layer a{.k{break-inside:avoid!important}}",
                ""
            ),
            Some(Avoid)
        );
        assert_eq!(
            break_inside_of(
                "@layer u{.k{break-inside:avoid!important}}.k{break-inside:auto!important}",
                ""
            ),
            Some(Avoid)
        );
        // A layer beats its own sub-layers for normal declarations, and loses to them for important ones.
        assert_eq!(
            break_inside_of(
                "@layer a{.k{break-inside:auto!important}@layer b{.k{break-inside:avoid!important}}}",
                ""
            ),
            Some(Avoid)
        );
    }

    #[test]
    fn important_in_the_style_attribute_beats_everything() {
        for css in [
            ".k{break-inside:auto!important}",
            "@layer u{.k{break-inside:auto!important}}",
            "@layer a,b;@layer a{.k{break-inside:auto!important}}",
        ] {
            assert_eq!(
                break_inside_of(css, "break-inside:avoid!important"),
                Some(Avoid),
                "{css}"
            );
        }
    }

    #[test]
    fn specificity_orders_rules_inside_one_layer_and_unlayered() {
        // The higher specificity wins whatever the source order.
        for css in [
            "@layer u{p.k{break-inside:auto}.k{break-inside:avoid}}",
            "@layer u{.k{break-inside:avoid}p.k{break-inside:auto}}",
            "p.k{break-inside:auto}.k{break-inside:avoid}",
            "@layer u{p#k{break-inside:auto}p.k,.k{break-inside:avoid}}",
        ] {
            assert_eq!(break_inside_of(css, ""), Some(Auto), "{css}");
        }
        // Same specificity: the later rule wins.
        assert_eq!(
            break_inside_of("@layer u{.k{break-inside:auto}.k{break-inside:avoid}}", ""),
            Some(Avoid)
        );
        // A layer outranks specificity: the later layer wins with the weaker selector.
        assert_eq!(
            break_inside_of(
                "@layer a,b;@layer a{p.k{break-inside:auto}}@layer b{.k{break-inside:avoid}}",
                ""
            ),
            Some(Avoid)
        );
        // A selector list counts the selector that matched: `p.k` (0,1,1) matches, `.zz` does not.
        assert_eq!(
            break_inside_of(
                "@layer u{p.k,.zz{break-inside:auto}.k{break-inside:avoid}}",
                ""
            ),
            Some(Auto)
        );
        // `.zz` is not on the element, so the rule's weakest selector never counts.
        assert_eq!(
            break_inside_of(
                "@layer u{.k{break-inside:avoid}.zz,.k.k.k{break-inside:auto}}",
                ""
            ),
            Some(Auto)
        );
        // Important declarations are ordered by specificity too.
        assert_eq!(
            break_inside_of(
                "@layer u{p.k{break-inside:avoid!important}.k{break-inside:auto!important}}",
                ""
            ),
            Some(Avoid)
        );
    }

    #[test]
    fn id_beats_classes_and_types_and_adjacent_siblings_add_up() {
        assert_eq!(
            break_inside_of("#k{break-inside:avoid}p.k.k.k.k{break-inside:auto}", ""),
            Some(Avoid)
        );
        assert_eq!(
            break_inside_of("p.k{break-inside:avoid}.k.k{break-inside:auto}", ""),
            Some(Auto)
        );
    }

    #[test]
    fn whitespace_inside_a_dotted_layer_name_is_invalid() {
        // Cascade 5 §6.4.2: `<layer-name> = <ident> [ '.' <ident> ]*`; no whitespace around the dot.
        for css in [
            "@layer a .b{.k{break-inside:avoid}}",
            "@layer a. b{.k{break-inside:avoid}}",
            "@layer a . b{.k{break-inside:avoid}}",
        ] {
            assert_eq!(break_inside_of(css, ""), None, "{css}");
        }
        assert_eq!(
            break_inside_of("@layer a.b{.k{break-inside:avoid}}", ""),
            Some(Avoid)
        );
        // The invalid statement does not register `a` or `b` either: `a` stays after `b`.
        assert_eq!(
            break_inside_of(
                "@layer b,a;@layer a .x;@layer a{.k{break-inside:avoid}}@layer b{.k{break-inside:auto}}",
                ""
            ),
            Some(Avoid)
        );
    }
    // -------- break-before / break-after values (F87 review) --------

    fn before_after(css: &str) -> (Option<BreakBefore>, Option<BreakAfter>) {
        let p = parse_declaration_block(css);
        (p.break_before, p.break_after)
    }

    #[test]
    fn avoid_and_column_values_map_to_no_forced_break() {
        // css-break-3 §3.1. The table models "forced page break" or "none"; `avoid*` and `column` (outside a
        // multicol) are none, and an unknown region value is dropped.
        for v in [
            "auto",
            "avoid",
            "avoid-page",
            "avoid-column",
            "column",
            "AVOID-PAGE",
        ] {
            assert_eq!(
                before_after(&format!("break-before:{v};break-after:{v}")),
                (Some(BreakBefore::Auto), Some(BreakAfter::Auto)),
                "{v}"
            );
        }
        for v in ["page", "left", "right", "recto", "verso"] {
            assert_eq!(
                before_after(&format!("break-before:{v};break-after:{v}")),
                (Some(BreakBefore::Page), Some(BreakAfter::Page)),
                "{v}"
            );
        }
        for v in ["region", "avoid-region", "banana", "page page"] {
            assert_eq!(
                before_after(&format!("break-before:{v};break-after:{v}")),
                (None, None),
                "{v}"
            );
        }
    }

    #[test]
    fn a_later_avoid_value_overrides_an_earlier_page() {
        // `bf_page_then_always` of the validator: Chrome, page 1.
        for v in ["avoid", "avoid-page", "avoid-column", "column"] {
            assert_eq!(
                before_after(&format!(
                    "break-before:page;break-before:{v};break-after:page;break-after:{v}"
                )),
                (Some(BreakBefore::Auto), Some(BreakAfter::Auto)),
                "{v}"
            );
        }
    }

    #[test]
    fn always_is_a_value_of_the_legacy_properties_only() {
        // css-break-3 §3.4: `page-break-*` takes `auto | always | avoid | left | right`; `break-*` has no `always`
        // (Chrome ignores `break-before: always`, and keeps an earlier `page`).
        assert_eq!(
            before_after("break-before:always;break-after:always"),
            (None, None)
        );
        assert_eq!(
            before_after("page-break-before:always;page-break-after:always"),
            (Some(BreakBefore::Page), Some(BreakAfter::Page))
        );
        assert_eq!(
            before_after("break-before:page;break-before:always"),
            (Some(BreakBefore::Page), None)
        );
        // `avoid` maps through the alias too, and the new-only values are not legacy ones.
        assert_eq!(
            before_after("page-break-before:page;page-break-before:avoid"),
            (Some(BreakBefore::Auto), None)
        );
        for v in ["page", "recto", "verso", "column", "avoid-page"] {
            assert_eq!(
                before_after(&format!("page-break-before:{v};page-break-after:{v}")),
                (None, None),
                "{v}"
            );
        }
    }
    // -------- selectors: the engine's own parser and matcher (F87 review) --------

    /// Ids of the elements of `html` (those with an `id`, in document order) that `selector{break-before:page}`
    /// reaches.
    fn matched_ids(selector: &str, html: &str) -> Vec<String> {
        let doc = build_doc(html);
        let table = build_column_style_table(
            &doc,
            &parse_stylesheet(&format!("{selector}{{break-before:page}}")),
        );
        collect_in_document_order(&doc, |n| {
            n.element_data()
                .and_then(|e| crate::blitz_adapter::get_attr(e, "id"))
                .is_some()
        })
        .into_iter()
        .filter(|id| table.get(id).is_some_and(|p| p.break_before.is_some()))
        .map(|id| {
            let n = doc.get_node(id).expect("node");
            crate::blitz_adapter::get_attr(n.element_data().expect("element"), "id")
                .expect("id")
                .to_string()
        })
        .collect()
    }

    const TREE: &str = r#"<html><body id="body">
        <div id="d1" class="invoice a" data-x="abc b">
          <table id="t"><tbody><tr id="r1" class="k"><td id="c1">x</td></tr><tr id="r2"><td id="c2"></td></tr></tbody></table>
          <p id="p1" class="k">one</p><p id="p2" class="b">two</p><p id="p3" class="k a">three</p>
        </div>
        <div id="d2" class="a"><p id="p4" class="k">four</p></div>
      </body></html>"#;

    #[test]
    fn descendant_and_child_combinators_are_matched() {
        assert_eq!(matched_ids("body .k", TREE), ["r1", "p1", "p3", "p4"]);
        assert_eq!(matched_ids("#d1 .k", TREE), ["r1", "p1", "p3"]);
        assert_eq!(matched_ids(".invoice tr", TREE), ["r1", "r2"]);
        assert_eq!(
            matched_ids("html body div .k", TREE),
            ["r1", "p1", "p3", "p4"]
        );
        assert_eq!(matched_ids("div > .k", TREE), ["p1", "p3", "p4"]);
        assert_eq!(matched_ids("body>div>.k", TREE), ["p1", "p3", "p4"]);
        assert_eq!(matched_ids("tbody > tr", TREE), ["r1", "r2"]);
        assert_eq!(matched_ids("table tr > td", TREE), ["c1", "c2"]);
        assert_eq!(matched_ids("#d2 tr", TREE), Vec::<String>::new());
    }

    #[test]
    fn sibling_combinators_are_matched() {
        assert_eq!(matched_ids("#p1 ~ p", TREE), ["p2", "p3"]);
        assert_eq!(matched_ids(".k + p", TREE), ["p2"]);
        assert_eq!(matched_ids("p + p + p", TREE), ["p3"]);
        assert_eq!(matched_ids("tr + tr", TREE), ["r2"]);
    }

    #[test]
    fn attribute_selectors_are_matched() {
        assert_eq!(matched_ids("[data-x]", TREE), ["d1"]);
        assert_eq!(matched_ids("[data-x=\"abc b\"]", TREE), ["d1"]);
        assert_eq!(matched_ids("[data-x~=b]", TREE), ["d1"]);
        assert_eq!(matched_ids("[data-x~=ab]", TREE), Vec::<String>::new());
        assert_eq!(matched_ids("[data-x^=ab]", TREE), ["d1"]);
        assert_eq!(matched_ids("[data-x$=\" b\"]", TREE), ["d1"]);
        assert_eq!(matched_ids("[data-x*=\"c \"]", TREE), ["d1"]);
        assert_eq!(matched_ids("[class=\"k\"]", TREE), ["r1", "p1", "p4"]);
        assert_eq!(matched_ids("p[class~=a]", TREE), ["p3"]);
    }

    #[test]
    fn not_is_where_and_structural_pseudo_classes_are_matched() {
        assert_eq!(matched_ids(".k:not(.a)", TREE), ["r1", "p1", "p4"]);
        assert_eq!(matched_ids("p:not(.k):not(.a)", TREE), ["p2"]);
        assert_eq!(matched_ids("p:is(.a, .b)", TREE), ["p2", "p3"]);
        assert_eq!(matched_ids("p:where(.b)", TREE), ["p2"]);
        assert_eq!(matched_ids(":is(td, th)", TREE), ["c1", "c2"]);
        assert_eq!(matched_ids("p:first-child", TREE), ["p4"]);
        assert_eq!(matched_ids("#d1 > :first-child", TREE), ["t"]);
        assert_eq!(matched_ids("#d1 > :last-child", TREE), ["p3"]);
        assert_eq!(matched_ids("p:nth-child(2n+1)", TREE), ["p2", "p4"]);
        assert_eq!(matched_ids("#d1 > p:nth-child(odd)", TREE), ["p2"]);
        assert_eq!(matched_ids("#d1 > p:nth-child(even)", TREE), ["p1", "p3"]);
        assert_eq!(matched_ids("tr:nth-child(2)", TREE), ["r2"]);
        assert_eq!(matched_ids("td:empty", TREE), ["c2"]);
        assert_eq!(matched_ids(":root", TREE), Vec::<String>::new()); // the root `<html>` has no id
        assert_eq!(matched_ids("html > body", TREE), ["body"]);
    }

    #[test]
    fn interactive_pseudo_classes_pseudo_elements_and_bad_selectors_match_nothing() {
        for sel in [
            ".k:hover",
            ".k:focus",
            ".k:active",
            ".k::before",
            ".k::after",
            ".k:bogus",
            ".k:not(",
            ".k,,.a",
            ".k,.a:bogus",
            "..k",
            "> .k",
            "",
        ] {
            assert_eq!(matched_ids(sel, TREE), Vec::<String>::new(), "{sel:?}");
        }
        // One valid selector of a list that is valid as a whole still matches.
        assert_eq!(matched_ids(".k:hover, #p1", TREE), ["p1"]);
        // A bad selector drops its rule and nothing else.
        let rules = parse_stylesheet(".k:bogus{break-before:page}.a{break-before:page}");
        assert_eq!(rules.len(), 1);
    }

    #[test]
    fn selector_specificity_comes_from_the_parsed_selector() {
        // Selectors 4 §17: a descendant combinator adds its compounds; `:where()` is 0; `:is()` and `:not()` take
        // their most specific argument; an attribute counts as a class.
        for (css, want) in [
            // `body .k` (0,1,1) beats `.k` whatever the order.
            ("body .k{break-inside:auto}.k{break-inside:avoid}", Auto),
            (".k{break-inside:avoid}body .k{break-inside:auto}", Auto),
            (
                "html body div p.k{break-inside:auto}#k{break-inside:avoid}",
                Avoid,
            ),
            ("body>.k{break-inside:auto}.k{break-inside:avoid}", Auto),
            // `:where()` adds nothing: equal specificity, the later wins.
            (
                ":where(div) .k{break-inside:auto}.k{break-inside:avoid}",
                Avoid,
            ),
            (
                ":where(#k).k{break-inside:auto}.k{break-inside:avoid}",
                Avoid,
            ),
            // `:is()` and `:not()` take the maximum of their arguments.
            (
                ":is(#k, .zz){break-inside:auto}.k.k.k{break-inside:avoid}",
                Auto,
            ),
            (
                ".k:not(#zz){break-inside:auto}.k.k{break-inside:avoid}",
                Auto,
            ),
            (
                ".k:not(.zz){break-inside:auto}.k.k{break-inside:avoid}",
                Avoid,
            ),
            // An attribute selector is a class, and so is a pseudo-class.
            ("[class]{break-inside:auto}.k{break-inside:avoid}", Avoid),
            (
                "[class].k{break-inside:auto}.k.k{break-inside:avoid}",
                Avoid,
            ),
            (
                "p:first-child{break-inside:auto}.k{break-inside:avoid}",
                Auto,
            ),
        ] {
            assert_eq!(break_inside_of(css, ""), Some(want), "{css}");
        }
        // An unlayered descendant rule beats a layered utility (the validator's H1).
        assert_eq!(
            break_inside_of(
                "@layer utilities{.k{break-inside:avoid}}body .k{break-inside:auto}",
                ""
            ),
            Some(Auto)
        );
        // The highest matching selector of a list counts.
        assert_eq!(
            break_inside_of("body .k, .zz{break-inside:auto}.k{break-inside:avoid}", ""),
            Some(Auto)
        );
    }
}
