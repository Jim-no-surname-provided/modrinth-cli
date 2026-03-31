use std::cmp::Ordering;
use std::collections::HashMap;
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

#[derive(Debug, Clone)]
pub struct GameVersionIndex {
    ranks: HashMap<String, usize>,
    longest_first: Vec<String>,
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

impl GameVersionIndex {
    pub fn new<I, S>(versions: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut ranks = HashMap::new();
        let mut longest_first = Vec::new();

        for version in versions {
            let version = version.into();
            let next_rank = ranks.len();
            if ranks.insert(version.clone(), next_rank).is_none() {
                longest_first.push(version);
            }
        }

        longest_first
            .sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));

        Self {
            ranks,
            longest_first,
        }
    }

    fn contains(&self, version: &str) -> bool {
        self.ranks.contains_key(version)
    }

    fn rank(&self, version: &str) -> Option<usize> {
        self.ranks.get(version).copied()
    }

    fn split_loader_directory(&self, value: &str) -> Option<(String, String)> {
        self.longest_first.iter().find_map(|version| {
            value.strip_prefix(version).and_then(|remainder| {
                remainder
                    .strip_prefix('-')
                    .filter(|loader_version| looks_like_loader_version(loader_version))
                    .map(|loader_version| (version.clone(), loader_version.to_string()))
            })
        })
    }
}

impl MinecraftContext {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn discover(start: &Path) -> Result<Self> {
        Self::discover_with_game_versions(start, None)
    }

    pub fn discover_with_game_versions(
        start: &Path,
        known_game_versions: Option<&GameVersionIndex>,
    ) -> Result<Self> {
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

        let environment = detect_environment(&effective_root, known_game_versions)?;

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

fn detect_environment(
    root: &Path,
    known_game_versions: Option<&GameVersionIndex>,
) -> Result<DetectedEnvironment> {
    detect_launcher_manifest(root)
        .or_else(|| detect_fabric(root, known_game_versions))
        .or_else(|| detect_quilt(root, known_game_versions))
        .or_else(|| detect_forge(root, known_game_versions))
        .or_else(|| detect_neoforge(root, known_game_versions))
        .with_context(|| {
            format!(
                "failed to detect loader and Minecraft version from {}",
                root.display()
            )
        })
}

fn detect_fabric(
    root: &Path,
    known_game_versions: Option<&GameVersionIndex>,
) -> Option<DetectedEnvironment> {
    latest_environment(
        detect_fabric_server_jars(root, known_game_versions)
            .into_iter()
            .chain(detect_dot_fabric_server_jars(root, known_game_versions))
            .chain(detect_fabric_remapped_jars(root, known_game_versions))
            .chain(detect_fabric_library_layout(root, known_game_versions)),
        known_game_versions,
    )
}

fn detect_quilt(
    root: &Path,
    known_game_versions: Option<&GameVersionIndex>,
) -> Option<DetectedEnvironment> {
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
                let minecraft_version = parse_quilt_loader_library_jar(
                    &file_name,
                    &loader_version,
                    known_game_versions,
                )?;

                Some(DetectedEnvironment {
                    loader: Loader::Quilt,
                    loader_version: Some(loader_version),
                    minecraft_version,
                })
            }),
        known_game_versions,
    )
}

fn detect_forge(
    root: &Path,
    known_game_versions: Option<&GameVersionIndex>,
) -> Option<DetectedEnvironment> {
    latest_environment(
        WalkDir::new(root.join("libraries/net/minecraftforge/forge"))
            .max_depth(2)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_dir())
            .filter_map(|entry| {
                let folder = entry.file_name().to_string_lossy().to_string();
                let (minecraft_version, loader_version) =
                    parse_forge_directory(&folder, known_game_versions)?;

                Some(DetectedEnvironment {
                    loader: Loader::Forge,
                    loader_version: Some(loader_version),
                    minecraft_version,
                })
            }),
        known_game_versions,
    )
}

fn detect_neoforge(
    root: &Path,
    known_game_versions: Option<&GameVersionIndex>,
) -> Option<DetectedEnvironment> {
    latest_environment(
        WalkDir::new(root.join("libraries/net/neoforged/neoforge"))
            .max_depth(2)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_dir())
            .filter_map(|entry| {
                let folder = entry.file_name().to_string_lossy().to_string();
                let (minecraft_version, loader_version) =
                    parse_neoforge_directory(&folder, known_game_versions)?;

                Some(DetectedEnvironment {
                    loader: Loader::NeoForge,
                    loader_version: Some(loader_version),
                    minecraft_version,
                })
            }),
        known_game_versions,
    )
}

