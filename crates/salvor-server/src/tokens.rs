//! Named bearer tokens, hashed at rest, with the file re-read when it changes.
//!
//! The single shared secret in [`crate::auth`] answers one question: is the
//! caller allowed in. A token file answers a second one: which caller. Each
//! entry pairs a name an operator chose with the SHA-256 of a token that was
//! minted once and never written down here, so a copy of the file hands
//! nobody a working credential.
//!
//! # File format
//!
//! ```toml
//! [tokens.ci]
//! hash = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
//!
//! [tokens.dashboard]
//! hash = "60303ae22b998861bce3b28f33eec1be758a213c86c93c076dbe9f558c11c752"
//! ```
//!
//! One table per token, the table's key is the name. `hash` is 64 lowercase
//! hex characters, the SHA-256 of the whole presented bearer string, and an
//! entry that gives no `hash`, or gives one that is not a string, is refused
//! by a message naming that entry and that key. `role` is reserved: a file
//! that carries one loads clean today and the key keeps its meaning when a
//! later build reads it. Any other per-token key is ignored with a warning
//! naming it, so a typo alongside a good `hash` is visible in the log rather
//! than silent.
//!
//! # Token wire format
//!
//! A minted token is `sv_` + 43 base62 characters (32 CSPRNG bytes) + `_` +
//! a 6-character checksum: `sv_<43 chars>_<6 chars>`. [`mint`] is the only
//! place that assembles one; [`checksum`] is the only place that computes the
//! trailing six characters, so a caller that mints and a caller that ever
//! checks a checksum read the same six characters off the same payload.
//!
//! Verification never takes that string apart. It hashes the whole presented
//! value, checksum included, through [`digest`], and compares the result
//! against a stored one, so the checksum needs no decoder anywhere in this
//! crate: it is a copy-paste guard for the human moving the token around, not
//! a byte [`TokenSet::match_name`] ever inspects on its own. A token with a
//! checksum some other tool computed differently still verifies, as long as
//! the whole string hashes to a stored entry; a checksum only ever protects
//! against a slip made before the token reaches here.
//!
//! # Why an mtime poll and not a filesystem watcher
//!
//! [`TokenStore::current`] stats the file on each auth attempt and re-parses
//! only when the modification time or the length differs from the last read.
//! A stat is one syscall against a file of a few hundred bytes that the page
//! cache already holds, and it costs nothing on the pass-through path, where
//! no token file is configured at all. The alternative is a watcher crate
//! (`notify`) plus a background thread, which adds a dependency tree and a
//! platform matrix to save a syscall per request. A rewrite that lands within
//! the filesystem's timestamp resolution AND keeps the byte length identical
//! is the case a stat misses; `touch` on the file, or any edit that changes
//! its size, is read on the next request.
//!
//! # What a reload writes down
//!
//! A reload that changes the set logs one `INFO` naming the entries added,
//! the entries removed, and the entries whose hash was replaced under the
//! same name. Names only, never hashes. Someone who can write the file can
//! add an entry, use it, and take it out again; that sequence leaves two
//! lines in a log that ships off the box, which is the record a file
//! comparison after the fact cannot produce. A reload that changes nothing
//! logs nothing, so a `touch` is silent.
//!
//! The mode and owner checks run on every read, not only at startup. A file
//! made group- or world-readable after the server started, or handed to
//! another owner, is refused on its next read: the last set that loaded stays
//! in force and one warning names the file and the mode.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use serde::Deserialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// The prefix every minted token carries, ahead of its random half.
pub const TOKEN_PREFIX: &str = "sv_";

/// The fewest bytes a `--auth-token` value may carry.
///
/// 16 bytes is the floor for the env-var single token, which an operator
/// types or pastes and which is therefore the one that gets set to a
/// memorable word. A stored hash needs no such floor: it hides the length of
/// what produced it, so [`TokenSet`] checks the hash's shape and nothing
/// about the token behind it.
pub const MIN_SINGLE_TOKEN_BYTES: usize = 16;

/// Checks an operator-supplied single token against the entropy floor.
///
/// # Errors
///
/// A message naming the floor and the value's length, for the surface that
/// read the value to print before it binds a port. The value itself is never
/// part of the message.
pub fn check_single_token(token: &str) -> Result<(), String> {
    if token.len() < MIN_SINGLE_TOKEN_BYTES {
        return Err(format!(
            "the token is {} bytes and the floor is {MIN_SINGLE_TOKEN_BYTES}; generate one with \
             `openssl rand -hex 32`",
            token.len()
        ));
    }
    Ok(())
}

/// The lowest byte a token may carry, `!`.
const FIRST_PRINTABLE: u8 = 0x21;

/// The highest byte a token may carry, `~`.
const LAST_PRINTABLE: u8 = 0x7e;

