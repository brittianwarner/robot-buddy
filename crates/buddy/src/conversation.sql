CREATE TABLE IF NOT EXISTS turns(id TEXT PRIMARY KEY,request_id TEXT NOT NULL UNIQUE,text TEXT NOT NULL,status TEXT NOT NULL DEFAULT 'queued',answer TEXT NOT NULL DEFAULT '',created INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS turns_status ON turns(status,created);
-- Prompt context reads the latest completed turns by ID.
CREATE INDEX IF NOT EXISTS turns_completed ON turns(status,id DESC);
CREATE TABLE IF NOT EXISTS events(sequence INTEGER PRIMARY KEY AUTOINCREMENT,turn_id TEXT NOT NULL,kind TEXT NOT NULL,payload TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS tool_calls(turn_id TEXT NOT NULL,call_id TEXT NOT NULL,name TEXT NOT NULL,args TEXT NOT NULL,result TEXT NOT NULL DEFAULT '',PRIMARY KEY(turn_id,call_id));
CREATE VIRTUAL TABLE IF NOT EXISTS history_search USING fts5(turn_id UNINDEXED,user_text,assistant_text);
-- Each turn's FTS row ID lives in an indexed table, so replacing a turn's search row is a
-- keyed write. INTEGER PRIMARY KEY keeps the IDs stable across VACUUM.
CREATE TABLE IF NOT EXISTS history_rows(fts_rowid INTEGER PRIMARY KEY,turn_id TEXT NOT NULL UNIQUE);
-- A turn's search row is written at its mapped row ID, replacing any row already there,
-- so the index keeps one row per turn. Version 1 code that runs again recreates its own
-- triggers (turns_search_insert, turns_search_update); these stand down while those exist.
-- The rebuild in conversation/history.rs drops the older triggers in its start transaction
-- and maps rows that predate history_rows.
DROP TRIGGER IF EXISTS turns_history_insert;
DROP TRIGGER IF EXISTS turns_history_update;
CREATE TRIGGER IF NOT EXISTS turns_fts_insert AFTER INSERT ON turns
WHEN NOT EXISTS(SELECT 1 FROM sqlite_master WHERE type='trigger' AND name='turns_search_insert')
BEGIN
  INSERT OR IGNORE INTO history_rows(turn_id) VALUES(new.id);
  INSERT OR REPLACE INTO history_search(rowid,turn_id,user_text,assistant_text)
    SELECT fts_rowid,new.id,new.text,new.answer FROM history_rows WHERE turn_id=new.id;
END;
CREATE TRIGGER IF NOT EXISTS turns_fts_update AFTER UPDATE OF answer ON turns
WHEN NOT EXISTS(SELECT 1 FROM sqlite_master WHERE type='trigger' AND name='turns_search_update')
  AND EXISTS(SELECT 1 FROM history_rows r JOIN history_search h ON h.rowid=r.fts_rowid
    WHERE r.turn_id=new.id AND h.turn_id=new.id)
BEGIN
  INSERT OR REPLACE INTO history_search(rowid,turn_id,user_text,assistant_text)
    SELECT fts_rowid,new.id,new.text,new.answer FROM history_rows WHERE turn_id=new.id;
END;
-- A turn whose row is not mapped yet (one in flight when the rebuild started) adopts its
-- newest row, a short read from the top of the index, or a new row ID if it has none.
CREATE TRIGGER IF NOT EXISTS turns_fts_adopt AFTER UPDATE OF answer ON turns
WHEN NOT EXISTS(SELECT 1 FROM sqlite_master WHERE type='trigger' AND name='turns_search_update')
  AND NOT EXISTS(SELECT 1 FROM history_rows r JOIN history_search h ON h.rowid=r.fts_rowid
    WHERE r.turn_id=new.id AND h.turn_id=new.id)
BEGIN
  INSERT OR REPLACE INTO history_rows(fts_rowid,turn_id) VALUES((SELECT rowid FROM history_search
    WHERE turn_id=new.id ORDER BY rowid DESC LIMIT 1),new.id);
  INSERT OR REPLACE INTO history_search(rowid,turn_id,user_text,assistant_text)
    SELECT fts_rowid,new.id,new.text,new.answer FROM history_rows WHERE turn_id=new.id;
END;
-- Progress of the batched history search rebuild; one row while it runs.
CREATE TABLE IF NOT EXISTS history_backfill(singleton INTEGER PRIMARY KEY CHECK(singleton=1),floor INTEGER NOT NULL,cursor TEXT NOT NULL,sweeping INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS schema_migrations(name TEXT PRIMARY KEY,applied INTEGER NOT NULL);

CREATE TABLE IF NOT EXISTS runs(turn_id TEXT PRIMARY KEY,version TEXT NOT NULL,state TEXT NOT NULL,preamble TEXT NOT NULL,phase TEXT NOT NULL);

CREATE INDEX IF NOT EXISTS events_turn_kind ON events(turn_id,kind);

CREATE TABLE IF NOT EXISTS system_prompt (
  singleton INTEGER PRIMARY KEY CHECK(singleton=1),
  revision INTEGER NOT NULL,
  prompt TEXT NOT NULL,
  updated INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS prompt_versions (
  revision INTEGER PRIMARY KEY,
  prompt TEXT NOT NULL,
  reason TEXT NOT NULL,
  updated INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS voice_settings (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1),revision INTEGER NOT NULL,config TEXT NOT NULL,updated INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS voice_versions (
 revision INTEGER PRIMARY KEY,config TEXT NOT NULL,reason TEXT NOT NULL,updated INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS context_settings(singleton INTEGER PRIMARY KEY CHECK(singleton=1),revision INTEGER NOT NULL,config TEXT NOT NULL,updated INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS context_versions(revision INTEGER PRIMARY KEY,config TEXT NOT NULL,reason TEXT NOT NULL,updated INTEGER NOT NULL);

CREATE TABLE IF NOT EXISTS personas(name TEXT COLLATE NOCASE PRIMARY KEY,latest INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS persona_versions(name TEXT COLLATE NOCASE NOT NULL,version INTEGER NOT NULL,prompt TEXT NOT NULL,voice TEXT NOT NULL,reason TEXT NOT NULL,updated INTEGER NOT NULL,PRIMARY KEY(name,version));

CREATE TABLE IF NOT EXISTS speech_turns(turn_id TEXT PRIMARY KEY,voice TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS speech_chunks(turn_id TEXT NOT NULL,chunk_index INTEGER NOT NULL,text TEXT NOT NULL,PRIMARY KEY(turn_id,chunk_index));
