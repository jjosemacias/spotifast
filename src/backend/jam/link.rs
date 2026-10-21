//! Long-link validation adapted from the local librespot Jam experiment at
//! 627b5f0 (core/src/jam.rs). No resolver, decoding or diagnostic token output.
use super::Error;

#[derive(Clone, PartialEq, Eq)]
pub struct Link(pub(super) String);

impl Link {
    pub fn parse(input: &str) -> Result<Self, Error> {
        if input.len() > 2048 {
            return Err(Error::InvalidLink);
        }
        let tail = input
            .strip_prefix("https://open.spotify.com/socialsession/")
            .ok_or(Error::InvalidLink)?;
        let (token, query) = tail
            .split_once('?')
            .map_or((tail, None), |(t, q)| (t, Some(q)));
        if token.is_empty()
            || token.len() > 1024
            || !token.bytes().all(|b| b.is_ascii_alphanumeric())
        {
            return Err(Error::InvalidLink);
        }
        if let Some(query) = query {
            let (mut si, mut source) = (false, false);
            for pair in query.split('&') {
                let (key, value) = pair.split_once('=').ok_or(Error::InvalidLink)?;
                match key {
                    "si" if !si => si = true,
                    "utm_source" if !source => source = true,
                    _ => return Err(Error::InvalidLink),
                }
                if value.is_empty()
                    || value.len() > 256
                    || !value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
                {
                    return Err(Error::InvalidLink);
                }
            }
        }
        Ok(Self(token.to_owned()))
    }
}
