use std::cmp::Ordering;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
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

    loop {
        if let Some(root) = detect_root_candidate(&current) {
            return Ok(root);
        }

        let Some(parent) = current.parent() else {
            break;
        };

        current = parent.to_path_buf();
    }

    bail!(
        "could not find a Minecraft root or mods directory from {}",
        start.display()
    )
}

fn detect_root_candidate(path: &Path) -> Option<PathBuf> {
    if path.file_name().and_then(|name| name.to_str()) == Some("mods") {
        return Some(path.to_path_buf());
    }

    if path.join("mods").is_dir() {
        return Some(path.to_path_buf());
    }

    let minecraft_root = path.join("minecraft");
    if minecraft_root.join("mods").is_dir() {
        return Some(minecraft_root);
    }

    None
}

fn detect_environment(root: &Path) -> Result<DetectedEnvironment> {
    detect_launcher_manifest(root)
        .or_else(|| detect_fabric(root))
        .or_else(|| detect_quilt(root))
        .or_else(|| detect_forge(root))
        .or_else(|| detect_neoforge(root))
        .with_context(|| {
            format!(
                "failed to detect loader and Minecraft version from {}",
                root.display()
            )
        })
}

fn detect_fabric(root: &Path) -> Option<DetectedEnvironment> {
    latest_environment(
        detect_fabric_server_jars(root)
            .into_iter()
            .chain(detect_dot_fabric_server_jars(root))
            .chain(detect_fabric_remapped_jars(root))
            .chain(detect_fabric_library_layout(root)),
    )
}

fn detect_quilt(root: &Path) -> Option<DetectedEnvironment> {
    latest_environment(
        WalkDir::new(root.join("libraries/org/quiltmc/quilt-loader"))
            .max_depth(3)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .filter_map(|entry| {
                let file_name = entry.file_name().to_string_lossy().to_string();
                let loader_version = entry
                    .path()
                    .parent()?
                    .file_name()?
                    .to_string_lossy()
                    .to_string();
                let minecraft_version =
                    parse_quilt_loader_library_jar(&file_name, &loader_version)?;

                Some(DetectedEnvironment {
                    loader: Loader::Quilt,
                    loader_version: Some(loader_version),
                    minecraft_version,
                })
            }),
    )
}

fn detect_forge(root: &Path) -> Option<DetectedEnvironment> {
    latest_environment(
        WalkDir::new(root.join("libraries/net/minecraftforge/forge"))
            .max_depth(2)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_dir())
            .filter_map(|entry| {
                let folder = entry.file_name().to_string_lossy().to_string();
                let (minecraft_version, loader_version) = parse_forge_directory(&folder)?;

                Some(DetectedEnvironment {
                    loader: Loader::Forge,
                    loader_version: Some(loader_version),
                    minecraft_version,
                })
            }),
    )
}

fn detect_neoforge(root: &Path) -> Option<DetectedEnvironment> {
    latest_environment(
        WalkDir::new(root.join("libraries/net/neoforged/neoforge"))
            .max_depth(2)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_dir())
            .filter_map(|entry| {
                let folder = entry.file_name().to_string_lossy().to_string();
                let (minecraft_version, loader_version) = parse_neoforge_directory(&folder)?;

                Some(DetectedEnvironment {
                    loader: Loader::NeoForge,
                    loader_version: Some(loader_version),
                    minecraft_version,
                })
            }),
    )
}

fn looks_like_minecraft_version(value: &str) -> bool {
    value.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
        && value.chars().any(|ch| ch.is_ascii_digit())
}

fn detect_fabric_server_jars(root: &Path) -> Option<DetectedEnvironment> {
    let entries = fs::read_dir(root).ok()?;
    latest_environment(entries.filter_map(|entry| {
        let entry = entry.ok()?;
        if !entry.file_type().ok()?.is_file() {
            return None;
        }

        let file_name = entry.file_name().to_string_lossy().to_string();
        let (loader_version, minecraft_version) = parse_root_fabric_server_jar(&file_name)?;

        Some(DetectedEnvironment {
            loader: Loader::Fabric,
            loader_version: Some(loader_version),
            minecraft_version,
        })
    }))
}

fn detect_dot_fabric_server_jars(root: &Path) -> Option<DetectedEnvironment> {
    let entries = fs::read_dir(root.join(".fabric/server")).ok()?;
    latest_environment(entries.filter_map(|entry| {
        let entry = entry.ok()?;
        if !entry.file_type().ok()?.is_file() {
            return None;
        }

        let file_name = entry.file_name().to_string_lossy().to_string();
        let (loader_version, minecraft_version) = parse_dot_fabric_server_jar(&file_name)?;

        Some(DetectedEnvironment {
            loader: Loader::Fabric,
            loader_version: Some(loader_version),
            minecraft_version,
        })
    }))
}

