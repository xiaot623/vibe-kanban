import {
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
} from 'react';
import type { ReactNode } from 'react';
import type {
  ActionType,
  ApprovalStatus,
  NormalizedEntry,
  ToolStatus,
} from 'shared/types';
import { Button } from '@/components/ui/button';
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from '@/components/ui/tooltip';
import { approvalsApi } from '@/lib/api';
import { Check, X } from 'lucide-react';
import WYSIWYGEditor from '@/components/ui/wysiwyg';

import { useHotkeysContext } from 'react-hotkeys-hook';
import { TabNavContext } from '@/contexts/TabNavigationContext';
import { useKeyApproveRequest, useKeyDenyApproval, Scope } from '@/keyboard';
import { useProject } from '@/contexts/ProjectContext';
import { useApprovalForm } from '@/contexts/ApprovalFormContext';

const DEFAULT_DENIAL_REASON = 'User denied this tool use request.';
const ASK_USER_QUESTION_TOOL_NAME = 'AskUserQuestion';

type JsonRecord = Record<string, unknown>;

interface AskUserQuestionOption {
  label: string;
  description?: string;
  value: string;
}

interface AskUserQuestionItem {
  id: string;
  header?: string;
  question: string;
  options: AskUserQuestionOption[];
  multiSelectMin?: number;
  multiSelectMax?: number;
  isMultiSelect: boolean;
}

interface AskUserQuestionPayload {
  originalInput: JsonRecord;
  questions: AskUserQuestionItem[];
}

// ---------- Types ----------
interface PendingApprovalEntryProps {
  pendingStatus: Extract<ToolStatus, { status: 'pending_approval' }>;
  executionProcessId?: string;
  toolName: string;
  actionType: ActionType;
  entry: NormalizedEntry;
  children: ReactNode;
}

const isRecord = (value: unknown): value is JsonRecord =>
  typeof value === 'object' && value !== null && !Array.isArray(value);

const readString = (value: unknown): string | undefined => {
  if (typeof value !== 'string') return undefined;
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : undefined;
};

const readPositiveInteger = (value: unknown): number | undefined => {
  if (typeof value !== 'number' || !Number.isFinite(value)) return undefined;
  const intVal = Math.floor(value);
  return intVal > 0 ? intVal : undefined;
};

const parseAskUserQuestionInput = (
  input: JsonRecord
): AskUserQuestionPayload | null => {
  const rawQuestions = input.questions;
  if (!Array.isArray(rawQuestions)) return null;

  const questions: AskUserQuestionItem[] = rawQuestions.map((raw, index) => {
    const record = isRecord(raw) ? raw : {};
    const options = Array.isArray(record.options)
      ? record.options
          .map((rawOption) => {
            if (typeof rawOption === 'string') {
              const label = readString(rawOption);
              if (!label) return null;
              return { label, value: label };
            }
            if (!isRecord(rawOption)) return null;
            const label =
              readString(rawOption.label) ?? readString(rawOption.value);
            if (!label) return null;
            return {
              label,
              value: readString(rawOption.value) ?? label,
              description: readString(rawOption.description),
            };
          })
          .filter((option): option is AskUserQuestionOption => option !== null)
      : [];
    const multiSelectMin = readPositiveInteger(record.multiSelectMin);
    const multiSelectMax = readPositiveInteger(record.multiSelectMax);
    const isMultiSelect =
      options.length > 0 &&
      ((multiSelectMin ?? 1) > 1 || (multiSelectMax ?? 1) > 1);

    return {
      id: readString(record.id) ?? `question_${index + 1}`,
      header: readString(record.header),
      question:
        readString(record.question) ??
        readString(record.header) ??
        `Question ${index + 1}`,
      options,
      multiSelectMin,
      multiSelectMax,
      isMultiSelect,
    };
  });

  return {
    originalInput: input,
    questions,
  };
};

const findAskUserQuestionInput = (
  source: unknown,
  depth = 0
): JsonRecord | null => {
  if (!isRecord(source) || depth > 6) return null;
  if (Array.isArray(source.questions)) return source;

  const nestedKeys = [
    'input',
    'tool_input',
    'arguments',
    'tool_data',
    'action_type',
    'content_block',
  ];
  for (const key of nestedKeys) {
    const nested = findAskUserQuestionInput(source[key], depth + 1);
    if (nested) return nested;
  }

  return null;
};

