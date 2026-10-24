//! Bounded, owned diagnostics only. Never format or retain the original error.
use std::error::Error as StdError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HyperContext {
    Parse,
    User,
    Canceled,
    Closed,
    Incomplete,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum H2Origin {
    Remote,
    Library,
    Unknown,
}
/// Only standard HTTP/2 reason codes, never a stream ID or extension payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct H2Reason(u8);
impl H2Reason {
    pub fn from_standard(code: u32) -> Option<Self> {
        (code <= 13).then_some(Self(code as u8))
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoKind {
    ConnectionRefused,
    ConnectionReset,
    ConnectionAborted,
    NotConnected,
    BrokenPipe,
    UnexpectedEof,
    TimedOut,
    PermissionDenied,
    Other,
}
impl From<std::io::ErrorKind> for IoKind {
    fn from(kind: std::io::ErrorKind) -> Self {
        use std::io::ErrorKind as K;
        match kind {
            K::ConnectionRefused => Self::ConnectionRefused,
            K::ConnectionReset => Self::ConnectionReset,
            K::ConnectionAborted => Self::ConnectionAborted,
            K::NotConnected => Self::NotConnected,
            K::BrokenPipe => Self::BrokenPipe,
            K::UnexpectedEof => Self::UnexpectedEof,
            K::TimedOut => Self::TimedOut,
            K::PermissionDenied => Self::PermissionDenied,
            _ => Self::Other,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TlsKind {
    UnknownIssuer,
    NameRejected,
    CertificateRejected,
    Other,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FutureCause {
    H2Reset(H2Reason, H2Origin),
    H2GoAway(H2Reason, H2Origin),
    H2Reason(H2Reason),
    Io(IoKind),
    Tls(TlsKind),
    Opaque,
}
/// This value cannot contain source text, identities, network data or HTTP status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FutureFailure {
    pub hyper: Option<HyperContext>,
    pub cause: FutureCause,
}
impl FutureFailure {
    pub const OPAQUE: Self = Self {
        hyper: None,
        cause: FutureCause::Opaque,
    };
}

pub(super) fn project(error: &hyper_util::client::legacy::Error) -> FutureFailure {
    classify(error)
}

fn classify(mut node: &(dyn StdError + 'static)) -> FutureFailure {
    let mut hyper = None;
    let mut io_fallback = None;
    for _ in 0..8 {
        if let Some(error) = node.downcast_ref::<hyper_util::client::legacy::Error>() {
            match error.source() {
                Some(source) => {
                    node = source;
                    continue;
                }
                None => break,
            }
        }
        if let Some(error) = node.downcast_ref::<hyper::Error>() {
            if hyper.is_none() {
                // is_timeout walks arbitrary source internally: deliberately not used.
                hyper = if error.is_parse() {
                    Some(HyperContext::Parse)
                } else if error.is_user() {
                    Some(HyperContext::User)
                } else if error.is_incomplete_message() {
                    Some(HyperContext::Incomplete)
                } else if error.is_closed() {
                    Some(HyperContext::Closed)
                } else if error.is_canceled() {
                    Some(HyperContext::Canceled)
                } else {
                    None
                };
            }
            match error.source() {
                Some(source) => {
                    node = source;
                    continue;
                }
                None => break,
            }
        }
        if let Some(error) = node.downcast_ref::<h2::Error>() {
            if let Some(reason) = error.reason() {
                let Some(reason) = H2Reason::from_standard(u32::from(reason)) else {
                    break;
                };
                let origin = if error.is_remote() {
                    H2Origin::Remote
                } else if error.is_library() {
                    H2Origin::Library
                } else {
                    H2Origin::Unknown
                };
                let cause = if error.is_reset() {
                    FutureCause::H2Reset(reason, origin)
                } else if error.is_go_away() {
                    FutureCause::H2GoAway(reason, origin)
                } else {
                    FutureCause::H2Reason(reason)
                };
                return FutureFailure { hyper, cause };
            }
            // h2::Error::source is empty. Hyper may also flatten this to IO itself.
            if let Some(error) = error.get_io() {
                node = error;
                continue;
            }
            break;
        }
        if let Some(error) = node.downcast_ref::<rustls::Error>() {
            let kind = match error {
                rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer) => {
                    TlsKind::UnknownIssuer
                }
                rustls::Error::InvalidCertificate(
                    rustls::CertificateError::NotValidForName
                    | rustls::CertificateError::NotValidForNameContext { .. },
                ) => TlsKind::NameRejected,
                rustls::Error::InvalidCertificate(_) => TlsKind::CertificateRejected,
                _ => TlsKind::Other,
            };
            return FutureFailure {
                hyper,
                cause: FutureCause::Tls(kind),
            };
        }
        if let Some(error) = node.downcast_ref::<std::io::Error>() {
            io_fallback = Some(IoKind::from(error.kind()));
            if let Some(inner) = error.get_ref() {
                node = inner;
                continue;
            }
            break;
        }
        break; // Never invoke source(), Debug or Display on an unknown type.
    }
    FutureFailure {
        hyper,
        cause: io_fallback.map_or(FutureCause::Opaque, FutureCause::Io),
    }
}

#[cfg(test)]
mod tests;