fn detect_fabric_library_layout(root: &Path) -> Option<DetectedEnvironment> {
    let intermediary_version = detect_fabric_intermediary_version(root);

    latest_environment(
        WalkDir::new(root.join("libraries/net/fabricmc/fabric-loader"))
            .max_depth(3)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .filter_map(|entry| {
                let file_name = entry.file_name().to_string_lossy().to_string();
                let parent_version = entry
                    .path()
                    .parent()?
                    .file_name()?
                    .to_string_lossy()
                    .to_string();
                let (loader_version, minecraft_version) =
                    parse_fabric_loader_library_jar(&file_name, &parent_version)?;

                Some(DetectedEnvironment {
                    loader: Loader::Fabric,
                    loader_version: Some(loader_version),
                    minecraft_version: minecraft_version
                        .or_else(|| intermediary_version.clone())?,
                })
            }),
    )
}

fn detect_fabric_remapped_jars(root: &Path) -> Option<DetectedEnvironment> {
    let entries = fs::read_dir(root.join(".fabric/remappedJars")).ok()?;
    latest_environment(entries.filter_map(|entry| {
        let entry = entry.ok()?;
        if !entry.file_type().ok()?.is_dir() {
            return None;
        }

        let folder = entry.file_name().to_string_lossy().to_string();
        let (minecraft_version, loader_version) = parse_fabric_remapped_jar_dir(&folder)?;

        Some(DetectedEnvironment {
            loader: Loader::Fabric,
            loader_version: Some(loader_version),
            minecraft_version,
        })
    }))
}

fn detect_fabric_intermediary_version(root: &Path) -> Option<String> {
    let entries = fs::read_dir(root.join("libraries/net/fabricmc/intermediary")).ok()?;
    latest_version(entries.filter_map(|entry| {
        let entry = entry.ok()?;
        if !entry.file_type().ok()?.is_dir() {
            return None;
        }

        let version = entry.file_name().to_string_lossy().to_string();
        looks_like_minecraft_version(&version).then_some(version)
    }))
}

fn parse_root_fabric_server_jar(file_name: &str) -> Option<(String, String)> {
    let trimmed = file_name
        .strip_prefix("fabric-server-mc.")?
        .strip_suffix(".jar")?;
    let (minecraft_version, remainder) = trimmed.split_once("-loader.")?;
    let (loader_version, _) = remainder.split_once("-launcher.")?;
    looks_like_minecraft_version(minecraft_version)
        .then(|| (loader_version.to_string(), minecraft_version.to_string()))
}

fn parse_dot_fabric_server_jar(file_name: &str) -> Option<(String, String)> {
    let trimmed = file_name
        .strip_prefix("fabric-loader-server-")?
        .strip_suffix(".jar")?;
    let (loader_version, minecraft_version) = trimmed.split_once("-minecraft-")?;
    looks_like_minecraft_version(minecraft_version)
        .then(|| (loader_version.to_string(), minecraft_version.to_string()))
}

fn parse_fabric_loader_library_jar(
    file_name: &str,
    parent_version: &str,
) -> Option<(String, Option<String>)> {
    let trimmed = file_name
        .strip_prefix("fabric-loader-")?
        .strip_suffix(".jar")?;
    if !trimmed.starts_with(parent_version) {
        return None;
    }

    let minecraft_version = trimmed
        .strip_prefix(parent_version)
        .and_then(|remainder| remainder.strip_prefix('-'))
        .filter(|version| looks_like_minecraft_version(version))
        .map(str::to_string);

    Some((parent_version.to_string(), minecraft_version))
}

fn parse_fabric_remapped_jar_dir(folder: &str) -> Option<(String, String)> {
    let trimmed = folder.strip_prefix("minecraft-")?;
    let (minecraft_version, loader_version) = trimmed.rsplit_once('-')?;
    looks_like_minecraft_version(minecraft_version)
        .then(|| (minecraft_version.to_string(), loader_version.to_string()))
}

fn parse_quilt_loader_library_jar(file_name: &str, loader_version: &str) -> Option<String> {
    let trimmed = file_name
        .strip_prefix("quilt-loader-")?
        .strip_suffix(".jar")?;
    let minecraft_version = trimmed.strip_prefix(&(loader_version.to_string() + "-"))?;
    looks_like_minecraft_version(minecraft_version).then(|| minecraft_version.to_string())
}

