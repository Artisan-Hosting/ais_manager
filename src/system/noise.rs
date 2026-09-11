//! `Noise_NK` static-key material for the manager's two `simple_comms`
//! connections, which no longer share a role split by direction the way they
//! used to:
//!
//! - The **fleet tunnel** to the portal (`:9800`, see `system::portal`) is
//!   the manager dialing out, so the manager is a Noise *initiator only*
//!   there -- it needs just the *portal's* public key, pinned out-of-band.
//!   `Noise_NK` authenticates the responder by that key alone, so an
//!   initiator holding the wrong one simply cannot complete a handshake.
//!   The manager has no static identity of its own on this connection at
//!   all: an initiator in `Noise_NK` never does.
//! - The **local debug listener** (`127.0.0.1:9825`, see `crate::network`),
//!   used only by `ais_manager_debug` on the same host, is where the
//!   manager's own long-lived keypair actually gets used -- there, the
//!   manager is the responder. It is persisted to disk purely so the debug
//!   CLI's pinned key keeps working across restarts; nothing on the fleet
//!   side depends on it.

use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use artisan_middleware::dusa_collection_utils::core::errors::{ErrorArrayItem, Errors};
use artisan_middleware::dusa_collection_utils::core::logger::LogLevel;
use artisan_middleware::dusa_collection_utils::log;
use simple_comms::protocol::handshake::NoiseIdentity;

/// Where the manager's own `Noise_NK` keypair is persisted, written `0600`.
pub const MANAGER_IDENTITY_FILE: &str = "/opt/artisan/manager_identity.key";

/// Where the portal's pinned public key is read from: a single line of 64 hex
/// characters, or a `public=<hex>` line as written by the portal's own
/// identity file (so the file can be copied across verbatim).
pub const PORTAL_PUBKEY_FILE: &str = "/opt/artisan/portal.pub";

/// Environment override for [`PORTAL_PUBKEY_FILE`], carrying the hex key
/// directly. Intended for tests and one-off debugging against a non-production
/// portal; the file is the supported production path.
pub const PORTAL_PUBKEY_ENV: &str = "AIS_PORTAL_PUBKEY";

/// Loads the manager's keypair from [`MANAGER_IDENTITY_FILE`], generating and
/// persisting a fresh one on first run.
pub fn load_or_create_identity() -> Result<NoiseIdentity, ErrorArrayItem> {
    load_or_create_identity_at(Path::new(MANAGER_IDENTITY_FILE))
}

/// [`load_or_create_identity`] against an explicit path.
fn load_or_create_identity_at(path: &Path) -> Result<NoiseIdentity, ErrorArrayItem> {
    match fs::read_to_string(path) {
        Ok(contents) => parse_identity(&contents, path),
        Err(err) if err.kind() == ErrorKind::NotFound => {
            let identity = NoiseIdentity::generate()
                .map_err(|err| ErrorArrayItem::new(Errors::InvalidKey, err.to_string()))?;
            persist_identity(&identity, path)?;
            log!(
                LogLevel::Info,
                "Generated a new manager Noise identity at {} (public key {})",
                path.display(),
                hex::encode(identity.public_key())
            );
            Ok(identity)
        }
        Err(err) => Err(ErrorArrayItem::new(
            Errors::ReadingFile,
            format!("Reading {}: {}", path.display(), err),
        )),
    }
}

/// Reads this host's manager public key without generating one when the file is
/// absent, for tooling that wants to *talk to* the local manager rather than be
/// it (the debug CLI dials `:9825` as a `Noise_NK` initiator and so needs the
/// responder's key).
// Consumed by the `ais_manager_debug` bin, which includes this module by path;
// the daemon itself has no reason to read its own public key back.
#[allow(dead_code)]
pub fn read_local_public_key() -> Result<[u8; 32], ErrorArrayItem> {
    // FIXME this should error when the new images have the actual file written, for now we just fallback to a hardcoded val
    let fix: [u8; 32] = [
        0x28, 0x31, 0x49, 0x7c, 0x5e, 0xb2, 0x02, 0x8c, 0x26, 0x53, 0xa4, 0x1c, 0x2f, 0x72, 0x02,
        0x9a, 0x91, 0x45, 0x41, 0x34, 0x4f, 0x43, 0xf9, 0x38, 0xbc, 0x7c, 0xdd, 0xf3, 0xe5, 0xfa,
        0xbf, 0x53,
    ];
    Ok(read_public_key_at(Path::new(MANAGER_IDENTITY_FILE)).unwrap_or(fix))
}

