//! Presentation only. No identity here confers membership, authority or freshness.
//! Field provenance: social_connect_v2.proto at 4118319; J02-05, row R02.
use serde_json::Value;

use super::diagnostic::{Probe, UrlField};
use super::{Error, Link, projection};

/// No Debug/Serialize: names, identities and invitation tokens stay ephemeral.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Metadata {
    pub owner: Option<String>,
    /// None is omitted/unknown; Some(empty) is an explicitly supplied empty list.
    /// Even a supplied list has no demonstrated completeness guarantee.
    pub members: Option<Vec<Member>>,
    pub invitation: Option<Invitation>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Member {
    pub id: Option<String>,
    pub username: Option<String>,
    pub name: Option<String>,
    pub image: Option<String>,
    pub listening: Option<bool>,
    pub controlling: Option<bool>,
}

impl Member {
    pub fn named_by(&self, id: &str) -> bool {
        self.id.as_deref() == Some(id) || self.username.as_deref() == Some(id)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Invitation(String);

impl Invitation {
    /// Exact known forms only. Never extract a marker from an arbitrary origin.
    pub fn parse(value: &str) -> Option<Self> {
        if value.len() > 2048 {
            return None;
        }
        // PR935 at 5fca13ce also supplies this exact HTTPS join path in its
        // Session fixture. Normalize metadata only; Join still takes long links.
        let input = if let Some(token) = value
            .strip_prefix("spotify://sessions/join/")
            .or_else(|| value.strip_prefix("https://open.spotify.com/sessions/join/"))
        {
            if token.is_empty()
                || token.len() > 1024
                || !token.bytes().all(|b| b.is_ascii_alphanumeric())
            {
                return None;
            }
            format!("https://open.spotify.com/socialsession/{token}")
        } else {
            value.to_owned()
        };
        Link::parse(&input).ok()?;
        Some(Self(input))
    }
    pub fn url(&self) -> &str {
        &self.0
    }
}

fn field<'a>(value: &'a Value, snake: &str, camel: &str) -> Result<Option<&'a Value>, Error> {
    if snake != camel && value.get(snake).is_some() && value.get(camel).is_some() {
        return Err(Error::InvalidResponse);
    }
    Ok(value.get(snake).or_else(|| value.get(camel)))
}

fn string(value: &Value, snake: &str, camel: &str, max: usize) -> Result<Option<String>, Error> {
    string_observed(value, snake, camel, max, None)
}

/// Only the invitation field supplies a probe. This is the same string parser
/// for observed and ordinary snapshots, not a second pass after a failure.
fn string_observed(
    value: &Value,
    snake: &str,
    camel: &str,
    max: usize,
    diagnostic: Option<&Probe>,
) -> Result<Option<String>, Error> {
    let observe = |class| {
        if let Some(probe) = diagnostic {
            probe.url_field(class);
        }
    };
    let Some(value) = field(value, snake, camel).inspect_err(|_| {
        observe(UrlField::AliasConflict);
    })?
    else {
        observe(UrlField::Absent);
        return Ok(None);
    };
    let s = value.as_str().ok_or_else(|| {
        observe(UrlField::TypeRejected);
        Error::InvalidResponse
    })?;
    if s.len() > max || s.chars().any(char::is_control) {
        observe(UrlField::BoundsOrControlRejected);
        return Err(Error::InvalidResponse);
    }
    if s.trim().is_empty() {
        observe(UrlField::Blank);
        Ok(None)
    } else {
        Ok(Some(s.to_owned()))
    }
}

fn flag(value: &Value, snake: &str, camel: &str) -> Result<Option<bool>, Error> {
    field(value, snake, camel)?
        .map(|v| v.as_bool().ok_or(Error::InvalidResponse))
        .transpose()
}

/// Reuse the artwork mechanism only for bounded Spotify CDN image identifiers.
/// Arbitrary origins, credentials, query strings and identity-bearing paths fail closed.
fn image(value: &str) -> Option<String> {
    let id = value
        .strip_prefix("spotify:image:")
        .or_else(|| value.strip_prefix("https://i.scdn.co/image/"))?;
    (id.len() == 40 && id.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| format!("https://i.scdn.co/image/{id}"))
}

