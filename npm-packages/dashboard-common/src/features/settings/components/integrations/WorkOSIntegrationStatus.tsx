import { AuthIntegration } from "@common/lib/integrationHelpers";
import { useQuery } from "convex/react";
import udfs from "@common/udfs";
import { useMemo } from "react";

export function WorkOSIntegrationStatus({
  integration,
}: {
  integration: AuthIntegration;
}) {
  const environmentVariables = useQuery(
    udfs.listEnvironmentVariables.default,
    {},
  );

  const workosEnvVars = useMemo(() => {
    if (!environmentVariables) return null;

    const clientId = environmentVariables.find(
      (envVar) => envVar.name === "WORKOS_CLIENT_ID",
    )?.value;

    return {
      clientId: clientId || null,
    };
  }, [environmentVariables]);

  if (!integration.existing) {
    return null;
  }

  // Check if environment variables match
  const clientIdMatches =
    workosEnvVars?.clientId &&
    workosEnvVars.clientId === integration.existing.workosClientId;

  const isConnected = clientIdMatches;

  return (
    <div className="flex flex-col gap-2">
      <div className="flex flex-col items-center">
        <div className="ml-auto flex gap-2">
          {isConnected ? (
            <div className="text-xs text-content-success">Active</div>
          ) : (
            <div className="text-xs text-content-warning">Misconfigured</div>
          )}
        </div>
      </div>
    </div>
  );
}
