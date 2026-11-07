//! Per-batch filesystem boundary for Agy calls.
//!
//! Each agent call receives a fresh private directory containing only that batch's
//! staged media. The directory is removed when the Rust turn completes.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_WORKSPACE: AtomicU64 = AtomicU64::new(0);
const MAX_STAGED_IMAGE_BYTES: usize = 1_048_576;

/// A private per-turn directory nested inside the group's project directory.
///
/// The project directory itself is long-lived and holds `AGENTS.md`, which Agy loads on
/// its own every run; only this nested turn directory is created and removed around a
/// single call.
#[derive(Debug)]
pub struct SandboxWorkspace {
    path: PathBuf,
}

impl SandboxWorkspace {
    pub fn create() -> io::Result<Self> {
        let root = std::env::temp_dir().join("agy-work");
        fs::create_dir_all(&root)?;
        let metadata = fs::symlink_metadata(&root)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Agy workspace root must be a real directory",
            ));
        }
        set_private_permissions(&root)?;

        for _ in 0..8 {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let sequence = NEXT_WORKSPACE.fetch_add(1, Ordering::Relaxed);
            let path = root.join(format!(
                "batch-{}-{timestamp}-{sequence}",
                std::process::id()
            ));
            match create_private_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique Agy workspace",
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Write the single normalized collage under a fixed, non-user-controlled name.
    pub fn write_collage(&self, bytes: &[u8]) -> io::Result<PathBuf> {
        if bytes.is_empty() || bytes.len() > MAX_STAGED_IMAGE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "staged image size is outside the allowed range",
            ));
        }
        let path = self.path.join("collage.jpg");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        set_private_file_permissions(&file)?;
        file.write_all(bytes)?;
        file.flush()?;
        Ok(path)
    }

    /// Ensure the long-lived project directory holds the current character card.
    ///
    /// The canonical copy lives here. Each turn directory then hardlinks it so Agy's cwd
    /// actually contains `AGENTS.md` instead of relying on a parent walk.
    pub fn ensure_project_dir(dir: &Path, agents_md: &str) -> io::Result<PathBuf> {
        if agents_md.len() > crate::agent::prompt::AGENTS_MD_MAX_BYTES
            || agents_md.starts_with("---")
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "AGENTS.md must be the plain character card under 64000 bytes",
            ));
        }
        fs::create_dir_all(dir)?;
        set_private_permissions(dir)?;
        let path = dir.join("AGENTS.md");
        // Only touch the file when the content actually differs, so an unchanged card does
        // not rewrite the file every turn.
        if let Ok(existing) = fs::read(&path)
            && existing == agents_md.as_bytes()
        {
            return Ok(path);
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        set_private_file_permissions(&file)?;
        file.write_all(agents_md.as_bytes())?;
        file.flush()?;
        Ok(path)
    }

    /// Declare which group this turn belongs to, for the memory MCP server.
    ///
    /// Writes two copies:
    /// * the per-turn sandbox (create-new; unique dir, so no clobber);
    /// * the group project directory, next to `AGENTS.md`, overwriting if a
    ///   previous turn left a file behind.
    ///
    /// 2026-10-28: the resident PTY cwd/`--add-dir` is the group project
    /// directory, not this turn sandbox. A file that only lived here was
    /// invisible to `roots/list`, so every MCP tool (including `submit_reply`)
    /// failed the scope gate. The project-dir copy is what the MCP child
    /// actually reads. Each turn rewrites it with this group's `bot_id` and
    /// `group_id`, which is the same isolation the original comment described
    /// — two groups still cannot share a file because they do not share a
    /// project directory.
    pub fn write_scope(&self, bot_id: &str, group_id: &str) -> io::Result<PathBuf> {
        let turn_path = write_scope_file(&self.path, bot_id, group_id, true)?;
        if let Some(project) = project_dir_of_turn(&self.path) {
            write_scope_file(project, bot_id, group_id, false)?;
        }
        Ok(turn_path)
    }

    /// Plant `scope.json` in a long-lived project directory, overwriting.
    ///
    /// Used by the canary (no turn sandbox) and as the public overwrite helper.
    pub fn write_project_scope(
        project_dir: &Path,
        bot_id: &str,
        group_id: &str,
    ) -> io::Result<PathBuf> {
        fs::create_dir_all(project_dir)?;
        write_scope_file(project_dir, bot_id, group_id, false)
    }

    /// Run one turn inside an existing project directory.
    ///
    /// The project directory is the group's conversation workspace: it holds the
    /// character card that Agy loads by itself on every run, so a turn only has to stage
    /// this batch's own files. That is what makes the card persistent instead of resending
    /// it as prompt text each time.
    pub fn open_in(project_dir: &Path) -> io::Result<Self> {
        let project = fs::symlink_metadata(project_dir)?;
        if project.file_type().is_symlink() || !project.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "project dir must be a real directory",
            ));
        }
        let root = project_dir.join("turns");
        fs::create_dir_all(&root)?;
        set_private_permissions(&root)?;
        for _ in 0..8 {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let sequence = NEXT_WORKSPACE.fetch_add(1, Ordering::Relaxed);
            let path = root.join(format!(
                "turn-{}-{timestamp}-{sequence}",
                std::process::id()
            ));
            match create_private_dir(&path) {
                Ok(()) => {
                    if let Err(error) = link_agents_md(project_dir, &path) {
                        let _ = fs::remove_dir_all(&path);
                        return Err(error);
                    }
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique Agy turn directory",
        ))
    }
}

