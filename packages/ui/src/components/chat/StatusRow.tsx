import React from "react";
import { useSessionUIStore } from '@/sync/session-ui-store';
import { cn } from "@/lib/utils";
import { useDirectorySync, useDirectoryStore } from "@/sync/sync-context";
import type { Todo } from "@opencode-ai/sdk/v2/client";

// Compat aliases for old TodoItem shape
type TodoItem = Todo & { id?: string };
type TodoStatus = string;
type TodoPriority = string;
import { useUIStore } from "@/stores/useUIStore";
import { useTodosPersistStore } from "@/stores/useTodosPersistStore";
import { WorkingPlaceholder } from "./message/parts/WorkingPlaceholder";
import { isVSCodeRuntime } from "@/lib/desktop";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { Icon } from "@/components/icon/Icon";
import { useI18n } from "@/lib/i18n";

const STATUS_ROW_CONTAINER_STYLE = { containerType: "inline-size" as const, containerName: "status-row" };

/** Custom event dispatched when a user clicks a todo item to navigate to its source message. */
const CHAT_SCROLL_TO_MESSAGE_EVENT = 'openchamber:chat-scroll-to-message';

const statusConfig: Record<TodoStatus, { textClassName: string }> = {
  in_progress: {
    textClassName: "text-foreground",
  },
  pending: {
    textClassName: "text-foreground",
  },
  completed: {
    textClassName: "text-muted-foreground line-through",
  },
  cancelled: {
    textClassName: "text-muted-foreground line-through",
  },
};

const priorityClassName: Record<TodoPriority, string> = {
  high: "text-[var(--status-warning)]",
  medium: "text-muted-foreground",
  low: "text-muted-foreground/70",
};

const priorityIcon: Record<TodoPriority, React.ReactNode> = {
  high: <Icon name="arrow-up-double" className="h-3.5 w-3.5"  aria-hidden="true"/>,
  medium: <Icon name="arrow-up-s" className="h-3.5 w-3.5"  aria-hidden="true"/>,
  low: <Icon name="arrow-down-s" className="h-3.5 w-3.5"  aria-hidden="true"/>,
};

const statusLabelKey: Record<TodoStatus, string> = {
  in_progress: "chat.statusRow.todo.status.inProgress",
  pending: "chat.statusRow.todo.status.pending",
  completed: "chat.statusRow.todo.status.completed",
  cancelled: "chat.statusRow.todo.status.cancelled",
};

const priorityLabelKey: Record<TodoPriority, string> = {
  high: "chat.statusRow.todo.priority.high",
  medium: "chat.statusRow.todo.priority.medium",
  low: "chat.statusRow.todo.priority.low",
};

interface TodoItemRowProps {
  todo: TodoItem;
  onNavigate?: (todo: TodoItem) => void;
}

const TodoItemRow: React.FC<TodoItemRowProps> = ({ todo, onNavigate }) => {
  const { t } = useI18n();
  const config = statusConfig[todo.status] || statusConfig.pending;
  const statusKey = statusLabelKey[todo.status] ?? statusLabelKey.pending;
  const priorityKey = priorityLabelKey[todo.priority] ?? priorityLabelKey.medium;

  const statusIcon =
    todo.status === "in_progress" ? (
      <Icon name="record-circle" className="h-3.5 w-3.5 text-[var(--status-info)]"  aria-hidden="true"/>
    ) : todo.status === "completed" ? (
      <Icon name="checkbox-circle" className="h-3.5 w-3.5 text-[var(--status-success)]"  aria-hidden="true"/>
    ) : (
      <Icon name="time" className="h-3.5 w-3.5 text-muted-foreground"  aria-hidden="true"/>
    );

  return (
    <div className="flex items-center min-w-0 py-0.5 gap-2">
      <Tooltip>
        <TooltipTrigger asChild>
          <span className="flex-shrink-0">{statusIcon}</span>
        </TooltipTrigger>
        <TooltipContent side="left" sideOffset={6}>
          {t(statusKey as never)}
        </TooltipContent>
      </Tooltip>
      <button
        type="button"
        onClick={() => onNavigate?.(todo)}
        className={cn(
          "flex-1 typography-ui-label text-left",
          config.textClassName,
          onNavigate && "hover:underline focus-visible:underline focus-visible:outline-none"
        )}
      >
        {todo.content}
      </button>
      <Tooltip>
        <TooltipTrigger asChild>
          <span
            className={cn(
              "typography-meta flex items-center justify-center flex-shrink-0 leading-none",
              priorityClassName[todo.priority] ?? priorityClassName.medium
            )}
          >
            {priorityIcon[todo.priority] ?? priorityIcon.medium}
          </span>
        </TooltipTrigger>
        <TooltipContent side="right" sideOffset={6}>
          {t(priorityKey as never)}
        </TooltipContent>
      </Tooltip>
    </div>
  );
};

