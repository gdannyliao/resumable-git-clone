use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    RateLimited,
    Congestion,
    Network,
    ShallowUnsupported,
    Fatal,
}

/// 分类错误：类别 + 消息。（原为与 FailureKind 1:1 平行的五变体枚举，
/// 折叠为单结构体 —— 三个互译 match 随之消失，deletion test 通过。）
#[derive(Debug)]
pub struct RgcError {
    kind: FailureKind,
    message: String,
}

impl RgcError {
    pub fn new(kind: FailureKind, message: String) -> Self {
        Self { kind, message }
    }

    pub fn kind(&self) -> FailureKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for RgcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{:?}] {}", self.kind, self.message)
    }
}

impl std::error::Error for RgcError {}

/// 从 anyhow::Error 提取失败类别（无法识别 → Fatal）
pub fn kind_of(err: &anyhow::Error) -> FailureKind {
    err.downcast_ref::<RgcError>().map(|e| e.kind).unwrap_or(FailureKind::Fatal)
}

/// 按 git stderr 关键词分类。
/// RateLimited/Congestion 触发全局降速（不计片重试预算）；Network 计预算可退避重试；
/// ShallowUnsupported 触发"退化整支 fetch"；Fatal 放弃。
/// 关键词锚定真实 git/curl 输出（勿用臆造字符串）。
pub fn classify(stderr: &str) -> FailureKind {
    let s = stderr.to_lowercase();
    if ["http 429", "returned error: 429", "error: 429"].iter().any(|k| s.contains(k))
        || s.contains("rate limit")
        || s.contains("secondary rate")
        || s.contains("abuse")
    {
        FailureKind::RateLimited
    } else if s.contains("connection reset") {
        FailureKind::Congestion
    } else if [
        "could not resolve host",
        "connection refused",
        "the remote end hung up",
        "could not read from remote repository",
        "timed out",
        "early eof",
        "rpc failed",
        "unable to access",
        "temporary failure",
    ]
    .iter()
    .any(|k| s.contains(k))
    {
        FailureKind::Network
    } else if s.contains("does not support shallow") || (s.contains("shallow") && s.contains("dumb http")) {
        FailureKind::ShallowUnsupported
    } else {
        FailureKind::Fatal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // —— 真实 git/curl 输出（从 git 2.50 二进制/源码锚定） ——

    #[test]
    fn classify_network_error() {
        assert_eq!(
            classify("fatal: unable to access 'https://github.com/x/y/': Could not resolve host: github.com"),
            FailureKind::Network
        );
        assert_eq!(classify("fatal: the remote end hung up unexpectedly"), FailureKind::Network);
        assert_eq!(classify("fatal: Could not read from remote repository."), FailureKind::Network);
        assert_eq!(classify("error: RPC failed; curl 56 OpenSSL SSL_read: error"), FailureKind::Network);
    }

    #[test]
    fn classify_rate_limit() {
        assert_eq!(classify("error: RPC failed; The requested URL returned error: 429"), FailureKind::RateLimited);
        assert_eq!(classify("remote: abuse detection triggered"), FailureKind::RateLimited);
        // 进度计数里的 "1429" 不得误判
        assert_eq!(
            classify("remote: Enumerating objects: 1429, done.\nfatal: the remote end hung up unexpectedly"),
            FailureKind::Network
        );
        // 优先级：429 与网络关键词同时出现 → RateLimited
        assert_eq!(
            classify("fatal: unable to access 'https://x/': The requested URL returned error: 429"),
            FailureKind::RateLimited
        );
    }

    #[test]
    fn classify_congestion() {
        assert_eq!(classify("error: RPC failed; curl 56 Recv failure: Connection reset by peer"), FailureKind::Congestion);
    }

    #[test]
    fn classify_shallow_unsupported() {
        assert_eq!(classify("fatal: Server does not support shallow clients"), FailureKind::ShallowUnsupported);
        assert_eq!(classify("fatal: Server does not support shallow requests"), FailureKind::ShallowUnsupported);
        assert_eq!(classify("dumb http transport does not support shallow capabilities"), FailureKind::ShallowUnsupported);
    }

    #[test]
    fn classify_fatal_by_default() {
        assert_eq!(classify("fatal: Authentication failed"), FailureKind::Fatal);
    }

    #[test]
    fn kind_of_wraps_through_anyhow() {
        let e: anyhow::Error = RgcError::new(FailureKind::Network, "boom".into()).into();
        assert_eq!(kind_of(&e), FailureKind::Network);
    }

    #[test]
    fn kind_of_unknown_is_fatal() {
        let e = anyhow::anyhow!("disk low");
        assert_eq!(kind_of(&e), FailureKind::Fatal);
    }

    #[test]
    fn display_shows_kind_and_message() {
        let e = RgcError::new(FailureKind::Congestion, "reset".into());
        assert_eq!(e.to_string(), "[Congestion] reset");
    }
}