impl Drop for SandboxWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// If `turn_path` is `.../turns/turn-*`, return the group project directory.
fn project_dir_of_turn(turn_path: &Path) -> Option<&Path> {
    let turns = turn_path.parent()?;
    if turns.file_name() != Some(std::ffi::OsStr::new("turns")) {
        return None;
    }
    turns.parent()
}

/// Write `scope.json` into `dir`. `create_new` for a unique turn dir; overwrite
/// for the long-lived project dir that a previous turn may already have filled.
fn write_scope_file(
    dir: &Path,
    bot_id: &str,
    group_id: &str,
    create_new: bool,
) -> io::Result<PathBuf> {
    let path = dir.join(crate::mcp::SCOPE_FILE);
    let mut opts = OpenOptions::new();
    opts.write(true);
    if create_new {
        opts.create_new(true);
    } else {
        opts.create(true).truncate(true);
    }
    let mut file = opts.open(&path)?;
    set_private_file_permissions(&file)?;
    file.write_all(crate::mcp::render_scope_json(bot_id, group_id).as_bytes())?;
    file.flush()?;
    Ok(path)
}

/// Put `AGENTS.md` in the turn cwd. Prefer a hardlink of the project card so Agy
/// does not have to walk parents; fall back to a copy when the filesystem refuses.
fn link_agents_md(project_dir: &Path, turn_dir: &Path) -> io::Result<()> {
    let src = project_dir.join("AGENTS.md");
    if !src.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "project AGENTS.md missing",
        ));
    }
    let dest = turn_dir.join("AGENTS.md");
    match fs::hard_link(&src, &dest) {
        Ok(()) => Ok(()),
        Err(_) => {
            fs::copy(&src, &dest)?;
            Ok(())
        }
    }
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

fn set_private_permissions(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn set_private_file_permissions(file: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = file;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocates_private_unique_directories_and_cleans_them_up() {
        let first = SandboxWorkspace::create().expect("workspace");
        let second = SandboxWorkspace::create().expect("workspace");
        assert_ne!(first.path(), second.path());
        assert!(first.path().is_dir());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(first.path()).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }

        let path = first.write_collage(b"png fixture").expect("write collage");
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("collage.jpg")
        );
        assert_eq!(fs::read(&path).expect("read fixture"), b"png fixture");
        let first_path = first.path().to_path_buf();
        drop(first);
        assert!(!first_path.exists());
    }

    #[test]
    fn open_in_links_the_project_agents_md() {
        let dir = tempfile::tempdir().expect("tmp");
        let project = dir.path().join("g");
        SandboxWorkspace::ensure_project_dir(&project, "# card\n").expect("card");
        let ws = SandboxWorkspace::open_in(&project).expect("turn");
        let dest = ws.path().join("AGENTS.md");
        assert_eq!(fs::read_to_string(&dest).expect("read"), "# card\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let a = fs::metadata(project.join("AGENTS.md")).expect("src").ino();
            let b = fs::metadata(&dest).expect("dest").ino();
            assert_eq!(a, b, "turn cwd should hardlink the project card");
        }
    }

    #[test]
    fn write_scope_after_open_in_is_visible_at_the_project_root() {
        // 2026-10-28: the resident PTY cwd/--add-dir is the group project
        // directory. A scope.json that only lives in turns/turn-* is invisible
        // to roots/list, so every MCP tool (including submit_reply) fails the
        // scope gate. This assertion is the regression pin.
        let dir = tempfile::tempdir().expect("tmp");
        let project = dir.path().join("g");
        SandboxWorkspace::ensure_project_dir(&project, "# card\n").expect("card");
        let ws = SandboxWorkspace::open_in(&project).expect("turn");
        ws.write_scope("bot", "100").expect("scope");
        let at_project = project.join(crate::mcp::SCOPE_FILE);
        assert!(
            at_project.is_file(),
            "scope.json must sit next to AGENTS.md, got turn-only at {:?}",
            ws.path().join(crate::mcp::SCOPE_FILE)
        );
        let raw = fs::read_to_string(&at_project).expect("read");
        let scope = crate::mcp::parse_scope_json(&raw).expect("parse");
        assert_eq!(scope.bot_id, "bot");
        assert_eq!(scope.group_id, "100");
        assert!(
            ws.path().join(crate::mcp::SCOPE_FILE).is_file(),
            "turn-dir copy should stay"
        );
    }

    #[test]
    fn write_project_scope_overwrites_a_stale_file() {
        let dir = tempfile::tempdir().expect("tmp");
        let project = dir.path().join("g");
        SandboxWorkspace::write_project_scope(&project, "bot", "100").expect("first");
        SandboxWorkspace::write_project_scope(&project, "bot", "200").expect("second");
        let raw = fs::read_to_string(project.join(crate::mcp::SCOPE_FILE)).expect("read");
        let scope = crate::mcp::parse_scope_json(&raw).expect("parse");
        assert_eq!(scope.group_id, "200");
    }

    #[test]
    fn rejects_empty_or_oversized_staged_files() {
        let workspace = SandboxWorkspace::create().expect("workspace");
        assert!(workspace.write_collage(&[]).is_err());
        assert!(
            workspace
                .write_collage(&vec![0; MAX_STAGED_IMAGE_BYTES + 1])
                .is_err()
        );
    }
}
