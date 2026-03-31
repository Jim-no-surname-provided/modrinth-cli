use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

use crate::minecraft::DetectedEnvironment;

const DEFAULT_API_BASE: &str = "https://api.modrinth.com/v2";
const DEFAULT_CACHE_LIMIT_MB: u64 = 64;
const DEFAULT_CACHE_TTL_SECS: u64 = 60 * 60 * 12;
const MEGABYTE: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheKind {
    GameVersions,
    Projects,
    SearchResults,
    ProjectVersions,
    VersionLookupByHash,
    LatestVersionsLookup,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheSettings {
    pub game_versions: bool,
    pub projects: bool,
    pub search_results: bool,
    pub project_versions: bool,
    pub version_lookup_by_hash: bool,
    pub latest_versions_lookup: bool,
}

#[derive(Debug, Clone)]
pub struct CacheStats {
    pub dir: PathBuf,
    pub settings_path: PathBuf,
    pub max_bytes: u64,
    pub ttl: Duration,
    pub entry_count: usize,
    pub total_bytes: u64,
    pub settings: CacheSettings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheClearResult {
    pub removed_entries: usize,
    pub removed_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct ModrinthClient {
    base_url: String,
    http: Client,
    cache: ResponseCache,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub slug: String,
    pub title: String,
    #[serde(default)]
    pub _description: String,
    #[serde(default)]
    pub downloads: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub project_id: Option<String>,
    pub slug: String,
    pub title: String,
    #[serde(default)]
    pub downloads: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Version {
    #[serde(rename = "id")]
    pub _id: String,
    pub project_id: String,
    #[serde(rename = "name")]
    pub _name: String,
    pub version_number: String,
    pub version_type: String,
    #[serde(default)]
    #[serde(rename = "loaders")]
    pub _loaders: Vec<String>,
    #[serde(default)]
    #[serde(rename = "game_versions")]
    pub _game_versions: Vec<String>,
    #[serde(default)]
    pub featured: bool,
    pub status: Option<String>,
    pub date_published: DateTime<Utc>,
    #[serde(default)]
    pub downloads: u64,
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
    #[serde(default)]
    pub files: Vec<VersionFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dependency {
    #[serde(rename = "version_id")]
    pub _version_id: Option<String>,
    pub project_id: Option<String>,
    pub file_name: Option<String>,
    pub dependency_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionFile {
    pub hashes: FileHashes,
    pub url: String,
    pub filename: String,
    pub primary: bool,
    pub file_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileHashes {
    pub sha1: Option<String>,
    #[serde(rename = "sha512")]
    pub _sha512: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    hits: Vec<SearchHit>,
}

#[derive(Debug, Serialize)]
struct LatestVersionsRequest<'a> {
    hashes: &'a [String],
    algorithm: &'static str,
    loaders: Vec<String>,
    game_versions: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct GameVersionTag {
    version: String,
}

#[derive(Debug, Clone)]
struct ResponseCache {
    dir: PathBuf,
    settings_path: PathBuf,
    max_bytes: u64,
    ttl: Duration,
    settings: CacheSettings,
}

#[derive(Debug)]
struct CacheEntryInfo {
    path: PathBuf,
    modified: SystemTime,
    size: u64,
}

impl CacheKind {
    pub const ALL: [Self; 6] = [
        Self::GameVersions,
        Self::Projects,
        Self::SearchResults,
        Self::ProjectVersions,
        Self::VersionLookupByHash,
        Self::LatestVersionsLookup,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::GameVersions => "Game version tags",
            Self::Projects => "Project metadata",
            Self::SearchResults => "Search results",
            Self::ProjectVersions => "Project versions",
            Self::VersionLookupByHash => "Installed version lookups",
            Self::LatestVersionsLookup => "Batch update lookups",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::GameVersions => "Reuse the Modrinth game-version tag index.",
            Self::Projects => "Reuse project records fetched by slug or project id.",
            Self::SearchResults => "Reuse repeated search result pages.",
            Self::ProjectVersions => "Reuse compatible version lists for a project and environment.",
            Self::VersionLookupByHash => "Reuse installed-jar hash lookups.",
            Self::LatestVersionsLookup => "Reuse bulk update checks for installed hashes.",
        }
    }

    fn file_prefix(self) -> &'static str {
        match self {
            Self::GameVersions => "game_versions",
            Self::Projects => "projects",
            Self::SearchResults => "search_results",
            Self::ProjectVersions => "project_versions",
            Self::VersionLookupByHash => "version_lookup_by_hash",
            Self::LatestVersionsLookup => "latest_versions_lookup",
        }
    }
}

impl Default for CacheSettings {
    fn default() -> Self {
        Self {
            game_versions: true,
            projects: true,
            search_results: true,
            project_versions: true,
            version_lookup_by_hash: true,
            latest_versions_lookup: true,
        }
    }
}

impl CacheSettings {
    pub fn is_enabled(&self, kind: CacheKind) -> bool {
        match kind {
            CacheKind::GameVersions => self.game_versions,
            CacheKind::Projects => self.projects,
            CacheKind::SearchResults => self.search_results,
            CacheKind::ProjectVersions => self.project_versions,
            CacheKind::VersionLookupByHash => self.version_lookup_by_hash,
            CacheKind::LatestVersionsLookup => self.latest_versions_lookup,
        }
    }

    pub fn set_enabled(&mut self, kind: CacheKind, enabled: bool) {
        match kind {
            CacheKind::GameVersions => self.game_versions = enabled,
            CacheKind::Projects => self.projects = enabled,
            CacheKind::SearchResults => self.search_results = enabled,
            CacheKind::ProjectVersions => self.project_versions = enabled,
            CacheKind::VersionLookupByHash => self.version_lookup_by_hash = enabled,
            CacheKind::LatestVersionsLookup => self.latest_versions_lookup = enabled,
        }
    }
}

impl ModrinthClient {
    pub fn new(base_url: String) -> Result<Self> {
        Self::new_with_cache(base_url, ResponseCache::from_env())
    }

    fn new_with_cache(base_url: String, cache: ResponseCache) -> Result<Self> {
        let http = Client::builder()
            .user_agent(format!("modrinth-cli/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .context("failed to build HTTP client")?;

        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            http,
            cache,
        })
    }

    pub fn new_from_env() -> Result<Self> {
        let base_url =
            env::var("MODRINTH_API_BASE").unwrap_or_else(|_| DEFAULT_API_BASE.to_string());
        Self::new(base_url)
    }

    pub fn game_versions(&self) -> Result<Vec<String>> {
        let tags: Vec<GameVersionTag> = self.get_json_cached(
            CacheKind::GameVersions,
            format!("{}/tag/game_version", self.base_url),
            "failed to fetch Modrinth game versions",
            "game versions request failed",
            "failed to decode Modrinth game versions response",
        )?;

        Ok(tags.into_iter().map(|tag| tag.version).collect())
    }

    pub fn cached_game_versions(&self) -> Option<Vec<String>> {
        let url = format!("{}/tag/game_version", self.base_url);
        let cache_key = self.cache.key_for("GET", &url, None);
        let bytes = self.cache.read(CacheKind::GameVersions, &cache_key)?;
        let tags: Vec<GameVersionTag> = serde_json::from_slice(&bytes).ok()?;
        Some(tags.into_iter().map(|tag| tag.version).collect())
    }

    pub fn get_project(&self, id_or_slug: &str) -> Result<Option<Project>> {
        let url = format!("{}/project/{}", self.base_url, id_or_slug);
        self.get_optional_json_cached(
            CacheKind::Projects,
            url,
            "failed to fetch project",
            "project request failed",
            "failed to decode project response",
        )
    }

    pub fn search_projects(
        &self,
        query: &str,
        environment: Option<&DetectedEnvironment>,
    ) -> Result<Vec<SearchHit>> {
        let mut facets = vec![vec!["project_type:mod".to_string()]];
        if let Some(environment) = environment {
            facets.push(vec![format!("categories:{}", environment.loader.as_str())]);
            facets.push(vec![format!("versions:{}", environment.minecraft_version)]);
        }

        let facets = serde_json::to_string(&facets)?;
        let url = format!(
            "{}/search?query={}&limit=20&index=relevance&facets={}",
            self.base_url,
            urlencoding::encode(query),
            urlencoding::encode(&facets)
        );

        let parsed: SearchResponse = self.get_json_cached(
            CacheKind::SearchResults,
            url,
            "failed to search Modrinth",
            "Modrinth search failed",
            "failed to decode Modrinth search response",
        )?;
        Ok(parsed.hits)
    }

    pub fn project_versions(
        &self,
        id_or_slug: &str,
        environment: &DetectedEnvironment,
    ) -> Result<Vec<Version>> {
        let loaders = serde_json::to_string(&vec![environment.loader.as_str()])?;
        let game_versions = serde_json::to_string(&vec![environment.minecraft_version.clone()])?;
        let url = format!(
            "{}/project/{}/version?loaders={}&game_versions={}",
            self.base_url,
            id_or_slug,
            urlencoding::encode(&loaders),
            urlencoding::encode(&game_versions)
        );

        self.get_json_cached(
            CacheKind::ProjectVersions,
            url,
            "failed to fetch project versions",
            "project versions request failed",
            "failed to decode project versions response",
        )
    }

    pub fn version_from_hash(&self, sha1: &str) -> Result<Option<Version>> {
        self.get_optional_json_cached(
            CacheKind::VersionLookupByHash,
            format!("{}/version_file/{}?algorithm=sha1", self.base_url, sha1),
            "failed to lookup installed mod by hash",
            "version-from-hash request failed",
            "failed to decode version-from-hash response",
        )
    }

    pub fn latest_versions_for_hashes(
        &self,
        hashes: &[String],
        environment: &DetectedEnvironment,
    ) -> Result<HashMap<String, Version>> {
        if hashes.is_empty() {
            return Ok(HashMap::new());
        }

        let body = LatestVersionsRequest {
            hashes,
            algorithm: "sha1",
            loaders: vec![environment.loader.as_str().to_string()],
            game_versions: vec![environment.minecraft_version.clone()],
        };

        self.post_json_cached(
            CacheKind::LatestVersionsLookup,
            format!("{}/version_files/update", self.base_url),
            &body,
            "failed to query latest versions from hashes",
            "latest versions request failed",
            "failed to decode latest versions response",
        )
    }

    pub fn download(&self, url: &str) -> Result<Response> {
        self.http
            .get(url)
            .send()
            .with_context(|| format!("failed to download {}", url))?
            .error_for_status()
            .with_context(|| format!("download failed for {}", url))
    }

    pub fn cache_stats(&self) -> Result<CacheStats> {
        self.cache.stats()
    }

    pub fn clear_cache(&self) -> Result<CacheClearResult> {
        self.cache.clear()
    }

    pub fn cache_settings(&self) -> CacheSettings {
        self.cache.settings()
    }

    pub fn save_cache_settings(&mut self, settings: CacheSettings) -> Result<()> {
        self.cache.save_settings(settings)
    }

    fn get_json_cached<T>(
        &self,
        kind: CacheKind,
        url: String,
        fetch_context: &str,
        request_context: &str,
        decode_context: &str,
    ) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let cache_key = self.cache.key_for("GET", &url, None);
        if let Some(bytes) = self.cache.read(kind, &cache_key) {
            return serde_json::from_slice(&bytes).with_context(|| decode_context.to_string());
        }

        let response = self
            .http
            .get(&url)
            .send()
            .with_context(|| fetch_context.to_string())?
            .error_for_status()
            .with_context(|| request_context.to_string())?;
        let bytes = response
            .bytes()
            .with_context(|| decode_context.to_string())?
            .to_vec();
        self.cache.write(kind, &cache_key, &bytes);
        serde_json::from_slice(&bytes).with_context(|| decode_context.to_string())
    }

    fn get_optional_json_cached<T>(
        &self,
        kind: CacheKind,
        url: String,
        fetch_context: &str,
        request_context: &str,
        decode_context: &str,
    ) -> Result<Option<T>>
    where
        T: DeserializeOwned,
    {
        let cache_key = self.cache.key_for("GET", &url, None);
        if let Some(bytes) = self.cache.read(kind, &cache_key) {
            return serde_json::from_slice(&bytes)
                .map(Some)
                .with_context(|| decode_context.to_string());
        }

        let response = self
            .http
            .get(&url)
            .send()
            .with_context(|| fetch_context.to_string())?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }

        let response = response
            .error_for_status()
            .with_context(|| request_context.to_string())?;
        let bytes = response
            .bytes()
            .with_context(|| decode_context.to_string())?
            .to_vec();
        self.cache.write(kind, &cache_key, &bytes);
        serde_json::from_slice(&bytes)
            .map(Some)
            .with_context(|| decode_context.to_string())
    }

    fn post_json_cached<T, B>(
        &self,
        kind: CacheKind,
        url: String,
        body: &B,
        fetch_context: &str,
        request_context: &str,
        decode_context: &str,
    ) -> Result<T>
    where
        T: DeserializeOwned,
        B: Serialize,
    {
        let body_bytes = serde_json::to_vec(body).context("failed to encode request body")?;
        let cache_key = self.cache.key_for("POST", &url, Some(&body_bytes));
        if let Some(bytes) = self.cache.read(kind, &cache_key) {
            return serde_json::from_slice(&bytes).with_context(|| decode_context.to_string());
        }

        let response = self
            .http
            .post(&url)
            .json(body)
            .send()
            .with_context(|| fetch_context.to_string())?
            .error_for_status()
            .with_context(|| request_context.to_string())?;
        let bytes = response
            .bytes()
            .with_context(|| decode_context.to_string())?
            .to_vec();
        self.cache.write(kind, &cache_key, &bytes);
        serde_json::from_slice(&bytes).with_context(|| decode_context.to_string())
    }
}

impl ResponseCache {
    fn from_env() -> Self {
        let dir = default_cache_dir();
        let settings_path = default_settings_path();
        let max_bytes = env::var("MODRINTH_CACHE_LIMIT_MB")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(DEFAULT_CACHE_LIMIT_MB)
            .saturating_mul(MEGABYTE);
        let ttl = Duration::from_secs(
            env::var("MODRINTH_CACHE_TTL_SECS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(DEFAULT_CACHE_TTL_SECS),
        );
        let settings = Self::load_settings(&settings_path).unwrap_or_default();

        Self::new_with_settings_path(dir, settings_path, max_bytes, ttl, settings)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn new(dir: PathBuf, max_bytes: u64, ttl: Duration) -> Self {
        Self::new_with_settings_path(
            dir.clone(),
            dir.join("cache-settings.json"),
            max_bytes,
            ttl,
            CacheSettings::default(),
        )
    }

    fn new_with_settings_path(
        dir: PathBuf,
        settings_path: PathBuf,
        max_bytes: u64,
        ttl: Duration,
        settings: CacheSettings,
    ) -> Self {
        Self {
            dir,
            settings_path,
            max_bytes,
            ttl,
            settings,
        }
    }

    fn read(&self, kind: CacheKind, key: &str) -> Option<Vec<u8>> {
        if !self.is_enabled() || !self.settings.is_enabled(kind) {
            return None;
        }

        let path = self.path_for(kind, key);
        let metadata = fs::metadata(&path).ok()?;
        let modified = metadata.modified().ok()?;
        if self.is_stale(modified) {
            let _ = fs::remove_file(&path);
            return None;
        }

        fs::read(path).ok()
    }

    fn write(&self, kind: CacheKind, key: &str, bytes: &[u8]) {
        let _ = self.write_inner(kind, key, bytes);
    }

    fn write_inner(&self, kind: CacheKind, key: &str, bytes: &[u8]) -> Result<()> {
        if !self.is_enabled() || !self.settings.is_enabled(kind) {
            return Ok(());
        }

        fs::create_dir_all(&self.dir)
            .with_context(|| format!("failed to create cache directory {}", self.dir.display()))?;
        let path = self.path_for(kind, key);
        let temp_path = path.with_extension("tmp");
        fs::write(&temp_path, bytes)
            .with_context(|| format!("failed to write cache file {}", temp_path.display()))?;
        fs::rename(&temp_path, &path)
            .with_context(|| format!("failed to move cache file {}", path.display()))?;
        self.evict_oldest_entries()
    }

    fn evict_oldest_entries(&self) -> Result<()> {
        let mut entries = self.entry_infos()?;
        let mut total_size: u64 = entries.iter().map(|entry| entry.size).sum();
        if total_size <= self.max_bytes {
            return Ok(());
        }

        entries.sort_by(|left, right| left.modified.cmp(&right.modified));
        for entry in entries {
            fs::remove_file(&entry.path)
                .with_context(|| format!("failed to remove cache file {}", entry.path.display()))?;
            total_size = total_size.saturating_sub(entry.size);
            if total_size <= self.max_bytes {
                break;
            }
        }

        Ok(())
    }

    fn entry_infos(&self) -> Result<Vec<CacheEntryInfo>> {
        let mut entries = Vec::new();
        if !self.dir.exists() {
            return Ok(entries);
        }

        for entry in fs::read_dir(&self.dir)
            .with_context(|| format!("failed to read cache directory {}", self.dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            let metadata = entry.metadata()?;
            entries.push(CacheEntryInfo {
                path,
                modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                size: metadata.len(),
            });
        }

        Ok(entries)
    }

    fn stats(&self) -> Result<CacheStats> {
        let entries = self.entry_infos()?;
        Ok(CacheStats {
            dir: self.dir.clone(),
            settings_path: self.settings_path.clone(),
            max_bytes: self.max_bytes,
            ttl: self.ttl,
            entry_count: entries.len(),
            total_bytes: entries.iter().map(|entry| entry.size).sum(),
            settings: self.settings.clone(),
        })
    }

    fn clear(&self) -> Result<CacheClearResult> {
        let entries = self.entry_infos()?;
        let removed_entries = entries.len();
        let removed_bytes = entries.iter().map(|entry| entry.size).sum();

        for entry in entries {
            fs::remove_file(&entry.path)
                .with_context(|| format!("failed to remove cache file {}", entry.path.display()))?;
        }

        Ok(CacheClearResult {
            removed_entries,
            removed_bytes,
        })
    }

    fn settings(&self) -> CacheSettings {
        self.settings.clone()
    }

    fn save_settings(&mut self, settings: CacheSettings) -> Result<()> {
        self.persist_settings(&settings)?;
        self.settings = settings.clone();

        for kind in CacheKind::ALL {
            if !settings.is_enabled(kind) {
                self.clear_kind(kind)?;
            }
        }

        Ok(())
    }

    fn key_for(&self, method: &str, url: &str, body: Option<&[u8]>) -> String {
        let mut hasher = Sha1::new();
        hasher.update(method.as_bytes());
        hasher.update([0]);
        hasher.update(url.as_bytes());
        hasher.update([0]);
        if let Some(body) = body {
            hasher.update(body);
        }
        bytes_to_hex(&hasher.finalize())
    }

    fn path_for(&self, kind: CacheKind, key: &str) -> PathBuf {
        self.dir
            .join(format!("{}-{key}.cache", kind.file_prefix()))
    }

    fn is_enabled(&self) -> bool {
        self.max_bytes > 0 && !self.ttl.is_zero()
    }

    fn is_stale(&self, modified: SystemTime) -> bool {
        SystemTime::now()
            .duration_since(modified)
            .is_ok_and(|age| age > self.ttl)
    }

    fn load_settings(path: &PathBuf) -> Option<CacheSettings> {
        let bytes = fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn persist_settings(&self, settings: &CacheSettings) -> Result<()> {
        if let Some(parent) = self.settings_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create settings directory {}", parent.display())
            })?;
        }

        let temp_path = self.settings_path.with_extension("tmp");
        let bytes = serde_json::to_vec_pretty(settings).context("failed to encode cache settings")?;
        fs::write(&temp_path, bytes)
            .with_context(|| format!("failed to write {}", temp_path.display()))?;
        fs::rename(&temp_path, &self.settings_path)
            .with_context(|| format!("failed to move {}", self.settings_path.display()))?;
        Ok(())
    }

    fn clear_kind(&self, kind: CacheKind) -> Result<CacheClearResult> {
        let prefix = format!("{}-", kind.file_prefix());
        let entries: Vec<CacheEntryInfo> = self
            .entry_infos()?
            .into_iter()
            .filter(|entry| {
                entry
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&prefix))
            })
            .collect();
        let removed_entries = entries.len();
        let removed_bytes = entries.iter().map(|entry| entry.size).sum();

        for entry in entries {
            fs::remove_file(&entry.path)
                .with_context(|| format!("failed to remove cache file {}", entry.path.display()))?;
        }

        Ok(CacheClearResult {
            removed_entries,
            removed_bytes,
        })
    }
}

pub fn app_cache_dir() -> PathBuf {
    if let Some(dir) = env::var_os("MODRINTH_CACHE_DIR") {
        return PathBuf::from(dir);
    }

    if let Some(dir) = env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(dir).join("modrinth-cli");
    }

    #[cfg(target_os = "macos")]
    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home).join("Library/Caches/modrinth-cli");
    }

    #[cfg(target_os = "windows")]
    if let Some(dir) = env::var_os("LOCALAPPDATA") {
        return PathBuf::from(dir).join("modrinth-cli");
    }

    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home).join(".cache/modrinth-cli");
    }

    env::temp_dir().join("modrinth-cli")
}

