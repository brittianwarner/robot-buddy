-- One-time rebuild of history search with stable row IDs. Runs in one transaction with
-- its schema_migrations marker. The search index is derived from turns, so rebuilding it
-- loses no history.
DELETE FROM history_search;
DELETE FROM history_rows;
INSERT INTO history_rows(turn_id) SELECT id FROM turns ORDER BY id;
INSERT INTO history_search(rowid,turn_id,user_text,assistant_text)
  SELECT r.fts_rowid,t.id,t.text,t.answer FROM history_rows r JOIN turns t ON t.id=r.turn_id;
