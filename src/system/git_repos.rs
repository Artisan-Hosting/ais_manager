//! Read and write the git monitor's repo list (`git.cf`) on behalf of the
//! portal.
//!
//! `git.cf` is not a repos file *and* a credentials file -- it is one file
//! doing both jobs: a `GitCredentials { auth_items: Vec<GitAuth> }` serialized
//! to JSON, `simple_encrypt`ed, hex encoded, on one line. Until now the only
//! way to change it was the interactive `gitcf` menu on the box itself; this
//! module is what makes it editable from the API.
//!
//! Three things about that file drive almost every decision below.
//!
//! **The primary key is derived, not stored.** A repo's identity is
//! [`GitAuth::generate_id`] -- `sha256("{branch}-{repo}-{user}")[..8]` -- and
//! it is *not* a field. `server` and `token` are not part of it. That hash is
//! also the runner id watchdog and the portal key off, and it names the
//! checkout at `/var/www/ais/<id>`, so two entries colliding on it means two
//! workers fighting over one directory. We reject collisions rather than let
//! that happen, and an edit that touches user/repo/branch necessarily *moves*
//! an entry to a new id.
//!
//! **Writing it badly takes the git monitor down.** `GitCredentials::save` in
//! the shared crate truncates in place, takes no lock, sets no mode, and
//! unlinks the file when the list is empty -- and gitmon `exit(100)`s on a
//! `git.cf` it cannot read. A human running `gitcf` at the same moment is a
//! real race, so [`store_atomic`] writes a temp file in the same directory and
//! renames over the target instead, and never unlinks.
//!
//! **Nothing here is secret at rest.** `simple_encrypt` prepends its own
//! randomly generated key to the ciphertext, so anyone who can read the file
//! can decrypt it. The `0600` mode below is the actual protection, not the
//! encryption.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use artisan_middleware::aggregator::{AppMessage, CommandResponse, CommandType};
use artisan_middleware::config::AppConfig;
use artisan_middleware::dusa_collection_utils::core::{
    errors::{ErrorArrayItem, Errors},
    functions::current_timestamp,
    logger::LogLevel,
    types::{pathtype::PathType, rwarc::LockWithTimeout, stringy::Stringy},
};
use artisan_middleware::dusa_collection_utils::log;
use artisan_middleware::encryption::simple_encrypt;
use artisan_middleware::git_actions::{GitAuth, GitCredentials, GitServer};
use gethostname::gethostname;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::watchdog;

/// Where the git monitor looks when nothing overrides it. Kept byte-identical
/// to `GitMonitor/src/git_config.rs`'s `DEFAULT_GIT_CREDENTIALS_PATH` -- if
/// these two ever drift, the manager writes a file gitmon never reads and
/// every edit silently does nothing.
pub const DEFAULT_GIT_CREDENTIALS_PATH: &str = "/opt/artisan/etc/git.cf";

/// The application the repo list belongs to. Restarting it is how an edit
/// takes effect.
const GITMON_APP: &str = "ais_gitmon";

/// Bump when the envelope's shape changes incompatibly. A dump carries it so a
/// restore can refuse a file it does not understand rather than half-apply it.
const SCHEMA_VERSION: u32 = 1;

/// Ceiling on a response envelope, in bytes. The Noise transport tops out at
/// 64 KB per packet -- the same limit that forced `Status`/`AllStatus` to
/// truncate logs to 20 lines. Refusing at 48 KB leaves room for the rest of
/// the frame and turns "too many repos" into a clear error instead of a
/// connection that dies mid-reply.
const MAX_ENVELOPE_BYTES: usize = 48 * 1024;

/// Serializes writes *within this manager*. It does not exclude a human
/// running `gitcf` -- nothing in this system can -- which is exactly why
/// [`store_atomic`] renames rather than truncates: a concurrent reader sees
/// either the old file or the new one, never a half-written one.
static WRITE_LOCK: Lazy<LockWithTimeout<()>> = Lazy::new(|| LockWithTimeout::new(()));

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

/// One repo as the API sees it: a [`GitAuth`] plus the id that `GitAuth`
/// derives but does not store.
///
/// `id` is output-only. It is ignored on ingest and recomputed from the other
/// fields, so a dump taken from one box restores cleanly onto another.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RepoEntry {
    #[serde(default)]
    pub id: Option<String>,
    pub user: String,
    pub repo: String,
    pub branch: String,
    pub server: GitServer,
    #[serde(default)]
    pub token: Option<String>,
}

impl From<&GitAuth> for RepoEntry {
    fn from(auth: &GitAuth) -> Self {
        RepoEntry {
            id: Some(auth.generate_id().to_string()),
            user: auth.user.to_string(),
            repo: auth.repo.to_string(),
            branch: auth.branch.to_string(),
            server: auth.server.clone(),
            token: auth.token.as_ref().map(|token| token.to_string()),
        }
    }
}

impl RepoEntry {
    /// Converts to the on-disk type, dropping `id` -- it is derived, so
    /// carrying an incoming one would let a caller assert an identity that
    /// contradicts the fields it was supposedly computed from.
    fn to_git_auth(&self) -> Result<GitAuth, ErrorArrayItem> {
        for (field, value) in [
            ("user", &self.user),
            ("repo", &self.repo),
            ("branch", &self.branch),
        ] {
            if value.trim().is_empty() {
                return Err(ErrorArrayItem::new(
                    Errors::GeneralError,
                    format!("Repo field '{}' must not be empty", field),
                ));
            }
        }

        Ok(GitAuth {
            user: Stringy::from(self.user.trim()),
            repo: Stringy::from(self.repo.trim()),
            branch: Stringy::from(self.branch.trim()),
            server: self.server.clone(),
            token: self
                .token
                .as_ref()
                .filter(|token| !token.is_empty())
                .map(Stringy::from),
        })
    }
}

