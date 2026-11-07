//! Step-text matching for delivery confirmation: ANSI stripping, normalization and fresh user-step choice.
//! Moved out of resident.rs verbatim (std-split-resident); logic unchanged.

use super::*;

/// Remove ANSI escape sequences so screens can be matched as plain text.
///
/// Covers the three families agy actually emits: CSI (`ESC [ … final`), OSC
/// (`ESC ] … BEL` or `… ESC \`), and single-character charset designators.
pub fn strip_ansi(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        i += 1;
        if i >= bytes.len() {
            break;
        }
        match bytes[i] {
            b'[' => {
                i += 1;
                while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
            }
            b']' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            b'(' | b')' | b'*' | b'+' => i += 2,
            _ => i += 1,
        }
    }
    let lossy = String::from_utf8_lossy(&out);
    lossy.replace("\r\n", "\n").replace('\r', "")
}

/// Collapse whitespace runs to single spaces and trim.
pub fn normalized(text: &str) -> String {
    text.replace('\0', "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether a step's text is the prompt that was delivered.
///
/// Exact after whitespace normalisation, or the step starts with the delivered
/// prompt's first [`MATCH_PREFIX_CHARS`] characters (agy may append its own
/// trailing newline or echo the paste with extra spacing).
pub fn step_text_matches(step: &str, delivered: &str) -> bool {
    step_match_kind(step, delivered).is_some()
}

/// `Some(true)` when the texts are equal after normalisation, `Some(false)` when
/// only the first [`MATCH_PREFIX_CHARS`] match. The prefix arm exists because agy
/// appends its own trailing newline; it is not an identity for an older turn.
pub(super) fn step_match_kind(step: &str, delivered: &str) -> Option<bool> {
    let a = normalized(step);
    let b = normalized(delivered);
    if a.is_empty() || b.is_empty() {
        return None;
    }
    if a == b {
        return Some(true);
    }
    let head: String = b.chars().take(MATCH_PREFIX_CHARS).collect();
    let a_head: String = a.chars().take(head.chars().count()).collect();
    if a_head == head { Some(false) } else { None }
}

/// What was already on disk before a paste. A hit at or below this mark is a
/// previous turn that happens to share the stable prompt prefix.
#[derive(Debug, Clone)]
pub(super) struct DeliveryFloor {
    /// Max step idx per DB that existed before the paste.
    pub(super) seen: HashMap<PathBuf, i64>,
    /// DB this session is allowed to append to, once it has a conversation.
    pub(super) owned: Option<PathBuf>,
}

impl DeliveryFloor {
    pub(super) fn accepts(&self, db: &Path, idx: i64) -> bool {
        if let Some(owned) = &self.owned {
            if db != owned.as_path() {
                return false;
            }
            return idx > self.seen.get(db).copied().unwrap_or(-1);
        }
        // Unbound sessions only bind a file created by this paste. The DB is not
        // supposed to exist before the first delivery; an older file's new row
        // can still be another group's turn.
        !self.seen.contains_key(db)
    }
}

#[derive(Debug, Clone)]
pub(super) struct UserStepHit {
    pub(super) db: PathBuf,
    pub(super) idx: i64,
    pub(super) exact: bool,
}

/// One DB, the newest matching step. Exact text wins over a prefix. Two DBs
/// with no unique exact hit is a refuse: a wrong conversation is worse than
/// no reply.
pub(super) fn choose_fresh_user_step(hits: Vec<UserStepHit>) -> Option<(PathBuf, i64)> {
    if hits.is_empty() {
        return None;
    }
    let exacts: Vec<UserStepHit> = hits.iter().filter(|hit| hit.exact).cloned().collect();
    let pool: &[UserStepHit] = if exacts.is_empty() { &hits } else { &exacts };
    let mut dbs: Vec<&Path> = pool.iter().map(|hit| hit.db.as_path()).collect();
    dbs.sort();
    dbs.dedup();
    if dbs.len() != 1 {
        return None;
    }
    pool.iter()
        .max_by_key(|hit| hit.idx)
        .map(|hit| (hit.db.clone(), hit.idx))
}

/// Byte ranges of `text`, each at most `max_chars` characters, always on a char
/// boundary so a multi-byte code point is never split across two PTY writes.
pub fn chunk_bounds(text: &str, max_chars: usize) -> Vec<Range<usize>> {
    let max_chars = max_chars.max(1);
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut count = 0usize;
    for (offset, ch) in text.char_indices() {
        if count == max_chars {
            out.push(start..offset);
            start = offset;
            count = 0;
        }
        let _ = ch;
        count += 1;
    }
    if start < text.len() {
        out.push(start..text.len());
    }
    out
}