const extractAskUserQuestionPayload = (
  toolName: string,
  actionType: ActionType,
  entry: NormalizedEntry
): AskUserQuestionPayload | null => {
  if (toolName !== ASK_USER_QUESTION_TOOL_NAME) return null;

  if (actionType.action === 'tool' && isRecord(actionType.arguments)) {
    const fromAction = parseAskUserQuestionInput(actionType.arguments);
    if (fromAction) return fromAction;
  }

  const metadata = (entry as NormalizedEntry & { metadata?: unknown }).metadata;
  const inputFromMetadata = findAskUserQuestionInput(metadata);
  if (inputFromMetadata) {
    return parseAskUserQuestionInput(inputFromMetadata);
  }

  return null;
};

function useApprovalCountdown(
  requestedAt: string | number | Date,
  timeoutAt: string | number | Date,
  paused: boolean
) {
  const totalSeconds = useMemo(() => {
    const total = Math.floor(
      (new Date(timeoutAt).getTime() - new Date(requestedAt).getTime()) / 1000
    );
    return Math.max(1, total);
  }, [requestedAt, timeoutAt]);

  const [timeLeft, setTimeLeft] = useState<number>(() => {
    const remaining = new Date(timeoutAt).getTime() - Date.now();
    return Math.max(0, Math.floor(remaining / 1000));
  });

  useEffect(() => {
    if (paused) return;
    const id = window.setInterval(() => {
      const remaining = new Date(timeoutAt).getTime() - Date.now();
      const next = Math.max(0, Math.floor(remaining / 1000));
      setTimeLeft(next);
      if (next <= 0) window.clearInterval(id);
    }, 1000);

    return () => window.clearInterval(id);
  }, [timeoutAt, paused]);

  const percent = useMemo(
    () =>
      Math.max(0, Math.min(100, Math.round((timeLeft / totalSeconds) * 100))),
    [timeLeft, totalSeconds]
  );

  return { timeLeft, percent };
}

function ActionButtons({
  disabled,
  isResponding,
  onApprove,
  onStartDeny,
}: {
  disabled: boolean;
  isResponding: boolean;
  onApprove: () => void;
  onStartDeny: () => void;
}) {
  return (
    <div className="flex items-center gap-1.5 pr-4">
      <Tooltip>
        <TooltipTrigger asChild>
          <Button
            onClick={onApprove}
            variant="ghost"
            className="h-8 w-8 rounded-full p-0"
            disabled={disabled}
            aria-label={isResponding ? 'Submitting approval' : 'Approve'}
            aria-busy={isResponding}
          >
            <Check className="h-5 w-5" />
          </Button>
        </TooltipTrigger>
        <TooltipContent>
          <p>{isResponding ? 'Submitting…' : 'Approve request'}</p>
        </TooltipContent>
      </Tooltip>

      <Tooltip>
        <TooltipTrigger asChild>
          <Button
            onClick={onStartDeny}
            variant="ghost"
            className="h-8 w-8 rounded-full p-0"
            disabled={disabled}
            aria-label={isResponding ? 'Submitting denial' : 'Deny'}
            aria-busy={isResponding}
          >
            <X className="h-5 w-5" />
          </Button>
        </TooltipTrigger>
        <TooltipContent>
          <p>{isResponding ? 'Submitting…' : 'Provide denial reason'}</p>
        </TooltipContent>
      </Tooltip>
    </div>
  );
}

function DenyReasonForm({
  isResponding,
  value,
  onChange,
  onCancel,
  onSubmit,
  projectId,
}: {
  isResponding: boolean;
  value: string;
  onChange: (v: string) => void;
  onCancel: () => void;
  onSubmit: () => void;
  projectId?: string;
}) {
  return (
    <div className="flex flex-col gap-2 p-4">
      <WYSIWYGEditor
        value={value}
        onChange={onChange}
        placeholder="Let the agent know why this request was denied... Type @ to insert tags or search files."
        disabled={isResponding}
        className="min-h-[80px]"
        projectId={projectId}
        onCmdEnter={onSubmit}
      />
      <div className="flex flex-wrap items-center justify-end gap-2">
        <Button
          variant="ghost"
          size="sm"
          onClick={onCancel}
          disabled={isResponding}
        >
          Cancel
        </Button>
        <Button size="sm" onClick={onSubmit} disabled={isResponding}>
          Deny
        </Button>
      </div>
    </div>
  );
}

