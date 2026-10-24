//! Saving finished recording transcripts as files automatically (the
//! Transcribe page's "Save transcripts automatically" setting), for files
//! from the Transcribe page and meetings alike. The CLI never does.
//!
//! The file goes next to the recording or into the chosen folder, as a
//! Word document, plain text or Markdown, rendered by `export` exactly like
//! an export. It is always a new file ("interview (2).docx" when
//! "interview.docx" exists), and a transcript with gaps says so in its name.
//! When the folder can't be written, the file goes to Documents/Handy
//! Transcripts instead; so does the transcript of a recording in the
//! temporary folder (taken out of a .zip file, say), which is emptied later.

use crate::file_transcription::export::{self, ExportOptions};
use crate::file_transcription::types::{FileTranscript, SavedTranscriptFile, TranscriptStatus};
use crate::settings::{TranscriptAutoSave, TranscriptSaveLocation};
use std::fs;
use std::path::{Path, PathBuf};

/// The folder in Documents that takes transcripts whose own folder can't be
/// written.
pub const FALLBACK_FOLDER_NAME: &str = "Handy Transcripts";
/// Added to the file name of a transcript with gaps.
pub const INCOMPLETE_SUFFIX: &str = " (incomplete)";

/// The file name without its extension: the recording's ("interview" for
/// "interview.m4a"), plus " (incomplete)" when the transcript has gaps.
pub fn file_stem(transcript: &FileTranscript) -> String {
    let stem = export::export_file_stem(&transcript.source_name);
    if transcript.status == TranscriptStatus::Partial {
        format!("{stem}{INCOMPLETE_SUFFIX}")
    } else {
        stem
    }
}

/// The folder `settings` saves the transcript of the recording at
/// `source_path` in: the chosen folder (when one is chosen and absolute),
/// else the recording's. None when the recording's path has no folder.
pub fn target_folder(source_path: &Path, settings: &TranscriptAutoSave) -> Option<PathBuf> {
    if let Some(chosen) = chosen_folder(settings) {
        return Some(chosen);
    }
    source_path
        .parent()
        .filter(|folder| !folder.as_os_str().is_empty())
        .map(Path::to_path_buf)
}

/// The folder `settings` chose, when they save into one and it is absolute.
fn chosen_folder(settings: &TranscriptAutoSave) -> Option<PathBuf> {
    let chosen = Path::new(settings.folder.trim());
    (settings.location == TranscriptSaveLocation::Folder && chosen.is_absolute())
        .then(|| chosen.to_path_buf())
}

/// Whether `folder` is the temporary folder `temp_dir` or inside it, as
/// written or once both are resolved (Windows may spell the temporary
/// folder with short names, as in C:\Users\DOUGLA~1\AppData\Local\Temp).
fn is_in_temp_folder(folder: &Path, temp_dir: &Path) -> bool {
    if folder.starts_with(temp_dir) {
        return true;
    }
    match (fs::canonicalize(folder), fs::canonicalize(temp_dir)) {
        (Ok(folder), Ok(temp_dir)) => folder.starts_with(temp_dir),
        _ => false,
    }
}

/// Documents/Handy Transcripts, in the user's Documents folder `documents`.
pub fn fallback_folder(documents: &Path) -> PathBuf {
    documents.join(FALLBACK_FOLDER_NAME)
}

