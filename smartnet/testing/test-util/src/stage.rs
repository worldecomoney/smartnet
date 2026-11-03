//! Classify `validationError` strings into probe stages (shape discovery only).

/// Coarse validation stage for fuzz output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ValidationStage {
    Accepted,
    RlpDecode,
    MalformedPayload,
    ExtraData,
    PostMerge,
    SealSignature,
    SealOther,
    Execution,
    #[allow(dead_code)]
    RpcError,
    Other,
}

impl ValidationStage {
    pub(crate) fn classify(status: &str, validation_error: Option<&str>) -> Self {
        if status == "VALID" {
            return Self::Accepted;
        }
        let err = validation_error.unwrap_or("").to_lowercase();
        if err.is_empty() && status == "INVALID" {
            return Self::Other;
        }
        if err.contains("unexpected string") ||
            err.contains("rlp") ||
            err.contains("decode") ||
            err.contains("failed to decode")
        {
            return Self::RlpDecode;
        }
        if err.contains("extra data") {
            return Self::ExtraData;
        }
        if err.contains("base fee") ||
            err.contains("withdrawal") ||
            err.contains("blob") ||
            err.contains("beacon") ||
            err.contains("requests") ||
            err.contains("terminal") ||
            err.contains("difficulty is not zero") ||
            err.contains("nonce") && err.contains("merge")
        {
            return Self::PostMerge;
        }
        if err.contains("signature") ||
            err.contains("seal") ||
            err.contains("signer") ||
            err.contains("recover")
        {
            return Self::SealSignature;
        }
        if err.contains("turn") || err.contains("difficulty") {
            return Self::SealOther;
        }
        if err.contains("state") ||
            err.contains("root") ||
            err.contains("receipt") ||
            err.contains("transaction") ||
            err.contains("gas") ||
            err.contains("execution")
        {
            return Self::Execution;
        }
        if err.contains("block hash") || err.contains("hash") {
            return Self::MalformedPayload;
        }
        Self::Other
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::RlpDecode => "rlp_decode",
            Self::MalformedPayload => "malformed_payload",
            Self::ExtraData => "extra_data",
            Self::PostMerge => "post_merge",
            Self::SealSignature => "seal_signature",
            Self::SealOther => "seal_other",
            Self::Execution => "execution",
            Self::RpcError => "rpc_error",
            Self::Other => "other",
        }
    }

    /// Deeper than extra-data rejection → payload envelope likely accepted.
    #[allow(dead_code)]
    pub(crate) const fn is_deeper_than_extra_data(self) -> bool {
        matches!(
            self,
            Self::Accepted |
                Self::SealSignature |
                Self::SealOther |
                Self::Execution |
                Self::PostMerge |
                Self::MalformedPayload
        )
    }
}
