import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Button } from '@/components/ui/button';
import { AlertTriangle, FileWarning } from 'lucide-react';
import NiceModal, { useModal } from '@ebay/nice-modal-react';
import { defineModal } from '@/lib/modals';

interface ConfigParseErrorDialogProps {
  errorMessage: string;
}

const ConfigParseErrorDialogImpl =
  NiceModal.create<ConfigParseErrorDialogProps>(({ errorMessage }) => {
    const modal = useModal();

    const handleClose = () => {
      modal.resolve('closed');
      modal.hide();
    };

    return (
      <Dialog
        open={modal.visible}
        onOpenChange={(open) => !open && handleClose()}
      >
        <DialogContent className="sm:max-w-[600px]">
          <DialogHeader>
            <div className="flex items-center gap-3">
              <FileWarning className="h-6 w-6 text-destructive" />
              <DialogTitle>Config File Parse Error</DialogTitle>
            </div>
            <DialogDescription className="text-left space-y-4 pt-4">
              <p>
                Failed to parse the configuration file. The application is using
                default settings, but your original configuration file has{' '}
                <strong>not been overwritten</strong>.
              </p>
              <div className="bg-muted p-3 rounded-md">
                <p className="text-sm font-mono text-destructive break-all">
                  {errorMessage}
                </p>
              </div>
              <p>
                Please fix the configuration file manually and restart the
                application, or save new settings to overwrite the broken
                configuration.
              </p>
              <p className="text-sm text-muted-foreground">
                <AlertTriangle className="inline h-4 w-4 mr-1" />
                Tip: Check for syntax errors like missing commas, unclosed
                brackets, or invalid JSON values in your config file.
              </p>
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button onClick={handleClose} variant="default">
              I Understand
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    );
  });

export const ConfigParseErrorDialog = defineModal<
  ConfigParseErrorDialogProps,
  'closed' | void
>(ConfigParseErrorDialogImpl);