function AskUserQuestionForm({
  questions,
  answers,
  errors,
  disabled,
  isResponding,
  onSingleSelect,
  onMultiToggle,
  onTextChange,
  onSubmit,
  onStartDeny,
}: {
  questions: AskUserQuestionItem[];
  answers: Record<string, string[]>;
  errors: Record<string, string>;
  disabled: boolean;
  isResponding: boolean;
  onSingleSelect: (questionId: string, value: string) => void;
  onMultiToggle: (questionId: string, value: string, checked: boolean) => void;
  onTextChange: (questionId: string, value: string) => void;
  onSubmit: () => void;
  onStartDeny: () => void;
}) {
  return (
    <div className="flex flex-col gap-3 p-4">
      {questions.map((question, index) => {
        const selected = answers[question.id] ?? [];
        const error = errors[question.id];
        const min = question.multiSelectMin ?? 1;
        const max = question.multiSelectMax ?? question.options.length;
        return (
          <div key={question.id} className="space-y-2 rounded-md border p-3">
            <div className="space-y-1">
              <div className="text-sm font-medium">
                {question.header ?? `Question ${index + 1}`}
              </div>
              <div className="text-sm text-muted-foreground">
                {question.question}
              </div>
            </div>

            {question.options.length > 0 ? (
              <div className="space-y-2">
                {question.options.map((option) => {
                  const isChecked = selected.includes(option.value);
                  return (
                    <label
                      key={`${question.id}-${option.value}`}
                      className="flex cursor-pointer items-start gap-2 rounded border px-2 py-1.5"
                    >
                      <input
                        type={question.isMultiSelect ? 'checkbox' : 'radio'}
                        name={question.id}
                        value={option.value}
                        checked={isChecked}
                        disabled={disabled}
                        onChange={(event) => {
                          if (question.isMultiSelect) {
                            onMultiToggle(
                              question.id,
                              option.value,
                              event.target.checked
                            );
                          } else {
                            onSingleSelect(question.id, option.value);
                          }
                        }}
                      />
                      <span className="space-y-0.5 text-sm">
                        <span className="block">{option.label}</span>
                        {option.description && (
                          <span className="block text-xs text-muted-foreground">
                            {option.description}
                          </span>
                        )}
                      </span>
                    </label>
                  );
                })}
                {question.isMultiSelect && (
                  <p className="text-xs text-muted-foreground">
                    Select {min}
                    {max !== min ? `-${max}` : ''} option
                    {max !== 1 ? 's' : ''}
                  </p>
                )}
              </div>
            ) : (
              <textarea
                value={selected[0] ?? ''}
                onChange={(event) =>
                  onTextChange(question.id, event.target.value)
                }
                disabled={disabled}
                rows={3}
                className="w-full rounded border px-2 py-1.5 text-sm"
                placeholder="Type your answer..."
              />
            )}

            {error && <p className="text-xs text-red-600">{error}</p>}
          </div>
        );
      })}

      <div className="flex flex-wrap items-center justify-end gap-2">
        <Button
          variant="ghost"
          size="sm"
          onClick={onStartDeny}
          disabled={disabled}
        >
          Deny
        </Button>
        <Button size="sm" onClick={onSubmit} disabled={disabled}>
          {isResponding ? 'Submitting…' : 'Submit answers'}
        </Button>
      </div>
    </div>
  );
}