/// Checks that every byte of a token can travel in an `Authorization` header.
///
/// A bearer is presented verbatim as `Authorization: Bearer <token>`, so the
/// token is bounded by what a header value holds and by how the scheme is
/// split off the front of it: printable ASCII, [`FIRST_PRINTABLE`] through
/// [`LAST_PRINTABLE`], and no space. A value carrying anything else can be
/// written into a token file and can never be presented, so it is refused
/// where it is imported rather than at the wire on every request after.
///
/// # Errors
///
/// A message naming the class of byte and the offset it sits at, what a token
/// may carry, and the header the rule comes from. The value itself is never
/// part of the message.
pub fn check_header_safe(token: &str) -> Result<(), String> {
    for (offset, byte) in token.bytes().enumerate() {
        if (FIRST_PRINTABLE..=LAST_PRINTABLE).contains(&byte) {
            continue;
        }
        return Err(format!(
            "the token carries {} at byte {offset}, which an `Authorization` header cannot carry; \
             a token holds printable ASCII only, 0x{FIRST_PRINTABLE:02x} to \
             0x{LAST_PRINTABLE:02x}, and no space, because it is presented as `Authorization: \
             Bearer <token>` and the scheme is split off on the space",
            byte_class(byte)
        ));
    }
    Ok(())
}

/// What kind of byte this is, for [`check_header_safe`]'s message. A class,
/// never the byte's own value, so a message about a secret names the shape of
/// the problem and no part of the secret.
fn byte_class(byte: u8) -> &'static str {
    match byte {
        b'\n' => "a newline",
        b'\r' => "a carriage return",
        b'\t' => "a tab",
        b' ' => "a space",
        0x00..=0x1f | 0x7f => "a control character",
        _ => "a byte outside ASCII",
    }
}

/// Per-token keys this build reads but does not act on. `role` is reserved
/// for the build that gives a token a role, so a file written against that
/// build loads here without a warning about a key on its way in.
const RESERVED_KEYS: &[&str] = &["role"];

/// The SHA-256 of a presented bearer string, the one digest both auth paths
/// compare.
#[must_use]
pub fn digest(presented: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(presented.as_bytes());
    hasher.finalize().into()
}

/// Whether two digests are equal, compared in constant time.
///
/// The comparison runs over all 32 bytes whatever the inputs are, so the time
/// it takes carries no information about how many leading bytes matched.
#[must_use]
pub fn digests_equal(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.ct_eq(right).into()
}

/// Whether two secret strings are equal, compared in constant time.
///
/// Both sides are hashed first, so the comparison is over two fixed-width
/// digests and the time it takes carries nothing about the lengths either.
/// This is what every secret comparison in the crate goes through, in place
/// of `==` on the strings themselves.
#[must_use]
pub fn secrets_equal(left: &str, right: &str) -> bool {
    digests_equal(&digest(left), &digest(right))
}

/// How many base62 characters a minted token's random half carries.
///
/// `62^43 > 2^256`, so every 256-bit value (32 CSPRNG bytes) fits in 43
/// base62 digits with room to spare; no 32-byte value this crate ever mints
/// needs a 44th.
pub const TOKEN_RANDOM_CHARS: usize = 43;

/// How many characters a minted token's trailing checksum carries.
pub const CHECKSUM_LEN: usize = 6;

/// The alphabet a minted token's random half is drawn from: digits, then
/// uppercase, then lowercase letters. Encoding only; nothing in this crate
/// ever decodes a token back out of it (see the module docs), so this
/// ordering is a minting detail, not a wire contract a reader depends on.
const BASE62_ALPHABET: &[u8; 62] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// The alphabet a checksum is drawn from: lowercase letters and digits. 36^6
/// is over two billion, so a typo lands on the right checksum by chance only
/// rarely.
const CHECKSUM_ALPHABET: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";

/// Encodes 32 bytes, most significant first, as [`TOKEN_RANDOM_CHARS`] base62
/// characters, left-padded with the alphabet's zero digit.
///
/// Ordinary base conversion by repeated division: `bytes` is read as one
/// big-endian 256-bit integer and divided down by 62 in place, one base62
/// digit per pass, until nothing is left.
fn base62(bytes: &[u8; 32]) -> String {
    let mut digits = Vec::with_capacity(TOKEN_RANDOM_CHARS);
    let mut remaining = *bytes;
    while remaining.iter().any(|&byte| byte != 0) {
        let mut remainder: u32 = 0;
        for byte in remaining.iter_mut() {
            let acc = remainder * 256 + u32::from(*byte);
            *byte = (acc / 62) as u8;
            remainder = acc % 62;
        }
        digits.push(BASE62_ALPHABET[remainder as usize]);
    }
    while digits.len() < TOKEN_RANDOM_CHARS {
        digits.push(BASE62_ALPHABET[0]);
    }
    digits.reverse();
    String::from_utf8(digits).expect("BASE62_ALPHABET is ASCII")
}

/// The checksum for a token's payload: everything before the trailing
/// checksum, i.e. [`TOKEN_PREFIX`] plus the base62 random half.
///
/// A corruption check, not a security boundary (see the module docs):
/// [`digest`] hashes the whole presented string, checksum included, so a
/// wrong checksum is never by itself a reason a request fails. What it
/// catches is a slip made copying a token by hand. It is the first
/// [`CHECKSUM_LEN`] bytes of `SHA-256(payload)`, each mapped into
/// [`CHECKSUM_ALPHABET`] by remainder, so the same payload always yields the
/// same six characters and a one-character change to the payload almost
/// always yields different ones. Defined once, here, so [`mint`] and any
/// future caller that chooses to check a checksum read the same six
/// characters off the same payload.
#[must_use]
pub fn checksum(payload: &str) -> String {
    let hash = digest(payload);
    hash[..CHECKSUM_LEN]
        .iter()
        .map(|byte| CHECKSUM_ALPHABET[(*byte as usize) % CHECKSUM_ALPHABET.len()] as char)
        .collect()
}

