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

/// A single parsed stylesheet rule: one or more selectors and the properties
/// that apply when any of them matches.
#[derive(Clone, Debug)]
pub(crate) struct StyleRule {
    pub selectors: Vec<ComplexSelector>,
    pub props: ColumnStyleProps,
    /// Cascade layer the rule was written in; `0` for unlayered rules.
    pub layer: LayerId,
}

/// One simple selector — a single unqualified component of a compound
/// selector. `Universal` matches every element; the other variants match
/// against the corresponding attribute / tag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SimpleSelector {
    /// Lowercased tag name (e.g. `div`).
    Type(String),
    /// Class name without the leading `.`.
    Class(String),
    /// Id without the leading `#`.
    Id(String),
    /// `*`.
    Universal,
}

/// Compound selector — multiple [`SimpleSelector`] parts that must all match
/// the same element (logical AND). `div.foo#bar` is a compound of three
/// parts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CompoundSelector {
    pub parts: Vec<SimpleSelector>,
}

/// One leg of a complex selector. `subject` is the element that must match
/// the rule target. `prev_sibling` (if `Some`) is a compound selector that
/// must match the element immediately preceding `subject` in the DOM.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ComplexSelector {
    pub subject: CompoundSelector,
    /// Adjacent-sibling context (`E + F` → F is subject, E is prev_sibling).
    pub prev_sibling: Option<CompoundSelector>,
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

fn parse_break_after_value<'i>(
    input: &mut Parser<'i, '_>,
) -> Result<BreakAfter, ParseError<'i, ()>> {
    let ident = input.expect_ident()?.clone();
    match ident.as_ref().to_ascii_lowercase().as_str() {
        "always" | "page" | "left" | "right" | "recto" | "verso" => Ok(BreakAfter::Page),
        "auto" => Ok(BreakAfter::Auto),
        _ => Err(input.new_error(BasicParseErrorKind::QualifiedRuleInvalid)),
    }
}

fn parse_break_before_value<'i>(
    input: &mut Parser<'i, '_>,
) -> Result<BreakBefore, ParseError<'i, ()>> {
    let ident = input.expect_ident()?.clone();
    match ident.as_ref().to_ascii_lowercase().as_str() {
        "always" | "page" | "left" | "right" | "recto" | "verso" => Ok(BreakBefore::Page),
        "auto" => Ok(BreakBefore::Auto),
        _ => Err(input.new_error(BasicParseErrorKind::QualifiedRuleInvalid)),
    }
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

/// Parse a CSS declaration block (no surrounding braces) into
/// [`ColumnStyleProps`]. Invalid declarations drop silently; valid ones fill
/// their field. Last-declaration-wins within a block.
pub(crate) fn parse_declaration_block(css: &str) -> ColumnStyleProps {
    let mut props = ColumnStyleProps::default();
    let mut input = ParserInput::new(css);
    let mut parser = Parser::new(&mut input);

    let mut decl_parser = ColumnDeclParser { props: &mut props };
    // `RuleBodyParser` walks `decl; decl; decl;` and feeds each declaration
    // through `DeclarationParser::parse_value`. Errors from one declaration
    // do not abort the block — they surface as `Err` items in the iterator
    // which we discard.
    let iter = RuleBodyParser::new(&mut parser, &mut decl_parser);
    for item in iter {
        let _ = item;
    }
    props
}