fn looks_like_minecraft_version(value: &str) -> bool {
    if value.is_empty()
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
    {
        return false;
    }

    if value
        .strip_prefix("rd-")
        .or_else(|| value.strip_prefix("inf-"))
        .is_some_and(|suffix| suffix.chars().all(|ch| ch.is_ascii_digit()))
    {
        return true;
    }

    if let Some(snapshot) = value.strip_prefix("3D-Shareware-v") {
        return looks_like_numeric_dotted_value(snapshot);
    }

    if looks_like_weekly_snapshot(value) || looks_like_classic_prefixed_version(value) {
        return true;
    }

    let parts: Vec<&str> = value.split('-').collect();
    let Some(base) = parts.first().copied() else {
        return false;
    };

    if parts.len() == 1 {
        return looks_like_numeric_dotted_value(base);
    }

    if looks_like_numeric_dotted_value(base) {
        return match parts.as_slice() {
            [_, suffix] => is_compact_prerelease_suffix(suffix) || is_named_version_suffix(suffix),
            [_, channel, number] => {
                is_split_prerelease_channel(channel) && number.chars().all(|ch| ch.is_ascii_digit())
            }
            _ => false,
        };
    }

    matches!(
        parts.as_slice(),
        [base, suffix]
            if looks_like_alphanumeric_dotted_value(base)
                && suffix.starts_with("Pre")
                && suffix[3..].chars().all(|ch| ch.is_ascii_digit())
    )
}

fn supports_minecraft_version(value: &str, known_game_versions: Option<&GameVersionIndex>) -> bool {
    known_game_versions.is_some_and(|index| index.contains(value))
        || looks_like_minecraft_version(value)
}

fn looks_like_numeric_dotted_value(value: &str) -> bool {
    value
        .split('.')
        .all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit()))
}

fn looks_like_alphanumeric_dotted_value(value: &str) -> bool {
    value
        .split('.')
        .all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_alphanumeric()))
        && value.chars().any(|ch| ch.is_ascii_digit())
        && value.chars().any(|ch| ch.is_ascii_alphabetic())
}

fn looks_like_weekly_snapshot(value: &str) -> bool {
    let Some((year, rest)) = value.split_once('w') else {
        return false;
    };

    year.chars().all(|ch| ch.is_ascii_digit())
        && !year.is_empty()
        && rest.chars().next().is_some_and(|ch| ch.is_ascii_digit())
        && rest.chars().all(|ch| ch.is_ascii_alphanumeric())
        && rest.chars().any(|ch| ch.is_ascii_alphabetic())
}

fn looks_like_classic_prefixed_version(value: &str) -> bool {
    let (prefix, rest) = value.split_at(1);

    matches!(prefix, "a" | "b" | "c")
        && !rest.is_empty()
        && rest
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_'))
        && rest.chars().any(|ch| ch.is_ascii_digit())
}

fn is_compact_prerelease_suffix(value: &str) -> bool {
    ["pre", "rc"].into_iter().any(|prefix| {
        value.strip_prefix(prefix).is_some_and(|suffix| {
            !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit())
        })
    })
}

fn is_named_version_suffix(value: &str) -> bool {
    matches!(
        value,
        "potato" | "combat" | "experimental" | "snapshot" | "shareware"
    )
}

fn is_split_prerelease_channel(value: &str) -> bool {
    matches!(value, "pre" | "rc" | "snapshot")
}

fn looks_like_loader_version(value: &str) -> bool {
    value.chars().next().is_some_and(|ch| ch.is_ascii_digit())
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | '+'))
}

fn detect_fabric_server_jars(
    root: &Path,
    known_game_versions: Option<&GameVersionIndex>,
) -> Option<DetectedEnvironment> {
    let entries = fs::read_dir(root).ok()?;
    latest_environment(
        entries.filter_map(|entry| {
            let entry = entry.ok()?;
            if !entry.file_type().ok()?.is_file() {
                return None;
            }

            let file_name = entry.file_name().to_string_lossy().to_string();
            let (loader_version, minecraft_version) = parse_root_fabric_server_jar(&file_name)?;
            if !supports_minecraft_version(&minecraft_version, known_game_versions) {
                return None;
            }

            Some(DetectedEnvironment {
                loader: Loader::Fabric,
                loader_version: Some(loader_version),
                minecraft_version,
            })
        }),
        known_game_versions,
    )
}

