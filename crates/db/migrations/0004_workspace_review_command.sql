ALTER TABLE workspaces
ADD COLUMN review_command_markdown_text TEXT;

ALTER TABLE workspaces
ADD COLUMN review_command_solved INTEGER;

ALTER TABLE workspaces
ADD COLUMN review_command_reason TEXT;

ALTER TABLE workspaces
ADD COLUMN review_command_created_at TEXT;

ALTER TABLE workspaces
ADD COLUMN review_command_updated_at TEXT;
