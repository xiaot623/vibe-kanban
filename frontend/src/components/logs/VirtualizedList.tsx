import {
  DataWithScrollModifier,
  ScrollModifier,
  VirtuosoMessageList,
  VirtuosoMessageListLicense,
  VirtuosoMessageListMethods,
  VirtuosoMessageListProps,
} from '@virtuoso.dev/message-list';
import { useEffect, useMemo, useRef, useState } from 'react';

import DisplayConversationEntry from '../NormalizedConversation/DisplayConversationEntry';
import { useEntries } from '@/contexts/EntriesContext';
import {
  AddEntryType,
  PatchTypeWithKey,
  useConversationHistory,
} from '@/hooks/useConversationHistory';
import { Loader2 } from 'lucide-react';
import {
  ActionType,
  NormalizedEntry,
  TaskWithAttemptStatus,
} from 'shared/types';
import type { WorkspaceWithSession } from '@/types/attempt';
import { ApprovalFormProvider } from '@/contexts/ApprovalFormContext';

interface VirtualizedListProps {
  attempt: WorkspaceWithSession;
  task?: TaskWithAttemptStatus;
}

interface MessageListContext {
  attempt: WorkspaceWithSession;
  task?: TaskWithAttemptStatus;
}

const INITIAL_TOP_ITEM = { index: 'LAST' as const, align: 'end' as const };

const InitialDataScrollModifier: ScrollModifier = {
  type: 'item-location',
  location: INITIAL_TOP_ITEM,
  purgeItemSizes: true,
};

const AutoScrollToBottom: ScrollModifier = {
  type: 'auto-scroll-to-bottom',
  autoScroll: 'smooth',
};

const ItemContent: VirtuosoMessageListProps<
  PatchTypeWithKey,
  MessageListContext
>['ItemContent'] = ({ data, context }) => {
  const attempt = context?.attempt;
  const task = context?.task;

  if (data.type === 'STDOUT') {
    return <p>{data.content}</p>;
  }
  if (data.type === 'STDERR') {
    return <p>{data.content}</p>;
  }
  if (data.type === 'NORMALIZED_ENTRY' && attempt) {
    return (
      <DisplayConversationEntry
        expansionKey={data.patchKey}
        entry={data.content}
        executionProcessId={data.executionProcessId}
        taskAttempt={attempt}
        task={task}
        plainTextFallbackOnMarkdownError={data.plainTextFallbackOnMarkdownError}
      />
    );
  }

  return null;
};

const computeItemKey: VirtuosoMessageListProps<
  PatchTypeWithKey,
  MessageListContext
>['computeItemKey'] = ({ data }) => `l-${data.patchKey}`;

const hasStructuredToolUseData = (actionType: ActionType): boolean => {
  switch (actionType.action) {
    case 'command_run':
      return Boolean(
        actionType.command.trim() ||
          actionType.result?.output?.trim() ||
          actionType.result?.exit_status
      );
    case 'file_edit':
      return Boolean(actionType.path.trim() || actionType.changes.length > 0);
    case 'file_read':
      return Boolean(actionType.path.trim());
    case 'search':
      return Boolean(actionType.query.trim());
    case 'web_fetch':
      return Boolean(actionType.url.trim());
    case 'tool':
      return Boolean(actionType.arguments || actionType.result);
    case 'task_create':
      return Boolean(actionType.description.trim());
    case 'plan_presentation':
      return Boolean(actionType.plan.trim());
    case 'todo_management':
      return (
        actionType.todos.length > 0 || Boolean(actionType.operation.trim())
      );
    case 'other':
      return Boolean(actionType.description.trim());
    default:
      return false;
  }
};

const isRenderableNormalizedEntry = (entry: NormalizedEntry): boolean => {
  if (entry.content.trim() !== '') {
    return true;
  }

  switch (entry.entry_type.type) {
    case 'loading':
    case 'next_action':
    case 'token_usage_info':
      return true;
    case 'tool_use':
      return (
        entry.entry_type.status.status === 'pending_approval' ||
        hasStructuredToolUseData(entry.entry_type.action_type)
      );
    default:
      return false;
  }
};

const filterRenderableEntries = (
  entries: PatchTypeWithKey[]
): PatchTypeWithKey[] => {
  return entries.filter((entry) => {
    if (entry.type !== 'NORMALIZED_ENTRY') {
      return true;
    }
    return isRenderableNormalizedEntry(entry.content);
  });
};

const VirtualizedList = ({ attempt, task }: VirtualizedListProps) => {
  const [channelData, setChannelData] =
    useState<DataWithScrollModifier<PatchTypeWithKey> | null>(null);
  const [loading, setLoading] = useState(true);
  const { setEntries, reset } = useEntries();

  useEffect(() => {
    setLoading(true);
    setChannelData(null);
    reset();
  }, [attempt.id, reset]);

  const onEntriesUpdated = (
    newEntries: PatchTypeWithKey[],
    addType: AddEntryType,
    newLoading: boolean
  ) => {
    const filteredEntries = filterRenderableEntries(newEntries);
    let scrollModifier: ScrollModifier | undefined;

    if (addType === 'initial' || addType === 'historic') {
      scrollModifier = InitialDataScrollModifier;
    } else if (addType === 'running' && !loading) {
      scrollModifier = AutoScrollToBottom;
    }

    setChannelData({ data: filteredEntries, scrollModifier });
    setEntries(filteredEntries);

    if (loading) {
      setLoading(newLoading);
    }
  };

  useConversationHistory({ attempt, onEntriesUpdated });

  const messageListRef = useRef<VirtuosoMessageListMethods | null>(null);
  const messageListContext = useMemo(
    () => ({ attempt, task }),
    [attempt, task]
  );

  return (
    <ApprovalFormProvider>
      <VirtuosoMessageListLicense
        licenseKey={import.meta.env.VITE_PUBLIC_REACT_VIRTUOSO_LICENSE_KEY}
      >
        <VirtuosoMessageList<PatchTypeWithKey, MessageListContext>
          ref={messageListRef}
          className="flex-1"
          data={channelData}
          initialLocation={INITIAL_TOP_ITEM}
          context={messageListContext}
          computeItemKey={computeItemKey}
          ItemContent={ItemContent}
          Header={() => <div className="h-2"></div>}
          Footer={() => <div className="h-2"></div>}
        />
      </VirtuosoMessageListLicense>
      {loading && (
        <div className="float-left top-0 left-0 w-full h-full bg-primary flex flex-col gap-2 justify-center items-center">
          <Loader2 className="h-8 w-8 animate-spin" />
          <p>Loading History</p>
        </div>
      )}
    </ApprovalFormProvider>
  );
};

export default VirtualizedList;
