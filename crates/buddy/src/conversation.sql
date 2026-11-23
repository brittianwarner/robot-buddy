CREATE TABLE IF NOT EXISTS turns(id TEXT PRIMARY KEY,request_id TEXT NOT NULL UNIQUE,text TEXT NOT NULL,status TEXT NOT NULL DEFAULT 'queued',answer TEXT NOT NULL DEFAULT '',created INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS turns_status ON turns(status,created);
-- Prompt context reads the latest completed turns by ID.
CREATE INDEX IF NOT EXISTS turns_completed ON turns(status,id DESC);
CREATE TABLE IF NOT EXISTS events(sequence INTEGER PRIMARY KEY AUTOINCREMENT,turn_id TEXT NOT NULL,kind TEXT NOT NULL,payload TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS tool_calls(turn_id TEXT NOT NULL,call_id TEXT NOT NULL,name TEXT NOT NULL,args TEXT NOT NULL,result TEXT NOT NULL DEFAULT '',PRIMARY KEY(turn_id,call_id));
CREATE VIRTUAL TABLE IF NOT EXISTS history_search USING fts5(turn_id UNINDEXED,user_text,assistant_text);
-- Each turn's FTS row ID lives in an indexed table, so replacing a turn's search row is a
-- keyed delete. INTEGER PRIMARY KEY keeps the IDs stable across VACUUM.
CREATE TABLE IF NOT EXISTS history_rows(fts_rowid INTEGER PRIMARY KEY,turn_id TEXT NOT NULL UNIQUE);
-- Version 1 deleted by the unindexed turn_id column, a full FTS scan per finished turn.
-- The one-time rebuild in conversation/history-rowids.sql replaces its rows.
DROP TRIGGER IF EXISTS turns_search_insert;
DROP TRIGGER IF EXISTS turns_search_update;
CREATE TRIGGER IF NOT EXISTS turns_history_insert AFTER INSERT ON turns BEGIN
  INSERT INTO history_rows(turn_id) VALUES(new.id);
  INSERT INTO history_search(rowid,turn_id,user_text,assistant_text)
    VALUES((SELECT fts_rowid FROM history_rows WHERE turn_id=new.id),new.id,new.text,new.answer);
END;
CREATE TRIGGER IF NOT EXISTS turns_history_update AFTER UPDATE OF answer ON turns BEGIN
  DELETE FROM history_search WHERE rowid=(SELECT fts_rowid FROM history_rows WHERE turn_id=old.id);
  INSERT INTO history_search(rowid,turn_id,user_text,assistant_text)
    VALUES((SELECT fts_rowid FROM history_rows WHERE turn_id=new.id),new.id,new.text,new.answer);
END;
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
