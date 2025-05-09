import { useContext, useEffect, useState } from "react";
import { useQuery } from "convex/react";
import udfs from "@common/udfs";
import { useRouter } from "next/router";
import Link from "next/link";
import { InfoCircledIcon } from "@radix-ui/react-icons";
import { EnvironmentVariable } from "system-udfs/convex/_system/frontend/common";
import {
  EnvironmentVariables,
  BaseEnvironmentVariable,
} from "@common/features/settings/components/EnvironmentVariables";
import { useUpdateEnvVars } from "@common/features/settings/lib/api";
import { DeploymentInfoContext } from "@common/lib/deploymentContext";
import { Button } from "@ui/Button";
import { Sheet } from "@ui/Sheet";
import { ProjectEnvVarConfig } from "@common/features/settings/lib/types";

export function DeploymentEnvironmentVariables() {
  const { useCurrentDeployment, useHasProjectAdminPermissions, projectsURI } =
    useContext(DeploymentInfoContext);
  const deployment = useCurrentDeployment();
  const hasAdminPermissions = useHasProjectAdminPermissions(
    deployment?.projectId,
  );
  const canManageEnvironmentVariables =
    deployment?.deploymentType !== "prod" || hasAdminPermissions;
  const environmentVariables: undefined | Array<EnvironmentVariable> = useQuery(
    udfs.listEnvironmentVariables.default,
    {},
  );
  const updateEnvironmentVariables = useUpdateEnvVars();

  const diff = useEnvironmentVariablesDiff();

  const projectSettingsURI = `${projectsURI}/settings`;

  const requestedEnvVars = useRequestedEnvVars();

  const [initialValues, setInitialValues] =
    useState<BaseEnvironmentVariable[]>(requestedEnvVars);

  const renderEnvironmentVariableDiffCallout = () => {
    if (diff.status !== "different") {
      return;
    }

    return (
      <div className="flex items-center justify-between rounded-md border px-3 py-2">
        <div className="flex items-center gap-2">
          <InfoCircledIcon />
          <p className="flex-1">
            This deployment has different environment variables from the{" "}
            <Link
              className="text-content-link underline"
              href={projectSettingsURI}
            >
              project defaults.
            </Link>
          </p>
        </div>
        <Button
          variant="neutral"
          size="sm"
          className="float-right"
          onClick={() => {
            const valuesFromProject = Array.from(diff.projectEnvVariables).map(
              ([name, value]) => ({
                name,
                value,
              }),
            );
            setInitialValues([...initialValues, ...valuesFromProject]);
          }}
        >
          Use project defaults
        </Button>
      </div>
    );
  };

  return (
    <Sheet className="flex flex-col gap-4 text-sm">
      <h3>Environment Variables</h3>
      <p className="text-sm text-content-primary">
        View and configure environment variables for your deployment.
      </p>
      <EnvironmentVariables
        hasAdminPermissions={canManageEnvironmentVariables}
        environmentVariables={environmentVariables}
        updateEnvironmentVariables={async (
          creations,
          modifications,
          deletions,
        ) => {
          await updateEnvironmentVariables([
            ...deletions.map(({ name }) => ({
              name,
              value: null,
            })),
            ...modifications.flatMap(({ oldEnvVar, newEnvVar }) =>
              oldEnvVar.name === newEnvVar.name
                ? [
                    {
                      name: newEnvVar.name,
                      value: newEnvVar.value,
                    },
                  ]
                : [
                    {
                      name: oldEnvVar.name,
                      value: null,
                    },
                    {
                      name: newEnvVar.name,
                      value: newEnvVar.value,
                    },
                  ],
            ),
            ...creations.map(({ name, value }) => ({
              name,
              value,
            })),
          ]);
          setInitialValues([]);
        }}
        initialFormValues={initialValues}
      />
      {renderEnvironmentVariableDiffCallout()}
    </Sheet>
  );
}

type EnvironmentVariableDiff =
  | {
      status: "same";
    }
  | { status: "loading" }
  | { status: "different"; projectEnvVariables: Map<string, string> };

// Split out for testing
export const diffEnvironmentVariables = (
  projectEnvVariables: { configs: ProjectEnvVarConfig[] },
  deploymentEnvVariables: EnvironmentVariable[],
  deploymentType: "dev" | "preview" | "prod",
): EnvironmentVariableDiff => {
  const deploymentEnvVarMap = new Map(
    deploymentEnvVariables.map((e) => [e.name, e.value]),
  );
  const projectEnvVariableArray: [string, string][] =
    projectEnvVariables.configs
      .filter((config) => config.deploymentTypes.includes(deploymentType))
      .map((config) => [config.name, config.value]);
  const projectEnvVariableMap = new Map(projectEnvVariableArray);
  for (const [name, value] of projectEnvVariableMap) {
    if (deploymentEnvVarMap.get(name) !== value) {
      return {
        status: "different",
        projectEnvVariables: projectEnvVariableMap,
      };
    }
  }
  return {
    status: "same",
  };
};

function useEnvironmentVariablesDiff(): EnvironmentVariableDiff {
  const environmentVariables: undefined | Array<EnvironmentVariable> = useQuery(
    udfs.listEnvironmentVariables.default,
    {},
  );
  const {
    useCurrentProject,
    useCurrentDeployment,
    useProjectEnvironmentVariables,
  } = useContext(DeploymentInfoContext);
  const projectId = useCurrentProject()?.id;
  const deploymentType = useCurrentDeployment()?.deploymentType;
  const projectEnvironmentVariables = useProjectEnvironmentVariables(
    projectId,
    100,
  );
  if (
    environmentVariables === undefined ||
    projectEnvironmentVariables === undefined ||
    deploymentType === undefined
  ) {
    return {
      status: "loading",
    };
  }
  return diffEnvironmentVariables(
    projectEnvironmentVariables,
    environmentVariables,
    deploymentType,
  );
}

function useRequestedEnvVars() {
  const router = useRouter();
  const varParam = router.query.var;
  const values =
    varParam === undefined
      ? []
      : Array.isArray(varParam)
        ? varParam.map((name) => ({ name, value: "" }))
        : [{ name: varParam, value: "" }];

  useEffect(() => {
    if (router.query.var !== undefined) {
      const url = new URL(window.location.href);
      url.searchParams.delete("var");
      window.history.replaceState({}, "", url.toString());
    }
  }, [router]);

  return values;
}
