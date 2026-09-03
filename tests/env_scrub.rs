//! run_git 必须擦除继承的 GIT_* 定位变量：rgc 常从 git hook / wrapper 里被调用，
//! 调用方环境里的 GIT_DIR/GIT_WORK_TREE 等会把子 git 重定向到别的仓库。
//! 独立成单独集成测试二进制：env::set_var 是进程全局的，本二进制只跑这一个测试，
//! 不会与并行的其他测试互相污染。
use rgc::gitio::{init_main_repo, run_git};

#[test]
fn git_env_vars_do_not_redirect_operations() {
    let td = tempfile::tempdir().unwrap();
    let main = td.path().join("r");
    // 先污染环境：指向不存在的仓库。若 run_git 不擦除，init/rev-parse 全部跑偏或失败。
    std::env::set_var("GIT_DIR", td.path().join("bogus.git"));
    std::env::set_var("GIT_WORK_TREE", td.path());
    std::env::set_var("GIT_INDEX_FILE", td.path().join("bogus.index"));
    init_main_repo(&main, "https://example.com/x.git").unwrap();
    let out = run_git(&["rev-parse", "--git-dir"], Some(&main)).unwrap();
    assert_eq!(out.stdout.trim(), ".git");
    assert!(main.join(".git").exists());
}
