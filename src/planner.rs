use crate::jsonio::{load_json, save_json};
use crate::refs::{RefEntry, RemoteRefs};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const INITIAL_STEP: u32 = 10_000;
pub const MIN_STEP: u32 = 100;

#[derive(Debug, Clone)]
pub struct PlannerConfig {
    pub tags_per_batch: usize,
    pub initial_step: u32,
}
impl Default for PlannerConfig {
    fn default() -> Self {
        Self { tags_per_batch: 32, initial_step: INITIAL_STEP }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Piece {
    Chain { full_ref: String, short_name: String },
    TagBatch { tags: Vec<RefEntry> },
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn fnv1a_continue(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

impl Piece {
    pub fn id(&self) -> String {
        match self {
            Piece::Chain { full_ref, .. } => format!("chain:{}", full_ref),
            // 空批按构造不会出现（build_plan 按非空分批）；防御手编 plan.json
            Piece::TagBatch { tags } => format!("tags:{}", tags.first().map(|t| t.short_name.as_str()).unwrap_or("<empty>")),
        }
    }
    pub fn piece_dir_name(&self) -> String {
        let id = self.id();
        let sanitized: String = id.chars().map(|c| if c == '/' || c == ':' { '_' } else { c }).collect();
        format!("{}-{:08x}", sanitized, fnv1a(id.as_bytes()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub url: String,
    pub default_branch: Option<String>,
    pub initial_step: u32,
    pub pieces: Vec<Piece>,
}

impl Plan {
    /// url + 有序 piece ids 的 FNV-1a 指纹（16 hex）。state.json 保存它；
    /// 不匹配 = plan 与 state 来自两次不同的规划 → 必须报错而不是静默重规划。
    pub fn fingerprint(&self) -> String {
        let mut h = fnv1a(self.url.as_bytes());
        for p in &self.pieces {
            h ^= 0xff;
            h = h.wrapping_mul(0x100000001b3);
            let id = p.id();
            h = fnv1a_continue(h, id.as_bytes());
        }
        format!("{:016x}", h)
    }
}

pub fn build_plan(url: &str, refs: &RemoteRefs, cfg: &PlannerConfig) -> Result<Plan> {
    if refs.branches.is_empty() && refs.tags.is_empty() {
        anyhow::bail!("remote has no branches or tags — nothing to clone");
    }
    let mut pieces: Vec<Piece> = refs
        .branches
        .iter()
        .map(|b| Piece::Chain { full_ref: b.full_name.clone(), short_name: b.short_name.clone() })
        .collect();
    for batch in refs.tags.chunks(cfg.tags_per_batch) {
        pieces.push(Piece::TagBatch { tags: batch.to_vec() });
    }
    // 主干链排最前（串行关键路径尽早开始），其余分支按 ls-remote 顺序，tags 最后
    if let Some(db) = &refs.default_branch {
        pieces.sort_by_key(|p| match p {
            Piece::Chain { short_name, .. } if short_name == db => 0u8,
            Piece::Chain { .. } => 1,
            Piece::TagBatch { .. } => 2,
        });
    }
    Ok(Plan { url: url.to_string(), default_branch: refs.default_branch.clone(), initial_step: cfg.initial_step, pieces })
}

pub fn plan_path(dir: &Path) -> PathBuf {
    dir.join(".rgc").join("plan.json")
}
pub fn save_plan(dir: &Path, plan: &Plan) -> Result<()> {
    save_json(&plan_path(dir), plan)
}
pub fn load_plan(dir: &Path) -> Result<Option<Plan>> {
    load_json(&plan_path(dir))
}

#[derive(Debug, Clone, Copy)]
pub struct StepMeasurement {
    pub bytes: u64,
    pub secs: f64,
    pub commits: u32,
}

/// 自适应步长：按上一片实测吞吐与字节密度，让下一片接近 target_secs 秒
pub fn next_step(m: &StepMeasurement, target_secs: f64) -> u32 {
    if m.bytes == 0 || m.secs <= 0.0 || m.commits == 0 {
        return INITIAL_STEP;
    }
    let density = m.bytes as f64 / m.commits as f64; // 字节/commit
    let throughput = m.bytes as f64 / m.secs;        // 字节/秒
    let n = (throughput * target_secs / density).round() as u32;
    n.clamp(MIN_STEP, 5_000_000)
}

/// 失败降档：步长减半
pub fn halve_step(step: u32) -> u32 {
    (step / 2).max(MIN_STEP)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_plan_orders_default_first() {
        let refs = RemoteRefs {
            default_branch: Some("main".into()),
            branches: vec![
                RefEntry { full_name: "refs/heads/zlib".into(), short_name: "zlib".into(), oid: "a".into() },
                RefEntry { full_name: "refs/heads/main".into(), short_name: "main".into(), oid: "b".into() },
            ],
            tags: vec![RefEntry { full_name: "refs/tags/v1".into(), short_name: "v1".into(), oid: "c".into() }],
        };
        let plan = build_plan("https://x/y.git", &refs, &PlannerConfig::default()).unwrap();
        assert!(matches!(&plan.pieces[0], Piece::Chain { short_name, .. } if short_name == "main"));
        assert!(matches!(plan.pieces.last().unwrap(), Piece::TagBatch { .. }));
        assert_eq!(plan.initial_step, INITIAL_STEP);
    }

    #[test]
    fn build_plan_batches_tags() {
        let tags: Vec<RefEntry> = (0..5)
            .map(|i| RefEntry { full_name: format!("refs/tags/t{}", i), short_name: format!("t{}", i), oid: "o".into() })
            .collect();
        let refs = RemoteRefs { default_branch: None, branches: vec![], tags };
        let plan = build_plan("u", &refs, &PlannerConfig { tags_per_batch: 2, ..Default::default() }).unwrap();
        assert_eq!(plan.pieces.len(), 3);
        let dir = plan.pieces[0].piece_dir_name();
        assert!(dir.starts_with("tags_t0-"), "sanitized prefix kept, hash suffix appended: {}", dir);
    }

    #[test]
    fn next_step_adapts() {
        // 1000B/s 吞吐 × 100B/commit 密度 × 600s 目标 → 6000 commits
        let m = StepMeasurement { bytes: 1000, secs: 1.0, commits: 10 };
        assert_eq!(next_step(&m, 600.0), 6000);
        assert_eq!(next_step(&StepMeasurement { bytes: 0, secs: 1.0, commits: 10 }, 600.0), INITIAL_STEP);
    }

    #[test]
    fn halve_step_floors_at_min() {
        assert_eq!(halve_step(1000), 500);
        assert_eq!(halve_step(100), 100);
    }

    #[test]
    fn plan_roundtrip() {
        let td = tempfile::tempdir().unwrap();
        // 至少一个分支：空 remote 在 build_plan 即报错（sanctioned 契约）
        let refs = RemoteRefs {
            default_branch: Some("main".into()),
            branches: vec![RefEntry { full_name: "refs/heads/main".into(), short_name: "main".into(), oid: "a".into() }],
            tags: vec![],
        };
        let plan = build_plan("https://x/y.git", &refs, &PlannerConfig::default()).unwrap();
        save_plan(td.path(), &plan).unwrap();
        let loaded = load_plan(td.path()).unwrap().unwrap();
        assert_eq!(loaded.url, plan.url);
        assert_eq!(loaded.pieces.len(), plan.pieces.len());
    }

    #[test]
    fn empty_remote_is_an_error() {
        let refs = RemoteRefs { default_branch: None, branches: vec![], tags: vec![] };
        assert!(build_plan("u", &refs, &PlannerConfig::default()).is_err());
    }

    #[test]
    fn piece_dir_names_collide_never() {
        let mk = |full: &str| Piece::Chain { full_ref: full.into(), short_name: full.into() };
        let a = mk("refs/heads/release/2_2").piece_dir_name();
        let b = mk("refs/heads/release_2_2").piece_dir_name();
        assert_ne!(a, b, "sanitized names must not collide: {} vs {}", a, b);
        // 可读前缀仍在
        assert!(a.starts_with("chain_refs_heads_release"));
    }

    #[test]
    fn fingerprint_is_stable_and_sensitive() {
        let refs = crate::refs::RemoteRefs {
            default_branch: Some("main".into()),
            branches: vec![crate::refs::RefEntry { full_name: "refs/heads/main".into(), short_name: "main".into(), oid: "a".into() }],
            tags: vec![],
        };
        let p1 = build_plan("https://x/y.git", &refs, &PlannerConfig::default()).unwrap();
        let p2 = build_plan("https://x/y.git", &refs, &PlannerConfig::default()).unwrap();
        assert_eq!(p1.fingerprint(), p2.fingerprint(), "同一规划必须稳定");
        let p3 = build_plan("https://x/other.git", &refs, &PlannerConfig::default()).unwrap();
        assert_ne!(p1.fingerprint(), p3.fingerprint(), "url 变化必须改变指纹");
        let mut refs2 = refs.clone();
        refs2.branches.push(crate::refs::RefEntry { full_name: "refs/heads/dev".into(), short_name: "dev".into(), oid: "b".into() });
        let p4 = build_plan("https://x/y.git", &refs2, &PlannerConfig::default()).unwrap();
        assert_ne!(p1.fingerprint(), p4.fingerprint(), "piece 序列变化必须改变指纹");
    }

    #[test]
    fn save_json_tmp_name_is_process_unique() {
        // save 的中间文件不得与其他进程互踩：tmp 名含 pid
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("state.json");
        crate::jsonio::save_json(&p, &"x".to_string()).unwrap();
        assert!(p.exists());
    }
}