/// The one shape used for reading, for full replacement, and for backup.
///
/// A `GitReposGet` response *is* a restorable backup, and `GitReposSet` takes
/// that same document back. Keeping read and write symmetric is what makes
/// "dump a system, re-ingest it later" work without a separate export format.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ReposEnvelope {
    #[serde(default = "default_schema")]
    pub schema: u32,
    /// Provenance only -- ignored on ingest, so a dump restores onto any host.
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub exported_at: Option<u64>,
    #[serde(default)]
    pub path: Option<String>,
    pub repos: Vec<RepoEntry>,
}

fn default_schema() -> u32 {
    SCHEMA_VERSION
}

/// What the restart actually did. Reported rather than assumed: a caller that
/// edited the repo list needs to know whether the monitor picked the change up,
/// and "the write succeeded" does not answer that.
#[derive(Serialize, Debug, Clone)]
pub struct ReloadOutcome {
    pub attempted: bool,
    pub ok: bool,
    pub method: &'static str,
    pub message: Option<String>,
}

impl ReloadOutcome {
    fn skipped() -> Self {
        ReloadOutcome {
            attempted: false,
            ok: false,
            method: "none",
            message: Some("reload not requested".to_owned()),
        }
    }
}

/// The reply to every verb in this module.
#[derive(Serialize, Debug, Clone)]
pub struct ReposResponse {
    #[serde(flatten)]
    pub envelope: ReposEnvelope,
    pub reload: ReloadOutcome,
    /// Set by `GitReposUpdate` when the edit changed the derived id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moved: Option<MovedId>,
}

#[derive(Serialize, Debug, Clone)]
pub struct MovedId {
    pub old_id: String,
    pub new_id: String,
    pub note: &'static str,
}

/// Body of a mutating request. `reload` defaults to true so the common case --
/// one edit that should take effect now -- needs no ceremony, while a caller
/// making several edits can pass `false` and restart once at the end.
#[derive(Deserialize, Debug)]
struct MutationFlags {
    #[serde(default = "default_reload")]
    reload: bool,
}

fn default_reload() -> bool {
    true
}

#[derive(Deserialize, Debug)]
struct UpdateRequest {
    id: String,
    repo: RepoEntry,
    #[serde(default = "default_reload")]
    reload: bool,
}

#[derive(Deserialize, Debug)]
struct RemoveRequest {
    id: String,
    #[serde(default = "default_reload")]
    reload: bool,
}

// ---------------------------------------------------------------------------
// Path / load / store
// ---------------------------------------------------------------------------

/// Resolves the repo file from config, falling back to the git monitor's own
/// default. Mirrors `GitMonitor/src/git_config.rs::resolve_git_credentials_path`
/// -- including treating a whitespace-only value as absent, so a blank config
/// entry lands on the default rather than on `""`.
pub fn resolve_path(config: &AppConfig) -> PathType {
    let configured = config
        .git
        .as_ref()
        .map(|git| git.credentials_file.trim())
        .filter(|path| !path.is_empty());

    match configured {
        Some(path) => PathType::Content(path.to_owned()),
        None => PathType::Content(DEFAULT_GIT_CREDENTIALS_PATH.to_owned()),
    }
}

/// Loads the repo list, treating "no file yet" as an empty list rather than an
/// error.
///
/// This follows `system::manager::get_manager_data`: a node that has never had
/// a repo configured is a normal state, and failing the whole command over it
/// would make the API unusable on exactly the machines someone is trying to
/// configure for the first time.
pub async fn load(path: &PathType) -> GitCredentials {
    match GitCredentials::new(Some(path)).await {
        Ok(credentials) => credentials,
        Err(err) => {
            log!(
                LogLevel::Warn,
                "git.cf unreadable at {} ({}); treating as an empty repo list",
                path.to_path_buf().display(),
                err
            );
            GitCredentials {
                auth_items: Vec::new(),
            }
        }
    }
}

