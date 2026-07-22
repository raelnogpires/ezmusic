use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use walkdir::WalkDir;

use crate::model::{Album, AlbumDraft, Track, TrackDraft};

const MIGRATION: &str = r#"
CREATE TABLE IF NOT EXISTS tracks (
    id INTEGER PRIMARY KEY,
    provider TEXT,
    source_id TEXT,
    title TEXT NOT NULL,
    artist TEXT NOT NULL,
    album TEXT,
    path TEXT NOT NULL UNIQUE,
    duration_seconds INTEGER,
    available INTEGER NOT NULL DEFAULT 1,
    imported INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    UNIQUE(provider, source_id)
);
CREATE INDEX IF NOT EXISTS tracks_title_idx ON tracks(title COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS tracks_artist_idx ON tracks(artist COLLATE NOCASE);

CREATE TABLE IF NOT EXISTS albums (
    id INTEGER PRIMARY KEY,
    provider TEXT NOT NULL,
    source_id TEXT NOT NULL,
    title TEXT NOT NULL,
    artist TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE(provider, source_id)
);
CREATE INDEX IF NOT EXISTS albums_title_idx ON albums(title COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS albums_artist_idx ON albums(artist COLLATE NOCASE);
CREATE TABLE IF NOT EXISTS album_tracks (
    album_id INTEGER NOT NULL REFERENCES albums(id) ON DELETE CASCADE,
    track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    PRIMARY KEY(album_id, position),
    UNIQUE(album_id, track_id)
);

CREATE TABLE IF NOT EXISTS imported_roots (
    path TEXT PRIMARY KEY,
    last_scan INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS playlists (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS playlist_tracks (
    playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    PRIMARY KEY(playlist_id, position)
);

CREATE TABLE IF NOT EXISTS queue (
    position INTEGER PRIMARY KEY,
    track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS download_jobs (
    job_id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    state TEXT NOT NULL,
    error TEXT,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

const MAX_IMPORT_FILES: usize = 100_000;
const MAX_QUERY_TRACKS: usize = 5_000;

pub struct LibraryDb {
    connection: Connection,
}

impl LibraryDb {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("falha ao criar {}", parent.display()))?;
        }
        let connection =
            Connection::open(path).with_context(|| format!("falha ao abrir {}", path.display()))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection
            .execute_batch(MIGRATION)
            .context("falha nas migracoes")?;
        Ok(Self { connection })
    }

    pub fn in_memory() -> Result<Self> {
        let connection = Connection::open_in_memory()?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.execute_batch(MIGRATION)?;
        Ok(Self { connection })
    }

    pub fn upsert_track(&self, track: &TrackDraft) -> Result<i64> {
        upsert_track(&self.connection, track)
    }

    pub fn upsert_album(&self, album: &AlbumDraft, tracks: &[TrackDraft]) -> Result<i64> {
        if tracks.is_empty() || tracks.len() > 500 {
            bail!("album deve conter entre 1 e 500 faixas");
        }
        let transaction = self.connection.unchecked_transaction()?;
        transaction.execute(
            "INSERT INTO albums(provider, source_id, title, artist, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(provider, source_id) DO UPDATE SET
               title=excluded.title, artist=excluded.artist",
            params![
                album.provider,
                album.source_id,
                album.title,
                album.artist,
                unix_time()
            ],
        )?;
        let album_id = transaction.query_row(
            "SELECT id FROM albums WHERE provider=?1 AND source_id=?2",
            params![album.provider, album.source_id],
            |row| row.get(0),
        )?;
        transaction.execute("DELETE FROM album_tracks WHERE album_id=?1", [album_id])?;
        for (position, track) in tracks.iter().enumerate() {
            let track_id = upsert_track(&transaction, track)?;
            transaction.execute(
                "INSERT INTO album_tracks(album_id, track_id, position) VALUES (?1, ?2, ?3)",
                params![album_id, track_id, position as i64],
            )?;
        }
        transaction.commit()?;
        Ok(album_id)
    }

    pub fn albums(&self, filter: Option<&str>, limit: usize) -> Result<Vec<Album>> {
        let limit = limit.min(MAX_QUERY_TRACKS) as i64;
        let mut output = Vec::new();
        if let Some(filter) = filter.filter(|value| !value.trim().is_empty()) {
            let pattern = format!("%{}%", filter.trim());
            let mut statement = self.connection.prepare(
                "SELECT a.id, a.provider, a.source_id, a.title, a.artist, COUNT(at.track_id),
                        COALESCE(SUM(album_track.available), 0)
                 FROM albums a LEFT JOIN album_tracks at ON at.album_id=a.id
                 LEFT JOIN tracks album_track ON album_track.id=at.track_id
                 WHERE a.title LIKE ?1 OR a.artist LIKE ?1 OR EXISTS (
                   SELECT 1 FROM album_tracks matched
                   JOIN tracks t ON t.id=matched.track_id
                   WHERE matched.album_id=a.id AND (t.title LIKE ?1 OR t.artist LIKE ?1)
                 )
                 GROUP BY a.id ORDER BY a.artist COLLATE NOCASE, a.title COLLATE NOCASE LIMIT ?2",
            )?;
            let rows = statement.query_map(params![pattern, limit], row_album)?;
            for row in rows {
                output.push(row?);
            }
        } else {
            let mut statement = self.connection.prepare(
                "SELECT a.id, a.provider, a.source_id, a.title, a.artist, COUNT(at.track_id),
                        COALESCE(SUM(album_track.available), 0)
                 FROM albums a LEFT JOIN album_tracks at ON at.album_id=a.id
                 LEFT JOIN tracks album_track ON album_track.id=at.track_id
                 GROUP BY a.id ORDER BY a.artist COLLATE NOCASE, a.title COLLATE NOCASE LIMIT ?1",
            )?;
            let rows = statement.query_map([limit], row_album)?;
            for row in rows {
                output.push(row?);
            }
        }
        Ok(output)
    }

    pub fn album_tracks(&self, album_id: i64) -> Result<Vec<Track>> {
        let mut statement = self.connection.prepare(
            "SELECT t.id, t.provider, t.source_id, t.title, t.artist, t.album, t.path,
                    t.duration_seconds, t.available, t.imported
             FROM album_tracks at JOIN tracks t ON t.id=at.track_id
             WHERE at.album_id=?1 ORDER BY at.position",
        )?;
        let rows = statement.query_map([album_id], row_track)?;
        rows.map(|row| row.map_err(Into::into)).collect()
    }

    pub fn standalone_tracks(
        &self,
        filter: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<Track>> {
        self.query_tracks(filter, limit, offset, true)
    }

    pub fn tracks(&self, filter: Option<&str>, limit: usize, offset: usize) -> Result<Vec<Track>> {
        self.query_tracks(filter, limit, offset, false)
    }

    fn query_tracks(
        &self,
        filter: Option<&str>,
        limit: usize,
        offset: usize,
        standalone_only: bool,
    ) -> Result<Vec<Track>> {
        let limit = limit.min(MAX_QUERY_TRACKS);
        let offset = offset.min(i64::MAX as usize);
        let standalone = if standalone_only {
            " AND NOT EXISTS (SELECT 1 FROM album_tracks at WHERE at.track_id=tracks.id)"
        } else {
            ""
        };
        let mut output = Vec::new();
        if let Some(filter) = filter.filter(|value| !value.trim().is_empty()) {
            let pattern = format!("%{}%", filter.trim());
            let sql = format!(
                "SELECT id, provider, source_id, title, artist, album, path, duration_seconds, available, imported
                 FROM tracks WHERE (title LIKE ?1 OR artist LIKE ?1 OR album LIKE ?1){standalone}
                 ORDER BY artist COLLATE NOCASE, title COLLATE NOCASE LIMIT ?2 OFFSET ?3"
            );
            let mut statement = self.connection.prepare(&sql)?;
            let rows =
                statement.query_map(params![pattern, limit as i64, offset as i64], row_track)?;
            for row in rows {
                output.push(row?);
            }
        } else {
            let sql = format!(
                "SELECT id, provider, source_id, title, artist, album, path, duration_seconds, available, imported
                 FROM tracks WHERE 1=1{standalone}
                 ORDER BY artist COLLATE NOCASE, title COLLATE NOCASE LIMIT ?1 OFFSET ?2"
            );
            let mut statement = self.connection.prepare(&sql)?;
            let rows = statement.query_map(params![limit as i64, offset as i64], row_track)?;
            for row in rows {
                output.push(row?);
            }
        }
        Ok(output)
    }

    pub fn track(&self, id: i64) -> Result<Option<Track>> {
        self.connection
            .query_row(
                "SELECT id, provider, source_id, title, artist, album, path, duration_seconds, available, imported
                 FROM tracks WHERE id=?1",
                [id],
                row_track,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn count_tracks(&self) -> Result<u64> {
        Ok(self
            .connection
            .query_row("SELECT COUNT(*) FROM tracks", [], |row| row.get(0))?)
    }

    pub fn add_import_root(&self, path: &Path) -> Result<()> {
        self.connection.execute(
            "INSERT OR IGNORE INTO imported_roots(path, last_scan) VALUES (?1, 0)",
            [path.to_string_lossy().as_ref()],
        )?;
        Ok(())
    }

    pub fn import_roots(&self) -> Result<Vec<PathBuf>> {
        let mut statement = self
            .connection
            .prepare("SELECT path FROM imported_roots ORDER BY path")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|row| row.map(PathBuf::from).map_err(Into::into))
            .collect()
    }

    pub fn scan_import_root(&self, root: &Path) -> Result<usize> {
        let canonical_root = root
            .canonicalize()
            .with_context(|| format!("pasta inexistente: {}", root.display()))?;
        let mut found = HashSet::new();
        for entry in WalkDir::new(&canonical_root).follow_links(false) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            if !entry.file_type().is_file() || !is_supported_audio(entry.path()) {
                continue;
            }
            let path = entry
                .path()
                .canonicalize()
                .unwrap_or_else(|_| entry.path().to_path_buf());
            found.insert(path);
            if found.len() > MAX_IMPORT_FILES {
                bail!("a importacao excedeu o limite de {MAX_IMPORT_FILES} arquivos de audio");
            }
        }

        let transaction = self.connection.unchecked_transaction()?;
        self.add_import_root(&canonical_root)?;
        for path in &found {
            let title = path
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("Faixa sem titulo")
                .to_string();
            self.upsert_track(&TrackDraft {
                provider: None,
                source_id: None,
                title,
                artist: "Desconhecido".into(),
                album: None,
                path: path.clone(),
                duration_seconds: None,
                imported: true,
            })?;
        }

        let mut statement = self
            .connection
            .prepare("SELECT id, path FROM tracks WHERE imported=1")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, path) = row?;
            let path = PathBuf::from(path);
            if path.starts_with(&canonical_root) {
                self.connection.execute(
                    "UPDATE tracks SET available=?1 WHERE id=?2",
                    params![found.contains(&path), id],
                )?;
            }
        }
        self.connection.execute(
            "UPDATE imported_roots SET last_scan=?1 WHERE path=?2",
            params![unix_time(), canonical_root.to_string_lossy()],
        )?;
        drop(statement);
        transaction.commit()?;
        Ok(found.len())
    }

    pub fn create_playlist(&self, name: &str) -> Result<i64> {
        let name = name.trim();
        if name.is_empty() {
            bail!("nome da playlist nao pode ser vazio");
        }
        if name.chars().count() > 128 {
            bail!("nome da playlist excede 128 caracteres");
        }
        self.connection.execute(
            "INSERT INTO playlists(name, created_at) VALUES (?1, ?2)
             ON CONFLICT(name) DO NOTHING",
            params![name, unix_time()],
        )?;
        Ok(self
            .connection
            .query_row("SELECT id FROM playlists WHERE name=?1", [name], |row| {
                row.get(0)
            })?)
    }

    pub fn playlists(&self) -> Result<Vec<(i64, String, u64)>> {
        let mut statement = self.connection.prepare(
            "SELECT p.id, p.name, COUNT(pt.track_id) FROM playlists p
             LEFT JOIN playlist_tracks pt ON pt.playlist_id=p.id
             GROUP BY p.id ORDER BY p.name COLLATE NOCASE",
        )?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        rows.map(|row| row.map_err(Into::into)).collect()
    }

    pub fn add_to_playlist(&self, playlist_id: i64, track_id: i64) -> Result<()> {
        let position: i64 = self.connection.query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM playlist_tracks WHERE playlist_id=?1",
            [playlist_id],
            |row| row.get(0),
        )?;
        self.connection.execute(
            "INSERT INTO playlist_tracks(playlist_id, track_id, position) VALUES (?1, ?2, ?3)",
            params![playlist_id, track_id, position],
        )?;
        Ok(())
    }

    pub fn playlist_tracks(&self, playlist_id: i64) -> Result<Vec<Track>> {
        let mut statement = self.connection.prepare(
            "SELECT t.id, t.provider, t.source_id, t.title, t.artist, t.album, t.path,
                    t.duration_seconds, t.available, t.imported
             FROM playlist_tracks pt JOIN tracks t ON t.id=pt.track_id
             WHERE pt.playlist_id=?1 ORDER BY pt.position",
        )?;
        let rows = statement.query_map([playlist_id], row_track)?;
        rows.map(|row| row.map_err(Into::into)).collect()
    }

    pub fn remove_from_playlist(&self, playlist_id: i64, position: usize) -> Result<bool> {
        let position = i64::try_from(position).context("posicao de playlist invalida")?;
        let transaction = self.connection.unchecked_transaction()?;
        let removed = transaction.execute(
            "DELETE FROM playlist_tracks WHERE playlist_id=?1 AND position=?2",
            params![playlist_id, position],
        )?;
        if removed > 0 {
            transaction.execute(
                "UPDATE playlist_tracks SET position=-position-1
                 WHERE playlist_id=?1 AND position>?2",
                params![playlist_id, position],
            )?;
            transaction.execute(
                "UPDATE playlist_tracks SET position=-position-2
                 WHERE playlist_id=?1 AND position<0",
                [playlist_id],
            )?;
        }
        transaction.commit()?;
        Ok(removed > 0)
    }

    pub fn save_queue(&mut self, tracks: &[Track]) -> Result<()> {
        let transaction = self.connection.transaction()?;
        transaction.execute("DELETE FROM queue", [])?;
        for (position, track) in tracks.iter().enumerate() {
            transaction.execute(
                "INSERT INTO queue(position, track_id) VALUES (?1, ?2)",
                params![position as i64, track.id],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn load_queue(&self) -> Result<Vec<Track>> {
        let mut statement = self.connection.prepare(
            "SELECT t.id, t.provider, t.source_id, t.title, t.artist, t.album, t.path,
                    t.duration_seconds, t.available, t.imported
             FROM queue q JOIN tracks t ON t.id=q.track_id ORDER BY q.position",
        )?;
        let rows = statement.query_map([], row_track)?;
        rows.map(|row| row.map_err(Into::into)).collect()
    }

    pub fn record_job(
        &self,
        job_id: &str,
        title: &str,
        state: &str,
        error: Option<&str>,
    ) -> Result<()> {
        self.connection.execute(
            "INSERT INTO download_jobs(job_id, title, state, error, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(job_id) DO UPDATE SET state=excluded.state, error=excluded.error,
                 updated_at=excluded.updated_at",
            params![job_id, title, state, error, unix_time()],
        )?;
        Ok(())
    }
}

fn upsert_track(connection: &Connection, track: &TrackDraft) -> Result<i64> {
    let path = track.path.to_string_lossy();
    connection.execute(
        r#"INSERT INTO tracks
           (provider, source_id, title, artist, album, path, duration_seconds, available, imported, created_at)
           VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8, ?9)
           ON CONFLICT(path) DO UPDATE SET
             provider=excluded.provider, source_id=excluded.source_id,
             title=excluded.title, artist=excluded.artist, album=excluded.album,
             duration_seconds=excluded.duration_seconds, available=1, imported=excluded.imported
           ON CONFLICT(provider, source_id) DO UPDATE SET
             title=excluded.title, artist=excluded.artist, album=excluded.album,
             path=excluded.path, duration_seconds=excluded.duration_seconds,
             available=1, imported=excluded.imported"#,
        params![
            track.provider,
            track.source_id,
            track.title,
            track.artist,
            track.album,
            path,
            track.duration_seconds,
            track.imported,
            unix_time(),
        ],
    )?;
    if let (Some(provider), Some(source_id)) = (&track.provider, &track.source_id) {
        Ok(connection.query_row(
            "SELECT id FROM tracks WHERE provider=?1 AND source_id=?2",
            params![provider, source_id],
            |row| row.get(0),
        )?)
    } else {
        Ok(connection.query_row(
            "SELECT id FROM tracks WHERE path=?1",
            [path.as_ref()],
            |row| row.get(0),
        )?)
    }
}

fn row_album(row: &rusqlite::Row<'_>) -> rusqlite::Result<Album> {
    Ok(Album {
        id: row.get(0)?,
        provider: row.get(1)?,
        source_id: row.get(2)?,
        title: row.get(3)?,
        artist: row.get(4)?,
        track_count: row.get(5)?,
        available_count: row.get(6)?,
    })
}

fn row_track(row: &rusqlite::Row<'_>) -> rusqlite::Result<Track> {
    Ok(Track {
        id: row.get(0)?,
        provider: row.get(1)?,
        source_id: row.get(2)?,
        title: row.get(3)?,
        artist: row.get(4)?,
        album: row.get(5)?,
        path: PathBuf::from(row.get::<_, String>(6)?),
        duration_seconds: row.get(7)?,
        available: row.get(8)?,
        imported: row.get(9)?,
    })
}

fn is_supported_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "opus" | "ogg" | "oga" | "mp3" | "flac" | "aac" | "m4a" | "wav" | "webm"
            )
        })
        .unwrap_or(false)
}

