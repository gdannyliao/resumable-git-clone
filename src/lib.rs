//! rgc 内部库目标：仅供本 crate 的测试/二进制使用，不是受支持的公共 API
//!（spec §2 明确非目标"作为库被复用"）。外部入口是 `rgc` 二进制（src/main.rs）。

pub mod equiv;
pub mod errors;
pub mod cli;
pub mod finalizer;
pub mod gitio;
pub mod jsonio;
pub mod planner;
pub mod refs;
pub mod scheduler;
pub mod state;

