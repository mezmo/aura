//! The session manifest: the declared, committed artifact set.
//!
//! One cumulative manifest per session, stored as a JSONB column on the
//! claims row (Q7 ruling: manifest-in-Postgres, so commit publication is
//! one atomic statement — invariant I4). Entries are epoch-qualified
//! paths with content digests; the map only ever grows across epochs.
//!
//! Two readers rely on it:
//!
//! - the claiming pod, to build the GC keep-set and the reify history
//!   view, straight from the granted claim (no filesystem read);
//! - the artifact read path (the aura seam), which verifies each file's
//!   digest on first read. With write-once paths (I1) a digest mismatch
//!   can only mean platform corruption — fail loud, never retry
//!   ([`ReadMiss`]).

use std::collections::{BTreeMap, btree_map};
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::epoch::Epoch;
use crate::identity::TurnId;

/// An artifact's path relative to the session root, always
/// epoch-qualified (`e{k}/...`).
///
/// Business rule (parse, don't validate — fill-phase): a single
/// relative path, first component `e{digits}`, no `.`/`..` components,
/// no separators beyond `/`. The parsed epoch prefix is carried as a
/// field so provenance can be *checked* against it ([`Manifest::declare`]
/// rejects a path/entry epoch mismatch). The write-once invariant (I1)
/// is enforced at `declare`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ArtifactPath {
    raw: String,
    epoch: Epoch,
}

/// Why a raw string is not an [`ArtifactPath`]. Diagnostic-only.
#[derive(Debug, thiserror::Error)]
#[error("invalid artifact path: {reason}")]
pub struct InvalidArtifactPath {
    /// The validation failure, for diagnostics only.
    pub reason: String,
}

impl ArtifactPath {
    /// Parse and constrain an artifact path. The sole constructor;
    /// fills both the raw form and the parsed epoch prefix.
    ///
    /// # Errors
    /// [`InvalidArtifactPath`] when the path is not epoch-qualified or
    /// contains a forbidden component.
    pub fn parse(raw: &str) -> Result<Self, InvalidArtifactPath> {
        if raw.is_empty() {
            return Err(InvalidArtifactPath {
                reason: "empty path".into(),
            });
        }
        if raw.contains('\\') {
            return Err(InvalidArtifactPath {
                reason: "backslash separator".into(),
            });
        }
        let mut segments = raw.split('/');
        let first = segments.next().unwrap_or_default();
        let digits = first.strip_prefix('e').ok_or_else(|| InvalidArtifactPath {
            reason: "path must be epoch-qualified (e{k}/...)".into(),
        })?;
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(InvalidArtifactPath {
                reason: "epoch prefix must be e{digits}".into(),
            });
        }
        // Reject non-canonical digit strings: `epoch_dir` derives the
        // directory as `e{k}`, so a leading-zero prefix like `e01` would
        // parse to epoch 1 while preserving `e01/...` — a permanent
        // read-miss. Parse-don't-validate: reject, never normalize.
        if digits.len() > 1 && digits.starts_with('0') {
            return Err(InvalidArtifactPath {
                reason: "epoch prefix must be canonical (no leading zeros)".into(),
            });
        }
        let k = digits.parse::<u64>().map_err(|_| InvalidArtifactPath {
            reason: "epoch prefix out of range".into(),
        })?;
        let epoch = Epoch::from_raw(k).ok_or_else(|| InvalidArtifactPath {
            reason: "epoch 0 is unreachable".into(),
        })?;
        // The epoch prefix must be followed by a path body; a bare `e1`
        // would collide with the epoch directory itself.
        let body = segments.next().ok_or_else(|| InvalidArtifactPath {
            reason: "epoch prefix requires a path body".into(),
        })?;
        if body.is_empty() {
            return Err(InvalidArtifactPath {
                reason: "empty path segment".into(),
            });
        }
        if body == "." || body == ".." {
            return Err(InvalidArtifactPath {
                reason: "dot component".into(),
            });
        }
        for segment in segments {
            if segment.is_empty() {
                return Err(InvalidArtifactPath {
                    reason: "empty path segment".into(),
                });
            }
            if segment == "." || segment == ".." {
                return Err(InvalidArtifactPath {
                    reason: "dot component".into(),
                });
            }
        }
        Ok(Self {
            raw: raw.to_string(),
            epoch,
        })
    }

    /// The epoch prefix the path was parsed from (`e{k}/...` → `k`).
    #[must_use]
    pub const fn epoch(&self) -> Epoch {
        self.epoch
    }
}