fn detect_dot_fabric_server_jars(
    root: &Path,
    known_game_versions: Option<&GameVersionIndex>,
) -> Option<DetectedEnvironment> {
    let entries = fs::read_dir(root.join(".fabric/server")).ok()?;
    latest_environment(
        entries.filter_map(|entry| {
            let entry = entry.ok()?;
            if !entry.file_type().ok()?.is_file() {
                return None;
            }

            let file_name = entry.file_name().to_string_lossy().to_string();
            let (loader_version, minecraft_version) = parse_dot_fabric_server_jar(&file_name)?;
            if !supports_minecraft_version(&minecraft_version, known_game_versions) {
                return None;
            }

            Some(DetectedEnvironment {
                loader: Loader::Fabric,
                loader_version: Some(loader_version),
                minecraft_version,
            })
        }),
        known_game_versions,
    )
}

fn detect_fabric_library_layout(
    root: &Path,
    known_game_versions: Option<&GameVersionIndex>,
) -> Option<DetectedEnvironment> {
    let intermediary_version = detect_fabric_intermediary_version(root, known_game_versions);

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
                let (loader_version, minecraft_version) = parse_fabric_loader_library_jar(
                    &file_name,
                    &parent_version,
                    known_game_versions,
                )?;

                Some(DetectedEnvironment {
                    loader: Loader::Fabric,
                    loader_version: Some(loader_version),
                    minecraft_version: minecraft_version
                        .or_else(|| intermediary_version.clone())?,
                })
            }),
        known_game_versions,
    )
}

fn detect_fabric_remapped_jars(
    root: &Path,
    known_game_versions: Option<&GameVersionIndex>,
) -> Option<DetectedEnvironment> {
    let entries = fs::read_dir(root.join(".fabric/remappedJars")).ok()?;
    latest_environment(
        entries.filter_map(|entry| {
            let entry = entry.ok()?;
            if !entry.file_type().ok()?.is_dir() {
                return None;
            }

            let folder = entry.file_name().to_string_lossy().to_string();
            let (minecraft_version, loader_version) = parse_fabric_remapped_jar_dir(&folder)?;
            if !supports_minecraft_version(&minecraft_version, known_game_versions) {
                return None;
            }

            Some(DetectedEnvironment {
                loader: Loader::Fabric,
                loader_version: Some(loader_version),
                minecraft_version,
            })
        }),
        known_game_versions,
    )
}

fn detect_fabric_intermediary_version(
    root: &Path,
    known_game_versions: Option<&GameVersionIndex>,
) -> Option<String> {
    let entries = fs::read_dir(root.join("libraries/net/fabricmc/intermediary")).ok()?;
    latest_version(
        entries.filter_map(|entry| {
            let entry = entry.ok()?;
            if !entry.file_type().ok()?.is_dir() {
                return None;
            }

            let version = entry.file_name().to_string_lossy().to_string();
            supports_minecraft_version(&version, known_game_versions).then_some(version)
        }),
        known_game_versions,
    )
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
    known_game_versions: Option<&GameVersionIndex>,
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
        .filter(|version| supports_minecraft_version(version, known_game_versions))
        .map(str::to_string);

    Some((parent_version.to_string(), minecraft_version))
}

fn parse_fabric_remapped_jar_dir(folder: &str) -> Option<(String, String)> {
    let trimmed = folder.strip_prefix("minecraft-")?;
    let (minecraft_version, loader_version) = trimmed.rsplit_once('-')?;
    looks_like_minecraft_version(minecraft_version)
        .then(|| (minecraft_version.to_string(), loader_version.to_string()))
}

fn parse_quilt_loader_library_jar(
    file_name: &str,
    loader_version: &str,
    known_game_versions: Option<&GameVersionIndex>,
) -> Option<String> {
    let trimmed = file_name
        .strip_prefix("quilt-loader-")?
        .strip_suffix(".jar")?;
    let minecraft_version = trimmed.strip_prefix(&(loader_version.to_string() + "-"))?;
    supports_minecraft_version(minecraft_version, known_game_versions)
        .then(|| minecraft_version.to_string())
}

fn parse_forge_directory(
    folder: &str,
    known_game_versions: Option<&GameVersionIndex>,
) -> Option<(String, String)> {
    split_loader_directory_version(folder, known_game_versions)
}

