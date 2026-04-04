import { memo, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  MDXEditor,
  type MDXEditorMethods,
  codeBlockPlugin,
  codeMirrorPlugin,
  diffSourcePlugin,
  headingsPlugin,
  imagePlugin,
  linkDialogPlugin,
  linkPlugin,
  listsPlugin,
  markdownShortcutPlugin,
  quotePlugin,
  thematicBreakPlugin,
} from '@mdxeditor/editor';
import '@mdxeditor/editor/style.css';
import { useImageUpload } from '@/hooks';
import { cn } from '@/lib/utils';
import type { ImageResponse } from 'shared/types';
import { Button } from '@/components/ui/button';
import { Check, Clipboard, Pencil, Trash2 } from 'lucide-react';
import { writeClipboardViaBridge } from '@/vscode/bridge';

/**
 * Local image metadata for rendering uploaded images before they're saved to the server.
 */
export type LocalImageMetadata = {
  path: string; // ".vibe-images/uuid.png"
  proxy_url: string; // "/api/images/{id}/file"
  file_name: string;
  size_bytes: number;
  format: string;
};

/**
 * Normalized comment data for GitHub PR comments embedded in markdown.
 * Uses string IDs (bigint converted) and consistent field names.
 * Serialized as fenced code blocks with language 'gh-comment'.
 */
export interface NormalizedComment {
  id: string;
  comment_type: 'general' | 'review';
  author: string;
  body: string;
  created_at: string;
  url?: string | null;
  // Review-specific (optional)
  path?: string;
  line?: number | null;
  diff_hunk?: string | null;
}

/** Markdown string representing the editor content */
export type SerializedEditorState = string;

type MarkdownEditorProps = {
  placeholder?: string;
  /** Markdown string representing the editor content */
  value: SerializedEditorState;
  onChange?: (state: SerializedEditorState) => void;
  disabled?: boolean;
  className?: string;
  /** Task attempt ID for resolving .vibe-images paths (preferred over taskId) */
  taskAttemptId?: string;
  /** Task ID for resolving .vibe-images paths when taskAttemptId is not available */
  taskId?: string;
  /** Local images for immediate rendering (before saved to server) */
  localImages?: LocalImageMetadata[];
  /** Optional edit callback - shows edit button in read-only mode when provided */
  onEdit?: () => void;
  /** Optional delete callback - shows delete button in read-only mode when provided */
  onDelete?: () => void;
  onCmdEnter?: () => void;
  onShiftCmdEnter?: () => void;
  /** Whether the editor is in fullscreen mode (affects min-height) */
  isFullscreen?: boolean;
  /** Called when an image is successfully uploaded */
  onImageUploaded?: (image: ImageResponse) => void;
  /**
   * Paste files handler (for image paste-to-upload flows).
   * Note: MDXEditor handles image uploads via the imagePlugin natively.
   * This prop is accepted for API compatibility but may not be wired if
   * MDXEditor's own image upload handler is preferred.
   */
  onPasteFiles?: (files: File[]) => void;
  /** Project/workspace ID for file search (accepted for API compat, not used in MDXEditor) */
  projectId?: string;
  /** Workspace ID for file search (accepted for API compat, not used in MDXEditor) */
  workspaceId?: string;
  /** Auto-focus the editor on mount */
  autoFocus?: boolean;
  /**
   * Function to find a matching diff path for clickable inline code.
   * Accepted for API compat; not implemented in MDXEditor migration.
   */
  findMatchingDiffPath?: (text: string) => string | null;
  /**
   * Callback when clickable inline code is clicked.
   * Accepted for API compat; not implemented in MDXEditor migration.
   */
  onCodeClick?: (fullPath: string) => void;
};