/// Writes `transcript` to a new file as `settings` asks (whether or not
/// they are enabled: the caller decides that) and returns where. When its
/// folder can't be written, the file goes to the fallback folder in
/// `documents` (the user's Documents folder, created if missing; None: no
/// fallback). So does the transcript of a recording in `temp_dir` (the
/// system's temporary folder; None: none), as that folder is emptied later,
/// with the transcript in it; with no fallback, it is saved there all the
/// same. Err with an English reason when it couldn't be saved at all.
pub fn save(
    transcript: &FileTranscript,
    settings: &TranscriptAutoSave,
    documents: Option<&Path>,
    temp_dir: Option<&Path>,
    options: &ExportOptions,
) -> Result<SavedTranscriptFile, String> {
    let format = settings.format.transcript_format();
    let contents = export::render_file(transcript, format, options)
        .map_err(|e| format!("Failed to render the transcript: {e}"))?;
    let stem = file_stem(transcript);
    let extension = format.extension();

    let target = target_folder(Path::new(&transcript.source_path), settings);
    let fallback = documents
        .map(fallback_folder)
        .filter(|fallback| target.as_deref() != Some(fallback.as_path()));
    // A drive's root, or a folder that holds Documents, is no real
    // temporary folder (a misconfigured TMP), so it is ignored.
    let temp_dir = temp_dir.filter(|temp| {
        temp.parent().is_some() && !documents.is_some_and(|documents| documents.starts_with(temp))
    });
    let in_temp = |folder: &Path| temp_dir.is_some_and(|temp| is_in_temp_folder(folder, temp));
    let first_error = match &target {
        // The transcript of a recording in the temporary folder would be
        // emptied with it, so it goes to the fallback; a folder the user
        // chose is used wherever it is.
        Some(folder)
            if fallback.is_some() && chosen_folder(settings).is_none() && in_temp(folder) =>
        {
            format!(
                "{} is a temporary folder, which is emptied later",
                folder.display()
            )
        }
        Some(folder) => {
            // A chosen folder that was removed is made again; the
            // recording's folder exists, or the recording is gone too.
            let made = if settings.location == TranscriptSaveLocation::Folder {
                fs::create_dir_all(folder)
            } else {
                Ok(())
            };
            match made.and_then(|()| export::write_new_file(folder, &stem, extension, &contents)) {
                Ok(path) => return Ok(saved(&path, None)),
                Err(e) => format!("Couldn't write to {}: {e}", folder.display()),
            }
        }
        None => "The recording's folder is unknown".to_string(),
    };

    let Some(fallback) = fallback else {
        return Err(first_error);
    };
    fs::create_dir_all(&fallback)
        .and_then(|()| export::write_new_file(&fallback, &stem, extension, &contents))
        .map(|path| saved(&path, target.as_deref()))
        .map_err(|e| {
            format!(
                "{first_error}; couldn't write to {} either: {e}",
                fallback.display()
            )
        })
}