impl AsRef<str> for ArtifactPath {
    fn as_ref(&self) -> &str {
        &self.raw
    }
}

impl AsRef<std::path::Path> for ArtifactPath {
    fn as_ref(&self) -> &std::path::Path {
        std::path::Path::new(&self.raw)
    }
}

impl fmt::Display for ArtifactPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl Serialize for ArtifactPath {
    /// The JSON form is the plain path string (including as a map key).
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_ref())
    }
}

impl<'de> Deserialize<'de> for ArtifactPath {
    /// Deserialization validates: a manifest carrying an unparseable
    /// path fails to load rather than smuggling it through.
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct PathVisitor;
        impl serde::de::Visitor<'_> for PathVisitor {
            type Value = ArtifactPath;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an epoch-qualified artifact path string")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                ArtifactPath::parse(v).map_err(serde::de::Error::custom)
            }
        }
        d.deserialize_string(PathVisitor)
    }
}

/// A content digest (sha256) over one committed artifact. The wire form
/// is lowercase hex.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest([u8; 32]);

/// Why a raw string is not a [`Digest`]. Diagnostic-only.
#[derive(Debug, thiserror::Error)]
#[error("invalid digest: {reason}")]
pub struct InvalidDigest {
    /// The validation failure, for diagnostics only.
    pub reason: String,
}

/// The value of a single hex digit, either case.
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

impl Digest {
    /// Wrap an already-computed digest.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw digest bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Parse a lowercase hex digest. The sole string constructor.
    ///
    /// # Errors
    /// [`InvalidDigest`] when the string is not 64 lowercase hex digits.
    pub fn from_hex(raw: &str) -> Result<Self, InvalidDigest> {
        if raw.len() != 64 {
            return Err(InvalidDigest {
                reason: "digest must be exactly 64 hex chars".into(),
            });
        }
        let mut bytes = [0u8; 32];
        for (i, chunk) in raw.as_bytes().chunks(2).enumerate() {
            let hi = hex_val(chunk[0]).ok_or_else(|| InvalidDigest {
                reason: "non-hex character".into(),
            })?;
            let lo = hex_val(chunk[1]).ok_or_else(|| InvalidDigest {
                reason: "non-hex character".into(),
            })?;
            bytes[i] = (hi << 4) | lo;
        }
        Ok(Self(bytes))
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl Serialize for Digest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct DigestVisitor;
        impl serde::de::Visitor<'_> for DigestVisitor {
            type Value = Digest;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a 64-digit lowercase hex sha256")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Digest::from_hex(v).map_err(serde::de::Error::custom)
            }
        }
        d.deserialize_string(DigestVisitor)
    }
}

/// One manifest entry: the committed artifact's digest and provenance.
/// Public fields are all validated types; `bytes` is a plain metric (no
/// domain rule branches on it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// The artifact's sha256.
    pub digest: Digest,
    /// The turn that committed it.
    pub turn: TurnId,
    /// The epoch whose dir holds it.
    pub epoch: Epoch,
    /// Byte length (diagnostics and drain sizing).
    pub bytes: u64,
}

/// Why a declaration was rejected. The two variants are the two rules a
/// manifest enforces; nothing else can fail.
#[derive(Debug, thiserror::Error)]
pub enum DeclareError {
    /// The write-once rule (I1): the path already has an entry.
    #[error("artifact path already declared (write-once violation): {0}")]
    AlreadyDeclared(ArtifactPath),
    /// The entry's provenance epoch disagrees with the path's epoch
    /// prefix — a cross-field state production never reaches through
    /// `ActiveTurn::write_artifact`, but a hand-built delta is rejected
    /// here rather than stored.
    #[error("entry epoch {declared} disagrees with path prefix: {path}")]
    EpochMismatch {
        /// The offending path.
        path: ArtifactPath,
        /// The epoch the entry claimed.
        declared: Epoch,
    },
}