/// Mints a fresh token: 32 bytes from the OS CSPRNG, encoded as
/// [`TOKEN_PREFIX`] + [`TOKEN_RANDOM_CHARS`] base62 characters + `_` + a
/// [`checksum`] of the prefix and the random characters together.
///
/// # Errors
///
/// The underlying `getrandom` call failing, which means the OS's own
/// randomness source is unavailable; this is not expected to happen on any
/// platform this crate ships for.
pub fn mint() -> Result<String, getrandom::Error> {
    let mut random = [0u8; 32];
    getrandom::fill(&mut random)?;
    let payload = format!("{TOKEN_PREFIX}{}", base62(&random));
    let sum = checksum(&payload);
    Ok(format!("{payload}_{sum}"))
}

/// Checks that `path` is mode 0600 or tighter and owned by the user running
/// this process: the same guard [`TokenStore::current`] runs on every read,
/// exposed here so `salvor token new` can refuse a bad token file with the
/// identical message the server would print, rather than a second
/// implementation of the same two checks.
///
/// # Errors
///
/// [`TokenFileError::Read`] if `path` cannot be stat'd, [`TokenFileError::Mode`]
/// if it is readable by group or other, [`TokenFileError::Owner`] if it
/// belongs to another user.
pub fn check_file_guard(path: &Path) -> Result<(), TokenFileError> {
    let meta = fs::metadata(path).map_err(|source| TokenFileError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    check_guard(path, &meta)
}

/// Why a token file was refused at load.
#[derive(Debug, thiserror::Error)]
pub enum TokenFileError {
    /// The file could not be read.
    #[error("token file {path} could not be read: {source}")]
    Read {
        /// The file named on the command line.
        path: PathBuf,
        /// The underlying IO failure.
        source: std::io::Error,
    },
    /// The file is readable by group or other.
    #[error(
        "token file {path} has mode {mode:04o}, which is readable by group or other; \
         token hashes are credentials, so run `chmod 0600 {path}` and start again"
    )]
    Mode {
        /// The file named on the command line.
        path: PathBuf,
        /// The permission bits the file carries.
        mode: u32,
    },
    /// The file is not valid TOML. A repeated `[tokens.<name>]` table lands
    /// here: TOML refuses a duplicate key outright, so two entries under one
    /// name never reach the parser below.
    #[error("token file {path} is not valid TOML: {source}")]
    Parse {
        /// The file named on the command line.
        path: PathBuf,
        /// The parser's own message, which names the line and column.
        source: toml::de::Error,
    },
    /// An entry has no usable `hash` key: it is missing, it holds something
    /// that is not a string, or the entry is not a table at all. Told apart
    /// from [`Parse`](Self::Parse) so a file whose TOML is well formed and
    /// whose entry is wrong is named as the entry it is, rather than as a
    /// syntax error at a line and column.
    #[error(
        "token file {path} gives token `{name}` {problem}; `hash` is 64 lowercase hex characters \
         in quotes, the SHA-256 of the whole token, as `sha256sum` prints it"
    )]
    HashKey {
        /// The file named on the command line.
        path: PathBuf,
        /// The token name whose entry is wrong.
        name: String,
        /// What the entry gives instead of a `hash` string.
        problem: String,
    },
    /// An entry's `hash` is not 64 lowercase hex characters.
    #[error(
        "token file {path} gives token `{name}` a hash of {len} characters that is not 64 \
         lowercase hex; `hash` is the SHA-256 of the whole token, as `sha256sum` prints it"
    )]
    BadHash {
        /// The file named on the command line.
        path: PathBuf,
        /// The token name whose hash is malformed.
        name: String,
        /// How many characters the value carries.
        len: usize,
    },
    /// The file belongs to some other user than the one serving.
    #[error(
        "token file {path} is owned by uid {uid} and this process runs as uid {serving}; \
         a token file another user can rewrite is a token file another user controls, so run \
         `chown {serving} {path}` and start again"
    )]
    Owner {
        /// The file named on the command line.
        path: PathBuf,
        /// The uid that owns the file.
        uid: u32,
        /// The uid this process runs as.
        serving: u32,
    },
    /// The file parsed and declared no tokens at all.
    #[error(
        "token file {path} declares no tokens; add a [tokens.<name>] table with a `hash` key, \
         or drop --token-file and use --auth-token for a single shared secret"
    )]
    Empty {
        /// The file named on the command line.
        path: PathBuf,
    },
}

/// One named token: the name an operator chose and the digest to compare
/// against.
#[derive(Debug, Clone)]
struct Named {
    name: String,
    hash: [u8; 32],
}

/// The token file's contents as this build reads them.
#[derive(Debug, Clone, Default)]
pub struct TokenSet {
    entries: Vec<Named>,
}

