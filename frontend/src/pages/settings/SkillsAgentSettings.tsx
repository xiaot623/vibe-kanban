import { FormEvent, useMemo, useState } from 'react';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Checkbox } from '@/components/ui/checkbox';
import { Label } from '@/components/ui/label';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
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
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from '@/components/ui/tooltip';
import { Info, AlertTriangle } from 'lucide-react';
import { skillsApi } from '@/lib/api';
import type { SkillInfo, AgentSkillLinkInfo } from 'shared/types';
import { BaseCodingAgent, SkillLinkState } from 'shared/types';

interface CanonicalSkillWithFolder extends SkillInfo {
  folderName: string;
}

function folderNameFromPath(path: string): string {
  const normalized = path.replace(/\\/g, '/').replace(/\/+$/, '');
  const segments = normalized.split('/');
  return segments[segments.length - 1] || normalized;
}

function isLinkedState(state: SkillLinkState): boolean {
  return state === SkillLinkState.LINKED;
}

function isLegacyLink(link: AgentSkillLinkInfo): boolean {
  return link.is_legacy;
}

function executorLabel(
  executor: BaseCodingAgent,
  t: (key: string) => string
): string {
  switch (executor) {
    case BaseCodingAgent.CLAUDE_CODE:
      return t('settings.skills.nav.claude');
    case BaseCodingAgent.CODEX:
      return t('settings.skills.nav.codex');
    case BaseCodingAgent.GEMINI:
      return t('settings.skills.nav.gemini');
    case BaseCodingAgent.OPENCODE:
      return t('settings.skills.nav.opencode');
    case BaseCodingAgent.PI:
      return t('settings.skills.nav.pi');
    default:
      return executor;
  }
}