fn parse_forge_directory(folder: &str) -> Option<(String, String)> {
    let (minecraft_version, loader_version) = folder.split_once('-')?;
    looks_like_minecraft_version(minecraft_version)
        .then(|| (minecraft_version.to_string(), loader_version.to_string()))
}

fn parse_neoforge_directory(folder: &str) -> Option<(String, String)> {
    if let Some((minecraft_version, loader_version)) = folder.split_once('-') {
        return looks_like_minecraft_version(minecraft_version)
            .then(|| (minecraft_version.to_string(), loader_version.to_string()));
    }

    let mut parts = folder.split('.');
    let major = parts.next()?;
    let minor = parts.next()?;
    let _patch = parts.next()?;
    if major.chars().all(|ch| ch.is_ascii_digit()) && minor.chars().all(|ch| ch.is_ascii_digit()) {
        return Some((format!("1.{major}.{minor}"), folder.to_string()));
    }

    None
}

fn detect_launcher_manifest(root: &Path) -> Option<DetectedEnvironment> {
    root.ancestors()
        .find_map(|ancestor| detect_mmc_pack_manifest(&ancestor.join("mmc-pack.json")))
}

fn detect_mmc_pack_manifest(path: &Path) -> Option<DetectedEnvironment> {
    let manifest = fs::read_to_string(path).ok()?;
    let manifest: MmcPackManifest = serde_json::from_str(&manifest).ok()?;
    manifest.detect_environment()
}

fn latest_environment<I>(candidates: I) -> Option<DetectedEnvironment>
where
    I: IntoIterator<Item = DetectedEnvironment>,
{
    candidates.into_iter().max_by(compare_detected_environment)
}

fn latest_version<I>(versions: I) -> Option<String>
where
    I: IntoIterator<Item = String>,
{
    versions
        .into_iter()
        .max_by(|left, right| compare_version_strings(left, right))
}

fn compare_detected_environment(
    left: &DetectedEnvironment,
    right: &DetectedEnvironment,
) -> Ordering {
    compare_version_strings(&left.minecraft_version, &right.minecraft_version).then_with(|| {
        compare_optional_versions(
            left.loader_version.as_deref(),
            right.loader_version.as_deref(),
        )
    })
}

fn compare_optional_versions(left: Option<&str>, right: Option<&str>) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => compare_version_strings(left, right),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => Ordering::Equal,
    }
}

fn compare_version_strings(left: &str, right: &str) -> Ordering {
    let mut left_parts = left
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty());
    let mut right_parts = right
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty());

    loop {
        match (left_parts.next(), right_parts.next()) {
            (Some(left), Some(right)) => {
                let ordering = compare_version_part(left, right);
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            (Some(_), None) => return Ordering::Greater,
            (None, Some(_)) => return Ordering::Less,
            (None, None) => return Ordering::Equal,
        }
    }
}

fn compare_version_part(left: &str, right: &str) -> Ordering {
    match (left.parse::<u64>(), right.parse::<u64>()) {
        (Ok(left), Ok(right)) => left.cmp(&right),
        _ => left.cmp(right),
    }
}

#[derive(Debug, Deserialize)]
struct MmcPackManifest {
    #[serde(default)]
    components: Vec<MmcPackComponent>,
}

impl MmcPackManifest {
    fn detect_environment(&self) -> Option<DetectedEnvironment> {
        let minecraft_version = self.component_version("net.minecraft")?;

        let (loader, loader_version) = [
            ("net.fabricmc.fabric-loader", Loader::Fabric),
            ("org.quiltmc.quilt-loader", Loader::Quilt),
            ("net.minecraftforge", Loader::Forge),
            ("net.neoforged", Loader::NeoForge),
            ("net.neoforged.neoforge", Loader::NeoForge),
        ]
        .into_iter()
        .find_map(|(uid, loader)| self.component_version(uid).map(|version| (loader, version)))?;

        Some(DetectedEnvironment {
            loader,
            loader_version: Some(loader_version),
            minecraft_version,
        })
    }

    fn component_version(&self, uid: &str) -> Option<String> {
        self.components
            .iter()
            .find(|component| component.uid == uid)
            .map(|component| component.version.clone())
    }
}

#[derive(Debug, Deserialize)]
struct MmcPackComponent {
    uid: String,
    version: String,
}