/// The whole document, read in two steps: TOML first, then the fields of each
/// entry. Every entry arrives as a raw value and is taken apart in
/// [`TokenSet::parse`], so an entry with no `hash`, or a `hash` holding a
/// number, is refused by a message naming that entry and that key instead of
/// by the TOML parser reporting a struct field it could not fill as a syntax
/// error.
#[derive(Debug, Deserialize)]
struct RawFile {
    #[serde(default)]
    tokens: BTreeMap<String, toml::Value>,
}

/// What a TOML value is, for [`TokenFileError::HashKey`]'s message.
fn value_kind(value: &toml::Value) -> &'static str {
    match value {
        toml::Value::String(_) => "a string",
        toml::Value::Integer(_) => "an integer",
        toml::Value::Float(_) => "a float",
        toml::Value::Boolean(_) => "a boolean",
        toml::Value::Datetime(_) => "a datetime",
        toml::Value::Array(_) => "an array",
        toml::Value::Table(_) => "a table",
    }
}

impl TokenSet {
    /// Parses a token file's text, naming `path` in every refusal.
    ///
    /// # Errors
    ///
    /// [`TokenFileError::Parse`] for text that is not valid TOML (a repeated
    /// token name included), [`TokenFileError::HashKey`] for an entry with no
    /// `hash` key or a `hash` that is not a string,
    /// [`TokenFileError::BadHash`] for a `hash` that is not 64 lowercase hex,
    /// and [`TokenFileError::Empty`] for a file that declares no tokens.
    pub fn parse(path: &Path, text: &str) -> Result<Self, TokenFileError> {
        let raw: RawFile = toml::from_str(text).map_err(|source| TokenFileError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        if raw.tokens.is_empty() {
            return Err(TokenFileError::Empty {
                path: path.to_path_buf(),
            });
        }
        let mut entries = Vec::with_capacity(raw.tokens.len());
        for (name, entry) in raw.tokens {
            let bad_key = |problem: String| TokenFileError::HashKey {
                path: path.to_path_buf(),
                name: name.clone(),
                problem,
            };
            let Some(table) = entry.as_table() else {
                return Err(bad_key(format!(
                    "{} where a [tokens.{name}] table with a `hash` key belongs",
                    value_kind(&entry)
                )));
            };
            let Some(value) = table.get("hash") else {
                return Err(bad_key("no `hash` key".to_owned()));
            };
            let Some(hash_text) = value.as_str() else {
                return Err(bad_key(format!(
                    "a `hash` that is {}, not a string",
                    value_kind(value)
                )));
            };
            let Some(hash) = parse_hash(hash_text) else {
                return Err(TokenFileError::BadHash {
                    path: path.to_path_buf(),
                    name,
                    len: hash_text.chars().count(),
                });
            };
            let ignored: Vec<&str> = table
                .keys()
                .map(String::as_str)
                .filter(|key| *key != "hash" && !RESERVED_KEYS.contains(key))
                .collect();
            if !ignored.is_empty() {
                tracing::warn!(
                    token = %name,
                    file = %path.display(),
                    keys = ?ignored,
                    "token file entry carries keys this build does not read"
                );
            }
            entries.push(Named { name, hash });
        }
        Ok(Self { entries })
    }

    /// The name of the entry whose stored hash equals `presented`, if any.
    ///
    /// Every entry is compared, with no early exit on a match, so the number
    /// of comparisons a request pays for depends on how many tokens the file
    /// declares and not on which one was presented.
    #[must_use]
    pub fn match_name(&self, presented: &[u8; 32]) -> Option<&str> {
        let mut found: Option<&str> = None;
        for entry in &self.entries {
            if digests_equal(&entry.hash, presented) {
                found = Some(entry.name.as_str());
            }
        }
        found
    }

    /// How many tokens the file declares.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the file declares no tokens. A loaded set is never empty:
    /// [`parse`](Self::parse) refuses that file.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every declared name, in the file's sorted order. Names, never hashes.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.name.as_str()).collect()
    }

    /// The digest stored under `name`, for comparing two loads of the file.
    fn hash_for(&self, name: &str) -> Option<&[u8; 32]> {
        self.entries
            .iter()
            .find(|entry| entry.name == name)
            .map(|entry| &entry.hash)
    }

    /// What changed between the set loaded before and this one: names added,
    /// names removed, and names kept whose hash was replaced. Names only.
    #[must_use]
    fn diff(&self, previous: &Self) -> Change {
        let mut change = Change::default();
        for name in self.names() {
            match previous.hash_for(name) {
                None => change.added.push(name.to_owned()),
                Some(before) if self.hash_for(name) != Some(before) => {
                    change.rotated.push(name.to_owned());
                }
                Some(_) => {}
            }
        }
        for name in previous.names() {
            if self.hash_for(name).is_none() {
                change.removed.push(name.to_owned());
            }
        }
        change
    }
}

/// What one reload did to the set, by name.
#[derive(Debug, Default, PartialEq, Eq)]
struct Change {
    added: Vec<String>,
    removed: Vec<String>,
    rotated: Vec<String>,
}

impl Change {
    /// Whether the reload left the set exactly as it was.
    fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.rotated.is_empty()
    }
}

