mod minecraft;
mod modrinth;
mod search;

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use minecraft::{DetectedEnvironment, MinecraftContext};
use modrinth::{ModrinthClient, Project, Version, VersionFile};
use search::{CandidateChoice, SearchCandidate, choose_best_match};
use sha1::{Digest, Sha1};

#[derive(Parser, Debug)]
#[command(
    name = "mod",
    version,
    about = "Install and update Modrinth mods for the current Minecraft directory",
    arg_required_else_help = true,
    after_help = "Examples:\n  mod install sodium\n  mod install simple voice chat\n  mod update all\n  mod update sodium"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    #[command(alias = "i")]
    Install {
        #[arg(help = "Project name or search query")]
        query: Vec<String>,
    },
    #[command(alias = "u")]
    Update {
        #[arg(help = "Installed mod name, or `all` to update everything")]
        target: Vec<String>,
    },
}

#[derive(Debug, Clone)]
struct InstalledJar {
    path: PathBuf,
    sha1: String,
}

#[derive(Debug, Clone)]
struct InstalledMod {
    jar: InstalledJar,
    version: Version,
    project: Project,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("Error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let cwd = env::current_dir().context("failed to read current working directory")?;
    let context = MinecraftContext::discover(&cwd)?;
    print_environment(&context.environment, &context.root_dir, &context.mods_dir);

    let client = ModrinthClient::new_from_env()?;

    match cli.command {
        Command::Install { query } => {
            let query = join_words(query)?;
            install_from_query(&client, &context, &query)?;
        }
        Command::Update { target } => {
            let target = join_words(target)?;
            if target.eq_ignore_ascii_case("all") {
                update_all(&client, &context)?;
            } else {
                update_named(&client, &context, &target)?;
            }
        }
    }

    Ok(())
}

fn install_from_query(
    client: &ModrinthClient,
    context: &MinecraftContext,
    query: &str,
) -> Result<()> {
    let mut inventory = build_installed_inventory(client, context)?;
    let mut project_cache = HashMap::new();
    let project = resolve_project_query(client, &context.environment, query, &mut project_cache)?;
    println!("Selected project: {} ({})", project.title, project.slug);

    let mut visited = HashSet::new();
    install_project_recursive(
        client,
        context,
        &project,
        &mut inventory,
        &mut project_cache,
        &mut visited,
        0,
    )
}

fn update_all(client: &ModrinthClient, context: &MinecraftContext) -> Result<()> {
    let installed = build_installed_inventory(client, context)?;
    if installed.is_empty() {
        println!(
            "No Modrinth-backed mods found in {}",
            context.mods_dir.display()
        );
        return Ok(());
    }

    let hashes: Vec<String> = installed
        .values()
        .map(|item| item.jar.sha1.clone())
        .collect();
    let latest_by_hash = client.latest_versions_for_hashes(&hashes, &context.environment)?;
    let mut updated = 0usize;

    for item in installed.into_values() {
        match latest_by_hash.get(&item.jar.sha1) {
            Some(latest_version) => {
                if replace_if_newer(client, context, &item, latest_version)? {
                    updated += 1;
                }
            }
            None => {
                println!(
                    "Skipped {}: no compatible update found for {} {}",
                    item.project.title,
                    context.environment.loader,
                    context.environment.minecraft_version
                );
            }
        }
    }

    if updated == 0 {
        println!("Everything is already up to date.");
    }

    Ok(())
}

fn update_named(client: &ModrinthClient, context: &MinecraftContext, query: &str) -> Result<()> {
    let installed = build_installed_inventory(client, context)?;
    if installed.is_empty() {
        bail!(
            "no installed Modrinth-backed mods were found in {}",
            context.mods_dir.display()
        );
    }

    let candidates: Vec<SearchCandidate> = installed
        .values()
        .map(|item| SearchCandidate {
            key: item.project.id.clone(),
            slug: item.project.slug.clone(),
            title: item.project.title.clone(),
            downloads: item.project.downloads,
        })
        .collect();

    let selected_project_id = match choose_best_match(query, &candidates) {
        CandidateChoice::Selected(candidate) => candidate.key,
        CandidateChoice::Ambiguous(options) => {
            let suggestions = format_suggestions(&options);
            bail!(
                "update target `{}` is ambiguous. Closest installed matches:\n{}",
                query,
                suggestions
            );
        }
        CandidateChoice::NoMatches => {
            bail!(
                "`{}` is not installed in {}. Run `mod install {}` instead.",
                query,
                context.mods_dir.display(),
                query
            );
        }
    };

    let item = installed
        .into_values()
        .find(|entry| entry.project.id == selected_project_id)
        .ok_or_else(|| anyhow!("installed project disappeared during selection"))?;

    let latest_by_hash =
        client.latest_versions_for_hashes(&[item.jar.sha1.clone()], &context.environment)?;

    match latest_by_hash.get(&item.jar.sha1) {
        Some(latest_version) => {
            replace_if_newer(client, context, &item, latest_version)?;
            Ok(())
        }
        None => bail!(
            "no compatible update found for {} on {} {}",
            item.project.title,
            context.environment.loader,
            context.environment.minecraft_version
        ),
    }
}

fn install_project_recursive(
    client: &ModrinthClient,
    context: &MinecraftContext,
    project: &Project,
    inventory: &mut HashMap<String, InstalledMod>,
    project_cache: &mut HashMap<String, Project>,
    visited: &mut HashSet<String>,
    depth: usize,
) -> Result<()> {
    if !visited.insert(project.id.clone()) {
        return Ok(());
    }

    let prefix = "  ".repeat(depth);
    let versions = client.project_versions(&project.id, &context.environment)?;
    let version = select_version_for_environment(versions).with_context(|| {
        format!(
            "no compatible version found for {} on {} {}",
            project.title, context.environment.loader, context.environment.minecraft_version
        )
    })?;

    for dependency in version.required_dependencies() {
        if let Some(project_id) = dependency.project_id.clone() {
            if inventory.contains_key(&project_id) {
                continue;
            }

            let dependency_project = fetch_project_cached(client, project_cache, &project_id)?
                .ok_or_else(|| {
                    anyhow!("dependency project {project_id} could not be loaded from Modrinth")
                })?;

            println!(
                "{}Installing required dependency: {} ({})",
                prefix, dependency_project.title, dependency_project.slug
            );

            install_project_recursive(
                client,
                context,
                &dependency_project,
                inventory,
                project_cache,
                visited,
                depth + 1,
            )?;
        } else if let Some(file_name) = dependency.file_name.as_deref() {
            println!(
                "{}Skipping external required dependency {}: not addressable through Modrinth project metadata",
                prefix, file_name
            );
        }
    }

    let installed = inventory.get(&project.id).cloned();
    install_selected_version(client, context, project, version, installed, inventory)?;
    Ok(())
}

fn install_selected_version(
    client: &ModrinthClient,
    context: &MinecraftContext,
    project: &Project,
    version: Version,
    installed: Option<InstalledMod>,
    inventory: &mut HashMap<String, InstalledMod>,
) -> Result<()> {
    let file = version.primary_download_file().ok_or_else(|| {
        anyhow!(
            "version {} for {} does not expose a downloadable jar file",
            version.version_number,
            project.title
        )
    })?;

    if let Some(existing) = installed.as_ref() {
        if file.sha1().is_some_and(|hash| hash == existing.jar.sha1) {
            println!(
                "{} is already at {}",
                project.title,
                version.version_number.as_str()
            );
            return Ok(());
        }
    }

    let destination = context.mods_dir.join(&file.filename);
    download_and_place_file(
        client,
        &file,
        &destination,
        installed.as_ref().map(|item| item.jar.path.as_path()),
    )?;

    println!(
        "Installed {} {} -> {}",
        project.title,
        version.version_number,
        destination.display()
    );

    let expected_sha1 = file
        .sha1()
        .ok_or_else(|| anyhow!("downloaded file metadata was missing sha1"))?;

    inventory.insert(
        project.id.clone(),
        InstalledMod {
            jar: InstalledJar {
                path: destination,
                sha1: expected_sha1.to_string(),
            },
            version,
            project: project.clone(),
        },
    );

    Ok(())
}

fn replace_if_newer(
    client: &ModrinthClient,
    context: &MinecraftContext,
    installed: &InstalledMod,
    latest_version: &Version,
) -> Result<bool> {
    let latest_file = latest_version.primary_download_file().ok_or_else(|| {
        anyhow!(
            "latest version {} for {} has no downloadable jar",
            latest_version.version_number,
            installed.project.title
        )
    })?;

    if latest_file
        .sha1()
        .is_some_and(|latest_hash| latest_hash == installed.jar.sha1)
    {
        println!(
            "{} is already current ({})",
            installed.project.title, installed.version.version_number
        );
        return Ok(false);
    }

    let destination = context.mods_dir.join(&latest_file.filename);
    download_and_place_file(
        client,
        &latest_file,
        &destination,
        Some(installed.jar.path.as_path()),
    )?;

    println!(
        "Updated {}: {} -> {}",
        installed.project.title, installed.version.version_number, latest_version.version_number
    );
    Ok(true)
}

fn resolve_project_query(
    client: &ModrinthClient,
    environment: &DetectedEnvironment,
    query: &str,
    project_cache: &mut HashMap<String, Project>,
) -> Result<Project> {
    let exact_slug = slugify_query(query);
    if !exact_slug.is_empty() {
        if let Some(project) = client.get_project(&exact_slug)? {
            project_cache.insert(project.id.clone(), project.clone());
            return Ok(project);
        }
    }

    let hits = client.search_projects(query, environment)?;
    if hits.is_empty() {
        bail!("no Modrinth mods matched `{}`", query);
    }

    let candidates: Vec<SearchCandidate> = hits
        .iter()
        .map(|hit| SearchCandidate {
            key: hit.project_id.clone().unwrap_or_else(|| hit.slug.clone()),
            slug: hit.slug.clone(),
            title: hit.title.clone(),
            downloads: hit.downloads,
        })
        .collect();

    let selected_key = match choose_best_match(query, &candidates) {
        CandidateChoice::Selected(candidate) => candidate.key,
        CandidateChoice::Ambiguous(options) => {
            let suggestions = format_suggestions(&options);
            bail!(
                "search for `{}` was too ambiguous. Closest matches:\n{}",
                query,
                suggestions
            );
        }
        CandidateChoice::NoMatches => bail!("no Modrinth mods matched `{}`", query),
    };

    let selected_hit = hits
        .iter()
        .find(|hit| {
            hit.project_id.as_deref() == Some(selected_key.as_str()) || hit.slug == selected_key
        })
        .ok_or_else(|| anyhow!("selected search hit disappeared before project fetch"))?;

    let lookup_key = selected_hit
        .project_id
        .as_deref()
        .unwrap_or(selected_hit.slug.as_str());
    let project = client.get_project(lookup_key)?.ok_or_else(|| {
        anyhow!(
            "Modrinth project {} could not be fetched",
            selected_hit.title
        )
    })?;
    project_cache.insert(project.id.clone(), project.clone());
    Ok(project)
}

fn build_installed_inventory(
    client: &ModrinthClient,
    context: &MinecraftContext,
) -> Result<HashMap<String, InstalledMod>> {
    fs::create_dir_all(&context.mods_dir)
        .with_context(|| format!("failed to create {}", context.mods_dir.display()))?;

    let jars = scan_installed_jars(&context.mods_dir)?;
    let mut project_cache = HashMap::new();
    let mut installed = HashMap::new();

    for jar in jars {
        let Some(version) = client.version_from_hash(&jar.sha1)? else {
            continue;
        };

        let Some(project) = fetch_project_cached(client, &mut project_cache, &version.project_id)?
        else {
            continue;
        };

        installed.insert(
            project.id.clone(),
            InstalledMod {
                jar,
                version,
                project,
            },
        );
    }

    Ok(installed)
}

fn fetch_project_cached(
    client: &ModrinthClient,
    cache: &mut HashMap<String, Project>,
    id_or_slug: &str,
) -> Result<Option<Project>> {
    if let Some(project) = cache.get(id_or_slug) {
        return Ok(Some(project.clone()));
    }

    let Some(project) = client.get_project(id_or_slug)? else {
        return Ok(None);
    };

    cache.insert(project.id.clone(), project.clone());
    cache.insert(project.slug.clone(), project.clone());
    Ok(Some(project))
}

fn scan_installed_jars(mods_dir: &Path) -> Result<Vec<InstalledJar>> {
    let mut jars = Vec::new();
    if !mods_dir.exists() {
        return Ok(jars);
    }

    for entry in
        fs::read_dir(mods_dir).with_context(|| format!("failed to read {}", mods_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("jar") {
            continue;
        }

        jars.push(InstalledJar {
            sha1: compute_sha1(&path)?,
            path,
        });
    }

    Ok(jars)
}

fn compute_sha1(path: &Path) -> Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = Sha1::new();
    let mut buffer = [0u8; 8192];

    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(bytes_to_hex(&hasher.finalize()))
}

fn download_and_place_file(
    client: &ModrinthClient,
    file: &VersionFile,
    destination: &Path,
    previous_path: Option<&Path>,
) -> Result<()> {
    let temp_path = destination.with_extension("download.tmp");
    if temp_path.exists() {
        fs::remove_file(&temp_path)
            .with_context(|| format!("failed to remove {}", temp_path.display()))?;
    }

    let mut response = client.download(&file.url)?;
    let mut output = File::create(&temp_path)
        .with_context(|| format!("failed to create {}", temp_path.display()))?;
    io::copy(&mut response, &mut output)
        .with_context(|| format!("failed to write {}", temp_path.display()))?;
    output.flush()?;

    if let Some(expected_sha1) = file.sha1() {
        let actual_sha1 = compute_sha1(&temp_path)?;
        if actual_sha1 != expected_sha1 {
            bail!(
                "downloaded file hash mismatch for {}: expected {}, got {}",
                file.filename,
                expected_sha1,
                actual_sha1
            );
        }
    }

    if destination.exists() {
        fs::remove_file(destination)
            .with_context(|| format!("failed to replace {}", destination.display()))?;
    }

    fs::rename(&temp_path, destination)
        .with_context(|| format!("failed to move {} into place", destination.display()))?;

    if let Some(previous) = previous_path {
        if previous != destination && previous.exists() {
            fs::remove_file(previous)
                .with_context(|| format!("failed to remove old mod {}", previous.display()))?;
        }
    }

    Ok(())
}

fn select_version_for_environment(versions: Vec<Version>) -> Option<Version> {
    let listed_only: Vec<Version> = versions
        .into_iter()
        .filter(|version| version.is_public())
        .collect();
    if listed_only.is_empty() {
        return None;
    }

    for version_type in ["release", "beta", "alpha"] {
        let mut matches: Vec<Version> = listed_only
            .iter()
            .filter(|version| version.version_type == version_type)
            .cloned()
            .collect();

        if matches.is_empty() {
            continue;
        }

        matches.sort_by(|left, right| {
            right
                .featured
                .cmp(&left.featured)
                .then_with(|| right.date_published.cmp(&left.date_published))
                .then_with(|| right.downloads.cmp(&left.downloads))
        });
        return matches.into_iter().next();
    }

    None
}

fn slugify_query(query: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;

    for ch in query.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }

    slug.trim_matches('-').to_string()
}

fn join_words(parts: Vec<String>) -> Result<String> {
    let joined = parts.join(" ").trim().to_string();
    if joined.is_empty() {
        bail!("expected a search query or mod name");
    }
    Ok(joined)
}

fn format_suggestions(candidates: &[SearchCandidate]) -> String {
    candidates
        .iter()
        .take(5)
        .map(|candidate| format!("- {} ({})", candidate.title, candidate.slug))
        .collect::<Vec<_>>()
        .join("\n")
}

fn print_environment(environment: &DetectedEnvironment, root_dir: &Path, mods_dir: &Path) {
    match environment.loader_version.as_deref() {
        Some(loader_version) => println!(
            "Detected {} {} on Minecraft {}",
            environment.loader, loader_version, environment.minecraft_version
        ),
        None => println!(
            "Detected {} on Minecraft {}",
            environment.loader, environment.minecraft_version
        ),
    }
    println!("Root: {}", root_dir.display());
    println!("Mods: {}", mods_dir.display());
}

fn bytes_to_hex(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::Method::{GET, POST};
    use httpmock::MockServer;
    use serde_json::json;
    use tempfile::tempdir;

    fn mock_project(id: &str, slug: &str, title: &str, downloads: u64) -> serde_json::Value {
        json!({
            "id": id,
            "slug": slug,
            "title": title,
            "description": format!("{title} description"),
            "downloads": downloads
        })
    }

    fn mock_version(
        id: &str,
        project_id: &str,
        version_number: &str,
        version_type: &str,
        sha1: &str,
        filename: &str,
        url: &str,
        dependencies: serde_json::Value,
    ) -> serde_json::Value {
        json!({
            "id": id,
            "project_id": project_id,
            "author_id": "author",
            "name": format!("Version {version_number}"),
            "version_number": version_number,
            "changelog": null,
            "dependencies": dependencies,
            "game_versions": ["1.21.1"],
            "version_type": version_type,
            "loaders": ["fabric"],
            "featured": true,
            "status": "listed",
            "requested_status": null,
            "date_published": "2026-03-28T00:00:00Z",
            "downloads": 1000,
            "changelog_url": null,
            "files": [{
                "hashes": { "sha1": sha1, "sha512": "unused" },
                "url": url,
                "filename": filename,
                "primary": true,
                "size": 1234,
                "file_type": null
            }]
        })
    }

    fn write_fabric_root(root: &Path) {
        fs::create_dir_all(root.join("mods")).unwrap();
        let fabric_dir = root.join("libraries/net/fabricmc/fabric-loader/0.16.10");
        fs::create_dir_all(&fabric_dir).unwrap();
        fs::write(
            fabric_dir.join("fabric-loader-0.16.10-1.21.1.jar"),
            b"loader",
        )
        .unwrap();
    }

    fn write_modern_fabric_library_root(root: &Path) {
        fs::create_dir_all(root.join("mods")).unwrap();

        let intermediary_dir = root.join("libraries/net/fabricmc/intermediary/1.21.11");
        fs::create_dir_all(&intermediary_dir).unwrap();
        fs::write(
            intermediary_dir.join("intermediary-1.21.11.jar"),
            b"intermediary",
        )
        .unwrap();

        for loader_version in ["0.18.4", "0.18.5"] {
            let fabric_dir = root.join(format!(
                "libraries/net/fabricmc/fabric-loader/{loader_version}"
            ));
            fs::create_dir_all(&fabric_dir).unwrap();
            fs::write(
                fabric_dir.join(format!("fabric-loader-{loader_version}.jar")),
                b"loader",
            )
            .unwrap();
        }
    }

    fn write_modern_dot_fabric_root(root: &Path) {
        fs::create_dir_all(root.join("mods")).unwrap();
        let fabric_dir = root.join(".fabric/server");
        fs::create_dir_all(&fabric_dir).unwrap();

        fs::write(
            fabric_dir.join("fabric-loader-server-0.18.4-minecraft-1.21.11.jar"),
            b"loader",
        )
        .unwrap();
        fs::write(
            fabric_dir.join("fabric-loader-server-0.18.5-minecraft-1.21.11.jar"),
            b"loader",
        )
        .unwrap();
    }

    fn write_fabric_remapped_root(root: &Path) {
        fs::create_dir_all(root.join("mods")).unwrap();

        for folder in [
            "minecraft-1.21.10-0.18.1",
            "minecraft-1.21.11-0.18.4",
            "minecraft-1.21.11-0.18.5",
        ] {
            let remapped_dir = root.join(format!(".fabric/remappedJars/{folder}"));
            fs::create_dir_all(&remapped_dir).unwrap();
            fs::write(remapped_dir.join("client-intermediary.jar"), b"client").unwrap();
        }
    }

    fn write_prism_instance_root(instance_root: &Path) {
        let minecraft_root = instance_root.join("minecraft");
        fs::create_dir_all(minecraft_root.join("mods")).unwrap();
        fs::write(
            instance_root.join("mmc-pack.json"),
            r#"{
                "components": [
                    { "uid": "net.minecraft", "version": "1.21.11" },
                    { "uid": "net.fabricmc.fabric-loader", "version": "0.18.5" }
                ]
            }"#,
        )
        .unwrap();
    }

    #[test]
    fn detects_root_from_mods_directory() {
        let temp = tempdir().unwrap();
        write_fabric_root(temp.path());

        let context = MinecraftContext::discover(&temp.path().join("mods")).unwrap();
        assert_eq!(
            fs::canonicalize(&context.root_dir).unwrap(),
            fs::canonicalize(temp.path()).unwrap()
        );
        assert_eq!(
            fs::canonicalize(&context.mods_dir).unwrap(),
            fs::canonicalize(temp.path().join("mods")).unwrap()
        );
        assert_eq!(context.environment.loader.as_str(), "fabric");
        assert_eq!(context.environment.minecraft_version, "1.21.1");
        assert_eq!(
            context.environment.loader_version.as_deref(),
            Some("0.16.10")
        );
    }

    #[test]
    fn detects_modern_fabric_library_layout() {
        let temp = tempdir().unwrap();
        write_modern_fabric_library_root(temp.path());

        let context = MinecraftContext::discover(&temp.path().join("mods")).unwrap();
        assert_eq!(context.environment.loader.as_str(), "fabric");
        assert_eq!(context.environment.minecraft_version, "1.21.11");
        assert_eq!(
            context.environment.loader_version.as_deref(),
            Some("0.18.5")
        );
    }

    #[test]
    fn detects_dot_fabric_server_layout_and_prefers_latest_loader() {
        let temp = tempdir().unwrap();
        write_modern_dot_fabric_root(temp.path());

        let context = MinecraftContext::discover(&temp.path().join("mods")).unwrap();
        assert_eq!(context.environment.loader.as_str(), "fabric");
        assert_eq!(context.environment.minecraft_version, "1.21.11");
        assert_eq!(
            context.environment.loader_version.as_deref(),
            Some("0.18.5")
        );
    }

    #[test]
    fn detects_fabric_remapped_jars_layout() {
        let temp = tempdir().unwrap();
        write_fabric_remapped_root(temp.path());

        let context = MinecraftContext::discover(&temp.path().join("mods")).unwrap();
        assert_eq!(context.environment.loader.as_str(), "fabric");
        assert_eq!(context.environment.minecraft_version, "1.21.11");
        assert_eq!(
            context.environment.loader_version.as_deref(),
            Some("0.18.5")
        );
    }

    #[test]
    fn detects_prism_launcher_manifest_from_instance_parent() {
        let temp = tempdir().unwrap();
        let instance_root = temp.path().join("macos boosted beta");
        write_prism_instance_root(&instance_root);

        let context = MinecraftContext::discover(&instance_root.join("minecraft/mods")).unwrap();
        assert_eq!(context.environment.loader.as_str(), "fabric");
        assert_eq!(context.environment.minecraft_version, "1.21.11");
        assert_eq!(
            context.environment.loader_version.as_deref(),
            Some("0.18.5")
        );
    }

    #[test]
    fn prefers_best_release_version() {
        let release = serde_json::from_value::<Version>(mock_version(
            "v1",
            "project",
            "1.2.0",
            "release",
            "a1",
            "release.jar",
            "https://example.com/release.jar",
            json!([]),
        ))
        .unwrap();
        let beta = serde_json::from_value::<Version>(mock_version(
            "v2",
            "project",
            "1.3.0-beta",
            "beta",
            "b1",
            "beta.jar",
            "https://example.com/beta.jar",
            json!([]),
        ))
        .unwrap();

        let selected = select_version_for_environment(vec![beta, release]).unwrap();
        assert_eq!(selected.version_number, "1.2.0");
    }

    #[test]
    fn install_query_fetches_fuzzy_match_and_dependency() {
        let temp = tempdir().unwrap();
        write_fabric_root(temp.path());
        let root = temp.path();
        let context = MinecraftContext::discover(root).unwrap();
        let server = MockServer::start();

        let dependency_file = b"dependency-jar";
        let dependency_sha1 = bytes_to_hex(&Sha1::digest(dependency_file));
        let mod_file = b"main-mod-jar";
        let mod_sha1 = bytes_to_hex(&Sha1::digest(mod_file));

        server.mock(|when, then| {
            when.method(GET).path("/project/shulker-tooltips");
            then.status(404);
        });

        server.mock(|when, then| {
            when.method(GET).path("/search");
            then.status(200).json_body(json!({
                "hits": [
                    {
                        "project_id": "dep1",
                        "slug": "api-lib",
                        "title": "API Lib",
                        "downloads": 50
                    },
                    {
                        "project_id": "proj1",
                        "slug": "shulkerboxtooltips",
                        "title": "ShulkerBox Tooltips",
                        "downloads": 5000
                    }
                ]
            }));
        });

        server.mock(|when, then| {
            when.method(GET).path("/project/proj1");
            then.status(200).json_body(mock_project(
                "proj1",
                "shulkerboxtooltips",
                "ShulkerBox Tooltips",
                5000,
            ));
        });

        server.mock(|when, then| {
            when.method(GET).path("/project/dep1");
            then.status(200)
                .json_body(mock_project("dep1", "api-lib", "API Lib", 50));
        });

        server.mock(|when, then| {
            when.method(GET).path("/project/proj1/version");
            then.status(200).json_body(json!([mock_version(
                "ver-main",
                "proj1",
                "2.0.0",
                "release",
                &mod_sha1,
                "shulkerboxtooltips.jar",
                &server.url("/files/shulkerboxtooltips.jar"),
                json!([
                    {
                        "version_id": null,
                        "project_id": "dep1",
                        "file_name": null,
                        "dependency_type": "required"
                    }
                ])
            )]));
        });

        server.mock(|when, then| {
            when.method(GET).path("/project/dep1/version");
            then.status(200).json_body(json!([mock_version(
                "ver-dep",
                "dep1",
                "1.0.0",
                "release",
                &dependency_sha1,
                "api-lib.jar",
                &server.url("/files/api-lib.jar"),
                json!([])
            )]));
        });

        server.mock(|when, then| {
            when.method(GET).path("/files/api-lib.jar");
            then.status(200).body(dependency_file);
        });

        server.mock(|when, then| {
            when.method(GET).path("/files/shulkerboxtooltips.jar");
            then.status(200).body(mod_file);
        });

        let client = ModrinthClient::new(server.base_url()).unwrap();
        install_from_query(&client, &context, "shulker tooltips").unwrap();

        let dep_path = root.join("mods/api-lib.jar");
        let mod_path = root.join("mods/shulkerboxtooltips.jar");
        assert!(dep_path.exists());
        assert!(mod_path.exists());
        assert_eq!(compute_sha1(&dep_path).unwrap(), dependency_sha1);
        assert_eq!(compute_sha1(&mod_path).unwrap(), mod_sha1);
    }

    #[test]
    fn update_all_replaces_outdated_mods() {
        let temp = tempdir().unwrap();
        write_fabric_root(temp.path());
        let root = temp.path();
        let context = MinecraftContext::discover(root).unwrap();
        let server = MockServer::start();

        let current_bytes = b"old-version";
        let current_sha1 = bytes_to_hex(&Sha1::digest(current_bytes));
        let latest_bytes = b"new-version";
        let latest_sha1 = bytes_to_hex(&Sha1::digest(latest_bytes));

        let installed_path = root.join("mods/sodium.jar");
        fs::write(&installed_path, current_bytes).unwrap();

        server.mock(|when, then| {
            when.method(GET)
                .path(format!("/version_file/{}", current_sha1))
                .query_param("algorithm", "sha1");
            then.status(200).json_body(mock_version(
                "ver-old",
                "proj-sodium",
                "1.0.0",
                "release",
                &current_sha1,
                "sodium.jar",
                &server.url("/files/sodium-1.0.0.jar"),
                json!([]),
            ));
        });

        server.mock(|when, then| {
            when.method(GET).path("/project/proj-sodium");
            then.status(200)
                .json_body(mock_project("proj-sodium", "sodium", "Sodium", 100000));
        });

        server.mock(|when, then| {
            when.method(POST).path("/version_files/update");
            then.status(200).json_body(json!({
                current_sha1.clone(): mock_version(
                    "ver-new",
                    "proj-sodium",
                    "2.0.0",
                    "release",
                    &latest_sha1,
                    "sodium.jar",
                    &server.url("/files/sodium-2.0.0.jar"),
                    json!([])
                )
            }));
        });

        server.mock(|when, then| {
            when.method(GET).path("/files/sodium-2.0.0.jar");
            then.status(200).body(latest_bytes);
        });

        let client = ModrinthClient::new(server.base_url()).unwrap();
        update_all(&client, &context).unwrap();

        assert_eq!(compute_sha1(&installed_path).unwrap(), latest_sha1);
    }
}