struct ColumnDeclParser<'a> {
    props: &'a mut ColumnStyleProps,
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
        if name.eq_ignore_ascii_case("column-rule") {
            if let Some(spec) = parse_column_rule_shorthand(input) {
                // A shorthand with `style: None` is equivalent to "don't
                // draw" — we store it anyway so a later longhand can read
                // the width/colour the author still wanted.
                self.props.rule = Some(spec);
            }
        } else if name.eq_ignore_ascii_case("column-rule-width") {
            if let Ok(w) = input.parse_entirely(parse_length) {
                let mut spec = self.props.rule.unwrap_or_default();
                spec.width = w.as_pt();
                self.props.rule = Some(spec);
            }
        } else if name.eq_ignore_ascii_case("column-rule-style") {
            let result = input.parse_entirely(|input| {
                let ident = input.expect_ident()?.clone();
                parse_rule_style_ident(&ident)
                    .ok_or_else(|| input.new_error::<()>(BasicParseErrorKind::QualifiedRuleInvalid))
            });
            if let Ok(style) = result {
                let mut spec = self.props.rule.unwrap_or_default();
                spec.style = style;
                self.props.rule = Some(spec);
            }
        } else if name.eq_ignore_ascii_case("column-rule-color") {
            if let Ok(color) = input.parse_entirely(parse_color) {
                let mut spec = self.props.rule.unwrap_or_default();
                spec.color = color;
                self.props.rule = Some(spec);
            }
        } else if name.eq_ignore_ascii_case("column-fill") {
            if let Ok(fill) = input.parse_entirely(parse_column_fill_value) {
                self.props.fill = Some(fill);
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
                self.props.break_inside = Some(bi);
            }
        } else if name.eq_ignore_ascii_case("break-after")
            || name.eq_ignore_ascii_case("page-break-after")
        {
            if let Ok(v) = input.parse_entirely(parse_break_after_value) {
                self.props.break_after = Some(v);
            }
        } else if name.eq_ignore_ascii_case("break-before")
            || name.eq_ignore_ascii_case("page-break-before")
        {
            if let Ok(v) = input.parse_entirely(parse_break_before_value) {
                self.props.break_before = Some(v);
            }
        } else if name.eq_ignore_ascii_case("page") {
            if let Ok(v) = input.parse_entirely(parse_page_value) {
                self.props.page = Some(v);
            }
        } else {
            // Unknown property — discard its value tokens silently.
            while input.next().is_ok() {}
        }
        Ok(())
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
// Selector parser
// ---------------------------------------------------------------------------

/// Parse a comma-separated selector list. Returns an empty `Vec` when any
/// selector uses unsupported syntax (pseudo-classes, attribute selectors,
/// descendant/child/general-sibling combinators) — per Phase A scope
/// discipline the whole rule is dropped silently so authors do not get a
/// half-applied result. The adjacent-sibling combinator (`+`) is supported.
pub(crate) fn parse_selector_list(input: &str) -> Vec<ComplexSelector> {
    let mut parser_input = ParserInput::new(input);
    let mut parser = Parser::new(&mut parser_input);

    let mut selectors: Vec<ComplexSelector> = Vec::new();
    let mut current = CompoundSelector::default();
    let mut prev_sibling: Option<CompoundSelector> = None;
    // `just_saw_whitespace` is true when the previous real token completed a
    // compound and whitespace followed: the next real token must be `,`, `+`,
    // or EOF, otherwise we have a descendant combinator which is not supported.
    let mut just_saw_whitespace = false;

    loop {
        let tok = match parser.next_including_whitespace() {
            Ok(t) => t.clone(),
            Err(_) => break,
        };

        if matches!(tok, Token::WhiteSpace(_)) {
            just_saw_whitespace = !current.parts.is_empty();
            continue;
        }

        match tok {
            Token::Comma => {
                if current.parts.is_empty() {
                    return Vec::new();
                }
                selectors.push(ComplexSelector {
                    subject: std::mem::take(&mut current),
                    prev_sibling: prev_sibling.take(),
                });
                just_saw_whitespace = false;
            }
            Token::Delim('+') => {
                if current.parts.is_empty() || prev_sibling.is_some() {
                    return Vec::new();
                }
                prev_sibling = Some(std::mem::take(&mut current));
                just_saw_whitespace = false;
            }
            Token::Delim('.') => {
                if just_saw_whitespace {
                    return Vec::new();
                }
                match parser.next_including_whitespace() {
                    Ok(Token::Ident(name)) => {
                        current
                            .parts
                            .push(SimpleSelector::Class(name.as_ref().to_string()));
                    }
                    _ => return Vec::new(),
                }
                just_saw_whitespace = false;
            }
            Token::Delim('*') => {
                if just_saw_whitespace {
                    return Vec::new();
                }
                current.parts.push(SimpleSelector::Universal);
                just_saw_whitespace = false;
            }
            Token::IDHash(name) | Token::Hash(name) => {
                if just_saw_whitespace {
                    return Vec::new();
                }
                current
                    .parts
                    .push(SimpleSelector::Id(name.as_ref().to_string()));
                just_saw_whitespace = false;
            }
            Token::Ident(name) => {
                if just_saw_whitespace {
                    return Vec::new();
                }
                current
                    .parts
                    .push(SimpleSelector::Type(name.as_ref().to_ascii_lowercase()));
                just_saw_whitespace = false;
            }
            // Unsupported tokens: pseudo-classes (`:`), attribute selectors
            // (`[`), child/general-sibling combinators (`>`, `~`), numbers,
            // and anything else the Phase A grammar does not model. Drop the
            // whole list.
            _ => return Vec::new(),
        }
    }

    if !current.parts.is_empty() {
        selectors.push(ComplexSelector {
            subject: std::mem::take(&mut current),
            prev_sibling: prev_sibling.take(),
        });
    } else if selectors.is_empty() || prev_sibling.is_some() {
        // Either the input was empty (no selectors accepted) or a trailing
        // combinator like `.a, .b +` left `prev_sibling` without a subject.
        // Either way, the whole list is invalid — drop it.
        return Vec::new();
    }
    selectors
}

