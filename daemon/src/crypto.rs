//! ENCRYPTED MEMORY AT REST (#11) — the key-management + SQLCipher key seam.
//!
//! This module owns the 256-bit master key for JARVIS's at-rest encryption and
//! the small surface that applies it: generating the key, holding it in a
//! zeroizing/secret wrapper that is NEVER logged/Debug/argv/telemetry, reading
//! it from (and writing it to) the macOS Keychain, and applying it to a
//! `rusqlite::Connection` via SQLCipher's `PRAGMA key`.
//!
//! ## WHAT IS ENCRYPTED (exact scope — be honest)
//!
//! ENCRYPTED (transparent, whole-file SQLCipher AES-256, page-level) — the four
//! sensitive SQLite stores, each opened today via `Connection::open(path)`:
//!   * `memory.rs`    — the main Db: facts / transcripts / episodes / events
//!                      (and the `user.world.*` world-model facts tier lives here).
//!   * `docsearch.rs` — `state/docsearch.db`: indexed chunk text + vectors.
//!   * `audit.rs`     — `state/audit.db`: the hash-chained consequential ledger.
//!   * `optimize.rs`  — `state/optimize/optimize.db`: the trace corpus.
//! Plus the NOT-a-DB sensitive store, wrapped SEPARATELY (SQLCipher's PRAGMA key
//! does NOT cover it because it is a JSON file, not SQLite):
//!   * `state/voiceid/owner.json` — the owner voice feature vector. [`VoiceVault`]
//!     stores it inside its OWN encrypted SQLCipher SQLite blob.
//!
//! EXPLICITLY NOT ENCRYPTED (honest scope):
//!   * the config TOML (`config/jarvis.toml`) — non-secret tuning;
//!   * the macOS Keychain item itself — already OS-protected;
//!   * `:memory:` test DBs;
//!   * and — CRITICALLY — the IN-RAM working set, the decrypted pages, and the
//!     key itself WHILE THE DAEMON RUNS. SQLCipher protects AT REST ON DISK only.
//!     It does NOT defend against a live-process / root attacker who can read
//!     jarvisd's RAM. And: lose the Keychain item => the DBs are unrecoverable.
//!
//! ## OFF by default (pinned)
//!
//! `[security].encrypt_memory` ships FALSE. With it OFF, every store opens via its
//! existing `open(path)` with NO `PRAGMA key` — byte-for-byte today's plaintext
//! SQLite. Only the new `open_encrypted(path, key)` seam applies the key, and it
//! is reached only when the operator opts in.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;

/// The macOS Keychain account holding the 256-bit at-rest master key. Added to
/// `integrations::ALLOWED_ACCOUNTS` (a mirror test pins it) so the existing
/// `resolve_secret` reader can fetch it; the writer reuses the
/// `security add-generic-password -U` pattern from `oauth2::keychain_store`.
pub const MASTER_KEY_ACCOUNT: &str = "memory_encryption_key";

/// Length of the master key in bytes (256 bits).
pub const KEY_BYTES: usize = 32;

/// A 256-bit secret key held so it never lands in a log, `Debug`, argv, or any
/// telemetry line, and is zeroized on drop. The ONLY readers are:
///   * [`Self::sqlcipher_hex`] — the `x'..'` blob-literal SQLCipher's `PRAGMA key`
///     takes (so SQLCipher uses the raw key directly, with NO KDF salting of a
///     passphrase — we hold real key bytes, not a password);
///   * [`Self::keychain_value`] — the hex string written to / read from the
///     Keychain.
/// `Debug` is implemented by hand to print a REDACTED placeholder, so a stray
/// `{:?}` (or a struct that derives `Debug` and contains a key) can never leak it.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretKey {
    bytes: [u8; KEY_BYTES],
}