function MarkdownEditor(props: MarkdownEditorProps) {
  const {
    placeholder,
    value,
    onChange,
    disabled = false,
    className,
    taskAttemptId,
    taskId,
    localImages,
    onEdit,
    onDelete,
    onCmdEnter,
    onShiftCmdEnter,
    isFullscreen = false,
    onImageUploaded,
  } = props;
  const editorRef = useRef<MDXEditorMethods>(null);
  const lastMarkdownRef = useRef(value);
  const { upload, uploadForTask } = useImageUpload();

  // Copy button state (read-only mode)
  const [copied, setCopied] = useState(false);
  const handleCopy = useCallback(async () => {
    if (!value) return;
    try {
      // Unescape markdown-escaped underscores for cleaner clipboard output
      const unescaped = value.replace(/\\_/g, '_');
      await writeClipboardViaBridge(unescaped);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 400);
    } catch {
      // noop – bridge handles fallback
    }
  }, [value]);

  const handleChange = useCallback(
    (nextValue: string) => {
      lastMarkdownRef.current = nextValue;
      onChange?.(nextValue);
    },
    [onChange]
  );

  useEffect(() => {
    const editor = editorRef.current;
    if (!editor) return;
    if (lastMarkdownRef.current === value) return;

    const currentValue = editor.getMarkdown();
    if (currentValue !== value) {
      editor.setMarkdown(value);
    }
    lastMarkdownRef.current = value;
  }, [value]);

  const imageUploadHandler = useCallback(
    async (file: File) => {
      const image =
        taskId
          ? await uploadForTask(taskId, file)
          : taskAttemptId
            ? await uploadForTask(taskAttemptId, file)
            : await upload(file);

      onImageUploaded?.(image);
      return image.file_path;
    },
    [taskId, taskAttemptId, upload, uploadForTask, onImageUploaded]
  );

  const imagePreviewHandler = useCallback(
    async (src: string) => {
      if (!src.startsWith('.vibe-images/')) return src;

      const localImage = localImages?.find((image) => image.path === src);
      if (localImage) return localImage.proxy_url;

      // Prefer taskAttemptId endpoint, fall back to taskId
      if (taskAttemptId) {
        const response = await fetch(
          `/api/task-attempts/${taskAttemptId}/images/metadata?path=${encodeURIComponent(src)}`
        );
        const data = (await response.json()) as {
          data?: { proxy_url?: string | null } | null;
        };
        return data.data?.proxy_url ?? src;
      }

      if (taskId) {
        const response = await fetch(
          `/api/images/task/${taskId}/metadata?path=${encodeURIComponent(src)}`
        );
        const data = (await response.json()) as {
          data?: { proxy_url?: string | null } | null;
        };
        return data.data?.proxy_url ?? src;
      }

      return src;
    },
    [localImages, taskAttemptId, taskId]
  );

  const handleKeyDownCapture = useCallback(
    (event: React.KeyboardEvent<HTMLDivElement>) => {
      if (disabled || event.nativeEvent.isComposing) return;

      const isModifierPressed = event.metaKey || event.ctrlKey;
      if (!isModifierPressed || event.key !== 'Enter') return;

      event.preventDefault();
      event.stopPropagation();

      if (event.shiftKey) {
        onShiftCmdEnter?.();
        return;
      }

      onCmdEnter?.();
    },
    [disabled, onCmdEnter, onShiftCmdEnter]
  );

  const plugins = useMemo(
    () => [
      headingsPlugin({ allowedHeadingLevels: [1, 2, 3] }),
      listsPlugin(),
      quotePlugin(),
      markdownShortcutPlugin(),
      linkPlugin(),
      linkDialogPlugin(),
      imagePlugin({
        imageUploadHandler,
        imagePreviewHandler,
      }),
      codeBlockPlugin({ defaultCodeBlockLanguage: 'txt' }),
      codeMirrorPlugin({
        codeBlockLanguages: {
          txt: 'Text',
          md: 'Markdown',
          json: 'JSON',
          js: 'JavaScript',
          jsx: 'JSX',
          ts: 'TypeScript',
          tsx: 'TSX',
          sh: 'Shell',
          bash: 'Bash',
          rust: 'Rust',
          css: 'CSS',
          html: 'HTML',
          'gh-comment': 'GitHub Comment',
          py: 'Python',
          go: 'Go',
          java: 'Java',
          cpp: 'C++',
          c: 'C',
          rb: 'Ruby',
          php: 'PHP',
          swift: 'Swift',
          kt: 'Kotlin',
          yaml: 'YAML',
          toml: 'TOML',
          sql: 'SQL',
          graphql: 'GraphQL',
          dockerfile: 'Dockerfile',
        },
      }),
      diffSourcePlugin({ viewMode: 'rich-text' }),
      thematicBreakPlugin(),
    ],
    [imagePreviewHandler, imageUploadHandler]
  );

  const editorContent = (
    <div
      className={cn(
        'wysiwyg text-base',
        !disabled && className,
        !disabled && isFullscreen && 'h-full flex flex-col'
      )}
      onKeyDownCapture={disabled ? undefined : handleKeyDownCapture}
    >
      <MDXEditor
        ref={editorRef}
        markdown={value}
        onChange={disabled ? undefined : handleChange}
        placeholder={placeholder}
        readOnly={disabled}
        className={cn(
          'mdxeditor markdown-editor flex flex-col bg-transparent',
          !disabled && isFullscreen && 'flex-1 min-h-0',
          '[--basePageBg:transparent]',
          '[--baseBase:transparent]',
          '[--baseBgSubtle:transparent]',
          '[--baseBg:transparent]',
          '[--baseBgHover:hsl(var(--background))]',
          '[--baseBgActive:hsl(var(--accent))]',
          '[--baseLine:hsl(var(--border))]',
          '[--baseBorder:hsl(var(--border))]',
          '[--baseBorderHover:hsl(var(--muted-foreground)/0.6)]',
          '[--baseText:hsl(var(--muted-foreground))]',
          '[--baseTextContrast:hsl(var(--foreground))]',
          '[--accentSolid:hsl(var(--brand))]',
          '[--accentSolidHover:hsl(var(--brand-hover))]',
          '[--accentText:hsl(var(--brand))]',
          '[--accentTextContrast:hsl(var(--text-on-brand))]',
          '[&_.cm-editor]:bg-transparent [&_.cm-editor]:text-sm',
          '[&_.mdxeditor-root-contenteditable]:bg-transparent',
          '[&_.cm-scroller]:min-h-[3rem] [&_.cm-scroller]:overflow-auto',
          !disabled && isFullscreen && '[&_.cm-scroller]:min-h-[18rem]'
        )}
        contentEditableClassName={cn(
          'bg-transparent text-base leading-6 text-foreground',
          'font-sans outline-none',
          '[&_a]:text-blue-600 [&_a]:underline [&_a]:underline-offset-2 dark:[&_a]:text-blue-400',
          '[&_a:hover]:text-blue-800 dark:[&_a:hover]:text-blue-300',
          '[&_blockquote]:my-3 [&_blockquote]:border-l-4 [&_blockquote]:border-primary-foreground [&_blockquote]:pl-4 [&_blockquote]:text-muted-foreground',
          '[&_code]:rounded [&_code]:bg-muted [&_code]:bg-panel [&_code]:px-1 [&_code]:py-0.5 [&_code]:font-mono',
          '[&_h1]:mt-4 [&_h1]:mb-2 [&_h1]:text-2xl [&_h1]:font-semibold',
          '[&_h2]:mt-3 [&_h2]:mb-2 [&_h2]:text-xl [&_h2]:font-semibold',
          '[&_h3]:mt-3 [&_h3]:mb-2 [&_h3]:text-lg [&_h3]:font-semibold',
          '[&_h4]:mt-2 [&_h4]:mb-1 [&_h4]:text-base [&_h4]:font-medium',
          '[&_h5]:mt-2 [&_h5]:mb-1 [&_h5]:text-sm [&_h5]:font-medium',
          '[&_h6]:mt-2 [&_h6]:mb-1 [&_h6]:text-xs [&_h6]:font-medium [&_h6]:uppercase [&_h6]:tracking-wide',
          '[&_hr]:my-4 [&_hr]:border-border',
          '[&_img]:max-h-80 [&_img]:rounded-sm [&_img]:border [&_img]:border-border',
          '[&_li]:my-1 [&_ol]:my-1 [&_ol]:list-decimal [&_ol]:list-inside [&_ul]:my-1 [&_ul]:list-disc [&_ul]:list-inside',
          '[&_p]:mb-2 [&_p:last-child]:mb-0',
          '[&_pre]:my-2 [&_pre]:overflow-x-auto [&_pre]:rounded-md [&_pre]:bg-secondary [&_pre]:px-3 [&_pre]:py-2 [&_pre]:font-mono [&_pre]:whitespace-pre',
          '[&_strong]:font-semibold',
          '[&_em]:italic',
          '[&_table]:border-collapse [&_table]:my-2 [&_table]:w-full [&_table]:text-sm',
          '[&_td]:border [&_td]:border-low [&_td]:px-3 [&_td]:py-2 [&_td]:text-left [&_td]:align-top',
          '[&_th]:bg-muted [&_th]:font-semibold [&_th]:border [&_th]:border-low [&_th]:px-3 [&_th]:py-2 [&_th]:text-left [&_th]:align-top',
          disabled
            ? cn(className)
            : cn(
                'min-h-[2.5rem]',
                isFullscreen ? 'flex-1 overflow-y-auto' : ''
              )
        )}
        plugins={plugins}
      />
    </div>
  );

  // Wrap with action buttons in read-only mode
  if (disabled) {
    return (
      <div className="relative group">
        <div className="sticky top-0 right-2 z-10 pointer-events-none h-0">
          <div className="flex justify-end gap-1 opacity-0 group-hover:opacity-100 transition-opacity duration-150">
            {/* Copy button */}
            <Button
              type="button"
              aria-label={copied ? 'Copied!' : 'Copy as Markdown'}
              title={copied ? 'Copied!' : 'Copy as Markdown'}
              variant="icon"
              size="icon"
              onClick={handleCopy}
              className="pointer-events-auto p-2 bg-muted h-8 w-8"
            >
              {copied ? (
                <Check className="w-4 h-4 text-success" />
              ) : (
                <Clipboard className="w-4 h-4 text-muted-foreground" />
              )}
            </Button>
            {/* Edit button - only if onEdit provided */}
            {onEdit && (
              <Button
                type="button"
                aria-label="Edit"
                title="Edit"
                variant="icon"
                size="icon"
                onClick={onEdit}
                className="pointer-events-auto p-2 bg-muted h-8 w-8"
              >
                <Pencil className="w-4 h-4 text-muted-foreground" />
              </Button>
            )}
            {/* Delete button - only if onDelete provided */}
            {onDelete && (
              <Button
                type="button"
                aria-label="Delete"
                title="Delete"
                variant="icon"
                size="icon"
                onClick={onDelete}
                className="pointer-events-auto p-2 bg-muted h-8 w-8"
              >
                <Trash2 className="w-4 h-4 text-muted-foreground" />
              </Button>
            )}
          </div>
        </div>
        {editorContent}
      </div>
    );
  }

  return editorContent;
}

export default memo(MarkdownEditor);