fn matches_compound(sel: &CompoundSelector, node: &blitz_dom::Node) -> bool {
    let Some(elem) = node.element_data() else {
        return false;
    };
    if sel.parts.is_empty() {
        return false;
    }
    sel.parts.iter().all(|part| match part {
        SimpleSelector::Universal => true,
        SimpleSelector::Type(tag) => elem.name.local.as_ref().eq_ignore_ascii_case(tag),
        SimpleSelector::Class(want) => crate::blitz_adapter::get_attr(elem, "class")
            .is_some_and(|classes| classes.split_whitespace().any(|c| c == want)),
        SimpleSelector::Id(want) => {
            crate::blitz_adapter::get_attr(elem, "id").is_some_and(|id| id == want)
        }
    })
}

/// Match a complex selector against `node`, given `node`'s previous *element*
/// sibling (the nearest preceding sibling that is an element, or `None` when
/// there is none — text/comment siblings are skipped by the caller).
///
/// The previous element sibling is supplied by the caller so this stays
/// O(selector): the cascade [`walk`] threads it in document order, which keeps
/// a flat sibling list linear. Resolving it here instead — by scanning the
/// parent's child list on every rule check — is what made a wide sibling list
/// O(n^2).
fn matches_complex_with_prev(
    sel: &ComplexSelector,
    node: &blitz_dom::Node,
    previous_element_sibling: Option<&blitz_dom::Node>,
) -> bool {
    if !matches_compound(&sel.subject, node) {
        return false;
    }
    match (&sel.prev_sibling, previous_element_sibling) {
        // No adjacent-sibling constraint: the subject match is sufficient.
        (None, _) => true,
        // `E + F` requires an element immediately before the subject.
        (Some(_), None) => false,
        (Some(sibling_sel), Some(prev)) => matches_compound(sibling_sel, prev),
    }
}

