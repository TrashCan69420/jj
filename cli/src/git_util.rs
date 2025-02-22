// Copyright 2024 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Git utilities shared by various commands.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Instant;
use std::{error, iter};

use itertools::Itertools;
use jj_lib::git::{self, FailedRefExport, FailedRefExportReason, GitImportStats, RefName};
use jj_lib::git_backend::GitBackend;
use jj_lib::op_store::{RefTarget, RemoteRef};
use jj_lib::repo::{ReadonlyRepo, Repo};
use jj_lib::store::Store;
use jj_lib::workspace::Workspace;
use unicode_width::UnicodeWidthStr;

use crate::command_error::{user_error, CommandError};
use crate::formatter::Formatter;
use crate::progress::Progress;
use crate::ui::Ui;

pub fn get_git_repo(store: &Store) -> Result<git2::Repository, CommandError> {
    match store.backend_impl().downcast_ref::<GitBackend>() {
        None => Err(user_error("The repo is not backed by a git repo")),
        Some(git_backend) => Ok(git_backend.open_git_repo()?),
    }
}

pub fn is_colocated_git_workspace(workspace: &Workspace, repo: &ReadonlyRepo) -> bool {
    let Some(git_backend) = repo.store().backend_impl().downcast_ref::<GitBackend>() else {
        return false;
    };
    let Some(git_workdir) = git_backend.git_workdir() else {
        return false; // Bare repository
    };
    if git_workdir == workspace.workspace_root() {
        return true;
    }
    // Colocated workspace should have ".git" directory, file, or symlink. Compare
    // its parent as the git_workdir might be resolved from the real ".git" path.
    let Ok(dot_git_path) = workspace.workspace_root().join(".git").canonicalize() else {
        return false;
    };
    git_workdir.canonicalize().ok().as_deref() == dot_git_path.parent()
}

fn terminal_get_username(ui: &mut Ui, url: &str) -> Option<String> {
    ui.prompt(&format!("Username for {url}")).ok()
}

fn terminal_get_pw(ui: &mut Ui, url: &str) -> Option<String> {
    ui.prompt_password(&format!("Passphrase for {url}: ")).ok()
}

fn pinentry_get_pw(url: &str) -> Option<String> {
    // https://www.gnupg.org/documentation/manuals/assuan/Server-responses.html#Server-responses
    fn decode_assuan_data(encoded: &str) -> Option<String> {
        let encoded = encoded.as_bytes();
        let mut decoded = Vec::with_capacity(encoded.len());
        let mut i = 0;
        while i < encoded.len() {
            if encoded[i] != b'%' {
                decoded.push(encoded[i]);
                i += 1;
                continue;
            }
            i += 1;
            let byte =
                u8::from_str_radix(std::str::from_utf8(encoded.get(i..i + 2)?).ok()?, 16).ok()?;
            decoded.push(byte);
            i += 2;
        }
        String::from_utf8(decoded).ok()
    }

    let mut pinentry = std::process::Command::new("pinentry")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    let mut interact = || -> std::io::Result<_> {
        #[rustfmt::skip]
        let req = format!(
            "SETTITLE jj passphrase\n\
             SETDESC Enter passphrase for {url}\n\
             SETPROMPT Passphrase:\n\
             GETPIN\n"
        );
        pinentry.stdin.take().unwrap().write_all(req.as_bytes())?;
        let mut out = String::new();
        pinentry.stdout.take().unwrap().read_to_string(&mut out)?;
        Ok(out)
    };
    let maybe_out = interact();
    _ = pinentry.wait();
    for line in maybe_out.ok()?.split('\n') {
        if !line.starts_with("D ") {
            continue;
        }
        let (_, encoded) = line.split_at(2);
        return decode_assuan_data(encoded);
    }
    None
}

#[tracing::instrument]
fn get_ssh_keys(_username: &str) -> Vec<PathBuf> {
    let mut paths = vec![];
    if let Some(home_dir) = dirs::home_dir() {
        let ssh_dir = Path::new(&home_dir).join(".ssh");
        for filename in ["id_ed25519_sk", "id_ed25519", "id_rsa"] {
            let key_path = ssh_dir.join(filename);
            if key_path.is_file() {
                tracing::info!(path = ?key_path, "found ssh key");
                paths.push(key_path);
            }
        }
    }
    if paths.is_empty() {
        tracing::info!("no ssh key found");
    }
    paths
}

