import { useEffect, useState } from 'react';
import NiceModal, { useModal } from '@ebay/nice-modal-react';
import { useTranslation } from 'react-i18next';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Label } from '@/components/ui/label';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { invoke } from '@tauri-apps/api/core';
import { attemptsApi, type ExportContextFormat } from '@/lib/api';
import { defineModal, getErrorMessage } from '@/lib/modals';

export interface ExportContextDialogProps {
  attemptId: string;
  taskTitle: string;
}

const INVALID_FILENAME_CHARS = /[<>:"/\\|?*]/g;
const TRAILING_DOTS_AND_SPACES = /[. ]+$/g;
const FALLBACK_FILENAME = 'task-context';
const SAVE_PICKER_CANCEL_ERROR = 'AbortError';
const TAURI_SAVE_CANCELLED_ERROR = 'TAURI_SAVE_CANCELLED';

type SaveFilePickerOptionsLike = {
  suggestedName?: string;
  types?: Array<{
    description?: string;
    accept: Record<string, string[]>;
  }>;
};

type WritableFileLike = {
  write: (data: Blob) => Promise<void>;
  close: () => Promise<void>;
};

type FileHandleLike = {
  createWritable: () => Promise<WritableFileLike>;
};

type SaveFilePickerWindow = Window &
  typeof globalThis & {
    showSaveFilePicker?: (
      options?: SaveFilePickerOptionsLike
    ) => Promise<FileHandleLike>;
  };

type TauriWindow = Window &
  typeof globalThis & {
    __TAURI_INTERNALS__?: unknown;
  };

function replaceControlChars(value: string): string {
  return Array.from(value)
    .map((char) => {
      const code = char.charCodeAt(0);
      return code <= 31 || code === 127 ? ' ' : char;
    })
    .join('');
}

function normalizeFilenameBase(taskTitle: string): string {
  const sanitized = replaceControlChars(taskTitle)
    .replace(INVALID_FILENAME_CHARS, ' ')
    .replace(TRAILING_DOTS_AND_SPACES, '')
    .trim();

  return sanitized || FALLBACK_FILENAME;
}

function triggerDownload(blob: Blob, fileName: string) {
  const objectUrl = URL.createObjectURL(blob);
  const anchor = document.createElement('a');
  anchor.href = objectUrl;
  anchor.download = fileName;
  anchor.style.display = 'none';
  document.body.appendChild(anchor);
  anchor.click();
  anchor.remove();
  window.setTimeout(() => URL.revokeObjectURL(objectUrl), 0);
}

function isSavePickerCancelled(err: unknown): boolean {
  return err instanceof DOMException && err.name === SAVE_PICKER_CANCEL_ERROR;
}

function isTauriRuntime(): boolean {
  const tauriWindow = window as TauriWindow;
  return Boolean(tauriWindow.__TAURI_INTERNALS__);
}

function toErrorMessage(err: unknown): string | null {
  if (typeof err === 'string') {
    return err;
  }

  if (err instanceof Error) {
    return err.message;
  }

  if (
    err &&
    typeof err === 'object' &&
    'message' in err &&
    typeof err.message === 'string'
  ) {
    return err.message;
  }

  return null;
}

function isTauriSaveCancelled(err: unknown): boolean {
  const message = toErrorMessage(err);
  if (!message) {
    return false;
  }

  return message.includes(TAURI_SAVE_CANCELLED_ERROR);
}

function getMimeType(format: ExportContextFormat): string {
  return format === 'pdf' ? 'application/pdf' : 'text/markdown';
}

function getFileExtension(format: ExportContextFormat): string {
  return format === 'pdf' ? '.pdf' : '.md';
}

async function saveWithSystemFilePicker(
  blob: Blob,
  fileName: string,
  format: ExportContextFormat
): Promise<boolean> {
  const pickerWindow = window as SaveFilePickerWindow;
  if (typeof pickerWindow.showSaveFilePicker !== 'function') {
    return false;
  }

  const handle = await pickerWindow.showSaveFilePicker({
    suggestedName: fileName,
    types: [
      {
        description: format === 'pdf' ? 'PDF Document' : 'Markdown Document',
        accept: {
          [getMimeType(format)]: [getFileExtension(format)],
        },
      },
    ],
  });

  const writable = await handle.createWritable();
  try {
    await writable.write(blob);
  } finally {
    await writable.close();
  }

  return true;
}

async function saveWithTauriDialog(
  blob: Blob,
  fileName: string,
  format: ExportContextFormat
): Promise<boolean> {
  if (!isTauriRuntime()) {
    return false;
  }

  const bytes = Array.from(new Uint8Array(await blob.arrayBuffer()));
  await invoke('save_export_context_file', {
    suggestedFileName: fileName,
    format,
    bytes,
  });
  return true;
}

const ExportContextDialogImpl = NiceModal.create<ExportContextDialogProps>(
  ({ attemptId, taskTitle }) => {
    const modal = useModal();
    const { t } = useTranslation(['tasks', 'common']);
    const [format, setFormat] = useState<ExportContextFormat>('md');
    const [error, setError] = useState<string | null>(null);
    const [isExporting, setIsExporting] = useState(false);

    useEffect(() => {
      if (!modal.visible) {
        return;
      }

      setFormat('md');
      setError(null);
      setIsExporting(false);
    }, [modal.visible]);

    const handleCancel = () => {
      if (isExporting) {
        return;
      }

      modal.resolve();
      modal.hide();
    };

    const handleExport = async () => {
      setIsExporting(true);
      setError(null);

      try {
        const blob = await attemptsApi.exportContext(attemptId, format);
        const fileName = `${normalizeFilenameBase(taskTitle)}.${format}`;

        const savedByTauriDialog = await saveWithTauriDialog(
          blob,
          fileName,
          format
        );

        if (!savedByTauriDialog) {
          const savedByPicker = await saveWithSystemFilePicker(
            blob,
            fileName,
            format
          );
          if (!savedByPicker) {
            triggerDownload(blob, fileName);
          }
        }

        modal.resolve();
        modal.hide();
      } catch (err) {
        if (isSavePickerCancelled(err) || isTauriSaveCancelled(err)) {
          return;
        }

        const message = getErrorMessage(err);
        setError(message || t('exportContextDialog.genericError'));
      } finally {
        setIsExporting(false);
      }
    };

    return (
      <Dialog open={modal.visible} onOpenChange={(open) => !open && handleCancel()}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>{t('exportContextDialog.title')}</DialogTitle>
            <DialogDescription>
              {t('exportContextDialog.description')}
            </DialogDescription>
          </DialogHeader>

          <div className="space-y-2">
            <Label htmlFor="export-context-format">
              {t('exportContextDialog.formatLabel')}
            </Label>
            <Select
              value={format}
              onValueChange={(value) => {
                setFormat(value as ExportContextFormat);
                setError(null);
              }}
              disabled={isExporting}
            >
              <SelectTrigger id="export-context-format">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="md">
                  {t('exportContextDialog.formatMarkdown')}
                </SelectItem>
                <SelectItem value="pdf">
                  {t('exportContextDialog.formatPdf')}
                </SelectItem>
              </SelectContent>
            </Select>
          </div>

          {error && (
            <Alert variant="destructive">
              <AlertDescription>{error}</AlertDescription>
            </Alert>
          )}

          <DialogFooter>
            <Button
              variant="outline"
              onClick={handleCancel}
              disabled={isExporting}
            >
              {t('common:buttons.cancel')}
            </Button>
            <Button onClick={handleExport} disabled={isExporting}>
              {isExporting
                ? t('exportContextDialog.inProgress')
                : t('exportContextDialog.action')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    );
  }
);

export const ExportContextDialog = defineModal<ExportContextDialogProps, void>(
  ExportContextDialogImpl
);