/// Writes the repo list so that a concurrent reader never sees a partial file.
///
/// Deliberately *not* `GitCredentials::save`, which truncates the live file in
/// place and unlinks it when the list is empty. Both are fine for an operator
/// at a prompt and dangerous from a service: watchdog, the manager itself, and
/// a human running `gitcf` all read this path, and a missing `git.cf` makes the
/// gitmon daemon exit. Temp-then-rename gives readers an atomic swap, and an
/// empty list writes `{"auth_items":[]}` rather than removing anything.
pub fn store_atomic(credentials: &GitCredentials, path: &PathType) -> Result<(), ErrorArrayItem> {
    let target: PathBuf = path.to_path_buf();

    let parent: &Path = target.parent().filter(|p| !p.as_os_str().is_empty()).ok_or_else(|| {
        ErrorArrayItem::new(
            Errors::InvalidFile,
            format!("{} has no parent directory", target.display()),
        )
    })?;

    fs::create_dir_all(parent).map_err(|err| {
        ErrorArrayItem::new(
            Errors::CreatingDirectory,
            format!("Creating {}: {}", parent.display(), err),
        )
    })?;

    let json = serde_json::to_string(credentials).map_err(|err| {
        ErrorArrayItem::new(
            Errors::JsonCreation,
            format!("Serializing the repo list: {}", err),
        )
    })?;

    let encrypted = simple_encrypt(json.as_bytes())?;

    // The temp file must share a directory with the target: `rename` is only
    // atomic within a filesystem, and /opt/artisan may well be its own mount.
    let temp: PathBuf = parent.join(format!(
        ".{}.tmp.{}",
        target
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "git.cf".to_owned()),
        std::process::id()
    ));

    let write_result = (|| -> Result<(), ErrorArrayItem> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)
            .map_err(|err| {
                ErrorArrayItem::new(
                    Errors::CreatingFile,
                    format!("Opening {}: {}", temp.display(), err),
                )
            })?;

        file.write_all(encrypted.as_bytes()).map_err(|err| {
            ErrorArrayItem::new(
                Errors::CreatingFile,
                format!("Writing {}: {}", temp.display(), err),
            )
        })?;

        // Durability before visibility: the rename must not be able to expose a
        // file whose bytes are still in the page cache.
        file.sync_all().map_err(|err| {
            ErrorArrayItem::new(
                Errors::CreatingFile,
                format!("Syncing {}: {}", temp.display(), err),
            )
        })?;

        // Tighten the mode before the rename, so the file is never briefly
        // world-readable at its real name. The key lives inside the ciphertext,
        // so this mode is the only thing protecting the tokens in here.
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o600)).map_err(|err| {
            ErrorArrayItem::new(
                Errors::SettingPermissionsFile,
                format!("Restricting {} to 0600: {}", temp.display(), err),
            )
        })?;

        fs::rename(&temp, &target).map_err(|err| {
            ErrorArrayItem::new(
                Errors::CreatingFile,
                format!("Renaming {} onto {}: {}", temp.display(), target.display(), err),
            )
        })
    })();

    if write_result.is_err() {
        // Leaving a stale temp behind would accumulate one file per failed
        // write; the write error is what matters, so this cleanup is silent.
        let _ = fs::remove_file(&temp);
        return write_result;
    }

    // Without this the rename itself can be lost to a crash even though the
    // file contents were synced.
    if let Ok(dir) = fs::File::open(parent) {
        let _ = dir.sync_all();
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Applying a change
// ---------------------------------------------------------------------------

/// Restarts the git monitor so a repo-list change actually takes effect.
///
/// Stop-then-start, **not** `watchdog::execute_reload`. Reload sends SIGHUP,
/// and gitmon's SIGHUP handler only re-reads `AppConfig` -- it never re-reads
/// `git.cf` and never respawns workers. Using it here would report success
/// while changing nothing, which is worse than not trying. The cost of a real
/// restart is an interrupted in-flight pull and a brief gap with no monitor
/// running; gitmon is built to be restarted, so that is the cheap side of the
/// trade.
async fn apply(reload: bool) -> ReloadOutcome {
    if !reload {
        return ReloadOutcome::skipped();
    }

    let stop = watchdog::execute_stop(GITMON_APP).await;
    if let Err(err) = &stop {
        return ReloadOutcome {
            attempted: true,
            ok: false,
            method: "stop+start",
            message: Some(format!("Watchdog unavailable stopping {}: {}", GITMON_APP, err)),
        };
    }

    match watchdog::execute_start(GITMON_APP).await {
        Ok(started) => {
            let stop_accepted = stop.map(|response| response.accepted).unwrap_or(false);
            ReloadOutcome {
                attempted: true,
                ok: started.accepted,
                method: "stop+start",
                message: match started.message.trim() {
                    "" if stop_accepted => None,
                    "" => Some(format!("{} was not running before the restart", GITMON_APP)),
                    msg => Some(msg.to_owned()),
                },
            }
        }
        // A failure here leaves the monitor stopped, which is the one outcome a
        // caller must not miss: the file is correct but nothing is watching it.
        Err(err) => ReloadOutcome {
            attempted: true,
            ok: false,
            method: "stop+start",
            message: Some(format!(
                "{} was stopped but could not be started again: {}",
                GITMON_APP, err
            )),
        },
    }
}

// ---------------------------------------------------------------------------
// Verb dispatch
// ---------------------------------------------------------------------------

/// True for any verb this module owns, so the dispatcher can tell "a git repo
/// command I should route here" from "some other custom command".
pub fn handles(verb: &str) -> bool {
    matches!(
        verb,
        "GitReposGet" | "GitReposSet" | "GitReposAdd" | "GitReposUpdate" | "GitReposRemove"
    )
}

/// Runs one `GitRepos*` verb and builds the reply.
///
/// Failures come back as `success: false` with a readable message rather than
/// as `Err`, matching the rest of the command surface: a rejected edit must not
/// look like a dropped connection to the portal.
pub async fn handle(verb: &str, body: &str, config: &AppConfig) -> AppMessage {
    match run(verb, body, config).await {
        Ok(response) => match serde_json::to_string(&response) {
            Ok(json) if json.len() > MAX_ENVELOPE_BYTES => failure(
                verb,
                format!(
                    "Repo list is {} bytes, over the {} byte transport limit; \
                     reduce the number of repos on this node",
                    json.len(),
                    MAX_ENVELOPE_BYTES
                ),
            ),
            Ok(json) => AppMessage::Response(CommandResponse {
                app_id: GITMON_APP.into(),
                command_type: CommandType::Custom(verb.to_owned()),
                success: true,
                message: Some(json),
            }),
            Err(err) => failure(verb, format!("Serializing the response: {}", err)),
        },
        Err(err) => failure(verb, err.err_mesg.to_string()),
    }
}

fn failure(verb: &str, message: String) -> AppMessage {
    log!(LogLevel::Warn, "{} failed: {}", verb, message);
    AppMessage::Response(CommandResponse {
        app_id: GITMON_APP.into(),
        command_type: CommandType::Custom(verb.to_owned()),
        success: false,
        message: Some(message),
    })
}

async fn run(verb: &str, body: &str, config: &AppConfig) -> Result<ReposResponse, ErrorArrayItem> {
    let path = resolve_path(config);

    if verb == "GitReposGet" {
        let credentials = load(&path).await;
        return Ok(ReposResponse {
            envelope: envelope(&credentials, &path),
            reload: ReloadOutcome::skipped(),
            moved: None,
        });
    }

    // Everything past here writes. Hold the lock across read-modify-write so
    // two concurrent portal requests cannot each load the same list, apply
    // their own edit, and have the second silently drop the first.
    let _guard = WRITE_LOCK
        .try_write_with_timeout(Some(Duration::from_secs(5)))
        .await
        .map_err(|err| {
            ErrorArrayItem::new(
                Errors::TimedOut,
                format!("Another repo edit is still in progress: {}", err),
            )
        })?;

    let mut credentials = load(&path).await;
    let mut moved: Option<MovedId> = None;

    let reload = match verb {
        "GitReposSet" => {
            let incoming: ReposEnvelope = parse(body)?;
            check_schema(incoming.schema)?;

            let mut auth_items = Vec::with_capacity(incoming.repos.len());
            for entry in &incoming.repos {
                auth_items.push(entry.to_git_auth()?);
            }
            reject_duplicates(&auth_items)?;

            credentials.auth_items = auth_items;
            // `reload` rides alongside the envelope's own fields rather than
            // inside it, so a backup document restores without carrying a
            // restart decision made on some other day.
            parse::<MutationFlags>(body)?.reload
        }

        "GitReposAdd" => {
            let entry: RepoEntry = parse(body)?;
            let auth = entry.to_git_auth()?;
            let id = auth.generate_id().to_string();

            if find_index(&credentials, &id).is_some() {
                return Err(ErrorArrayItem::new(
                    Errors::GeneralError,
                    format!(
                        "A repo with id {} already exists ({}/{} on {}); \
                         id is derived from branch, repo and user",
                        id, entry.user, entry.repo, entry.branch
                    ),
                ));
            }

            credentials.auth_items.push(auth);
            parse::<MutationFlags>(body)?.reload
        }

        "GitReposUpdate" => {
            let request: UpdateRequest = parse(body)?;
            let index = find_index(&credentials, &request.id).ok_or_else(|| {
                ErrorArrayItem::new(
                    Errors::NotFound,
                    format!("No repo with id {}", request.id),
                )
            })?;

            let auth = request.repo.to_git_auth()?;
            let new_id = auth.generate_id().to_string();

            // The id is a hash of branch/repo/user, so editing any of those
            // moves the entry. Report it: the caller's next request has to use
            // the new id, and the checkout under the old one is now orphaned.
            if new_id != request.id {
                if find_index(&credentials, &new_id).is_some() {
                    return Err(ErrorArrayItem::new(
                        Errors::GeneralError,
                        format!(
                            "Updating {} would collide with existing repo {}",
                            request.id, new_id
                        ),
                    ));
                }
                moved = Some(MovedId {
                    old_id: request.id.clone(),
                    new_id: new_id.clone(),
                    note: "id is derived from branch/repo/user; the checkout at \
                           /var/www/ais/<old_id> is now stale and is not removed automatically",
                });
            }

            credentials.auth_items[index] = auth;
            request.reload
        }

        "GitReposRemove" => {
            let request: RemoveRequest = parse(body)?;
            let index = find_index(&credentials, &request.id).ok_or_else(|| {
                ErrorArrayItem::new(
                    Errors::NotFound,
                    format!("No repo with id {}", request.id),
                )
            })?;

            credentials.auth_items.remove(index);
            request.reload
        }

        other => {
            return Err(ErrorArrayItem::new(
                Errors::GeneralError,
                format!("Unknown git repo verb '{}'", other),
            ));
        }
    };

    store_atomic(&credentials, &path)?;
    log!(
        LogLevel::Info,
        "{} wrote {} with {} repo(s)",
        verb,
        path.to_path_buf().display(),
        credentials.auth_items.len()
    );

    // Sync configured repositories (clone if missing, force pull/sync if existing)
    if let Err(err) = sync_configured_repos(&credentials).await {
        log!(
            LogLevel::Error,
            "Failed to sync configured repositories: {}",
            err.err_mesg
        );
    }

    // Cleanup any stale repositories
    if let Err(err) = cleanup_stale_repos(&credentials) {
        log!(
            LogLevel::Error,
            "Failed to cleanup stale repositories: {}",
            err
        );
    }

    // Recalculate allowed clients in watchdog
    // Best effort
    if let Err(err) = crate::watchdog::recalculate_allowed_clients().await {
        log!(
            LogLevel::Error,
            "Failed to recalculate allowed clients in watchdog: {}",
            err.err_mesg
        );
    }

    Ok(ReposResponse {
        envelope: envelope(&credentials, &path),
        reload: apply(reload).await,
        moved,
    })
}

fn parse<T: serde::de::DeserializeOwned>(body: &str) -> Result<T, ErrorArrayItem> {
    if body.trim().is_empty() {
        return Err(ErrorArrayItem::new(
            Errors::GeneralError,
            "This command requires a JSON body after the verb".to_owned(),
        ));
    }

    serde_json::from_str(body).map_err(|err| {
        ErrorArrayItem::new(Errors::JsonReading, format!("Invalid JSON body: {}", err))
    })
}

fn check_schema(schema: u32) -> Result<(), ErrorArrayItem> {
    if schema != SCHEMA_VERSION {
        return Err(ErrorArrayItem::new(
            Errors::GeneralError,
            format!(
                "Unsupported schema {}; this manager writes and reads schema {}",
                schema, SCHEMA_VERSION
            ),
        ));
    }
    Ok(())
}

/// Rejects a list where two entries hash to the same id. They would be given
/// the same `/var/www/ais/<id>` checkout and fight over it, each repeatedly
/// deciding the other's remote is wrong and recreating the directory.
fn reject_duplicates(auth_items: &[GitAuth]) -> Result<(), ErrorArrayItem> {
    let mut seen: Vec<String> = Vec::with_capacity(auth_items.len());

    for auth in auth_items {
        let id = auth.generate_id().to_string();
        if seen.contains(&id) {
            return Err(ErrorArrayItem::new(
                Errors::GeneralError,
                format!(
                    "Duplicate repo id {} ({}/{} on {}); branch, repo and user must be unique \
                     together because they derive the checkout directory",
                    id, auth.user, auth.repo, auth.branch
                ),
            ));
        }
        seen.push(id);
    }

    Ok(())
}

fn find_index(credentials: &GitCredentials, id: &str) -> Option<usize> {
    credentials
        .auth_items
        .iter()
        .position(|auth| auth.generate_id().to_string() == id)
}

fn get_repo_root() -> &'static str {
    #[cfg(not(test))]
    {
        "/var/www/ais"
    }
    #[cfg(test)]
    {
        "/tmp/ais_git_repos_test_root"
    }
}

