use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use eyre::{Context, Result, bail, ensure, eyre};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::cli::args::parse_tool_arg_input;
use crate::config::provenance::{ConfigFileScope, ConfigProvenance};
use crate::config::tracking::Tracker;
use crate::file::{self, display_path};

const STATE_SCHEMA_VERSION: u8 = 1;
const TRANSACTION_SCHEMA_VERSION: u8 = 1;
const MANAGED_MARKER_PREFIX: &str = "# managed by mise tool-stubs bundle ";
const TRANSACTION_DIR_PREFIX: &str = ".mise-tool-stubs-transaction-";

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
#[derive(Debug, usage_rs::Subcommands)]
pub(crate) enum Commands {
    Sync(Sync),
    Status(Status),
    Upgrade(Upgrade),
    Remove(Remove),
}

impl Commands {
    pub(crate) async fn run(self) -> Result<()> {
        match self {
            Self::Sync(cmd) => cmd.run().await,
            Self::Status(cmd) => cmd.run().await,
            Self::Upgrade(cmd) => cmd.run().await,
            Self::Remove(cmd) => cmd.run(),
        }
    }
}

/// Create or update executable stubs declared in configuration
#[derive(Debug, usage_rs::Args)]
pub(crate) struct Sync {
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
pub(crate) struct Status {
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
pub(crate) struct Upgrade {
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
pub(crate) struct Remove {
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

struct SyncFileTransaction {
    path: PathBuf,
    originals: PathBuf,
}

#[derive(Debug, Deserialize, Serialize)]
struct SyncTransactionRecord {
    schema_version: u8,
    bundle_id: String,
    into: PathBuf,
    state_before: Option<BundleState>,
    changes: BTreeMap<String, TransactionChange>,
    tracking_before: Vec<TrackedStubSnapshot>,
}

#[derive(Debug, Deserialize, Serialize)]
struct TransactionChange {
    had_original: bool,
    installed_hash: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct TrackedStubSnapshot {
    path: PathBuf,
    tracked: bool,
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
        file::create_dir_all(&bundle.into)?;
        let _into_lock = crate::lock_file::get(&bundle.into, false)?;
        let _lock = crate::lock_file::get(&bundle.state_path, false)?;
        recover_interrupted_sync(&bundle)?;
        bundle.state = load_state(&bundle.state_path)?;
        if let Some(state) = &bundle.state {
            for (name, command) in &state.commands {
                validate_managed_path(state, name, &command.path)?;
            }
        }
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

        let status_by_name = statuses
            .iter()
            .map(|status| (status.command.as_str(), status.status))
            .collect::<BTreeMap<_, _>>();
        let staging = tempfile::Builder::new()
            .prefix(".mise-tool-stubs-stage-")
            .tempdir_in(&bundle.into)?;
        let mut changes = BTreeMap::new();
        for (name, command) in &desired {
            if status_by_name.get(name.as_str()) != Some(&StatusKind::Current) {
                let staged_path = staging.path().join(name);
                file::write_atomic(&staged_path, &command.contents)?;
                file::make_executable(&staged_path)?;
                changes.insert(name.clone(), (command.path.clone(), Some(staged_path)));
            }
        }
        if let Some(state) = &bundle.state {
            for (name, managed) in &state.commands {
                if desired.contains_key(name) {
                    continue;
                }
                validate_managed_path(state, name, &managed.path)?;
                if path_exists(&managed.path) {
                    if file_is_owned_by_another_bundle(&managed.path, &bundle.id) {
                        continue;
                    }
                    let unchanged = !managed.path.is_symlink()
                        && managed.path.is_file()
                        && file_hash(&managed.path)? == managed.content_hash;
                    ensure!(
                        unchanged || self.force,
                        "refusing to remove modified tool stub {}; use --force",
                        display_path(&managed.path)
                    );
                }
                changes.insert(name.clone(), (managed.path.clone(), None));
            }
        }

        let managed_commands = desired
            .iter()
            .map(|(name, command)| {
                (
                    name.clone(),
                    ManagedCommand {
                        path: command.path.clone(),
                        content_hash: command.content_hash.clone(),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        let final_state = BundleState {
            schema_version: STATE_SCHEMA_VERSION,
            id: bundle.id.clone(),
            source_path: bundle.source_path.clone(),
            source_hash,
            into: bundle.into.clone(),
            commands: managed_commands,
        };
        let state_parent = bundle
            .state_path
            .parent()
            .expect("tool-stub bundle state has a parent directory");
        file::create_dir_all(state_parent)?;
        let prepared_state = prepare_state_write(&bundle.state_path, &final_state)?;

        let affected_paths = bundle
            .state
            .iter()
            .flat_map(|state| state.commands.values())
            .map(|command| command.path.clone())
            .chain(desired.values().map(|command| command.path.clone()))
            .collect::<BTreeSet<_>>();
        let tracking_snapshot = affected_paths
            .iter()
            .map(|path| Ok((path.clone(), Tracker::is_stub_tracked(path)?)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        let transaction = SyncFileTransaction::begin(&bundle, &changes, &tracking_snapshot)?;
        let commit_result = (|| -> Result<()> {
            for (name, (path, _)) in &changes {
                transaction.back_up(name, path)?;
            }
            transaction.sync_backups(&bundle.into)?;
            for (name, (path, staged_path)) in &changes {
                if let Some(staged_path) = staged_path {
                    transaction.install(staged_path, path)?;
                    debug_assert_eq!(path, &bundle.into.join(name));
                }
            }
            file::sync_dir(&bundle.into)?;

            for command in desired.values() {
                Tracker::track_stub(&command.path)?;
            }
            if let Some(state) = &bundle.state {
                for (name, managed) in &state.commands {
                    if !desired.contains_key(name) && changes.contains_key(name) {
                        Tracker::untrack_stub(&managed.path)?;
                    }
                }
            }
            prepared_state.commit()?;
            transaction.mark_committed()
        })();

        if let Err(err) = commit_result {
            return match transaction.rollback(&bundle) {
                Ok(()) => Err(err),
                Err(rollback_err) => Err(err.wrap_err(format!(
                    "failed to roll back tool-stub sync: {rollback_err:#}"
                ))),
            };
        }

        if let Err(err) = transaction.finish() {
            warn!(
                "failed to remove completed tool-stub transaction; the next sync will clean it up: {err:#}"
            );
        }

        for (_, (path, staged_path)) in changes {
            if staged_path.is_some() {
                info!("wrote {}", display_path(path));
            }
        }
        Ok(())
    }
}

impl SyncFileTransaction {
    fn begin(
        bundle: &Bundle,
        changes: &BTreeMap<String, (PathBuf, Option<PathBuf>)>,
        tracking_before: &BTreeMap<PathBuf, bool>,
    ) -> Result<Self> {
        let path = transaction_path(bundle);
        ensure!(
            !path_exists(&path),
            "tool-stub transaction already exists at {}",
            display_path(&path)
        );
        fs::create_dir(&path)?;
        let originals = path.join("originals");
        let setup_result = (|| -> Result<()> {
            fs::create_dir(&originals)?;
            let changes = changes
                .iter()
                .map(|(name, (destination, staged_path))| {
                    Ok((
                        name.clone(),
                        TransactionChange {
                            had_original: path_exists(destination),
                            installed_hash: staged_path.as_deref().map(file_hash).transpose()?,
                        },
                    ))
                })
                .collect::<Result<BTreeMap<_, _>>>()?;
            let record = SyncTransactionRecord {
                schema_version: TRANSACTION_SCHEMA_VERSION,
                bundle_id: bundle.id.clone(),
                into: bundle.into.clone(),
                state_before: bundle.state.clone(),
                changes,
                tracking_before: tracking_before
                    .iter()
                    .map(|(path, tracked)| TrackedStubSnapshot {
                        path: path.clone(),
                        tracked: *tracked,
                    })
                    .collect(),
            };
            write_transaction_record(&path, &record)?;
            file::sync_dir(&bundle.into)
        })();
        if let Err(err) = setup_result {
            if let Err(cleanup_err) = remove_transaction_dir(&path) {
                return Err(err.wrap_err(format!(
                    "failed to clean up incomplete tool-stub transaction: {cleanup_err:#}"
                )));
            }
            return Err(err);
        }
        Ok(Self { path, originals })
    }

    fn back_up(&self, name: &str, path: &Path) -> Result<()> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err.into()),
        };
        ensure!(
            !metadata.file_type().is_dir(),
            "refusing to replace directory {}; remove it first",
            display_path(path)
        );
        let backup_path = self.originals.join(name);
        fs::rename(path, &backup_path)
            .wrap_err_with(|| format!("failed to back up tool stub {}", display_path(path)))?;
        Ok(())
    }

    fn install(&self, staged_path: &Path, path: &Path) -> Result<()> {
        fs::rename(staged_path, path)
            .wrap_err_with(|| format!("failed to install tool stub {}", display_path(path)))?;
        Ok(())
    }

    fn sync_backups(&self, into: &Path) -> Result<()> {
        file::sync_dir(&self.originals)?;
        file::sync_dir(into)
    }

    fn mark_committed(&self) -> Result<()> {
        file::prepare_atomic_write(transaction_commit_path(&self.path), b"committed\n")?.commit()
    }

    fn rollback(self, bundle: &Bundle) -> Result<()> {
        let record = read_transaction_record(&self.path)?;
        rollback_transaction(bundle, &self.path, &record)
    }

    fn finish(self) -> Result<()> {
        remove_transaction_dir(&self.path)
    }
}

fn transaction_path(bundle: &Bundle) -> PathBuf {
    bundle
        .into
        .join(format!("{TRANSACTION_DIR_PREFIX}{}", bundle.id))
}

fn transaction_record_path(transaction_path: &Path) -> PathBuf {
    transaction_path.join("transaction.json")
}

fn transaction_commit_path(transaction_path: &Path) -> PathBuf {
    transaction_path.join("committed")
}

fn transaction_is_committed(transaction_path: &Path) -> Result<bool> {
    let path = transaction_commit_path(transaction_path);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    };
    ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "invalid tool-stub transaction commit marker {}",
        display_path(&path)
    );
    ensure!(
        fs::read(&path)? == b"committed\n",
        "invalid tool-stub transaction commit marker {}",
        display_path(&path)
    );
    Ok(true)
}

fn write_transaction_record(path: &Path, record: &SyncTransactionRecord) -> Result<()> {
    let mut contents = serde_json::to_vec_pretty(record)?;
    contents.push(b'\n');
    file::prepare_atomic_write(transaction_record_path(path), contents)?.commit()
}

fn read_transaction_record(path: &Path) -> Result<SyncTransactionRecord> {
    let record_path = transaction_record_path(path);
    serde_json::from_str(&file::read_to_string(&record_path)?)
        .wrap_err_with(|| format!("failed to parse {}", display_path(record_path)))
}

fn recover_interrupted_sync(bundle: &Bundle) -> Result<()> {
    let path = transaction_path(bundle);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "invalid tool-stub transaction path {}",
        display_path(&path)
    );