// ---------- Main Component ----------
const PendingApprovalEntry = ({
  pendingStatus,
  executionProcessId,
  toolName,
  actionType,
  entry,
  children,
}: PendingApprovalEntryProps) => {
  const [isResponding, setIsResponding] = useState(false);
  const [hasResponded, setHasResponded] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [askAnswers, setAskAnswers] = useState<Record<string, string[]>>({});
  const [askErrors, setAskErrors] = useState<Record<string, string>>({});

  const {
    isEnteringReason,
    denyReason,
    setIsEnteringReason,
    setDenyReason,
    clear,
  } = useApprovalForm(pendingStatus.approval_id);

  const { projectId } = useProject();

  const askUserQuestionPayload = useMemo(
    () => extractAskUserQuestionPayload(toolName, actionType, entry),
    [toolName, actionType, entry]
  );
  const isAskUserQuestion = toolName === ASK_USER_QUESTION_TOOL_NAME;
  const askQuestions = useMemo(
    () => askUserQuestionPayload?.questions ?? [],
    [askUserQuestionPayload]
  );

  useEffect(() => {
    setAskAnswers({});
    setAskErrors({});
  }, [pendingStatus.approval_id, askUserQuestionPayload]);

  const { enableScope, disableScope, activeScopes } = useHotkeysContext();
  const tabNav = useContext(TabNavContext);
  const isLogsTabActive = tabNav ? tabNav.activeTab === 'logs' : true;
  const dialogScopeActive = activeScopes.includes(Scope.DIALOG);
  const shouldControlScopes = isLogsTabActive && !dialogScopeActive;
  const approvalsScopeEnabledRef = useRef(false);
  const dialogScopeActiveRef = useRef(dialogScopeActive);

  useEffect(() => {
    dialogScopeActiveRef.current = dialogScopeActive;
  }, [dialogScopeActive]);

  const { timeLeft } = useApprovalCountdown(
    pendingStatus.requested_at,
    pendingStatus.timeout_at,
    hasResponded
  );

  const disabled = isResponding || hasResponded || timeLeft <= 0;

  const shouldEnableApprovalsScope = shouldControlScopes && !disabled;

  useEffect(() => {
    const shouldEnable = shouldEnableApprovalsScope;

    if (shouldEnable && !approvalsScopeEnabledRef.current) {
      enableScope(Scope.APPROVALS);
      disableScope(Scope.KANBAN);
      approvalsScopeEnabledRef.current = true;
    } else if (!shouldEnable && approvalsScopeEnabledRef.current) {
      disableScope(Scope.APPROVALS);
      if (!dialogScopeActive) {
        enableScope(Scope.KANBAN);
      }
      approvalsScopeEnabledRef.current = false;
    }

    return () => {
      if (approvalsScopeEnabledRef.current) {
        disableScope(Scope.APPROVALS);
        if (!dialogScopeActiveRef.current) {
          enableScope(Scope.KANBAN);
        }
        approvalsScopeEnabledRef.current = false;
      }
    };
  }, [
    disableScope,
    enableScope,
    dialogScopeActive,
    shouldEnableApprovalsScope,
  ]);

  const respondWithStatus = useCallback(
    async (status: ApprovalStatus) => {
      if (disabled) return;
      if (!executionProcessId) {
        setError('Missing executionProcessId');
        return;
      }

      setIsResponding(true);
      setError(null);

      try {
        await approvalsApi.respond(pendingStatus.approval_id, {
          execution_process_id: executionProcessId,
          status,
        });
        setHasResponded(true);
        clear();
      } catch (e: unknown) {
        console.error('Approval respond failed:', e);
        const errorMessage =
          e instanceof Error ? e.message : 'Failed to send response';
        setError(errorMessage);
      } finally {
        setIsResponding(false);
      }
    },
    [disabled, executionProcessId, pendingStatus.approval_id, clear]
  );

  const respondApproval = useCallback(
    (approved: boolean, reason?: string) => {
      const status: ApprovalStatus = approved
        ? { status: 'approved' }
        : { status: 'denied', reason };
      void respondWithStatus(status);
    },
    [respondWithStatus]
  );

  const clearQuestionError = useCallback((questionId: string) => {
    setAskErrors((prev) => {
      if (!prev[questionId]) return prev;
      const next = { ...prev };
      delete next[questionId];
      return next;
    });
  }, []);

  const handleSingleSelect = useCallback(
    (questionId: string, value: string) => {
      setAskAnswers((prev) => ({ ...prev, [questionId]: [value] }));
      clearQuestionError(questionId);
    },
    [clearQuestionError]
  );

  const handleMultiToggle = useCallback(
    (questionId: string, value: string, checked: boolean) => {
      setAskAnswers((prev) => {
        const current = prev[questionId] ?? [];
        const next = checked
          ? Array.from(new Set([...current, value]))
          : current.filter((item) => item !== value);
        return { ...prev, [questionId]: next };
      });
      clearQuestionError(questionId);
    },
    [clearQuestionError]
  );

  const handleTextChange = useCallback(
    (questionId: string, value: string) => {
      setAskAnswers((prev) => ({ ...prev, [questionId]: [value] }));
      clearQuestionError(questionId);
    },
    [clearQuestionError]
  );

  const validateAskAnswers = useCallback(() => {
    const nextErrors: Record<string, string> = {};
    const normalizedAnswers: Record<string, string[]> = {};

    for (const question of askQuestions) {
      const rawAnswers = askAnswers[question.id] ?? [];
      const answers = rawAnswers
        .map((value) => value.trim())
        .filter((value) => value.length > 0);
      normalizedAnswers[question.id] = answers;

      if (question.options.length === 0) {
        if (answers.length === 0) {
          nextErrors[question.id] = 'Answer required';
        }
        continue;
      }

      if (question.isMultiSelect) {
        const min = question.multiSelectMin ?? 1;
        const max = question.multiSelectMax ?? question.options.length;
        if (answers.length < min) {
          nextErrors[question.id] =
            `Select at least ${min} option${min > 1 ? 's' : ''}`;
        } else if (answers.length > max) {
          nextErrors[question.id] =
            `Select at most ${max} option${max > 1 ? 's' : ''}`;
        }
      } else if (answers.length !== 1) {
        nextErrors[question.id] = 'Select one option';
      }
    }

    return { nextErrors, normalizedAnswers };
  }, [askAnswers, askQuestions]);

  const handleSubmitAskAnswers = useCallback(() => {
    if (!isAskUserQuestion || disabled) return;

    const { nextErrors, normalizedAnswers } = validateAskAnswers();
    setAskErrors(nextErrors);
    if (Object.keys(nextErrors).length > 0) return;

    const originalInput = askUserQuestionPayload?.originalInput ?? {};
    const status: ApprovalStatus = {
      status: 'provided_input',
      input: {
        ...originalInput,
        answers: normalizedAnswers,
      },
    };
    void respondWithStatus(status);
  }, [
    isAskUserQuestion,
    disabled,
    validateAskAnswers,
    askUserQuestionPayload,
    respondWithStatus,
  ]);

  const handleApprove = useCallback(() => {
    if (isAskUserQuestion) {
      handleSubmitAskAnswers();
      return;
    }
    respondApproval(true);
  }, [isAskUserQuestion, handleSubmitAskAnswers, respondApproval]);
  const handleStartDeny = useCallback(() => {
    if (disabled) return;
    setError(null);
    setIsEnteringReason(true);
  }, [disabled, setIsEnteringReason]);

  const handleCancelDeny = useCallback(() => {
    if (isResponding) return;
    clear();
  }, [isResponding, clear]);

  const handleSubmitDeny = useCallback(() => {
    const trimmed = denyReason.trim();
    respondApproval(false, trimmed || DEFAULT_DENIAL_REASON);
  }, [denyReason, respondApproval]);

  const triggerDeny = useCallback(
    (event?: KeyboardEvent) => {
      if (!isEnteringReason || disabled || hasResponded) return;
      event?.preventDefault();
      handleSubmitDeny();
    },
    [isEnteringReason, disabled, hasResponded, handleSubmitDeny]
  );

  useKeyApproveRequest(handleApprove, {
    scope: Scope.APPROVALS,
    when: () => shouldEnableApprovalsScope && !isEnteringReason,
    preventDefault: true,
  });

  useKeyDenyApproval(triggerDeny, {
    scope: Scope.APPROVALS,
    when: () => shouldEnableApprovalsScope && !hasResponded,
    enableOnFormTags: ['textarea', 'TEXTAREA'],
    preventDefault: true,
  });

  return (
    <div className="relative mt-3">
      <div className="overflow-hidden">
        {children}

        <div className="bg-background px-2 py-1.5 text-xs sm:text-sm">
          <TooltipProvider>
            <div className="flex items-center justify-between gap-1.5 pl-4">
              <div className="flex items-center gap-1.5">
                {!isEnteringReason && (
                  <span className="text-muted-foreground">
                    {isAskUserQuestion
                      ? 'Please answer the questions to continue.'
                      : 'Would you like to approve this?'}
                  </span>
                )}
              </div>
              {!isEnteringReason && !isAskUserQuestion && (
                <ActionButtons
                  disabled={disabled}
                  isResponding={isResponding}
                  onApprove={handleApprove}
                  onStartDeny={handleStartDeny}
                />
              )}
            </div>

            {error && (
              <div
                className="mt-1 text-xs text-red-600"
                role="alert"
                aria-live="polite"
              >
                {error}
              </div>
            )}

            {isEnteringReason && !hasResponded && (
              <DenyReasonForm
                isResponding={isResponding}
                value={denyReason}
                onChange={setDenyReason}
                onCancel={handleCancelDeny}
                onSubmit={handleSubmitDeny}
                projectId={projectId}
              />
            )}

            {!isEnteringReason && !hasResponded && isAskUserQuestion && (
              <AskUserQuestionForm
                questions={askQuestions}
                answers={askAnswers}
                errors={askErrors}
                disabled={disabled}
                isResponding={isResponding}
                onSingleSelect={handleSingleSelect}
                onMultiToggle={handleMultiToggle}
                onTextChange={handleTextChange}
                onSubmit={handleSubmitAskAnswers}
                onStartDeny={handleStartDeny}
              />
            )}
          </TooltipProvider>
        </div>
      </div>
    </div>
  );
};

export default PendingApprovalEntry;