fn unix_time() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn upserts_and_filters_tracks() {
        let db = LibraryDb::in_memory().unwrap();
        let draft = TrackDraft {
            provider: Some("youtube".into()),
            source_id: Some("abc".into()),
            title: "Oceano".into(),
            artist: "Djavan".into(),
            album: None,
            path: PathBuf::from("/tmp/abc.opus"),
            duration_seconds: Some(240),
            imported: false,
        };
        let first = db.upsert_track(&draft).unwrap();
        let second = db.upsert_track(&draft).unwrap();
        assert_eq!(first, second);
        assert_eq!(db.tracks(Some("dja"), 10, 0).unwrap().len(), 1);
    }

    #[test]
    fn imports_only_supported_files_and_marks_missing() {
        let directory = tempdir().unwrap();
        std::fs::write(directory.path().join("song.opus"), b"not-a-real-opus").unwrap();
        std::fs::write(directory.path().join("notes.txt"), b"ignore").unwrap();
        let db = LibraryDb::in_memory().unwrap();
        assert_eq!(db.scan_import_root(directory.path()).unwrap(), 1);
        assert_eq!(db.count_tracks().unwrap(), 1);
        std::fs::remove_file(directory.path().join("song.opus")).unwrap();
        db.scan_import_root(directory.path()).unwrap();
        assert!(!db.tracks(None, 10, 0).unwrap()[0].available);
    }

    #[test]
    fn persists_playlist_order() {
        let db = LibraryDb::in_memory().unwrap();
        let id = db
            .upsert_track(&TrackDraft {
                provider: None,
                source_id: None,
                title: "One".into(),
                artist: "Artist".into(),
                album: None,
                path: PathBuf::from("/tmp/one.flac"),
                duration_seconds: None,
                imported: true,
            })
            .unwrap();
        let playlist = db.create_playlist("Foco").unwrap();
        db.add_to_playlist(playlist, id).unwrap();
        assert_eq!(db.playlist_tracks(playlist).unwrap()[0].title, "One");
    }

    #[test]
    fn removes_playlist_tracks_and_compacts_positions() {
        let db = LibraryDb::in_memory().unwrap();
        let mut ids = Vec::new();
        for index in 0..3 {
            ids.push(
                db.upsert_track(&TrackDraft {
                    provider: None,
                    source_id: None,
                    title: format!("Track {index}"),
                    artist: "Artist".into(),
                    album: None,
                    path: PathBuf::from(format!("/tmp/playlist-{index}.opus")),
                    duration_seconds: None,
                    imported: true,
                })
                .unwrap(),
            );
        }
        let playlist = db.create_playlist("Order").unwrap();
        for id in ids {
            db.add_to_playlist(playlist, id).unwrap();
        }

        assert!(db.remove_from_playlist(playlist, 1).unwrap());
        assert_eq!(
            db.playlist_tracks(playlist)
                .unwrap()
                .into_iter()
                .map(|track| track.title)
                .collect::<Vec<_>>(),
            ["Track 0", "Track 2"]
        );
        let positions: Vec<i64> = db
            .connection
            .prepare("SELECT position FROM playlist_tracks ORDER BY position")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(positions, [0, 1]);
        assert!(db.remove_from_playlist(playlist, 1).unwrap());
        assert!(db.remove_from_playlist(playlist, 0).unwrap());
        assert!(db.playlist_tracks(playlist).unwrap().is_empty());
        assert_eq!(db.playlists().unwrap()[0].2, 0);
    }

    #[test]
    fn rescanning_one_root_does_not_hide_similar_sibling() {
        let directory = tempdir().unwrap();
        let music = directory.path().join("music");
        let music_other = directory.path().join("music-other");
        std::fs::create_dir_all(&music).unwrap();
        std::fs::create_dir_all(&music_other).unwrap();
        std::fs::write(music.join("one.opus"), b"one").unwrap();
        std::fs::write(music_other.join("two.opus"), b"two").unwrap();
        let db = LibraryDb::in_memory().unwrap();
        db.scan_import_root(&music).unwrap();
        db.scan_import_root(&music_other).unwrap();
        std::fs::remove_file(music.join("one.opus")).unwrap();
        db.scan_import_root(&music).unwrap();
        let tracks = db.tracks(None, 10, 0).unwrap();
        assert!(
            !tracks
                .iter()
                .find(|track| track.title == "one")
                .unwrap()
                .available
        );
        assert!(
            tracks
                .iter()
                .find(|track| track.title == "two")
                .unwrap()
                .available
        );
    }

    #[test]
    fn rejects_empty_playlist_names() {
        let db = LibraryDb::in_memory().unwrap();
        assert!(db.create_playlist("   ").is_err());
    }

    #[test]
    fn migrates_existing_database_without_losing_tracks() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("library.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE tracks (
                    id INTEGER PRIMARY KEY, provider TEXT, source_id TEXT, title TEXT NOT NULL,
                    artist TEXT NOT NULL, album TEXT, path TEXT NOT NULL UNIQUE,
                    duration_seconds INTEGER, available INTEGER NOT NULL DEFAULT 1,
                    imported INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL,
                    UNIQUE(provider, source_id));
                 INSERT INTO tracks(title, artist, path, created_at)
                 VALUES ('Legacy', 'Artist', '/tmp/legacy.opus', 1);",
            )
            .unwrap();
        drop(connection);

        let db = LibraryDb::open(path).unwrap();
        assert_eq!(db.tracks(None, 10, 0).unwrap()[0].title, "Legacy");
        assert!(db.albums(None, 10).unwrap().is_empty());
    }

    #[test]
    fn upserts_album_tracks_transactionally_in_source_order() {
        let db = LibraryDb::in_memory().unwrap();
        let album = AlbumDraft {
            provider: "youtube".into(),
            source_id: "video".into(),
            title: "Record".into(),
            artist: "Artist".into(),
        };
        let drafts: Vec<_> = ["One", "Two"]
            .into_iter()
            .enumerate()
            .map(|(index, title)| TrackDraft {
                provider: Some("youtube".into()),
                source_id: Some(format!("video-chapter-{index}")),
                title: title.into(),
                artist: "Artist".into(),
                album: Some("Record".into()),
                path: PathBuf::from(format!("/tmp/chapter-{index}.opus")),
                duration_seconds: Some(60),
                imported: false,
            })
            .collect();
        let first = db.upsert_album(&album, &drafts).unwrap();
        let second = db.upsert_album(&album, &drafts).unwrap();
        assert_eq!(first, second);
        assert_eq!(db.albums(None, 10).unwrap()[0].track_count, 2);
        assert_eq!(
            db.album_tracks(first)
                .unwrap()
                .into_iter()
                .map(|track| track.title)
                .collect::<Vec<_>>(),
            ["One", "Two"]
        );
        assert!(db.standalone_tracks(None, 10, 0).unwrap().is_empty());

        db.upsert_track(&TrackDraft {
            provider: None,
            source_id: None,
            title: "Standalone".into(),
            artist: "Artist".into(),
            album: None,
            path: PathBuf::from("/tmp/standalone.opus"),
            duration_seconds: None,
            imported: true,
        })
        .unwrap();
        assert_eq!(db.standalone_tracks(None, 10, 0).unwrap().len(), 1);
    }
}