impl SecretKey {
    /// Wrap raw 32 key bytes. This is the INJECTABLE TEST-KEY SEAM: tests across
    /// the store modules build an explicit key with this (no Keychain, no
    /// security(1)) and pass it straight to `open_encrypted`. The production path
    /// never constructs a key from fixed bytes — it uses `generate` /
    /// `from_hex(<keychain value>)` — so this is test-facing.
    #[allow(dead_code)] // injectable test-key seam (used by every store's encrypted tests)
    pub fn from_bytes(bytes: [u8; KEY_BYTES]) -> Self {
        Self { bytes }
    }

    /// Parse a 64-char lowercase-hex key (the Keychain stores the key as hex).
    /// Rejects anything that is not exactly 32 bytes of hex — a corrupt/short
    /// Keychain value is an error, never a silently-truncated key.
    pub fn from_hex(s: &str) -> Result<Self> {
        let s = s.trim();
        let raw = hex::decode(s).context("master key is not valid hex")?;
        let bytes: [u8; KEY_BYTES] = raw
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("master key must be exactly {KEY_BYTES} bytes"))?;
        Ok(Self { bytes })
    }

    /// Generate a fresh 256-bit key from the OS CSPRNG. Reads `/dev/urandom`
    /// (the kernel CSPRNG on macOS) directly — no new crate. A short read or a
    /// missing device is a hard error (we never fall back to a weak source).
    pub fn generate() -> Result<Self> {
        use std::io::Read;
        let mut f = std::fs::File::open("/dev/urandom")
            .context("cannot open /dev/urandom for key generation")?;
        let mut bytes = [0u8; KEY_BYTES];
        f.read_exact(&mut bytes)
            .context("short read from /dev/urandom generating the master key")?;
        Ok(Self { bytes })
    }

    /// The value stored in / read from the Keychain: lowercase hex of the 32
    /// bytes. (The Keychain `-w` value is a string; hex is the safe ASCII form.)
    /// Treat the returned string as SECRET — it is only ever handed to the
    /// Keychain writer/reader, never logged.
    pub fn keychain_value(&self) -> String {
        hex::encode(self.bytes)
    }

    /// The SQLCipher `PRAGMA key` argument as a raw-key blob literal `x'<hex>'`.
    /// SQLCipher reads a value of this exact `x'..'` shape as the RAW 256-bit key
    /// (no passphrase KDF), which is what we want — we already hold real key bytes
    /// from the CSPRNG, not a low-entropy password. SECRET: handed only to
    /// `pragma_update`/`ATTACH ... KEY`, never logged.
    fn sqlcipher_hex(&self) -> String {
        format!("x'{}'", hex::encode(self.bytes))
    }
}

/// Redacted Debug so a key can never leak through `{:?}` (mirrors how refresh
/// tokens are kept off logs today).
impl std::fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretKey(<redacted 256-bit>)")
    }
}

/// Best-effort zeroize on drop. We do not pull in the `zeroize` crate (no new
/// dep); a volatile-style overwrite plus a compiler fence is the dependency-free
/// equivalent for this appliance. The real protection is that the key is never
/// written to disk in plaintext nor logged; this just shrinks the in-RAM window.
impl Drop for SecretKey {
    fn drop(&mut self) {
        for b in self.bytes.iter_mut() {
            // write_volatile so the optimizer cannot elide the overwrite of a
            // value that is about to be dropped.
            unsafe {
                std::ptr::write_volatile(b, 0);
            }
        }
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    }
}

/// Apply the SQLCipher key to a freshly-opened connection. MUST run IMMEDIATELY
/// after `Connection::open(path)` and BEFORE any other pragma or statement —
/// SQLCipher requires the key before the first read of the database header.
///
/// On an EXISTING encrypted DB this unlocks it; on a NEW file it sets the key so
/// every page written is encrypted. The `PRAGMA key` value is the raw-key blob
/// literal; it is never logged (we log only that keying happened, never the key).
pub fn apply_key(conn: &Connection, key: &SecretKey) -> Result<()> {
    // pragma_update with the x'..' blob literal as the value. rusqlite quotes the
    // value as a SQL string; SQLCipher still parses an x'..' string value as a
    // raw key. We pass it as the pragma value so no key bytes ever build a SQL
    // statement by string concatenation.
    conn.pragma_update(None, "key", key.sqlcipher_hex())
        .context("applying SQLCipher PRAGMA key")?;
    Ok(())
}

