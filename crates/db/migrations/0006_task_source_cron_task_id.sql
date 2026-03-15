-- Add source_cron_task_id to tasks for cron-triggered tasks
ALTER TABLE tasks ADD COLUMN source_cron_task_id BLOB;
