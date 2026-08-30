use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use eyre::{Context, Result, bail, ensure, eyre};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::cli::args::parse_tool_arg_input;
use crate::config::provenance::{ConfigFileScope, ConfigProvenance};
use crate::config::tracking::Tracker;
use crate::file::{self, display_path};

const STATE_SCHEMA_VERSION: u8 = 1;
const MANAGED_MARKER_PREFIX: &str = "# managed by mise tool-stubs bundle ";

/// Manage executable tool stubs declared in mise configuration
///
/// Normal system and user configs declare commands under `[tool_stubs]`.
/// String values use `tool@version` syntax; tables accept the same fields as
/// `mise tool-stub`. Syncing writes executable stubs without installing their
/// tools. The first invocation installs its declared version lazily.
///
/// ```toml
/// [tool_stubs]
/// rg = "ripgrep@14"
/// node = { version = "22", bin = "node" }
/// ```
#[derive(Debug, usage_rs::Args)]
#[usage(verbatim_doc_comment)]
pub(crate) struct ToolStubs {
    #[usage(subcommand)]
    command: Commands,
}

#[derive(Debug, usage_rs::Subcommands)]
enum Commands {
    Sync(Sync),
    Status(Status),
    Upgrade(Upgrade),
    Remove(Remove),
}

impl ToolStubs {
    pub(crate) async fn run(self) -> Result<()> {
        match self.command {
            Commands::Sync(cmd) => cmd.run().await,
            Commands::Status(cmd) => cmd.run().await,
            Commands::Upgrade(cmd) => cmd.run().await,
            Commands::Remove(cmd) => cmd.run(),
        }
    }
}

/// Create or update executable stubs declared in configuration
#[derive(Debug, usage_rs::Args)]
struct Sync {
    /// Custom manifest; omit to read [tool_stubs] from the selected config scope
    #[usage(value_name = "MANIFEST", value_hint = usage_rs::ValueHint::FilePath)]
    manifest: Option<PathBuf>,

    /// Directory in which to write commands from a custom manifest
    #[usage(long, value_name = "DIR", value_hint = usage_rs::ValueHint::DirPath)]
    into: Option<PathBuf>,

    /// Use [tool_stubs] from system config and the system tool-stub bin
    #[usage(long, conflicts = "into")]
    system: bool,

    /// Replace modified or conflicting command files
    #[usage(long, short)]
    force: bool,
}

/// Show whether generated stubs match their source configuration and ownership state
#[derive(Debug, usage_rs::Args)]
struct Status {
    /// Custom manifest; omit to read [tool_stubs] from the selected config scope
    #[usage(value_name = "MANIFEST", value_hint = usage_rs::ValueHint::FilePath)]
    manifest: Option<PathBuf>,

    /// Directory containing the commands (normally discovered from prior sync state)
    #[usage(long, value_name = "DIR", value_hint = usage_rs::ValueHint::DirPath)]
    into: Option<PathBuf>,

    /// Use [tool_stubs] from system config and the system tool-stub bin
    #[usage(long, conflicts = "into")]
    system: bool,

    /// Print only command names that need to be synchronized
    #[usage(long)]
    missing: bool,

    /// Print structured status as JSON
    #[usage(long)]
    json: bool,
}

/// Upgrade versions selected by managed tool stubs
#[derive(Debug, usage_rs::Args)]
struct Upgrade {
    /// Custom manifest whose managed stubs should be upgraded; omit for tracked stubs
    #[usage(value_name = "MANIFEST", value_hint = usage_rs::ValueHint::FilePath)]
    manifest: Option<PathBuf>,

    /// Directory containing the commands (normally discovered from prior sync state)
    #[usage(long, value_name = "DIR", value_hint = usage_rs::ValueHint::DirPath)]
    into: Option<PathBuf>,

    /// Upgrade installed tools selected by system tool stubs
    #[usage(long, conflicts = "into")]
    system: bool,

    /// Number of parallel install jobs
    #[usage(long, short, env = "MISE_JOBS")]
    jobs: Option<usize>,

    /// Print the upgrades without installing them
    #[usage(long, short = 'n')]
    dry_run: bool,

    /// Leave replaced versions in place without scheduling them for pruning
    #[usage(long, overrides = "prune")]
    no_prune: bool,

    /// Immediately remove versions replaced by this upgrade
    #[usage(long, overrides = "no_prune")]
    prune: bool,

