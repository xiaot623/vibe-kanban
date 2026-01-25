import { ReactNode, useState } from 'react';
import {
  Group,
  Panel,
  Separator,
  useDefaultLayout,
  type PanelSize,
} from 'react-resizable-panels';
import { AnimatePresence, motion } from 'framer-motion';
import { cn } from '@/lib/utils';
import { Maximize2, Minimize2 } from 'lucide-react';

export type LayoutMode = 'diffs' | null;

interface TasksLayoutProps {
  kanban: ReactNode;
  attempt: ReactNode;
  aux: ReactNode;
  isPanelOpen: boolean;
  mode: LayoutMode;
  isMobile?: boolean;
  rightHeader?: ReactNode;
  isFullscreen?: boolean;
  onFullscreenChange?: (isFullscreen: boolean) => void;
}

const MIN_PANEL_SIZE = 20; // percentage (0-100)
const COLLAPSED_SIZE = 0; // percentage (0-100)

/**
 * FullscreenToggle - Button to toggle between fullscreen and split view.
 */
function FullscreenToggle({
  isFullscreen,
  onToggle,
}: {
  isFullscreen: boolean;
  onToggle: () => void;
}) {
  return (
    <button
      onClick={onToggle}
      className={cn(
        'flex items-center justify-center w-8 h-8 rounded',
        'text-muted-foreground hover:text-foreground hover:bg-muted',
        'transition-colors focus:outline-none focus-visible:ring-2 focus-visible:ring-ring'
      )}
      aria-label={isFullscreen ? 'Exit fullscreen' : 'Enter fullscreen'}
      title={isFullscreen ? 'Exit fullscreen' : 'Enter fullscreen'}
    >
      {isFullscreen ? (
        <Minimize2 className="w-4 h-4" />
      ) : (
        <Maximize2 className="w-4 h-4" />
      )}
    </button>
  );
}

/**
 * AuxRouter - Handles nested AnimatePresence for diffs transitions.
 */
function AuxRouter({ mode, aux }: { mode: LayoutMode; aux: ReactNode }) {
  return (
    <AnimatePresence initial={false} mode="popLayout">
      {mode && (
        <motion.div
          key={mode}
          initial={{ opacity: 0 }}
          animate={{ opacity: 1 }}
          exit={{ opacity: 0 }}
          transition={{ duration: 0.2, ease: [0.2, 0, 0, 1] }}
          className="h-full min-h-0"
        >
          {aux}
        </motion.div>
      )}
    </AnimatePresence>
  );
}

/**
 * FullscreenView - Mobile-like fullscreen view for desktop.
 * Shows attempt or aux content one at a time, with a toggle to exit fullscreen.
 */
function FullscreenView({
  attempt,
  aux,
  mode,
  rightHeader,
  onFullscreenToggle,
}: {
  attempt: ReactNode;
  aux: ReactNode;
  mode: LayoutMode;
  rightHeader?: ReactNode;
  onFullscreenToggle: () => void;
}) {
  const showAux = mode !== null;

  return (
    <div className="h-full min-h-0 flex flex-col">
      {(rightHeader || true) && (
        <div className="shrink-0 sticky top-0 z-20 bg-background border-b flex items-center">
          <div className="flex-1">{rightHeader}</div>
          <div className="px-2">
            <FullscreenToggle isFullscreen={true} onToggle={onFullscreenToggle} />
          </div>
        </div>
      )}
      <div className="flex-1 min-h-0">
        {showAux ? <AuxRouter mode={mode} aux={aux} /> : attempt}
      </div>
    </div>
  );
}

/**
 * RightWorkArea - Contains header and Attempt/Aux content.
 * Shows just Attempt when mode === null, or Attempt | Aux split when mode !== null.
 */
