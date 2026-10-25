use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub const DATABASE_NAME: &str = "snapdiff.sqlite3";

#[derive(Clone)]
pub struct StorageLocation {
    pub config_file: PathBuf,
    pub default_dir: PathBuf,
    pub install_dir: PathBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Preference {
    version: u32,
    directory: PathBuf,
}

impl StorageLocation {
    pub fn new(config_dir: &Path, default_dir: PathBuf, executable_dir: &Path) -> Self {
        Self {
            config_file: config_dir.join("database-location.json"),
            default_dir,
            install_dir: executable_dir.join("data"),
        }
    }

    pub fn saved_directory(&self) -> Result<Option<PathBuf>, String> {
        let bytes = match fs::read(&self.config_file) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("读取数据库位置配置失败: {e}")),
        };
        let value: Preference =
            serde_json::from_slice(&bytes).map_err(|e| format!("数据库位置配置无效: {e}"))?;
        if value.version != 1 || !value.directory.is_absolute() {
            return Err("数据库位置配置版本或路径无效".into());
        }
        Ok(Some(value.directory))
    }

    pub fn save(&self, directory: &Path) -> Result<(), String> {
        crate::storage::atomic_write(&self.config_file, |writer| {
            serde_json::to_writer_pretty(
                writer,
                &Preference {
                    version: 1,
                    directory: directory.to_owned(),
                },
            )
            .map_err(|e| e.to_string())
        })
    }

    /// NSIS writes a Unicode INI bootstrap; an existing per-user preference wins.
    pub fn installer_directory(&self) -> Result<Option<PathBuf>, String> {
        let file = self
            .install_dir
            .parent()
            .ok_or("安装目录无效")?
            .join("snapdiff-storage.ini");
        let bytes = match fs::read(file) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("读取安装时选择的数据库位置失败: {e}")),
        };
        let text = if bytes.starts_with(&[0xff, 0xfe]) {
            if bytes.len() % 2 != 0 {
                return Err("安装数据库位置配置无效".into());
            }
            let words: Vec<_> = bytes[2..]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect();
            String::from_utf16(&words).map_err(|_| "安装数据库位置配置无效")?
        } else {
            String::from_utf8(bytes).map_err(|_| "安装数据库位置配置无效")?
        };
        let mut section = false;
        let mut mode = "";
        let mut directory = "";
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                section = line.eq_ignore_ascii_case("[storage]");
            }
            if !section {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                if key.eq_ignore_ascii_case("mode") {
                    mode = value.trim();
                }
                if key.eq_ignore_ascii_case("directory") {
                    directory = value.trim();
                }
            }
        }
        match mode {
            "default" => Ok(None),
            "install" => Ok(Some(self.install_dir.clone())),
            "custom" if Path::new(directory).is_absolute() => Ok(Some(directory.into())),
            _ => Err("安装数据库位置配置无效".into()),
        }
    }
}

pub fn lock_directory(directory: &Path) -> Result<fs::File, String> {
    lock_file(directory, ".snapdiff-storage.lock")
}

/// Resolve a not-yet-created folder through its nearest existing ancestor.
/// This lets callers reject a junction into a scanned root before writing there.
pub fn resolve_directory(directory: &Path) -> Result<PathBuf, String> {
    let mut ancestor = directory;
    while !ancestor.exists() {
        ancestor = ancestor.parent().ok_or("目标数据目录不可访问")?;
    }
    let mut resolved =
        fs::canonicalize(ancestor).map_err(|e| format!("目标数据目录不可访问: {e}"))?;
    for component in directory
        .strip_prefix(ancestor)
        .map_err(|e| e.to_string())?
        .components()
    {
        match component {
            std::path::Component::Normal(name) => resolved.push(name),
            std::path::Component::CurDir => (),
            std::path::Component::ParentDir => {
                resolved.pop();
            }
            _ => return Err("目标数据目录无效".into()),
        }
    }
    Ok(resolved)
}

pub fn lock_file(directory: &Path, name: &str) -> Result<fs::File, String> {
    fs::create_dir_all(directory).map_err(|e| format!("创建数据目录失败: {e}"))?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(name))
        .map_err(|e| format!("数据库目录不可写: {e}"))?;
    file.try_lock()
        .map_err(|e| format!("数据库位置正在被其他 SnapDiff 实例使用，或无法锁定: {e}"))?;
    Ok(file)
}