fn default_cache_dir() -> PathBuf {
    app_cache_dir().join("http")
}

fn default_settings_path() -> PathBuf {
    if let Some(path) = env::var_os("MODRINTH_CACHE_SETTINGS_PATH") {
        return PathBuf::from(path);
    }

    app_cache_dir().join("cache-settings.json")
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

impl Version {
    pub fn required_dependencies(&self) -> impl Iterator<Item = &Dependency> {
        self.dependencies
            .iter()
            .filter(|dependency| dependency.dependency_type == "required")
    }

    pub fn is_public(&self) -> bool {
        match self.status.as_deref() {
            Some("listed") | Some("unlisted") | None => true,
            Some(_) => false,
        }
    }

    pub fn primary_download_file(&self) -> Option<VersionFile> {
        self.files
            .iter()
            .find(|file| file.primary && file.is_downloadable_jar())
            .cloned()
            .or_else(|| {
                self.files
                    .iter()
                    .find(|file| file.is_downloadable_jar())
                    .cloned()
            })
            .or_else(|| self.files.first().cloned())
    }
}

impl VersionFile {
    pub fn sha1(&self) -> Option<&str> {
        self.hashes.sha1.as_deref()
    }

    fn is_downloadable_jar(&self) -> bool {
        let not_auxiliary = !matches!(
            self.file_type.as_deref(),
            Some("sources-jar" | "dev-jar" | "javadoc-jar" | "signature")
        );
        not_auxiliary && self.filename.ends_with(".jar")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::minecraft::{DetectedEnvironment, Loader};
    use httpmock::Method::GET;
    use httpmock::MockServer;
    use serde_json::json;
    use std::thread;
    use tempfile::tempdir;

    #[test]
    fn fetches_game_versions_from_modrinth_tags() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/tag/game_version");
            then.status(200).json_body(json!([
                { "version": "1.21.11" },
                { "version": "future_version_x" }
            ]));
        });

        let client = ModrinthClient::new(server.base_url()).unwrap();
        let versions = client.game_versions().unwrap();

        assert_eq!(versions, vec!["1.21.11", "future_version_x"]);
    }

    #[test]
    fn search_projects_includes_environment_facets_when_present() {
        let server = MockServer::start();
        let expected_facets = serde_json::to_string(&vec![
            vec!["project_type:mod".to_string()],
            vec!["categories:fabric".to_string()],
            vec!["versions:1.21.1".to_string()],
        ])
        .unwrap();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/search")
                .query_param("query", "sodium")
                .query_param("limit", "20")
                .query_param("index", "relevance")
                .query_param("facets", &expected_facets);
            then.status(200).json_body(json!({
                "hits": [
                    {
                        "project_id": "proj1",
                        "slug": "sodium",
                        "title": "Sodium",
                        "downloads": 100000
                    }
                ]
            }));
        });

        let client = ModrinthClient::new(server.base_url()).unwrap();
        let environment = DetectedEnvironment {
            loader: Loader::Fabric,
            loader_version: Some("0.16.10".to_string()),
            minecraft_version: "1.21.1".to_string(),
        };

        let hits = client
            .search_projects("sodium", Some(&environment))
            .unwrap();

        mock.assert();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].slug, "sodium");
    }

    #[test]
    fn search_projects_omits_loader_and_version_facets_for_global_search() {
        let server = MockServer::start();
        let expected_facets =
            serde_json::to_string(&vec![vec!["project_type:mod".to_string()]]).unwrap();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/search").is_true(move |req| {
                let params = req.query_params_map();
                params.get("query") == Some(&"sodium".to_string())
                    && params.get("limit") == Some(&"20".to_string())
                    && params.get("index") == Some(&"relevance".to_string())
                    && params.get("facets") == Some(&expected_facets)
            });
            then.status(200).json_body(json!({
                "hits": [
                    {
                        "project_id": "proj1",
                        "slug": "sodium",
                        "title": "Sodium",
                        "downloads": 100000
                    }
                ]
            }));
        });

        let client = ModrinthClient::new(server.base_url()).unwrap();
        let hits = client.search_projects("sodium", None).unwrap();

        mock.assert();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].slug, "sodium");
    }

    #[test]
    fn cache_reuses_successful_get_responses() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/tag/game_version");
            then.status(200)
                .json_body(json!([{ "version": "1.21.11" }]));
        });
        let temp_dir = tempdir().unwrap();
        let client = ModrinthClient::new_with_cache(
            server.base_url(),
            ResponseCache::new(
                temp_dir.path().join("cache"),
                1024 * 1024,
                Duration::from_secs(60),
            ),
        )
        .unwrap();

        let first = client.game_versions().unwrap();
        let second = client.game_versions().unwrap();

        assert_eq!(first, vec!["1.21.11"]);
        assert_eq!(second, vec!["1.21.11"]);
        assert_eq!(mock.calls(), 1);
    }

    #[test]
    fn cached_game_versions_reads_cached_tags_without_network() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/tag/game_version");
            then.status(200)
                .json_body(json!([{ "version": "1.21.11" }]));
        });
        let temp_dir = tempdir().unwrap();
        let client = ModrinthClient::new_with_cache(
            server.base_url(),
            ResponseCache::new(
                temp_dir.path().join("cache"),
                1024 * 1024,
                Duration::from_secs(60),
            ),
        )
        .unwrap();

        assert_eq!(client.game_versions().unwrap(), vec!["1.21.11"]);
        assert_eq!(client.cached_game_versions().unwrap(), vec!["1.21.11"]);
        assert_eq!(mock.calls(), 1);
    }

    #[test]
    fn cache_evicts_oldest_entries_when_over_limit() {
        let temp_dir = tempdir().unwrap();
        let cache = ResponseCache::new(temp_dir.path().join("cache"), 20, Duration::from_secs(60));

        cache
            .write_inner(CacheKind::Projects, "oldest", b"1234567890")
            .unwrap();
        thread::sleep(Duration::from_millis(20));
        cache
            .write_inner(CacheKind::Projects, "middle", b"abcdefghij")
            .unwrap();
        thread::sleep(Duration::from_millis(20));
        cache
            .write_inner(CacheKind::Projects, "newest", b"ABCDEFGHIJ")
            .unwrap();

        assert!(cache.read(CacheKind::Projects, "oldest").is_none());
        assert_eq!(
            cache.read(CacheKind::Projects, "middle").unwrap(),
            b"abcdefghij"
        );
        assert_eq!(
            cache.read(CacheKind::Projects, "newest").unwrap(),
            b"ABCDEFGHIJ"
        );
    }

    #[test]
    fn cache_clear_removes_all_entries_and_reports_usage() {
        let temp_dir = tempdir().unwrap();
        let cache =
            ResponseCache::new(temp_dir.path().join("cache"), 1024, Duration::from_secs(60));

        cache
            .write_inner(CacheKind::Projects, "one", b"12345")
            .unwrap();
        cache
            .write_inner(CacheKind::SearchResults, "two", b"6789")
            .unwrap();

        let stats = cache.stats().unwrap();
        assert_eq!(stats.entry_count, 2);
        assert_eq!(stats.total_bytes, 9);

        let cleared = cache.clear().unwrap();
        assert_eq!(
            cleared,
            CacheClearResult {
                removed_entries: 2,
                removed_bytes: 9,
            }
        );

        let stats = cache.stats().unwrap();
        assert_eq!(stats.entry_count, 0);
        assert_eq!(stats.total_bytes, 0);
    }

    #[test]
    fn cache_settings_round_trip_and_disable_specific_kind() {
        let temp_dir = tempdir().unwrap();
        let settings_path = temp_dir.path().join("settings/cache-settings.json");
        let mut cache = ResponseCache::new_with_settings_path(
            temp_dir.path().join("cache"),
            settings_path.clone(),
            1024,
            Duration::from_secs(60),
            CacheSettings::default(),
        );

        cache
            .write_inner(CacheKind::SearchResults, "search", b"12345")
            .unwrap();
        cache
            .write_inner(CacheKind::Projects, "project", b"67890")
            .unwrap();

        let mut settings = cache.settings();
        settings.set_enabled(CacheKind::SearchResults, false);
        cache.save_settings(settings.clone()).unwrap();

        let reloaded = ResponseCache::load_settings(&settings_path).unwrap();
        assert_eq!(reloaded, settings);
        assert!(cache.read(CacheKind::SearchResults, "search").is_none());
        assert_eq!(
            cache.read(CacheKind::Projects, "project").unwrap(),
            b"67890"
        );
    }
}