fn parse_neoforge_directory(
    folder: &str,
    known_game_versions: Option<&GameVersionIndex>,
) -> Option<(String, String)> {
    split_loader_directory_version(folder, known_game_versions)
        .or_else(|| parse_neoforge_loader_only_version(folder))
}

fn split_loader_directory_version(
    folder: &str,
    known_game_versions: Option<&GameVersionIndex>,
) -> Option<(String, String)> {
    if let Some(known_versions) = known_game_versions {
        if let Some(split) = known_versions.split_loader_directory(folder) {
            return Some(split);
        }
    }

    folder.match_indices('-').rev().find_map(|(index, _)| {
        let minecraft_version = &folder[..index];
        let loader_version = &folder[index + 1..];
        (supports_minecraft_version(minecraft_version, known_game_versions)
            && looks_like_loader_version(loader_version))
        .then(|| (minecraft_version.to_string(), loader_version.to_string()))
    })
}

fn parse_neoforge_loader_only_version(folder: &str) -> Option<(String, String)> {
    let mut parts = folder.split(['.', '-']);
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

fn latest_environment<I>(
    candidates: I,
    known_game_versions: Option<&GameVersionIndex>,
) -> Option<DetectedEnvironment>
where
    I: IntoIterator<Item = DetectedEnvironment>,
{
    candidates
        .into_iter()
        .max_by(|left, right| compare_detected_environment(left, right, known_game_versions))
}

fn latest_version<I>(versions: I, known_game_versions: Option<&GameVersionIndex>) -> Option<String>
where
    I: IntoIterator<Item = String>,
{
    versions
        .into_iter()
        .max_by(|left, right| compare_minecraft_versions(left, right, known_game_versions))
}

fn compare_detected_environment(
    left: &DetectedEnvironment,
    right: &DetectedEnvironment,
    known_game_versions: Option<&GameVersionIndex>,
) -> Ordering {
    compare_minecraft_versions(
        &left.minecraft_version,
        &right.minecraft_version,
        known_game_versions,
    )
    .then_with(|| {
        compare_optional_versions(
            left.loader_version.as_deref(),
            right.loader_version.as_deref(),
        )
    })
}

fn compare_minecraft_versions(
    left: &str,
    right: &str,
    known_game_versions: Option<&GameVersionIndex>,
) -> Ordering {
    if let Some(ordering) = compare_known_game_version_rank(left, right, known_game_versions) {
        return ordering;
    }

    compare_version_strings(left, right)
}

fn compare_known_game_version_rank(
    left: &str,
    right: &str,
    known_game_versions: Option<&GameVersionIndex>,
) -> Option<Ordering> {
    let known_game_versions = known_game_versions?;
    let left_rank = known_game_versions.rank(left)?;
    let right_rank = known_game_versions.rank(right)?;
    Some(right_rank.cmp(&left_rank))
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
    let left_parts = tokenize_version_string(left);
    let right_parts = tokenize_version_string(right);
    let max_len = left_parts.len().max(right_parts.len());

    for index in 0..max_len {
        match (left_parts.get(index), right_parts.get(index)) {
            (Some(left), Some(right)) => {
                let ordering = compare_version_part(left, right);
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            (Some(extra), None) => {
                return if extra.is_prerelease_marker() {
                    Ordering::Less
                } else {
                    Ordering::Greater
                };
            }
            (None, Some(extra)) => {
                return if extra.is_prerelease_marker() {
                    Ordering::Greater
                } else {
                    Ordering::Less
                };
            }
            (None, None) => break,
        }
    }

    Ordering::Equal
}

fn tokenize_version_string(value: &str) -> Vec<VersionPart<'_>> {
    let mut parts = Vec::new();
    let mut start = None;
    let mut current_is_digit = false;

    for (index, ch) in value.char_indices() {
        if !ch.is_ascii_alphanumeric() {
            if let Some(token_start) = start.take() {
                parts.push(VersionPart::from(&value[token_start..index]));
            }
            continue;
        }

        let is_digit = ch.is_ascii_digit();
        match start {
            None => {
                start = Some(index);
                current_is_digit = is_digit;
            }
            Some(token_start) if current_is_digit != is_digit => {
                parts.push(VersionPart::from(&value[token_start..index]));
                start = Some(index);
                current_is_digit = is_digit;
            }
            Some(_) => {}
        }
    }

    if let Some(token_start) = start {
        parts.push(VersionPart::from(&value[token_start..]));
    }

    parts
}

fn compare_version_part(left: &VersionPart<'_>, right: &VersionPart<'_>) -> Ordering {
    match (left, right) {
        (VersionPart::Number(left), VersionPart::Number(right)) => left.cmp(right),
        (VersionPart::Text(left), VersionPart::Text(right)) => {
            compare_text_version_part(left, right)
        }
        (VersionPart::Number(_), VersionPart::Text(_)) => Ordering::Greater,
        (VersionPart::Text(_), VersionPart::Number(_)) => Ordering::Less,
    }
}

fn compare_text_version_part(left: &str, right: &str) -> Ordering {
    let left = left.to_ascii_lowercase();
    let right = right.to_ascii_lowercase();

    match (release_channel_rank(&left), release_channel_rank(&right)) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => left.cmp(&right),
    }
}

