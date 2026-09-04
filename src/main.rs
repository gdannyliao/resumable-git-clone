//! Task 15：rgc CLI 入口 —— 只做 clap 解析，流程编排在 [`rgc::cli`]。
//! main 返回 anyhow::Result：Err → 非零退出码（1）。

use anyhow::Result;
use clap::{Parser, Subcommand};
use rgc::cli::{default_dir, clone_flow, resume_flow, status, CloneOptions};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "rgc", version, about = "Resumable git clone for very large repositories")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 断点续传 clone（目标目录已有 .rgc/ 时自动恢复）
    Clone {
        url: String,
        dir: Option<String>,
        #[arg(long, default_value_t = 2)]
        jobs: usize,
        /// 单片目标时长（秒）
        #[arg(long, default_value_t = 600.0)]
        piece_target: f64,
        #[arg(long)]
        keep_state: bool,
        /// 测试旋钮：初始 deepen 步长
        #[arg(long, default_value_t = 10_000, hide = true)]
        initial_step: u32,
    },
    /// 显式恢复中断的 clone
    Resume {
        dir: String,
        #[arg(long, default_value_t = 2)]
        jobs: usize,
        #[arg(long, default_value_t = 600.0)]
        piece_target: f64,
        #[arg(long)]
        keep_state: bool,
    },
    /// 查看各片进度（只读）
    Status { dir: String },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Clone { url, dir, jobs, piece_target, keep_state, initial_step } => {
            let dest = dir.map(PathBuf::from).unwrap_or_else(|| default_dir(&url));
            clone_flow(&url, &dest, &CloneOptions { jobs, piece_target, keep_state, initial_step })
        }
        Cmd::Resume { dir, jobs, piece_target, keep_state } => {
            resume_flow(&PathBuf::from(dir), &CloneOptions { jobs, piece_target, keep_state, ..Default::default() })
        }
        Cmd::Status { dir } => status(&PathBuf::from(dir)),
    }
}
