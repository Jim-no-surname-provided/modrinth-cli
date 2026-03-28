use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub struct MinecraftContext {
    pub root_dir: PathBuf,
    pub mods_dir: PathBuf,
    pub environment: DetectedEnvironment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedEnvironment {
    pub loader: Loader,
    pub loader_version: Option<String>,
    pub minecraft_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Loader {
    Fabric,
    Quilt,
    Forge,
    NeoForge,
}

impl Loader {
    pub fn as_str(&self) -> &'static str {
        match self {
            Loader::Fabric => "fabric",
            Loader::Quilt => "quilt",
            Loader::Forge => "forge",
            Loader::NeoForge => "neoforge",
        }
    }
}

impl fmt::Display for Loader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl MinecraftContext {
    pub fn discover(start: &Path) -> Result<Self> {
        let root_dir = resolve_root_dir(start)?;
        let mods_dir = if root_dir.file_name().and_then(|name| name.to_str()) == Some("mods") {
            root_dir.clone()
        } else {
            root_dir.join("mods")
        };
        let effective_root = if mods_dir == root_dir {
            root_dir
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| root_dir.clone())
        } else {
            root_dir.clone()
        };

        let environment = detect_environment(&effective_root)?;

        Ok(Self {
            root_dir: effective_root,
            mods_dir,
            environment,
        })
    }
}

fn resolve_root_dir(start: &Path) -> Result<PathBuf> {
    let mut current = fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());

    if current.file_name().and_then(|name| name.to_str()) == Some("mods") {
        return Ok(current);
    }

    loop {
        if current.join("mods").exists() {
            return Ok(current);
        }

        let Some(parent) = current.parent() else {
            break;
        };

        if parent.file_name().and_then(|name| name.to_str()) == Some("mods") {
            return Ok(parent.to_path_buf());
        }

        current = parent.to_path_buf();
    }

    bail!(
        "could not find a Minecraft root or mods directory from {}",
        start.display()
    )
}

fn detect_environment(root: &Path) -> Result<DetectedEnvironment> {
    detect_fabric(root)
        .or_else(|| detect_quilt(root))
        .or_else(|| detect_forge(root))
        .or_else(|| detect_neoforge(root))
        .with_context(|| format!("failed to detect loader and Minecraft version from {}", root.display()))
}

fn detect_fabric(root: &Path) -> Option<DetectedEnvironment> {
    for entry in WalkDir::new(root.join("libraries/net/fabricmc/fabric-loader"))
        .max_depth(3)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let file_name = entry.file_name().to_string_lossy();
        if !file_name.starts_with("fabric-loader-") || !file_name.ends_with(".jar") {
            continue;
        }

        let loader_version = entry.path().parent()?.file_name()?.to_string_lossy().to_string();
        let trimmed = file_name
            .trim_end_matches(".jar")
            .trim_start_matches("fabric-loader-");
        let minecraft_version = trimmed.strip_prefix(&(loader_version.clone() + "-"))?.to_string();
        return Some(DetectedEnvironment {
            loader: Loader::Fabric,
            loader_version: Some(loader_version),
            minecraft_version,
        });
    }

    None
}

fn detect_quilt(root: &Path) -> Option<DetectedEnvironment> {
    for entry in WalkDir::new(root.join("libraries/org/quiltmc/quilt-loader"))
        .max_depth(3)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let file_name = entry.file_name().to_string_lossy();
        if !file_name.starts_with("quilt-loader-") || !file_name.ends_with(".jar") {
            continue;
        }

        let loader_version = entry.path().parent()?.file_name()?.to_string_lossy().to_string();
        let trimmed = file_name
            .trim_end_matches(".jar")
            .trim_start_matches("quilt-loader-");
        let minecraft_version = trimmed.strip_prefix(&(loader_version.clone() + "-"))?.to_string();
        return Some(DetectedEnvironment {
            loader: Loader::Quilt,
            loader_version: Some(loader_version),
            minecraft_version,
        });
    }

    None
}

fn detect_forge(root: &Path) -> Option<DetectedEnvironment> {
    for entry in WalkDir::new(root.join("libraries/net/minecraftforge/forge"))
        .max_depth(2)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_dir() {
            continue;
        }

        let folder = entry.file_name().to_string_lossy();
        let (minecraft_version, loader_version) = folder.split_once('-')?;
        if !looks_like_minecraft_version(minecraft_version) {
            continue;
        }

        return Some(DetectedEnvironment {
            loader: Loader::Forge,
            loader_version: Some(loader_version.to_string()),
            minecraft_version: minecraft_version.to_string(),
        });
    }

    None
}

fn detect_neoforge(root: &Path) -> Option<DetectedEnvironment> {
    for entry in WalkDir::new(root.join("libraries/net/neoforged/neoforge"))
        .max_depth(2)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_dir() {
            continue;
        }

        let folder = entry.file_name().to_string_lossy();
        let (minecraft_version, loader_version) = folder.split_once('-')?;
        if !looks_like_minecraft_version(minecraft_version) {
            continue;
        }

        return Some(DetectedEnvironment {
            loader: Loader::NeoForge,
            loader_version: Some(loader_version.to_string()),
            minecraft_version: minecraft_version.to_string(),
        });
    }

    None
}

fn looks_like_minecraft_version(value: &str) -> bool {
    value
        .chars()
        .all(|ch| ch.is_ascii_digit() || ch == '.')
        && value.chars().any(|ch| ch.is_ascii_digit())
}
