//! Errors of the CSI services, mapped onto gRPC status codes as the CSI
//! specification asks (design §14.2).

/// A CSI call's failure. Each variant is the gRPC code the CO sees; the
/// message says what to do about it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// A missing or malformed argument (`INVALID_ARGUMENT`).
    #[error("{0}")]
    InvalidArgument(String),
    /// The volume, node or path does not exist (`NOT_FOUND`).
    #[error("{0}")]
    NotFound(String),
    /// The volume exists with other properties (`ALREADY_EXISTS`).
    #[error("{0}")]
    AlreadyExists(String),
    /// The call cannot succeed in the current state (`FAILED_PRECONDITION`).
    #[error("{0}")]
    FailedPrecondition(String),
    /// Another call on the same object got in first (`ABORTED`).
    #[error("{0}")]
    Aborted(String),
    /// Not ready yet; the CO retries (`UNAVAILABLE`).
    #[error("{0}")]
    Unavailable(String),
    /// The request cannot be met here, such as a topology that excludes
    /// this node (`RESOURCE_EXHAUSTED`).
    #[error("{0}")]
    ResourceExhausted(String),
    /// Anything else (`INTERNAL`).
    #[error("{0}")]
    Internal(String),
}

impl Error {
    /// An `INTERNAL` error for an I/O failure, naming what was being done.
    pub fn io(what: impl std::fmt::Display, e: impl std::fmt::Display) -> Self {
        Self::Internal(format!("{what}: {e}"))
    }
}

impl From<Error> for tonic::Status {
    fn from(e: Error) -> Self {
        match e {
            Error::InvalidArgument(m) => tonic::Status::invalid_argument(m),
            Error::NotFound(m) => tonic::Status::not_found(m),
            Error::AlreadyExists(m) => tonic::Status::already_exists(m),
            Error::FailedPrecondition(m) => tonic::Status::failed_precondition(m),
            Error::Aborted(m) => tonic::Status::aborted(m),
            Error::Unavailable(m) => tonic::Status::unavailable(m),
            Error::ResourceExhausted(m) => tonic::Status::resource_exhausted(m),
            Error::Internal(m) => tonic::Status::internal(m),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_map_to_their_codes() {
        let cases = [
            (
                Error::InvalidArgument("a".into()),
                tonic::Code::InvalidArgument,
            ),
            (Error::NotFound("a".into()), tonic::Code::NotFound),
            (Error::AlreadyExists("a".into()), tonic::Code::AlreadyExists),
            (
                Error::FailedPrecondition("a".into()),
                tonic::Code::FailedPrecondition,
            ),
            (Error::Aborted("a".into()), tonic::Code::Aborted),
            (Error::Unavailable("a".into()), tonic::Code::Unavailable),
            (
                Error::ResourceExhausted("a".into()),
                tonic::Code::ResourceExhausted,
            ),
            (Error::Internal("a".into()), tonic::Code::Internal),
        ];
        for (error, code) in cases {
            let status = tonic::Status::from(error);
            assert_eq!(status.code(), code);
            assert_eq!(status.message(), "a");
        }
        assert_eq!(
            Error::io("reading x", "gone").to_string(),
            "reading x: gone"
        );
    }
}
