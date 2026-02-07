import { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { cloneDeep, merge, isEqual } from 'lodash';
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { Label } from '@/components/ui/label';
import { Input } from '@/components/ui/input';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Checkbox } from '@/components/ui/checkbox';
import { Loader2, Volume2 } from 'lucide-react';
import {
  DEFAULT_PR_DESCRIPTION_PROMPT,
  EditorType,
  PowerMode,
  SoundFile,
  ThemeMode,
  UiLanguage,
} from 'shared/types';
import { getLanguageOptions } from '@/i18n/languages';

import { toPrettyCase } from '@/utils/string';
import { useEditorAvailability } from '@/hooks/useEditorAvailability';
import { EditorAvailabilityIndicator } from '@/components/EditorAvailabilityIndicator';
import { useTheme } from '@/components/ThemeProvider';
import { useUserSystem } from '@/components/ConfigProvider';
import { TagManager } from '@/components/TagManager';

export function GeneralSettings() {
  const { t } = useTranslation(['settings', 'common']);

  // Get language options with proper display names
  const languageOptions = getLanguageOptions(
    t('language.browserDefault', {
      ns: 'common',
      defaultValue: 'Browser Default',
    })
  );
  const {
    config,
    loading,
    profiles,
    updateAndSaveConfig, // Use this on Save
  } = useUserSystem();

  // Draft state management
  const [draft, setDraft] = useState(() => (config ? cloneDeep(config) : null));
  const [dirty, setDirty] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [success, setSuccess] = useState(false);
  const [branchPrefixError, setBranchPrefixError] = useState<string | null>(
    null
  );
  const { setTheme } = useTheme();

  // Check editor availability when draft editor changes
  const editorAvailability = useEditorAvailability(draft?.editor.editor_type);

  const validateBranchPrefix = useCallback(
    (prefix: string): string | null => {
      if (!prefix) return null; // empty allowed
      if (prefix.includes('/'))
        return t('settings.general.git.branchPrefix.errors.slash');
      if (prefix.startsWith('.'))
        return t('settings.general.git.branchPrefix.errors.startsWithDot');
      if (prefix.endsWith('.') || prefix.endsWith('.lock'))
        return t('settings.general.git.branchPrefix.errors.endsWithDot');
      if (prefix.includes('..') || prefix.includes('@{'))
        return t('settings.general.git.branchPrefix.errors.invalidSequence');
      if (/[ \t~^:?*[\\]/.test(prefix))
        return t('settings.general.git.branchPrefix.errors.invalidChars');
      // Control chars check
      for (let i = 0; i < prefix.length; i++) {
        const code = prefix.charCodeAt(i);
        if (code < 0x20 || code === 0x7f)
          return t('settings.general.git.branchPrefix.errors.controlChars');
      }
      return null;
    },
    [t]
  );

  // When config loads or changes externally, update draft only if not dirty
  useEffect(() => {
    if (!config) return;
    if (!dirty) {
      setDraft(cloneDeep(config));
    }
  }, [config, dirty]);

  // Check for unsaved changes
  const hasUnsavedChanges = useMemo(() => {
    if (!draft || !config) return false;
    return !isEqual(draft, config);
  }, [draft, config]);

  // Generic draft update helper
  const updateDraft = useCallback(
    (patch: Partial<typeof config>) => {
      setDraft((prev: typeof config) => {
        if (!prev) return prev;
        const next = merge({}, prev, patch);
        // Mark dirty if changed
        if (!isEqual(next, config)) {
          setDirty(true);
        }
        return next;
      });
    },
    [config]
  );

  // Optional: warn on tab close/navigation with unsaved changes
  useEffect(() => {
    const handler = (e: BeforeUnloadEvent) => {
      if (hasUnsavedChanges) {
        e.preventDefault();
        e.returnValue = '';
      }
    };
    window.addEventListener('beforeunload', handler);
    return () => window.removeEventListener('beforeunload', handler);
  }, [hasUnsavedChanges]);

  const playSound = async (soundFile: SoundFile) => {
    const audio = new Audio(`/api/sounds/${soundFile}`);
    try {
      await audio.play();
    } catch (err) {
      console.error('Failed to play sound:', err);
    }
  };

  const generateLocalNetworkPassword = useCallback(() => {
    const bytes = new Uint8Array(12);
    crypto.getRandomValues(bytes);
    return Array.from(bytes, (byte) => byte.toString(16).padStart(2, '0')).join(
      ''
    );
  }, []);

  const localNetworkPasswordError = useMemo(() => {
    if (!draft?.local_network_access) return null;
    const password = draft.local_network_password?.trim() ?? '';
    if (!password) {
      return t('settings.general.beta.localNetworkAccess.password.error');
    }
    return null;
  }, [draft, t]);

  const telegramModeOptions = useMemo(() => {
    const executor = draft?.telegram?.default_executor;
    if (!profiles || !executor) return [];
    const variants = profiles[executor];
    if (!variants) return [];
    return Object.keys(variants)
      .filter((variant) => variant !== 'DEFAULT')
      .sort((a, b) => a.localeCompare(b));
  }, [draft?.telegram?.default_executor, profiles]);

  const handleSave = async () => {
    if (!draft) return;

    setSaving(true);
    setError(null);
    setSuccess(false);

    try {
      await updateAndSaveConfig(draft); // Atomically apply + persist
      setTheme(draft.theme);
      setDirty(false);
      setSuccess(true);
      setTimeout(() => setSuccess(false), 3000);
    } catch (err) {
      setError(t('settings.general.save.error'));
      console.error('Error saving config:', err);
    } finally {
      setSaving(false);
    }
  };

  const handleDiscard = () => {
    if (!config) return;
    setDraft(cloneDeep(config));
    setDirty(false);
  };

  const resetDisclaimer = async () => {
    if (!config) return;
    updateAndSaveConfig({ disclaimer_acknowledged: false });
  };

  const resetOnboarding = async () => {
    if (!config) return;
    updateAndSaveConfig({ onboarding_acknowledged: false });
  };

  if (loading) {
    return (
      <div className="flex items-center justify-center py-8">
        <Loader2 className="h-8 w-8 animate-spin" />
        <span className="ml-2">{t('settings.general.loading')}</span>
      </div>
    );
  }

  if (!config) {
    return (
      <div className="py-8">
        <Alert variant="destructive">
          <AlertDescription>{t('settings.general.loadError')}</AlertDescription>
        </Alert>
      </div>
    );
  }

  return (
    <div className="space-y-6">
      {error && (
        <Alert variant="destructive">
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      {success && (
        <Alert variant="success">
          <AlertDescription className="font-medium">
            {t('settings.general.save.success')}
          </AlertDescription>
        </Alert>
      )}

      <Card>
        <CardHeader>
          <CardTitle>{t('settings.general.appearance.title')}</CardTitle>
          <CardDescription>
            {t('settings.general.appearance.description')}
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="space-y-2">
            <Label htmlFor="theme">
              {t('settings.general.appearance.theme.label')}
            </Label>
            <Select
              value={draft?.theme}
              onValueChange={(value: ThemeMode) =>
                updateDraft({ theme: value })
              }
            >
              <SelectTrigger id="theme">
                <SelectValue
                  placeholder={t(
                    'settings.general.appearance.theme.placeholder'
                  )}
                />
              </SelectTrigger>
              <SelectContent>
                {Object.values(ThemeMode).map((theme) => (
                  <SelectItem key={theme} value={theme}>
                    {toPrettyCase(theme)}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <p className="text-sm text-muted-foreground">
              {t('settings.general.appearance.theme.helper')}
            </p>
          </div>

          <div className="space-y-2">
            <Label htmlFor="language">
              {t('settings.general.appearance.language.label')}
            </Label>
            <Select
              value={draft?.language}
              onValueChange={(value: UiLanguage) =>
                updateDraft({ language: value })
              }
            >
              <SelectTrigger id="language">
                <SelectValue
                  placeholder={t(
                    'settings.general.appearance.language.placeholder'
                  )}
                />
              </SelectTrigger>
              <SelectContent>
                {languageOptions.map((option) => (
                  <SelectItem key={option.value} value={option.value}>
                    {option.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <p className="text-sm text-muted-foreground">
              {t('settings.general.appearance.language.helper')}
            </p>
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>{t('settings.general.editor.title')}</CardTitle>
          <CardDescription>
            {t('settings.general.editor.description')}
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="space-y-2">
            <Label htmlFor="editor-type">
              {t('settings.general.editor.type.label')}
            </Label>
            <Select
              value={draft?.editor.editor_type}
              onValueChange={(value: EditorType) =>
                updateDraft({
                  editor: { ...draft!.editor, editor_type: value },
                })
              }
            >
              <SelectTrigger id="editor-type">
                <SelectValue
                  placeholder={t('settings.general.editor.type.placeholder')}
                />
              </SelectTrigger>
              <SelectContent>
                {Object.values(EditorType).map((editor) => (
                  <SelectItem key={editor} value={editor}>
                    {toPrettyCase(editor)}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>

            {/* Editor availability status indicator */}
            {draft?.editor.editor_type !== EditorType.CUSTOM && (
              <EditorAvailabilityIndicator availability={editorAvailability} />
            )}

            <p className="text-sm text-muted-foreground">
              {t('settings.general.editor.type.helper')}
            </p>
          </div>

          {draft?.editor.editor_type === EditorType.CUSTOM && (
            <div className="space-y-2">
              <Label htmlFor="custom-command">
                {t('settings.general.editor.customCommand.label')}
              </Label>
              <Input
                id="custom-command"
                placeholder={t(
                  'settings.general.editor.customCommand.placeholder'
                )}
                value={draft?.editor.custom_command || ''}
                onChange={(e) =>
                  updateDraft({
                    editor: {
                      ...draft!.editor,
                      custom_command: e.target.value || null,
                    },
                  })
                }
              />
              <p className="text-sm text-muted-foreground">
                {t('settings.general.editor.customCommand.helper')}
              </p>
            </div>
          )}

          {(draft?.editor.editor_type === EditorType.VS_CODE ||
            draft?.editor.editor_type === EditorType.CURSOR ||
            draft?.editor.editor_type === EditorType.WINDSURF ||
            draft?.editor.editor_type === EditorType.GOOGLE_ANTIGRAVITY ||
            draft?.editor.editor_type === EditorType.ZED ||
            draft?.editor.editor_type === EditorType.TRAE) && (
            <>
              <div className="space-y-2">
                <Label htmlFor="remote-ssh-host">
                  {t('settings.general.editor.remoteSsh.host.label')}
                </Label>
                <Input
                  id="remote-ssh-host"
                  placeholder={t(
                    'settings.general.editor.remoteSsh.host.placeholder'
                  )}
                  value={draft?.editor.remote_ssh_host || ''}
                  onChange={(e) =>
                    updateDraft({
                      editor: {
                        ...draft!.editor,
                        remote_ssh_host: e.target.value || null,
                      },
                    })
                  }
                />
                <p className="text-sm text-muted-foreground">
                  {t('settings.general.editor.remoteSsh.host.helper')}
                </p>
              </div>

              {draft?.editor.remote_ssh_host && (
                <div className="space-y-2">
                  <Label htmlFor="remote-ssh-user">
                    {t('settings.general.editor.remoteSsh.user.label')}
                  </Label>
                  <Input
                    id="remote-ssh-user"
                    placeholder={t(
                      'settings.general.editor.remoteSsh.user.placeholder'
                    )}
                    value={draft?.editor.remote_ssh_user || ''}
                    onChange={(e) =>
                      updateDraft({
                        editor: {
                          ...draft!.editor,
                          remote_ssh_user: e.target.value || null,
                        },
                      })
                    }
                  />
                  <p className="text-sm text-muted-foreground">
                    {t('settings.general.editor.remoteSsh.user.helper')}
                  </p>
                </div>
              )}
            </>
          )}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>{t('settings.general.git.title')}</CardTitle>
          <CardDescription>
            {t('settings.general.git.description')}
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="space-y-2">
            <Label htmlFor="git-branch-prefix">
              {t('settings.general.git.branchPrefix.label')}
            </Label>
            <Input
              id="git-branch-prefix"
              type="text"
              placeholder={t('settings.general.git.branchPrefix.placeholder')}
              value={draft?.git_branch_prefix ?? ''}
              onChange={(e) => {
                const value = e.target.value.trim();
                updateDraft({ git_branch_prefix: value });
                setBranchPrefixError(validateBranchPrefix(value));
              }}
              aria-invalid={!!branchPrefixError}
              className={branchPrefixError ? 'border-destructive' : undefined}
            />
            {branchPrefixError && (
              <p className="text-sm text-destructive">{branchPrefixError}</p>
            )}
            <p className="text-sm text-muted-foreground">
              {t('settings.general.git.branchPrefix.helper')}{' '}
              {draft?.git_branch_prefix ? (
                <>
                  {t('settings.general.git.branchPrefix.preview')}{' '}
                  <code className="text-xs bg-muted px-1 py-0.5 rounded">
                    {t('settings.general.git.branchPrefix.previewWithPrefix', {
                      prefix: draft.git_branch_prefix,
                    })}
                  </code>
                </>
              ) : (
                <>
                  {t('settings.general.git.branchPrefix.preview')}{' '}
                  <code className="text-xs bg-muted px-1 py-0.5 rounded">
                    {t('settings.general.git.branchPrefix.previewNoPrefix')}
                  </code>
                </>
              )}
            </p>
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>{t('settings.general.proxy.title')}</CardTitle>
          <CardDescription>
            {t('settings.general.proxy.description')}
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="space-y-2">
            <Label htmlFor="proxy-http">
              {t('settings.general.proxy.http.label')}
            </Label>
            <Input
              id="proxy-http"
              placeholder={t('settings.general.proxy.http.placeholder')}
              value={draft?.proxy?.http_proxy ?? ''}
              onChange={(e) => {
                const value = e.target.value.trim();
                updateDraft({
                  proxy: {
                    ...draft!.proxy,
                    http_proxy: value || null,
                  },
                });
              }}
            />
            <p className="text-sm text-muted-foreground">
              {t('settings.general.proxy.http.helper')}
            </p>
          </div>

          <div className="space-y-2">
            <Label htmlFor="proxy-https">
              {t('settings.general.proxy.https.label')}
            </Label>
            <Input
              id="proxy-https"
              placeholder={t('settings.general.proxy.https.placeholder')}
              value={draft?.proxy?.https_proxy ?? ''}
              onChange={(e) => {
                const value = e.target.value.trim();
                updateDraft({
                  proxy: {
                    ...draft!.proxy,
                    https_proxy: value || null,
                  },
                });
              }}
            />
            <p className="text-sm text-muted-foreground">
              {t('settings.general.proxy.https.helper')}
            </p>
          </div>

          <div className="space-y-2">
            <Label htmlFor="proxy-no">
              {t('settings.general.proxy.noProxy.label')}
            </Label>
            <Input
              id="proxy-no"
              placeholder={t('settings.general.proxy.noProxy.placeholder')}
              value={draft?.proxy?.no_proxy ?? ''}
              onChange={(e) => {
                const value = e.target.value.trim();
                updateDraft({
                  proxy: {
                    ...draft!.proxy,
                    no_proxy: value || null,
                  },
                });
              }}
            />
            <p className="text-sm text-muted-foreground">
              {t('settings.general.proxy.noProxy.helper')}
            </p>
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>{t('settings.general.pullRequests.title')}</CardTitle>
          <CardDescription>
            {t('settings.general.pullRequests.description')}
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="flex items-center space-x-2">
            <Checkbox
              id="pr-auto-description"
              checked={draft?.pr_auto_description_enabled ?? false}
              onCheckedChange={(checked: boolean) =>
                updateDraft({ pr_auto_description_enabled: checked })
              }
            />
            <div className="space-y-0.5">
              <Label htmlFor="pr-auto-description" className="cursor-pointer">
                {t('settings.general.pullRequests.autoDescription.label')}
              </Label>
              <p className="text-sm text-muted-foreground">
                {t('settings.general.pullRequests.autoDescription.helper')}
              </p>
            </div>
          </div>
          <div className="flex items-center space-x-2">
            <Checkbox
              id="use-custom-prompt"
              checked={draft?.pr_auto_description_prompt != null}
              onCheckedChange={(checked: boolean) => {
                if (checked) {
                  updateDraft({
                    pr_auto_description_prompt: DEFAULT_PR_DESCRIPTION_PROMPT,
                  });
                } else {
                  updateDraft({ pr_auto_description_prompt: null });
                }
              }}
            />
            <Label htmlFor="use-custom-prompt" className="cursor-pointer">
              {t('settings.general.pullRequests.customPrompt.useCustom')}
            </Label>
          </div>
          <div className="space-y-2">
            <textarea
              id="pr-custom-prompt"
              className={`flex min-h-[100px] w-full rounded-md border border-input bg-background px-3 py-2 text-sm ring-offset-background placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 ${
                draft?.pr_auto_description_prompt == null
                  ? 'opacity-50 cursor-not-allowed'
                  : ''
              }`}
              value={
                draft?.pr_auto_description_prompt ??
                DEFAULT_PR_DESCRIPTION_PROMPT
              }
              disabled={draft?.pr_auto_description_prompt == null}
              onChange={(e) =>
                updateDraft({
                  pr_auto_description_prompt: e.target.value,
                })
              }
            />
            <p className="text-sm text-muted-foreground">
              {t('settings.general.pullRequests.customPrompt.helper')}
            </p>
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>{t('settings.general.notifications.title')}</CardTitle>
          <CardDescription>
            {t('settings.general.notifications.description')}
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="flex items-center space-x-2">
            <Checkbox
              id="sound-enabled"
              checked={draft?.notifications.sound_enabled}
              onCheckedChange={(checked: boolean) =>
                updateDraft({
                  notifications: {
                    ...draft!.notifications,
                    sound_enabled: checked,
                  },
                })
              }
            />
            <div className="space-y-0.5">
              <Label htmlFor="sound-enabled" className="cursor-pointer">
                {t('settings.general.notifications.sound.label')}
              </Label>
              <p className="text-sm text-muted-foreground">
                {t('settings.general.notifications.sound.helper')}
              </p>
            </div>
          </div>
          {draft?.notifications.sound_enabled && (
            <div className="ml-6 space-y-2">
              <Label htmlFor="sound-file">
                {t('settings.general.notifications.sound.fileLabel')}
              </Label>
              <div className="flex gap-2">
                <Select
                  value={draft.notifications.sound_file}
                  onValueChange={(value: SoundFile) =>
                    updateDraft({
                      notifications: {
                        ...draft.notifications,
                        sound_file: value,
                      },
                    })
                  }
                >
                  <SelectTrigger id="sound-file" className="flex-1">
                    <SelectValue
                      placeholder={t(
                        'settings.general.notifications.sound.filePlaceholder'
                      )}
                    />
                  </SelectTrigger>
                  <SelectContent>
                    {Object.values(SoundFile).map((soundFile) => (
                      <SelectItem key={soundFile} value={soundFile}>
                        {toPrettyCase(soundFile)}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
                <Button
                  variant="outline"
                  size="sm"
                  onClick={() => playSound(draft.notifications.sound_file)}
                  className="px-3"
                >
                  <Volume2 className="h-4 w-4" />
                </Button>
              </div>
              <p className="text-sm text-muted-foreground">
                {t('settings.general.notifications.sound.fileHelper')}
              </p>
            </div>
          )}
          <div className="flex items-center space-x-2">
            <Checkbox
              id="push-notifications"
              checked={draft?.notifications.push_enabled}
              onCheckedChange={(checked: boolean) =>
                updateDraft({
                  notifications: {
                    ...draft!.notifications,
                    push_enabled: checked,
                  },
                })
              }
            />
            <div className="space-y-0.5">
              <Label htmlFor="push-notifications" className="cursor-pointer">
                {t('settings.general.notifications.push.label')}
              </Label>
              <p className="text-sm text-muted-foreground">
                {t('settings.general.notifications.push.helper')}
              </p>
            </div>
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>{t('settings.general.privacy.title')}</CardTitle>
          <CardDescription>
            {t('settings.general.privacy.description')}
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="flex items-center space-x-2">
            <Checkbox
              id="analytics-enabled"
              checked={draft?.analytics_enabled ?? false}
              onCheckedChange={(checked: boolean) =>
                updateDraft({ analytics_enabled: checked })
              }
            />
            <div className="space-y-0.5">
              <Label htmlFor="analytics-enabled" className="cursor-pointer">
                {t('settings.general.privacy.telemetry.label')}
              </Label>
              <p className="text-sm text-muted-foreground">
                {t('settings.general.privacy.telemetry.helper')}
              </p>
            </div>
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>{t('settings.general.taskTemplates.title')}</CardTitle>
          <CardDescription>
            {t('settings.general.taskTemplates.description')}
          </CardDescription>
        </CardHeader>
        <CardContent>
          <TagManager />
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>{t('settings.general.safety.title')}</CardTitle>
          <CardDescription>
            {t('settings.general.safety.description')}
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="flex items-center justify-between">
            <div>
              <p className="font-medium">
                {t('settings.general.safety.disclaimer.title')}
              </p>
              <p className="text-sm text-muted-foreground">
                {t('settings.general.safety.disclaimer.description')}
              </p>
            </div>
            <Button variant="outline" onClick={resetDisclaimer}>
              {t('settings.general.safety.disclaimer.button')}
            </Button>
          </div>
          <div className="flex items-center justify-between">
            <div>
              <p className="font-medium">
                {t('settings.general.safety.onboarding.title')}
              </p>
              <p className="text-sm text-muted-foreground">
                {t('settings.general.safety.onboarding.description')}
              </p>
            </div>
            <Button variant="outline" onClick={resetOnboarding}>
              {t('settings.general.safety.onboarding.button')}
            </Button>
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>{t('settings.general.beta.title')}</CardTitle>
          <CardDescription>
            {t('settings.general.beta.description')}
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="space-y-3">
            <div className="flex items-center space-x-2">
              <Checkbox
                id="local-network-access"
                checked={draft?.local_network_access ?? false}
                onCheckedChange={(checked: boolean) => {
                  if (checked) {
                    const existingPassword =
                      draft?.local_network_password?.trim();
                    updateDraft({
                      local_network_access: true,
                      local_network_password:
                        existingPassword || generateLocalNetworkPassword(),
                    });
                  } else {
                    updateDraft({ local_network_access: false });
                  }
                }}
              />
              <div className="space-y-0.5">
                <Label
                  htmlFor="local-network-access"
                  className="cursor-pointer"
                >
                  {t('settings.general.beta.localNetworkAccess.label')}
                </Label>
                <p className="text-sm text-muted-foreground">
                  {t('settings.general.beta.localNetworkAccess.helper')}
                </p>
              </div>
            </div>

            {draft?.local_network_access && (
              <div className="ml-6 space-y-2">
                <Label htmlFor="local-network-password">
                  {t('settings.general.beta.localNetworkAccess.password.label')}
                </Label>
                <div className="flex gap-2">
                  <Input
                    id="local-network-password"
                    type="text"
                    placeholder={t(
                      'settings.general.beta.localNetworkAccess.password.placeholder'
                    )}
                    value={draft?.local_network_password ?? ''}
                    onChange={(e) =>
                      updateDraft({
                        local_network_password: e.target.value || null,
                      })
                    }
                  />
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() =>
                      updateDraft({
                        local_network_password: generateLocalNetworkPassword(),
                      })
                    }
                  >
                    {t(
                      'settings.general.beta.localNetworkAccess.password.generate'
                    )}
                  </Button>
                </div>
                {localNetworkPasswordError && (
                  <p className="text-sm text-destructive">
                    {localNetworkPasswordError}
                  </p>
                )}
                <p className="text-sm text-muted-foreground">
                  {t(
                    'settings.general.beta.localNetworkAccess.password.helper'
                  )}
                </p>
              </div>
            )}
          </div>

          <div className="flex items-center space-x-2">
            <Checkbox
              id="commit-reminder"
              checked={draft?.commit_reminder ?? false}
              onCheckedChange={(checked: boolean) =>
                updateDraft({ commit_reminder: checked })
              }
            />
            <div className="space-y-0.5">
              <Label htmlFor="commit-reminder" className="cursor-pointer">
                {t('settings.general.beta.commitReminder.label')}
              </Label>
              <p className="text-sm text-muted-foreground">
                {t('settings.general.beta.commitReminder.helper')}
              </p>
            </div>
          </div>

          <div className="space-y-2">
            <Label htmlFor="power-mode">
              {t('settings.general.beta.powerMode.label')}
            </Label>
            <Select
              value={draft?.power_mode ?? 'SYSTEM_DEFAULT'}
              onValueChange={(value: string) =>
                updateDraft({ power_mode: value as PowerMode })
              }
            >
              <SelectTrigger id="power-mode">
                <SelectValue
                  placeholder={t('settings.general.beta.powerMode.label')}
                />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="SYSTEM_DEFAULT">
                  {t('settings.general.beta.powerMode.options.systemDefault')}
                </SelectItem>
                <SelectItem value="KEEP_AWAKE">
                  {t('settings.general.beta.powerMode.options.keepAwake')}
                </SelectItem>
                <SelectItem value="KEEP_SCREEN_ON">
                  {t('settings.general.beta.powerMode.options.keepScreenOn')}
                </SelectItem>
              </SelectContent>
            </Select>
            <p className="text-sm text-muted-foreground">
              {t(
                `settings.general.beta.powerMode.descriptions.${
                  draft?.power_mode === 'KEEP_SCREEN_ON'
                    ? 'keepScreenOn'
                    : draft?.power_mode === 'KEEP_AWAKE'
                      ? 'keepAwake'
                      : 'systemDefault'
                }`
              )}
            </p>
          </div>

          <div className="flex items-center space-x-2">
            <Checkbox
              id="telegram-enabled"
              checked={draft?.telegram?.enabled ?? false}
              onCheckedChange={(checked: boolean) =>
                updateDraft({
                  telegram: {
                    ...draft!.telegram,
                    enabled: checked,
                  },
                })
              }
            />
            <div className="space-y-0.5">
              <Label htmlFor="telegram-enabled" className="cursor-pointer">
                {t('settings.general.telegram.enabled.label')}
              </Label>
              <p className="text-sm text-muted-foreground">
                {t('settings.general.telegram.enabled.helper')}
              </p>
            </div>
          </div>

          {draft?.telegram?.enabled && (
            <>
              <div className="ml-6 space-y-2">
                <Label htmlFor="telegram-bot-token">
                  {t('settings.general.telegram.botToken.label')}
                </Label>
                <Input
                  id="telegram-bot-token"
                  type="password"
                  value={draft?.telegram?.bot_token ?? ''}
                  onChange={(e) =>
                    updateDraft({
                      telegram: {
                        ...draft!.telegram,
                        bot_token: e.target.value.trim() || null,
                      },
                    })
                  }
                  placeholder={t(
                    'settings.general.telegram.botToken.placeholder'
                  )}
                />
                <p className="text-sm text-muted-foreground">
                  {t('settings.general.telegram.botToken.helper')}
                </p>
              </div>

              <div className="ml-6 space-y-2">
                <Label htmlFor="telegram-chat-id">
                  {t('settings.general.telegram.chatId.label')}
                </Label>
                <Input
                  id="telegram-chat-id"
                  type="number"
                  value={draft?.telegram?.chat_id?.toString() ?? ''}
                  onChange={(e) => {
                    const nextValue = e.target.value.trim();
                    let chatId: bigint | null = null;
                    if (nextValue !== '') {
                      try {
                        chatId = BigInt(nextValue);
                      } catch {
                        // Invalid bigint, keep null
                      }
                    }
                    updateDraft({
                      telegram: {
                        ...draft!.telegram,
                        chat_id: chatId,
                      },
                    });
                  }}
                  placeholder={t(
                    'settings.general.telegram.chatId.placeholder'
                  )}
                />
                <p className="text-sm text-muted-foreground">
                  {t('settings.general.telegram.chatId.helper')}
                </p>
              </div>

              <div className="ml-6 space-y-2">
                <Label htmlFor="telegram-default-executor">
                  {t('settings.general.telegram.defaultExecutor.label')}
                </Label>
                {profiles ? (
                  <Select
                    value={draft?.telegram?.default_executor ?? ''}
                    onValueChange={(value: string) => {
                      const currentMode =
                        draft?.telegram?.default_mode ?? 'DEFAULT';
                      let nextMode = currentMode;
                      const variants = profiles?.[value];
                      if (
                        variants &&
                        currentMode !== 'DEFAULT' &&
                        !variants[currentMode]
                      ) {
                        nextMode = 'DEFAULT';
                      }
                      updateDraft({
                        telegram: {
                          ...draft!.telegram,
                          default_executor: value,
                          default_mode: nextMode,
                        },
                      });
                    }}
                  >
                    <SelectTrigger id="telegram-default-executor">
                      <SelectValue
                        placeholder={t(
                          'settings.general.telegram.defaultExecutor.placeholder'
                        )}
                      />
                    </SelectTrigger>
                    <SelectContent>
                      {Object.keys(profiles)
                        .sort((a, b) => a.localeCompare(b))
                        .map((executor) => (
                          <SelectItem key={executor} value={executor}>
                            {executor}
                          </SelectItem>
                        ))}
                    </SelectContent>
                  </Select>
                ) : (
                  <Input
                    id="telegram-default-executor"
                    value={draft?.telegram?.default_executor ?? ''}
                    onChange={(e) =>
                      updateDraft({
                        telegram: {
                          ...draft!.telegram,
                          default_executor: e.target.value,
                        },
                      })
                    }
                    placeholder={t(
                      'settings.general.telegram.defaultExecutor.placeholder'
                    )}
                  />
                )}
                <p className="text-sm text-muted-foreground">
                  {t('settings.general.telegram.defaultExecutor.helper')}
                </p>
              </div>

              <div className="ml-6 space-y-2">
                <Label htmlFor="telegram-default-mode">
                  {t('settings.general.telegram.defaultMode.label')}
                </Label>
                {profiles ? (
                  <Select
                    value={draft?.telegram?.default_mode ?? 'DEFAULT'}
                    onValueChange={(value: string) =>
                      updateDraft({
                        telegram: {
                          ...draft!.telegram,
                          default_mode: value,
                        },
                      })
                    }
                  >
                    <SelectTrigger id="telegram-default-mode">
                      <SelectValue
                        placeholder={t(
                          'settings.general.telegram.defaultMode.placeholder'
                        )}
                      />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectItem value="DEFAULT">
                        {t('settings.general.telegram.defaultMode.defaultLabel')}
                      </SelectItem>
                      {telegramModeOptions.map((mode) => (
                        <SelectItem key={mode} value={mode}>
                          {mode}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                ) : (
                  <Input
                    id="telegram-default-mode"
                    value={draft?.telegram?.default_mode ?? ''}
                    onChange={(e) =>
                      updateDraft({
                        telegram: {
                          ...draft!.telegram,
                          default_mode: e.target.value,
                        },
                      })
                    }
                    placeholder={t(
                      'settings.general.telegram.defaultMode.placeholder'
                    )}
                  />
                )}
                <p className="text-sm text-muted-foreground">
                  {t('settings.general.telegram.defaultMode.helper')}
                </p>
              </div>
            </>
          )}
        </CardContent>
      </Card>

      {/* Sticky Save Button */}
      <div className="sticky bottom-0 z-10 bg-background/80 backdrop-blur-sm border-t py-4">
        <div className="flex items-center justify-between">
          {hasUnsavedChanges ? (
            <span className="text-sm text-muted-foreground">
              {t('settings.general.save.unsavedChanges')}
            </span>
          ) : (
            <span />
          )}
          <div className="flex gap-2">
            <Button
              variant="outline"
              onClick={handleDiscard}
              disabled={!hasUnsavedChanges || saving}
            >
              {t('settings.general.save.discard')}
            </Button>
            <Button
              onClick={handleSave}
              disabled={
                !hasUnsavedChanges ||
                saving ||
                !!branchPrefixError ||
                !!localNetworkPasswordError
              }
            >
              {saving && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
              {t('settings.general.save.button')}
            </Button>
          </div>
        </div>
      </div>
    </div>
  );
}
