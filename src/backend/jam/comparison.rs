//! Closed diagnostic predicates only. Never constructs or supplies an invitation.
use super::Link;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Shape {
    #[default]
    NotObserved,
    Accepted,
    BareAsciiAlnum,
    HttpKnownLong,
    RelativeKnownLong,
    SpaceWrappedKnownLong,
    RelativeJoin,
    NetworkRelativeKnown,
    CaseVariantKnownPrefix,
    LiteralSlashEscapedKnown,
    PercentEncodedKnownPrefix,
    SpaceWrappedBare,
    Other,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OfficialRelation {
    #[default]
    NotComparable,
    Same,
    Different,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Comparison {
    pub shape: Shape,
    pub official_relation: OfficialRelation,
}

fn candidate(value: &str) -> (Shape, Option<&str>) {
    if value.len() > 2048 {
        return (Shape::Other, None);
    }
    let known = [
        ("https://open.spotify.com/socialsession/", Shape::Accepted),
        (
            "http://open.spotify.com/socialsession/",
            Shape::HttpKnownLong,
        ),
        ("/socialsession/", Shape::RelativeKnownLong),
    ];
    for (prefix, shape) in known {
        if let Some(tail) = value.strip_prefix(prefix) {
            return Link::parse_tail(tail)
                .map_or((Shape::Other, None), |token| (shape, Some(token)));
        }
    }
    for prefix in [
        "spotify://sessions/join/",
        "https://open.spotify.com/sessions/join/",
    ] {
        if let Some(token) = value.strip_prefix(prefix) {
            return if Link::validate_token(token).is_ok() {
                (Shape::Accepted, Some(token))
            } else {
                (Shape::Other, None)
            };
        }
    }
    if Link::validate_token(value).is_ok() {
        return (Shape::BareAsciiAlnum, Some(value));
    }
    let core = value.trim_matches(' ');
    if core != value
        && let Some(tail) = core.strip_prefix("https://open.spotify.com/socialsession/")
    {
        return Link::parse_tail(tail).map_or((Shape::Other, None), |token| {
            (Shape::SpaceWrappedKnownLong, Some(token))
        });
    }
    extended_candidate(value)
}

// One finite diagnostic extension only. Every rule consumes a complete fixed
// envelope and borrows a strict token. Never decodes or searches for a secret.
fn extended_candidate(value: &str) -> (Shape, Option<&str>) {
    if let Some(token) = value.strip_prefix("/sessions/join/") {
        return token_candidate(Shape::RelativeJoin, token);
    }
    for (prefix, shape) in [
        (
            "//open.spotify.com/socialsession/",
            Shape::NetworkRelativeKnown,
        ),
        (
            r"https:\/\/open.spotify.com\/socialsession\/",
            Shape::LiteralSlashEscapedKnown,
        ),
    ] {
        if let Some(tail) = value.strip_prefix(prefix) {
            return Link::parse_tail(tail)
                .map_or((Shape::Other, None), |token| (shape, Some(token)));
        }
    }
    const HTTPS: &str = "https://open.spotify.com/socialsession/";
    if let Some(prefix) = value.get(..HTTPS.len())
        && prefix != HTTPS
        && prefix.eq_ignore_ascii_case(HTTPS)
    {
        return Link::parse_tail(&value[HTTPS.len()..]).map_or((Shape::Other, None), |token| {
            (Shape::CaseVariantKnownPrefix, Some(token))
        });
    }
    const ENCODED: &str = "https%3A%2F%2Fopen.spotify.com%2Fsocialsession%2F";
    if let Some(prefix) = value.get(..ENCODED.len())
        && prefix.bytes().zip(ENCODED.bytes()).all(|(actual, fixed)| {
            // Only the hexadecimal letters in the fixed escapes vary in case.
            actual == fixed || matches!(fixed, b'A' | b'F') && actual == fixed + 32
        })
    {
        return token_candidate(Shape::PercentEncodedKnownPrefix, &value[ENCODED.len()..]);
    }
    let core = value.trim_matches(' ');
    if core != value {
        return token_candidate(Shape::SpaceWrappedBare, core);
    }
    (Shape::Other, None)
}

fn token_candidate(shape: Shape, token: &str) -> (Shape, Option<&str>) {
    if Link::validate_token(token).is_ok() {
        (shape, Some(token))
    } else {
        (Shape::Other, None)
    }
}

pub(super) fn compare(value: &str, official: &Link) -> Comparison {
    let (shape, token) = candidate(value);
    Comparison {
        shape,
        official_relation: token.map_or(OfficialRelation::NotComparable, |token| {
            if token == official.0 {
                OfficialRelation::Same
            } else {
                OfficialRelation::Different
            }
        }),
    }
}

impl Shape {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::NotObserved => "not_observed",
            Self::Accepted => "accepted",
            Self::BareAsciiAlnum => "bare_ascii_alnum",
            Self::HttpKnownLong => "http_known_long",
            Self::RelativeKnownLong => "relative_known_long",
            Self::SpaceWrappedKnownLong => "space_wrapped_known_long",
            Self::RelativeJoin => "relative_join",
            Self::NetworkRelativeKnown => "network_relative_known",
            Self::CaseVariantKnownPrefix => "case_variant_known_prefix",
            Self::LiteralSlashEscapedKnown => "literal_slash_escaped_known",
            Self::PercentEncodedKnownPrefix => "percent_encoded_known_prefix",
            Self::SpaceWrappedBare => "space_wrapped_bare",
            Self::Other => "other",
        }
    }
}
impl OfficialRelation {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::NotComparable => "not_comparable",
            Self::Same => "same",
            Self::Different => "different",
        }
    }
}