export function SkillsAgentSettings() {
  const { t } = useTranslation('settings');
  const queryClient = useQueryClient();
  const [executor, setExecutor] = useState<BaseCodingAgent>(
    BaseCodingAgent.CLAUDE_CODE
  );
  const [isAddDialogOpen, setIsAddDialogOpen] = useState(false);
  const [selectedSkillNames, setSelectedSkillNames] = useState<string[]>([]);
  const [actionError, setActionError] = useState<string | null>(null);

  const {
    data: skillsResponse,
    isLoading: loadingSkills,
    error: skillsError,
  } = useQuery({
    queryKey: ['skills', 'all'],
    queryFn: () => skillsApi.list(),
  });

  const {
    data: linksResponse,
    isLoading: loadingLinks,
    error: linksError,
  } = useQuery({
    queryKey: ['skills', 'links', executor],
    queryFn: () => skillsApi.listLinks(executor),
  });

  const linkedSkills = useMemo(
    () =>
      (linksResponse?.links ?? []).filter((link) => isLinkedState(link.state)),
    [linksResponse?.links]
  );

  const canonicalSkillsByFolder = useMemo(() => {
    const map = new Map<string, CanonicalSkillWithFolder>();
    for (const skill of skillsResponse?.skills ?? []) {
      const folderName = folderNameFromPath(skill.path);
      map.set(folderName, { ...skill, folderName });
    }
    return map;
  }, [skillsResponse?.skills]);

  const addableSkills = useMemo(
    () =>
      (linksResponse?.links ?? [])
        .filter((link) => !isLinkedState(link.state))
        .map((link) => {
          const canonical = canonicalSkillsByFolder.get(link.skill_name);
          return {
            link,
            displayName: canonical?.name ?? link.skill_name,
            description: canonical?.description ?? '',
          };
        }),
    [linksResponse?.links, canonicalSkillsByFolder]
  );

  const linkMutation = useMutation({
    mutationFn: (skillNames: string[]) =>
      skillsApi.link(executor, { skill_names: skillNames }),
    onSuccess: async () => {
      await queryClient.invalidateQueries({
        queryKey: ['skills', 'links', executor],
      });
      setSelectedSkillNames([]);
      setIsAddDialogOpen(false);
      setActionError(null);
    },
    onError: (err) => {
      setActionError(
        err instanceof Error
          ? err.message
          : t('settings.skills.errors.linkFailed')
      );
    },
  });

  const unlinkMutation = useMutation({
    mutationFn: (skillName: string) => skillsApi.unlink(executor, skillName),
    onSuccess: async () => {
      await queryClient.invalidateQueries({
        queryKey: ['skills', 'links', executor],
      });
      setActionError(null);
    },
    onError: (err) => {
      setActionError(
        err instanceof Error
          ? err.message
          : t('settings.skills.errors.unlinkFailed')
      );
    },
  });

  const handleAddSubmit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    setActionError(null);
    if (selectedSkillNames.length === 0) {
      setActionError(t('settings.skills.errors.selectAtLeastOne'));
      return;
    }
    await linkMutation.mutateAsync(selectedSkillNames);
  };

  const toggleSkillSelection = (skillName: string, checked: boolean) => {
    setSelectedSkillNames((prev) => {
      if (checked) {
        return prev.includes(skillName) ? prev : [...prev, skillName];
      }
      return prev.filter((name) => name !== skillName);
    });
  };

  const resolveLinkedSkillName = (link: AgentSkillLinkInfo) =>
    canonicalSkillsByFolder.get(link.skill_name)?.name ?? link.skill_name;
  const resolveLinkedSkillDescription = (link: AgentSkillLinkInfo) =>
    link.description ||
    canonicalSkillsByFolder.get(link.skill_name)?.description ||
    (isLegacyLink(link) ? t('settings.skills.labels.legacyDescription') : '');

  const pageLabel = executorLabel(executor, (key) => t(key));

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between gap-2">
        <div className="flex items-center gap-2">
          <Label htmlFor="executor-select" className="text-sm shrink-0">
            {t('settings.skills.agent.executor', { defaultValue: 'Agent' })}
          </Label>
          <Select
            value={executor}
            onValueChange={(v) => {
              setExecutor(v as BaseCodingAgent);
              setActionError(null);
              setSelectedSkillNames([]);
            }}
          >
            <SelectTrigger id="executor-select" className="w-[160px]">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value={BaseCodingAgent.CLAUDE_CODE}>
                {t('settings.skills.nav.claude')}
              </SelectItem>
              <SelectItem value={BaseCodingAgent.CODEX}>
                {t('settings.skills.nav.codex')}
              </SelectItem>
              <SelectItem value={BaseCodingAgent.GEMINI}>
                {t('settings.skills.nav.gemini')}
              </SelectItem>
              <SelectItem value={BaseCodingAgent.OPENCODE}>
                {t('settings.skills.nav.opencode')}
              </SelectItem>
              <SelectItem value={BaseCodingAgent.PI}>
                {t('settings.skills.nav.pi')}
              </SelectItem>
            </SelectContent>
          </Select>
        </div>
        <Button
          onClick={() => {
            setSelectedSkillNames([]);
            setActionError(null);
            setIsAddDialogOpen(true);
          }}
          disabled={addableSkills.length === 0}
        >
          {t('settings.skills.actions.add')}
        </Button>
      </div>

      {actionError && (
        <Alert variant="destructive">
          <AlertDescription>{actionError}</AlertDescription>
        </Alert>
      )}
      {(skillsError || linksError) && (
        <Alert variant="destructive">
          <AlertDescription>
            {skillsError instanceof Error
              ? skillsError.message
              : linksError instanceof Error
                ? linksError.message
                : t('settings.skills.errors.loadFailed')}
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
              <TableHeaderCell className="w-[96px] whitespace-nowrap pl-2 sm:w-[120px] sm:pl-4">
                {t('settings.skills.table.actions')}
              </TableHeaderCell>
            </TableRow>
          </TableHead>
          <TableBody>
            {(loadingSkills || loadingLinks) && <TableLoading colSpan={3} />}
            {!loadingSkills && !loadingLinks && linkedSkills.length === 0 && (
              <TableEmpty colSpan={3}>
                {t('settings.skills.empty.noLinkedSkills')}
              </TableEmpty>
            )}
            {!loadingSkills &&
              !loadingLinks &&
              linkedSkills.map((link) => (
                <TableRow key={link.skill_name}>
                  <TableCell className="pr-3 sm:pr-4">
                    <div className="flex min-w-0 items-center gap-1.5">
                      {isLegacyLink(link) && (
                        <TooltipProvider>
                          <Tooltip>
                            <TooltipTrigger asChild>
                              <AlertTriangle className="h-3.5 w-3.5 shrink-0 text-yellow-500" />
                            </TooltipTrigger>
                            <TooltipContent side="bottom">
                              {t('settings.skills.labels.legacyDescription')}
                            </TooltipContent>
                          </Tooltip>
                        </TooltipProvider>
                      )}
                      <span className="truncate">
                        {resolveLinkedSkillName(link)}
                      </span>
                    </div>
                  </TableCell>
                  <TableCell className="max-w-0 pr-3 text-muted-foreground sm:pr-4">
                    {(() => {
                      const desc = resolveLinkedSkillDescription(link);
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
                  <TableCell className="w-[96px] pl-2 sm:w-[120px] sm:pl-4">
                    <Button
                      variant="ghost"
                      className="h-8 px-0 text-xs sm:h-9 sm:text-sm"
                      onClick={() => unlinkMutation.mutate(link.skill_name)}
                      disabled={
                        unlinkMutation.isPending || linkMutation.isPending
                      }
                    >
                      {t('settings.skills.actions.unlink')}
                    </Button>
                  </TableCell>
                </TableRow>
              ))}
          </TableBody>
        </Table>
      </div>

      <Dialog open={isAddDialogOpen} onOpenChange={setIsAddDialogOpen}>
        <DialogContent className="sm:max-w-[560px]">
          <DialogHeader>
            <DialogTitle>
              {t('settings.skills.addDialog.title', { agent: pageLabel })}
            </DialogTitle>
            <DialogDescription>
              {t('settings.skills.addDialog.description')}
            </DialogDescription>
          </DialogHeader>

          <form className="space-y-4" onSubmit={handleAddSubmit}>
            <div className="max-h-[320px] overflow-auto border rounded-md p-2 space-y-2">
              {addableSkills.length === 0 && (
                <div className="text-sm text-muted-foreground px-1 py-2">
                  {t('settings.skills.empty.noAddableSkills')}
                </div>
              )}
              {addableSkills.map((item) => (
                <label
                  key={item.link.skill_name}
                  className="flex items-start gap-3 p-2 border rounded-sm cursor-pointer hover:bg-muted/40"
                >
                  <Checkbox
                    checked={selectedSkillNames.includes(item.link.skill_name)}
                    onCheckedChange={(checked) =>
                      toggleSkillSelection(item.link.skill_name, checked)
                    }
                    disabled={linkMutation.isPending}
                  />
                  <div className="min-w-0">
                    <div className="text-sm font-medium">
                      {item.displayName}
                    </div>
                    <div className="text-xs text-muted-foreground">
                      {item.description || '—'}
                    </div>
                  </div>
                </label>
              ))}
            </div>

            {actionError && (
              <Alert variant="destructive">
                <AlertDescription>{actionError}</AlertDescription>
              </Alert>
            )}

            <DialogFooter>
              <Button
                type="button"
                variant="ghost"
                onClick={() => setIsAddDialogOpen(false)}
                disabled={linkMutation.isPending}
              >
                {t('settings.skills.actions.cancel')}
              </Button>
              <Button type="submit" disabled={linkMutation.isPending}>
                {linkMutation.isPending
                  ? t('settings.skills.actions.adding')
                  : t('settings.skills.actions.addSelected')}
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>
    </div>
  );
}