/// Reads 64 lowercase hex characters into a digest, or `None` for anything
/// else. Uppercase hex is refused rather than folded, so the file has one
/// spelling and a diff between two of them is a real difference.
fn parse_hash(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64
        || !text
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
    {
        return None;
    }
    let mut out = [0u8; 32];
    for (slot, pair) in out.iter_mut().zip(text.as_bytes().as_chunks::<2>().0) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        *slot = ((hi << 4) | lo) as u8;
    }
    Some(out)
}

/// What a stat says about the file, and the whole basis for deciding a
/// re-parse is due. Length rides along with the modification time because a
/// filesystem's timestamp resolution is coarser than an edit can be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
}

impl Stamp {
    /// The stamp a stat carries.
    fn of(meta: &fs::Metadata) -> Self {
        Self {
            modified: meta.modified().ok(),
            len: meta.len(),
        }
    }
}

/// The last good set, the stamp it was read at, and the stamp of the last
/// version that would not parse.
#[derive(Debug)]
struct Loaded {
    stamp: Stamp,
    set: Arc<TokenSet>,
    warned_for: Option<Stamp>,
}

/// A token file plus the last set that parsed cleanly from it.
///
/// [`current`](Self::current) is what auth calls. It re-reads the file when
/// the file changed and keeps the last good set otherwise, so adding a token
/// and revoking one both take effect on the next request with no restart.
#[derive(Debug)]
pub struct TokenStore {
    path: PathBuf,
    loaded: Mutex<Loaded>,
}

impl TokenStore {
    /// Reads and parses `path`, refusing a file that is readable by group or
    /// other, does not parse, gives a malformed hash, or declares no tokens.
    ///
    /// # Errors
    ///
    /// A [`TokenFileError`] naming the file and what is wrong with it.
    pub fn load(path: &Path) -> Result<Self, TokenFileError> {
        let (stamp, text) = read_checked(path)?;
        let set = TokenSet::parse(path, &text)?;
        Ok(Self {
            path: path.to_path_buf(),
            loaded: Mutex::new(Loaded {
                stamp,
                set: Arc::new(set),
                warned_for: None,
            }),
        })
    }

    /// The file this store reads.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The current token set: re-read when the file changed since the last
    /// read, the last good set otherwise.
    ///
    /// Three things happen on every call, whether or not the file changed.
    /// The file is stat'd. Its mode and owner are checked, so a file loosened
    /// or handed to another user after the server started is refused on its
    /// next read rather than at the next restart. And a reload that changes
    /// the set logs what it changed, by name.
    ///
    /// A version of the file that will not load keeps the last good set in
    /// place and logs one warning for that version, so a half-written file
    /// mid-save neither opens the server up nor empties it, and a file left
    /// broken does not warn once per request.
    #[must_use]
    pub fn current(&self) -> Arc<TokenSet> {
        let mut loaded = self.loaded.lock().expect("token store lock");
        let meta = match fs::metadata(&self.path) {
            Ok(meta) => meta,
            Err(source) => {
                let error = TokenFileError::Read {
                    path: self.path.clone(),
                    source,
                };
                self.refuse(&mut loaded, None, &error.to_string());
                return loaded.set.clone();
            }
        };
        let stamp = Stamp::of(&meta);
        // Before the change check, not after: a chmod changes neither the
        // modification time nor the length, so a mode this call refused would
        // otherwise be read only on the next content edit.
        if let Err(error) = check_guard(&self.path, &meta) {
            self.refuse(&mut loaded, Some(stamp), &error.to_string());
            return loaded.set.clone();
        }
        if stamp == loaded.stamp {
            return loaded.set.clone();
        }
        let read = fs::read_to_string(&self.path)
            .map_err(|source| TokenFileError::Read {
                path: self.path.clone(),
                source,
            })
            .and_then(|text| TokenSet::parse(&self.path, &text));
        match read {
            Ok(set) => {
                let change = set.diff(&loaded.set);
                loaded.stamp = stamp;
                loaded.set = Arc::new(set);
                loaded.warned_for = None;
                if !change.is_empty() {
                    tracing::info!(
                        file = %self.path.display(),
                        added = ?change.added,
                        removed = ?change.removed,
                        rotated = ?change.rotated,
                        tokens = loaded.set.len(),
                        "token file reloaded"
                    );
                }
                loaded.set.clone()
            }
            Err(error) => {
                self.refuse(&mut loaded, Some(stamp), &error.to_string());
                loaded.set.clone()
            }
        }
    }

    /// Logs one warning per refused version of the file, then keeps quiet
    /// about that version. The last good set is untouched either way, and so
    /// is the stamp it was read at, so a fixed file reloads on the next call.
    fn refuse(&self, loaded: &mut Loaded, stamp: Option<Stamp>, message: &str) {
        if loaded.warned_for == stamp && stamp.is_some() {
            return;
        }
        loaded.warned_for = stamp;
        tracing::warn!(
            file = %self.path.display(),
            detail = %message,
            "token file reload refused; the last set that loaded stays in force"
        );
    }
}