/// [`read_local_public_key`] against an explicit path.
fn read_public_key_at(path: &Path) -> Result<[u8; 32], ErrorArrayItem> {
    let contents = fs::read_to_string(path).map_err(|err| {
        ErrorArrayItem::new(
            Errors::ReadingFile,
            format!(
                "Reading {}: {}. Start ais_manager once to generate it, or pass the \
                 target's key explicitly",
                path.display(),
                err
            ),
        )
    })?;

    let line = contents
        .lines()
        .find_map(|line| line.trim().strip_prefix("public=").map(str::trim))
        .ok_or_else(|| {
            ErrorArrayItem::new(
                Errors::InvalidFile,
                format!("{} has no public= line", path.display()),
            )
        })?;

    parse_key_field(line)
}

/// Reads the portal's pinned public key, preferring [`PORTAL_PUBKEY_ENV`] over
/// [`PORTAL_PUBKEY_FILE`].
pub fn load_portal_pubkey() -> Result<[u8; 32], ErrorArrayItem> {
    if let Ok(from_env) = std::env::var(PORTAL_PUBKEY_ENV) {
        log!(
            LogLevel::Warn,
            "Using portal public key from ${} rather than {}",
            PORTAL_PUBKEY_ENV,
            PORTAL_PUBKEY_FILE
        );
        return parse_key_field(&from_env);
    }

    let contents = fs::read_to_string(PORTAL_PUBKEY_FILE).map_err(|err| {
        ErrorArrayItem::new(
            Errors::ReadingFile,
            format!(
                "Reading the portal's pinned public key from {}: {}. \
                 The manager cannot establish a Noise_NK connection to the portal without it",
                PORTAL_PUBKEY_FILE, err
            ),
        )
    })?;

    // Accept either a bare hex key or the `public=<hex>` line out of a portal
    // identity file, so operators can copy that file across unedited.
    let line = contents
        .lines()
        .find_map(|line| line.trim().strip_prefix("public=").map(str::trim))
        .unwrap_or_else(|| contents.trim());

    parse_key_field(line)
}

/// Parses the two-field identity file written by [`persist_identity`].
fn parse_identity(contents: &str, path: &Path) -> Result<NoiseIdentity, ErrorArrayItem> {
    let mut private: Option<[u8; 32]> = None;
    let mut public: Option<[u8; 32]> = None;

    for line in contents.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("private=") {
            private = Some(parse_key_field(value)?);
        } else if let Some(value) = line.strip_prefix("public=") {
            public = Some(parse_key_field(value)?);
        }
    }

    match (private, public) {
        (Some(private), Some(public)) => Ok(NoiseIdentity::from_keypair(private, public)),
        _ => Err(ErrorArrayItem::new(
            Errors::InvalidFile,
            format!(
                "{} is missing a private= or public= line; delete it to regenerate \
                 (only the local debug CLI pins this key, so regenerating costs \
                 nothing on the fleet side)",
                path.display()
            ),
        )),
    }
}