function RightWorkArea({
  attempt,
  aux,
  mode,
  rightHeader,
  onFullscreenToggle,
  showFullscreenToggle,
}: {
  attempt: ReactNode;
  aux: ReactNode;
  mode: LayoutMode;
  rightHeader?: ReactNode;
  onFullscreenToggle?: () => void;
  showFullscreenToggle?: boolean;
}) {
  const { defaultLayout, onLayoutChange } = useDefaultLayout({
    groupId: 'tasksLayout-attemptAux',
    storage: localStorage,
  });
  const [isAttemptCollapsed, setIsAttemptCollapsed] = useState(false);

  const handleAttemptResize = (size: PanelSize) => {
    setIsAttemptCollapsed(size.asPercentage === COLLAPSED_SIZE);
  };

  return (
    <div className="h-full min-h-0 flex flex-col">
      {(rightHeader || showFullscreenToggle) && (
        <div className="shrink-0 sticky top-0 z-20 bg-background border-b flex items-center">
          <div className="flex-1">{rightHeader}</div>
          {showFullscreenToggle && onFullscreenToggle && (
            <div className="px-2">
              <FullscreenToggle
                isFullscreen={false}
                onToggle={onFullscreenToggle}
              />
            </div>
          )}
        </div>
      )}
      <div className="flex-1 min-h-0">
        {mode === null ? (
          attempt
        ) : (
          <Group
            orientation="horizontal"
            className="h-full min-h-0"
            defaultLayout={defaultLayout}
            onLayoutChange={onLayoutChange}
          >
            <Panel
              id="attempt"
              defaultSize={34}
              minSize={MIN_PANEL_SIZE}
              collapsible
              collapsedSize={COLLAPSED_SIZE}
              onResize={handleAttemptResize}
              className="min-w-0 min-h-0 overflow-hidden"
              role="region"
              aria-label="Details"
            >
              {attempt}
            </Panel>

            <Separator
              id="handle-aa"
              className={cn(
                'relative z-30 bg-border cursor-col-resize group touch-none',
                'focus:outline-none focus-visible:ring-2 focus-visible:ring-ring/60',
                'focus-visible:ring-offset-1 focus-visible:ring-offset-background',
                'transition-all',
                isAttemptCollapsed ? 'w-6' : 'w-1'
              )}
              aria-label="Resize panels"
            >
              <div className="pointer-events-none absolute inset-y-0 left-1/2 -translate-x-1/2 w-px bg-border" />
              <div className="pointer-events-none absolute top-1/2 left-1/2 -translate-x-1/2 -translate-y-1/2 flex flex-col items-center gap-1 bg-muted/90 border border-border rounded-full px-1.5 py-3 opacity-70 group-hover:opacity-100 group-focus:opacity-100 transition-opacity shadow-sm">
                <span className="w-1 h-1 rounded-full bg-muted-foreground" />
                <span className="w-1 h-1 rounded-full bg-muted-foreground" />
                <span className="w-1 h-1 rounded-full bg-muted-foreground" />
              </div>
            </Separator>

            <Panel
              id="aux"
              defaultSize={66}
              minSize={MIN_PANEL_SIZE}
              className="min-w-0 min-h-0 overflow-hidden"
              role="region"
              aria-label="Diffs"
            >
              <AuxRouter mode={mode} aux={aux} />
            </Panel>
          </Group>
        )}
      </div>
    </div>
  );
}

/**
 * DesktopSimple - Conditionally renders layout based on mode and fullscreen state.
 * When fullscreen: Shows mobile-like single view (attempt or aux)
 * When mode !== null: Hides Kanban, shows Attempt | Aux split
 * Otherwise: Shows Kanban | Attempt split
 */
