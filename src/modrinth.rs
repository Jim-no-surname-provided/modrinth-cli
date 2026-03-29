use std::collections::HashMap;
use std::env;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use serde::{Deserialize, Serialize};

use crate::minecraft::DetectedEnvironment;

const DEFAULT_API_BASE: &str = "https://api.modrinth.com/v2";

#[derive(Debug, Clone)]
pub struct ModrinthClient {
    base_url: String,
    http: Client,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Project {
    pub id: String,
    pub slug: String,
    pub title: String,
    #[serde(default)]
    pub _description: String,
    #[serde(default)]
    pub downloads: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SearchHit {
    pub project_id: Option<String>,
    pub slug: String,
    pub title: String,
    #[serde(default)]
    pub downloads: u64,
}

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
pub struct Dependency {
    #[serde(rename = "version_id")]
    pub _version_id: Option<String>,
    pub project_id: Option<String>,
    pub file_name: Option<String>,
    pub dependency_type: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VersionFile {
    pub hashes: FileHashes,
    pub url: String,
    pub filename: String,
    pub primary: bool,
    pub file_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
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

impl ModrinthClient {
    pub fn new(base_url: String) -> Result<Self> {
        let http = Client::builder()
            .user_agent(format!("modrinth-cli/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .context("failed to build HTTP client")?;

        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            http,
        })
    }

    pub fn new_from_env() -> Result<Self> {
        let base_url =
            env::var("MODRINTH_API_BASE").unwrap_or_else(|_| DEFAULT_API_BASE.to_string());
        Self::new(base_url)
    }

    pub fn get_project(&self, id_or_slug: &str) -> Result<Option<Project>> {
        let url = format!("{}/project/{}", self.base_url, id_or_slug);
        let response = self
            .http
            .get(url)
            .send()
            .context("failed to fetch project")?;

        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }

        Ok(Some(
            response
                .error_for_status()
                .context("project request failed")?
                .json()
                .context("failed to decode project response")?,
        ))
    }

    pub fn search_projects(
        &self,
        query: &str,
        environment: &DetectedEnvironment,
    ) -> Result<Vec<SearchHit>> {
        let facets = serde_json::to_string(&vec![
            vec!["project_type:mod".to_string()],
            vec![format!("categories:{}", environment.loader.as_str())],
            vec![format!("versions:{}", environment.minecraft_version)],
        ])?;
        let url = format!(
            "{}/search?query={}&limit=20&index=relevance&facets={}",
            self.base_url,
            urlencoding::encode(query),
            urlencoding::encode(&facets)
        );

        let response = self
            .http
            .get(url)
            .send()
            .context("failed to search Modrinth")?
            .error_for_status()
            .context("Modrinth search failed")?;

        let parsed: SearchResponse = response
            .json()
            .context("failed to decode Modrinth search response")?;
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

        self.http
            .get(url)
            .send()
            .context("failed to fetch project versions")?
            .error_for_status()
            .context("project versions request failed")?
            .json()
            .context("failed to decode project versions response")
    }

    pub fn version_from_hash(&self, sha1: &str) -> Result<Option<Version>> {
        let response = self
            .http
            .get(format!(
                "{}/version_file/{}?algorithm=sha1",
                self.base_url, sha1
            ))
            .send()
            .context("failed to lookup installed mod by hash")?;

        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }

        Ok(Some(
            response
                .error_for_status()
                .context("version-from-hash request failed")?
                .json()
                .context("failed to decode version-from-hash response")?,
        ))
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

        self.http
            .post(format!("{}/version_files/update", self.base_url))
            .json(&body)
            .send()
            .context("failed to query latest versions from hashes")?
            .error_for_status()
            .context("latest versions request failed")?
            .json()
            .context("failed to decode latest versions response")
    }

    pub fn download(&self, url: &str) -> Result<Response> {
        self.http
            .get(url)
            .send()
            .with_context(|| format!("failed to download {}", url))?
            .error_for_status()
            .with_context(|| format!("download failed for {}", url))
    }
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