#[cfg(test)]
pub(super) fn template_fixtures(token: &str) -> [(String, Shape); 6] {
    [
        (format!("/sessions/join/{token}"), Shape::RelativeJoin),
        (
            format!("//open.spotify.com/socialsession/{token}?si=Ab_9&utm_source=copy-link"),
            Shape::NetworkRelativeKnown,
        ),
        (
            format!("HTTPS://OPEN.SPOTIFY.COM/SOCIALSESSION/{token}?si=Ab_9"),
            Shape::CaseVariantKnownPrefix,
        ),
        (
            format!(r"https:\/\/open.spotify.com\/socialsession\/{token}?utm_source=copy-link"),
            Shape::LiteralSlashEscapedKnown,
        ),
        (
            format!("https%3a%2f%2Fopen.spotify.com%2fsocialsession%2F{token}"),
            Shape::PercentEncodedKnownPrefix,
        ),
        (format!("  {token} "), Shape::SpaceWrappedBare),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::jam::metadata::Invitation;

    #[test]
    fn official_comparison_closed_forms_do_not_change_acceptance() {
        let official =
            Link::parse("https://open.spotify.com/socialsession/Case9?si=public").unwrap();
        for (value, shape, accepted) in [
            ("Case9", Shape::BareAsciiAlnum, false),
            (
                "http://open.spotify.com/socialsession/Case9?utm_source=x",
                Shape::HttpKnownLong,
                false,
            ),
            ("/socialsession/Case9?si=x", Shape::RelativeKnownLong, false),
            (
                "  https://open.spotify.com/socialsession/Case9?si=x  ",
                Shape::SpaceWrappedKnownLong,
                false,
            ),
            (
                "https://open.spotify.com/socialsession/Case9",
                Shape::Accepted,
                true,
            ),
            ("spotify://sessions/join/Case9", Shape::Accepted, true),
            (
                "https://open.spotify.com/sessions/join/Case9",
                Shape::Accepted,
                true,
            ),
        ] {
            assert_eq!(
                compare(value, &official),
                Comparison {
                    shape,
                    official_relation: OfficialRelation::Same
                }
            );
            assert_eq!(Invitation::parse(value).is_some(), accepted);
        }
        for value in [
            "case9",
            "Case9Suffix",
            "http://open.spotify.com/socialsession/Other9",
            "/socialsession/Other9",
        ] {
            assert_eq!(
                compare(value, &official).official_relation,
                OfficialRelation::Different
            );
        }
    }

    #[test]
    fn official_comparison_never_extracts_or_decodes_unlisted_values() {
        let official = Link::parse("https://open.spotify.com/socialsession/Case9").unwrap();
        for value in [
            "",
            "Case9?si=x",
            "%43ase9",
            "Cаse9",
            "http://open.spotify.com.evil/socialsession/Case9",
            "https://evil.test/socialsession/Case9",
            "junk/socialsession/Case9",
            "HTTP://open.spotify.com/socialsession/Case9",
            "http://open.spotify.com/socialsession/Case9#x",
            "/socialsession/Case9?bad=x",
            "/socialsession/Case9?si=x&si=y",
            "\thttps://open.spotify.com/socialsession/Case9",
            "spotify://sessions/join/Case9?si=x",
            "{\"url\":\"/socialsession/Case9\"}",
        ] {
            assert_eq!(
                compare(value, &official),
                Comparison {
                    shape: Shape::Other,
                    official_relation: OfficialRelation::NotComparable
                }
            );
        }
        for value in ["x".repeat(1025), "x".repeat(2049)] {
            assert_eq!(
                compare(&value, &official).official_relation,
                OfficialRelation::NotComparable
            );
        }
    }

    #[test]
    fn official_six_templates_are_complete_and_never_change_acceptance() {
        let official = Link::parse("https://open.spotify.com/socialsession/Case9").unwrap();
        assert_eq!(
            template_fixtures("Case9").map(|(_, shape)| shape.label()),
            [
                "relative_join",
                "network_relative_known",
                "case_variant_known_prefix",
                "literal_slash_escaped_known",
                "percent_encoded_known_prefix",
                "space_wrapped_bare",
            ]
        );
        for token in ["Case9", "case9", "Different9"] {
            for (value, shape) in template_fixtures(token) {
                assert_eq!(
                    compare(&value, &official),
                    Comparison {
                        shape,
                        official_relation: if token == "Case9" {
                            OfficialRelation::Same
                        } else {
                            OfficialRelation::Different
                        }
                    }
                );
                assert!(Link::parse(&value).is_err());
                assert!(Invitation::parse(&value).is_none());
            }
        }
        for invalid in [
            "",
            "Case9/extra",
            "Case9#frag",
            "%43ase9",
            "Case9\n",
            "Cаse9",
        ] {
            for (value, _) in template_fixtures(invalid) {
                assert_eq!(
                    compare(&value, &official),
                    Comparison {
                        shape: Shape::Other,
                        official_relation: OfficialRelation::NotComparable
                    }
                );
            }
        }
        for (value, _) in template_fixtures(&"x".repeat(1025)) {
            assert_eq!(candidate(&value), (Shape::Other, None));
        }
    }

    #[test]
    fn official_templates_reject_unknown_origins_wrappers_and_queries() {
        for value in [
            "//open.spotify.com.evil/socialsession/Case9",
            "//u@open.spotify.com/socialsession/Case9",
            "//open.spotify.com:443/socialsession/Case9",
            "HTTPS://EVIL.TEST/SOCIALSESSION/Case9",
            "HTTPS://OPEN.SPOTIFY.COM.evil/SOCIALSESSION/Case9",
            "HTTPS://OPEN.SPOTIFY.COM/SOCIALSESSION/Case9?SI=x",
            "HTTPS://OPEN.SPOTIFY.COM/SOCIALSESSION/Case9?si=x&si=y",
            "HTTPS://OPEN.SPOTIFY.COM/SOCIALSESSION/Case9?si=%41",
            "//open.spotify.com/socialsession/Case9?pi=x",
            "//open.spotify.com/socialsession/Case9?si=x&",
            "/sessions/join/Case9?si=x",
            "https%3A%2F%2Fopen.spotify.com%2Fsocialsession%2FCase9?si=x",
            "https%3A%2F%2Fevil.test%2Fsocialsession%2FCase9",
            "https%3A%2F%2FOPEN.spotify.com%2Fsocialsession%2FCase9",
            "https%253A%252F%252Fopen.spotify.com%252Fsocialsession%252FCase9",
            r"https:\/\/evil.test\/socialsession\/Case9",
            r"https:\/\/open.spotify.com\/socialsession\/Case9?pi=x",
            " /sessions/join/Case9 ",
            " //open.spotify.com/socialsession/Case9 ",
            " HTTPS://OPEN.SPOTIFY.COM/SOCIALSESSION/Case9 ",
            " Case9?si=x ",
            "\tCase9\t",
            "\u{a0}Case9\u{a0}",
            "'Case9'",
            "prefixCase9suffix/unknown",
        ] {
            assert_eq!(candidate(value), (Shape::Other, None), "{value}");
            assert!(Link::parse(value).is_err());
            assert!(Invitation::parse(value).is_none());
        }
    }
}
