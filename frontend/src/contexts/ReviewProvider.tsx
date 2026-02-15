import { SplitSide } from '@git-diff-view/react';
import {
  createContext,
  useContext,
  useState,
  ReactNode,
  useEffect,
  useCallback,
} from 'react';
import { genId } from '@/utils/id';
import { attemptsApi } from '@/lib/api';

export interface ReviewComment {
  id: string;
  filePath: string;
  lineNumber: number;
  side: SplitSide;
  text: string;
  solved: boolean;
  codeLine?: string;
}

export interface ReviewDraft {
  filePath: string;
  side: SplitSide;
  lineNumber: number;
  text: string;
  codeLine?: string;
}

interface ParsedPersistedComment {
  filePath: string;
  lineNumber: number;
  side: SplitSide;
  text: string;
  codeLine?: string;
}

interface ReviewContextType {
  comments: ReviewComment[];
  drafts: Record<string, ReviewDraft>;
  addComment: (comment: Omit<ReviewComment, 'id' | 'solved'>) => void;
  updateComment: (id: string, text: string) => void;
  toggleCommentSolved: (id: string) => void;
  deleteComment: (id: string) => void;
  clearComments: () => void;
  setDraft: (key: string, draft: ReviewDraft | null) => void;
  generateReviewMarkdown: () => string;
}

const ReviewContext = createContext<ReviewContextType | null>(null);

export function useReview() {
  const context = useContext(ReviewContext);
  if (!context) {
    throw new Error('useReview must be used within a ReviewProvider');
  }
  return context;
}

/**
 * Optional version of useReview that returns null if not inside a ReviewProvider.
 * Useful for components that may or may not be inside a review context.
 */
export function useReviewOptional() {
  return useContext(ReviewContext);
}

export function ReviewProvider({
  children,
  attemptId,
}: {
  children: ReactNode;
  attemptId?: string;
}) {
  const [comments, setComments] = useState<ReviewComment[]>([]);
  const [drafts, setDrafts] = useState<Record<string, ReviewDraft>>({});

  useEffect(() => {
    return () => clearComments();
  }, [attemptId]);

  const parsePersistedReviewMarkdown = useCallback(
    (markdown: string): ParsedPersistedComment[] => {
      const normalized = markdown.replace(/\r\n/g, '\n').trim();
      if (!normalized) return [];

      const withoutHeader = normalized
        .replace(/^\s*##\s+Review Comments\s+\(\d+\)\s*\n*/i, '')
        .trim();
      if (!withoutHeader) return [];

      const sections = withoutHeader
        .split(/\n(?=\*\*.+?\*\*\s+\(Line\s+\d+\))/)
        .map((section) => section.trim())
        .filter(Boolean);

      return sections
        .map((section) => {
          const headerMatch = section.match(/^\*\*(.+?)\*\*\s+\(Line\s+(\d+)\)\s*/);
          if (!headerMatch) return null;

          const filePath = headerMatch[1].trim();
          const lineNumber = Number(headerMatch[2]);
          if (!filePath || Number.isNaN(lineNumber)) return null;

          let rest = section.slice(headerMatch[0].length).trim();
          let codeLine: string | undefined;

          if (rest.startsWith('```')) {
            const fencedMatch = rest.match(/^```[\w-]*\n?([\s\S]*?)\n?```/);
            if (fencedMatch) {
              codeLine = fencedMatch[1].trim() || undefined;
              rest = rest.slice(fencedMatch[0].length).trim();
            }
          } else if (rest.startsWith('`')) {
            const inlineMatch = rest.match(/^`([^`]+)`/);
            if (inlineMatch) {
              codeLine = inlineMatch[1].trim() || undefined;
              rest = rest.slice(inlineMatch[0].length).trim();
            }
          }

          const text = rest
            .split('\n')
            .map((line) => line.replace(/^>\s?/, ''))
            .join('\n')
            .trim();

          return {
            filePath,
            lineNumber,
            side: SplitSide.new,
            text,
            ...(codeLine ? { codeLine } : {}),
          };
        })
        .filter(
          (comment): comment is ParsedPersistedComment =>
            comment !== null && !!comment.text
        );
    },
    []
  );

  useEffect(() => {
    let cancelled = false;

    if (!attemptId) {
      setComments([]);
      setDrafts({});
      return;
    }

    attemptsApi
      .getReviewCommand(attemptId)
      .then((command) => {
        if (cancelled) return;
        if (!command || command.solved || !command.markdown_text.trim()) {
          setComments([]);
          setDrafts({});
          return;
        }

        const parsed = parsePersistedReviewMarkdown(command.markdown_text).map(
          (comment) => ({
            ...comment,
            id: genId(),
            solved: false,
          })
        );

        setComments(parsed);
        setDrafts({});
      })
      .catch((error) => {
        if (cancelled) return;
        console.error('Failed to load persisted review command comments', error);
      });

    return () => {
      cancelled = true;
    };
  }, [attemptId, parsePersistedReviewMarkdown]);

  const addComment = (comment: Omit<ReviewComment, 'id' | 'solved'>) => {
    const newComment: ReviewComment = {
      ...comment,
      id: genId(),
      solved: false,
    };
    setComments((prev) => [...prev, newComment]);
  };

  const updateComment = (id: string, text: string) => {
    setComments((prev) =>
      prev.map((comment) =>
        comment.id === id ? { ...comment, text } : comment
      )
    );
  };

  const toggleCommentSolved = (id: string) => {
    setComments((prev) =>
      prev.map((comment) =>
        comment.id === id ? { ...comment, solved: !comment.solved } : comment
      )
    );
  };

  const deleteComment = (id: string) => {
    setComments((prev) => prev.filter((comment) => comment.id !== id));
  };

  const clearComments = () => {
    setComments([]);
    setDrafts({});
  };

  const setDraft = (key: string, draft: ReviewDraft | null) => {
    setDrafts((prev) => {
      if (draft === null) {
        const newDrafts = { ...prev };
        delete newDrafts[key];
        return newDrafts;
      }
      return { ...prev, [key]: draft };
    });
  };

  const generateReviewMarkdown = useCallback(() => {
    const unsolvedComments = comments.filter((comment) => !comment.solved);
    if (unsolvedComments.length === 0) return '';

    const commentsNum = unsolvedComments.length;

    const header = `## Review Comments (${commentsNum})\n\n`;
    const formatCodeLine = (line?: string) => {
      if (!line) return '';
      if (line.includes('`')) {
        return `\`\`\`\n${line}\n\`\`\``;
      }
      return `\`${line}\``;
    };

    const commentsMd = unsolvedComments
      .map((comment) => {
        const codeLine = formatCodeLine(comment.codeLine);
        // Format file paths in comment body with backticks
        const bodyWithFormattedPaths = comment.text
          .trim()
          .replace(/([/\\]?[\w.-]+(?:[/\\][\w.-]+)+)/g, '`$1`');
        if (codeLine) {
          return `**${comment.filePath}** (Line ${comment.lineNumber})\n${codeLine}\n\n> ${bodyWithFormattedPaths}\n`;
        }
        return `**${comment.filePath}** (Line ${comment.lineNumber})\n\n> ${bodyWithFormattedPaths}\n`;
      })
      .join('\n');

    return header + commentsMd;
  }, [comments]);

  return (
    <ReviewContext.Provider
      value={{
        comments,
        drafts,
        addComment,
        updateComment,
        toggleCommentSolved,
        deleteComment,
        clearComments,
        setDraft,
        generateReviewMarkdown,
      }}
    >
      {children}
    </ReviewContext.Provider>
  );
}