fn members(value: &Value, snake: &str, camel: &str) -> Result<Option<Vec<Member>>, Error> {
    let Some(values) = field(value, snake, camel)? else {
        return Ok(None);
    };
    let values = values.as_array().ok_or(Error::InvalidResponse)?;
    if values.len() > 100 {
        return Err(Error::InvalidResponse);
    }
    let mut result: Vec<Member> = Vec::new();
    for value in values {
        if !value.is_object() {
            return Err(Error::InvalidResponse);
        }
        let id = string(value, "id", "id", 256)?;
        let username = string(value, "username", "username", 256)?;
        if id.is_none() && username.is_none() {
            return Err(Error::InvalidResponse);
        }
        // Also reject aliases that would make one row match two identities.
        if result
            .iter()
            .any(|m| id.iter().chain(username.iter()).any(|id| m.named_by(id)))
        {
            return Err(Error::InvalidResponse);
        }
        let small = string(value, "image_url", "imageUrl", 2048)?;
        let large = string(value, "large_image_url", "largeImageUrl", 2048)?;
        result.push(Member {
            id,
            username,
            name: string(value, "display_name", "displayName", 512)?,
            image: small
                .as_deref()
                .and_then(image)
                .or_else(|| large.as_deref().and_then(image)),
            listening: flag(value, "is_listening", "isListening")?,
            controlling: flag(value, "is_controlling", "isControlling")?,
        });
    }
    Ok(Some(result))
}

pub(super) fn snapshot(value: &Value) -> Result<Metadata, Error> {
    snapshot_observed(value, None)
}

pub(super) fn snapshot_observed(
    value: &Value,
    diagnostic: Option<&Probe>,
) -> Result<Metadata, Error> {
    let metadata = Metadata {
        owner: string(value, "session_owner_id", "sessionOwnerId", 256)?,
        members: members(value, "session_members", "sessionMembers")?,
        invitation: string_observed(
            value,
            "join_session_url",
            "joinSessionUrl",
            2048,
            diagnostic,
        )?
        .as_deref()
        .and_then(|value| {
            let invitation = Invitation::parse(value);
            if let Some(probe) = diagnostic {
                probe.url_field(if invitation.is_some() {
                    UrlField::Accepted
                } else {
                    UrlField::FormatRejected
                });
            }
            invitation
        }),
    };
    if let (Some(owner), Some(members)) = (&metadata.owner, &metadata.members)
        && members.iter().filter(|m| m.named_by(owner)).count() > 1
    {
        return Err(Error::InvalidResponse);
    }
    Ok(metadata)
}

