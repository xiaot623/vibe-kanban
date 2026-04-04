import { useState } from 'react';
import { Button } from '@/components/ui/button';
import MarkdownEditor from '@/components/ui/markdown-editor';
import { useReview, type ReviewComment } from '@/contexts/ReviewProvider';
import { cn } from '@/lib/utils';

interface ReviewCommentRendererProps {
  comment: ReviewComment;
  projectId?: string;
}

export function ReviewCommentRenderer({
  comment,
  projectId,
}: ReviewCommentRendererProps) {
  const { deleteComment, updateComment, toggleCommentSolved } = useReview();
  const [isEditing, setIsEditing] = useState(false);
  const [editText, setEditText] = useState(comment.text);

  const handleDelete = () => {
    deleteComment(comment.id);
  };

  const handleEdit = () => {
    setEditText(comment.text);
    setIsEditing(true);
  };

  const handleSave = () => {
    if (editText.trim()) {
      updateComment(comment.id, editText.trim());
    }
    setIsEditing(false);
  };

  const handleCancel = () => {
    setEditText(comment.text);
    setIsEditing(false);
  };

  if (isEditing) {
    return (
      <div className="border-y bg-background p-3">
        <MarkdownEditor
          value={editText}
          onChange={setEditText}
          placeholder="Edit comment... (type @ to search files)"
          className="w-full bg-background text-foreground text-sm min-h-[48px]"
          projectId={projectId}
          onCmdEnter={handleSave}
          autoFocus
        />
        <div className="mt-2 flex gap-2">
          <Button size="xs" onClick={handleSave} disabled={!editText.trim()}>
            Save changes
          </Button>
          <Button
            size="xs"
            variant="ghost"
            onClick={handleCancel}
            className="text-secondary-foreground"
          >
            Cancel
          </Button>
        </div>
      </div>
    );
  }

  return (
    <div
      className={cn(
        'border-y px-3 py-2',
        comment.solved ? 'bg-muted/40' : 'bg-background'
      )}
    >
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="flex items-center gap-2 text-xs">
          <span
            className={cn(
              'rounded px-2 py-0.5 font-medium',
              comment.solved
                ? 'bg-green-500/10 text-green-700'
                : 'bg-amber-500/10 text-amber-700'
            )}
          >
            {comment.solved ? 'Solved' : 'Unsolved'}
          </span>
          <span className="text-muted-foreground">
            Line {comment.lineNumber}
          </span>
        </div>
        <div className="flex items-center gap-2">
          <Button
            size="xs"
            variant="outline"
            onClick={() => toggleCommentSolved(comment.id)}
          >
            {comment.solved ? 'Unsolve' : 'Solve'}
          </Button>
          <Button size="xs" variant="outline" onClick={handleEdit}>
            Edit
          </Button>
          <Button size="xs" variant="destructive" onClick={handleDelete}>
            Delete
          </Button>
        </div>
      </div>

      <div className={cn('mt-2', comment.solved && 'opacity-70')}>
        <MarkdownEditor
          value={comment.text}
          disabled={true}
          className="text-sm"
        />
      </div>
    </div>
  );
}
