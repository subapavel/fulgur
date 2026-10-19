use crate::config::{Config, Margin, PageSize};
use crate::gcpm::{PageSettingsRule, PageSizeDecl, PartialMargin};

/// Map a CSS page-size keyword (case-insensitive) to a [`PageSize`], or `None` when it is not one of
/// css-page-3's `<page-size>` keywords ("Page size: the size property").
///
/// `A4`, `A3` and `letter` keep the [`PageSize`] constants so everything rendered with them stays
/// byte-identical; the others are built from their millimetre or inch sizes.
pub(crate) fn keyword_to_page_size(name: &str) -> Option<PageSize> {
    Some(match name.to_ascii_lowercase().as_str() {
        "a5" => PageSize::custom(148.0, 210.0),
        "a4" => PageSize::A4,
        "a3" => PageSize::A3,
        "b5" => PageSize::custom(176.0, 250.0),
        "b4" => PageSize::custom(250.0, 353.0),
        "jis-b5" => PageSize::custom(182.0, 257.0),
        "jis-b4" => PageSize::custom(257.0, 364.0),
        "letter" => PageSize::LETTER,
        // 8.5 x 14 in and 11 x 17 in; a point is 1/72 in.
        "legal" => PageSize {
            width: 8.5 * 72.0,
            height: 14.0 * 72.0,
        },
        "ledger" => PageSize {
            width: 11.0 * 72.0,
            height: 17.0 * 72.0,
        },
        _ => return None,
    })
}

/// Returns `true` when `selector` matches the given page number.
///
/// Supported pseudo-selectors:
/// - `:first` — matches page 1 only
/// - `:left`  — even pages in LTR, odd pages in RTL (CSS Paged Media §5:
///   RTL documents start on a `:left` page)
/// - `:right` — odd pages in LTR, even pages in RTL
fn selector_matches(selector: &str, page_num: usize, first_page_is_left: bool) -> bool {
    match selector {
        ":first" => page_num == 1,
        ":left" => {
            if first_page_is_left {
                page_num % 2 == 1
            } else {
                page_num % 2 == 0
            }
        }
        ":right" => {
            if first_page_is_left {
                page_num % 2 == 0
            } else {
                page_num % 2 == 1
            }
        }
        _ => false,
    }
}

