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

#[derive(Debug, Deserialize)]
struct GameVersionTag {
    version: String,
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

    pub fn game_versions(&self) -> Result<Vec<String>> {
        let tags: Vec<GameVersionTag> = self
            .http
            .get(format!("{}/tag/game_version", self.base_url))
            .send()
            .context("failed to fetch Modrinth game versions")?
            .error_for_status()
            .context("game versions request failed")?
            .json()
            .context("failed to decode Modrinth game versions response")?;

        Ok(tags.into_iter().map(|tag| tag.version).collect())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::minecraft::{DetectedEnvironment, Loader};
    use httpmock::Method::GET;
    use httpmock::MockServer;
    use serde_json::json;

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
}