/// Test-only convenience wrapper that resolves `node`'s previous element
/// sibling from the document (an O(siblings) scan) before delegating to
/// [`matches_complex_with_prev`]. Production code never takes this path — the
/// cascade [`walk`] threads the previous element sibling in O(1); this exists
/// so the matcher's per-node DOM behaviour can be unit-tested in isolation.
#[cfg(test)]
pub(crate) fn matches_complex(
    sel: &ComplexSelector,
    node: &blitz_dom::Node,
    doc: &blitz_html::HtmlDocument,
) -> bool {
    matches_complex_with_prev(sel, node, previous_element_sibling(node, doc))
}

/// Resolve `node`'s previous element sibling by scanning its parent's child
/// list backwards from `node`, skipping non-element (text/comment) siblings.
/// O(siblings); test-only — the cascade [`walk`] computes this incrementally
/// in document order instead of rescanning per node.
#[cfg(test)]
fn previous_element_sibling<'a>(
    node: &blitz_dom::Node,
    doc: &'a blitz_html::HtmlDocument,
) -> Option<&'a blitz_dom::Node> {
    let parent = doc.get_node(node.parent?)?;
    let pos = parent.children.iter().position(|&id| id == node.id)?;
    parent.children[..pos]
        .iter()
        .rev()
        .filter_map(|&id| doc.get_node(id))
        .find(|n| n.element_data().is_some())
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
    use cssparser::Delimiter;
    let mut any = input.is_exhausted();
    while !input.is_exhausted() {
        let query = input.parse_until_before(Delimiter::Comma, |i| {
            Ok::<bool, ParseError<'_, ()>>(media_query_matches(i))
        });
        any |= query.unwrap_or(false);
        // Consume the comma; at the end of the input there is none.
        let _ = input.next();
    }
    any
}

