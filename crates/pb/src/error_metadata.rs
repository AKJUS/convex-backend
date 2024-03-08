use anyhow::Context;
use errors::{
    ErrorCode,
    ErrorMetadata,
};
use prost::Message;

use crate::errors::{
    ErrorCode as ErrorCodeProto,
    ErrorMetadata as ErrorMetadataProto,
    StatusDetails as StatusDetailsProto,
};

impl From<ErrorCode> for ErrorCodeProto {
    fn from(code: ErrorCode) -> Self {
        match code {
            ErrorCode::BadRequest => ErrorCodeProto::BadRequest,
            ErrorCode::Unauthenticated => ErrorCodeProto::Unauthenticated,
            ErrorCode::Forbidden => ErrorCodeProto::Forbidden,
            ErrorCode::NotFound => ErrorCodeProto::NotFound,
            ErrorCode::ClientDisconnect => ErrorCodeProto::ClientDisconnect,
            ErrorCode::Overloaded => ErrorCodeProto::Overloaded,
            ErrorCode::OCC => ErrorCodeProto::Occ,
            ErrorCode::PaginationLimit => ErrorCodeProto::PaginationLimit,
            ErrorCode::OutOfRetention => ErrorCodeProto::OutOfRetention,
            ErrorCode::OperationalInternalServerError => {
                ErrorCodeProto::OperationalInternalServerError
            },
        }
    }
}

impl From<ErrorCodeProto> for ErrorCode {
    fn from(code: ErrorCodeProto) -> Self {
        match code {
            ErrorCodeProto::BadRequest => ErrorCode::BadRequest,
            ErrorCodeProto::Unauthenticated => ErrorCode::Unauthenticated,
            ErrorCodeProto::Forbidden => ErrorCode::Forbidden,
            ErrorCodeProto::NotFound => ErrorCode::NotFound,
            ErrorCodeProto::ClientDisconnect => ErrorCode::ClientDisconnect,
            ErrorCodeProto::Overloaded => ErrorCode::Overloaded,
            ErrorCodeProto::Occ => ErrorCode::OCC,
            ErrorCodeProto::PaginationLimit => ErrorCode::PaginationLimit,
            ErrorCodeProto::OutOfRetention => ErrorCode::OutOfRetention,
            ErrorCodeProto::OperationalInternalServerError => {
                ErrorCode::OperationalInternalServerError
            },
        }
    }
}

impl From<ErrorMetadata> for ErrorMetadataProto {
    fn from(metadata: ErrorMetadata) -> Self {
        ErrorMetadataProto {
            code: ErrorCodeProto::from(metadata.code).into(),
            short_msg: Some(metadata.short_msg.to_string()),
            msg: Some(metadata.msg.to_string()),
        }
    }
}

impl TryFrom<ErrorMetadataProto> for ErrorMetadata {
    type Error = anyhow::Error;

    fn try_from(metadata: ErrorMetadataProto) -> anyhow::Result<Self> {
        let code = ErrorCodeProto::try_from(metadata.code)?.into();
        let short_msg = metadata.short_msg.context("Missing `short_msg` field")?;
        let msg = metadata.msg.context("Missing `msg` field")?;
        Ok(Self {
            code,
            short_msg: short_msg.into(),
            msg: msg.into(),
        })
    }
}

pub trait ErrorMetadataStatusExt {
    fn from_anyhow(error: anyhow::Error) -> Self;
    fn into_anyhow(self) -> anyhow::Error;
}

impl ErrorMetadataStatusExt for tonic::Status {
    fn from_anyhow(error: anyhow::Error) -> Self {
        let message = format!("{error}");
        if let Some(metadata) = error.downcast_ref::<ErrorMetadata>().cloned() {
            let code: tonic::Code = metadata.code.grpc_status_code();
            let details = StatusDetailsProto {
                error_metadata: Some(metadata.into()),
            };
            tonic::Status::with_details(code, message, details.encode_to_vec().into())
        } else {
            tonic::Status::internal(message)
        }
    }

    fn into_anyhow(self) -> anyhow::Error {
        let details = match StatusDetailsProto::decode(self.details()) {
            Ok(details) => details,
            Err(err) => {
                return anyhow::anyhow!("Failed to decode StatusDetails proto: {}", err);
            },
        };
        let mut error: anyhow::Error = self.into();
        if let Some(error_metadata) = details.error_metadata {
            let error_metadata = match ErrorMetadata::try_from(error_metadata) {
                Ok(error_metadata) => error_metadata,
                Err(err) => return err.context("Failed to parse ErrorMetadata proto"),
            };
            error = error.context(error_metadata)
        }
        error
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use value::testing::assert_roundtrips;

    use super::ErrorMetadata;
    use crate::{
        error_metadata::ErrorMetadataStatusExt,
        errors::ErrorMetadata as ErrorMetadataProto,
    };

    proptest! {
        #![proptest_config(
            ProptestConfig { failure_persistence: None, ..ProptestConfig::default() }
        )]

        #[test]
        fn test_error_metadata_roundtrips(left in any::<ErrorMetadata>()) {
            assert_roundtrips::<ErrorMetadata, ErrorMetadataProto>(left);
        }

        #[test]
        fn test_status_propagates_metadata(original_metadata in any::<ErrorMetadata>()) {
            let status = tonic::Status::from_anyhow(anyhow::anyhow!("Error").context(original_metadata.clone()));
            let error = status.into_anyhow();
            if let Some(received_metadata) = error.downcast_ref::<ErrorMetadata>() {
                assert_eq!(*received_metadata, original_metadata);
            } else {
                panic!("Didn't propagate error_metadata via Status");
            }
        }
    }

    #[test]
    fn test_status_no_error_metadata() {
        let status = tonic::Status::from_anyhow(anyhow::anyhow!("Error"));
        // Empty status details should parse as zero bytes.
        assert!(status.details().is_empty());

        // We should have no ErrorMetadata in the context.
        let error = status.into_anyhow();
        assert!(error.downcast_ref::<ErrorMetadata>().is_none());
    }
}