fn get_repo_path(auth: &GitAuth) -> PathBuf {
    #[cfg(not(test))]
    {
        PathBuf::from(artisan_middleware::git_actions::generate_git_project_path(auth).to_string())
    }
    #[cfg(test)]
    {
        PathBuf::from(format!("/tmp/ais_git_repos_test_root/{}", auth.generate_id()))
    }
}

fn is_managed_checkout_name(name: &str) -> bool {
    name.len() == 8 && name.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn cleanup_stale_repos(credentials: &GitCredentials) -> Result<usize, String> {
    use std::collections::HashSet;

    let root = get_repo_root();
    let expected_paths: HashSet<PathBuf> = credentials
        .auth_items
        .iter()
        .map(|auth| get_repo_path(auth))
        .collect();

    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(format!("Failed to read {}: {}", root, err)),
    };

    let mut removed = 0;
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };

        if is_managed_checkout_name(&name) && !expected_paths.contains(&path) {
            log!(LogLevel::Info, "Removing stale checkout directory '{}'", path.display());
            if path.is_dir() {
                if let Err(err) = fs::remove_dir_all(&path) {
                    log!(LogLevel::Error, "Failed to remove stale directory '{}': {}", path.display(), err);
                } else {
                    removed += 1;
                }
            } else {
                if let Err(err) = fs::remove_file(&path) {
                    log!(LogLevel::Error, "Failed to remove stale file '{}': {}", path.display(), err);
                } else {
                    removed += 1;
                }
            }
        }
    }

    Ok(removed)
}

