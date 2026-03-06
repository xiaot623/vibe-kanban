ALTER TABLE execution_process_logs
    ADD COLUMN msg_type TEXT;

CREATE INDEX IF NOT EXISTS idx_execution_process_logs_execution_id_msg_type
    ON execution_process_logs (execution_id, msg_type);
