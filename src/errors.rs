use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    RateLimited,
    Network,
    ShallowUnsupported,
    Fatal,
}

#[derive(Debug)]
pub enum RgcError {
    RateLimited(String),
    Retryable(String),
    ShallowUnsupported(String),
    Fatal(String),
}

impl RgcError {
    pub fn kind(&self) -> FailureKind {
        match self {
            RgcError::RateLimited(_) => FailureKind::RateLimited,
            RgcError::Retryable(_) => FailureKind::Network,
            RgcError::ShallowUnsupported(_) => FailureKind::ShallowUnsupported,
            RgcError::Fatal(_) => FailureKind::Fatal,
        }
    }

    pub fn message(&self) -> &str {
        match self {
            RgcError::RateLimited(m)
            | RgcError::Retryable(m)
            | RgcError::ShallowUnsupported(m)
            | RgcError::Fatal(m) => m,
        }
    }
}

impl fmt::Display for RgcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{:?}] {}", self.kind(), self.message())
    }
}

impl std::error::Error for RgcError {}

/// 从 anyhow::Error 提取失败类别（无法识别 → Fatal）
pub fn kind_of(err: &anyhow::Error) -> FailureKind {
    err.downcast_ref::<RgcError>().map(|e| e.kind()).unwrap_or(FailureKind::Fatal)
}

/// 按 git stderr 关键词分类。429/限流单独一类（不计片重试、触发全局降速）。
pub fn classify(stderr: &str) -> FailureKind {
    let s = stderr.to_lowercase();
    if s.contains("429") || s.contains("rate limit") || s.contains("secondary rate") || s.contains("abuse") {
        FailureKind::RateLimited
    } else if ["could not resolve host", "connection reset", "connection refused", "timed out", "early eof", "rpc failed", "unable to access", "temporary failure"]
        .iter()
        .any(|k| s.contains(k))
    {
        FailureKind::Network
    } else if s.contains("shallow") && (s.contains("not supported") || s.contains("dumb http")) {
        FailureKind::ShallowUnsupported
    } else {
        FailureKind::Fatal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_network_error() {
        let msg = "fatal: unable to access 'https://github.com/x/y/': Could not resolve host: github.com";
        assert_eq!(classify(msg), FailureKind::Network);
        assert_eq!(classify("error: RPC failed; curl 56 Recv failure: Connection reset by peer"), FailureKind::Network);
    }

    #[test]
    fn classify_rate_limit() {
        assert_eq!(classify("error: RPC failed; The requested URL returned error: 429"), FailureKind::RateLimited);
        assert_eq!(classify("remote: abuse detection triggered"), FailureKind::RateLimited);
    }

    #[test]
    fn classify_shallow_unsupported() {
        assert_eq!(classify("fatal: shallow fetch is not supported over dumb http"), FailureKind::ShallowUnsupported);
    }

    #[test]
    fn classify_fatal_by_default() {
        assert_eq!(classify("fatal: Authentication failed"), FailureKind::Fatal);
    }

    #[test]
    fn kind_of_wraps_through_anyhow() {
        let e: anyhow::Error = RgcError::Retryable("boom".into()).into();
        assert_eq!(kind_of(&e), FailureKind::Network);
    }
}