    let record = match read_transaction_record(&path) {
        Ok(record) => record,
        Err(_err) if !transaction_record_path(&path).exists() => {
            // The record is committed before any command is moved, so a directory
            // without it is safe to discard after an interruption during setup.
            remove_transaction_dir(&path)?;
            return Ok(());
        }
        Err(err) => return Err(err),
    };
    validate_transaction_record(bundle, &record)?;

    if transaction_is_committed(&path)? {
        remove_transaction_dir(&path)?;
        return Ok(());
    }

    warn!(
        "recovering interrupted tool-stub sync from {}",
        display_path(&path)
    );
    rollback_transaction(bundle, &path, &record)
}

fn validate_transaction_record(bundle: &Bundle, record: &SyncTransactionRecord) -> Result<()> {
    ensure!(
        record.schema_version == TRANSACTION_SCHEMA_VERSION,
        "unsupported tool-stub transaction version {} in {}",
        record.schema_version,
        display_path(transaction_path(bundle))
    );
    ensure!(
        record.bundle_id == bundle.id && record.into == bundle.into,
        "tool-stub transaction identity does not match {}",
        display_path(transaction_path(bundle))
    );
    if let Some(state) = &record.state_before {
        ensure!(
            state.id == bundle.id
                && state.into == bundle.into
                && state.id == bundle_id(&state.source_path, &state.into),
            "previous bundle state does not match tool-stub transaction {}",
            display_path(transaction_path(bundle))
        );
        for (name, command) in &state.commands {
            validate_managed_path(state, name, &command.path)?;
        }
    }
    for name in record.changes.keys() {
        validate_command_name(name)?;
    }
    for snapshot in &record.tracking_before {
        let name = snapshot
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                eyre!(
                    "invalid tracked stub path {} in tool-stub transaction",
                    display_path(&snapshot.path)
                )
            })?;
        validate_command_name(name)?;
        ensure!(
            snapshot.path == bundle.into.join(name),
            "invalid tracked stub path {} in tool-stub transaction",
            display_path(&snapshot.path)
        );
    }
    Ok(())
}

fn rollback_transaction(
    bundle: &Bundle,
    path: &Path,
    record: &SyncTransactionRecord,
) -> Result<()> {
    validate_transaction_record(bundle, record)?;
    let originals = path.join("originals");
    let mut errors = Vec::new();
    for (name, change) in record.changes.iter().rev() {
        let destination = bundle.into.join(name);
        let original = originals.join(name);
        let result = if path_exists(&original) {
            remove_transaction_destination(
                &destination,
                &record.bundle_id,
                change.installed_hash.as_deref(),
            )
            .and_then(|()| fs::rename(&original, &destination).map_err(Into::into))
        } else if !change.had_original {
            remove_transaction_destination(
                &destination,
                &record.bundle_id,
                change.installed_hash.as_deref(),
            )
        } else {
            Ok(())
        };
        if let Err(err) = result {
            errors.push(format!("{}: {err:#}", display_path(destination)));
        }
    }
    if let Err(err) = file::sync_dir(&bundle.into) {
        errors.push(format!("command directory: {err:#}"));
    }
    if let Err(err) = restore_bundle_state(&bundle.state_path, record.state_before.as_ref()) {
        errors.push(format!("bundle state: {err:#}"));
    }
    if let Err(err) = restore_tracking(&record.tracking_before) {
        errors.push(format!("tracked stubs: {err:#}"));
    }
    if errors.is_empty() {
        remove_transaction_dir(path)
    } else {
        bail!(
            "{}; original files preserved at {}",
            errors.join("; "),
            display_path(path)
        )
    }
}

fn remove_transaction_destination(
    path: &Path,
    bundle_id: &str,
    installed_hash: Option<&str>,
) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    ensure!(
        !metadata.file_type().is_dir(),
        "refusing to remove directory while recovering {}",
        display_path(path)
    );
    ensure!(
        !metadata.file_type().is_symlink() && metadata.file_type().is_file(),
        "refusing to remove a file not installed by the interrupted sync: {}",
        display_path(path)
    );
    let installed_hash = installed_hash.ok_or_else(|| {
        eyre!(
            "refusing to remove a file not installed by the interrupted sync: {}",
            display_path(path)
        )
    })?;
    ensure!(
        file_hash(path)? == installed_hash,
        "refusing to remove a file changed after the interrupted sync: {}",
        display_path(path)
    );
    ensure!(
        file_is_owned_by_bundle(path, bundle_id),
        "refusing to remove a file owned by another tool-stub bundle while recovering: {}",
        display_path(path)
    );
    fs::remove_file(path).map_err(Into::into)
}

fn restore_bundle_state(path: &Path, state: Option<&BundleState>) -> Result<()> {
    match state {
        Some(state) => prepare_state_write(path, state)?.commit(),
        None => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        },
    }
}

