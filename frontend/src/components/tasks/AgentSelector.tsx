import { Bot, ChevronDown } from 'lucide-react';
import { Button } from '@/components/ui/button';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { Label } from '@/components/ui/label';
import { cn } from '@/lib/utils';
import type { ExecutorProfileId, BaseCodingAgent } from 'shared/types';

interface AgentSelectorProps {
  profiles: Record<string, Record<string, unknown>> | null;
  selectedExecutorProfile: ExecutorProfileId | null;
  onChange: (profile: ExecutorProfileId) => void;
  disabled?: boolean;
  className?: string;
  showLabel?: boolean;
}

export function AgentSelector({
  profiles,
  selectedExecutorProfile,
  onChange,
  disabled,
  className = '',
  showLabel = false,
}: AgentSelectorProps) {
  const agents = profiles
    ? (Object.keys(profiles).sort() as BaseCodingAgent[])
    : [];
  const selectedAgent = selectedExecutorProfile?.executor;

  if (!profiles) return null;

  return (
    <div className={showLabel ? 'flex-1' : 'shrink-0'}>
      {showLabel && (
        <Label htmlFor="executor-profile" className="text-sm font-medium">
          Agent
        </Label>
      )}
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <Button
            variant="secondary"
            size="sm"
            className={cn(
              'px-2 flex items-center justify-between transition-all',
              showLabel && 'w-full mt-1.5',
              className
            )}
            disabled={disabled}
            aria-label="Select agent"
          >
            <Bot className="h-3 w-3 mr-1 flex-shrink-0" />
            <span className="text-xs truncate flex-1 text-left">
              {selectedAgent || 'Agent'}
            </span>
            <ChevronDown className="h-3 w-3 ml-1 flex-shrink-0" />
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent className="w-60">
          {agents.length === 0 ? (
            <div className="p-2 text-sm text-muted-foreground text-center">
              No agents available
            </div>
          ) : (
            agents.map((agent) => (
              <DropdownMenuItem
                key={agent}
                onClick={() => {
                  onChange({
                    executor: agent,
                    variant: null,
                  });
                }}
                className={selectedAgent === agent ? 'bg-accent' : ''}
              >
                {agent}
              </DropdownMenuItem>
            ))
          )}
        </DropdownMenuContent>
      </DropdownMenu>
    </div>
  );
}
