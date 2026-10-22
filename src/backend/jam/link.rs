//! Long-link validation adapted from the local librespot Jam experiment at
//! 627b5f0 (core/src/jam.rs). No resolver, decoding or diagnostic token output.
use super::Error;
use super::diagnostic::FormatBoundary;

#[derive(Clone, PartialEq, Eq)]
pub struct Link(pub(super) String);

impl Link {
    pub fn parse(input: &str) -> Result<Self, Error> {
        Self::parse_classified(input).map_err(|_| Error::InvalidLink)
    }

    pub(super) fn parse_classified(input: &str) -> Result<Self, FormatBoundary> {
        if input.len() > 2048 {
            // The metadata string limit rejects this before format parsing.
            return Err(FormatBoundary::NotApplicable);
        }
        let tail = input
            .strip_prefix("https://open.spotify.com/socialsession/")
            .ok_or_else(|| {
                if input.starts_with("https://open.spotify.com/") {
                    FormatBoundary::PrefixOpenOtherPath
                } else if input.starts_with("spotify:") {
                    FormatBoundary::PrefixSpotifyOther
                } else if input.starts_with("https://") {
                    FormatBoundary::PrefixHttpsOther
                } else if matches!(
                    input.trim_start().as_bytes().first(),
                    Some(b'{' | b'[' | b'"')
                ) {
                    FormatBoundary::PrefixStructured
                } else {
                    FormatBoundary::PrefixOther
                }
            })?;
        let (token, query) = tail
            .split_once('?')
            .map_or((tail, None), |(t, q)| (t, Some(q)));
        Self::validate_token(token)?;
        if let Some(query) = query {
            let (mut si, mut source) = (false, false);
            for pair in query.split('&') {
                let (key, value) = pair.split_once('=').ok_or(FormatBoundary::QueryShape)?;
                match key {
                    "si" if !si => si = true,
                    "utm_source" if !source => source = true,
                    _ => return Err(FormatBoundary::QueryKeyOrDuplicate),
                }
                if value.is_empty()
                    || value.len() > 256
                    || !value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
                {
                    return Err(FormatBoundary::QueryValueRejected);
                }
            }
        }
        Ok(Self(token.to_owned()))
    }

    pub(super) fn validate_token(token: &str) -> Result<(), FormatBoundary> {
        if token.is_empty() || token.len() > 1024 {
            return Err(FormatBoundary::TokenEmptyOrLimit);
        }
        // Classify only the first rejected byte, never its value or position.
        if let Some(byte) = token.bytes().find(|b| !b.is_ascii_alphanumeric()) {
            return Err(match byte {
                b'{' | b'}' | b'<' | b'>' => FormatBoundary::TokenPlaceholderMarker,
                b'?' => FormatBoundary::TokenQuery,
                b'#' => FormatBoundary::TokenFragment,
                b'%' => FormatBoundary::TokenPercentMarker,
                _ => FormatBoundary::TokenOtherCharacters,
            });
        }
        Ok(())
    }
}