fn remove_transaction_dir(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "refusing to remove invalid tool-stub transaction path {}",
        display_path(path)
    );
    fs::remove_dir_all(path)?;
    if let Some(parent) = path.parent() {
        file::sync_dir(parent)?;
    }
    Ok(())
}

fn prepare_state_write(path: &Path, state: &BundleState) -> Result<file::PreparedAtomicWrite> {
    let mut contents = serde_json::to_vec_pretty(state)?;
    contents.push(b'\n');
    file::prepare_atomic_write(path, contents)
}

fn restore_tracking(snapshot: &[TrackedStubSnapshot]) -> Result<()> {
    let mut errors = Vec::new();
    for snapshot in snapshot {
        let result = if snapshot.tracked {
            Tracker::track_stub(&snapshot.path)
        } else {
            Tracker::untrack_stub(&snapshot.path)
        };
        if let Err(err) = result {
            errors.push(format!("{}: {err:#}", display_path(&snapshot.path)));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        bail!("{}", errors.join("; "))
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
        let bundle = resolve_bundle(self.manifest.as_deref(), self.into.as_deref(), self.system)?;
        remove_bundle(bundle, self.force)
    }
}

fn remove_bundle(mut bundle: Bundle, force: bool) -> Result<()> {
    let _into_lock = crate::lock_file::get(&bundle.into, false)?;
    let _lock = crate::lock_file::get(&bundle.state_path, false)?;
    recover_interrupted_sync(&bundle)?;
    bundle.state = load_state(&bundle.state_path)?;
    let Some(state) = bundle.state else {
        info!(
            "tool-stub bundle is not synchronized: {}",
            display_path(&bundle.source_path)
        );
        return Ok(());
    };

    let mut preserved = BTreeSet::new();
    for (name, managed) in &state.commands {
        validate_managed_path(&state, name, &managed.path)?;
        if path_exists(&managed.path) {
            if file_is_owned_by_another_bundle(&managed.path, &state.id) {
                preserved.insert(managed.path.clone());
                continue;
            }
            let unchanged = !managed.path.is_symlink()
                && managed.path.is_file()
                && file_hash(&managed.path)? == managed.content_hash;
            ensure!(
                unchanged || force,
                "refusing to remove modified tool stub {}; use --force",
                display_path(&managed.path)
            );
        }
    }
    for managed in state.commands.values() {
        if preserved.contains(&managed.path) {
            continue;
        }
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

#[cfg(test)]
fn save_state(path: &Path, state: &BundleState) -> Result<()> {
    file::create_dir_all(
        path.parent()
            .expect("tool-stub bundle state has a parent directory"),
    )?;
    prepare_state_write(path, state)?.commit()
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
            && name != ".."
            && !name.starts_with(".mise-tool-stubs-"),
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
    if !file_is_owned_by_bundle(path, bundle_id) {
        return Ok(StatusKind::Conflict);
    }
    if actual_hash == expected_hash {
        return Ok(if cfg!(windows) || file::is_executable(path) {
            StatusKind::Current
        } else {
            StatusKind::Stale
        });
    }
    match previous {
        Some(managed) if managed.content_hash != actual_hash => Ok(StatusKind::Modified),
        Some(_) => Ok(StatusKind::Stale),
        None => Ok(StatusKind::Conflict),
    }
}

fn file_is_owned_by_bundle(path: &Path, bundle_id: &str) -> bool {
    file_bundle_owner(path).as_deref() == Some(bundle_id)
}

fn file_is_owned_by_another_bundle(path: &Path, bundle_id: &str) -> bool {
    file_bundle_owner(path).is_some_and(|owner| owner != bundle_id)
}

fn file_bundle_owner(path: &Path) -> Option<String> {
    let contents = match file::read_to_string(path) {
        Ok(contents) => contents,
        Err(_) => return None,
    };
    contents
        .lines()
        .find_map(|line| line.strip_prefix(MANAGED_MARKER_PREFIX).map(str::to_owned))
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
        assert!(validate_command_name(".mise-tool-stubs-transaction-owned").is_err());
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

    #[cfg(unix)]
    #[test]
    fn resumes_an_unrecorded_managed_write() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rg");
        let contents = "# managed by mise tool-stubs bundle bundle\n";
        std::fs::write(&path, contents).unwrap();

        assert_eq!(
            classify_path(
                &path,
                &crate::hash::hash_sha256_to_str(contents),
                "bundle",
                None
            )
            .unwrap(),
            StatusKind::Stale
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

    #[test]
    fn recovers_an_interrupted_sync_after_backing_up_a_command() {
        let tmp = tempfile::tempdir().unwrap();
        let source_path = tmp.path().join("stubs.toml");
        let into = tmp.path().join("bin");
        std::fs::create_dir(&into).unwrap();
        let id = bundle_id(&source_path, &into);
        let bundle = Bundle {
            id: id.clone(),
            source_path,
            manifest_path: None,
            into: into.clone(),
            state_path: tmp.path().join("state").join(format!("{id}.json")),
            state: None,
            system: false,
        };
        let command = into.join("rg");
        std::fs::write(&command, "old").unwrap();
        let changes = BTreeMap::from([("rg".into(), (command.clone(), None))]);

        let transaction = SyncFileTransaction::begin(&bundle, &changes, &BTreeMap::new()).unwrap();
        transaction.back_up("rg", &command).unwrap();
        drop(transaction);

        assert!(!command.exists());
        recover_interrupted_sync(&bundle).unwrap();
        assert_eq!(std::fs::read_to_string(command).unwrap(), "old");
        assert!(!transaction_path(&bundle).exists());
    }

    #[test]
    fn recovery_preserves_a_file_created_after_interruption() {
        let tmp = tempfile::tempdir().unwrap();
        let source_path = tmp.path().join("stubs.toml");
        let into = tmp.path().join("bin");
        std::fs::create_dir(&into).unwrap();
        let id = bundle_id(&source_path, &into);
        let bundle = Bundle {
            id: id.clone(),
            source_path,
            manifest_path: None,
            into: into.clone(),
            state_path: tmp.path().join("state").join(format!("{id}.json")),
            state: None,
            system: false,
        };
        let command = into.join("rg");
        let staged = into.join("staged-rg");
        std::fs::write(&staged, "generated").unwrap();
        let changes = BTreeMap::from([("rg".into(), (command.clone(), Some(staged)))]);

        let transaction = SyncFileTransaction::begin(&bundle, &changes, &BTreeMap::new()).unwrap();
        drop(transaction);
        std::fs::write(&command, "user-created").unwrap();

        assert!(recover_interrupted_sync(&bundle).is_err());
        assert_eq!(std::fs::read_to_string(command).unwrap(), "user-created");
        assert!(transaction_path(&bundle).exists());
    }

    #[test]
    fn recovery_preserves_a_file_owned_by_another_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let source_path = tmp.path().join("stubs.toml");
        let into = tmp.path().join("bin");
        std::fs::create_dir(&into).unwrap();
        let id = bundle_id(&source_path, &into);
        let bundle = Bundle {
            id: id.clone(),
            source_path,
            manifest_path: None,
            into: into.clone(),
            state_path: tmp.path().join("state").join(format!("{id}.json")),
            state: None,
            system: false,
        };
        let command = into.join("rg");
        let staged = into.join("staged-rg");
        let other_contents = render_command("other-bundle", "rg", "dummy@1".into()).unwrap();
        std::fs::write(&staged, &other_contents).unwrap();
        let changes = BTreeMap::from([("rg".into(), (command.clone(), Some(staged.clone())))]);

        let transaction = SyncFileTransaction::begin(&bundle, &changes, &BTreeMap::new()).unwrap();
        transaction.install(&staged, &command).unwrap();
        drop(transaction);

        assert!(recover_interrupted_sync(&bundle).is_err());
        assert_eq!(std::fs::read_to_string(command).unwrap(), other_contents);
        assert!(transaction_path(&bundle).exists());
    }

    #[test]
    fn remove_recovers_an_interrupted_sync_before_removing_the_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let source_path = tmp.path().join("stubs.toml");
        let into = tmp.path().join("bin");
        std::fs::create_dir(&into).unwrap();
        let id = bundle_id(&source_path, &into);
        let state_path = tmp.path().join("state").join(format!("{id}.json"));
        let command = into.join("rg");
        let contents = render_command(&id, "rg", "dummy@1".into()).unwrap();
        std::fs::write(&command, &contents).unwrap();
        let state = BundleState {
            schema_version: STATE_SCHEMA_VERSION,
            id: id.clone(),
            source_path: source_path.clone(),
            source_hash: "source".into(),
            into: into.clone(),
            commands: BTreeMap::from([(
                "rg".into(),
                ManagedCommand {
                    path: command.clone(),
                    content_hash: file_hash(&command).unwrap(),
                },
            )]),
        };
        save_state(&state_path, &state).unwrap();
        let bundle = Bundle {
            id,
            source_path,
            manifest_path: None,
            into,
            state_path: state_path.clone(),
            state: Some(state),
            system: false,
        };
        let changes = BTreeMap::from([("rg".into(), (command.clone(), None))]);
        let transaction = SyncFileTransaction::begin(&bundle, &changes, &BTreeMap::new()).unwrap();
        transaction.back_up("rg", &command).unwrap();
        drop(transaction);

        remove_bundle(bundle, false).unwrap();

        assert!(!command.exists());
        assert!(!state_path.exists());
    }

    #[test]
    fn committed_sync_is_not_rolled_back_during_recovery() {
        let tmp = tempfile::tempdir().unwrap();
        let source_path = tmp.path().join("stubs.toml");
        let into = tmp.path().join("bin");
        std::fs::create_dir(&into).unwrap();
        let id = bundle_id(&source_path, &into);
        let state_path = tmp.path().join("state").join(format!("{id}.json"));
        let bundle = Bundle {
            id: id.clone(),
            source_path: source_path.clone(),
            manifest_path: None,
            into: into.clone(),
            state_path: state_path.clone(),
            state: None,
            system: false,
        };
        let command = into.join("rg");
        let staged = into.join("staged-rg");
        std::fs::write(&command, "old").unwrap();
        std::fs::write(&staged, "new").unwrap();
        let changes = BTreeMap::from([("rg".into(), (command.clone(), Some(staged.clone())))]);

        let transaction = SyncFileTransaction::begin(&bundle, &changes, &BTreeMap::new()).unwrap();
        transaction.back_up("rg", &command).unwrap();
        transaction.install(&staged, &command).unwrap();
        save_state(
            &state_path,
            &BundleState {
                schema_version: STATE_SCHEMA_VERSION,
                id,
                source_path,
                source_hash: "source".into(),
                into,
                commands: BTreeMap::new(),
            },
        )
        .unwrap();
        transaction.mark_committed().unwrap();
        drop(transaction);

        recover_interrupted_sync(&bundle).unwrap();
        assert_eq!(std::fs::read_to_string(command).unwrap(), "new");
        assert!(!transaction_path(&bundle).exists());
    }
}