/// Refuses a token file whose mode or owner puts it in reach of anyone but
/// the user serving. Runs at load and on every read.
///
/// On a platform without unix permissions there is nothing to check and every
/// file passes.
fn check_guard(path: &Path, meta: &fs::Metadata) -> Result<(), TokenFileError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(TokenFileError::Mode {
                path: path.to_path_buf(),
                mode,
            });
        }
        // SAFETY: `geteuid` reads a field of the calling process and cannot
        // fail; it takes no arguments and touches no memory the caller owns.
        let serving = unsafe { libc::geteuid() };
        if meta.uid() != serving {
            return Err(TokenFileError::Owner {
                path: path.to_path_buf(),
                uid: meta.uid(),
                serving,
            });
        }
    }
    #[cfg(not(unix))]
    let _ = (path, meta);
    Ok(())
}

/// Checks the file's mode and owner and reads its text, returning the stamp
/// it was read at alongside.
fn read_checked(path: &Path) -> Result<(Stamp, String), TokenFileError> {
    let meta = fs::metadata(path).map_err(|source| TokenFileError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    check_guard(path, &meta)?;
    let text = fs::read_to_string(path).map_err(|source| TokenFileError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    Ok((Stamp::of(&meta), text))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    /// Hex of the SHA-256 of `text`, the way `sha256sum` prints it.
    fn hex(text: &str) -> String {
        digest(text).iter().map(|b| format!("{b:02x}")).collect()
    }

    fn write_mode(path: &Path, text: &str, mode: u32) {
        let mut file = fs::File::create(path).expect("create the token file");
        file.write_all(text.as_bytes()).expect("write it");
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
        }
        let _ = mode;
    }

    #[test]
    fn a_minted_token_verifies_against_its_stored_hash() {
        // The wire format the minting verb produces: prefix, 43 base62
        // characters, an underscore, a 6-character checksum. Verification
        // hashes the whole string, so the checksum is covered by the digest
        // like every other byte.
        let token = format!("{TOKEN_PREFIX}{}_{}", "a".repeat(43), "9f3k2q");
        assert_eq!(token.len(), 3 + 43 + 1 + 6);
        let file = format!("[tokens.ci]\nhash = \"{}\"\n", hex(&token));
        let set = TokenSet::parse(Path::new("tokens.toml"), &file).expect("parse");
        assert_eq!(set.match_name(&digest(&token)), Some("ci"));
        assert_eq!(set.match_name(&digest("sv_wrong")), None);
    }

    #[test]
    fn mint_produces_the_documented_wire_shape() {
        let token = mint().expect("the OS CSPRNG is available in a test process");
        assert!(token.starts_with(TOKEN_PREFIX), "{token}");
        assert_eq!(
            token.len(),
            3 + TOKEN_RANDOM_CHARS + 1 + CHECKSUM_LEN,
            "{token}"
        );
        let rest = &token[TOKEN_PREFIX.len()..];
        let (random, sum) = rest
            .split_once('_')
            .expect("one underscore before the checksum");
        assert_eq!(random.len(), TOKEN_RANDOM_CHARS);
        assert!(
            random.bytes().all(|b| BASE62_ALPHABET.contains(&b)),
            "{random}"
        );
        assert_eq!(sum.len(), CHECKSUM_LEN);
        assert!(sum.bytes().all(|b| CHECKSUM_ALPHABET.contains(&b)), "{sum}");
        // The checksum is exactly what checksum() computes over the payload
        // mint() assembled, so a caller that recomputes it agrees with mint.
        let payload = format!("{TOKEN_PREFIX}{random}");
        assert_eq!(checksum(&payload), sum);
    }

    #[test]
    fn two_mints_never_repeat_and_never_share_a_checksum_by_construction() {
        let a = mint().expect("mint");
        let b = mint().expect("mint");
        assert_ne!(
            a, b,
            "32 CSPRNG bytes colliding twice in a row is not a real risk to guard"
        );
    }

    #[test]
    fn a_one_character_change_to_the_payload_almost_always_changes_the_checksum() {
        // The checksum is a corruption check, not a security boundary (see the
        // module docs): nothing in this crate ever checks it again after
        // minting. What is provable is that it is SENSITIVE to corruption, so a
        // caller who did wire up a check would catch the typo this simulates.
        let payload = format!("{TOKEN_PREFIX}{}", base62(&[7u8; 32]));
        let original = checksum(&payload);
        let mut corrupted = payload.clone();
        // Flip the last character to something else in the same alphabet.
        let last = corrupted.pop().expect("payload is non-empty");
        let replacement = if last == b'0' as char { '1' } else { '0' };
        corrupted.push(replacement);
        assert_ne!(
            checksum(&corrupted),
            original,
            "a corrupted payload's checksum does not have to collide with the original's"
        );
    }

    #[test]
    fn a_hash_that_is_not_64_lowercase_hex_is_refused_by_name() {
        for bad in ["deadbeef", &hex("x").to_uppercase(), &"z".repeat(64)] {
            let text = format!("[tokens.ci]\nhash = \"{bad}\"\n");
            let error = TokenSet::parse(Path::new("tokens.toml"), &text).expect_err("refused");
            let message = error.to_string();
            assert!(message.contains("tokens.toml"), "names the file: {message}");
            assert!(message.contains("`ci`"), "names the token: {message}");
            assert!(
                message.contains("64 lowercase hex"),
                "says what a hash is: {message}"
            );
        }
    }

    #[test]
    fn an_entry_with_no_hash_key_names_the_entry_and_the_key() {
        let error = TokenSet::parse(Path::new("tokens.toml"), "[tokens.ci]\nrole = \"admin\"\n")
            .expect_err("refused");
        let message = error.to_string();
        assert!(message.contains("tokens.toml"), "names the file: {message}");
        assert!(message.contains("`ci`"), "names the entry: {message}");
        assert!(
            message.contains("no `hash` key"),
            "names the key: {message}"
        );
        assert!(
            !message.contains("TOML parse error"),
            "a well-formed file with a wrong entry is not a syntax error: {message}"
        );
    }

    #[test]
    fn an_entry_whose_hash_is_not_a_string_names_the_type_it_carries() {
        let error = TokenSet::parse(Path::new("tokens.toml"), "[tokens.ci]\nhash = 12345\n")
            .expect_err("refused");
        let message = error.to_string();
        assert!(message.contains("tokens.toml"), "names the file: {message}");
        assert!(message.contains("`ci`"), "names the entry: {message}");
        assert!(
            message.contains("a `hash` that is an integer, not a string"),
            "names the key and what it holds: {message}"
        );
        assert!(
            message.contains("64 lowercase hex"),
            "says what a hash is: {message}"
        );
        assert!(
            !message.contains("TOML parse error"),
            "a well-formed file with a wrong entry is not a syntax error: {message}"
        );
    }

    #[test]
    fn an_entry_that_is_not_a_table_names_the_table_it_should_be() {
        let error =
            TokenSet::parse(Path::new("tokens.toml"), "[tokens]\nci = 5\n").expect_err("refused");
        let message = error.to_string();
        assert!(message.contains("`ci`"), "names the entry: {message}");
        assert!(
            message.contains("[tokens.ci] table"),
            "names the table it should be: {message}"
        );
    }

    #[test]
    fn a_file_with_no_tokens_names_the_single_secret_flag() {
        let error = TokenSet::parse(Path::new("tokens.toml"), "").expect_err("refused");
        let message = error.to_string();
        assert!(message.contains("declares no tokens"), "{message}");
        assert!(message.contains("--auth-token"), "{message}");
    }

    #[test]
    fn a_repeated_token_name_is_refused() {
        let text = format!(
            "[tokens.ci]\nhash = \"{h}\"\n\n[tokens.ci]\nhash = \"{h}\"\n",
            h = hex("one")
        );
        let error = TokenSet::parse(Path::new("tokens.toml"), &text).expect_err("refused");
        let message = error.to_string();
        assert!(message.contains("tokens.toml"), "names the file: {message}");
        assert!(
            message.contains("not valid TOML"),
            "a repeated name is a TOML duplicate key: {message}"
        );
    }

    #[test]
    fn a_reserved_key_loads_and_an_unknown_one_still_loads() {
        let text = format!(
            "[tokens.ci]\nhash = \"{h}\"\nrole = \"admin\"\nnickname = \"builder\"\n",
            h = hex("one")
        );
        let set = TokenSet::parse(Path::new("tokens.toml"), &text).expect("parse");
        assert_eq!(set.names(), vec!["ci"]);
        assert_eq!(set.match_name(&digest("one")), Some("ci"));
    }

    #[test]
    #[cfg(unix)]
    fn a_file_readable_by_group_or_other_is_refused_at_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tokens.toml");
        let text = format!("[tokens.ci]\nhash = \"{}\"\n", hex("one"));
        write_mode(&path, &text, 0o644);
        let error = TokenStore::load(&path).expect_err("refused");
        let message = error.to_string();
        assert!(message.contains("0644"), "names the mode: {message}");
        assert!(message.contains("chmod 0600"), "says what to do: {message}");

        write_mode(&path, &text, 0o600);
        let store = TokenStore::load(&path).expect("0600 loads");
        assert_eq!(store.current().names(), vec!["ci"]);
    }

    #[test]
    #[cfg(unix)]
    fn a_rewrite_adds_and_revokes_without_a_reload_call() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tokens.toml");
        write_mode(
            &path,
            &format!("[tokens.ci]\nhash = \"{}\"\n", hex("one")),
            0o600,
        );
        let store = TokenStore::load(&path).expect("load");
        assert_eq!(store.current().match_name(&digest("one")), Some("ci"));

        write_mode(
            &path,
            &format!(
                "[tokens.ci]\nhash = \"{}\"\n\n[tokens.ops]\nhash = \"{}\"\n",
                hex("one"),
                hex("two")
            ),
            0o600,
        );
        assert_eq!(store.current().match_name(&digest("two")), Some("ops"));

        write_mode(
            &path,
            &format!("[tokens.ops]\nhash = \"{}\"\n", hex("two")),
            0o600,
        );
        let set = store.current();
        assert_eq!(set.match_name(&digest("one")), None, "revoked by rewrite");
        assert_eq!(set.match_name(&digest("two")), Some("ops"));
    }

    #[test]
    #[cfg(unix)]
    fn a_reload_reports_what_it_added_removed_and_rotated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tokens.toml");
        write_mode(
            &path,
            &format!(
                "[tokens.ci]\nhash = \"{}\"\n\n[tokens.ops]\nhash = \"{}\"\n",
                hex("one"),
                hex("two")
            ),
            0o600,
        );
        let store = TokenStore::load(&path).expect("load");
        let before = store.current();

        // `ci` keeps its name and gets a new hash, `ops` goes, `bot` arrives.
        write_mode(
            &path,
            &format!(
                "[tokens.ci]\nhash = \"{}\"\n\n[tokens.bot]\nhash = \"{}\"\n",
                hex("one-rotated"),
                hex("three")
            ),
            0o600,
        );
        let after = store.current();
        let change = after.diff(&before);
        assert_eq!(change.added, vec!["bot".to_owned()]);
        assert_eq!(change.removed, vec!["ops".to_owned()]);
        assert_eq!(change.rotated, vec!["ci".to_owned()]);

        // A reload that changes nothing has nothing to report, which is what
        // keeps a `touch` out of the log.
        assert!(after.diff(&after).is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn a_file_loosened_after_startup_is_refused_on_its_next_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tokens.toml");
        let text = format!("[tokens.ci]\nhash = \"{}\"\n", hex("one"));
        write_mode(&path, &text, 0o600);
        let store = TokenStore::load(&path).expect("load");
        assert_eq!(store.current().match_name(&digest("one")), Some("ci"));

        // A chmod alone changes neither the modification time nor the length,
        // so this is exactly the case a change check on its own would miss.
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
        }
        let set = store.current();
        assert_eq!(
            set.match_name(&digest("one")),
            Some("ci"),
            "the last set that loaded stays in force"
        );
        // The refusal is real: a token added while the file is loose does not
        // reach the set.
        write_mode(
            &path,
            &format!(
                "[tokens.ci]\nhash = \"{}\"\n\n[tokens.sneak]\nhash = \"{}\"\n",
                hex("one"),
                hex("sneaky")
            ),
            0o644,
        );
        assert_eq!(store.current().match_name(&digest("sneaky")), None);

        // Tightened again, the same file loads.
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod");
        }
        assert_eq!(store.current().match_name(&digest("sneaky")), Some("sneak"));
    }

    #[test]
    #[cfg(unix)]
    fn a_broken_rewrite_keeps_the_last_good_set() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tokens.toml");
        write_mode(
            &path,
            &format!("[tokens.ci]\nhash = \"{}\"\n", hex("one")),
            0o600,
        );
        let store = TokenStore::load(&path).expect("load");
        assert_eq!(store.current().match_name(&digest("one")), Some("ci"));

        write_mode(&path, "[tokens.ci]\nhash = \"nope\"\n", 0o600);
        let set = store.current();
        assert_eq!(
            set.match_name(&digest("one")),
            Some("ci"),
            "a broken file leaves the last good set in force"
        );
        assert_eq!(set.len(), 1, "and never empties it");
    }

    #[test]
    fn the_entropy_floor_names_itself_and_never_the_value() {
        let short = "hunter2";
        let message = check_single_token(short).expect_err("refused");
        assert!(message.contains("7 bytes"), "{message}");
        assert!(
            message.contains(&MIN_SINGLE_TOKEN_BYTES.to_string()),
            "names the floor: {message}"
        );
        assert!(!message.contains(short), "never the value: {message}");
        check_single_token(&"x".repeat(MIN_SINGLE_TOKEN_BYTES)).expect("the floor itself passes");
    }

    #[test]
    fn a_token_carrying_what_a_header_cannot_is_refused_by_class_and_offset() {
        for (token, class) in [
            ("sv_first\nsecond_half_of_it", "a newline"),
            ("sv_two words in a token here", "a space"),
            ("sv_tab\there_and_more_bytes", "a tab"),
            ("sv_control\u{1}here_and_more", "a control character"),
            ("sv_wide\u{e9}_and_more_bytes", "a byte outside ASCII"),
        ] {
            let message = check_header_safe(token).expect_err("refused");
            assert!(message.contains(class), "{token:?}: {message}");
            assert!(
                message.contains("`Authorization` header"),
                "says why: {message}"
            );
            assert!(
                message.contains("0x21 to 0x7e"),
                "says what a token may hold: {message}"
            );
            assert!(!message.contains(token), "never the value: {message}");
        }
    }

    #[test]
    fn every_byte_a_mint_produces_is_one_a_header_can_carry() {
        let token = mint().expect("the OS CSPRNG is available in a test process");
        check_header_safe(&token).expect("a minted token is presentable");
        // The two boundary bytes of the class, and the two just outside it.
        check_header_safe("!~").expect("the ends of the printable range pass");
        assert!(check_header_safe(" ").is_err(), "a space is out");
        assert!(check_header_safe("\u{7f}").is_err(), "delete is out");
    }

    #[test]
    fn secrets_compare_equal_only_to_themselves() {
        assert!(secrets_equal("dt_abc", "dt_abc"));
        assert!(!secrets_equal("dt_abc", "dt_abd"));
        assert!(!secrets_equal("dt_abc", "dt_abc "));
        assert!(!secrets_equal("", "dt_abc"));
    }
}