pub fn with_remote_git_callbacks<T>(
    ui: &mut Ui,
    f: impl FnOnce(git::RemoteCallbacks<'_>) -> T,
) -> T {
    let mut ui = Mutex::new(ui);
    let mut callback = None;
    if let Some(mut output) = ui.get_mut().unwrap().progress_output() {
        let mut progress = Progress::new(Instant::now());
        callback = Some(move |x: &git::Progress| {
            _ = progress.update(Instant::now(), x, &mut output);
        });
    }
    let mut callbacks = git::RemoteCallbacks::default();
    callbacks.progress = callback
        .as_mut()
        .map(|x| x as &mut dyn FnMut(&git::Progress));
    let mut get_ssh_keys = get_ssh_keys; // Coerce to unit fn type
    callbacks.get_ssh_keys = Some(&mut get_ssh_keys);
    let mut get_pw = |url: &str, _username: &str| {
        pinentry_get_pw(url).or_else(|| terminal_get_pw(*ui.lock().unwrap(), url))
    };
    callbacks.get_password = Some(&mut get_pw);
    let mut get_user_pw = |url: &str| {
        let ui = &mut *ui.lock().unwrap();
        Some((terminal_get_username(ui, url)?, terminal_get_pw(ui, url)?))
    };
    callbacks.get_username_password = Some(&mut get_user_pw);
    f(callbacks)
}

pub fn print_git_import_stats(
    ui: &mut Ui,
    repo: &dyn Repo,
    stats: &GitImportStats,
    show_ref_stats: bool,
) -> Result<(), CommandError> {
    if show_ref_stats {
        let refs_stats = stats
            .changed_remote_refs
            .iter()
            .map(|(ref_name, (remote_ref, ref_target))| {
                RefStatus::new(ref_name, remote_ref, ref_target, repo)
            })
            .collect_vec();

        let has_both_ref_kinds = refs_stats
            .iter()
            .any(|x| matches!(x.ref_kind, RefKind::Branch))
            && refs_stats
                .iter()
                .any(|x| matches!(x.ref_kind, RefKind::Tag));

        let max_width = refs_stats.iter().map(|x| x.ref_name.width()).max();
        if let Some(max_width) = max_width {
            let mut stderr = ui.stderr_formatter();
            for status in refs_stats {
                status.output(max_width, has_both_ref_kinds, &mut *stderr)?;
            }
        }
    }

    if !stats.abandoned_commits.is_empty() {
        writeln!(
            ui.stderr(),
            "Abandoned {} commits that are no longer reachable.",
            stats.abandoned_commits.len()
        )?;
    }

    Ok(())
}

struct RefStatus {
    ref_kind: RefKind,
    ref_name: String,
    tracking_status: TrackingStatus,
    import_status: ImportStatus,
}

impl RefStatus {
    fn new(
        ref_name: &RefName,
        remote_ref: &RemoteRef,
        ref_target: &RefTarget,
        repo: &dyn Repo,
    ) -> Self {
        let (ref_name, ref_kind, tracking_status) = match ref_name {
            RefName::RemoteBranch { branch, remote } => (
                format!("{branch}@{remote}"),
                RefKind::Branch,
                if repo.view().get_remote_branch(branch, remote).is_tracking() {
                    TrackingStatus::Tracked
                } else {
                    TrackingStatus::Untracked
                },
            ),
            RefName::Tag(tag) => (tag.clone(), RefKind::Tag, TrackingStatus::NotApplicable),
            RefName::LocalBranch(branch) => {
                (branch.clone(), RefKind::Branch, TrackingStatus::Tracked)
            }
        };

        let import_status = match (remote_ref.target.is_absent(), ref_target.is_absent()) {
            (true, false) => ImportStatus::New,
            (false, true) => ImportStatus::Deleted,
            _ => ImportStatus::Updated,
        };

        Self {
            ref_name,
            tracking_status,
            import_status,
            ref_kind,
        }
    }

    fn output(
        &self,
        max_ref_name_width: usize,
        has_both_ref_kinds: bool,
        out: &mut dyn Formatter,
    ) -> std::io::Result<()> {
        let tracking_status = match self.tracking_status {
            TrackingStatus::Tracked => "tracked",
            TrackingStatus::Untracked => "untracked",
            TrackingStatus::NotApplicable => "",
        };

        let import_status = match self.import_status {
            ImportStatus::New => "new",
            ImportStatus::Deleted => "deleted",
            ImportStatus::Updated => "updated",
        };

        let ref_name_display_width = self.ref_name.width();
        let pad_width = max_ref_name_width.saturating_sub(ref_name_display_width);
        let padded_ref_name = format!("{}{:>pad_width$}", self.ref_name, "", pad_width = pad_width);

        let ref_kind = match self.ref_kind {
            RefKind::Branch => "branch: ",
            RefKind::Tag if !has_both_ref_kinds => "tag: ",
            RefKind::Tag => "tag:    ",
        };

        write!(out, "{ref_kind}")?;
        write!(out.labeled("branch"), "{padded_ref_name}")?;
        writeln!(out, " [{import_status}] {tracking_status}")
    }
}

enum RefKind {
    Branch,
    Tag,
}

enum TrackingStatus {
    Tracked,
    Untracked,
    NotApplicable, // for tags
}

enum ImportStatus {
    New,
    Deleted,
    Updated,
}

pub fn print_failed_git_export(
    ui: &Ui,
    failed_branches: &[FailedRefExport],
) -> Result<(), std::io::Error> {
    if !failed_branches.is_empty() {
        writeln!(ui.warning(), "Failed to export some branches:")?;
        let mut formatter = ui.stderr_formatter();
        for FailedRefExport { name, reason } in failed_branches {
            formatter.write_str("  ")?;
            write!(formatter.labeled("branch"), "{name}")?;
            for err in iter::successors(Some(reason as &dyn error::Error), |err| err.source()) {
                write!(formatter, ": {err}")?;
            }
            writeln!(formatter)?;
        }
        drop(formatter);
        if failed_branches
            .iter()
            .any(|failed| matches!(failed.reason, FailedRefExportReason::FailedToSet(_)))
        {
            writeln!(
                ui.hint(),
                r#"Hint: Git doesn't allow a branch name that looks like a parent directory of
another (e.g. `foo` and `foo/bar`). Try to rename the branches that failed to
export or their "parent" branches."#,
            )?;
        }
    }
    Ok(())
}

/// Expands "~/" to "$HOME/" as Git seems to do for e.g. core.excludesFile.
pub fn expand_git_path(path_str: &str) -> PathBuf {
    if let Some(remainder) = path_str.strip_prefix("~/") {
        if let Ok(home_dir_str) = std::env::var("HOME") {
            return PathBuf::from(home_dir_str).join(remainder);
        }
    }
    PathBuf::from(path_str)
}
