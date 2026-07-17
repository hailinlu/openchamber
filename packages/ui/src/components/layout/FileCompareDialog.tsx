import React from 'react';

import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Button } from '@/components/ui/button';
import { Icon } from '@/components/icon/Icon';
import { PierreDiffViewer } from '@/components/views/PierreDiffViewer';
import { getLanguageFromExtension, isImageFile } from '@/lib/toolHelpers';
import { useRuntimeAPIs } from '@/hooks/useRuntimeAPIs';
import { useI18n } from '@/lib/i18n';

const LARGE_FILE_SIZE_BYTES = 1_000_000; // Warn before comparing files >1MB total

interface FileCompareDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  fileA: string | null;
  fileB: string | null;
}

export const FileCompareDialog: React.FC<FileCompareDialogProps> = ({
  open,
  onOpenChange,
  fileA,
  fileB,
}) => {
  const { t } = useI18n();
  const { files } = useRuntimeAPIs();

  const [contentA, setContentA] = React.useState<string | null>(null);
  const [contentB, setContentB] = React.useState<string | null>(null);
  const [loading, setLoading] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  const [binary, setBinary] = React.useState(false);
  const [swapped, setSwapped] = React.useState(false);
  const [renderSideBySide, setRenderSideBySide] = React.useState(true);
  const [wrapLines, setWrapLines] = React.useState(false);

  // Large file confirmation: store preloaded content until user confirms
  const [confirmLarge, setConfirmLarge] = React.useState<{
    contentA: string;
    contentB: string;
    combinedBytes: number;
  } | null>(null);

  // Reset and load when dialog opens with new files
  React.useEffect(() => {
    if (!open) {
      setContentA(null);
      setContentB(null);
      setLoading(false);
      setError(null);
      setBinary(false);
      setSwapped(false);
      setConfirmLarge(null);
      return;
    }

    if (!fileA || !fileB) {
      setError(t('fileCompare.error.missingFiles'));
      return;
    }

    let cancelled = false;

    const load = async () => {
      setLoading(true);
      setError(null);
      setBinary(false);
      setConfirmLarge(null);

      try {
        if (isImageFile(fileA) || isImageFile(fileB)) {
          setBinary(true);
          setLoading(false);
          return;
        }

        if (!files.readFile) {
          throw new Error(t('fileCompare.error.readNotAvailable'));
        }

        const [resultA, resultB] = await Promise.all([
          files.readFile(fileA),
          files.readFile(fileB),
        ]);

        if (cancelled) return;

        const combinedBytes = resultA.content.length + resultB.content.length;

        if (combinedBytes > LARGE_FILE_SIZE_BYTES) {
          setConfirmLarge({ contentA: resultA.content, contentB: resultB.content, combinedBytes });
          setLoading(false);
          return;
        }

        setContentA(resultA.content);
        setContentB(resultB.content);
      } catch (err) {
        if (!cancelled) {
          setError(err instanceof Error ? err.message : String(err));
        }
      } finally {
        if (!cancelled) {
          setLoading(false);
        }
      }
    };

    void load();

    return () => {
      cancelled = true;
    };
  }, [open, fileA, fileB, files, t]);

  const handleConfirmLarge = React.useCallback(() => {
    if (!confirmLarge) return;
    setContentA(confirmLarge.contentA);
    setContentB(confirmLarge.contentB);
    setConfirmLarge(null);
  }, [confirmLarge]);

  const handleCancelLarge = React.useCallback(() => {
    setConfirmLarge(null);
    onOpenChange(false);
  }, [onOpenChange]);

  const handleSwap = React.useCallback(() => {
    setSwapped((prev) => !prev);
  }, []);

  const original = swapped ? contentB : contentA;
  const modified = swapped ? contentA : contentB;

  const languageA = getLanguageFromExtension(fileA ?? '') || '';
  const languageB = getLanguageFromExtension(fileB ?? '') || '';
  const language = languageA || languageB || 'text';

  const fileNameA = fileA?.split('/').pop() ?? '';
  const fileNameB = fileB?.split('/').pop() ?? '';

  const hasContent = contentA !== null && contentB !== null;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        showCloseButton
        className="max-w-[92vw] w-[92vw] h-[88vh] max-h-[88vh] flex flex-col p-0 gap-0 overflow-hidden"
      >
        <DialogHeader className="shrink-0 px-5 pt-4 pb-2">
          <div className="flex items-center justify-between">
            <DialogTitle className="text-base font-medium">
              {t('fileCompare.title')}
            </DialogTitle>
          </div>

          {/* File path row */}
          <div className="flex items-center gap-2 mt-1.5 text-sm text-muted-foreground">
            <span className="truncate max-w-[280px]" title={fileA ?? ''}>
              {fileNameA}
            </span>
            <Icon name="arrow-left-right" className="h-3.5 w-3.5 shrink-0" />
            <span className="truncate max-w-[280px]" title={fileB ?? ''}>
              {fileNameB}
            </span>
            <Button
              variant="ghost"
              size="xs"
              className="h-6 gap-1 ml-1 shrink-0"
              onClick={handleSwap}
            >
              <Icon name="refresh" className="h-3.5 w-3.5" />
              {t('fileCompare.swap')}
            </Button>
          </div>

          {/* Toolbar */}
          {hasContent && (
            <div className="flex items-center gap-2 mt-2 border-t border-border/40 pt-2">
              <Button
                variant={renderSideBySide ? 'default' : 'ghost'}
                size="xs"
                className="h-7 gap-1"
                onClick={() => setRenderSideBySide(true)}
              >
                <Icon name="layout-column" className="h-3.5 w-3.5" />
                {t('fileCompare.sideBySide')}
              </Button>
              <Button
                variant={!renderSideBySide ? 'default' : 'ghost'}
                size="xs"
                className="h-7 gap-1"
                onClick={() => setRenderSideBySide(false)}
              >
                <Icon name="align-justify" className="h-3.5 w-3.5" />
                {t('fileCompare.unified')}
              </Button>
              <div className="mx-1 h-5 w-px bg-border/40" />
              <Button
                variant={wrapLines ? 'default' : 'ghost'}
                size="xs"
                className="h-7 gap-1"
                onClick={() => setWrapLines((prev) => !prev)}
              >
                <Icon name="text-wrap" className="h-3.5 w-3.5" />
                {t('fileCompare.wrapLines')}
              </Button>
            </div>
          )}
        </DialogHeader>

        {/* Body */}
        <div className="flex-1 min-h-0 overflow-auto px-5 pb-4">
          {loading && (
            <div className="flex items-center justify-center h-full text-muted-foreground gap-2">
              <Icon name="loader-4" className="h-5 w-5 animate-spin" />
              {t('fileCompare.loading')}
            </div>
          )}

          {error && (
            <div className="flex flex-col items-center justify-center h-full gap-3 text-center px-4">
              <Icon name="error-warning" className="h-10 w-10 text-status-error shrink-0" />
              <p className="text-muted-foreground">{error}</p>
            </div>
          )}

          {binary && (
            <div className="flex flex-col items-center justify-center h-full gap-3 text-center px-4">
              <Icon name="file-image" className="h-10 w-10 text-muted-foreground shrink-0" />
              <p className="text-muted-foreground">{t('fileCompare.binaryNotSupported')}</p>
            </div>
          )}

          {confirmLarge && (
            <div className="flex flex-col items-center justify-center h-full gap-4 text-center px-4">
              <Icon name="error-warning" className="h-10 w-10 text-muted-foreground shrink-0" />
              <p className="text-muted-foreground">
                {t('fileCompare.warning.largeFileDescription', {
                  sizeMb: (confirmLarge.combinedBytes / 1_000_000).toFixed(1),
                })}
              </p>
              <div className="flex gap-2">
                <Button variant="outline" onClick={handleCancelLarge}>
                  {t('sidebarFilesTree.dialog.cancel')}
                </Button>
                <Button variant="default" onClick={handleConfirmLarge}>
                  {t('fileCompare.warning.compareAnyway')}
                </Button>
              </div>
            </div>
          )}

          {hasContent && (
            <PierreDiffViewer
              original={original ?? ''}
              modified={modified ?? ''}
              language={language}
              fileName={fileA ?? ''}
              renderSideBySide={renderSideBySide}
              wrapLines={wrapLines}
              layout="fill"
            />
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
};