fn saved(path: &Path, unwritable_folder: Option<&Path>) -> SavedTranscriptFile {
    SavedTranscriptFile {
        path: path.to_string_lossy().into_owned(),
        unwritable_folder: unwritable_folder.map(|folder| folder.to_string_lossy().into_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_transcription::types::{TimingGrain, TranscriptSegment};
    use crate::settings::TranscriptSaveFormat;

    fn transcript(source: &Path, status: TranscriptStatus) -> FileTranscript {
        let text = "Hello there.";
        FileTranscript {
            source_path: source.to_string_lossy().into_owned(),
            source_name: source
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            duration_ms: 4_000,
            model_id: "tiny".to_string(),
            model_name: Some("Tiny".to_string()),
            language: Some("en".to_string()),
            status,
            status_detail: (status == TranscriptStatus::Partial)
                .then(|| "Transcription stopped early".to_string()),
            timing: TimingGrain::Segment,
            text: text.to_string(),
            paragraphs: vec![TranscriptSegment::new(0, 4_000, text)],
            segments: vec![TranscriptSegment::new(0, 4_000, text)],
            created_at: 1_700_000_000,
            history_id: None,
            speakers: None,
        }
    }

    fn settings(format: TranscriptSaveFormat) -> TranscriptAutoSave {
        TranscriptAutoSave {
            format,
            ..TranscriptAutoSave::default()
        }
    }

    #[test]
    fn file_stems_follow_the_recording_and_mark_gaps() {
        let source = Path::new("/audio/interview.final.m4a");
        assert_eq!(
            file_stem(&transcript(source, TranscriptStatus::Completed)),
            "interview.final"
        );
        assert_eq!(
            file_stem(&transcript(source, TranscriptStatus::Partial)),
            "interview.final (incomplete)"
        );
    }

    #[test]
    fn target_folder_is_the_recordings_unless_one_is_chosen() {
        let source = Path::new("/audio/talks/interview.m4a");
        let mut settings = TranscriptAutoSave::default();
        assert_eq!(
            target_folder(source, &settings),
            Some(PathBuf::from("/audio/talks"))
        );

        // A folder is used only when chosen, and only an absolute one.
        let chosen = std::env::temp_dir().join("handy-chosen");
        settings.folder = chosen.to_string_lossy().into_owned();
        assert_eq!(
            target_folder(source, &settings),
            Some(PathBuf::from("/audio/talks"))
        );
        settings.location = TranscriptSaveLocation::Folder;
        assert_eq!(target_folder(source, &settings), Some(chosen));
        for unusable in ["", "  ", "relative/folder"] {
            settings.folder = unusable.to_string();
            assert_eq!(
                target_folder(source, &settings),
                Some(PathBuf::from("/audio/talks")),
                "{unusable:?}"
            );
        }
        assert_eq!(target_folder(Path::new("interview.m4a"), &settings), None);
    }

    #[test]
    fn saves_a_word_document_next_to_the_recording() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("interview.m4a");
        fs::write(&source, b"audio").unwrap();
        let t = transcript(&source, TranscriptStatus::Completed);

        let saved = save(
            &t,
            &TranscriptAutoSave::default(),
            None,
            None,
            &ExportOptions::default(),
        )
        .unwrap();
        assert_eq!(
            saved,
            SavedTranscriptFile {
                path: dir
                    .path()
                    .join("interview.docx")
                    .to_string_lossy()
                    .into_owned(),
                unwritable_folder: None,
            }
        );
        let bytes = fs::read(&saved.path).unwrap();
        assert!(bytes.starts_with(b"PK"), "a ZIP package");
        assert_eq!(fs::read(&source).unwrap(), b"audio");
    }

    #[test]
    fn never_replaces_a_file_and_marks_incomplete_transcripts() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("interview.m4a");
        fs::write(dir.path().join("interview.txt"), "mine\n").unwrap();
        let options = ExportOptions::default();
        let txt = settings(TranscriptSaveFormat::Txt);

        let complete = transcript(&source, TranscriptStatus::Completed);
        let first = save(&complete, &txt, None, None, &options).unwrap();
        assert_eq!(
            first.path,
            dir.path().join("interview (2).txt").to_string_lossy()
        );
        assert_eq!(fs::read_to_string(&first.path).unwrap(), "Hello there.\n");
        assert_eq!(
            fs::read_to_string(dir.path().join("interview.txt")).unwrap(),
            "mine\n"
        );

        let partial = transcript(&source, TranscriptStatus::Partial);
        let saved = save(&partial, &txt, None, None, &options).unwrap();
        assert_eq!(
            saved.path,
            dir.path()
                .join("interview (incomplete).txt")
                .to_string_lossy()
        );
        let again = save(&partial, &txt, None, None, &options).unwrap();
        assert_eq!(
            again.path,
            dir.path()
                .join("interview (incomplete) (2).txt")
                .to_string_lossy()
        );

        let md = save(
            &complete,
            &settings(TranscriptSaveFormat::Md),
            None,
            None,
            &options,
        )
        .unwrap();
        assert_eq!(md.path, dir.path().join("interview.md").to_string_lossy());
        assert!(fs::read_to_string(&md.path)
            .unwrap()
            .starts_with("# interview.m4a\n"));
    }

    #[test]
    fn saves_into_a_chosen_folder_and_makes_it_again() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("recordings").join("interview.m4a");
        let chosen = dir.path().join("transcripts").join("2026");
        let settings = TranscriptAutoSave {
            location: TranscriptSaveLocation::Folder,
            folder: chosen.to_string_lossy().into_owned(),
            format: TranscriptSaveFormat::Txt,
            ..TranscriptAutoSave::default()
        };
        let saved = save(
            &transcript(&source, TranscriptStatus::Completed),
            &settings,
            None,
            None,
            &ExportOptions::default(),
        )
        .unwrap();
        assert_eq!(saved.path, chosen.join("interview.txt").to_string_lossy());
        assert_eq!(saved.unwritable_folder, None);
    }

    #[test]
    fn falls_back_to_documents_when_the_folder_cant_be_written() {
        let dir = tempfile::tempdir().unwrap();
        // The recording's folder is gone (or can't be written).
        let source = dir.path().join("unplugged").join("interview.m4a");
        let documents = dir.path().join("Documents");
        fs::create_dir(&documents).unwrap();
        let t = transcript(&source, TranscriptStatus::Completed);
        let txt = settings(TranscriptSaveFormat::Txt);
        let options = ExportOptions::default();

        let saved = save(&t, &txt, Some(&documents), None, &options).unwrap();
        let fallback = documents.join(FALLBACK_FOLDER_NAME);
        assert_eq!(saved.path, fallback.join("interview.txt").to_string_lossy());
        assert_eq!(
            saved.unwritable_folder.as_deref(),
            Some(dir.path().join("unplugged").to_string_lossy().as_ref())
        );
        // Names stay free there too.
        let again = save(&t, &txt, Some(&documents), None, &options).unwrap();
        assert_eq!(
            again.path,
            fallback.join("interview (2).txt").to_string_lossy()
        );

        // Without a Documents folder there is nowhere else to go.
        let err = save(&t, &txt, None, None, &options).unwrap_err();
        assert!(err.contains("unplugged"), "{err}");
        assert!(!dir.path().join("unplugged").exists());
    }

    #[cfg(unix)]
    #[test]
    fn falls_back_when_the_recordings_folder_is_read_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("readonly");
        fs::create_dir(&folder).unwrap();
        let source = folder.join("interview.m4a");
        fs::write(&source, b"audio").unwrap();
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o555)).unwrap();
        // Root may write anyway; then there is nothing to test.
        let writable = fs::write(folder.join("probe"), b"").is_ok();
        let documents = dir.path().join("Documents");

        let result = save(
            &transcript(&source, TranscriptStatus::Completed),
            &settings(TranscriptSaveFormat::Txt),
            Some(&documents),
            None,
            &ExportOptions::default(),
        );
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o755)).unwrap();
        let saved = result.unwrap();
        if writable {
            return;
        }
        assert_eq!(
            saved.path,
            documents
                .join(FALLBACK_FOLDER_NAME)
                .join("interview.txt")
                .to_string_lossy()
        );
        assert_eq!(
            saved.unwritable_folder.as_deref(),
            Some(folder.to_string_lossy().as_ref())
        );
        assert_eq!(
            fs::read_dir(&folder).unwrap().count(),
            1,
            "only the recording"
        );
    }

    #[test]
    fn a_recording_in_the_temporary_folder_is_saved_in_documents() {
        let dir = tempfile::tempdir().unwrap();
        // Where Explorer puts a recording dragged out of a .zip file.
        let temp = dir.path().join("Temp");
        let extracted = temp.join("Temp1_recordings.zip");
        fs::create_dir_all(&extracted).unwrap();
        let source = extracted.join("interview.m4a");
        fs::write(&source, b"audio").unwrap();
        let documents = dir.path().join("Documents");
        let t = transcript(&source, TranscriptStatus::Completed);
        let txt = settings(TranscriptSaveFormat::Txt);
        let options = ExportOptions::default();

        let saved = save(&t, &txt, Some(&documents), Some(&temp), &options).unwrap();
        let fallback = documents.join(FALLBACK_FOLDER_NAME);
        assert_eq!(saved.path, fallback.join("interview.txt").to_string_lossy());
        assert_eq!(
            saved.unwritable_folder.as_deref(),
            Some(extracted.to_string_lossy().as_ref())
        );
        assert_eq!(
            fs::read_dir(&extracted).unwrap().count(),
            1,
            "only the recording"
        );

        // A folder the user chose is used, even in there.
        let mine = temp.join("mine");
        let chosen = TranscriptAutoSave {
            location: TranscriptSaveLocation::Folder,
            folder: mine.to_string_lossy().into_owned(),
            ..txt.clone()
        };
        let saved = save(&t, &chosen, Some(&documents), Some(&temp), &options).unwrap();
        assert_eq!(saved.path, mine.join("interview.txt").to_string_lossy());
        assert_eq!(saved.unwritable_folder, None);

        // Without a fallback, the temporary folder beats no file at all.
        let saved = save(&t, &txt, None, Some(&temp), &options).unwrap();
        assert_eq!(
            saved.path,
            extracted.join("interview.txt").to_string_lossy()
        );

        // A "temporary folder" that holds Documents, or a drive's root, is
        // a misconfiguration and is ignored.
        for not_temp in [dir.path(), Path::new("/")] {
            let saved = save(&t, &txt, Some(&documents), Some(not_temp), &options).unwrap();
            assert_eq!(saved.unwritable_folder, None, "{}", not_temp.display());
            assert!(Path::new(&saved.path).starts_with(&extracted));
        }
    }

    #[cfg(unix)]
    #[test]
    fn temporary_folders_are_found_however_they_are_spelled() {
        let dir = tempfile::tempdir().unwrap();
        let temp = dir.path().join("Temp");
        let inside = temp.join("Temp1_recordings.zip");
        fs::create_dir_all(&inside).unwrap();
        // Another spelling of the same folder, like a Windows short name.
        let alias = dir.path().join("TEMP~1");
        std::os::unix::fs::symlink(&temp, &alias).unwrap();

        assert!(is_in_temp_folder(&inside, &temp));
        assert!(is_in_temp_folder(&temp, &temp));
        assert!(is_in_temp_folder(&inside, &alias));
        assert!(is_in_temp_folder(
            &alias.join("Temp1_recordings.zip"),
            &temp
        ));
        assert!(!is_in_temp_folder(dir.path(), &temp));
        // Whole folder names only: "Temporary" is not inside "Temp".
        let similar = dir.path().join("Temporary");
        fs::create_dir(&similar).unwrap();
        assert!(!is_in_temp_folder(&similar, &temp));
        assert!(!is_in_temp_folder(&dir.path().join("gone"), &temp));
    }

    #[test]
    fn a_failing_fallback_folder_reports_both_folders() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("gone").join("interview.m4a");
        // "Documents" is a file, so the fallback folder can't be made.
        let documents = dir.path().join("Documents");
        fs::write(&documents, b"not a folder").unwrap();
        let err = save(
            &transcript(&source, TranscriptStatus::Completed),
            &settings(TranscriptSaveFormat::Txt),
            Some(&documents),
            None,
            &ExportOptions::default(),
        )
        .unwrap_err();
        assert!(err.contains("gone"), "{err}");
        assert!(err.contains(FALLBACK_FOLDER_NAME), "{err}");
    }
}