fn media_query_matches(input: &mut Parser<'_, '_>) -> bool {
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
    if !input.is_exhausted() {
        return false;
    }
    let is_all = media_type.eq_ignore_ascii_case("all");
    let is_device = media_type.eq_ignore_ascii_case(READER_MEDIA_TYPE);
    // `not` negates a known type only: `not speech` is not "matches everything".
    let is_other_known =
        media_type.eq_ignore_ascii_case("print") || media_type.eq_ignore_ascii_case("screen");
    if !(is_all || is_device || is_other_known) {
        return false;
    }
    (is_all || is_device) != negated
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
/// The order of the result is the cascade for normal declarations: a stable sort by layer rank
/// ([`LayerTree::ranks`], unlayered rules last), so source order holds inside a layer and
/// [`build_column_style_table`] folding the rules in order gives last-wins per field.
pub(crate) fn parse_stylesheet(source: &str) -> Vec<StyleRule> {
    let mut rules = Vec::new();
    let mut layers = LayerTree::new();
    let mut input = ParserInput::new(source);
    let mut parser = Parser::new(&mut input);

    let mut sheet = SheetParser {
        rules: &mut rules,
        layers: &mut layers,
        layer: 0,
        depth: 0,
    };
    let iter = StyleSheetParser::new(&mut parser, &mut sheet);
    for item in iter {
        let _ = item;
    }
    if layers.nodes.len() > 1 {
        let ranks = layers.ranks();
        rules.sort_by_key(|r| ranks[r.layer]);
    }
    rules
}

struct SheetParser<'a> {
    rules: &'a mut Vec<StyleRule>,
    layers: &'a mut LayerTree,
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
    // `a.b` tokenises as an ident and a `.b` pair: no whitespace is allowed around the dot.
    while input.try_parse(|i| i.expect_delim('.')).is_ok() {
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
    type Prelude = Vec<ComplexSelector>;
    type QualifiedRule = ();
    type Error = ();

    fn parse_prelude<'t>(
        &mut self,
        input: &mut Parser<'i, 't>,
    ) -> Result<Self::Prelude, ParseError<'i, ()>> {
        // Slice out the raw selector text and hand it to the standalone
        // selector parser. This keeps cssparser's error recovery simple:
        // on an unsupported selector we return an empty list, which the
        // `parse_block` below checks.
        let start = input.position();
        while input.next_including_whitespace().is_ok() {}
        let raw = input.slice_from(start);
        let selectors = parse_selector_list(raw);
        if selectors.is_empty() {
            return Err(input.new_error(BasicParseErrorKind::QualifiedRuleInvalid));
        }
        Ok(selectors)
    }

    fn parse_block<'t>(
        &mut self,
        selectors: Self::Prelude,
        _start: &cssparser::ParserState,
        input: &mut Parser<'i, 't>,
    ) -> Result<Self::QualifiedRule, ParseError<'i, ()>> {
        let mut props = ColumnStyleProps::default();
        let mut decl_parser = ColumnDeclParser { props: &mut props };
        let iter = RuleBodyParser::new(input, &mut decl_parser);
        for item in iter {
            let _ = item;
        }
        if !props.is_empty() {
            self.rules.push(StyleRule {
                selectors,
                props,
                layer: self.layer,
            });
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Cascade driver
// ---------------------------------------------------------------------------

/// Build the per-document side-table by folding stylesheet rules (in the
/// order given: cascade order, see [`parse_stylesheet`]) and then inline `style` attributes (last — they beat the
/// stylesheet) into each node's entry. Nodes with no matching rule and no
/// inline declaration are skipped entirely.
pub(crate) fn build_column_style_table(
    doc: &blitz_html::HtmlDocument,
    stylesheet_rules: &[StyleRule],
) -> ColumnStyleTable {
    let mut table = ColumnStyleTable::new();
    let root = doc.root_element();
    // The root element has no previous sibling.
    walk(doc, root.id, None, stylesheet_rules, &mut table, 0);
    table
}

/// Recurse into `node_id`, folding matching rules into `table`.
///
/// Returns whether the visited node is an element, so the caller's child loop
/// can advance its previous-element-sibling marker without re-fetching the
/// child (the loop already pays for the `get_node` inside this call). A node
/// skipped by the depth guard or an unresolved id returns `false`; that only
/// diverges from "is this really an element" at `depth == MAX_DOM_DEPTH`, where
/// every sibling is likewise cut off before it could consume the marker, so
/// the marker value is never observed and the result is unchanged.
fn walk(
    doc: &blitz_html::HtmlDocument,
    node_id: usize,
    previous_element_sibling_id: Option<usize>,
    rules: &[StyleRule],
    table: &mut ColumnStyleTable,
    depth: usize,
) -> bool {
    if depth >= crate::MAX_DOM_DEPTH {
        return false;
    }
    let Some(node) = doc.get_node(node_id) else {
        return false;
    };

    let is_element = node.element_data().is_some();
    if is_element {
        let previous_element_sibling = previous_element_sibling_id.and_then(|id| doc.get_node(id));
        let mut props = ColumnStyleProps::default();
        for rule in rules {
            if rule
                .selectors
                .iter()
                .any(|sel| matches_complex_with_prev(sel, node, previous_element_sibling))
            {
                props.merge(rule.props.clone());
            }
        }
        if let Some(elem) = node.element_data() {
            if let Some(inline) = crate::blitz_adapter::get_attr(elem, "style") {
                let inline_props = parse_declaration_block(inline);
                props.merge(inline_props);
            }
        }
        if !props.is_empty() {
            table.insert(node_id, props);
        }
    }

    // Recurse in document order, carrying each child's previous *element*
    // sibling forward. Non-element children (text/comment) do not advance the
    // marker, so `E + F` skips them exactly as the old backward scan did — but
    // without rescanning the child list per node.
    let mut prev_child_element_id = None;
    for &child_id in &node.children {
        if walk(
            doc,
            child_id,
            prev_child_element_id,
            rules,
            table,
            depth + 1,
        ) {
            prev_child_element_id = Some(child_id);
        }
    }

    is_element
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

    #[test]
    fn selector_parses_type_class_id_universal_and_compound() {
        let list = parse_selector_list("div.foo#bar");
        assert_eq!(list.len(), 1);
        assert_eq!(
            list[0].subject.parts,
            vec![
                SimpleSelector::Type("div".into()),
                SimpleSelector::Class("foo".into()),
                SimpleSelector::Id("bar".into()),
            ]
        );
    }

    #[test]
    fn selector_universal() {
        let list = parse_selector_list("*");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].subject.parts, vec![SimpleSelector::Universal]);
    }

    #[test]
    fn selector_comma_list_parsed_as_multiple_compounds() {
        let list = parse_selector_list(".a, div, #x");
        assert_eq!(list.len(), 3);
        assert_eq!(
            list[0].subject.parts,
            vec![SimpleSelector::Class("a".into())]
        );
        assert_eq!(
            list[1].subject.parts,
            vec![SimpleSelector::Type("div".into())]
        );
        assert_eq!(list[2].subject.parts, vec![SimpleSelector::Id("x".into())]);
    }

    #[test]
    fn selector_unsupported_pseudo_drops_list() {
        assert!(parse_selector_list("a:hover").is_empty());
    }

    #[test]
    fn selector_unsupported_descendant_drops_list() {
        assert!(parse_selector_list("a b").is_empty());
    }

    #[test]
    fn selector_unsupported_child_combinator_drops_list() {
        assert!(parse_selector_list("a > b").is_empty());
    }

    #[test]
    fn selector_unsupported_attribute_drops_list() {
        assert!(parse_selector_list("input[type=x]").is_empty());
    }

    #[test]
    fn selector_tag_lowercased() {
        let list = parse_selector_list("DIV");
        assert_eq!(
            list[0].subject.parts,
            vec![SimpleSelector::Type("div".into())]
        );
    }

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

    #[test]
    fn matches_node_by_tag_class_and_id() {
        let doc = build_doc(
            r#"<html><body>
                <div class="mc" id="one"></div>
                <p class="other"></p>
              </body></html>"#,
        );
        let div_id = find_node_with(&doc, |n| {
            n.element_data()
                .is_some_and(|e| e.name.local.as_ref() == "div")
        })
        .expect("div");
        let div = doc.get_node(div_id).unwrap();

        let t = CompoundSelector {
            parts: vec![SimpleSelector::Type("div".into())],
        };
        let c = CompoundSelector {
            parts: vec![SimpleSelector::Class("mc".into())],
        };
        let i = CompoundSelector {
            parts: vec![SimpleSelector::Id("one".into())],
        };
        let compound = CompoundSelector {
            parts: vec![
                SimpleSelector::Type("div".into()),
                SimpleSelector::Class("mc".into()),
                SimpleSelector::Id("one".into()),
            ],
        };
        let not_match = CompoundSelector {
            parts: vec![SimpleSelector::Class("other".into())],
        };

        assert!(matches_compound(&t, div));
        assert!(matches_compound(&c, div));
        assert!(matches_compound(&i, div));
        assert!(matches_compound(&compound, div));
        assert!(!matches_compound(&not_match, div));
    }

    #[test]
    fn universal_selector_matches_every_element() {
        let doc = build_doc("<html><body><span></span></body></html>");
        let span_id = find_node_with(&doc, |n| {
            n.element_data()
                .is_some_and(|e| e.name.local.as_ref() == "span")
        })
        .expect("span");
        let span = doc.get_node(span_id).unwrap();
        let u = CompoundSelector {
            parts: vec![SimpleSelector::Universal],
        };
        assert!(matches_compound(&u, span));
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
        assert_eq!(rules[0].selectors[0].subject.parts.len(), 1);
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
        let css = r#"
            .z { column-fill: auto; }
            @layer late { .y { column-fill: auto; } }
            @media screen { .x { column-fill: auto; } }
            @layer early { .w { column-fill: auto; } }
        "#;
        let rules = parse_stylesheet(css);
        let class = |r: &StyleRule| match &r.selectors[0].subject.parts[0] {
            SimpleSelector::Class(c) => c.clone(),
            other => panic!("{other:?}"),
        };
        // Layers by first appearance (`late` < `early`), then unlayered rules (`.z`, `.x`) in source order.
        let order: Vec<String> = rules.iter().map(class).collect();
        assert_eq!(order, ["y", "w", "z", "x"]);
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
    fn stylesheet_drops_unsupported_selector() {
        let css = r#"
            a:hover { column-fill: auto; }
            .mc { column-fill: balance; }
        "#;
        let rules = parse_stylesheet(css);
        assert_eq!(rules.len(), 1);
        assert_eq!(
            rules[0].selectors[0].subject.parts[0],
            SimpleSelector::Class("mc".into())
        );
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
        let props = parse_inline_style("break-after: always");
        assert_eq!(props.break_after, Some(BreakAfter::Page));
    }

    #[test]
    fn parse_page_break_after_always_inline() {
        let props = parse_inline_style("page-break-after: always");
        assert_eq!(props.break_after, Some(BreakAfter::Page));
    }

    #[test]
    fn parse_break_before_always_inline() {
        let props = parse_inline_style("break-before: always");
        assert_eq!(props.break_before, Some(BreakBefore::Page));
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

    #[test]
    fn adjacent_sibling_selector_parsed() {
        let selectors = parse_selector_list("div + div");
        assert_eq!(selectors.len(), 1);
        let sel = &selectors[0];
        assert_eq!(sel.subject.parts, vec![SimpleSelector::Type("div".into())]);
        let prev = sel
            .prev_sibling
            .as_ref()
            .expect("prev_sibling should be Some");
        assert_eq!(prev.parts, vec![SimpleSelector::Type("div".into())]);
    }

    #[test]
    fn combinator_gt_drops_rule() {
        assert!(parse_selector_list("div > p").is_empty());
    }

    #[test]
    fn combinator_tilde_drops_rule() {
        assert!(parse_selector_list("div ~ p").is_empty());
    }

    #[test]
    fn simple_selector_no_sibling() {
        let selectors = parse_selector_list("div");
        assert_eq!(selectors.len(), 1);
        assert!(selectors[0].prev_sibling.is_none());
    }

    #[test]
    fn descendant_after_plus_drops_rule() {
        assert!(parse_selector_list("div + div .foo").is_empty());
    }

    /// `a + b + c` can't be represented by the one-leg ComplexSelector model,
    /// so it must be dropped rather than silently degrading to `b + c`.
    #[test]
    fn chained_adjacent_sibling_drops_rule() {
        assert!(parse_selector_list("div + p + span").is_empty());
    }

    /// Trailing `+` in a comma-separated list (`.a, .b +`) must invalidate
    /// the whole list rather than silently returning only `.a`.
    #[test]
    fn trailing_plus_in_comma_list_drops_rule() {
        assert!(parse_selector_list(".a, .b +").is_empty());
    }

    // -------- matches_complex DOM behaviour --------

    /// `div + div` matches the second div but not the first.
    #[test]
    fn matches_complex_adjacent_sibling_matches_second_not_first() {
        let doc = build_doc(
            r#"<html><body>
                <div id="first"></div>
                <div id="second"></div>
            </body></html>"#,
        );

        let first_id = find_node_with(&doc, |n| {
            n.element_data()
                .and_then(|e| crate::blitz_adapter::get_attr(e, "id"))
                == Some("first")
        })
        .expect("first");
        let second_id = find_node_with(&doc, |n| {
            n.element_data()
                .and_then(|e| crate::blitz_adapter::get_attr(e, "id"))
                == Some("second")
        })
        .expect("second");

        // `div + div`: prev_sibling = div, subject = div
        let div_compound = CompoundSelector {
            parts: vec![SimpleSelector::Type("div".into())],
        };
        let sel = ComplexSelector {
            subject: div_compound.clone(),
            prev_sibling: Some(div_compound),
        };

        let first = doc.get_node(first_id).unwrap();
        let second = doc.get_node(second_id).unwrap();

        // The second div has a div immediately before it → should match.
        assert!(matches_complex(&sel, second, &doc));
        // The first div has no element before it → should not match.
        assert!(!matches_complex(&sel, first, &doc));
    }

    /// A `ComplexSelector` with `prev_sibling: None` matches the first element.
    #[test]
    fn matches_complex_no_sibling_context_matches_first_element() {
        let doc = build_doc(
            r#"<html><body>
                <div id="only"></div>
            </body></html>"#,
        );

        let only_id = find_node_with(&doc, |n| {
            n.element_data()
                .and_then(|e| crate::blitz_adapter::get_attr(e, "id"))
                == Some("only")
        })
        .expect("only");

        let sel = ComplexSelector {
            subject: CompoundSelector {
                parts: vec![SimpleSelector::Type("div".into())],
            },
            prev_sibling: None,
        };

        let only = doc.get_node(only_id).unwrap();
        assert!(matches_complex(&sel, only, &doc));
    }

    /// Text nodes between siblings must not prevent the adjacent-sibling match.
    #[test]
    fn matches_complex_adjacent_sibling_skips_text_nodes() {
        // Explicit text between the two divs guarantees a text node in the DOM.
        let doc =
            build_doc(r#"<html><body><div id="a"></div>some text<div id="b"></div></body></html>"#);

        let b_id = find_node_with(&doc, |n| {
            n.element_data()
                .and_then(|e| crate::blitz_adapter::get_attr(e, "id"))
                == Some("b")
        })
        .expect("b");

        let div_compound = CompoundSelector {
            parts: vec![SimpleSelector::Type("div".into())],
        };
        let sel = ComplexSelector {
            subject: div_compound.clone(),
            prev_sibling: Some(div_compound),
        };

        let b = doc.get_node(b_id).unwrap();
        // Text node between #a and #b must be skipped; #a is still the
        // adjacent element sibling of #b.
        assert!(matches_complex(&sel, b, &doc));
    }

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
        let props = parse_declaration_block("break-before: always;");
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

    #[test]
    fn selector_leading_comma_drops_list() {
        // A leading comma means `current.parts` is empty when the first comma
        // is encountered — the whole list is immediately invalid.
        assert!(parse_selector_list(",div").is_empty());
    }

    #[test]
    fn selector_dot_followed_by_non_ident_drops_list() {
        // `..foo`: the outer `.` triggers the Delim('.') arm; the inner
        // `next_including_whitespace()` yields another `.` (not an ident),
        // so the `_ => return Vec::new()` arm fires.
        assert!(parse_selector_list("..foo").is_empty());
    }

    #[test]
    fn selector_star_after_whitespace_drops_list() {
        // `div *` is a descendant-star combinator — unsupported. The `*` token
        // arrives while `just_saw_whitespace` is true, triggering the early return.
        assert!(parse_selector_list("div *").is_empty());
    }

    #[test]
    fn selector_hash_after_whitespace_drops_list() {
        // `div #id` is a descendant combinator with an id — unsupported.
        // The IDHash token arrives while `just_saw_whitespace` is true.
        assert!(parse_selector_list("div #id").is_empty());
    }

    #[test]
    fn selector_dot_after_whitespace_drops_list() {
        // `div .foo` is a descendant combinator with a class — unsupported.
        // The Delim('.') token arrives while `just_saw_whitespace` is true.
        assert!(parse_selector_list("div .foo").is_empty());
    }

    #[test]
    fn selector_leading_plus_drops_list() {
        // A `+` with no preceding compound (current.parts empty) is invalid.
        assert!(parse_selector_list("+div").is_empty());
    }
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
}