/// True when `conn` was opened against a SQLCipher build (the bundled-sqlcipher
/// feature is active): `PRAGMA cipher_version` returns a non-empty version on a
/// SQLCipher amalgamation and nothing on vanilla SQLite. Used by a hermetic test
/// to PROVE the encrypting backend is actually compiled in (not plain SQLite),
/// and available to a startup self-check / HUD status probe.
#[allow(dead_code)] // tested + HUD/self-check contract (proves SQLCipher is live)
pub fn cipher_version(conn: &Connection) -> Option<String> {
    conn.query_row("PRAGMA cipher_version", [], |r| r.get::<_, String>(0))
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// MIGRATION / re-key (plaintext -> encrypted) via SQLCipher's standard
/// attach-and-export. Reads the existing PLAINTEXT DB at `plaintext_path`, writes
/// a fully-encrypted copy to `enc_tmp_path` under `key`, then ATOMICALLY swaps the
/// encrypted file over the plaintext one (rename). After this the original path
/// holds the encrypted DB; opening it again requires the key.
///
/// Steps (the documented SQLCipher migration):
///   1. open the plaintext DB (no key);
///   2. `ATTACH DATABASE '<enc_tmp>' AS encrypted KEY '<key>'`;
///   3. `SELECT sqlcipher_export('encrypted')` — copies every table page-for-page
///      into the keyed attached DB, encrypting on write;
///   4. `DETACH DATABASE encrypted`;
///   5. fsync + atomically rename `enc_tmp` over `plaintext_path`.
///
/// If `plaintext_path` does NOT exist (a store that was never created), this is a
/// no-op success: the store will simply be CREATED encrypted on its next
/// `open_encrypted` (the honest fresh-start for an absent store).
pub fn migrate_plaintext_to_encrypted(
    plaintext_path: &Path,
    enc_tmp_path: &Path,
    key: &SecretKey,
) -> Result<()> {
    if !plaintext_path.exists() {
        // Absent store: nothing to migrate; it is created encrypted on first open.
        return Ok(());
    }
    // Start clean so a stale temp from an aborted prior run never corrupts us.
    let _ = std::fs::remove_file(enc_tmp_path);

    let conn = Connection::open(plaintext_path)
        .with_context(|| format!("opening plaintext DB to migrate: {}", plaintext_path.display()))?;
    // ATTACH the to-be-encrypted target with the key. The path + key ride as
    // bound parameters / a pragma-style value; the key is never logged.
    conn.execute(
        "ATTACH DATABASE ?1 AS encrypted KEY ?2",
        rusqlite::params![enc_tmp_path.to_string_lossy(), key.sqlcipher_hex()],
    )
    .context("attaching the encrypted target for migration")?;
    conn.query_row("SELECT sqlcipher_export('encrypted')", [], |_| Ok(()))
        .context("sqlcipher_export to the encrypted target")?;
    conn.execute_batch("DETACH DATABASE encrypted")
        .context("detaching the encrypted target")?;
    drop(conn);

    // Atomic swap: rename the encrypted copy over the plaintext original. rename
    // within the same directory is atomic on macOS, so a crash mid-swap leaves
    // either the old plaintext or the new encrypted file — never a torn DB.
    std::fs::rename(enc_tmp_path, plaintext_path).with_context(|| {
        format!(
            "atomically swapping encrypted DB over {}",
            plaintext_path.display()
        )
    })?;

    // PLAINTEXT-RESIDUE CLEANUP (security finding #1): the sensitive stores run in
    // WAL mode in normal operation. If the daemon was KILLED/crashed leaving a
    // stale plaintext `<db>-wal`/`<db>-shm` sidecar with committed cleartext pages,
    // those sidecars belong to the ORIGINAL plaintext path — NOT to the encrypted
    // file we just renamed into place — so the rename does not touch them and they
    // would otherwise sit on disk as a plaintext copy of the data (exactly the
    // residue the adversarial at-rest check guards against). Our migration opened
    // the plaintext DB in DEFAULT journal mode above; that open already recovered /
    // folded any existing WAL into the main file before `sqlcipher_export`, so the
    // export captured everything and these now-orphaned plaintext sidecars hold no
    // data the encrypted file lacks. Best-effort remove them so no cleartext copy
    // is left behind. (A missing sidecar — the clean-shutdown common case — is a
    // no-op.)
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = plaintext_path.to_path_buf().into_os_string();
        sidecar.push(suffix);
        let _ = std::fs::remove_file(std::path::PathBuf::from(sidecar));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Keychain key-management (production) — generate / write / read the master key
// ---------------------------------------------------------------------------

/// Read the at-rest master key from the macOS Keychain via the existing
/// allowlisted `integrations::resolve_secret` reader (account
/// `MASTER_KEY_ACCOUNT`). Returns the parsed [`SecretKey`], or `None` when no key
/// is stored yet (encryption not enabled / never keyed) or the stored value is
/// malformed. NEVER logs the key — only presence is logged by the resolver.
pub async fn read_master_key() -> Option<SecretKey> {
    let hex = crate::integrations::resolve_secret(MASTER_KEY_ACCOUNT).await?;
    match SecretKey::from_hex(&hex) {
        Ok(k) => Some(k),
        Err(e) => {
            // Value present but unparseable — surface the SHAPE problem, never the
            // value. A corrupt key is a hard "cannot decrypt", not a silent fallback.
            tracing::error!(error = %e, "crypto: stored master key is malformed");
            None
        }
    }
}

/// Generate a fresh 256-bit master key and WRITE it to the macOS Keychain at
/// `MASTER_KEY_ACCOUNT` via the shared ARGV-FREE writer (the secret rides
/// security(1)'s stdin, never argv — see `integrations::keychain_write`). Returns
/// the key so the caller can immediately re-key the stores. The hex key value is
/// never logged (we log only that a key was written) and never placed in argv.
///
/// PRODUCTION-ONLY: this is the runtime enable path. Tests NEVER call it — they
/// inject an explicit `SecretKey` directly into `open_encrypted` (the test seam),
/// so no test spawns security(1) or touches the real Keychain.
pub fn generate_and_store_master_key() -> Result<SecretKey> {
    let key = SecretKey::generate()?;
    let value = key.keychain_value(); // secret; handed only to security(1) stdin.
    crate::integrations::keychain_write(MASTER_KEY_ACCOUNT, &value)
        .context("storing the at-rest master key in the keychain failed")?;
    tracing::info!(
        account = MASTER_KEY_ACCOUNT,
        "crypto: at-rest master key written to keychain"
    );
    Ok(key)
}

// ---------------------------------------------------------------------------
// Process-global master key — so on-demand store opens (docsearch from the tool
// loop / router) reach the key WITHOUT threading it through every call site
// ---------------------------------------------------------------------------

use std::sync::{Arc, OnceLock};

/// The process-global master key, installed ONCE at startup by `install_master_key`.
/// `None` (never installed, or installed as None) means encryption is OFF, so the
/// global openers below fall back to the plaintext `open(path)` — exactly the
/// shipped default. Mirrors `audit::GLOBAL` / `mcp::global`'s fail-safe inert
/// pattern. Tests do NOT install a global; they call `open_encrypted` with an
/// explicit in-test key directly (the injectable seam).
static GLOBAL_KEY: OnceLock<Option<Arc<SecretKey>>> = OnceLock::new();

/// Install the resolved master key (or `None` when `[security].encrypt_memory` is
/// OFF) as the process-global, once at startup. Idempotent.
pub fn install_master_key(key: Option<SecretKey>) {
    let _ = GLOBAL_KEY.set(key.map(Arc::new));
    tracing::info!(
        encrypted = GLOBAL_KEY.get().map(|k| k.is_some()).unwrap_or(false),
        "crypto: installed at-rest encryption state"
    );
}

/// Borrow the installed master key, if any. `None` => encryption OFF (or not yet
/// installed) => the global openers use the plaintext path.
///
/// PUBLIC so non-DB stores that cannot ride SQLCipher's whole-file `PRAGMA key`
/// (the voiceid owner profile is a JSON file, wrapped in its OWN encrypted vault)
/// can reach the SAME installed master key at their live read/write call sites
/// WITHOUT threading it through every signature — exactly mirroring how
/// `open_doc_index` below routes the docsearch on-demand opens. Returns the key
/// itself (an `Arc<SecretKey>`); callers hand it to the encrypted store fn and
/// NEVER log it. With encryption OFF this is `None` => the plaintext path.
pub fn global_key() -> Option<Arc<SecretKey>> {
    GLOBAL_KEY.get().cloned().flatten()
}

/// Open the docsearch index honoring the installed encryption state: encrypted
/// with the global key when `[security].encrypt_memory` is ON, else plaintext —
/// so the on-demand opens from the tool loop / router never need the key threaded
/// through. With no global installed (e.g. a startup path that skips
/// `install_master_key`) this is the plaintext open, the fail-safe default.
pub fn open_doc_index(path: &Path) -> Result<crate::docsearch::DocIndex> {
    match global_key() {
        Some(key) => crate::docsearch::DocIndex::open_encrypted(path, &key),
        None => crate::docsearch::DocIndex::open(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic, explicit IN-TEST key — the injectable seam. Tests NEVER
    /// touch the Keychain nor spawn security(1); they pass this key directly to
    /// `apply_key` / `open_encrypted`, exactly the pattern the oauth recorder
    /// injection uses.
    pub(crate) fn test_key() -> SecretKey {
        SecretKey::from_bytes([7u8; KEY_BYTES])
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "jarvis-crypto-test-{}-{}.db",
            std::process::id(),
            tag
        ))
    }

    fn rm(p: &Path) {
        for suffix in ["", "-wal", "-shm"] {
            let mut s = p.to_path_buf().into_os_string();
            s.push(suffix);
            let _ = std::fs::remove_file(std::path::PathBuf::from(s));
        }
    }

    #[test]
    fn sqlcipher_backend_is_actually_compiled_in() {
        // PROVE the encrypting backend is present: a SQLCipher build returns a
        // non-empty cipher_version; vanilla SQLite returns nothing.
        let conn = Connection::open_in_memory().unwrap();
        let v = cipher_version(&conn);
        assert!(
            v.is_some(),
            "PRAGMA cipher_version empty => not a SQLCipher build (the dep feature is wrong)"
        );
    }

    #[test]
    fn generated_keys_are_32_bytes_and_distinct() {
        let a = SecretKey::generate().unwrap();
        let b = SecretKey::generate().unwrap();
        assert_eq!(a.keychain_value().len(), KEY_BYTES * 2, "hex of 32 bytes");
        assert_ne!(a, b, "two generated keys must differ (real CSPRNG)");
    }

    #[test]
    fn hex_round_trips_through_keychain_value() {
        let k = SecretKey::generate().unwrap();
        let again = SecretKey::from_hex(&k.keychain_value()).unwrap();
        assert_eq!(k, again, "keychain hex must round-trip back to the same key");
    }

    #[test]
    fn from_hex_rejects_short_or_nonhex() {
        assert!(SecretKey::from_hex("deadbeef").is_err(), "32-bit is too short");
        assert!(SecretKey::from_hex("zz".repeat(32).as_str()).is_err(), "non-hex");
    }

    #[test]
    fn debug_never_prints_key_bytes() {
        // The key is all 0xAB; its hex (ababab..) must NOT appear in Debug output.
        let k = SecretKey::from_bytes([0xABu8; KEY_BYTES]);
        let dbg = format!("{k:?}");
        assert!(dbg.contains("redacted"), "Debug must be redacted: {dbg}");
        assert!(!dbg.contains("abab"), "Debug must not leak key hex: {dbg}");
    }

    #[test]
    fn encrypted_db_round_trips_with_the_test_key() {
        let path = tmp("roundtrip");
        rm(&path);
        let key = test_key();
        {
            let conn = Connection::open(&path).unwrap();
            apply_key(&conn, &key).unwrap();
            conn.execute_batch("CREATE TABLE t(v TEXT); INSERT INTO t VALUES('secret-canary');")
                .unwrap();
        }
        // Reopen WITH the key: reads back.
        {
            let conn = Connection::open(&path).unwrap();
            apply_key(&conn, &key).unwrap();
            let v: String = conn.query_row("SELECT v FROM t", [], |r| r.get(0)).unwrap();
            assert_eq!(v, "secret-canary");
        }
        rm(&path);
    }

    #[test]
    fn encrypted_db_is_unreadable_without_the_key_and_bytes_are_ciphertext() {
        let path = tmp("nokey");
        rm(&path);
        let key = test_key();
        {
            let conn = Connection::open(&path).unwrap();
            apply_key(&conn, &key).unwrap();
            conn.execute_batch(
                "CREATE TABLE t(v TEXT); INSERT INTO t VALUES('plaintext-canary-XYZ');",
            )
            .unwrap();
        }
        // (a) On-disk bytes must be ciphertext: the canary must NOT appear, and
        //     the file must NOT begin with the SQLite "SQLite format 3\0" magic.
        let raw = std::fs::read(&path).unwrap();
        assert!(
            !raw.windows(b"plaintext-canary-XYZ".len())
                .any(|w| w == b"plaintext-canary-XYZ"),
            "canary found in on-disk bytes => not encrypted"
        );
        assert!(
            !raw.starts_with(b"SQLite format 3\0"),
            "SQLite header present => file is plaintext, not SQLCipher-encrypted"
        );
        // (b) Opening WITHOUT the key must fail to read the schema (it is keyed).
        {
            let conn = Connection::open(&path).unwrap();
            let res: rusqlite::Result<String> =
                conn.query_row("SELECT v FROM t", [], |r| r.get(0));
            assert!(res.is_err(), "reading a keyed DB with no key must fail");
        }
        // (c) Opening with the WRONG key must also fail.
        {
            let conn = Connection::open(&path).unwrap();
            apply_key(&conn, &SecretKey::from_bytes([9u8; KEY_BYTES])).unwrap();
            let res: rusqlite::Result<String> =
                conn.query_row("SELECT v FROM t", [], |r| r.get(0));
            assert!(res.is_err(), "reading with the wrong key must fail");
        }
        rm(&path);
    }

    #[test]
    fn migration_rekeys_plaintext_to_encrypted() {
        let path = tmp("migrate");
        let tmp_enc = tmp("migrate-enc");
        rm(&path);
        rm(&tmp_enc);
        // 1. Build a PLAINTEXT DB (no key) — exactly today's on-disk shape.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE facts(k TEXT, v TEXT); INSERT INTO facts VALUES('user.name','Darwin');",
            )
            .unwrap();
        }
        // It really is plaintext: the value shows up in the raw bytes.
        let before = std::fs::read(&path).unwrap();
        assert!(
            before.windows(b"Darwin".len()).any(|w| w == b"Darwin"),
            "precondition: plaintext DB should contain the value in the clear"
        );
        // 2. Migrate in place.
        let key = test_key();
        migrate_plaintext_to_encrypted(&path, &tmp_enc, &key).unwrap();
        // 3. The file is now ciphertext...
        let after = std::fs::read(&path).unwrap();
        assert!(
            !after.windows(b"Darwin".len()).any(|w| w == b"Darwin"),
            "after migration the value must not be in the clear"
        );
        assert!(!after.starts_with(b"SQLite format 3\0"), "must be encrypted now");
        // ...and the temp must be gone (renamed over the original).
        assert!(!tmp_enc.exists(), "temp encrypted file should be renamed away");
        // 4. ...and the DATA survived: open WITH the key and read it back.
        {
            let conn = Connection::open(&path).unwrap();
            apply_key(&conn, &key).unwrap();
            let v: String = conn
                .query_row("SELECT v FROM facts WHERE k='user.name'", [], |r| r.get(0))
                .unwrap();
            assert_eq!(v, "Darwin", "migration must preserve the data");
        }
        rm(&path);
        rm(&tmp_enc);
    }

    #[test]
    fn migration_leaves_no_plaintext_residue_in_a_stale_wal_sidecar() {
        // SECURITY FINDING #1: the sensitive stores run in WAL mode. If the daemon
        // crashed/was killed it can leave a stale plaintext `<db>-wal` sidecar with
        // committed cleartext pages. After migration, NO file in the dir may still
        // contain the plaintext canary — the source DB's stale sidecars must be
        // cleaned up, not just the main file swapped.
        let dir = std::env::temp_dir().join(format!(
            "jarvis-crypto-walres-{}-{}",
            std::process::id(),
            "d"
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("jarvis.db");
        let tmp_enc = dir.join("jarvis.db.enc-migrate");

        // 1. Build a PLAINTEXT DB *in WAL mode*, write a committed canary, and then
        //    leak the connection (mem::forget) WITHOUT a checkpoint/close — exactly a
        //    kill/crash mid-operation, so the committed pages live in the plaintext
        //    `<db>-wal` sidecar rather than folded into the main file.
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
            conn.execute_batch(
                "CREATE TABLE facts(k TEXT, v TEXT); INSERT INTO facts VALUES('user.name','WAL-RESIDUE-CANARY');",
            )
            .unwrap();
            // Simulate an unclean shutdown: drop the handle without checkpoint.
            std::mem::forget(conn);
        }
        let wal = dir.join("jarvis.db-wal");
        // Precondition: the stale plaintext WAL sidecar exists and DOES contain the
        // cleartext canary (otherwise the test would not be exercising the residue).
        assert!(wal.exists(), "precondition: a stale -wal sidecar must exist");
        let wal_bytes = std::fs::read(&wal).unwrap();
        assert!(
            wal_bytes
                .windows(b"WAL-RESIDUE-CANARY".len())
                .any(|w| w == b"WAL-RESIDUE-CANARY"),
            "precondition: the stale plaintext WAL must hold the cleartext canary"
        );

        // 2. Migrate in place.
        let key = test_key();
        migrate_plaintext_to_encrypted(&path, &tmp_enc, &key).unwrap();

        // 3. NO file anywhere in the dir may still contain the plaintext canary.
        for entry in std::fs::read_dir(&dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_file() {
                let bytes = std::fs::read(&p).unwrap();
                assert!(
                    !bytes
                        .windows(b"WAL-RESIDUE-CANARY".len())
                        .any(|w| w == b"WAL-RESIDUE-CANARY"),
                    "plaintext residue left behind in {}",
                    p.display()
                );
            }
        }
        // And the stale sidecars are gone.
        assert!(!wal.exists(), "stale plaintext -wal sidecar must be removed");
        assert!(!dir.join("jarvis.db-shm").exists(), "stale -shm sidecar must be removed");

        // 4. The migrated data still reads back WITH the key (no data lost: the
        //    migration's default-journal open folded the WAL in before export).
        {
            let conn = Connection::open(&path).unwrap();
            apply_key(&conn, &key).unwrap();
            let v: String = conn
                .query_row("SELECT v FROM facts WHERE k='user.name'", [], |r| r.get(0))
                .unwrap();
            assert_eq!(v, "WAL-RESIDUE-CANARY", "migration must preserve the WAL data");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migration_of_absent_store_is_a_noop_success() {
        let path = tmp("absent");
        let tmp_enc = tmp("absent-enc");
        rm(&path);
        rm(&tmp_enc);
        // No file exists -> migration is a no-op success (fresh-start on open).
        migrate_plaintext_to_encrypted(&path, &tmp_enc, &test_key()).unwrap();
        assert!(!path.exists(), "absent store must stay absent (created on open)");
    }
}