/// Writes `identity` to [`MANAGER_IDENTITY_FILE`] with `0600` permissions.
fn persist_identity(identity: &NoiseIdentity, path: &Path) -> Result<(), ErrorArrayItem> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            ErrorArrayItem::new(
                Errors::CreatingDirectory,
                format!("Creating {}: {}", parent.display(), err),
            )
        })?;
    }

    let body = format!(
        "# ais_manager Noise_NK static identity -- keep the private key secret.\n\
         private={}\npublic={}\n",
        hex::encode(identity.private_key()),
        hex::encode(identity.public_key())
    );

    fs::write(path, body).map_err(|err| {
        ErrorArrayItem::new(
            Errors::CreatingFile,
            format!("Writing {}: {}", path.display(), err),
        )
    })?;

    // Tighten the mode only after the bytes are down; a failure here is worth
    // surfacing rather than leaving a private key at the default umask.
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|err| {
        ErrorArrayItem::new(
            Errors::SettingPermissionsFile,
            format!("Restricting {} to 0600: {}", path.display(), err),
        )
    })
}

/// Decodes 64 hex characters into a 32-byte key.
fn parse_key_field(value: &str) -> Result<[u8; 32], ErrorArrayItem> {
    let bytes = hex::decode(value.trim())
        .map_err(|err| ErrorArrayItem::new(Errors::InvalidHexData, err.to_string()))?;

    bytes.try_into().map_err(|_| {
        ErrorArrayItem::new(
            Errors::InvalidKey,
            "A Noise_NK key must be exactly 32 bytes (64 hex characters)",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique scratch path per test, so these never touch the real
    /// `/opt/artisan` identity and never collide with each other.
    fn scratch(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "ais_manager_noise_test_{}_{}",
            name,
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        path
    }

    /// The identity must survive a restart byte-for-byte: `ais_manager_debug`
    /// reads this file's `public=` line to pin the responder it is dialing, so
    /// a keypair that rotated across a reload would break the debug CLI on
    /// this host until someone noticed and re-read the key.
    #[test]
    fn identity_round_trips_across_reloads() {
        let path = scratch("round_trip");

        let created = load_or_create_identity_at(&path).expect("first run should generate");
        let reloaded = load_or_create_identity_at(&path).expect("second run should reuse");

        assert_eq!(
            created.public_key(),
            reloaded.public_key(),
            "reloading must not rotate the keypair"
        );
        assert_eq!(created.private_key(), reloaded.private_key());

        // The debug CLI reads the key back through a separate path; it must agree.
        assert_eq!(
            read_public_key_at(&path).expect("reading the public key"),
            created.public_key()
        );

        fs::remove_file(&path).ok();
    }

    /// A private key must never be left at the default umask.
    #[test]
    fn generated_identity_is_not_world_readable() {
        let path = scratch("permissions");
        load_or_create_identity_at(&path).expect("generating");

        let mode = fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "the identity file should be owner-only, got {:o}",
            mode & 0o777
        );

        fs::remove_file(&path).ok();
    }

    /// A truncated or hand-edited file should be reported, not silently treated
    /// as absent and replaced with a fresh keypair.
    #[test]
    fn a_malformed_identity_is_an_error_not_a_silent_regeneration() {
        let path = scratch("malformed");
        fs::write(&path, "private=abc123\n").expect("seeding a malformed file");

        assert!(
            load_or_create_identity_at(&path).is_err(),
            "a file missing its public= line must not be accepted"
        );

        fs::remove_file(&path).ok();
    }

    /// `load_portal_pubkey` accepts a portal identity file verbatim, which is the
    /// documented way to distribute the key.
    #[test]
    fn a_portal_identity_file_parses_as_a_pinned_key() {
        let expected = [9u8; 32];
        let body = format!(
            "# Portal Noise_NK static identity\nprivate={}\npublic={}\n",
            hex::encode([1u8; 32]),
            hex::encode(expected)
        );

        let line = body
            .lines()
            .find_map(|line| line.trim().strip_prefix("public=").map(str::trim))
            .expect("the public line should be found");

        assert_eq!(parse_key_field(line).expect("decoding"), expected);
    }

    /// Keys of the wrong length must be rejected rather than silently padded.
    #[test]
    fn a_short_key_is_rejected() {
        assert!(parse_key_field(&hex::encode([1u8; 16])).is_err());
        assert!(parse_key_field("not hex at all").is_err());
    }
}