const EMPTY_TODOS: TodoItem[] = [];

interface StatusRowProps {
  // Working state
  isWorking?: boolean;
  statusText?: string | null;
  isGenericStatus?: boolean;
  isWaitingForPermission?: boolean;
  wasAborted?: boolean;
  abortActive?: boolean;
  retryInfo?: { attempt?: number; next?: number } | null;
  // Abort state (for mobile/vscode)
  showAbort?: boolean;
  onAbort?: () => void;
  // Abort status display
  showAbortStatus?: boolean;
  showAssistantStatus?: boolean;
  showTodos?: boolean;
  agentName?: string;
  leftAccessory?: React.ReactNode;
}

export const StatusRow: React.FC<StatusRowProps> = ({
  isWorking = false,
  statusText = null,
  isGenericStatus,
  isWaitingForPermission,
  wasAborted,
  abortActive,
  retryInfo,
  showAbort,
  onAbort,
  showAbortStatus,
  showAssistantStatus = true,
  showTodos = true,
  agentName,
  leftAccessory,
}) => {
  const { t } = useI18n();
  const [isExpanded, setIsExpanded] = React.useState(true);
  const currentSessionId = useSessionUIStore((state) => state.currentSessionId);
  const liveTodos = useDirectorySync(
    React.useCallback(
      (state) => {
        if (!showTodos || !currentSessionId) return EMPTY_TODOS;
        return state.todo[currentSessionId] ?? EMPTY_TODOS;
      },
      [currentSessionId, showTodos],
    ),
  );
  const persistedSessionTodos = useTodosPersistStore(
    React.useCallback(
      (state) => (showTodos && currentSessionId ? state.sessions[currentSessionId]?.todos : undefined),
      [currentSessionId, showTodos],
    ),
  );
  const todos: TodoItem[] = React.useMemo(() => {
    if (!currentSessionId) return EMPTY_TODOS;
    if (liveTodos.length > 0) return liveTodos;
    return persistedSessionTodos ?? EMPTY_TODOS;
  }, [liveTodos, persistedSessionTodos, currentSessionId]);
  const isMobile = useUIStore((state) => state.isMobile);
  const isCompact = isMobile || isVSCodeRuntime();

  // Filter out cancelled todos for display and keep original order.
  // This prevents items from jumping around when status changes.
  const visibleTodos = React.useMemo(() => {
    return todos.filter((todo) => todo.status !== "cancelled");
  }, [todos]);

  // Find the current active todo (first in_progress, or first pending)
  const activeTodo = React.useMemo(() => {
    return (
      visibleTodos.find((t) => t.status === "in_progress") ||
      visibleTodos.find((t) => t.status === "pending") ||
      null
    );
  }, [visibleTodos]);

  // Calculate progress
  const progress = React.useMemo(() => {
    const total = todos.filter((t) => t.status !== "cancelled").length;
    const completed = todos.filter((t) => t.status === "completed").length;
    return { completed, total };
  }, [todos]);

  const statusSummary = React.useMemo(() => {
    const active = visibleTodos.filter((t) => t.status === "in_progress").length;
    const left = visibleTodos.filter((t) => t.status === "in_progress" || t.status === "pending").length;
    return { active, left };
  }, [visibleTodos]);

  const hasTodoContent = showTodos && todos.length > 0;
  const hasAssistantContent = showAssistantStatus && (
    isWorking ||
    Boolean(wasAborted) ||
    Boolean(showAbortStatus)
  );
  const hasLeftAccessory = Boolean(leftAccessory);
  // Original logic from ChatInput
  const shouldRenderPlaceholder = !showAbortStatus && (wasAborted || !abortActive);

  const hasContent = hasAssistantContent || hasTodoContent || hasLeftAccessory;

  const directoryStore = useDirectoryStore();

  const handleNavigateToTodo = React.useCallback((clickedTodo: TodoItem) => {
    if (!currentSessionId) return;
    const state = directoryStore.getState();
    const messages = state.message[currentSessionId];
    const allParts = state.part;
    if (!messages?.length) return;

    // Two passes, both newest-to-oldest so the latest "Update Todo List"
    // tool output wins:
    //   1. Require status === "in_progress" (matches the "In Progress"
    //      rendered section heading).
    //   2. Fallback: match by content only, any status. Lets the user
    //      locate completed/pending todos instead of silently no-op'ing.
    for (const requireInProgress of [true, false]) {
      for (let i = messages.length - 1; i >= 0; i--) {
        const msg = messages[i];
        const messageId = (msg as Record<string, unknown>).id as string | undefined;
        if (!messageId) continue;
        const parts = allParts[messageId];
        if (!parts?.length) continue;

        for (const part of parts) {
          if (typeof part !== 'object' || !part) continue;
          const p = part as Record<string, unknown>;
          if (p.type !== 'tool') continue;

          const toolState = p.state as Record<string, unknown> | undefined;
          if (!toolState) continue;

          const raw = typeof toolState.output === 'string'
            ? toolState.output.trim()
            : '';
          if (!raw) continue;

          // Try parsing the tool output as a JSON array of {content, status, priority}
          try {
            const parsed = JSON.parse(raw);
            if (Array.isArray(parsed)) {
              const match = parsed.find(
                (t: unknown) =>
                  typeof t === 'object' && t !== null &&
                  typeof (t as Record<string, unknown>).content === 'string' &&
                  (t as Record<string, unknown>).content === clickedTodo.content &&
                  (!requireInProgress ||
                    (t as Record<string, unknown>).status === 'in_progress'),
              );
              if (match) {
                window.dispatchEvent(new CustomEvent(CHAT_SCROLL_TO_MESSAGE_EVENT, {
                  detail: { messageId, sessionId: currentSessionId },
                }));
                return;
              }
            }
          } catch {
            // Not valid JSON — skip
          }
        }
      }
    }
  }, [currentSessionId, directoryStore]);

  const toggleExpanded = () => setIsExpanded((prev) => !prev);
  const todoSummaryLabel = t('chat.statusRow.summary.activeLeft', {
    active: statusSummary.active,
    left: statusSummary.left,
  });

  // Abort button for mobile/vscode
  const abortButton = showAbort && onAbort ? (
    <button
      type="button"
      onClick={onAbort}
      className="flex items-center justify-center h-[1.2rem] w-[1.2rem] text-[var(--status-error)] transition-opacity hover:opacity-80 focus-visible:outline-none flex-shrink-0"
      aria-label={t('chat.statusRow.actions.stopGeneratingAria')}
    >
      <Icon name="close-circle" aria-hidden="true"/>
    </button>
  ) : null;

  // Todo trigger button
  const todoTrigger = hasTodoContent ? (
    <button
      type="button"
      onClick={toggleExpanded}
      className="flex items-center gap-1 flex-shrink-0 text-muted-foreground"
      aria-label={todoSummaryLabel}
      title={todoSummaryLabel}
    >
      {/* Desktop: show task text; Mobile/VSCode: just "Tasks" */}
      {!isCompact && activeTodo ? (
        <span className="status-row__active-todo typography-ui-label text-foreground truncate max-w-[200px]">
          {activeTodo.content}
        </span>
      ) : (
        <span className="typography-ui-label">{t('chat.statusRow.tasksTitle')}</span>
      )}
      <span className="typography-meta flex items-center gap-1 tabular-nums" aria-hidden="true">
        <span className="flex items-center gap-0.5">
          <Icon name="record-circle" className="h-3.5 w-3.5 text-[var(--status-info)]" />
          {statusSummary.active}
        </span>
        <span>·</span>
        <span className="flex items-center gap-0.5">
          <Icon name="time" className="h-3.5 w-3.5" />
          {statusSummary.left}
        </span>
      </span>
      {isExpanded ? (
        <Icon name="arrow-up-s" className="h-3.5 w-3.5" />
      ) : (
        <Icon name="arrow-down-s" className="h-3.5 w-3.5" />
      )}
    </button>
  ) : null;

  // Don't render if nothing to show
  if (!hasContent) {
    return null;
  }

  return (
    <div className={cn("mb-1", !hasLeftAccessory && "chat-column")} style={STATUS_ROW_CONTAINER_STYLE}>
      <div className={cn("flex items-center justify-between py-0.5 gap-2 h-[1.2rem]", hasLeftAccessory && "px-0.5")}>
        {/* Left: Abort status | Working placeholder | leftAccessory */}
        <div className={cn("flex-1 flex items-center min-w-0 gap-2", hasLeftAccessory ? "pl-1.5" : "overflow-x-hidden")}>
          {showAssistantStatus && showAbortStatus ? (
            <div className="flex h-full items-center text-[var(--status-error)] pl-0.5">
              <span className="flex items-center gap-1.5 typography-ui-label">
                <Icon name="close-circle" aria-hidden="true"/>
                {t('chat.statusRow.aborted')}
              </span>
            </div>
          ) : showAssistantStatus && shouldRenderPlaceholder ? (
            <WorkingPlaceholder
              key={currentSessionId ?? "no-session"}
              isWorking={isWorking}
              statusText={statusText}
              isGenericStatus={isGenericStatus}
              isWaitingForPermission={isWaitingForPermission}
              retryInfo={retryInfo}
              agentName={agentName}
            />
          ) : leftAccessory ? (
            leftAccessory
          ) : null}
        </div>

        {/* Right: Abort (mobile only) + Todo */}
        <div className={cn("relative flex items-center gap-2 flex-shrink-0", hasLeftAccessory ? "pr-1.5" : "-mr-3")}>
          {abortButton}
          {todoTrigger}

          {/* Collapsed chip — floated at top-right when popover is hidden */}
          {hasTodoContent && !isExpanded && (
            <button
              type="button"
              onClick={toggleExpanded}
              style={{
                top: "calc(var(--oc-header-height, 48px) + 8px)",
                right: "calc(var(--oc-context-panel-width, 0px) + var(--oc-right-sidebar-width, 0px) + 12px)",
              }}
              className={cn(
                "fixed z-50",
                "flex items-center gap-1.5 h-[1.6rem] px-2 rounded-xl",
                "bg-[var(--surface-elevated)] text-[var(--surface-elevated-foreground)]",
                "shadow-[inset_0_1px_0_0_rgba(255,255,255,0.8),inset_0_0_0_1px_rgba(0,0,0,0.04),0_0_0_1px_rgba(0,0,0,0.10),0_1px_2px_-0.5px_rgba(0,0,0,0.08),0_4px_8px_-2px_rgba(0,0,0,0.08),0_12px_20px_-4px_rgba(0,0,0,0.08)]",
                "dark:shadow-[inset_0_1px_0_0_rgba(255,255,255,0.12),inset_0_0_0_1px_rgba(255,255,255,0.08),0_0_0_1px_rgba(0,0,0,0.36),0_1px_1px_-0.5px_rgba(0,0,0,0.22),0_3px_3px_-1.5px_rgba(0,0,0,0.20),0_6px_6px_-3px_rgba(0,0,0,0.16)]",
                "transition-opacity hover:opacity-90 focus-visible:outline-none focus-visible:opacity-90",
                "animate-in fade-in-0 zoom-in-95 duration-150",
                "typography-ui-label font-medium text-muted-foreground"
              )}
              aria-label={t('chat.statusRow.actions.expandTasksAria')}
              title={t('chat.statusRow.actions.expandTasksAria')}
            >
              <Icon name="checkbox-circle" className="h-3.5 w-3.5 text-[var(--status-success)]" aria-hidden="true" />
              <span>{t('chat.statusRow.tasksTitle')}</span>
              <span className="typography-meta tabular-nums">
                {progress.completed}/{progress.total}
              </span>
              <Icon name="arrow-down-s" className="h-3.5 w-3.5" aria-hidden="true" />
            </button>
          )}

          {/* Popover dropdown — floated at top-right of chat area */}
          {isExpanded && hasTodoContent && (
            <div
              style={{
                maxWidth: "min(28rem, calc(100vw - 4ch))",
                backgroundColor: "var(--surface-elevated)",
                color: "var(--surface-elevated-foreground)",
                top: "calc(var(--oc-header-height, 48px) + 8px)",
                right: "calc(var(--oc-context-panel-width, 0px) + var(--oc-right-sidebar-width, 0px) + 12px)",
              }}
              className={cn(
                "fixed z-50",
                "w-max min-w-[200px] rounded-xl p-1",
                "shadow-[inset_0_1px_0_0_rgba(255,255,255,0.8),inset_0_0_0_1px_rgba(0,0,0,0.04),0_0_0_1px_rgba(0,0,0,0.10),0_1px_2px_-0.5px_rgba(0,0,0,0.08),0_4px_8px_-2px_rgba(0,0,0,0.08),0_12px_20px_-4px_rgba(0,0,0,0.08)]",
                "dark:shadow-[inset_0_1px_0_0_rgba(255,255,255,0.12),inset_0_0_0_1px_rgba(255,255,255,0.08),0_0_0_1px_rgba(0,0,0,0.36),0_1px_1px_-0.5px_rgba(0,0,0,0.22),0_3px_3px_-1.5px_rgba(0,0,0,0.20),0_6px_6px_-3px_rgba(0,0,0,0.16)]",
                "animate-in fade-in-0 zoom-in-95 slide-in-from-top-2",
                "duration-150"
              )}
            >
              {/* Header */}
              <div className="flex items-center gap-1.5 px-2 py-1 typography-ui-label font-medium text-muted-foreground">
                <span>{t('chat.statusRow.tasksTitle')}</span>
                <span className="typography-meta tabular-nums">
                  {progress.completed}/{progress.total}
                </span>
                <button
                  type="button"
                  onClick={toggleExpanded}
                  className="ml-auto flex items-center justify-center h-[1.2rem] w-[1.2rem] rounded text-muted-foreground transition-opacity hover:opacity-80 focus-visible:outline-none focus-visible:opacity-80"
                  aria-label={t('chat.statusRow.actions.collapseTasksAria')}
                  title={t('chat.statusRow.actions.collapseTasksAria')}
                >
                  <Icon name="arrow-up-s" className="h-3.5 w-3.5" aria-hidden="true" />
                </button>
              </div>

              {/* Todo list */}
              <div className="px-1 max-h-[200px] overflow-y-auto">
                {visibleTodos.map((todo, index) => (
                  <TodoItemRow key={todo.id ?? `todo-${index}`} todo={todo} onNavigate={handleNavigateToTodo} />
                ))}
              </div>
            </div>
          )}
        </div>
      </div>
    </div>
  );
};
