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
    (Shape::Other, None)
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
            "Case9 ",
            " Case9",
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
}