    /// Connect install command input and output directly to the terminal
    #[usage(long, overrides = "jobs")]
    raw: bool,
}

/// Remove command files owned by a synchronized bundle
#[derive(Debug, usage_rs::Args)]
struct Remove {
    /// Custom manifest; omit to use the synchronized selected config scope
    #[usage(value_name = "MANIFEST", value_hint = usage_rs::ValueHint::FilePath)]
    manifest: Option<PathBuf>,

    /// Directory containing the commands (normally discovered from prior sync state)
    #[usage(long, value_name = "DIR", value_hint = usage_rs::ValueHint::DirPath)]
    into: Option<PathBuf>,

    /// Use the synchronized system config catalogue and system tool-stub bin
    #[usage(long, conflicts = "into")]
    system: bool,

    /// Remove owned command files even when their contents were modified
    #[usage(long, short)]
    force: bool,
}

#[derive(Debug, Deserialize)]
struct Manifest {
    commands: IndexMap<String, toml::Value>,
}

#[derive(Debug)]
struct CommandSpec {
    value: toml::Value,
    provenance: ConfigProvenance,
}

#[derive(Clone, Debug)]
struct DesiredCommand {
    name: String,
    path: PathBuf,
    contents: String,
    content_hash: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ManagedCommand {
    path: PathBuf,
    content_hash: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct BundleState {
    schema_version: u8,
    id: String,
    source_path: PathBuf,
    source_hash: String,
    into: PathBuf,
    commands: BTreeMap<String, ManagedCommand>,
}

#[derive(Debug)]
struct Bundle {
    id: String,
    source_path: PathBuf,
    manifest_path: Option<PathBuf>,
    into: PathBuf,
    state_path: PathBuf,
    state: Option<BundleState>,
    system: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum StatusKind {
    Current,
    Missing,
    Modified,
    Stale,
    Conflict,
}

impl StatusKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Missing => "missing",
            Self::Modified => "modified",
            Self::Stale => "stale",
            Self::Conflict => "conflict",
        }
    }
}

#[derive(Debug, Serialize)]
struct CommandStatus {
    command: String,
    path: PathBuf,
    status: StatusKind,
}

