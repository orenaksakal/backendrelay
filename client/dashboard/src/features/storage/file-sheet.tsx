import { Download, FileQuestion, FolderOpen } from 'lucide-react';
import { CopyButton } from '@/components/data/copy-button';
import { Bytes } from '@/components/data/formatters';
import {
  KeyValueList,
  type KeyValueItem,
} from '@/components/data/key-value-list';
import { RelativeTime } from '@/components/data/relative-time';
import { EmptyState } from '@/components/feedback/empty-state';
import { ErrorState } from '@/components/feedback/error-state';
import { FormSkeleton } from '@/components/feedback/skeletons';
import { Button, buttonVariants } from '@/components/ui/button';
import {
  Sheet,
  SheetBody,
  SheetContent,
  SheetDescription,
  SheetFooter,
  SheetHeader,
  SheetTitle,
} from '@/components/ui/sheet';
import type { AppInstantClient } from '@/lib/instant';
import { useFile } from '@/lib/storage/rows';
import {
  fileContentType,
  fileKindFor,
  fileName,
  filePath,
  fileSize,
  fileUploadedAt,
  fileUrl,
  type FileRow,
} from '@/lib/storage/storage';

export interface FileSheetProps {
  client: AppInstantClient;
  fileId: string;
  /** The list row, so the header is populated before the sheet query lands. */
  fallback: FileRow | null;
  onOpenChange: (open: boolean) => void;
  onDelete: (file: FileRow) => void;
  /** C.9: delete needs the collaborator role. */
  canWrite: boolean;
}

/**
 * C.9 file sheet (`?file=`): a preview, the catalog metadata, and the copy,
 * download and delete actions. It only mounts behind the admin data gate.
 */
export function FileSheet({
  client,
  fileId,
  fallback,
  onOpenChange,
  onDelete,
  canWrite,
}: FileSheetProps) {
  const detail = useFile(client, fileId);
  const file = detail.file ?? fallback;
  const name = file === null ? 'File' : fileName(file);
  const path = file === null ? null : filePath(file);
  const url = file === null ? null : fileUrl(file);

  return (
    <Sheet
      open
      onOpenChange={(next) => {
        if (!next) onOpenChange(false);
      }}
    >
      <SheetContent size="sm">
        <SheetHeader>
          <SheetTitle className="break-all">{name}</SheetTitle>
          <SheetDescription className="break-all">
            {path ?? 'File in this app.'}
          </SheetDescription>
        </SheetHeader>

        {detail.error !== null ? (
          <SheetBody>
            <ErrorState thing="file" message={detail.error} />
          </SheetBody>
        ) : file === null && detail.isLoading ? (
          <SheetBody>
            <FormSkeleton rows={4} />
          </SheetBody>
        ) : file === null ? (
          <SheetBody>
            <EmptyState
              icon={FolderOpen}
              title="File not found"
              description="This file no longer exists in the app."
            />
          </SheetBody>
        ) : (
          <SheetBody className="flex flex-col gap-4">
            <FilePreview file={file} />
            <FileMetadata file={file} />
          </SheetBody>
        )}

        <SheetFooter className="flex-wrap justify-between">
          <div className="flex min-w-0 flex-col gap-1">
            <div className="flex flex-wrap items-center gap-1">
              {url === null ? (
                <span className="text-muted-foreground text-xs">
                  This file has no URL.
                </span>
              ) : (
                <CopyButton
                  value={url}
                  label="Copy URL"
                  aria-label="Copy file URL"
                />
              )}
            </div>
            <p className="text-muted-foreground text-xs">
              Links expire after 7 days.
            </p>
          </div>
          <div className="flex flex-wrap items-center gap-2">
            {url === null ? null : (
              <a
                href={url}
                download={name}
                className={buttonVariants({ variant: 'secondary' })}
              >
                <Download aria-hidden="true" />
                Download
              </a>
            )}
            {canWrite ? (
              <Button
                type="button"
                variant="destructive"
                disabled={file === null}
                onClick={() => {
                  if (file !== null) onDelete(file);
                }}
              >
                Delete
              </Button>
            ) : null}
          </div>
        </SheetFooter>
      </SheetContent>
    </Sheet>
  );
}

function FilePreview({ file }: { file: FileRow }) {
  const url = fileUrl(file);
  const name = fileName(file);
  if (url === null) {
    return (
      <p className="border-border text-muted-foreground rounded-md border p-4 text-sm">
        This file has no preview URL.
      </p>
    );
  }

  const kind = fileKindFor(file);
  if (kind === 'image') {
    return (
      <img
        src={url}
        alt={name}
        className="border-border max-h-72 w-auto max-w-full self-start rounded-md border object-contain"
      />
    );
  }
  if (kind === 'video') {
    return (
      // eslint-disable-next-line jsx-a11y/media-has-caption -- user-provided media has no caption track
      <video
        src={url}
        controls
        aria-label={name}
        className="border-border w-full rounded-md border"
      />
    );
  }
  if (kind === 'audio') {
    // eslint-disable-next-line jsx-a11y/media-has-caption -- user-provided media has no caption track
    return <audio src={url} controls aria-label={name} className="w-full" />;
  }
  if (kind === 'pdf') {
    return (
      <embed
        src={url}
        type="application/pdf"
        title={name}
        className="border-border h-72 w-full rounded-md border"
      />
    );
  }
  return (
    <p className="border-border text-muted-foreground flex items-center gap-2 rounded-md border p-4 text-sm">
      <FileQuestion aria-hidden="true" className="size-4 shrink-0" />
      No preview for this file type.
    </p>
  );
}

function FileMetadata({ file }: { file: FileRow }) {
  const size = fileSize(file);
  const uploaded = fileUploadedAt(file);
  const contentType = fileContentType(file);
  const path = filePath(file);

  const items: KeyValueItem[] = [
    {
      label: 'Path',
      value:
        path === null ? (
          <span className="text-muted-foreground">—</span>
        ) : (
          <span className="text-cell font-mono break-all">{path}</span>
        ),
    },
    {
      label: 'Type',
      value:
        contentType === null ? (
          <span className="text-muted-foreground">—</span>
        ) : (
          <span className="text-cell">{contentType}</span>
        ),
    },
    {
      label: 'Size',
      value:
        size === null ? (
          <span className="text-muted-foreground">—</span>
        ) : (
          <Bytes value={size} />
        ),
    },
    {
      label: 'Uploaded',
      value:
        uploaded === null ? (
          <span className="text-muted-foreground">—</span>
        ) : (
          <RelativeTime value={uploaded} />
        ),
    },
  ];

  return <KeyValueList items={items} />;
}
