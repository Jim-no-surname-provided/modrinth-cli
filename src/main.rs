mod minecraft;
mod modrinth;
mod search;
mod tui;

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use minecraft::{DetectedEnvironment, GameVersionIndex, MinecraftContext};
use modrinth::{CacheKind, CacheStats, ModrinthClient, Project, SearchHit, Version, VersionFile};
use search::{CandidateChoice, SearchCandidate, choose_best_match, rank_candidates};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use tui::{DynamicInstallerOutcome, InstallerSelection, run_cache_settings_editor, run_dynamic_installer};

const SEARCH_RESULT_LIMIT: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchSort {
    BestMatch,
    Downloads,
    Title,
    Slug,
}

#[derive(Parser, Debug)]
#[command(
    name = "mod",
    version,
    about = "Search, install, and update Modrinth mods",
    arg_required_else_help = true,
    after_help = "Examples:\n  mod search sodium\n  mod s voice chat\n  mod search --downloads sodium\n  mod search --title sodium\n  mod search --global sodium\n  mod install sodium\n  mod install simple voice chat\n  mod tui\n  mod update all\n  mod update sodium\n  mod cache\n  mod cache clear\n  mod cache settings"
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
    #[command(
        name = "dynamic-installer",
        alias = "tui",
        about = "Open an interactive terminal UI for live search and multi-select install"
    )]
    DynamicInstaller,
    #[command(about = "Check for or apply mod loader updates")]
    Loader {
        #[command(subcommand)]
        command: LoaderCommand,
    },
    #[command(about = "List installed mods and show available updates")]
    List {
        #[arg(long, help = "Include beta and alpha mod updates")]
        include_prerelease: bool,
    },
    #[command(alias = "u")]
    Update {
        #[arg(help = "Installed mod name, or `all` to update everything")]
        target: Vec<String>,
        #[arg(long, help = "Include beta and alpha mod updates")]
        include_prerelease: bool,
    },
    #[command(alias = "s")]
    Search {
        #[arg(help = "Project name or search query")]
        query: Vec<String>,
        #[arg(
            long,
            help = "Search all Modrinth mods without filtering by detected loader or Minecraft version"
        )]
        global: bool,
        #[arg(
            long,
            conflicts_with_all = ["best_match", "title", "slug"],
            help = "Sort results by downloads, highest first"
        )]
        downloads: bool,
        #[arg(
            long = "best-match",
            conflicts_with_all = ["downloads", "title", "slug"],
            help = "Sort results by fuzzy match quality (default)"
        )]
        best_match: bool,
        #[arg(
            long,
            conflicts_with_all = ["downloads", "best_match", "slug"],
            help = "Sort results alphabetically by title"
        )]
        title: bool,
        #[arg(
            long,
            conflicts_with_all = ["downloads", "best_match", "title"],
            help = "Sort results alphabetically by slug"
        )]
        slug: bool,
    },
    #[command(
        about = "Inspect or clear the local Modrinth API cache",
        arg_required_else_help = false
    )]
    Cache {
        #[command(subcommand)]
        command: Option<CacheCommand>,
    },
}

#[derive(Subcommand, Debug)]
enum LoaderCommand {
    #[command(about = "Check for a compatible mod loader update")]
    Check,
    #[command(about = "Apply a compatible mod loader update")]
    Update,
}

