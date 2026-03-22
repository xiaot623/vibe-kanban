import { memo, useCallback, useEffect, useMemo, useRef } from 'react';
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
import type { LocalImageMetadata } from './wysiwyg/context/task-attempt-context';
import type { ImageResponse } from 'shared/types';

type TaskDescriptionEditorProps = {
  value: string;
  onChange: (md: string) => void;
  disabled?: boolean;
  placeholder?: string;
  taskId?: string;
  isFullscreen: boolean;
  onCmdEnter?: () => void;
  onShiftCmdEnter?: () => void;
  localImages?: LocalImageMetadata[];
  className?: string;
  onImageUploaded?: (image: ImageResponse) => void;
};

function TaskDescriptionEditor({
  value,
  onChange,
  disabled = false,
  placeholder,
  taskId,
  isFullscreen,
  onCmdEnter,
  onShiftCmdEnter,
  localImages,
  className,
  onImageUploaded,
}: TaskDescriptionEditorProps) {
  const editorRef = useRef<MDXEditorMethods>(null);
  const lastMarkdownRef = useRef(value);
  const { upload, uploadForTask } = useImageUpload();

  const handleChange = useCallback(
    (nextValue: string) => {
      lastMarkdownRef.current = nextValue;
      onChange(nextValue);
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
      const image = taskId
        ? await uploadForTask(taskId, file)
        : await upload(file);

      onImageUploaded?.(image);
      return image.file_path;
    },
    [taskId, upload, uploadForTask, onImageUploaded]
  );

  const imagePreviewHandler = useCallback(
    async (src: string) => {
      if (!src.startsWith('.vibe-images/')) return src;

      const localImage = localImages?.find((image) => image.path === src);
      if (localImage) return localImage.proxy_url;

      if (!taskId) return src;

      const response = await fetch(
        `/api/images/task/${taskId}/metadata?path=${encodeURIComponent(src)}`
      );
      const data = (await response.json()) as {
        data?: { proxy_url?: string | null } | null;
      };

      return data.data?.proxy_url ?? src;
    },
    [localImages, taskId]
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
        },
      }),
      diffSourcePlugin({ viewMode: 'rich-text' }),
      thematicBreakPlugin(),
    ],
    [imagePreviewHandler, imageUploadHandler]
  );

  return (
    <div
      className={cn(
        'h-full min-h-0 rounded-none border border-input bg-transparent',
        'focus-within:ring-1 focus-within:ring-ring',
        className
      )}
      onKeyDownCapture={handleKeyDownCapture}
    >
      <MDXEditor
        ref={editorRef}
        markdown={value}
        onChange={handleChange}
        placeholder={placeholder}
        readOnly={disabled}
        className={cn(
          'mdxeditor task-description-editor flex h-full min-h-0 flex-col bg-transparent text-sm',
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
          '[&_.cm-editor]:h-full [&_.cm-editor]:bg-transparent [&_.cm-editor]:text-sm',
          '[&_.mdxeditor-root-contenteditable]:bg-transparent',
          '[&_.cm-scroller]:min-h-[6rem] [&_.cm-scroller]:overflow-auto',
          isFullscreen && '[&_.cm-scroller]:min-h-[18rem]'
        )}
        contentEditableClassName={cn(
          'min-h-[6rem] bg-transparent text-sm leading-6 text-foreground',
          'font-sans',
          '[&_.token.comment]:text-muted-foreground',
          '[&_a]:text-blue-600 [&_a]:underline [&_a]:underline-offset-2 dark:[&_a]:text-blue-400',
          '[&_blockquote]:my-3 [&_blockquote]:border-l-2 [&_blockquote]:border-border [&_blockquote]:pl-4 [&_blockquote]:text-muted-foreground',
          '[&_code]:rounded-sm [&_code]:bg-muted [&_code]:px-1 [&_code]:py-0.5 [&_code]:font-mono [&_code]:text-[0.9em]',
          '[&_h1]:mt-4 [&_h1]:text-2xl [&_h1]:font-semibold',
          '[&_h2]:mt-4 [&_h2]:text-xl [&_h2]:font-semibold',
          '[&_h3]:mt-3 [&_h3]:text-lg [&_h3]:font-semibold',
          '[&_hr]:my-4 [&_hr]:border-border',
          '[&_img]:max-h-80 [&_img]:rounded-sm [&_img]:border [&_img]:border-border',
          '[&_li]:my-1 [&_ol]:my-2 [&_ol]:list-decimal [&_ol]:pl-6 [&_ul]:my-2 [&_ul]:list-disc [&_ul]:pl-6',
          '[&_p]:mb-2 [&_p:last-child]:mb-0',
          '[&_pre]:my-3 [&_pre]:overflow-x-auto [&_pre]:rounded-sm [&_pre]:border [&_pre]:border-border [&_pre]:bg-muted [&_pre]:p-3',
          isFullscreen ? 'flex-1 overflow-y-auto' : 'max-h-24 overflow-y-auto'
        )}
        plugins={plugins}
      />
    </div>
  );
}

export default memo(TaskDescriptionEditor);