impl Sync {
    async fn run(self) -> Result<()> {
        let mut bundle =
            resolve_bundle(self.manifest.as_deref(), self.into.as_deref(), self.system)?;
        let _into_lock = crate::lock_file::get(&bundle.into, false)?;
        let _lock = crate::lock_file::get(&bundle.state_path, false)?;
        bundle.state = load_state(&bundle.state_path)?;
        let (source_hash, desired) = load_desired(&bundle).await?;
        let statuses = collect_status(&bundle, &desired)?;
        let blockers = statuses
            .iter()
            .filter(|status| matches!(status.status, StatusKind::Modified | StatusKind::Conflict))
            .collect::<Vec<_>>();
        if !self.force && !blockers.is_empty() {
            let details = blockers
                .iter()
                .map(|status| {
                    format!(
                        "{} ({})",
                        display_path(&status.path),
                        status.status.as_str()
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            bail!("refusing to overwrite tool stubs not safely owned by this bundle: {details}");
        }

        file::create_dir_all(&bundle.into)?;

        // Remove commands deleted from the manifest before writing the new set.
        if let Some(state) = &bundle.state {
            for (name, managed) in &state.commands {
                if desired.contains_key(name) {
                    continue;
                }
                validate_managed_path(state, name, &managed.path)?;
                if path_exists(&managed.path) {
                    let unchanged = !managed.path.is_symlink()
                        && managed.path.is_file()
                        && file_hash(&managed.path)? == managed.content_hash;
                    ensure!(
                        unchanged || self.force,
                        "refusing to remove modified tool stub {}; use --force",
                        display_path(&managed.path)
                    );
                    file::remove_file(&managed.path)?;
                }
                Tracker::untrack_stub(&managed.path)?;
            }
        }

        let mut managed_commands = BTreeMap::new();
        let status_by_name = statuses
            .iter()
            .map(|status| (status.command.as_str(), status.status))
            .collect::<BTreeMap<_, _>>();
        for (name, command) in &desired {
            if status_by_name.get(name.as_str()) != Some(&StatusKind::Current) {
                if command.path.is_symlink() {
                    std::fs::remove_file(&command.path)?;
                }
                file::write_atomic(&command.path, &command.contents)?;
                file::make_executable(&command.path)?;
                info!("wrote {}", display_path(&command.path));
            }
            Tracker::track_stub(&command.path)?;
            managed_commands.insert(
                name.clone(),
                ManagedCommand {
                    path: command.path.clone(),
                    content_hash: command.content_hash.clone(),
                },
            );
        }

        let state = BundleState {
            schema_version: STATE_SCHEMA_VERSION,
            id: bundle.id,
            source_path: bundle.source_path,
            source_hash,
            into: bundle.into,
            commands: managed_commands,
        };
        save_state(&bundle.state_path, &state)?;
        Ok(())
    }
}

impl Status {
    async fn run(self) -> Result<()> {
        let bundle = resolve_bundle(self.manifest.as_deref(), self.into.as_deref(), self.system)?;
        let (_, desired) = load_desired(&bundle).await?;
        let mut statuses = collect_status(&bundle, &desired)?;
        if self.missing {
            statuses.retain(|status| status.status != StatusKind::Current);
        }
        if self.json {
            miseprintln!("{}", serde_json::to_string_pretty(&statuses)?);
        } else if self.missing {
            for status in statuses {
                miseprintln!("{}", status.command);
            }
        } else {
            for status in statuses {
                miseprintln!(
                    "{:<9} {:<20} {}",
                    status.status.as_str(),
                    status.command,
                    display_path(&status.path)
                );
            }
        }
        Ok(())
    }
}

impl Upgrade {
    async fn run(self) -> Result<()> {
        let paths = if let Some(manifest) = self.manifest {
            let bundle = resolve_bundle(Some(&manifest), self.into.as_deref(), self.system)?;
            let state = bundle.state.ok_or_else(|| {
                eyre!(
                    "tool-stub bundle has not been synchronized: {}",
                    display_path(&bundle.source_path)
                )
            })?;
            for (name, command) in &state.commands {
                validate_managed_path(&state, name, &command.path)?;
            }
            state
                .commands
                .into_values()
                .map(|command| command.path)
                .collect()
        } else {
            ensure!(self.into.is_none(), "--into requires a manifest argument");
            if self.system {
                stub_paths_from_states(list_states(true)?)?
            } else {
                tracked_stub_paths()?
            }
        };
        crate::cli::upgrade::upgrade_tool_stub_paths(
            paths,
            self.jobs,
            self.dry_run,
            self.no_prune,
            self.prune,
            self.raw,
        )
        .await?;
        Ok(())
    }
}

impl Remove {
    fn run(self) -> Result<()> {
        let mut bundle =
            resolve_bundle(self.manifest.as_deref(), self.into.as_deref(), self.system)?;
        let _into_lock = crate::lock_file::get(&bundle.into, false)?;
        let _lock = crate::lock_file::get(&bundle.state_path, false)?;
        bundle.state = load_state(&bundle.state_path)?;
        let Some(state) = bundle.state else {
            info!(
                "tool-stub bundle is not synchronized: {}",
                display_path(&bundle.source_path)
            );
            return Ok(());
        };

        for (name, managed) in &state.commands {
            validate_managed_path(&state, name, &managed.path)?;
            if path_exists(&managed.path) {
                let unchanged = !managed.path.is_symlink()
                    && managed.path.is_file()
                    && file_hash(&managed.path)? == managed.content_hash;
                ensure!(
                    unchanged || self.force,
                    "refusing to remove modified tool stub {}; use --force",
                    display_path(&managed.path)
                );
            }
        }
        for managed in state.commands.values() {
            if path_exists(&managed.path) {
                file::remove_file(&managed.path)?;
                info!("removed {}", display_path(&managed.path));
            }
            Tracker::untrack_stub(&managed.path)?;
        }
        match std::fs::remove_file(&bundle.state_path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }
}

fn default_config_source(system: bool) -> PathBuf {
    if system {
        crate::env::MISE_SYSTEM_CONFIG_FILE
            .clone()
            .unwrap_or_else(|| crate::dirs::SYSTEM_CONFIG.to_path_buf())
    } else {
        crate::env::MISE_GLOBAL_CONFIG_FILE
            .clone()
            .unwrap_or_else(|| crate::dirs::CONFIG.to_path_buf())
    }
}

fn default_into(system: bool) -> PathBuf {
    if system {
        crate::dirs::SYSTEM_TOOL_STUBS.clone()
    } else {
        crate::dirs::TOOL_STUBS.clone()
    }
}

fn absolute(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path).wrap_err_with(|| format!("failed to resolve {}", display_path(path)))
}

fn bundle_id(source_path: &Path, into: &Path) -> String {
    crate::hash::hash_sha256_to_str(&format!(
        "{}\0{}",
        source_path.to_string_lossy(),
        into.to_string_lossy()
    ))
}

fn state_dir(system: bool) -> &'static Path {
    if system {
        &crate::dirs::SYSTEM_TOOL_STUB_BUNDLES
    } else {
        &crate::dirs::TOOL_STUB_BUNDLES
    }
}

fn state_path(id: &str, system: bool) -> PathBuf {
    state_dir(system).join(format!("{id}.json"))
}

fn resolve_bundle(manifest: Option<&Path>, into: Option<&Path>, system: bool) -> Result<Bundle> {
    ensure!(
        manifest.is_some() || into.is_none(),
        "--into requires a manifest argument"
    );
    let manifest_path = manifest.map(absolute).transpose()?;
    let source_path = match &manifest_path {
        Some(path) => path.clone(),
        None => absolute(&default_config_source(system))?,
    };
    let into = match into {
        Some(into) => absolute(into)?,
        None => {
            let matches = list_states(system)?
                .into_iter()
                .filter(|state| state.source_path == source_path)
                .collect::<Vec<_>>();
            match matches.as_slice() {
                [] => absolute(&default_into(system))?,
                [state] => state.into.clone(),
                _ => bail!(
                    "multiple tool-stub bundles use {}; specify --into",
                    display_path(&source_path)
                ),
            }
        }
    };
    let id = bundle_id(&source_path, &into);
    let state_path = state_path(&id, system);
    let state = load_state(&state_path)?;
    Ok(Bundle {
        id,
        source_path,
        manifest_path,
        into,
        state_path,
        state,
        system,
    })
}

fn list_states(system: bool) -> Result<Vec<BundleState>> {
    list_states_in(state_dir(system))
}

fn list_states_in(dir: &Path) -> Result<Vec<BundleState>> {
    if !dir.is_dir() {
        return Ok(vec![]);
    }
    let mut states = vec![];
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        match load_state(&path) {
            Ok(Some(state)) => states.push(state),
            Ok(None) => {}
            Err(err) => warn!(
                "ignoring invalid tool-stub bundle state {}: {err:#}",
                display_path(&path)
            ),
        }
    }
    Ok(states)
}

fn load_state(path: &Path) -> Result<Option<BundleState>> {
    if !path.exists() {
        return Ok(None);
    }
    let state: BundleState = serde_json::from_str(&file::read_to_string(path)?)
        .wrap_err_with(|| format!("failed to parse {}", display_path(path)))?;
    ensure!(
        state.schema_version == STATE_SCHEMA_VERSION,
        "unsupported tool-stub bundle state version {} in {}",
        state.schema_version,
        display_path(path)
    );
    ensure!(
        path.file_stem().and_then(|stem| stem.to_str()) == Some(state.id.as_str())
            && state.id == bundle_id(&state.source_path, &state.into),
        "tool-stub bundle identity does not match {}",
        display_path(path)
    );
    Ok(Some(state))
}

fn save_state(path: &Path, state: &BundleState) -> Result<()> {
    file::create_dir_all(
        path.parent()
            .expect("tool-stub bundle state has a parent directory"),
    )?;
    let mut contents = serde_json::to_vec_pretty(state)?;
    contents.push(b'\n');
    file::write_atomic(path, contents)
}

async fn load_desired(bundle: &Bundle) -> Result<(String, BTreeMap<String, DesiredCommand>)> {
    let (source_hash, commands) = if let Some(manifest_path) = &bundle.manifest_path {
        let contents = file::read_to_string(manifest_path).wrap_err_with(|| {
            format!(
                "failed to read tool-stub manifest {}",
                display_path(manifest_path)
            )
        })?;
        let manifest: Manifest = toml::from_str(&contents).wrap_err_with(|| {
            format!(
                "failed to parse tool-stub manifest {}",
                display_path(manifest_path)
            )
        })?;
        let provenance = ConfigProvenance::from_path(manifest_path);
        let commands = manifest
            .commands
            .into_iter()
            .map(|(name, value)| {
                (
                    name,
                    CommandSpec {
                        value,
                        provenance: provenance.clone(),
                    },
                )
            })
            .collect();
        (crate::hash::hash_sha256_to_str(&contents), commands)
    } else {
        load_config_commands(bundle.system).await?
    };
    ensure!(
        !commands.is_empty(),
        "tool-stub configuration has no commands"
    );
    let mut desired = BTreeMap::new();
    for (name, spec) in commands {
        validate_command_name(&name)?;
        let rendered = render_command(&bundle.id, &name, spec.value).wrap_err_with(|| {
            format!(
                "invalid tool-stub command {name} in {}",
                display_path(spec.provenance.path())
            )
        })?;
        let path = bundle.into.join(&name);
        desired.insert(
            name.clone(),
            DesiredCommand {
                name,
                path,
                content_hash: crate::hash::hash_sha256_to_str(&rendered),
                contents: rendered,
            },
        );
    }
    Ok((source_hash, desired))
}

async fn load_config_commands(system: bool) -> Result<(String, IndexMap<String, CommandSpec>)> {
    let scope = if system {
        ConfigFileScope::System
    } else {
        ConfigFileScope::User
    };
    let config = crate::config::Config::get().await?;
    let mut commands = IndexMap::new();
    for config_file in config.config_files.values().rev() {
        let provenance = config_file.provenance();
        if provenance.scope() != scope {
            continue;
        }
        for (name, value) in config_file.tool_stubs() {
            commands.insert(
                name,
                CommandSpec {
                    value,
                    provenance: provenance.clone(),
                },
            );
        }
    }
    let values = commands
        .iter()
        .map(|(name, spec)| (name.clone(), spec.value.clone()))
        .collect::<IndexMap<_, _>>();
    let contents = toml::to_string(&values)?;
    Ok((crate::hash::hash_sha256_to_str(&contents), commands))
}

fn validate_command_name(name: &str) -> Result<()> {
    ensure!(!name.is_empty(), "command names cannot be empty");
    let path = Path::new(name);
    ensure!(
        path.file_name().and_then(|part| part.to_str()) == Some(name)
            && path.components().count() == 1
            && name != "."
            && name != "..",
        "command name must be a single file name: {name}"
    );
    Ok(())
}

fn render_command(id: &str, name: &str, value: toml::Value) -> Result<String> {
    let table = match value {
        toml::Value::String(tool) => {
            let (tool_name, version) = parse_tool_arg_input(&tool);
            ensure!(
                !tool_name.is_empty(),
                "invalid tool request for command {name}: {tool}"
            );
            let mut table = toml::map::Map::new();
            table.insert("tool".into(), toml::Value::String(tool_name.to_string()));
            table.insert(
                "version".into(),
                toml::Value::String(version.unwrap_or("latest").to_string()),
            );
            table.insert("bin".into(), toml::Value::String(name.to_string()));
            table
        }
        toml::Value::Table(table) => table,
        _ => bail!("command {name} must be a tool string or table"),
    };
    let body = toml::to_string(&table)?;
    let rendered =
        format!("#!/usr/bin/env -S mise tool-stub\n{MANAGED_MARKER_PREFIX}{id}\n\n{body}");
    crate::cli::tool_stub::ToolStubFile::from_content(&rendered, name)
        .wrap_err_with(|| format!("invalid tool-stub configuration for command {name}"))?;
    Ok(rendered)
}

fn collect_status(
    bundle: &Bundle,
    desired: &BTreeMap<String, DesiredCommand>,
) -> Result<Vec<CommandStatus>> {
    let mut statuses = vec![];
    for command in desired.values() {
        let previous = bundle
            .state
            .as_ref()
            .and_then(|state| state.commands.get(&command.name));
        let status = classify_path(&command.path, &command.content_hash, &bundle.id, previous)?;
        statuses.push(CommandStatus {
            command: command.name.clone(),
            path: command.path.clone(),
            status,
        });
    }
    if let Some(state) = &bundle.state {
        for (name, managed) in &state.commands {
            if desired.contains_key(name) {
                continue;
            }
            validate_managed_path(state, name, &managed.path)?;
            let status = if path_exists(&managed.path)
                && (managed.path.is_symlink()
                    || !managed.path.is_file()
                    || file_hash(&managed.path)? != managed.content_hash)
            {
                StatusKind::Modified
            } else {
                StatusKind::Stale
            };
            statuses.push(CommandStatus {
                command: name.clone(),
                path: managed.path.clone(),
                status,
            });
        }
    }
    statuses.sort_by(|a, b| a.command.cmp(&b.command));
    Ok(statuses)
}

fn classify_path(
    path: &Path,
    expected_hash: &str,
    bundle_id: &str,
    previous: Option<&ManagedCommand>,
) -> Result<StatusKind> {
    if !path_exists(path) {
        return Ok(StatusKind::Missing);
    }
    if path.is_symlink() || !path.is_file() {
        return Ok(StatusKind::Conflict);
    }
    let actual_hash = file_hash(path)?;
    if actual_hash == expected_hash && (cfg!(windows) || file::is_executable(path)) {
        return Ok(StatusKind::Current);
    }
    let contents = match file::read_to_string(path) {
        Ok(contents) => contents,
        Err(_) => return Ok(StatusKind::Conflict),
    };
    let owned = contents
        .lines()
        .any(|line| line == format!("{MANAGED_MARKER_PREFIX}{bundle_id}"));
    if !owned {
        return Ok(StatusKind::Conflict);
    }
    match previous {
        Some(managed) if managed.content_hash != actual_hash => Ok(StatusKind::Modified),
        Some(_) => Ok(StatusKind::Stale),
        None => Ok(StatusKind::Conflict),
    }
}

fn path_exists(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

fn validate_managed_path(state: &BundleState, name: &str, path: &Path) -> Result<()> {
    validate_command_name(name)?;
    ensure!(
        path == state.into.join(name),
        "invalid managed tool-stub path {} in bundle state",
        display_path(path)
    );
    Ok(())
}

fn file_hash(path: &Path) -> Result<String> {
    crate::hash::file_hash_sha256(path, None)
}

pub(crate) fn tracked_stub_paths() -> Result<Vec<PathBuf>> {
    let mut paths = Tracker::list_all_stubs()?;
    paths.extend(stub_paths_from_states(list_states(true)?)?);
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn stub_paths_from_states(states: Vec<BundleState>) -> Result<Vec<PathBuf>> {
    let mut paths = vec![];
    for state in states {
        for (name, command) in &state.commands {
            validate_managed_path(&state, name, &command.path)?;
            paths.push(command.path.clone());
        }
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_string_commands_as_valid_stubs() {
        let rendered =
            render_command("bundle", "rg", toml::Value::String("ripgrep@14".into())).unwrap();
        assert!(rendered.contains("tool = \"ripgrep\""));
        assert!(rendered.contains("version = \"14\""));
        assert!(rendered.contains("bin = \"rg\""));
        assert!(rendered.contains("# managed by mise tool-stubs bundle bundle"));
    }

    #[test]
    fn rejects_command_paths() {
        assert!(validate_command_name("bin/rg").is_err());
        assert!(validate_command_name("..").is_err());
        assert!(validate_command_name("rg").is_ok());
    }

    #[test]
    fn does_not_trust_an_unrecorded_ownership_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rg");
        std::fs::write(
            &path,
            "# managed by mise tool-stubs bundle bundle\nchanged\n",
        )
        .unwrap();

        assert_eq!(
            classify_path(&path, "different", "bundle", None).unwrap(),
            StatusKind::Conflict
        );
    }

    #[test]
    fn treats_non_utf8_files_as_conflicts() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rg");
        std::fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();

        assert_eq!(
            classify_path(&path, "different", "bundle", None).unwrap(),
            StatusKind::Conflict
        );
    }

    #[cfg(unix)]
    #[test]
    fn treats_dangling_symlinks_as_conflicts() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rg");
        symlink(tmp.path().join("missing"), &path).unwrap();

        assert_eq!(
            classify_path(&path, "different", "bundle", None).unwrap(),
            StatusKind::Conflict
        );
    }

    #[test]
    fn skips_invalid_state_while_discovering_bundles() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("broken.json"), "not json").unwrap();

        let source_path = tmp.path().join("stubs.toml");
        let into = tmp.path().join("bin");
        let id = bundle_id(&source_path, &into);
        let state = BundleState {
            schema_version: STATE_SCHEMA_VERSION,
            id: id.clone(),
            source_path,
            source_hash: "source".into(),
            into,
            commands: BTreeMap::new(),
        };
        save_state(&tmp.path().join(format!("{id}.json")), &state).unwrap();

        let states = list_states_in(tmp.path()).unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].id, id);
    }
}