/// The cumulative declared artifact set for a session. Serialized
/// transparently as the entries map itself, so a fresh row's `'{}'`
/// jsonb deserializes to the empty manifest. Construction of new entries
/// goes through [`declare`](Self::declare), which rejects duplicate
/// paths and epoch mismatches: a manifest can grow but never mutate or
/// misattribute an entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Manifest {
    entries: BTreeMap<ArtifactPath, ManifestEntry>,
}

impl Manifest {
    /// The empty manifest of a fresh session.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Number of declared artifacts.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is declared yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entry for a path, if declared.
    #[must_use]
    pub fn get(&self, path: &ArtifactPath) -> Option<&ManifestEntry> {
        self.entries.get(path)
    }

    /// Every declared path — the GC keep-set's source.
    pub fn paths(&self) -> impl Iterator<Item = &ArtifactPath> {
        self.entries.keys()
    }

    /// Every (path, entry) pair.
    pub fn entries(&self) -> impl Iterator<Item = (&ArtifactPath, &ManifestEntry)> {
        self.entries.iter()
    }

    /// Declare a newly committed artifact. Write-once (I1): declaring a
    /// path that already has an entry is an error, never an overwrite.
    /// The entry's epoch must equal the path's prefix epoch, so
    /// provenance cannot disagree with location.
    ///
    /// # Errors
    /// [`DeclareError::AlreadyDeclared`] when the path has an entry;
    /// [`DeclareError::EpochMismatch`] when entry and path epochs
    /// disagree.
    pub fn declare(
        &mut self,
        path: ArtifactPath,
        entry: ManifestEntry,
    ) -> Result<(), DeclareError> {
        if entry.epoch != path.epoch() {
            return Err(DeclareError::EpochMismatch {
                declared: entry.epoch,
                path,
            });
        }
        match self.entries.entry(path) {
            btree_map::Entry::Vacant(vacant) => {
                vacant.insert(entry);
                Ok(())
            }
            btree_map::Entry::Occupied(occupied) => {
                Err(DeclareError::AlreadyDeclared(occupied.key().clone()))
            }
        }
    }

    /// Merge a turn's delta into the base manifest, entry by entry. The
    /// barrier's commit path uses this, so a delta that collides with or
    /// misattributes committed state fails the commit rather than
    /// replacing it (cumulativeness is enforced, not asserted).
    ///
    /// # Errors
    /// The first [`DeclareError`] encountered; entries declared before
    /// the failure stay declared (callers treat any error as fatal to
    /// the commit, so the partial state never reaches the store).
    pub fn extend_from(&mut self, delta: Manifest) -> Result<(), DeclareError> {
        for (path, entry) in delta.entries {
            self.declare(path, entry)?;
        }
        Ok(())
    }
}

/// How a manifest-referenced read failed. The two variants are the two
/// failure classes with opposite correct responses; collapsing them
/// would make corruption retried or propagation failed loud.
#[derive(Debug, thiserror::Error)]
pub enum ReadMiss {
    /// The file is not visible — propagation class. Retry inside the
    /// propagation window, then escalate through the repair lane.
    #[error("referenced artifact not yet visible: {0}")]
    NotFound(ArtifactPath),
    /// The file read but does not match the manifest digest —
    /// corruption class (with write-once paths, mismatch can only be
    /// platform corruption). Fail loud immediately; never retry.
    #[error("referenced artifact digest mismatch at {path}: expected {expected}, read {actual}")]
    Corrupt {
        /// The referenced path.
        path: ArtifactPath,
        /// The manifest's digest.
        expected: Digest,
        /// The digest of the bytes actually read.
        actual: Digest,
    },
}