/// Resolve effective page size, margin, and landscape for a given page number.
///
/// Priority model (highest wins):
///
/// ```text
/// CLI override (config.overrides) > CSS @page selector match > CSS @page default > Config defaults
/// ```
///
/// "Config defaults" are `Config::page_size` / `Config::margin` while the matching `overrides` flag is
/// false: `ConfigBuilder::default_margin` sets the margin that way.
///
/// When `config.overrides.page_size` is set (i.e. `--size` / `builder.page_size`),
/// the CLI fully owns page geometry **including orientation**: CSS `@page`
/// orientation is intentionally ignored, and landscape is taken solely from
/// `config.landscape` (request it with `--landscape` / `builder.landscape(true)`).
pub fn resolve_page_settings(
    rules: &[PageSettingsRule],
    page_num: usize,
    _total_pages: usize,
    config: &Config,
    first_page_is_left: bool,
) -> (PageSize, Margin, bool) {
    // --- Collect CSS declarations, separating default from selector-matched ---
    let mut default_size: Option<&PageSizeDecl> = None;
    let mut default_margin = PartialMargin::default();
    let mut matched_size: Option<&PageSizeDecl> = None;
    let mut matched_margin = PartialMargin::default();

    // Later rules override earlier ones per side; selector-matched layer
    // wins over the default layer.
    for rule in rules {
        match &rule.page_selector {
            None => {
                if rule.size.is_some() {
                    default_size = rule.size.as_ref();
                }
                default_margin.merge(&rule.margin);
            }
            Some(sel) => {
                if selector_matches(sel, page_num, first_page_is_left) {
                    if rule.size.is_some() {
                        matched_size = rule.size.as_ref();
                    }
                    matched_margin.merge(&rule.margin);
                }
            }
        }
    }

    let css_size = matched_size.or(default_size);

    // --- Resolve page size and landscape ---
    let (size, landscape) = if config.overrides.page_size {
        // CLI `--size` owns geometry, including orientation: CSS `@page`
        // orientation is ignored so `--size` is authoritative on both axes.
        // Landscape comes solely from `config.landscape` (default portrait;
        // set via `--landscape` / `builder.landscape(true)`).
        (config.page_size, config.landscape)
    } else {
        match css_size {
            Some(PageSizeDecl::Keyword(name)) => {
                // Keyword without orientation carries no landscape signal;
                // use config.landscape regardless of override flag.
                // The parser only stores known keywords; a rule built by hand with another one keeps the
                // configured size.
                (
                    keyword_to_page_size(name).unwrap_or(config.page_size),
                    config.landscape,
                )
            }
            Some(PageSizeDecl::KeywordWithOrientation(name, is_landscape)) => {
                let ls = if config.overrides.landscape {
                    config.landscape
                } else {
                    *is_landscape
                };
                let size = if name == "auto" {
                    config.page_size
                } else {
                    keyword_to_page_size(name).unwrap_or(config.page_size)
                };
                (size, ls)
            }
            Some(PageSizeDecl::Custom(w, h)) => {
                let ls = if config.overrides.landscape {
                    config.landscape
                } else {
                    false
                };
                (
                    PageSize {
                        width: *w,
                        height: *h,
                    },
                    ls,
                )
            }
            Some(PageSizeDecl::Auto) | None => {
                // No CSS size — fall back entirely to config defaults.
                (config.page_size, config.landscape)
            }
        }
    };

    // Margin cascade: config.margin → default @page → matched selector.
    // Each layer overlays per-side, so partial longhand declarations only
    // affect their own side.
    let margin = if config.overrides.margin {
        config.margin
    } else {
        let mut m = config.margin;
        default_margin.apply_to_margin(&mut m);
        matched_margin.apply_to_margin(&mut m);
        m
    };

    (size, margin, landscape)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, PageSize};
    use crate::gcpm::{PageSettingsRule, PageSizeDecl, PartialMargin};

    #[test]
    fn test_no_page_settings_uses_config() {
        let config = Config::default();
        let (size, margin, landscape) = resolve_page_settings(&[], 1, 10, &config, false);
        assert!((size.width - PageSize::A4.width).abs() < 0.01);
        assert!((margin.top - config.margin.top).abs() < 0.01);
        assert!(!landscape);
    }

    #[test]
    fn test_css_size_overrides_default_config() {
        let config = Config::default(); // overrides all false
        let rules = vec![PageSettingsRule {
            page_selector: None,
            size: Some(PageSizeDecl::Keyword("letter".into())),
            margin: PartialMargin::default(),
        }];
        let (size, _, _) = resolve_page_settings(&rules, 1, 10, &config, false);
        assert!((size.width - PageSize::LETTER.width).abs() < 0.01);
    }

    #[test]
    fn test_cli_override_beats_css() {
        let config = Config::builder().page_size(PageSize::A3).build();
        // config.overrides.page_size is true
        let rules = vec![PageSettingsRule {
            page_selector: None,
            size: Some(PageSizeDecl::Keyword("letter".into())),
            margin: PartialMargin::default(),
        }];
        let (size, _, _) = resolve_page_settings(&rules, 1, 10, &config, false);
        assert!((size.width - PageSize::A3.width).abs() < 0.01);
    }

    #[test]
    fn test_selector_first_matches_page_1() {
        let config = Config::default();
        let rules = vec![
            PageSettingsRule {
                page_selector: None,
                size: None,
                margin: PartialMargin::from_uniform(20.0),
            },
            PageSettingsRule {
                page_selector: Some(":first".into()),
                size: None,
                margin: PartialMargin::from_uniform(50.0),
            },
        ];
        let (_, margin_p1, _) = resolve_page_settings(&rules, 1, 10, &config, false);
        assert!((margin_p1.top - 50.0).abs() < 0.01);
        let (_, margin_p2, _) = resolve_page_settings(&rules, 2, 10, &config, false);
        assert!((margin_p2.top - 20.0).abs() < 0.01);
    }

    #[test]
    fn test_left_right_selectors() {
        let config = Config::default();
        let rules = vec![
            PageSettingsRule {
                page_selector: Some(":left".into()),
                size: None,
                margin: PartialMargin::from_sides(20.0, 30.0, 20.0, 10.0),
            },
            PageSettingsRule {
                page_selector: Some(":right".into()),
                size: None,
                margin: PartialMargin::from_sides(20.0, 10.0, 20.0, 30.0),
            },
        ];
        let (_, m2, _) = resolve_page_settings(&rules, 2, 10, &config, false);
        assert!((m2.left - 10.0).abs() < 0.01);
        let (_, m3, _) = resolve_page_settings(&rules, 3, 10, &config, false);
        assert!((m3.left - 30.0).abs() < 0.01);
    }

    #[test]
    fn test_page_size_landscape_from_css() {
        let config = Config::default();
        let rules = vec![PageSettingsRule {
            page_selector: None,
            size: Some(PageSizeDecl::KeywordWithOrientation("A4".into(), true)),
            margin: PartialMargin::default(),
        }];
        let (_, _, landscape) = resolve_page_settings(&rules, 1, 10, &config, false);
        assert!(landscape);
    }

    #[test]
    fn test_cli_override_ignores_css_landscape() {
        // `--size` (overrides.page_size) is authoritative on the orientation
        // axis too: a CSS `@page { size: <keyword> landscape }` must not rotate
        // the CLI-specified size. Without `--landscape`, config.landscape is
        // false, so the result stays portrait. (fulgur-u4k5)
        let config = Config::builder().page_size(PageSize::A4).build();
        let rules = vec![PageSettingsRule {
            page_selector: None,
            size: Some(PageSizeDecl::KeywordWithOrientation("A4".into(), true)),
            margin: PartialMargin::default(),
        }];
        let (_, _, landscape) = resolve_page_settings(&rules, 1, 10, &config, false);
        assert!(!landscape, "CLI --size must ignore CSS @page landscape");
    }

    #[test]
    fn test_cli_override_with_landscape_flag_is_landscape() {
        // `--size A4 --landscape` still yields landscape: orientation comes
        // solely from config.landscape in the override path. (fulgur-u4k5)
        let config = Config::builder()
            .page_size(PageSize::A4)
            .landscape(true)
            .build();
        let rules = vec![PageSettingsRule {
            page_selector: None,
            size: Some(PageSizeDecl::KeywordWithOrientation("A4".into(), false)),
            margin: PartialMargin::default(),
        }];
        let (_, _, landscape) = resolve_page_settings(&rules, 1, 10, &config, false);
        assert!(landscape, "--landscape must force landscape under --size");
    }

    #[test]
    fn test_custom_page_size() {
        let config = Config::default();
        let rules = vec![PageSettingsRule {
            page_selector: None,
            size: Some(PageSizeDecl::Custom(400.0, 600.0)),
            margin: PartialMargin::default(),
        }];
        let (size, _, landscape) = resolve_page_settings(&rules, 1, 10, &config, false);
        assert!((size.width - 400.0).abs() < 0.01);
        assert!((size.height - 600.0).abs() < 0.01);
        assert!(!landscape);
    }

    #[test]
    fn test_partial_margin_inherits_unset_sides_from_default() {
        // Default `@page { margin: 0 }` should provide bottom and left when
        // a matched selector only sets top and right.
        let config = Config::default();
        let rules = vec![
            PageSettingsRule {
                page_selector: None,
                size: None,
                margin: PartialMargin::from_uniform(0.0),
            },
            PageSettingsRule {
                page_selector: Some(":right".into()),
                size: None,
                margin: PartialMargin {
                    top: Some(150.0),
                    right: Some(375.0),
                    bottom: None,
                    left: None,
                },
            },
        ];
        let (_, m, _) = resolve_page_settings(&rules, 1, 10, &config, false);
        assert!((m.top - 150.0).abs() < 0.01);
        assert!((m.right - 375.0).abs() < 0.01);
        assert!((m.bottom - 0.0).abs() < 0.01);
        assert!((m.left - 0.0).abs() < 0.01);
    }

    #[test]
    fn test_partial_margin_falls_back_to_config_when_no_default_rule() {
        let config = Config::default();
        let rules = vec![PageSettingsRule {
            page_selector: Some(":first".into()),
            size: None,
            margin: PartialMargin {
                top: Some(100.0),
                right: None,
                bottom: None,
                left: None,
            },
        }];
        let (_, m, _) = resolve_page_settings(&rules, 1, 10, &config, false);
        assert!((m.top - 100.0).abs() < 0.01);
        assert!((m.right - config.margin.right).abs() < 0.01);
        assert!((m.bottom - config.margin.bottom).abs() < 0.01);
        assert!((m.left - config.margin.left).abs() < 0.01);
    }

    #[test]
    fn test_left_right_selectors_rtl() {
        // In RTL documents the first page is :left (first_page_is_left = true).
        // Odd pages → :left, even pages → :right.
        let config = Config::default();
        let rules = vec![
            PageSettingsRule {
                page_selector: Some(":left".into()),
                size: None,
                margin: PartialMargin::from_uniform(11.0),
            },
            PageSettingsRule {
                page_selector: Some(":right".into()),
                size: None,
                margin: PartialMargin::from_uniform(22.0),
            },
        ];
        let (_, m1, _) = resolve_page_settings(&rules, 1, 4, &config, true);
        assert!(
            (m1.top - 11.0).abs() < 0.01,
            "page 1 should be :left in RTL"
        );
        let (_, m2, _) = resolve_page_settings(&rules, 2, 4, &config, true);
        assert!(
            (m2.top - 22.0).abs() < 0.01,
            "page 2 should be :right in RTL"
        );
        let (_, m3, _) = resolve_page_settings(&rules, 3, 4, &config, true);
        assert!(
            (m3.top - 11.0).abs() < 0.01,
            "page 3 should be :left in RTL"
        );
        let (_, m4, _) = resolve_page_settings(&rules, 4, 4, &config, true);
        assert!(
            (m4.top - 22.0).abs() < 0.01,
            "page 4 should be :right in RTL"
        );
    }

    // --- F6 (htmltopdf): complete css-page-3 keyword table, config default margin ---

    fn keyword(name: &str) -> (f32, f32) {
        let s =
            keyword_to_page_size(name).unwrap_or_else(|| panic!("{name} is a page-size keyword"));
        (s.width, s.height)
    }

    fn assert_pt(got: (f32, f32), want: (f32, f32), what: &str) {
        assert!(
            (got.0 - want.0).abs() < 0.01 && (got.1 - want.1).abs() < 0.01,
            "{what}: {got:?}, want {want:?}"
        );
    }

    #[test]
    fn keyword_table_covers_css_page_3() {
        // Sizes in mm from css-page-3 "Page size: the size property", converted at 72/25.4.
        let mm = |w: f32, h: f32| (w * 72.0 / 25.4, h * 72.0 / 25.4);
        assert_pt(keyword("a5"), mm(148.0, 210.0), "A5");
        assert_pt(keyword("a4"), mm(210.0, 297.0), "A4");
        assert_pt(keyword("a3"), mm(297.0, 420.0), "A3");
        assert_pt(keyword("b5"), mm(176.0, 250.0), "B5");
        assert_pt(keyword("b4"), mm(250.0, 353.0), "B4");
        assert_pt(keyword("jis-b5"), mm(182.0, 257.0), "JIS-B5");
        assert_pt(keyword("jis-b4"), mm(257.0, 364.0), "JIS-B4");
        assert_pt(keyword("letter"), (612.0, 792.0), "letter");
        assert_pt(keyword("legal"), (612.0, 1008.0), "legal");
        assert_pt(keyword("ledger"), (792.0, 1224.0), "ledger");
    }

    #[test]
    fn keyword_table_is_case_insensitive_and_keeps_the_existing_constants() {
        assert_pt(keyword("A5"), keyword("a5"), "case");
        assert_pt(keyword("JIS-B5"), keyword("jis-b5"), "case");
        // A4, A3 and letter stay the constants every golden was rendered with.
        let a4 = keyword("A4");
        assert_eq!((a4.0, a4.1), (PageSize::A4.width, PageSize::A4.height));
        let a3 = keyword("A3");
        assert_eq!((a3.0, a3.1), (PageSize::A3.width, PageSize::A3.height));
        let l = keyword("Letter");
        assert_eq!(
            (l.0, l.1),
            (PageSize::LETTER.width, PageSize::LETTER.height)
        );
    }

    #[test]
    fn unknown_keyword_is_not_a_page_size() {
        assert!(keyword_to_page_size("Foo").is_none());
        assert!(keyword_to_page_size("auto").is_none());
        assert!(keyword_to_page_size("").is_none());
    }

    #[test]
    fn css_a5_resolves_to_a5() {
        let config = Config::default();
        let rules = vec![PageSettingsRule {
            page_selector: None,
            size: Some(PageSizeDecl::Keyword("A5".into())),
            margin: PartialMargin::default(),
        }];
        let (size, _, landscape) = resolve_page_settings(&rules, 1, 1, &config, false);
        assert_pt((size.width, size.height), keyword("A5"), "size: A5");
        assert!(!landscape);
    }

    #[test]
    fn css_landscape_a5_is_landscape() {
        let config = Config::default();
        let rules = vec![PageSettingsRule {
            page_selector: None,
            size: Some(PageSizeDecl::KeywordWithOrientation("A5".into(), true)),
            margin: PartialMargin::default(),
        }];
        let (size, _, landscape) = resolve_page_settings(&rules, 1, 1, &config, false);
        assert_pt(
            (size.width, size.height),
            keyword("A5"),
            "size: A5 landscape",
        );
        assert!(landscape);
    }

    #[test]
    fn default_margin_is_beaten_by_css_margin() {
        use crate::config::Margin;
        let config = Config::builder()
            .default_margin(Margin::uniform(40.0))
            .build();
        assert!(!config.overrides.margin, "a default is not an override");
        // No CSS margin: the default stands, per side.
        let (_, m, _) = resolve_page_settings(&[], 1, 1, &config, false);
        assert_eq!(m, Margin::uniform(40.0));
        // CSS margin: wins, side by side.
        let rules = vec![PageSettingsRule {
            page_selector: None,
            size: None,
            margin: PartialMargin::from_sides(10.0, 11.0, 12.0, 13.0),
        }];
        let (_, m, _) = resolve_page_settings(&rules, 1, 1, &config, false);
        assert_eq!(
            m,
            Margin {
                top: 10.0,
                right: 11.0,
                bottom: 12.0,
                left: 13.0
            }
        );
        // CSS size alone leaves the default margin alone.
        let rules = vec![PageSettingsRule {
            page_selector: None,
            size: Some(PageSizeDecl::Keyword("A5".into())),
            margin: PartialMargin::default(),
        }];
        let (_, m, _) = resolve_page_settings(&rules, 1, 1, &config, false);
        assert_eq!(m, Margin::uniform(40.0));
    }

    #[test]
    fn explicit_margin_still_beats_css() {
        use crate::config::Margin;
        let config = Config::builder().margin(Margin::uniform(40.0)).build();
        assert!(config.overrides.margin);
        let rules = vec![PageSettingsRule {
            page_selector: None,
            size: None,
            margin: PartialMargin::from_uniform(10.0),
        }];
        let (_, m, _) = resolve_page_settings(&rules, 1, 1, &config, false);
        assert_eq!(m, Margin::uniform(40.0));
    }
}