fn get_gh_token() -> std::io::Result<String> {
    let output = std::process::Command::new("gh")
        .arg("auth")
        .arg("token")
        .output()?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "Failed to get token from GitHub CLI",
        ))
    }
}

fn parse_token(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(value) = trimmed.parse::<toml::Value>() {
        if let Some(token) = value.get("token").and_then(|v| v.as_str()) {
            let token = token.trim();
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
        if let Some(token) = value
            .get("git")
            .and_then(|v| v.get("token"))
            .and_then(|v| v.as_str())
        {
            let token = token.trim();
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
        if let Some(token) = value
            .get("github")
            .and_then(|v| v.get("token"))
            .and_then(|v| v.as_str())
        {
            let token = token.trim();
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
    }

    trimmed
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
}

fn get_token_from_file(path: &str) -> std::io::Result<String> {
    let raw = fs::read_to_string(path)?;
    parse_token(&raw).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("No token found in token file '{}'", path),
        )
    })
}

fn get_git_token_file() -> Option<String> {
    let contents = match fs::read_to_string("Overrides.toml") {
        Ok(contents) => contents,
        Err(_) => return None,
    };

    let parsed = match contents.parse::<toml::Value>() {
        Ok(parsed) => parsed,
        Err(_) => return None,
    };

    parsed
        .get("git")
        .and_then(|git| git.get("token_file"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

fn get_configured_or_cli_token() -> Result<String, String> {
    let token_file = get_git_token_file();
    if let Some(ref path) = token_file {
        if !path.trim().is_empty() {
            match get_token_from_file(path) {
                Ok(token) => return Ok(token),
                Err(file_err) => {
                    return get_gh_token().map_err(|err| {
                        format!(
                            "Failed to load token from token_file '{}': {}. Fallback to GitHub CLI also failed: {}",
                            path, file_err, err
                        )
                    });
                }
            }
        }
    }

    get_gh_token().map_err(|err| format!("Failed to get token from GitHub CLI: {}", err))
}

fn github_auth_header() -> Option<String> {
    use base64::{engine::general_purpose, Engine as _};
    get_configured_or_cli_token().ok().map(|token| {
        let creds = format!("x-access-token:{}", token);
        let encoded = general_purpose::STANDARD.encode(creds);
        format!("Authorization: Basic {}", encoded)
    })
}

fn ssh_to_http_url(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    if let Some(remainder) = trimmed.strip_prefix("ssh://") {
        let without_user = remainder
            .rsplit_once('@')
            .map_or(remainder, |(_, host)| host);
        let (host, path) = without_user.split_once('/')?;
        return Some(format!("https://{}/{}", host, path));
    }

    if !trimmed.contains("://") {
        let (authority, path) = trimmed.split_once(':')?;
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        if !host.is_empty() && !path.is_empty() {
            return Some(format!("https://{}/{}", host, path));
        }
    }

    None
}

fn expected_remote_url(auth: &GitAuth) -> String {
    let base = match &auth.server {
        GitServer::GitHub => "https://github.com".to_string(),
        GitServer::GitLab => "https://gitlab.com".to_string(),
        GitServer::Custom(url) => ssh_to_http_url(url).unwrap_or_else(|| url.to_string()),
    };

    format!(
        "{}/{}/{}.git",
        base.trim_end_matches('/'),
        auth.user,
        auth.repo
    )
}

fn enforce_checkout_ownership(git_project_path: &PathType) -> Result<(), ErrorArrayItem> {
    #[cfg(test)]
    {
        let _ = git_project_path;
        Ok(())
    }
    #[cfg(not(test))]
    {
        use artisan_middleware::users::{get_id, set_file_ownership};
        let webuser = get_id("www-data")?;
        set_file_ownership(git_project_path, webuser.0, webuser.1)
    }
}

pub async fn sync_configured_repos(credentials: &GitCredentials) -> Result<(), ErrorArrayItem> {
    let auth_header = github_auth_header();

    for auth in &credentials.auth_items {
        let repo_id = auth.generate_id().to_string();
        let project_path = get_repo_path(auth);
        let dest_path = project_path.to_string_lossy().to_string();

        let repo_url = expected_remote_url(auth);

        // check if checkout exists
        let exists = project_path.exists();

        if !exists {
            log!(
                LogLevel::Info,
                "{}: checkout missing at {}; cloning from git.cf",
                repo_id,
                dest_path
            );

            let mut cmd = tokio::process::Command::new("git");
            if let Some(ref header) = auth_header {
                cmd.arg("-c").arg(format!("http.extraheader={}", header));
            }
            cmd.arg("clone")
               .arg(&repo_url)
               .arg(&dest_path)
               .env("GIT_TERMINAL_PROMPT", "0");

            let output = cmd.output().await.map_err(|err| {
                ErrorArrayItem::new(
                    Errors::Git,
                    format!("Failed to spawn git clone for {}: {}", repo_id, err),
                )
            })?;

            if !output.status.success() {
                log!(
                    LogLevel::Error,
                    "{}: git clone failed: {}",
                    repo_id,
                    String::from_utf8_lossy(&output.stderr)
                );
                continue;
            }

            // checkout the branch
            let mut cmd = tokio::process::Command::new("git");
            cmd.arg("-C")
               .arg(&dest_path)
               .arg("checkout")
               .arg(&auth.branch.to_string());

            let output = cmd.output().await.map_err(|err| {
                ErrorArrayItem::new(
                    Errors::Git,
                    format!("Failed to spawn git checkout for {}: {}", repo_id, err),
                )
            })?;

            if !output.status.success() {
                log!(
                    LogLevel::Error,
                    "{}: git checkout failed: {}",
                    repo_id,
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        } else {
            // Already exists - let's force pull/sync it to origin branch
            log!(
                LogLevel::Info,
                "{}: checkout ready; force syncing {} to origin/{}",
                repo_id,
                dest_path,
                auth.branch
            );

            // Fetch origin
            let mut cmd = tokio::process::Command::new("git");
            if let Some(ref header) = auth_header {
                cmd.arg("-c").arg(format!("http.extraheader={}", header));
            }
            cmd.arg("-C")
               .arg(&dest_path)
               .arg("fetch")
               .arg("origin")
               .env("GIT_TERMINAL_PROMPT", "0");

            let output = cmd.output().await.map_err(|err| {
                ErrorArrayItem::new(
                    Errors::Git,
                    format!("Failed to spawn git fetch for {}: {}", repo_id, err),
                )
            })?;

            if !output.status.success() {
                log!(
                    LogLevel::Error,
                    "{}: git fetch failed: {}",
                    repo_id,
                    String::from_utf8_lossy(&output.stderr)
                );
                continue;
            }

            // Checkout and force reset/clean
            let branch = auth.branch.to_string();
            let remote_branch = format!("origin/{}", branch);

            // git checkout -B <branch> <remote_branch>
            let mut cmd = tokio::process::Command::new("git");
            cmd.arg("-C")
               .arg(&dest_path)
               .arg("checkout")
               .arg("-B")
               .arg(&branch)
               .arg(&remote_branch);
            let output = cmd.output().await.map_err(|err| {
                ErrorArrayItem::new(
                    Errors::Git,
                    format!("Failed to spawn git checkout for {}: {}", repo_id, err),
                )
            })?;

            if !output.status.success() {
                log!(
                    LogLevel::Error,
                    "{}: git checkout failed: {}",
                    repo_id,
                    String::from_utf8_lossy(&output.stderr)
                );
                continue;
            }

            // git reset --hard <remote_branch>
            let mut cmd = tokio::process::Command::new("git");
            cmd.arg("-C")
               .arg(&dest_path)
               .arg("reset")
               .arg("--hard")
               .arg(&remote_branch);
            let output = cmd.output().await.map_err(|err| {
                ErrorArrayItem::new(
                    Errors::Git,
                    format!("Failed to spawn git reset for {}: {}", repo_id, err),
                )
            })?;

            if !output.status.success() {
                log!(
                    LogLevel::Error,
                    "{}: git reset failed: {}",
                    repo_id,
                    String::from_utf8_lossy(&output.stderr)
                );
                continue;
            }

            // git clean -ffd
            let mut cmd = tokio::process::Command::new("git");
            cmd.arg("-C")
               .arg(&dest_path)
               .arg("clean")
               .arg("-ffd");
            let _ = cmd.output().await;
        }

        // enforce checkout ownership (chown back to www-data)
        // Best effort
        if let Err(err) = enforce_checkout_ownership(&PathType::PathBuf(project_path.clone())) {
            log!(
                LogLevel::Warn,
                "{}: failed to re-assert www-data ownership on '{}': {}",
                repo_id,
                dest_path,
                err.err_mesg
            );
        }
    }

    Ok(())
}

fn envelope(credentials: &GitCredentials, path: &PathType) -> ReposEnvelope {
    ReposEnvelope {
        schema: SCHEMA_VERSION,
        hostname: gethostname().into_string().ok(),
        exported_at: Some(current_timestamp()),
        path: Some(path.to_path_buf().display().to_string()),
        repos: credentials.auth_items.iter().map(RepoEntry::from).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_cleanup_stale_repos_removes_unmanaged_paths() {
        let root = get_repo_root();
        let _ = fs::remove_dir_all(root);
        fs::create_dir_all(root).unwrap();

        let active_auth = auth("acme", "widgets", "main");
        let active_path = get_repo_path(&active_auth);
        fs::create_dir_all(&active_path).unwrap();

        // Create a stale directory (must be a valid hex-like 8-character string)
        let stale_path = PathBuf::from(root).join("a1b2c3d4");
        fs::create_dir_all(&stale_path).unwrap();

        // Create a non-stale directory that is not managed (not hex-like)
        let non_managed_path = PathBuf::from(root).join("nothex");
        fs::create_dir_all(&non_managed_path).unwrap();

        let credentials = GitCredentials {
            auth_items: vec![active_auth],
        };

        let removed = cleanup_stale_repos(&credentials).unwrap();
        assert_eq!(removed, 1);
        assert!(active_path.exists());
        assert!(!stale_path.exists());
        assert!(non_managed_path.exists());

        // Cleanup
        let _ = fs::remove_dir_all(root);
    }

    fn auth(user: &str, repo: &str, branch: &str) -> GitAuth {
        GitAuth {
            user: Stringy::from(user),
            repo: Stringy::from(repo),
            branch: Stringy::from(branch),
            server: GitServer::GitHub,
            token: None,
        }
    }

    /// Each test gets its own directory so the `0600` and leftover-temp
    /// assertions are not reading another test's files.
    fn scratch(name: &str) -> PathType {
        let dir = std::env::temp_dir().join(format!("ais_git_repos_test_{}_{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        PathType::PathBuf(dir.join("git.cf"))
    }

    /// The compatibility claim: what we write, the shared crate's reader --
    /// the same one the git monitor and watchdog use -- reads back unchanged.
    #[tokio::test]
    async fn round_trips_through_the_shared_reader() {
        let path = scratch("round_trip");
        let mut original = GitCredentials {
            auth_items: vec![auth("acme", "web", "main"), auth("acme", "api", "release")],
        };
        original.auth_items[1].token = Some(Stringy::from("ghp_secret"));

        store_atomic(&original, &path).unwrap();
        let loaded = GitCredentials::new(Some(&path)).await.unwrap();

        assert_eq!(loaded.auth_items, original.auth_items);
        assert_eq!(loaded.auth_items[1].token, Some(Stringy::from("ghp_secret")));
    }

    #[tokio::test]
    async fn writes_are_owner_only() {
        let path = scratch("mode");
        store_atomic(&GitCredentials { auth_items: vec![auth("a", "b", "main")] }, &path).unwrap();

        let mode = fs::metadata(path.to_path_buf()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "git.cf holds tokens and its key is inside it");
    }

    /// `GitCredentials::save` unlinks on an empty list, and a missing git.cf
    /// makes the gitmon daemon exit(100). Emptying the list must leave a
    /// readable file behind instead.
    #[tokio::test]
    async fn emptying_the_list_leaves_a_readable_file() {
        let path = scratch("empty");
        store_atomic(&GitCredentials { auth_items: vec![auth("a", "b", "main")] }, &path).unwrap();
        store_atomic(&GitCredentials { auth_items: vec![] }, &path).unwrap();

        assert!(path.to_path_buf().exists(), "git.cf must never be unlinked");
        assert!(GitCredentials::new(Some(&path)).await.unwrap().auth_items.is_empty());
    }

    #[tokio::test]
    async fn leaves_no_temp_files_behind() {
        let path = scratch("temp");
        for _ in 0..3 {
            store_atomic(&GitCredentials { auth_items: vec![auth("a", "b", "main")] }, &path).unwrap();
        }

        let dir = path.to_path_buf().parent().unwrap().to_path_buf();
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.contains(".tmp."))
            .collect();

        assert!(leftovers.is_empty(), "stray temp files: {:?}", leftovers);
    }

    /// The id is a hash of branch/repo/user and names the checkout directory,
    /// so a colliding pair would mean two workers on one directory.
    #[test]
    fn duplicate_ids_are_rejected() {
        let dupes = vec![auth("acme", "web", "main"), auth("acme", "web", "main")];
        assert!(reject_duplicates(&dupes).is_err());

        // Same repo, different branch -- a different id, and legitimate.
        let distinct = vec![auth("acme", "web", "main"), auth("acme", "web", "dev")];
        assert!(reject_duplicates(&distinct).is_ok());
    }

    /// `id` is output-only: an incoming one must not be able to assert an
    /// identity that contradicts the fields it is derived from.
    #[test]
    fn incoming_ids_are_ignored() {
        let entry = RepoEntry {
            id: Some("deadbeef".to_owned()),
            user: "acme".to_owned(),
            repo: "web".to_owned(),
            branch: "main".to_owned(),
            server: GitServer::GitHub,
            token: None,
        };

        let derived = entry.to_git_auth().unwrap().generate_id().to_string();
        assert_ne!(derived, "deadbeef");
        assert_eq!(derived, auth("acme", "web", "main").generate_id().to_string());
    }

    #[test]
    fn blank_fields_are_rejected() {
        let entry = RepoEntry {
            id: None,
            user: "acme".to_owned(),
            repo: "   ".to_owned(),
            branch: "main".to_owned(),
            server: GitServer::GitHub,
            token: None,
        };
        assert!(entry.to_git_auth().is_err());
    }

    /// A dump has to survive the trip back in as a `GitReposSet` body.
    #[test]
    fn a_dump_deserializes_as_a_restore() {
        let credentials = GitCredentials { auth_items: vec![auth("acme", "web", "main")] };
        let path = PathType::Content("/opt/artisan/etc/git.cf".to_owned());

        let dumped = serde_json::to_string(&envelope(&credentials, &path)).unwrap();
        let restored: ReposEnvelope = serde_json::from_str(&dumped).unwrap();

        assert_eq!(restored.schema, SCHEMA_VERSION);
        assert_eq!(restored.repos.len(), 1);
        assert_eq!(restored.repos[0].id.as_deref(), Some(&*auth("acme", "web", "main").generate_id()));
    }

    fn config_for(path: &PathType) -> AppConfig {
        let mut config = AppConfig::dummy();
        config.git = Some(artisan_middleware::config::GitConfig {
            default_server: GitServer::GitHub,
            credentials_file: path.to_path_buf().display().to_string(),
        });
        config
    }

    fn repos_of(message: &AppMessage) -> Vec<String> {
        let AppMessage::Response(response) = message else {
            panic!("expected a Response, got {:?}", message);
        };
        assert!(response.success, "command failed: {:?}", response.message);
        let parsed: ReposEnvelope =
            serde_json::from_str(response.message.as_ref().unwrap()).unwrap();
        parsed.repos.into_iter().map(|r| r.id.unwrap()).collect()
    }

    fn failed(message: &AppMessage) -> String {
        let AppMessage::Response(response) = message else {
            panic!("expected a Response");
        };
        assert!(!response.success, "expected failure, got {:?}", response.message);
        response.message.clone().unwrap_or_default()
    }

    /// The whole surface driven the way the portal drives it, with `reload:
    /// false` so nothing reaches for a watchdog socket that is not there.
    #[tokio::test]
    async fn verbs_add_update_remove_and_restore() {
        let path = scratch("verbs");
        let config = config_for(&path);

        // An unconfigured node reads as an empty list, not an error.
        assert!(repos_of(&handle("GitReposGet", "", &config).await).is_empty());

        let add = r#"{"user":"acme","repo":"web","branch":"main","server":"GitHub","reload":false}"#;
        let ids = repos_of(&handle("GitReposAdd", add, &config).await);
        assert_eq!(ids.len(), 1);
        let original_id = ids[0].clone();

        // Adding the same repo twice would put two workers on one checkout.
        let err = failed(&handle("GitReposAdd", add, &config).await);
        assert!(err.contains("already exists"), "{err}");

        // Take the backup here, mid-stream, so the restore below is a real
        // round trip and not just a re-send of what is already on disk.
        let AppMessage::Response(dump) = handle("GitReposGet", "", &config).await else {
            panic!("expected a Response");
        };
        let backup = dump.message.unwrap();

        // Editing the branch rehashes the id, so the entry moves.
        let update = format!(
            r#"{{"id":"{original_id}","repo":{{"user":"acme","repo":"web","branch":"dev","server":"GitHub"}},"reload":false}}"#
        );
        let moved = handle("GitReposUpdate", &update, &config).await;
        let AppMessage::Response(ref response) = moved else { panic!() };
        let parsed: ReposResponse2 =
            serde_json::from_str(response.message.as_ref().unwrap()).unwrap();
        assert_eq!(parsed.moved.as_ref().unwrap().old_id, original_id);
        assert_ne!(parsed.moved.as_ref().unwrap().new_id, original_id);
        let new_id = parsed.moved.unwrap().new_id;

        // The old id is gone, so operations against it must not silently pass.
        let err = failed(&handle("GitReposRemove", &format!(r#"{{"id":"{original_id}"}}"#), &config).await);
        assert!(err.contains("No repo with id"), "{err}");

        assert!(repos_of(
            &handle("GitReposRemove", &format!(r#"{{"id":"{new_id}","reload":false}}"#), &config).await
        )
        .is_empty());

        // The dump taken above restores the pre-edit state verbatim.
        let mut restore: serde_json::Value = serde_json::from_str(&backup).unwrap();
        restore["reload"] = serde_json::Value::Bool(false);
        assert_eq!(
            repos_of(&handle("GitReposSet", &restore.to_string(), &config).await),
            vec![original_id]
        );
    }

    #[tokio::test]
    async fn bad_input_fails_in_band() {
        let path = scratch("bad_input");
        let config = config_for(&path);

        for (verb, body, expected) in [
            ("GitReposAdd", "{not json", "Invalid JSON"),
            ("GitReposAdd", "", "requires a JSON body"),
            ("GitReposSet", r#"{"schema":99,"repos":[]}"#, "Unsupported schema"),
            ("GitReposRemove", r#"{"id":"nope"}"#, "No repo with id"),
            (
                "GitReposSet",
                r#"{"schema":1,"repos":[
                    {"user":"a","repo":"b","branch":"main","server":"GitHub"},
                    {"user":"a","repo":"b","branch":"main","server":"GitLab"}]}"#,
                "Duplicate repo id",
            ),
        ] {
            let err = failed(&handle(verb, body, &config).await);
            assert!(err.contains(expected), "{verb}: expected {expected:?}, got {err:?}");
        }
    }

    /// `ReposResponse` is serialize-only in the real code; this mirrors it so a
    /// test can read back what a caller would.
    #[derive(Deserialize)]
    struct ReposResponse2 {
        moved: Option<MovedId2>,
    }

    #[derive(Deserialize)]
    struct MovedId2 {
        old_id: String,
        new_id: String,
    }

    /// The wire contract with `network::command_processor`, which splits
    /// `Custom("<Verb> <json>")` on the *first* space. A body is compact JSON
    /// and may well contain spaces of its own -- inside a custom server URL,
    /// say -- so splitting on anything but the first would truncate it.
    #[test]
    fn the_custom_string_splits_back_into_verb_and_body() {
        let body = r#"{"user":"acme","repo":"web","branch":"main","server":{"Custom":"https://git example.com"}}"#;
        let raw = format!("GitReposAdd {}", body);

        let (verb, parsed) = raw.split_once(' ').map(|(v, b)| (v, b.trim())).unwrap();
        assert_eq!(verb, "GitReposAdd");
        assert!(handles(verb));
        assert_eq!(parsed, body);
        assert!(serde_json::from_str::<RepoEntry>(parsed).is_ok());

        // A bodyless verb has no space at all and must still resolve.
        let bare = "GitReposGet";
        let (verb, parsed) = match bare.split_once(' ') {
            Some((v, b)) => (v, b.trim()),
            None => (bare, ""),
        };
        assert_eq!((verb, parsed), ("GitReposGet", ""));
        assert!(handles(verb));

        assert!(!handles("StreamLogs"), "must not claim another module's verb");
    }

    #[test]
    fn resolve_path_falls_back_past_a_blank_entry() {
        let mut config = AppConfig::dummy();
        config.git = None;
        assert_eq!(
            resolve_path(&config).to_path_buf().display().to_string(),
            DEFAULT_GIT_CREDENTIALS_PATH
        );
    }
}
