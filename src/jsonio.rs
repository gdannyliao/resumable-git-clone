use serde::{de::DeserializeOwned, Serialize};
use std::path::Path;

/// 原子写：tmp + rename，防写坏
pub fn save_json<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(value)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// 读取；损坏视同缺失（由调用方决定对账重建）
pub fn load_json<T: DeserializeOwned>(path: &Path) -> anyhow::Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)?;
    match serde_json::from_str(&text) {
        Ok(v) => Ok(Some(v)),
        Err(e) => {
            eprintln!("warning: {}: {} — treating as absent", path.display(), e);
            Ok(None)
        }
    }
}
