#![allow(dead_code)]
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn git(dir: &Path, args: &[&str]) {
    let st = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("git executable not found on PATH");
    assert!(st.success(), "git {:?} failed in {:?}", args, dir);
}

/// 构建裸仓库 origin：<total> 个 commit 的 main；
/// branches 在第 at 个 commit 处创建，tags 打在第 at 个 commit。
/// 返回 origin 裸仓库路径（临时目录泄漏到测试进程结束，可接受）。
///
/// 注意：两次调用的 commit OID 不同（时间戳），只有同一 origin 的仓库间
/// 比较才是有效的（等价性校验器只做同源比较）。
/// 坐标 at 必须落在 1..=total，否则 panic（fail fast，防止下游测试静默走样）。
pub fn build_origin(total: u32, branches: &[(&str, u32)], tags: &[(&str, u32)]) -> PathBuf {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().to_path_buf();
    std::mem::forget(td);
    let origin = root.join("origin.git");
    git(&root, &["init", "--quiet", "--bare", "-b", "main", origin.to_str().unwrap()]);
    let work = root.join("work");
    git(&root, &["clone", "--quiet", origin.to_str().unwrap(), work.to_str().unwrap()]);
    git(&work, &["config", "user.email", "t@t"]);
    git(&work, &["config", "user.name", "t"]);
    // 隔离开发者全局配置，保证 fixture 封闭性
    git(&work, &["config", "commit.gpgsign", "false"]);
    git(&work, &["config", "tag.forceSignAnnotated", "false"]);
    for (name, at) in branches {
        assert!(*at >= 1 && *at <= total, "branch {:?} coordinate {} out of range 1..={}", name, at, total);
    }
    for (name, at) in tags {
        assert!(*at >= 1 && *at <= total, "tag {:?} coordinate {} out of range 1..={}", name, at, total);
    }
    for i in 0..total {
        std::fs::write(work.join(format!("f{}.txt", i % 7)), format!("content {}", i)).unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "--quiet", "-m", &format!("c{}", i)]);
        for (name, at) in branches {
            if *at == i + 1 {
                git(&work, &["branch", name]);
            }
        }
        for (name, at) in tags {
            if *at == i + 1 {
                git(&work, &["tag", name]);
            }
        }
    }
    git(&work, &["push", "--quiet", "origin", "--all"]);
    git(&work, &["push", "--quiet", "origin", "--tags"]);
    origin
}
