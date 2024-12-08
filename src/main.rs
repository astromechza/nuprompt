use std::error::Error;
use std::io::{stdout, BufWriter, Write};
use std::os::unix::prelude::OsStrExt;
use std::path::PathBuf;
use std::str::FromStr;
use git2::{Repository, Status, StatusOptions};
use log::debug;

struct GitBits {
    index_modified: bool,
    worktree_modified: bool,
    untracked_files: bool,
}

fn main() -> Result<(), Box<dyn Error>> {
    env_logger::init();

    let username = users::get_current_username();
    let pwd = std::env::var("PWD").map(PathBuf::from).ok().map(|mut p| {
        let ceil: &[PathBuf] = &[];
        let git_bits: Option<(String, GitBits)> = Repository::open_ext(&p, git2::RepositoryOpenFlags::empty(), ceil).ok().map_or_else(|| {
            None
        }, |r| {
            let mut gb = GitBits{
                index_modified: false,
                worktree_modified: false,
                untracked_files: false,
            };
            if let Ok(x) = r.statuses(Some(StatusOptions::new().include_ignored(false).include_untracked(true).exclude_submodules(true).include_unreadable(false))) {
                for x in x.iter() {
                    debug!("git status {:?}: {:?}", x.path(), x.status());
                    match x.status() {
                        Status::WT_NEW => {gb.untracked_files = true}
                        Status::WT_MODIFIED => {gb.worktree_modified = true}
                        Status::WT_DELETED => {gb.worktree_modified = true}
                        Status::WT_TYPECHANGE => {gb.worktree_modified = false}
                        Status::WT_RENAMED => {gb.worktree_modified = true}
                        Status::INDEX_NEW => {gb.index_modified = true}
                        Status::INDEX_MODIFIED => {gb.index_modified = true}
                        Status::INDEX_DELETED => {gb.index_modified = true}
                        Status::INDEX_TYPECHANGE => {gb.index_modified = true}
                        Status::INDEX_RENAMED => {gb.index_modified = true}
                        _ => {}
                    }
                }
            }
            match r.head() {
                Ok(h) => Some((h.shorthand().unwrap().to_owned(), gb)),
                _ => Some(("NO HEAD".to_string(), gb)),
            }
        });

        if let Ok(h) = std::env::var("HOME").map(PathBuf::from) {
            if p.starts_with(&h) {
                p = PathBuf::from_str("~").unwrap().join(p.strip_prefix(h).unwrap())
            }
        }

        (p.into_os_string(), git_bits)
    });

    let mut stdout_lock = stdout().lock();
    let mut buf_writer = BufWriter::new(&mut stdout_lock);
    buf_writer.write_all(b"[")?;
    match username {
        None => buf_writer.write_all("UNKNOWN USER".as_bytes())?,
        Some(u) => buf_writer.write_all(u.as_bytes())?,
    };
    buf_writer.write_all(b" ")?;
    match pwd.as_ref() {
        None => buf_writer.write_all("NO PWD".as_bytes())?,
        Some(p) => {
            if let Some(git_bits) = &p.1 {
                buf_writer.write_all(git_bits.0.as_bytes())?;
                if git_bits.1.untracked_files || git_bits.1.worktree_modified || git_bits.1.index_modified {
                    buf_writer.write_all(b":")?;
                    if git_bits.1.index_modified {
                        buf_writer.write_all(b"s")?;
                    }
                    if git_bits.1.worktree_modified {
                        buf_writer.write_all(b"d")?;
                    }
                    if git_bits.1.untracked_files {
                        buf_writer.write_all(b"u")?;
                    }
                }
                buf_writer.write_all(b" ")?;
            }
            buf_writer.write_all(p.0.as_bytes())?;
        },
    };
    buf_writer.write_all(b" > ")?;
    buf_writer.flush()?;
    Ok(())
}