function DesktopSimple({
  kanban,
  attempt,
  aux,
  mode,
  rightHeader,
  isFullscreen,
  onFullscreenToggle,
}: {
  kanban: ReactNode;
  attempt: ReactNode;
  aux: ReactNode;
  mode: LayoutMode;
  rightHeader?: ReactNode;
  isFullscreen?: boolean;
  onFullscreenToggle?: () => void;
}) {
  const { defaultLayout, onLayoutChange } = useDefaultLayout({
    groupId: 'tasksLayout-kanbanAttempt',
    storage: localStorage,
  });
  const [isKanbanCollapsed, setIsKanbanCollapsed] = useState(false);

  const handleKanbanResize = (size: PanelSize) => {
    setIsKanbanCollapsed(size.asPercentage === COLLAPSED_SIZE);
  };

  // When in fullscreen mode, render mobile-like single view
  if (isFullscreen && onFullscreenToggle) {
    return (
      <FullscreenView
        attempt={attempt}
        aux={aux}
        mode={mode}
        rightHeader={rightHeader}
        onFullscreenToggle={onFullscreenToggle}
      />
    );
  }

  // When diffs is open, hide Kanban entirely and render only RightWorkArea
  if (mode !== null) {
    return (
      <RightWorkArea
        attempt={attempt}
        aux={aux}
        mode={mode}
        rightHeader={rightHeader}
        onFullscreenToggle={onFullscreenToggle}
        showFullscreenToggle={true}
      />
    );
  }

  // When only viewing attempt logs, show Kanban | Attempt (no aux)
  return (
    <Group
      orientation="horizontal"
      className="h-full min-h-0"
      defaultLayout={defaultLayout}
      onLayoutChange={onLayoutChange}
    >
      <Panel
        id="kanban"
        defaultSize={66}
        minSize={MIN_PANEL_SIZE}
        collapsible
        collapsedSize={COLLAPSED_SIZE}
        onResize={handleKanbanResize}
        className="min-w-0 min-h-0 overflow-hidden"
        role="region"
        aria-label="Kanban board"
      >
        {kanban}
      </Panel>

      <Separator
        id="handle-kr"
        className={cn(
          'relative z-30 bg-border cursor-col-resize group touch-none',
          'focus:outline-none focus-visible:ring-2 focus-visible:ring-ring/60',
          'focus-visible:ring-offset-1 focus-visible:ring-offset-background',
          'transition-all',
          isKanbanCollapsed ? 'w-6' : 'w-1'
        )}
        aria-label="Resize panels"
      >
        <div className="pointer-events-none absolute inset-y-0 left-1/2 -translate-x-1/2 w-px bg-border" />
        <div className="pointer-events-none absolute top-1/2 left-1/2 -translate-x-1/2 -translate-y-1/2 flex flex-col items-center gap-1 bg-muted/90 border border-border rounded-full px-1.5 py-3 opacity-70 group-hover:opacity-100 group-focus:opacity-100 transition-opacity shadow-sm">
          <span className="w-1 h-1 rounded-full bg-muted-foreground" />
          <span className="w-1 h-1 rounded-full bg-muted-foreground" />
          <span className="w-1 h-1 rounded-full bg-muted-foreground" />
        </div>
      </Separator>

      <Panel
        id="right"
        defaultSize={34}
        minSize={MIN_PANEL_SIZE}
        className="min-w-0 min-h-0 overflow-hidden"
      >
        <RightWorkArea
          attempt={attempt}
          aux={aux}
          mode={mode}
          rightHeader={rightHeader}
          onFullscreenToggle={onFullscreenToggle}
          showFullscreenToggle={true}
        />
      </Panel>
    </Group>
  );
}

export function TasksLayout({
  kanban,
  attempt,
  aux,
  isPanelOpen,
  mode,
  isMobile = false,
  rightHeader,
  isFullscreen = false,
  onFullscreenChange,
}: TasksLayoutProps) {
  const desktopKey = isPanelOpen
    ? isFullscreen
      ? 'desktop-fullscreen'
      : 'desktop-with-panel'
    : 'kanban-only';

  const handleFullscreenToggle = () => {
    onFullscreenChange?.(!isFullscreen);
  };

  if (isMobile) {
    // When panel is open and mode is set, show aux content (diffs)
    // Otherwise show attempt content
    const showAux = isPanelOpen && mode !== null;

    return (
      <div className="h-full min-h-0 flex flex-col">
        {/* Header is visible when panel is open */}
        {isPanelOpen && rightHeader && (
          <div className="shrink-0 sticky top-0 z-20 bg-background border-b">
            {rightHeader}
          </div>
        )}

        <div className="flex-1 min-h-0">
          {!isPanelOpen ? (
            kanban
          ) : showAux ? (
            <AuxRouter mode={mode} aux={aux} />
          ) : (
            attempt
          )}
        </div>
      </div>
    );
  }

  let desktopNode: ReactNode;

  if (!isPanelOpen) {
    desktopNode = (
      <div
        className="h-full min-h-0 min-w-0 overflow-hidden"
        role="region"
        aria-label="Kanban board"
      >
        {kanban}
      </div>
    );
  } else {
    desktopNode = (
      <DesktopSimple
        kanban={kanban}
        attempt={attempt}
        aux={aux}
        mode={mode}
        rightHeader={rightHeader}
        isFullscreen={isFullscreen}
        onFullscreenToggle={handleFullscreenToggle}
      />
    );
  }

  return (
    <AnimatePresence initial={false} mode="popLayout">
      <motion.div
        key={desktopKey}
        className="h-full min-h-0"
        initial={{ opacity: 0 }}
        animate={{ opacity: 1 }}
        exit={{ opacity: 0 }}
        transition={{ duration: 0.3, ease: [0.2, 0, 0, 1] }}
      >
        {desktopNode}
      </motion.div>
    </AnimatePresence>
  );
}
