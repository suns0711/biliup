ALTER TABLE local_drafts ADD COLUMN submitted_at INTEGER;

UPDATE local_drafts SET submitted_at = updated_at WHERE status = 'submitted' AND submitted_at IS NULL;