/// Dealer is a delta envelope, never the four-field HTTP profile. Its ordering
/// is unproven: decoding does not apply it or refresh Current. Every notification
/// still retires the binding for a deliberate Current (including malformed data).
pub(super) fn dealer(body: &[u8]) -> Result<(), Error> {
    let root = projection::json(body)?;
    string(&root, "reason", "reason", 128)?.ok_or(Error::InvalidResponse)?;
    if let Some(session) = root.get("session") {
        if !session.is_object() {
            return Err(Error::InvalidResponse);
        }
        string(session, "session_id", "sessionId", 2048)?.ok_or(Error::InvalidResponse)?;
        snapshot(session)?;
    }
    // Omitted delta members are unknown, never an instruction to empty a list.
    members(&root, "updated_session_members", "updatedSessionMembers")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(body: &str) -> Result<Metadata, Error> {
        snapshot(&projection::json(body.as_bytes())?)
    }

    #[test]
    fn omission_explicit_empty_and_partial_members_are_distinct() {
        assert!(parse("{}").unwrap().members.is_none());
        assert!(
            parse(r#"{"session_members":[]}"#)
                .unwrap()
                .members
                .unwrap()
                .is_empty()
        );
        let data = parse(r#"{"sessionOwnerId":"u","sessionMembers":[{"id":"u","displayName":"Host","isListening":false}]}"#).unwrap();
        let member = &data.members.as_ref().unwrap()[0];
        assert!(member.named_by(data.owner.as_deref().unwrap()));
        assert_eq!(member.listening, Some(false));
        assert!(member.controlling.is_none());
        assert!(data.invitation.is_none());
    }

    #[test]
    fn metadata_rejects_types_aliases_duplicate_identities_and_budgets() {
        for body in [
            r#"{"session_members":null}"#,
            r#"{"session_members":{}}"#,
            r#"{"session_members":[{}]}"#,
            r#"{"session_members":[{"id":"u","is_listening":0}]}"#,
            r#"{"session_members":[{"id":"u"},{"username":"u"}]}"#,
            r#"{"session_owner_id":"u","sessionOwnerId":"u"}"#,
            r#"{"session_members":[{"id":"u","display_name":"one","displayName":"two"}]}"#,
            r#"{"unknown":{"x":1,"x":2}}"#,
            r#"{"join_session_url":true}"#,
        ] {
            assert!(parse(body).is_err(), "accepted malformed fixture");
        }
        let rows = serde_json::json!({"session_members": (0..101).map(|n| serde_json::json!({"id": n.to_string()})).collect::<Vec<_>>()});
        assert!(snapshot(&rows).is_err());
        assert!(snapshot(&serde_json::json!({"session_owner_id":"x".repeat(257)})).is_err());
        assert!(
            snapshot(
                &serde_json::json!({"session_members":[{"id":"u", "display_name":"x".repeat(513)}]})
            )
            .is_err()
        );
        assert!(
            parse(&format!(
                "{{\"ignored\":{}}}",
                "[".repeat(33) + "0" + &"]".repeat(33)
            ))
            .is_err()
        );
    }

    #[test]
    fn invitation_and_avatar_origins_are_exact_and_bounded() {
        for url in [
            "https://evil.test/socialsession/Tok9",
            "https://open.spotify.com.evil/socialsession/Tok9",
            "spotify://sessions/join/Tok9?x=1",
            "https://open.spotify.com/socialsession/Tok9#x",
            "https://open.spotify.com/socialsession/Tok9?si=x&si=y",
            "https://spotify.link/Tok9",
            "https://open.spotify.com/socialsession/%54ok9",
        ] {
            assert!(Invitation::parse(url).is_none());
        }
        assert!(
            Invitation::parse("https://open.spotify.com/socialsession/Tok9?si=x")
                .unwrap()
                .url()
                .ends_with("?si=x")
        );
        assert_eq!(
            Invitation::parse("spotify://sessions/join/Tok9")
                .unwrap()
                .url(),
            "https://open.spotify.com/socialsession/Tok9"
        );
        assert!(image("https://evil.test/avatar").is_none());
        assert!(
            image("https://i.scdn.co/image/0123456789012345678901234567890123456789?user=u")
                .is_none()
        );
        assert!(image("spotify:image:0123456789012345678901234567890123456789").is_some());
        // Bad URLs are unavailable without weakening required field parsing.
        assert!(
            parse(r#"{"join_session_url":"https://evil.test/"}"#)
                .unwrap()
                .invitation
                .is_none()
        );
    }

    #[test]
    fn https_join_address_normalizes_only_exact_origin_path_and_alphanumeric_token() {
        let address = "https://open.spotify.com/sessions/join/Tok9";
        let invitation = Invitation::parse(address).unwrap();
        assert_eq!(
            invitation.url(),
            "https://open.spotify.com/socialsession/Tok9"
        );
        // This is a metadata normalization, not another accepted Join input.
        assert_eq!(Link::parse(address).err(), Some(Error::InvalidLink));
        assert!(Link::parse(invitation.url()).is_ok());
        for address in [
            "http://open.spotify.com/sessions/join/Tok9",
            "https://evil.test/sessions/join/Tok9",
            "https://open.spotify.com.evil/sessions/join/Tok9",
            "https://user@open.spotify.com/sessions/join/Tok9",
            "https://open.spotify.com:443/sessions/join/Tok9",
            "https://OPEN.spotify.com/sessions/join/Tok9",
            "https://open.spotify.com/Sessions/join/Tok9",
            "https://open.spotify.com/x/../sessions/join/Tok9",
            "https://open.spotify.com//sessions/join/Tok9",
            "https://spclient/sessions/join/Tok9",
            "https://open.spotify.com/sessions/join/",
            "https://open.spotify.com/sessions/join/Tok9/",
            "https://open.spotify.com/sessions/join/Tok9?si=x",
            "https://open.spotify.com/sessions/join/Tok9?utm_source=x",
            "https://open.spotify.com/sessions/join/Tok9?unknown=x",
            "https://open.spotify.com/sessions/join/Tok9#x",
            "https://open.spotify.com/sessions/join/Tok_9",
            "https://open.spotify.com/sessions/join/Tok-9",
            "https://open.spotify.com/sessions/join/%54ok9",
            "https://open.spotify.com/sessions/join/Toké",
            " https://open.spotify.com/sessions/join/Tok9",
            "https://open.spotify.com/sessions/join/Tok9 ",
            "https://open.spotify.com/sessions/join/Tok\n9",
            r#"{"url":"https://open.spotify.com/sessions/join/Tok9"}"#,
        ] {
            assert!(Invitation::parse(address).is_none());
        }
        for (length, valid) in [(1024, true), (1025, false), (2049, false)] {
            let address = format!(
                "https://open.spotify.com/sessions/join/{}",
                "A".repeat(length)
            );
            assert_eq!(Invitation::parse(&address).is_some(), valid);
        }
    }

    #[test]
    fn dealer_envelope_never_uses_the_http_four_field_profile() {
        let no_members =
            br#"{"reason":"SESSION_MEMBER_PAUSE_CHANGED","session":{"session_id":"s"}}"#;
        assert!(dealer(no_members).is_ok());
        assert!(projection::read(no_members).is_err());
        for body in [
            br#"{"reason":true}"#.as_slice(),
            br#"{"reason":"USER_JOINED","session_members":[],"updated_session_members":null}"#,
            br#"{"reason":"USER_JOINED","session":{"session_id":"s","sessionId":"s"}}"#,
            br#"{"reason":"USER_LEFT","updated_session_members":[{"id":"u"},{"id":"u"}]}"#,
        ] {
            assert!(dealer(body).is_err());
        }
        // Unknown reasons remain notifications requiring reconciliation, not authority.
        assert!(dealer(br#"{"reason":"FUTURE_REASON"}"#).is_ok());
    }
}
