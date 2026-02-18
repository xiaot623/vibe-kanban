import { FormEvent, useMemo, useState } from 'react';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Label } from '@/components/ui/label';
import { Input } from '@/components/ui/input';
import {
  Table,
  TableBody,
  TableCell,
  TableEmpty,
  TableHead,
  TableHeaderCell,
  TableLoading,
  TableRow,
} from '@/components/ui/table';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from '@/components/ui/tooltip';
import { ChevronRight, Info } from 'lucide-react';
import { skillsApi } from '@/lib/api';
import type { ImportSkillsBody, ImportSkillsResponse } from 'shared/types';
import { SkillsAgentSettings } from './SkillsAgentSettings';

interface ImportFormState {
  source: string;
  git_ref: string;
  subpath: string;
  skill_filter: string;
}

const defaultImportForm: ImportFormState = {
  source: '',
  git_ref: '',
  subpath: '',
  skill_filter: '',
};

function toImportBody(form: ImportFormState): ImportSkillsBody {
  const valueOrNull = (value: string): string | null => value.trim() || null;
  return {
    source: form.source.trim(),
    git_ref: valueOrNull(form.git_ref),
    subpath: valueOrNull(form.subpath),
    skill_filter: valueOrNull(form.skill_filter),
  };
}

export function SkillsAllSettings() {
  const { t } = useTranslation('settings');
  const queryClient = useQueryClient();
  const [isImportDialogOpen, setIsImportDialogOpen] = useState(false);
  const [showImportAdvancedOptions, setShowImportAdvancedOptions] =
    useState(false);
  const [importForm, setImportForm] =
    useState<ImportFormState>(defaultImportForm);
  const [importError, setImportError] = useState<string | null>(null);
  const [lastImportResult, setLastImportResult] =
    useState<ImportSkillsResponse | null>(null);

  const {
    data: skillsResponse,
    isLoading,
    error,
  } = useQuery({
    queryKey: ['skills', 'all'],
    queryFn: () => skillsApi.list(),
  });

  const importMutation = useMutation({
    mutationFn: (body: ImportSkillsBody) => skillsApi.importFromGit(body),
    onSuccess: async (result) => {
      setLastImportResult(result);
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ['skills', 'all'] }),
        queryClient.invalidateQueries({ queryKey: ['skills', 'links'] }),
      ]);
      setIsImportDialogOpen(false);
      setImportForm(defaultImportForm);
      setImportError(null);
    },
    onError: (err) => {
      setImportError(
        err instanceof Error
          ? err.message
          : t('settings.skills.errors.importFailed')
      );
    },
  });

  const sortedSkills = useMemo(
    () =>
      [...(skillsResponse?.skills ?? [])].sort((left, right) =>
        left.name.localeCompare(right.name)
      ),
    [skillsResponse?.skills]
  );

  const handleImportSubmit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    setImportError(null);

    if (!importForm.source.trim()) {
      setImportError(t('settings.skills.importDialog.errors.sourceRequired'));
      return;
    }

    await importMutation.mutateAsync(toImportBody(importForm));
  };

  return (
    <Card>
      <CardHeader className="flex flex-row items-start justify-between gap-4">
        <div>
          <CardTitle>{t('settings.skills.all.title')}</CardTitle>
          <CardDescription>
            {t('settings.skills.all.description')}
          </CardDescription>
        </div>
        <Button
          onClick={() => {
            setShowImportAdvancedOptions(false);
            setIsImportDialogOpen(true);
          }}
        >
          {t('settings.skills.actions.import')}
        </Button>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="text-sm text-muted-foreground">
          {t('settings.skills.all.canonicalPath')}:{' '}
          <span className="font-mono text-foreground">
            {skillsResponse?.canonical_dir ?? '~/.kanban/skills'}
          </span>
        </div>

        {error && (
          <Alert variant="destructive">
            <AlertDescription>
              {error instanceof Error
                ? error.message
                : t('settings.skills.errors.loadFailed')}
            </AlertDescription>
          </Alert>
        )}

        {lastImportResult && (
          <Alert>
            <AlertDescription>
              {t('settings.skills.importSummary.imported', {
                count: lastImportResult.imported.length,
              })}
              {' · '}
              {t('settings.skills.importSummary.skipped', {
                count: lastImportResult.skipped.length,
              })}
              {' · '}
              {t('settings.skills.importSummary.warnings', {
                count: lastImportResult.warnings.length,
              })}
            </AlertDescription>
          </Alert>
        )}

        <div className="border rounded-md p-3">
          <Table className="w-full table-fixed">
            <TableHead>
              <TableRow>
                <TableHeaderCell className="w-[140px] pr-3 sm:w-[180px] sm:pr-4">
                  {t('settings.skills.table.name')}
                </TableHeaderCell>
                <TableHeaderCell className="pr-3 sm:pr-4">
                  {t('settings.skills.table.description')}
                </TableHeaderCell>
                <TableHeaderCell className="w-[140px] whitespace-nowrap pl-2 sm:w-[220px] sm:pl-4">
                  {t('settings.skills.table.path')}
                </TableHeaderCell>
              </TableRow>
            </TableHead>
            <TableBody>
              {isLoading && <TableLoading colSpan={3} />}
              {!isLoading && sortedSkills.length === 0 && (
                <TableEmpty colSpan={3}>
                  {t('settings.skills.empty.noSkills')}
                </TableEmpty>
              )}
              {!isLoading &&
                sortedSkills.map((skill) => (
                  <TableRow key={skill.path}>
                    <TableCell className="pr-3 sm:pr-4">
                      <span className="block truncate">{skill.name}</span>
                    </TableCell>
                    <TableCell className="max-w-0 pr-3 text-muted-foreground sm:pr-4">
                      {(() => {
                        const desc = skill.description || '';
                        if (!desc) return '—';
                        return (
                          <div className="flex min-w-0 items-center gap-1">
                            <span className="min-w-0 flex-1 truncate">
                              {desc}
                            </span>
                            <TooltipProvider>
                              <Tooltip>
                                <TooltipTrigger asChild>
                                  <Info className="hidden h-3.5 w-3.5 shrink-0 cursor-pointer text-muted-foreground hover:text-foreground sm:block" />
                                </TooltipTrigger>
                                <TooltipContent
                                  side="bottom"
                                  className="max-w-[320px]"
                                >
                                  {desc}
                                </TooltipContent>
                              </Tooltip>
                            </TooltipProvider>
                          </div>
                        );
                      })()}
                    </TableCell>
                    <TableCell className="w-[140px] pl-2 font-mono text-xs sm:w-[220px] sm:pl-4">
                      <div className="flex min-w-0 items-center gap-1">
                        <span className="min-w-0 flex-1 truncate">
                          {skill.path}
                        </span>
                        <TooltipProvider>
                          <Tooltip>
                            <TooltipTrigger asChild>
                              <Info className="hidden h-3.5 w-3.5 shrink-0 cursor-pointer text-muted-foreground hover:text-foreground sm:block" />
                            </TooltipTrigger>
                            <TooltipContent side="bottom" className="max-w-[420px]">
                              {skill.path}
                            </TooltipContent>
                          </Tooltip>
                        </TooltipProvider>
                      </div>
                    </TableCell>
                  </TableRow>
                ))}
            </TableBody>
          </Table>
        </div>
      </CardContent>

      <CardContent className="space-y-4 border-t pt-6">
        <SkillsAgentSettings />
      </CardContent>

      <Dialog open={isImportDialogOpen} onOpenChange={setIsImportDialogOpen}>
        <DialogContent className="sm:max-w-[560px]">
          <DialogHeader>
            <DialogTitle>{t('settings.skills.importDialog.title')}</DialogTitle>
            <DialogDescription>
              {t('settings.skills.importDialog.description')}
            </DialogDescription>
          </DialogHeader>

          <form className="space-y-4" onSubmit={handleImportSubmit}>
            <div className="space-y-2">
              <Label htmlFor="skills-import-source">
                {t('settings.skills.importDialog.source')}
              </Label>
              <Input
                id="skills-import-source"
                value={importForm.source}
                onChange={(event) =>
                  setImportForm((prev) => ({
                    ...prev,
                    source: event.target.value,
                  }))
                }
                placeholder={t(
                  'settings.skills.importDialog.sourcePlaceholder'
                )}
                disabled={importMutation.isPending}
              />
            </div>

            <div className="space-y-2">
              <button
                type="button"
                onClick={() => setShowImportAdvancedOptions((prev) => !prev)}
                className="flex w-full items-center gap-2 text-left text-sm text-muted-foreground transition-colors hover:text-foreground"
              >
                <ChevronRight
                  className={`h-3 w-3 transition-transform ${showImportAdvancedOptions ? 'rotate-90' : ''}`}
                />
                <span>
                  {t('settings.skills.importDialog.advancedOptions')}
                </span>
              </button>

              {showImportAdvancedOptions && (
                <div className="space-y-4 rounded-md border p-3">
                  <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
                    <div className="space-y-2">
                      <Label htmlFor="skills-import-git-ref">
                        {t('settings.skills.importDialog.gitRef')}
                      </Label>
                      <Input
                        id="skills-import-git-ref"
                        value={importForm.git_ref}
                        onChange={(event) =>
                          setImportForm((prev) => ({
                            ...prev,
                            git_ref: event.target.value,
                          }))
                        }
                        placeholder={t(
                          'settings.skills.importDialog.gitRefPlaceholder'
                        )}
                        disabled={importMutation.isPending}
                      />
                    </div>
                    <div className="space-y-2">
                      <Label htmlFor="skills-import-subpath">
                        {t('settings.skills.importDialog.subpath')}
                      </Label>
                      <Input
                        id="skills-import-subpath"
                        value={importForm.subpath}
                        onChange={(event) =>
                          setImportForm((prev) => ({
                            ...prev,
                            subpath: event.target.value,
                          }))
                        }
                        placeholder={t(
                          'settings.skills.importDialog.subpathPlaceholder'
                        )}
                        disabled={importMutation.isPending}
                      />
                    </div>
                  </div>

                  <div className="space-y-2">
                    <Label htmlFor="skills-import-filter">
                      {t('settings.skills.importDialog.skillFilter')}
                    </Label>
                    <Input
                      id="skills-import-filter"
                      value={importForm.skill_filter}
                      onChange={(event) =>
                        setImportForm((prev) => ({
                          ...prev,
                          skill_filter: event.target.value,
                        }))
                      }
                      placeholder={t(
                        'settings.skills.importDialog.skillFilterPlaceholder'
                      )}
                      disabled={importMutation.isPending}
                    />
                  </div>
                </div>
              )}
            </div>

            {importError && (
              <Alert variant="destructive">
                <AlertDescription>{importError}</AlertDescription>
              </Alert>
            )}

            <DialogFooter>
              <Button
                type="button"
                variant="ghost"
                onClick={() => setIsImportDialogOpen(false)}
                disabled={importMutation.isPending}
              >
                {t('settings.skills.actions.cancel')}
              </Button>
              <Button type="submit" disabled={importMutation.isPending}>
                {importMutation.isPending
                  ? t('settings.skills.actions.importing')
                  : t('settings.skills.actions.import')}
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>
    </Card>
  );
}
