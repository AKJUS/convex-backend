import { useContext, useEffect, useState } from "react";
import { useDebounce, useLocalStorage } from "react-use";
import { TextInput } from "@ui/TextInput";
import { LogList } from "@common/features/logs/components/LogList";
import { LogToolbar } from "@common/features/logs/components/LogToolbar";
import { filterLogs } from "@common/features/logs/lib/filterLogs";
import { displayNameToIdentifier } from "@common/lib/functions/FunctionsProvider";
import { functionIdentifierValue } from "@common/lib/functions/generateFileTree";
import { UdfLog, useLogs } from "@common/lib/useLogs";
import { ModuleFunction } from "@common/lib/functions/types";
import { Nent } from "@common/lib/useNents";
import { Button } from "@ui/Button";
import { ExternalLinkIcon } from "@radix-ui/react-icons";
import { useRouter } from "next/router";
import { DeploymentInfoContext } from "@common/lib/deploymentContext";
import { MultiSelectValue } from "@ui/MultiSelectCombobox";

type LogLevel = "success" | "failure" | "DEBUG" | "INFO" | "WARN" | "ERROR";

const DEFAULT_LOG_LEVELS: LogLevel[] = [
  "success",
  "failure",
  "DEBUG",
  "INFO",
  "WARN",
  "ERROR",
];

interface FunctionLogsProps {
  currentOpenFunction: ModuleFunction;
  selectedNent?: Nent;
}

export function FunctionLogs({
  currentOpenFunction,
  selectedNent,
}: FunctionLogsProps) {
  const functionId = functionIdentifierValue(
    displayNameToIdentifier(currentOpenFunction.displayName),
    selectedNent?.path,
  );

  const [logs, setLogs] = useState<UdfLog[]>([]);
  const [paused, setPaused] = useState<number>(0);
  const [manuallyPaused, setManuallyPaused] = useState(false);

  // Store filter and selected levels in local storage, scoped to the function
  const [filter, setFilter] = useLocalStorage<string>(
    `function-logs/${functionId}/filter`,
    "",
  );
  const [innerFilter, setInnerFilter] = useState(filter ?? "");
  const [selectedLevelsStorage, setSelectedLevelsStorage] = useLocalStorage<
    LogLevel[] | "all"
  >(`function-logs/${functionId}/selected-levels`, "all");

  // Convert the stored levels to MultiSelectValue type
  const selectedLevels: MultiSelectValue =
    selectedLevelsStorage === "all"
      ? "all"
      : ((selectedLevelsStorage || []) as string[]);
  const setSelectedLevels = (newLevels: MultiSelectValue) => {
    // Store in localStorage
    setSelectedLevelsStorage(
      newLevels === "all" ? "all" : (newLevels as LogLevel[]),
    );
  };

  useDebounce(
    () => {
      setFilter(innerFilter);
    },
    200,
    [innerFilter],
  );

  // Sync innerFilter when filter changes externally (e.g., from side panel)
  useEffect(() => {
    if (filter !== undefined && filter !== innerFilter) {
      setInnerFilter(filter);
    }
  }, [filter, innerFilter]);

  const onPause = (p: boolean) => {
    const now = new Date().getTime();
    setPaused(p ? now : 0);
  };

  const logsConnectivityCallbacks = {
    onReconnected: () => {},
    onDisconnected: () => {},
  };

  const receiveLogs = (entries: UdfLog[]) => {
    setLogs((prev) => {
      const newLogs = filterLogs(
        {
          logTypes: DEFAULT_LOG_LEVELS,
          functions: [functionId],
          selectedFunctions: [functionId],
          selectedNents: selectedNent ? [selectedNent.path] : "all",
          filter: "",
        },
        entries,
      );
      if (!newLogs) {
        return prev;
      }
      return [...prev, ...newLogs].slice(
        Math.max(prev.length + entries.length - 10000, 0),
        prev.length + entries.length,
      );
    });
  };

  useLogs(logsConnectivityCallbacks, receiveLogs, paused > 0 || manuallyPaused);

  const router = useRouter();
  const { deploymentsURI } = useContext(DeploymentInfoContext);

  return (
    <div className="flex h-full w-full max-w-full min-w-0 grow flex-col gap-2 overflow-hidden">
      <div className="flex min-w-0 shrink-0">
        <LogToolbar
          functions={[functionId]}
          selectedFunctions={[functionId]}
          setSelectedFunctions={(_functions) => {}}
          selectedLevels={selectedLevels}
          setSelectedLevels={setSelectedLevels}
          selectedNents={selectedNent ? [selectedNent.path] : "all"}
          setSelectedNents={() => {}}
          hideFunctionFilter
          firstItem={
            <div className="flex min-w-0 grow gap-2">
              <Button
                variant="neutral"
                size="sm"
                icon={<ExternalLinkIcon />}
                href={`${deploymentsURI}/logs${router.query.component ? `?component=${router.query.component}` : ""}`}
              >
                View all Logs
              </Button>
              <TextInput
                id="Search logs"
                placeholder="Filter logs..."
                value={innerFilter}
                onChange={(e) => setInnerFilter(e.target.value)}
                type="search"
              />
            </div>
          }
        />
      </div>
      <div className="flex min-h-0 min-w-0 grow">
        <LogList
          nents={selectedNent ? [selectedNent] : []}
          logs={logs}
          filteredLogs={filterLogs(
            {
              logTypes: selectedLevels,
              functions: [functionId],
              selectedFunctions: [functionId],
              selectedNents: selectedNent ? [selectedNent.path] : "all",
              filter: filter ?? "",
            },
            logs,
          )}
          deploymentAuditLogs={[]}
          filter={filter ?? ""}
          setFilter={setFilter}
          clearedLogs={[]}
          setClearedLogs={() => {}}
          paused={paused > 0 || manuallyPaused}
          setPaused={onPause}
          setManuallyPaused={(p) => {
            onPause(p);
            setManuallyPaused(p);
          }}
        />
      </div>
    </div>
  );
}