#[derive(Subcommand, Debug)]
enum CacheCommand {
    #[command(about = "Delete all cached Modrinth API responses")]
    Clear,
    #[command(about = "Open an interactive ratatui screen to choose what gets cached")]
    Settings,
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

#[derive(Debug, Clone)]
struct InstalledJarMetadata {
    path: PathBuf,
    size: u64,
    modified: FileStamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct FileStamp {
    secs_since_epoch: u64,
    nanos_since_second: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct InstalledInventoryCache {
    entries: Vec<InstalledInventoryCacheEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InstalledInventoryCacheEntry {
    path: PathBuf,
    size: u64,
    modified: FileStamp,
    resolved: Option<CachedInstalledMod>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedInstalledMod {
    sha1: String,
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
    let mut client = ModrinthClient::new_from_env()?;
    match cli.command {
        Command::Install { query } => {
            let query = join_words(query)?;
            let known_game_versions = load_known_game_versions(&client);
            let context = discover_context(&cwd, known_game_versions.as_ref())?;
            print_environment(&context.environment, &context.root_dir, &context.mods_dir);
            install_from_query(&client, &context, &query)?;
        }
        Command::DynamicInstaller => {
            let known_game_versions = load_known_game_versions(&client);
            let context = discover_context(&cwd, known_game_versions.as_ref())?;
            let installed = build_installed_inventory(&client, &context)?;
            match run_dynamic_installer(
                &client,
                &context.environment,
                build_installed_project_labels(&installed),
            )? {
                DynamicInstallerOutcome::Cancelled => {
                    println!("Dynamic installer cancelled.");
                }   
                DynamicInstallerOutcome::Install(selections) => {
                    print_environment(&context.environment, &context.root_dir, &context.mods_dir);
                    install_projects_from_selections_with_inventory(
                        &client,
                        &context,
                        &selections,
                        Some(installed),
                    )?;
                }
            }
        }
        Command::Loader { command } => {
            let known_game_versions = load_known_game_versions(&client);
            let context = discover_context(&cwd, known_game_versions.as_ref())?;

            match command {
                LoaderCommand::Check => check_loader(&context)?,
                LoaderCommand::Update => update_loader(&context)?,
            }
        }
        Command::List { include_prerelease } => {
            let known_game_versions = load_known_game_versions(&client);
            let context = discover_context(&cwd, known_game_versions.as_ref())?;
            print_environment(&context.environment, &context.root_dir, &context.mods_dir);
            println!();
            list_mods(&client, &context, include_prerelease)?;
            println!();
            if let Err(error) = check_loader(&context) {
                eprintln!("Loader check failed: {error:#}");
            }
        }
        Command::Update {
            target,
            include_prerelease,
        } => {
            let target = join_words(target)?;
            let known_game_versions = load_known_game_versions(&client);
            let context = discover_context(&cwd, known_game_versions.as_ref())?;
            print_environment(&context.environment, &context.root_dir, &context.mods_dir);
            if target.eq_ignore_ascii_case("all") {
                update_all(&client, &context, include_prerelease)?;
            } else {
                update_named(&client, &context, &target, include_prerelease)?;
            }
        }
        Command::Search {
            query,
            global,
            downloads,
            best_match,
            title,
            slug,
        } => {
            let query = join_words(query)?;
            let sort = search_sort_from_flags(downloads, best_match, title, slug);
            let known_game_versions = (!global).then(|| load_known_game_versions(&client)).flatten();
            print!(
                "{}",
                execute_search(
                    &client,
                    &cwd,
                    known_game_versions.as_ref(),
                    &query,
                    global,
                    sort,
                )?
            );
        }
        Command::Cache { command } => {
            handle_cache_command(&mut client, command)?;
        }
    }

    Ok(())
}

fn load_known_game_versions(client: &ModrinthClient) -> Option<GameVersionIndex> {
    client
        .cached_game_versions()
        .or_else(|| client.game_versions().ok())
        .map(GameVersionIndex::new)
}

fn handle_cache_command(client: &mut ModrinthClient, command: Option<CacheCommand>) -> Result<()> {
    match command {
        None => {
            let stats = client.cache_stats()?;
            print!("{}", format_cache_stats(&stats));
        }
        Some(CacheCommand::Clear) => {
            let cleared = client.clear_cache()?;
            let stats = client.cache_stats()?;
            println!(
                "Cleared {} cached response{} ({})",
                cleared.removed_entries,
                if cleared.removed_entries == 1 {
                    ""
                } else {
                    "s"
                },
                format_bytes(cleared.removed_bytes)
            );
            println!("Cache directory: {}", stats.dir.display());
        }
        Some(CacheCommand::Settings) => {
            let initial = client.cache_settings();
            match run_cache_settings_editor(&initial)? {
                Some(updated) => {
                    client.save_cache_settings(updated)?;
                    let stats = client.cache_stats()?;
                    println!("Saved cache settings.");
                    println!("Settings file: {}", stats.settings_path.display());
                }
                None => {
                    println!("Cache settings unchanged.");
                }
            }
        }
    }

    Ok(())
}

fn discover_context(
    cwd: &Path,
    known_game_versions: Option<&GameVersionIndex>,
) -> Result<MinecraftContext> {
    MinecraftContext::discover_with_game_versions(cwd, known_game_versions)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoaderManifest {
    game_versions: Vec<LoaderManifestGame>,
    #[serde(default)]
    version_groups: Vec<LoaderVersionGroup>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoaderManifestGame {
    id: String,
    version_group: Option<String>,
    loaders: Vec<LoaderRelease>,
}

#[derive(Deserialize)]
struct LoaderVersionGroup {
    id: String,
    loaders: Vec<LoaderRelease>,
}

#[derive(Deserialize)]
struct LoaderRelease {
    id: String,
    stable: bool,
}

fn compatible_loader_releases(
    loader: &minecraft::Loader,
    minecraft_version: &str,
) -> Result<Vec<LoaderRelease>> {
    fetch_compatible_loader_releases(loader, minecraft_version, &str::to_owned)
}

fn fetch_compatible_loader_releases(
    loader: &minecraft::Loader,
    minecraft_version: &str,
    resolve_url: &dyn Fn(&str) -> String,
) -> Result<Vec<LoaderRelease>> {
    use minecraft::Loader;

    // Fetch the Modrinth manifest for the detected loader
    let path = match loader {
        Loader::Fabric => "fabric/v0",
        Loader::Quilt => "quilt/v1",
        Loader::Forge => "forge/v0",
        Loader::NeoForge => "neo/v0",
    };
    let url = format!("https://launcher-meta.modrinth.com/{path}/manifest.json");
    let mut manifest: LoaderManifest = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?
        .get(resolve_url(&url))
        .send()
        .with_context(|| format!("failed to fetch the {loader} manifest"))?
        .error_for_status()
        .with_context(|| format!("{loader} manifest request failed"))?
        .json()
        .with_context(|| format!("failed to decode the {loader} manifest"))?;

    // Find the exact Minecraft version before resolving shared loader lists
    let game = manifest
        .game_versions
        .iter_mut()
        .find(|game| game.id == minecraft_version)
        .with_context(|| format!("no {loader} entry for Minecraft {minecraft_version}"))?;
    let releases = match (loader, game.version_group.as_deref()) {
        (Loader::Quilt, Some(group_id)) => {
            let group = manifest
                .version_groups
                .iter_mut()
                .find(|group| group.id == group_id)
                .with_context(|| format!("missing Quilt version group {group_id}"))?;
            &mut group.loaders
        }
        (Loader::Fabric, _) if game.loaders.is_empty() => {
            // Fabric can store universal releases in one shared entry
            let mut shared = manifest
                .game_versions
                .iter_mut()
                .filter(|entry| !entry.loaders.is_empty());
            let entry = shared.next().context("missing shared Fabric loader list")?;
            if shared.next().is_some() {
                bail!("could not identify a unique shared Fabric loader list");
            }
            &mut entry.loaders
        }
        _ => &mut game.loaders,
    };

    Ok(std::mem::take(releases))
}

fn is_numeric_loader_release(release: &LoaderRelease) -> bool {
    release
        .id
        .split('.')
        .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

fn check_loader(context: &MinecraftContext) -> Result<()> {
    use minecraft::Loader;

    let environment = &context.environment;
    let loader = &environment.loader;
    let minecraft_version = &environment.minecraft_version;
    let installed = environment
        .loader_version
        .as_deref()
        .with_context(|| format!("could not determine the installed {loader} version"))?;

    let releases = compatible_loader_releases(loader, minecraft_version)?;

    // Use manifest order and infer stable Forge releases from numeric version IDs
    let latest = releases.first().with_context(|| {
        format!("no compatible {loader} releases for Minecraft {minecraft_version}")
    })?;
    let stable = releases.iter().find(|release| match loader {
        Loader::Forge | Loader::NeoForge => is_numeric_loader_release(release),
        Loader::Fabric | Loader::Quilt => release.stable,
    });

    // Compare release positions before choosing which versions to highlight
    let installed_index = releases.iter().position(|release| release.id == installed);
    let (cyan, green, yellow, foreground) =
        if env::var_os("NO_COLOR").is_none_or(|value| value.is_empty()) {
            ("\x1b[36m", "\x1b[32m", "\x1b[33m", "\x1b[39m")
        } else {
            ("", "", "", "")
        };
    let format_release = |release: &LoaderRelease| {
        let color = if release.id == installed {
            green
        } else if installed_index.is_some_and(|index| {
            releases[..index].iter().any(|candidate| candidate.id == release.id)
        }) {
            yellow
        } else {
            ""
        };
        let reset = if color.is_empty() { "" } else { foreground };
        format!("{color}{}{reset}", release.id)
    };

    // Highlight only the version values
    let installed_version = format!("{green}{installed}{foreground}");
    let latest_version = format_release(latest);
    println!("Installed {loader} on Minecraft {cyan}{minecraft_version}{foreground}: {installed_version}");
    println!("Latest compatible: {latest_version}");
    match stable {
        Some(release) => println!("Latest stable: {}", format_release(release)),
        None => println!("Latest stable: none listed"),
    }

    match installed_index {
        Some(0) => println!("No loader update available."),
        Some(_) => println!("Loader update available: {installed_version} → {latest_version}"),
        None => println!("The manifest does not list {installed_version}, so update status is unknown."),
    }

    Ok(())
}

fn update_loader(context: &MinecraftContext) -> Result<()> {
    update_loader_with(context, &str::to_owned, run_java_installer)
}

fn update_loader_with(
    context: &MinecraftContext,
    resolve_url: &dyn Fn(&str) -> String,
    run_installer: fn(&Path, &Path, &[&str]) -> Result<()>,
) -> Result<()> {
    use minecraft::Loader;

    match context.environment.loader {
        Loader::Fabric => update_loader_fabric(context, resolve_url),
        Loader::Quilt => update_loader_quilt(context, resolve_url, run_installer),
        Loader::Forge => update_loader_forge(context, resolve_url, run_installer),
        Loader::NeoForge => update_loader_neoforge(context, resolve_url, run_installer),
    }
}

fn update_loader_fabric(
    context: &MinecraftContext,
    resolve_url: &dyn Fn(&str) -> String,
) -> Result<()> {
    // Find the one executable launcher in the instance root
    let mut launchers = Vec::new();
    for entry in fs::read_dir(&context.root_dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_file()
            && path.extension().is_some_and(|extension| extension == "jar")
            && let Ok(environment) = minecraft::read_fabric_server_launcher(&path)
        {
            launchers.push((path, environment));
        }
    }
    let [(launcher, environment)] = launchers.as_slice() else {
        bail!(
            "expected one executable Fabric launcher in {}, found {}",
            context.root_dir.display(),
            launchers.len()
        );
    };
    let minecraft_version = &environment.minecraft_version;
    if minecraft_version != &context.environment.minecraft_version {
        bail!(
            "the Fabric launcher targets Minecraft {minecraft_version}, but the detected instance targets {}",
            context.environment.minecraft_version
        );
    }
    let installed = environment
        .loader_version
        .as_deref()
        .context("missing installed Fabric Loader version")?;

    // Compare the installed version with the newest stable compatible release
    let releases =
        fetch_compatible_loader_releases(&environment.loader, minecraft_version, resolve_url)?;
    let Some(target_index) = releases.iter().position(|release| release.stable) else {
        println!("No stable Fabric Loader release found for Minecraft {minecraft_version}.");
        return Ok(());
    };
    let installed_index = releases
        .iter()
        .position(|release| release.id == installed)
        .with_context(|| {
            format!(
                "could not compare installed Fabric Loader {installed} with the available releases"
            )
        })?;
    if target_index >= installed_index {
        println!("No stable Fabric Loader update available for Minecraft {minecraft_version}.");
        return Ok(());
    }
    let target = &releases[target_index].id;

    // Download and check the new launcher before touching the old one
    let game_segment = urlencoding::encode(minecraft_version);
    let loader_segment = urlencoding::encode(target);
    let url = format!(
        "https://meta.fabricmc.net/v2/versions/loader/{game_segment}/{loader_segment}/stable/server/jar"
    );
    let mut response = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?
        .get(resolve_url(&url))
        .send()
        .context("failed to download the Fabric launcher")?
        .error_for_status()
        .context("Fabric launcher download failed")?;
    let mut download = tempfile::NamedTempFile::new_in(&context.root_dir)?;
    response.copy_to(download.as_file_mut())?;
    let downloaded = minecraft::read_fabric_server_launcher(download.path())
        .context("downloaded file is not an executable Fabric server launcher")?;
    if downloaded.minecraft_version != *minecraft_version
        || downloaded.loader_version.as_deref() != Some(target.as_str())
    {
        bail!(
            "downloaded Fabric launcher does not match Minecraft {minecraft_version} and Fabric Loader {target}"
        );
    }

    // Back up the old launcher before replacing it at the same path
    let current = File::open(launcher)?;
    current
        .try_lock()
        .context("could not lock the Fabric launcher for updating")?;
    if minecraft::read_fabric_server_launcher(launcher)? != *environment {
        bail!("the Fabric launcher changed while downloading the update");
    }
    download
        .as_file()
        .set_permissions(current.metadata()?.permissions())?;
    download.as_file().sync_all()?;
    let mut backup_name = launcher.as_os_str().to_os_string();
    backup_name.push(format!(".{installed}.bak"));
    let backup = PathBuf::from(backup_name);
    fs::hard_link(launcher, &backup)
        .with_context(|| format!("could not back up the launcher to {}", backup.display()))?;
    if let Err(error) = download.persist(launcher) {
        fs::remove_file(&backup).with_context(|| {
            format!(
                "could not replace the launcher ({error}) or remove the unused backup {}",
                backup.display()
            )
        })?;
        return Err(error).context("could not replace the Fabric launcher");
    }

    println!("Updated Fabric Loader {installed} → {target} for Minecraft {minecraft_version}.");
    println!("Backup: {}", backup.display());
    Ok(())
}

#[derive(Deserialize)]
struct QuiltInstallerRelease {
    url: String,
}

fn update_loader_quilt(
    context: &MinecraftContext,
    resolve_url: &dyn Fn(&str) -> String,
    run_installer: fn(&Path, &Path, &[&str]) -> Result<()>,
) -> Result<()> {
    let environment = &context.environment;
    let minecraft_version = environment.minecraft_version.as_str();
    let (installed, launcher_game) =
        minecraft::read_quilt_server_launcher(&context.root_dir)?;
    if launcher_game.as_deref().is_some_and(|game| game != minecraft_version) {
        bail!("Quilt launcher targets a different Minecraft version than the detected instance");
    }

    // Pick the newest stable loader for the current Minecraft version
    let releases =
        fetch_compatible_loader_releases(&environment.loader, minecraft_version, resolve_url)?;
    let Some(target_index) = releases.iter().position(|release| release.stable) else {
        println!("No stable Quilt Loader release found for Minecraft {minecraft_version}.");
        return Ok(());
    };
    let installed_index = releases
        .iter()
        .position(|release| release.id == installed)
        .with_context(|| format!("could not compare installed Quilt Loader {installed}"))?;
    if target_index >= installed_index {
        println!("No stable Quilt Loader update available for Minecraft {minecraft_version}.");
        return Ok(());
    }
    let target = releases[target_index].id.as_str();

    // Fetch the current official installer and download it to a temporary JAR
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;
    let installers: Vec<QuiltInstallerRelease> = client
        .get(resolve_url("https://meta.quiltmc.org/v3/versions/installer"))
        .send()
        .context("failed to fetch Quilt installer releases")?
        .error_for_status()?
        .json()
        .context("failed to decode Quilt installer releases")?;
    let release = installers.first().context("no Quilt installer release found")?;
    let mut response = client
        .get(resolve_url(&release.url))
        .send()
        .context("failed to download the Quilt installer")?
        .error_for_status()?;
    let mut installer = tempfile::Builder::new().suffix(".jar").tempfile()?;
    response.copy_to(installer.as_file_mut())?;

    // Update the launcher in place and keep the existing Minecraft server JAR
    println!("Updating Quilt Loader {installed} → {target} for Minecraft {minecraft_version}.");
    run_installer(
        &context.root_dir,
        installer.path(),
        &["install", "server", minecraft_version, target, "--install-dir=."],
    )
    .context("Quilt server installation failed")?;

    // Confirm the launcher now references the requested loader
    let (actual, launcher_game) =
        minecraft::read_quilt_server_launcher(&context.root_dir)?;
    if actual != target || launcher_game.as_deref().is_some_and(|game| game != minecraft_version) {
        bail!("Quilt installer did not configure Loader {target} for Minecraft {minecraft_version}");
    }

    println!("Updated Quilt Loader to {target} for Minecraft {minecraft_version}.");
    Ok(())
}

fn update_loader_forge(
    context: &MinecraftContext,
    resolve_url: &dyn Fn(&str) -> String,
    run_installer: fn(&Path, &Path, &[&str]) -> Result<()>,
) -> Result<()> {
    update_loader_with_server_installer(
        context,
        "Forge",
        resolve_url,
        run_installer,
        |minecraft_version, target| {
            let artifact = format!("{minecraft_version}-{target}");
            format!(
                "https://maven.minecraftforge.net/net/minecraftforge/forge/{artifact}/forge-{artifact}-installer.jar"
            )
        },
    )
}

fn update_loader_neoforge(
    context: &MinecraftContext,
    resolve_url: &dyn Fn(&str) -> String,
    run_installer: fn(&Path, &Path, &[&str]) -> Result<()>,
) -> Result<()> {
    update_loader_with_server_installer(
        context,
        "NeoForge",
        resolve_url,
        run_installer,
        |_, target| {
            format!(
                "https://maven.neoforged.net/releases/net/neoforged/neoforge/{target}/neoforge-{target}-installer.jar"
            )
        },
    )
}

fn update_loader_with_server_installer(
    context: &MinecraftContext,
    loader_name: &str,
    resolve_url: &dyn Fn(&str) -> String,
    run_installer: fn(&Path, &Path, &[&str]) -> Result<()>,
    installer_url: impl FnOnce(&str, &str) -> String,
) -> Result<()> {
    let environment = &context.environment;
    let minecraft_version = &environment.minecraft_version;
    let installed = environment
        .loader_version
        .as_deref()
        .with_context(|| format!("could not determine the installed {loader_name} version"))?;

    // Compare the installed version with the newest stable release
    let releases =
        fetch_compatible_loader_releases(&environment.loader, minecraft_version, resolve_url)?;
    let Some(target_index) = releases.iter().position(is_numeric_loader_release) else {
        println!("No stable {loader_name} release found for Minecraft {minecraft_version}.");
        return Ok(());
    };
    let installed_index = releases
        .iter()
        .position(|release| release.id == installed)
        .with_context(|| format!("could not compare installed {loader_name} {installed}"))?;
    if target_index >= installed_index {
        println!("No stable {loader_name} update available for Minecraft {minecraft_version}.");
        return Ok(());
    }
    let target = &releases[target_index].id;

    // Download the official installer to a temporary JAR
    let url = installer_url(minecraft_version, target);
    let mut response = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?
        .get(resolve_url(&url))
        .send()
        .with_context(|| format!("failed to download the {loader_name} installer"))?
        .error_for_status()
        .with_context(|| format!("{loader_name} installer download failed"))?;
    let mut installer = tempfile::Builder::new().suffix(".jar").tempfile()?;
    response.copy_to(installer.as_file_mut())?;

    // Let the installer update the existing server
    println!("Updating {loader_name} {installed} → {target} for Minecraft {minecraft_version}.");
    run_installer(&context.root_dir, installer.path(), &["--installServer"])
        .with_context(|| format!("{loader_name} server installation failed"))?;

    println!("{loader_name} installer completed for {target}.");
    Ok(())
}

fn run_java_installer(root: &Path, installer: &Path, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new("java")
        .arg("-jar")
        .arg(installer)
        .args(args)
        .current_dir(root)
        .status()
        .context("could not start Java for the installer")?;
    if !status.success() {
        bail!("installer exited with {status}");
    }
    Ok(())
}

fn install_from_query(
    client: &ModrinthClient,
    context: &MinecraftContext,
    query: &str,
) -> Result<()> {
    let mut project_cache = HashMap::new();
    let project = resolve_project_query(client, &context.environment, query, &mut project_cache)?;
    install_projects(client, context, vec![project], None, &mut project_cache)
}

#[cfg_attr(not(test), allow(dead_code))]
fn install_projects_from_selections(
    client: &ModrinthClient,
    context: &MinecraftContext,
    selections: &[InstallerSelection],
) -> Result<()> {
    install_projects_from_selections_with_inventory(client, context, selections, None)
}

fn install_projects_from_selections_with_inventory(
    client: &ModrinthClient,
    context: &MinecraftContext,
    selections: &[InstallerSelection],
    existing_inventory: Option<HashMap<String, InstalledMod>>,
) -> Result<()> {
    let mut project_cache = HashMap::new();
    let mut projects = Vec::with_capacity(selections.len());

    for selection in selections {
        let project = fetch_project_cached(client, &mut project_cache, &selection.lookup_key)?
            .ok_or_else(|| {
                anyhow!(
                    "Modrinth project {} ({}) could not be fetched",
                    selection.title,
                    selection.slug
                )
            })?;
        projects.push(project);
    }

    install_projects(
        client,
        context,
        projects,
        existing_inventory,
        &mut project_cache,
    )
}

fn install_projects(
    client: &ModrinthClient,
    context: &MinecraftContext,
    projects: Vec<Project>,
    existing_inventory: Option<HashMap<String, InstalledMod>>,
    project_cache: &mut HashMap<String, Project>,
) -> Result<()> {
    let mut inventory = match existing_inventory {
        Some(existing_inventory) => existing_inventory,
        None => build_installed_inventory(client, context)?,
    };

    for project in projects {
        println!("Selected project: {} ({})", project.title, project.slug);
        let mut visited = HashSet::new();
        install_project_recursive(
            client,
            context,
            &project,
            &mut inventory,
            project_cache,
            &mut visited,
            0,
        )?;
    }

    Ok(())
}

fn update_version_types(include_prerelease: bool) -> &'static [&'static str] {
    if include_prerelease {
        &["release", "beta", "alpha"]
    } else {
        &["release"]
    }
}

fn prerelease_suffix(version: &Version) -> &'static str {
    match version.version_type.as_str() {
        "beta" => " (beta)",
        "alpha" => " (alpha)",
        _ => "",
    }
}

fn list_mods(
    client: &ModrinthClient,
    context: &MinecraftContext,
    include_prerelease: bool,
) -> Result<()> {
    // Scan once and check recognized jars for compatible updates
    let mut mods = resolve_installed_jars(client, context)?;
    mods.sort_by(|left, right| left.path.cmp(&right.path));

    // Collect hashes for the update lookup and measure the table columns
    let mut hashes = Vec::new();
    let mut name_width = "Mod".len();
    let mut version_width = "Version".len().max("Unknown".len());

    for entry in &mods {
        if let Some(item) = &entry.resolved {
            hashes.push(item.sha1.clone());
            name_width = name_width.max(item.project.title.chars().count());
            version_width = version_width.max(item.version.version_number.chars().count());
        } else {
            let name = entry.path.file_name().unwrap_or(entry.path.as_os_str());
            name_width = name_width.max(name.to_string_lossy().chars().count());
        }
    }
    let latest = client.latest_versions_for_hashes(
        &hashes,
        &context.environment,
        Some(update_version_types(include_prerelease)),
    )?;

    // Color the borders without changing the table text
    let (gray, foreground) = if env::var_os("NO_COLOR").is_none_or(|value| value.is_empty()) {
        ("\x1b[90m", "\x1b[39m")
    } else {
        ("", "")
    };
    let edge = format!("{gray}│{foreground}");
    let update_heading = "Update?";
    let update_width = if include_prerelease {
        update_heading.len().max("⬇ (alpha)".chars().count())
    } else {
        update_heading.len()
    };
    let name_rule = "─".repeat(name_width + 2);
    let version_rule = "─".repeat(version_width + 2);
    let update_rule = "─".repeat(update_width + 2);

    // Use the same column widths for the borders and rows
    let print_rule = |left, joint, right| {
        println!("{gray}{left}{update_rule}{joint}{name_rule}{joint}{version_rule}{right}{foreground}");
    };
    let print_row = |name: &str, version: &str, update: &str| {
        println!(
            "{edge} {update:>update_width$} {edge} {name:<name_width$} {edge} {version:<version_width$} {edge}"
        );
    };

    print_rule('┌', '┬', '┐');
    print_row("Mod", "Version", update_heading);
    print_rule('├', '┼', '┤');
    let mut update_count = 0;
    for entry in mods {
        if let Some(item) = entry.resolved {
            let update = latest
                .get(&item.sha1)
                .filter(|version| has_jar_update(&item.sha1, &item.version, version));
            let marker = update
                .map(|version| format!("⬇{}", prerelease_suffix(version)))
                .unwrap_or_default();
            update_count += usize::from(update.is_some());
            print_row(&item.project.title, &item.version.version_number, &marker);
        } else {
            let name = entry.path.file_name().unwrap_or(entry.path.as_os_str());
            print_row(&name.to_string_lossy(), "Unknown", "");
        }
    }

    print_rule('└', '┴', '┘');
    println!("\n{update_count} update(s) available.");
    Ok(())
}

fn has_jar_update(installed_sha1: &str, installed: &Version, latest: &Version) -> bool {
    // Skip older releases and compare the hash of the jar the updater would pick
    latest.date_published >= installed.date_published
        && latest.primary_download_file().is_some_and(|file| {
            file.is_downloadable_jar() && file.sha1().is_some_and(|hash| hash != installed_sha1)
        })
}

fn update_all(
    client: &ModrinthClient,
    context: &MinecraftContext,
    include_prerelease: bool,
) -> Result<()> {
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
    let latest_by_hash = client.latest_versions_for_hashes(
        &hashes,
        &context.environment,
        Some(update_version_types(include_prerelease)),
    )?;
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

fn update_named(
    client: &ModrinthClient,
    context: &MinecraftContext,
    query: &str,
    include_prerelease: bool,
) -> Result<()> {
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

    let latest_by_hash = client.latest_versions_for_hashes(
        &[item.jar.sha1.clone()],
        &context.environment,
        Some(update_version_types(include_prerelease)),
    )?;

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

    if let Some(installed) = inventory.get(&project.id).cloned() {
        update_selected_project_if_installed(
            client,
            context,
            &installed,
            inventory,
            project_cache,
            visited,
            depth,
        )?;
        return Ok(());
    }

    let versions = client.project_versions(&project.id, &context.environment)?;
    let version = select_version_for_environment(versions).with_context(|| {
        format!(
            "no compatible version found for {} on {} {}",
            project.title, context.environment.loader, context.environment.minecraft_version
        )
    })?;

    install_required_dependencies(
        client,
        context,
        &version,
        inventory,
        project_cache,
        visited,
        depth,
    )?;

    let installed = inventory.get(&project.id).cloned();
    install_selected_version(client, context, project, version, installed, inventory)?;
    Ok(())
}

fn update_selected_project_if_installed(
    client: &ModrinthClient,
    context: &MinecraftContext,
    installed: &InstalledMod,
    inventory: &mut HashMap<String, InstalledMod>,
    project_cache: &mut HashMap<String, Project>,
    visited: &mut HashSet<String>,
    depth: usize,
) -> Result<()> {
    let latest_by_hash = client.latest_versions_for_hashes(
        &[installed.jar.sha1.clone()],
        &context.environment,
        None,
    )?;

    let Some(latest_version) = latest_by_hash.get(&installed.jar.sha1).cloned() else {
        println!(
            "No newer versions available for {}.",
            installed.project.title
        );
        return Ok(());
    };

    install_required_dependencies(
        client,
        context,
        &latest_version,
        inventory,
        project_cache,
        visited,
        depth,
    )?;

    if latest_version
        .primary_download_file()
        .and_then(|file| file.sha1().map(|sha1| sha1.to_string()))
        .is_some_and(|latest_hash| latest_hash == installed.jar.sha1)
    {
        println!(
            "No newer versions available for {}.",
            installed.project.title
        );
        return Ok(());
    }

    if replace_if_newer(client, context, installed, &latest_version)? {
        let latest_file = latest_version.primary_download_file().ok_or_else(|| {
            anyhow!(
                "latest version {} for {} has no downloadable jar",
                latest_version.version_number,
                installed.project.title
            )
        })?;
        let expected_sha1 = latest_file
            .sha1()
            .ok_or_else(|| anyhow!("downloaded file metadata was missing sha1"))?;
        inventory.insert(
            installed.project.id.clone(),
            InstalledMod {
                jar: InstalledJar {
                    path: context.mods_dir.join(&latest_file.filename),
                    sha1: expected_sha1.to_string(),
                },
                version: latest_version,
                project: installed.project.clone(),
            },
        );
    }

    Ok(())
}

fn install_required_dependencies(
    client: &ModrinthClient,
    context: &MinecraftContext,
    version: &Version,
    inventory: &mut HashMap<String, InstalledMod>,
    project_cache: &mut HashMap<String, Project>,
    visited: &mut HashSet<String>,
    depth: usize,
) -> Result<()> {
    let prefix = "  ".repeat(depth);

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
    if latest_version.date_published < installed.version.date_published {
        println!(
            "Skipped {}: {} was published before installed version {}.",
            installed.project.title,
            latest_version.version_number,
            installed.version.version_number,
        );
        return Ok(false);
    }

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
        "Updated {}: {} -> {}{}",
        installed.project.title,
        installed.version.version_number,
        latest_version.version_number,
        prerelease_suffix(latest_version),
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

    let hits = client.search_projects(query, Some(environment))?;
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
    // Keep one entry per project for install and update commands
    let mut installed = HashMap::new();
    for entry in resolve_installed_jars(client, context)? {
        if let Some(item) = entry.resolved {
            installed.insert(item.project.id.clone(), item.to_installed_mod(entry.path));
        }
    }
    Ok(installed)
}

// Seperated from buil_installed_inventory into its own function so it can be called on its own
fn resolve_installed_jars(
    client: &ModrinthClient,
    context: &MinecraftContext,
) -> Result<Vec<InstalledInventoryCacheEntry>> {
    fs::create_dir_all(&context.mods_dir)
        .with_context(|| format!("failed to create {}", context.mods_dir.display()))?;

    let jars = scan_installed_jars(&context.mods_dir)?;
    let cached_entries = read_installed_inventory_cache(&context.mods_dir)
        .unwrap_or_default()
        .entries
        .into_iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect::<HashMap<_, _>>();
    let mut project_cache = HashMap::new();
    let mut next_cache_entries = Vec::with_capacity(jars.len());

    for jar in jars {
        if let Some(cached_entry) = cached_entries
            .get(&jar.path)
            .filter(|entry| entry.matches(&jar))
        {
            next_cache_entries.push(cached_entry.clone());
            if let Some(cached_mod) = cached_entry.resolved.as_ref() {
                project_cache.insert(cached_mod.project.id.clone(), cached_mod.project.clone());
                project_cache.insert(cached_mod.project.slug.clone(), cached_mod.project.clone());
            }
            continue;
        }

        let sha1 = compute_sha1(&jar.path)?;
        let resolved = match client.version_from_hash(&sha1)? {
            Some(version) => fetch_project_cached(client, &mut project_cache, &version.project_id)?
                .map(|project| CachedInstalledMod {
                    sha1: sha1.clone(),
                    version,
                    project,
                }),
            None => None,
        };

        next_cache_entries.push(InstalledInventoryCacheEntry::from_scan(&jar,resolved));
    }

    // Save the scan and pass the same entries back to the caller
    let cache = InstalledInventoryCache {
        entries: next_cache_entries,
    };
    let _ = write_installed_inventory_cache(&context.mods_dir, &cache);

    Ok(cache.entries)
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

fn build_installed_project_labels(
    installed: &HashMap<String, InstalledMod>,
) -> HashMap<String, String> {
    let mut labels = HashMap::with_capacity(installed.len() * 2);

    for item in installed.values() {
        let label = format!("Already downloaded ({})", item.version.version_number);
        labels.insert(item.project.id.clone(), label.clone());
        labels.insert(item.project.slug.clone(), label);
    }

    labels
}

fn scan_installed_jars(mods_dir: &Path) -> Result<Vec<InstalledJarMetadata>> {
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

        let metadata = entry
            .metadata()
            .with_context(|| format!("failed to read {}", path.display()))?;
        let modified = metadata
            .modified()
            .with_context(|| format!("failed to read modified time for {}", path.display()))?;
        jars.push(InstalledJarMetadata {
            path,
            size: metadata.len(),
            modified: FileStamp::from_system_time(modified)?,
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

fn execute_search(
    client: &ModrinthClient,
    cwd: &Path,
    known_game_versions: Option<&GameVersionIndex>,
    query: &str,
    global: bool,
    sort: SearchSort,
) -> Result<String> {
    let mut output = String::new();
    let environment = if global {
        output.push_str("Searching all Modrinth mods.\n");
        None
    } else {
        match discover_context(cwd, known_game_versions) {
            Ok(context) => {
                output.push_str(&format_environment(
                    &context.environment,
                    &context.root_dir,
                    &context.mods_dir,
                ));
                Some(context.environment)
            }
            Err(_) => {
                output.push_str(&format!(
                    "No Minecraft environment detected from {}. Searching all Modrinth mods instead.\n",
                    cwd.display()
                ));
                None
            }
        }
    };

    let hits = client.search_projects(query, environment.as_ref())?;
    let ranked = sort_search_hits(query, &hits, sort);
    if ranked.is_empty() {
        bail!("no Modrinth mods matched `{}`", query);
    }

    output.push_str(&format!(
        "Search results for `{query}` (sorted by {}):\n",
        sort.label()
    ));
    for candidate in ranked.iter().take(SEARCH_RESULT_LIMIT) {
        output.push_str(&format!(
            "- {} ({}) - {} downloads\n",
            candidate.title,
            candidate.slug,
            format_download_count(candidate.downloads)
        ));
    }

    Ok(output)
}

fn search_sort_from_flags(
    downloads: bool,
    best_match: bool,
    title: bool,
    slug: bool,
) -> SearchSort {
    if downloads {
        SearchSort::Downloads
    } else if title {
        SearchSort::Title
    } else if slug {
        SearchSort::Slug
    } else if best_match {
        SearchSort::BestMatch
    } else {
        SearchSort::BestMatch
    }
}

fn sort_search_hits(query: &str, hits: &[SearchHit], sort: SearchSort) -> Vec<SearchCandidate> {
    let mut candidates: Vec<SearchCandidate> = hits
        .iter()
        .map(|hit| SearchCandidate {
            key: hit.project_id.clone().unwrap_or_else(|| hit.slug.clone()),
            slug: hit.slug.clone(),
            title: hit.title.clone(),
            downloads: hit.downloads,
        })
        .collect();

    match sort {
        SearchSort::BestMatch => rank_candidates(query, &candidates),
        SearchSort::Downloads => {
            candidates.sort_by(|left, right| {
                right
                    .downloads
                    .cmp(&left.downloads)
                    .then_with(|| {
                        left.title
                            .to_ascii_lowercase()
                            .cmp(&right.title.to_ascii_lowercase())
                    })
                    .then_with(|| {
                        left.slug
                            .to_ascii_lowercase()
                            .cmp(&right.slug.to_ascii_lowercase())
                    })
            });
            candidates
        }
        SearchSort::Title => {
            candidates.sort_by(|left, right| {
                left.title
                    .to_ascii_lowercase()
                    .cmp(&right.title.to_ascii_lowercase())
                    .then_with(|| {
                        left.slug
                            .to_ascii_lowercase()
                            .cmp(&right.slug.to_ascii_lowercase())
                    })
                    .then_with(|| right.downloads.cmp(&left.downloads))
            });
            candidates
        }
        SearchSort::Slug => {
            candidates.sort_by(|left, right| {
                left.slug
                    .to_ascii_lowercase()
                    .cmp(&right.slug.to_ascii_lowercase())
                    .then_with(|| {
                        left.title
                            .to_ascii_lowercase()
                            .cmp(&right.title.to_ascii_lowercase())
                    })
                    .then_with(|| right.downloads.cmp(&left.downloads))
            });
            candidates
        }
    }
}

impl SearchSort {
    fn label(self) -> &'static str {
        match self {
            Self::BestMatch => "best match",
            Self::Downloads => "downloads",
            Self::Title => "title",
            Self::Slug => "slug",
        }
    }
}

fn print_environment(environment: &DetectedEnvironment, root_dir: &Path, mods_dir: &Path) {
    print!("{}", format_environment(environment, root_dir, mods_dir));
}

fn format_environment(
    environment: &DetectedEnvironment,
    root_dir: &Path,
    mods_dir: &Path,
) -> String {
    let loader = match environment.loader_version.as_deref() {
        Some(version) => format!("{} {version}", environment.loader),
        None => environment.loader.to_string(),
    };

    // Honor the user's terminal color preference
    let (green, cyan, foreground) = if env::var_os("NO_COLOR").is_none_or(|value| value.is_empty()) {
        ("\x1b[32m", "\x1b[36m", "\x1b[39m")
    } else {
        ("", "", "")
    };
    format!(
        "Detected {green}{loader}{foreground} on {cyan}Minecraft {}{foreground}\nRoot: {}\nMods: {}\n",
        environment.minecraft_version,
        root_dir.display(),
        mods_dir.display(),
    )
}

fn format_download_count(downloads: u64) -> String {
    let digits = downloads.to_string();
    let mut reversed = String::with_capacity(digits.len() + digits.len() / 3);

    for (index, ch) in digits.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            reversed.push(',');
        }
        reversed.push(ch);
    }

    reversed.chars().rev().collect()
}

fn format_cache_stats(stats: &CacheStats) -> String {
    let usage_percent = if stats.max_bytes == 0 {
        0.0
    } else {
        (stats.total_bytes as f64 / stats.max_bytes as f64) * 100.0
    };

    format!(
        "Cache directory: {}\nSettings file: {}\nEntries: {}\nUsage: {} / {} ({usage_percent:.1}%)\nTTL: {}\nEviction: oldest files first when limit is exceeded\n{}\n",
        stats.dir.display(),
        stats.settings_path.display(),
        stats.entry_count,
        format_bytes(stats.total_bytes),
        format_bytes(stats.max_bytes),
        format_duration(stats.ttl),
        format_cache_settings_summary(stats),
    )
}

fn format_cache_settings_summary(stats: &CacheStats) -> String {
    let lines = CacheKind::ALL
        .iter()
        .map(|kind| {
            format!(
                "{} {}",
                if stats.settings.is_enabled(*kind) { "[x]" } else { "[ ]" },
                kind.label()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("Cached endpoints:\n{lines}")
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];

    if bytes < 1024 {
        return format!("{bytes} B");
    }

    let mut value = bytes as f64;
    let mut unit_index = 0usize;
    while value >= 1024.0 && unit_index < UNITS.len() - 1 {
        value /= 1024.0;
        unit_index += 1;
    }

    if value >= 10.0 || unit_index == 0 {
        format!("{value:.0} {}", UNITS[unit_index])
    } else {
        format!("{value:.1} {}", UNITS[unit_index])
    }
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds % 86_400 == 0 {
        let days = seconds / 86_400;
        return format!("{days} day{}", if days == 1 { "" } else { "s" });
    }
    if seconds % 3_600 == 0 {
        let hours = seconds / 3_600;
        return format!("{hours} hour{}", if hours == 1 { "" } else { "s" });
    }
    if seconds % 60 == 0 {
        let minutes = seconds / 60;
        return format!("{minutes} minute{}", if minutes == 1 { "" } else { "s" });
    }
    format!("{seconds} second{}", if seconds == 1 { "" } else { "s" })
}

fn bytes_to_hex(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

impl FileStamp {
    fn from_system_time(time: SystemTime) -> Result<Self> {
        let duration = time
            .duration_since(UNIX_EPOCH)
            .context("file modified time predates unix epoch")?;
        Ok(Self {
            secs_since_epoch: duration.as_secs(),
            nanos_since_second: duration.subsec_nanos(),
        })
    }
}

impl InstalledInventoryCacheEntry {
    fn from_scan(scan: &InstalledJarMetadata, resolved: Option<CachedInstalledMod>) -> Self {
        Self {
            path: scan.path.clone(),
            size: scan.size,
            modified: scan.modified,
            resolved,
        }
    }

    fn matches(&self, scan: &InstalledJarMetadata) -> bool {
        self.size == scan.size && self.modified == scan.modified
    }
}

impl CachedInstalledMod {
    fn to_installed_mod(&self, path: PathBuf) -> InstalledMod {
        InstalledMod {
            jar: InstalledJar {
                path,
                sha1: self.sha1.clone(),
            },
            version: self.version.clone(),
            project: self.project.clone(),
        }
    }
}

fn read_installed_inventory_cache(mods_dir: &Path) -> Result<InstalledInventoryCache> {
    let cache_path = installed_inventory_cache_path(mods_dir);
    if !cache_path.exists() {
        return Ok(InstalledInventoryCache::default());
    }

    let bytes = fs::read(&cache_path)
        .with_context(|| format!("failed to read {}", cache_path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to decode {}", cache_path.display()))
}

fn write_installed_inventory_cache(mods_dir: &Path, cache: &InstalledInventoryCache) -> Result<()> {
    let cache_path = installed_inventory_cache_path(mods_dir);
    let parent = cache_path.parent().ok_or_else(|| {
        anyhow!(
            "inventory cache path {} had no parent directory",
            cache_path.display()
        )
    })?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;

    let bytes = serde_json::to_vec(cache).context("failed to encode installed inventory cache")?;
    let temp_path = cache_path.with_extension("tmp");
    fs::write(&temp_path, &bytes)
        .with_context(|| format!("failed to write {}", temp_path.display()))?;
    fs::rename(&temp_path, &cache_path)
        .with_context(|| format!("failed to move {}", cache_path.display()))
}

fn installed_inventory_cache_path(mods_dir: &Path) -> PathBuf {
    let mut hasher = Sha1::new();
    hasher.update(mods_dir.to_string_lossy().as_bytes());
    let cache_key = bytes_to_hex(hasher.finalize());
    modrinth::app_cache_dir()
        .join("inventory")
        .join(format!("{cache_key}.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use httpmock::Method::{GET, POST};
    use httpmock::MockServer;
    use serde_json::json;
    use tempfile::tempdir;

    fn fixture_jar(entry: &str, content: &str) -> Vec<u8> {
        let mut jar = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        jar.start_file(entry, zip::write::SimpleFileOptions::default())
            .unwrap();
        jar.write_all(content.as_bytes()).unwrap();
        jar.finish().unwrap().into_inner()
    }

    fn fixture_loader_manifest(server: &MockServer, path: &str, game: &str, old: &str, new: &str) {
        let releases = json!([{"id": new, "stable": true}, {"id": old, "stable": true}]);
        let manifest = if path == "/quilt/v1/manifest.json" {
            json!({
                "gameVersions": [{"id": game, "loaders": [], "versionGroup": "modern"}],
                "versionGroups": [{"id": "modern", "loaders": releases}]
            })
        } else {
            json!({"gameVersions": [{"id": game, "loaders": releases}]})
        };
        server.mock(|when, then| {
            when.method(GET).path(path);
            then.json_body(manifest);
        });
    }

    fn quilt_fixture_library(version: &str) -> String {
        format!("libraries/org/quiltmc/quilt-loader/{version}/quilt-loader-{version}.jar")
    }

    fn quilt_fixture_jar(version: &str) -> Vec<u8> {
        fixture_jar(
            "META-INF/MANIFEST.MF",
            &format!(
                "Manifest-Version: 1.0\r\nClass-Path: {}\r\n\r\n",
                quilt_fixture_library(version)
            ),
        )
    }

    fn simulate_server_installer(root: &Path, installer: &Path, args: &[&str]) -> Result<()> {
        assert_eq!(
            fs::read(root.join("server.jar"))?,
            b"existing Minecraft server"
        );
        assert_eq!(
            installer.extension().and_then(|ext| ext.to_str()),
            Some("jar")
        );
        let payload = fs::read_to_string(installer)?;
        if let Some(version) = payload.strip_prefix("quilt:") {
            assert!(
                args.windows(3)
                    .any(|group| group == ["install", "server", "1.21.1"])
            );
            assert!(args.iter().any(|arg| arg.contains(version)));
            let library = root.join(quilt_fixture_library(version));
            fs::create_dir_all(library.parent().unwrap())?;
            fs::write(library, b"new Quilt Loader")?;
            fs::write(
                root.join("quilt-server-launch.jar"),
                quilt_fixture_jar(version),
            )?;
        } else {
            assert_eq!(args, ["--installServer"]);
            let args_file = format!("libraries/{payload}/unix_args.txt");
            let path = root.join(&args_file);
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(path, b"new installer args")?;
            fs::write(root.join("run.sh"), format!("java @{args_file} \"$@\"\n"))?;
        }
        Ok(())
    }

    fn run_loader_update_fixture(root: &Path, server: &MockServer) -> Result<()> {
        update_loader_with(
            &discover_context(root, None)?,
            &|url| server.url(reqwest::Url::parse(url).unwrap().path()),
            simulate_server_installer,
        )
    }

    #[test]
    fn loader_update_replaces_fabric_server_launcher() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("mods")).unwrap();
        let launcher = root.path().join("fabric-server.jar");
        let original = fixture_jar(
            "install.properties",
            "fabric-loader-version=0.16.10\ngame-version=1.21.1\n",
        );
        let replacement = fixture_jar(
            "install.properties",
            "fabric-loader-version=0.16.14\ngame-version=1.21.1\n",
        );
        fs::write(&launcher, &original).unwrap();
        let server = MockServer::start();
        fixture_loader_manifest(
            &server,
            "/fabric/v0/manifest.json",
            "1.21.1",
            "0.16.10",
            "0.16.14",
        );
        let download = server.mock(|when, then| {
            when.method(GET)
                .path("/v2/versions/loader/1.21.1/0.16.14/stable/server/jar");
            then.body(replacement.clone());
        });

        run_loader_update_fixture(root.path(), &server).unwrap();

        download.assert();
        assert_eq!(fs::read(&launcher).unwrap(), replacement);
        assert_eq!(
            fs::read(root.path().join("fabric-server.jar.0.16.10.bak")).unwrap(),
            original
        );
        let detected = discover_context(root.path(), None).unwrap();
        assert_eq!(
            detected.environment.loader_version.as_deref(),
            Some("0.16.14")
        );
        assert_eq!(detected.environment.minecraft_version, "1.21.1");
    }

    fn assert_server_installer_update(
        manifest: &str,
        game: &str,
        old: &str,
        new: &str,
        old_library: &str,
        new_library: &str,
        installer_path: &str,
    ) {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("mods")).unwrap();
        fs::create_dir_all(root.path().join("libraries").join(old_library)).unwrap();
        fs::write(root.path().join("server.jar"), b"existing Minecraft server").unwrap();
        fs::write(
            root.path().join("run.sh"),
            format!("java @libraries/{old_library}/unix_args.txt \"$@\"\n"),
        )
        .unwrap();
        let server = MockServer::start();
        fixture_loader_manifest(&server, manifest, game, old, new);
        let download = server.mock(|when, then| {
            when.method(GET).path(installer_path);
            then.body(new_library);
        });

        run_loader_update_fixture(root.path(), &server).unwrap();

        download.assert();
        let args_file = format!("libraries/{new_library}/unix_args.txt");
        assert_eq!(
            fs::read(root.path().join(&args_file)).unwrap(),
            b"new installer args"
        );
        assert_eq!(
            fs::read_to_string(root.path().join("run.sh")).unwrap(),
            format!("java @{args_file} \"$@\"\n")
        );
        assert_eq!(
            fs::read(root.path().join("server.jar")).unwrap(),
            b"existing Minecraft server"
        );
    }

    #[test]
    fn loader_update_runs_forge_server_installer() {
        assert_server_installer_update(
            "/forge/v0/manifest.json",
            "1.20.1",
            "47.4.28",
            "47.4.29",
            "net/minecraftforge/forge/1.20.1-47.4.28",
            "net/minecraftforge/forge/1.20.1-47.4.29",
            "/net/minecraftforge/forge/1.20.1-47.4.29/forge-1.20.1-47.4.29-installer.jar",
        );
    }

    #[test]
    fn loader_update_runs_neoforge_server_installer() {
        assert_server_installer_update(
            "/neo/v0/manifest.json",
            "1.21.1",
            "21.1.243",
            "21.1.268",
            "net/neoforged/neoforge/21.1.243",
            "net/neoforged/neoforge/21.1.268",
            "/releases/net/neoforged/neoforge/21.1.268/neoforge-21.1.268-installer.jar",
        );
    }

    #[test]
    fn loader_update_updates_quilt_server_launcher() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("mods")).unwrap();
        let library = root.path().join(quilt_fixture_library("0.27.0"));
        fs::create_dir_all(library.parent().unwrap()).unwrap();
        fs::write(&library, b"old Quilt Loader").unwrap();
        fs::write(
            library.with_file_name("quilt-loader-0.27.0-1.21.1.jar"),
            b"",
        )
        .unwrap();
        fs::write(root.path().join("server.jar"), b"existing Minecraft server").unwrap();
        let launcher = root.path().join("quilt-server-launch.jar");
        fs::write(&launcher, quilt_fixture_jar("0.27.0")).unwrap();
        let server = MockServer::start();
        fixture_loader_manifest(
            &server,
            "/quilt/v1/manifest.json",
            "1.21.1",
            "0.27.0",
            "0.28.0",
        );
        server.mock(|when, then| {
            when.method(GET).path("/v3/versions/installer");
            then.json_body(json!([{"version": "0.12.1", "url": "https://maven.quiltmc.org/repository/release/org/quiltmc/quilt-installer/0.12.1/quilt-installer-0.12.1.jar"}]));
        });
        server.mock(|when, then| {
            when.method(GET).path(
                "/repository/release/org/quiltmc/quilt-installer/0.12.1/quilt-installer-0.12.1.jar",
            );
            then.body("quilt:0.28.0");
        });

        run_loader_update_fixture(root.path(), &server).unwrap();

        let mut jar = zip::ZipArchive::new(File::open(launcher).unwrap()).unwrap();
        let mut manifest = String::new();
        jar.by_name("META-INF/MANIFEST.MF")
            .unwrap()
            .read_to_string(&mut manifest)
            .unwrap();
        assert!(manifest.contains(&format!("Class-Path: {}", quilt_fixture_library("0.28.0"))));
        assert!(root.path().join(quilt_fixture_library("0.28.0")).is_file());
        assert_eq!(
            fs::read(root.path().join("server.jar")).unwrap(),
            b"existing Minecraft server"
        );
    }

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

    fn mock_search_hit(
        project_id: &str,
        slug: &str,
        title: &str,
        downloads: u64,
    ) -> serde_json::Value {
        json!({
            "project_id": project_id,
            "slug": slug,
            "title": title,
            "downloads": downloads
        })
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

    fn write_quilt_root(root: &Path) {
        fs::create_dir_all(root.join("mods")).unwrap();
        for loader_version in ["0.27.1", "0.28.0"] {
            let quilt_dir = root.join(format!(
                "libraries/org/quiltmc/quilt-loader/{loader_version}"
            ));
            fs::create_dir_all(&quilt_dir).unwrap();
            fs::write(
                quilt_dir.join(format!("quilt-loader-{loader_version}-1.21.1.jar")),
                b"loader",
            )
            .unwrap();
        }
    }

    fn write_forge_root(root: &Path) {
        fs::create_dir_all(root.join("mods")).unwrap();
        for version in ["1.21.1-52.0.1", "1.21.1-52.0.2"] {
            let forge_dir = root.join(format!("libraries/net/minecraftforge/forge/{version}"));
            fs::create_dir_all(&forge_dir).unwrap();
        }
    }

    fn write_forge_prerelease_root(root: &Path) {
        fs::create_dir_all(root.join("mods")).unwrap();
        let forge_dir = root.join("libraries/net/minecraftforge/forge/1.20.5-pre1-49.0.38");
        fs::create_dir_all(&forge_dir).unwrap();
    }

    fn write_neoforge_root(root: &Path) {
        fs::create_dir_all(root.join("mods")).unwrap();
        for version in ["20.6.90", "21.1.172"] {
            let neoforge_dir = root.join(format!("libraries/net/neoforged/neoforge/{version}"));
            fs::create_dir_all(&neoforge_dir).unwrap();
        }
    }

    fn write_neoforge_loader_only_prerelease_root(root: &Path) {
        fs::create_dir_all(root.join("mods")).unwrap();
        let neoforge_dir = root.join("libraries/net/neoforged/neoforge/20.6.90-beta");
        fs::create_dir_all(&neoforge_dir).unwrap();
    }

    fn write_snapshot_fabric_root(root: &Path) {
        fs::create_dir_all(root.join("mods")).unwrap();
        fs::write(
            root.join("fabric-server-mc.24w14potato-loader.0.15.11-launcher.1.0.0.jar"),
            b"server",
        )
        .unwrap();
    }

    fn write_mixed_fabric_versions(root: &Path) {
        fs::create_dir_all(root.join("mods")).unwrap();
        fs::write(
            root.join("fabric-server-mc.1.21.10-loader.0.18.5-launcher.1.0.0.jar"),
            b"server",
        )
        .unwrap();

        let intermediary_dir = root.join("libraries/net/fabricmc/intermediary/1.21.11");
        fs::create_dir_all(&intermediary_dir).unwrap();
        fs::write(
            intermediary_dir.join("intermediary-1.21.11.jar"),
            b"intermediary",
        )
        .unwrap();

        let fabric_dir = root.join("libraries/net/fabricmc/fabric-loader/0.18.4");
        fs::create_dir_all(&fabric_dir).unwrap();
        fs::write(fabric_dir.join("fabric-loader-0.18.4.jar"), b"loader").unwrap();
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
    fn detects_snapshot_fabric_versions_supported_by_modrinth() {
        let temp = tempdir().unwrap();
        write_snapshot_fabric_root(temp.path());

        let context = MinecraftContext::discover(&temp.path().join("mods")).unwrap();
        assert_eq!(context.environment.loader.as_str(), "fabric");
        assert_eq!(context.environment.minecraft_version, "24w14potato");
        assert_eq!(
            context.environment.loader_version.as_deref(),
            Some("0.15.11")
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
    fn detects_prism_launcher_manifest_from_instance_root() {
        let temp = tempdir().unwrap();
        let instance_root = temp.path().join("macos boosted beta");
        write_prism_instance_root(&instance_root);

        let context = MinecraftContext::discover(&instance_root).unwrap();
        assert_eq!(
            fs::canonicalize(&context.root_dir).unwrap(),
            fs::canonicalize(instance_root.join("minecraft")).unwrap()
        );
        assert_eq!(context.environment.loader.as_str(), "fabric");
        assert_eq!(context.environment.minecraft_version, "1.21.11");
        assert_eq!(
            context.environment.loader_version.as_deref(),
            Some("0.18.5")
        );
    }

    #[test]
    fn detects_quilt_library_layout() {
        let temp = tempdir().unwrap();
        write_quilt_root(temp.path());

        let context = MinecraftContext::discover(&temp.path().join("mods")).unwrap();
        assert_eq!(context.environment.loader.as_str(), "quilt");
        assert_eq!(context.environment.minecraft_version, "1.21.1");
        assert_eq!(
            context.environment.loader_version.as_deref(),
            Some("0.28.0")
        );
    }

    #[test]
    fn detects_forge_library_layout() {
        let temp = tempdir().unwrap();
        write_forge_root(temp.path());

        let context = MinecraftContext::discover(&temp.path().join("mods")).unwrap();
        assert_eq!(context.environment.loader.as_str(), "forge");
        assert_eq!(context.environment.minecraft_version, "1.21.1");
        assert_eq!(
            context.environment.loader_version.as_deref(),
            Some("52.0.2")
        );
    }

    #[test]
    fn detects_forge_prerelease_minecraft_versions() {
        let temp = tempdir().unwrap();
        write_forge_prerelease_root(temp.path());

        let context = MinecraftContext::discover(&temp.path().join("mods")).unwrap();
        assert_eq!(context.environment.loader.as_str(), "forge");
        assert_eq!(context.environment.minecraft_version, "1.20.5-pre1");
        assert_eq!(
            context.environment.loader_version.as_deref(),
            Some("49.0.38")
        );
    }

    #[test]
    fn detects_neoforge_library_layout_without_minecraft_prefix() {
        let temp = tempdir().unwrap();
        write_neoforge_root(temp.path());

        let context = MinecraftContext::discover(&temp.path().join("mods")).unwrap();
        assert_eq!(context.environment.loader.as_str(), "neoforge");
        assert_eq!(context.environment.minecraft_version, "1.21.1");
        assert_eq!(
            context.environment.loader_version.as_deref(),
            Some("21.1.172")
        );
    }

    #[test]
    fn detects_neoforge_loader_only_prerelease_versions() {
        let temp = tempdir().unwrap();
        write_neoforge_loader_only_prerelease_root(temp.path());

        let context = MinecraftContext::discover(&temp.path().join("mods")).unwrap();
        assert_eq!(context.environment.loader.as_str(), "neoforge");
        assert_eq!(context.environment.minecraft_version, "1.20.6");
        assert_eq!(
            context.environment.loader_version.as_deref(),
            Some("20.6.90-beta")
        );
    }

    #[test]
    fn prefers_newer_minecraft_version_over_higher_loader_version() {
        let temp = tempdir().unwrap();
        write_mixed_fabric_versions(temp.path());

        let context = MinecraftContext::discover(&temp.path().join("mods")).unwrap();
        assert_eq!(context.environment.loader.as_str(), "fabric");
        assert_eq!(context.environment.minecraft_version, "1.21.11");
        assert_eq!(
            context.environment.loader_version.as_deref(),
            Some("0.18.4")
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
    fn search_lists_ranked_results() {
        let temp = tempdir().unwrap();
        write_fabric_root(temp.path());
        let server = MockServer::start();

        server.mock(|when, then| {
            when.method(GET).path("/search");
            then.status(200).json_body(json!({
                "hits": [
                    mock_search_hit("3", "api-lib", "API Lib", 50),
                    mock_search_hit("1", "tooltipfix", "Tooltip Fix", 3000),
                    mock_search_hit("2", "shulkerboxtooltips", "ShulkerBox Tooltips", 1000)
                ]
            }));
        });

        let client = ModrinthClient::new(server.base_url()).unwrap();
        let output = execute_search(
            &client,
            temp.path(),
            None,
            "shulker tooltips",
            false,
            SearchSort::BestMatch,
        )
        .unwrap();

        assert!(output.contains("Detected fabric 0.16.10 on Minecraft 1.21.1"));
        assert!(output.contains("Search results for `shulker tooltips` (sorted by best match):"));
        let shulker_pos = output.find("ShulkerBox Tooltips").unwrap();
        let tooltip_fix_pos = output.find("Tooltip Fix").unwrap();
        assert!(shulker_pos < tooltip_fix_pos);
    }

    #[test]
    fn search_global_works_without_detected_environment() {
        let temp = tempdir().unwrap();
        let server = MockServer::start();

        server.mock(|when, then| {
            when.method(GET).path("/search");
            then.status(200).json_body(json!({
                "hits": [mock_search_hit("1", "sodium", "Sodium", 100000)]
            }));
        });

        let client = ModrinthClient::new(server.base_url()).unwrap();
        let output = execute_search(
            &client,
            temp.path(),
            None,
            "sodium",
            true,
            SearchSort::BestMatch,
        )
        .unwrap();

        assert!(output.contains("Searching all Modrinth mods."));
        assert!(output.contains("Sodium (sodium) - 100,000 downloads"));
        assert!(!output.contains("No Minecraft environment detected"));
    }

    #[test]
    fn search_falls_back_to_global_results_when_no_environment_is_detected() {
        let temp = tempdir().unwrap();
        let server = MockServer::start();

        server.mock(|when, then| {
            when.method(GET).path("/search");
            then.status(200).json_body(json!({
                "hits": [mock_search_hit("1", "sodium", "Sodium", 100000)]
            }));
        });

        let client = ModrinthClient::new(server.base_url()).unwrap();
        let output = execute_search(
            &client,
            temp.path(),
            None,
            "sodium",
            false,
            SearchSort::BestMatch,
        )
        .unwrap();

        assert!(output.contains("No Minecraft environment detected"));
        assert!(output.contains("Searching all Modrinth mods instead."));
        assert!(output.contains("Sodium (sodium) - 100,000 downloads"));
    }

    #[test]
    fn top_level_help_includes_search_command_and_examples() {
        let help = Cli::command().render_help().to_string();

        assert!(help.contains("search"));
        assert!(help.contains("cache"));
        assert!(help.contains("dynamic-installer"));
        assert!(help.contains("mod search sodium"));
        assert!(help.contains("mod s voice chat"));
        assert!(help.contains("mod search --downloads sodium"));
        assert!(help.contains("mod search --title sodium"));
        assert!(help.contains("mod search --global sodium"));
        assert!(help.contains("mod tui"));
        assert!(help.contains("mod cache"));
        assert!(help.contains("mod cache clear"));
        assert!(help.contains("mod cache settings"));
    }

    #[test]
    fn search_downloads_sort_prefers_most_downloaded_results() {
        let temp = tempdir().unwrap();
        let server = MockServer::start();

        server.mock(|when, then| {
            when.method(GET).path("/search");
            then.status(200).json_body(json!({
                "hits": [
                    mock_search_hit("1", "sodium", "Sodium", 100000),
                    mock_search_hit("2", "sodium-extra", "Sodium Extra", 200000)
                ]
            }));
        });

        let client =
            ModrinthClient::new_with_test_cache(server.base_url(), temp.path().join("http")).unwrap();
        let output = execute_search(
            &client,
            temp.path(),
            None,
            "sodium",
            true,
            SearchSort::Downloads,
        )
        .unwrap();

        assert!(output.contains("Search results for `sodium` (sorted by downloads):"));
        let sodium_extra_pos = output.find("Sodium Extra").unwrap();
        let sodium_pos = output.find("Sodium (sodium)").unwrap();
        assert!(sodium_extra_pos < sodium_pos);
    }

    #[test]
    fn search_subcommand_help_includes_sort_flags() {
        let mut command = Cli::command();
        let help = command
            .find_subcommand_mut("search")
            .unwrap()
            .render_help()
            .to_string();

        assert!(help.contains("--downloads"));
        assert!(help.contains("--best-match"));
        assert!(help.contains("--title"));
        assert!(help.contains("--slug"));
    }

    #[test]
    fn dynamic_installer_alias_parses() {
        let cli = Cli::try_parse_from(["mod", "tui"]).unwrap();
        assert!(matches!(cli.command, Command::DynamicInstaller));
    }

    #[test]
    fn cache_clear_subcommand_parses() {
        let cli = Cli::try_parse_from(["mod", "cache", "clear"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Cache {
                command: Some(CacheCommand::Clear)
            }
        ));
    }

    #[test]
    fn cache_settings_subcommand_parses() {
        let cli = Cli::try_parse_from(["mod", "cache", "settings"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Cache {
                command: Some(CacheCommand::Settings)
            }
        ));
    }

    #[test]
    fn cache_subcommand_help_mentions_clear() {
        let mut command = Cli::command();
        let help = command
            .find_subcommand_mut("cache")
            .unwrap()
            .render_help()
            .to_string();

        assert!(help.contains("Inspect or clear the local Modrinth API cache"));
        assert!(help.contains("clear"));
        assert!(help.contains("settings"));
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
    fn list_updates_require_a_different_jar_without_going_back_in_time() {
        // Start with two releases that contain the same jar
        let installed: Version = serde_json::from_value(mock_version(
            "installed",
            "proj",
            "1.0",
            "release",
            "installed-hash",
            "mod.jar",
            "https://example.com/mod.jar",
            json!([]),
        ))
        .unwrap();
        let mut latest = installed.clone();
        latest.date_published = installed.date_published + chrono::Duration::days(1);
        assert!(!has_jar_update("installed-hash", &installed, &latest));

        // A different jar counts as an update unless its release is older
        latest.files[0].hashes.sha1 = Some("different-hash".into());
        assert!(has_jar_update("installed-hash", &installed, &latest));
        latest.date_published = installed.date_published - chrono::Duration::days(1);
        assert!(!has_jar_update("installed-hash", &installed, &latest));

        // Skip releases whose jar has no hash to compare
        latest.date_published = installed.date_published + chrono::Duration::days(1);
        latest.files[0].hashes.sha1 = None;
        assert!(!has_jar_update("installed-hash", &installed, &latest));
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
            when.method(POST).path("/version_files/update").json_body(json!({
                "hashes": [&current_sha1],
                "algorithm": "sha1",
                "loaders": ["fabric"],
                "game_versions": ["1.21.1"],
                "version_types": ["release"]
            }));
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

        let beta_bytes = b"newer-beta";
        let beta_sha1 = bytes_to_hex(&Sha1::digest(beta_bytes));
        let mut beta = mock_version(
            "ver-beta", "proj-sodium", "3.0.0-beta", "beta", &beta_sha1,
            "sodium.jar", &server.url("/files/sodium-beta.jar"), json!([]),
        );
        beta["date_published"] = json!("2026-03-29T00:00:00Z");
        server.mock(|when, then| {
            when.method(POST).path("/version_files/update").json_body(json!({
                "hashes": [&current_sha1],
                "algorithm": "sha1",
                "loaders": ["fabric"],
                "game_versions": ["1.21.1"],
                "version_types": ["release", "beta", "alpha"]
            }));
            then.json_body(json!({current_sha1.clone(): beta.clone()}));
        });
        let beta_download = server.mock(|when, then| {
            when.method(GET).path("/files/sodium-beta.jar");
            then.body(beta_bytes);
        });

        let client = ModrinthClient::new(server.base_url()).unwrap();
        update_all(&client, &context, false).unwrap();

        assert_eq!(compute_sha1(&installed_path).unwrap(), latest_sha1);
        assert_eq!(beta_download.calls(), 0);

        fs::write(&installed_path, current_bytes).unwrap();
        let selected: Version = serde_json::from_value(beta).unwrap();
        assert_eq!(prerelease_suffix(&selected), " (beta)");
        update_named(&client, &context, "sodium", true).unwrap();
        assert_eq!(compute_sha1(&installed_path).unwrap(), beta_sha1);
        beta_download.assert();

        let mut older = selected.clone();
        older.version_type = "release".into();
        older.date_published -= chrono::Duration::days(1);
        older.files[0].hashes.sha1 = Some(latest_sha1);
        let installed = InstalledMod {
            jar: InstalledJar {
                path: installed_path.clone(),
                sha1: beta_sha1,
            },
            version: selected,
            project: serde_json::from_value(mock_project(
                "proj-sodium", "sodium", "Sodium", 100000,
            )).unwrap(),
        };
        assert!(!replace_if_newer(&client, &context, &installed, &older).unwrap());
        assert_eq!(fs::read(&installed_path).unwrap(), beta_bytes);
    }

    #[test]
    fn installed_inventory_cache_reuses_unchanged_jars() {
        let temp = tempdir().unwrap();
        write_fabric_root(temp.path());
        let root = temp.path();
        let context = MinecraftContext::discover(root).unwrap();
        let cache_path = installed_inventory_cache_path(&context.mods_dir);
        let _ = fs::remove_file(&cache_path);
        let server = MockServer::start();

        let current_bytes = b"cached-version";
        let current_sha1 = bytes_to_hex(&Sha1::digest(current_bytes));
        fs::write(root.join("mods/sodium.jar"), current_bytes).unwrap();

        let version_mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/version_file/{}", current_sha1))
                .query_param("algorithm", "sha1");
            then.status(200).json_body(mock_version(
                "ver-current",
                "proj-sodium",
                "2.0.0",
                "release",
                &current_sha1,
                "sodium.jar",
                &server.url("/files/sodium-2.0.0.jar"),
                json!([]),
            ));
        });

        let project_mock = server.mock(|when, then| {
            when.method(GET).path("/project/proj-sodium");
            then.status(200)
                .json_body(mock_project("proj-sodium", "sodium", "Sodium", 100000));
        });

        let client =
            ModrinthClient::new_with_test_cache(server.base_url(), temp.path().join("http")).unwrap();
        let first = build_installed_inventory(&client, &context).unwrap();
        // Clear the HTTP responses so the second scan has to reuse the inventory
        client.clear_cache().unwrap();
        let second = build_installed_inventory(&client, &context).unwrap();

        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_eq!(version_mock.calls(), 1);
        assert_eq!(project_mock.calls(), 1);

        let _ = fs::remove_file(&cache_path);
    }

    #[test]
    fn dynamic_installer_batch_installs_multiple_selected_projects() {
        let temp = tempdir().unwrap();
        write_fabric_root(temp.path());
        let root = temp.path();
        let context = MinecraftContext::discover(root).unwrap();
        let server = MockServer::start();

        let sodium_bytes = b"sodium";
        let sodium_sha1 = bytes_to_hex(&Sha1::digest(sodium_bytes));
        let iris_bytes = b"iris";
        let iris_sha1 = bytes_to_hex(&Sha1::digest(iris_bytes));

        server.mock(|when, then| {
            when.method(GET).path("/project/proj-sodium");
            then.status(200)
                .json_body(mock_project("proj-sodium", "sodium", "Sodium", 100000));
        });

        server.mock(|when, then| {
            when.method(GET).path("/project/proj-iris");
            then.status(200)
                .json_body(mock_project("proj-iris", "iris", "Iris", 75000));
        });

        server.mock(|when, then| {
            when.method(GET).path("/project/proj-sodium/version");
            then.status(200).json_body(json!([mock_version(
                "ver-sodium",
                "proj-sodium",
                "2.0.0",
                "release",
                &sodium_sha1,
                "sodium.jar",
                &server.url("/files/sodium.jar"),
                json!([])
            )]));
        });

        server.mock(|when, then| {
            when.method(GET).path("/project/proj-iris/version");
            then.status(200).json_body(json!([mock_version(
                "ver-iris",
                "proj-iris",
                "3.0.0",
                "release",
                &iris_sha1,
                "iris.jar",
                &server.url("/files/iris.jar"),
                json!([])
            )]));
        });

        server.mock(|when, then| {
            when.method(GET).path("/files/sodium.jar");
            then.status(200).body(sodium_bytes);
        });

        server.mock(|when, then| {
            when.method(GET).path("/files/iris.jar");
            then.status(200).body(iris_bytes);
        });

        let client = ModrinthClient::new(server.base_url()).unwrap();
        install_projects_from_selections(
            &client,
            &context,
            &[
                InstallerSelection {
                    lookup_key: "proj-sodium".to_string(),
                    slug: "sodium".to_string(),
                    title: "Sodium".to_string(),
                },
                InstallerSelection {
                    lookup_key: "proj-iris".to_string(),
                    slug: "iris".to_string(),
                    title: "Iris".to_string(),
                },
            ],
        )
        .unwrap();

        assert_eq!(
            compute_sha1(&root.join("mods/sodium.jar")).unwrap(),
            sodium_sha1
        );
        assert_eq!(
            compute_sha1(&root.join("mods/iris.jar")).unwrap(),
            iris_sha1
        );
    }

    #[test]
    fn dynamic_installer_selection_checks_updates_for_installed_mods() {
        let temp = tempdir().unwrap();
        write_fabric_root(temp.path());
        let root = temp.path();
        let context = MinecraftContext::discover(root).unwrap();
        let server = MockServer::start();

        let current_bytes = b"current-version";
        let current_sha1 = bytes_to_hex(&Sha1::digest(current_bytes));
        let latest_bytes = b"latest-version";
        let latest_sha1 = bytes_to_hex(&Sha1::digest(latest_bytes));
        let installed_path = root.join("mods/sodium.jar");
        fs::write(&installed_path, current_bytes).unwrap();

        let version_from_hash_mock = server.mock(|when, then| {
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
        let installed = build_installed_inventory(&client, &context).unwrap();
        install_projects_from_selections_with_inventory(
            &client,
            &context,
            &[InstallerSelection {
                lookup_key: "proj-sodium".to_string(),
                slug: "sodium".to_string(),
                title: "Sodium".to_string(),
            }],
            Some(installed),
        )
        .unwrap();

        assert_eq!(compute_sha1(&installed_path).unwrap(), latest_sha1);
        assert_eq!(version_from_hash_mock.calls(), 1);
    }

    #[test]
    fn dynamic_installer_selection_reports_no_newer_versions_for_current_mods() {
        let temp = tempdir().unwrap();
        write_fabric_root(temp.path());
        let root = temp.path();
        let context = MinecraftContext::discover(root).unwrap();
        let server = MockServer::start();

        let current_bytes = b"current-version";
        let current_sha1 = bytes_to_hex(&Sha1::digest(current_bytes));
        let installed_path = root.join("mods/sodium.jar");
        fs::write(&installed_path, current_bytes).unwrap();

        server.mock(|when, then| {
            when.method(GET)
                .path(format!("/version_file/{}", current_sha1))
                .query_param("algorithm", "sha1");
            then.status(200).json_body(mock_version(
                "ver-current",
                "proj-sodium",
                "2.0.0",
                "release",
                &current_sha1,
                "sodium.jar",
                &server.url("/files/sodium-2.0.0.jar"),
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
                    "ver-current",
                    "proj-sodium",
                    "2.0.0",
                    "release",
                    &current_sha1,
                    "sodium.jar",
                    &server.url("/files/sodium-2.0.0.jar"),
                    json!([])
                )
            }));
        });

        let client = ModrinthClient::new(server.base_url()).unwrap();
        install_projects_from_selections(
            &client,
            &context,
            &[InstallerSelection {
                lookup_key: "proj-sodium".to_string(),
                slug: "sodium".to_string(),
                title: "Sodium".to_string(),
            }],
        )
        .unwrap();

        assert_eq!(compute_sha1(&installed_path).unwrap(), current_sha1);
    }
}