fn release_channel_rank(value: &str) -> Option<u8> {
    match value {
        "snapshot" => Some(0),
        "pre" => Some(1),
        "rc" => Some(2),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum VersionPart<'a> {
    Number(u64),
    Text(&'a str),
}

impl<'a> VersionPart<'a> {
    fn from(value: &'a str) -> Self {
        match value.parse::<u64>() {
            Ok(number) => Self::Number(number),
            Err(_) => Self::Text(value),
        }
    }

    fn is_prerelease_marker(&self) -> bool {
        matches!(self, Self::Text(value) if release_channel_rank(&value.to_ascii_lowercase()).is_some())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_modrinth_game_version_families() {
        for version in [
            "1.21.11",
            "1.21-pre1",
            "1.21.11-pre5",
            "1.20.6-rc1",
            "26.1-pre-3",
            "26.1-rc-3",
            "26.1-snapshot-11",
            "25w46a",
            "24w14potato",
            "3D-Shareware-v1.34",
            "1.RV-Pre1",
            "b1.7.3",
            "a1.2.6",
            "c0.30_01c",
            "inf-20100618",
            "rd-161348",
        ] {
            assert!(
                looks_like_minecraft_version(version),
                "{version} should be accepted"
            );
        }
    }

    #[test]
    fn splits_hyphenated_loader_directories_on_the_right_boundary() {
        assert_eq!(
            split_loader_directory_version("1.20.5-pre1-49.0.38", None),
            Some(("1.20.5-pre1".to_string(), "49.0.38".to_string()))
        );
        assert_eq!(
            split_loader_directory_version("26.1-snapshot-11-54.0.1", None),
            Some(("26.1-snapshot-11".to_string(), "54.0.1".to_string()))
        );
        assert_eq!(
            split_loader_directory_version("inf-20100618-1.0.0", None),
            Some(("inf-20100618".to_string(), "1.0.0".to_string()))
        );
        assert_eq!(
            split_loader_directory_version("1.7.10-10.13.4.1614-1.7.10", None),
            Some(("1.7.10".to_string(), "10.13.4.1614-1.7.10".to_string()))
        );
    }

    #[test]
    fn infers_neoforge_versions_from_loader_only_folders_with_suffixes() {
        assert_eq!(
            parse_neoforge_directory("20.6.90-beta", None),
            Some(("1.20.6".to_string(), "20.6.90-beta".to_string()))
        );
        assert_eq!(
            parse_neoforge_directory("1.20.6-20.6.90-beta", None),
            Some(("1.20.6".to_string(), "20.6.90-beta".to_string()))
        );
    }

    #[test]
    fn accepts_dynamic_modrinth_versions_outside_builtin_heuristics() {
        let known_versions = GameVersionIndex::new(["future_version_x"]);

        assert!(!looks_like_minecraft_version("future_version_x"));
        assert!(supports_minecraft_version(
            "future_version_x",
            Some(&known_versions)
        ));
        assert_eq!(
            split_loader_directory_version("future_version_x-99.0.0", Some(&known_versions)),
            Some(("future_version_x".to_string(), "99.0.0".to_string()))
        );
    }

    #[test]
    fn compares_snapshots_and_prereleases_naturally() {
        assert_eq!(compare_version_strings("25w09a", "25w10a"), Ordering::Less);
        assert_eq!(
            compare_version_strings("1.21.11-pre5", "1.21.11-pre12"),
            Ordering::Less
        );
        assert_eq!(
            compare_version_strings("1.21.11-rc1", "1.21.11"),
            Ordering::Less
        );
        assert_eq!(
            compare_version_strings("26.1-snapshot-11", "26.1-pre-1"),
            Ordering::Less
        );
    }
}
