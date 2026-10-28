//! Moving the library into a new download folder.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::{file_name, scan};

/// What moving takes along from the download folder `from`: the manga folders and loose
/// chapters the library lists. Anything else stays.
pub fn movable(from: &Path) -> io::Result<Vec<PathBuf>> {
    Ok(scan(from)?
        .into_iter()
        .filter_map(|manga| {
            manga
                .dir
                .or_else(|| manga.chapters.first().map(|chapter| chapter.path.clone()))
        })
        .collect())
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct MoveReport {
    pub moved: usize,
    /// Names the new folder already had, left where they were.
    pub skipped: Vec<String>,
    /// Names that couldn't be moved, and why; they're still in the old folder.
    pub failed: Vec<(String, String)>,
}

/// Moves `entries` into `to` one at a time, calling `progress` with each one's index and name
/// as it starts. Nothing in `to` is overwritten.
pub fn move_entries(entries: &[PathBuf], to: &Path, progress: impl Fn(usize, &str)) -> MoveReport {
    let mut report = MoveReport::default();
    if let Err(err) = fs::create_dir_all(to) {
        report.failed = entries
            .iter()
            .map(|path| (file_name(path), err.to_string()))
            .collect();
        return report;
    }
    for (index, path) in entries.iter().enumerate() {
        let name = file_name(path);
        progress(index, &name);
        let target = to.join(&name);
        if to.starts_with(path) {
            let reason = "the new folder is inside it".to_owned();
            report.failed.push((name, reason));
        } else if fs::symlink_metadata(&target).is_ok() {
            report.skipped.push(name);
        } else {
            match move_one(path, &target) {
                Ok(()) => report.moved += 1,
                Err(err) => report.failed.push((name, err.to_string())),
            }
        }
    }
    report
}

/// Renames when both folders are on the same drive, which is instant; copies otherwise.
fn move_one(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Err(err) if err.kind() == io::ErrorKind::CrossesDevices => copy_then_remove(from, to),
        result => result,
    }
}

/// Copies under a hidden temporary name, renames that into place and only then removes the
/// original. When copying fails the partial copy goes and the original stays as it was.
fn copy_then_remove(from: &Path, to: &Path) -> io::Result<()> {
    let partial = to.with_file_name(format!(".{}.toshokan-moving", file_name(to)));
    // Left over from a move that was cut short.
    let _ = remove(&partial);
    if let Err(err) = copy(from, &partial) {
        let _ = remove(&partial);
        return Err(err);
    }
    fs::rename(&partial, to)?;
    remove(from)
}

fn copy(from: &Path, to: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(from)?;
    if metadata.is_symlink() {
        let message = format!("{} is a link; move it yourself", from.display());
        return Err(io::Error::other(message));
    }
    if !metadata.is_dir() {
        return fs::copy(from, to).map(|_| ());
    }
    fs::create_dir(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        copy(&entry.path(), &to.join(entry.file_name()))?;
    }
    Ok(())
}

fn remove(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn write(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"page").unwrap();
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn moves_the_library_and_leaves_everything_else() {
        let old = tempfile::tempdir().unwrap();
        let new = tempfile::tempdir().unwrap();
        write(&old.path().join("Berserk/Ch. 0001.cbz"));
        write(&old.path().join("Berserk/cover.jpg"));
        write(&old.path().join("Akira 01.cbz"));
        write(&old.path().join("Vagabond/Ch. 0001.cbz"));
        write(&old.path().join("notes.txt"));
        // The new folder already has its own Vagabond.
        write(&new.path().join("Vagabond/Ch. 0002.cbz"));

        let entries = movable(old.path()).unwrap();
        assert_eq!(entries.len(), 3);
        let started = RefCell::new(Vec::new());
        let report = move_entries(&entries, new.path(), |index, name| {
            started.borrow_mut().push((index, name.to_owned()));
        });
        assert_eq!(
            report,
            MoveReport {
                moved: 2,
                skipped: vec!["Vagabond".to_owned()],
                failed: vec![],
            }
        );
        assert_eq!(started.borrow()[0], (0, "Akira 01.cbz".to_owned()));
        assert_eq!(names(old.path()), ["Vagabond", "notes.txt"]);
        assert_eq!(names(new.path()), ["Akira 01.cbz", "Berserk", "Vagabond"]);
        assert_eq!(
            names(&new.path().join("Berserk")),
            ["Ch. 0001.cbz", "cover.jpg"]
        );
        assert_eq!(names(&new.path().join("Vagabond")), ["Ch. 0002.cbz"]);
    }

    #[test]
    fn copies_across_drives_and_cleans_up_a_failed_copy() {
        let old = tempfile::tempdir().unwrap();
        let new = tempfile::tempdir().unwrap();
        write(&old.path().join("Berserk/Ch. 0001/001.png"));
        write(&old.path().join("Berserk/Ch. 0002.cbz"));
        copy_then_remove(&old.path().join("Berserk"), &new.path().join("Berserk")).unwrap();
        assert!(names(old.path()).is_empty());
        assert_eq!(
            names(&new.path().join("Berserk")),
            ["Ch. 0001", "Ch. 0002.cbz"]
        );
        assert_eq!(names(&new.path().join("Berserk/Ch. 0001")), ["001.png"]);

        // A link can't be copied: nothing is left half-done.
        write(&old.path().join("Akira/Ch. 0001.cbz"));
        std::os::unix::fs::symlink("/etc", old.path().join("Akira/elsewhere")).unwrap();
        let err = copy_then_remove(&old.path().join("Akira"), &new.path().join("Akira"));
        assert!(err.unwrap_err().to_string().contains("is a link"));
        assert_eq!(
            names(&old.path().join("Akira")),
            ["Ch. 0001.cbz", "elsewhere"]
        );
        assert_eq!(names(new.path()), ["Berserk"]);
    }

    #[test]
    fn never_moves_a_folder_into_itself() {
        let old = tempfile::tempdir().unwrap();
        write(&old.path().join("Berserk/Ch. 0001.cbz"));
        let inside = old.path().join("Berserk/new library");
        let report = move_entries(&movable(old.path()).unwrap(), &inside, |_, _| {});
        assert_eq!(report.moved, 0);
        assert_eq!(report.failed[0].1, "the new folder is inside it");
        assert!(old.path().join("Berserk/Ch. 0001.cbz").exists());
    }
}
