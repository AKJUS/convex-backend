import { render, screen } from "@testing-library/react";
import { ConvexProvider } from "convex/react";
import mockRouter from "next-router-mock";
import udfs from "udfs";
import { EnvironmentVariable } from "system-udfs/convex/_system/frontend/common";
import {
  DeploymentEnvironmentVariables,
  diffEnvironmentVariables,
} from "features/settings/components/DeploymentEnvironmentVariables";
import { mockConvexReactClient } from "lib/mockConvexReactClient";
import {
  ConnectedDeploymentContext,
  DeploymentInfo,
  DeploymentInfoContext,
} from "lib/deploymentContext";
import { ProjectEnvVarConfig } from "features/settings/lib/types";

const deploymentInfo: DeploymentInfo = {
  ok: true,
  deploymentUrl: process.env.NEXT_PUBLIC_DEPLOYMENT_URL!,
  adminKey: process.env.NEXT_PUBLIC_ADMIN_KEY!,
  useCurrentTeam: () => ({
    id: 0,
    name: "Team",
    slug: "team",
  }),
  useTeamMembers: () => [],
  useTeamEntitlements: () => ({
    auditLogsEnabled: true,
  }),
  useCurrentUsageBanner: () => null,
  useCurrentProject: () => ({
    id: 0,
    name: "Project",
    slug: "project",
    teamId: 0,
  }),
  useCurrentDeployment: () => ({
    id: 0,
    name: "local",
    deploymentType: "prod",
    projectId: 0,
    kind: "local",
    previewIdentifier: null,
  }),
  useHasProjectAdminPermissions: () => true,
  useIsDeploymentPaused: () => false,
  useProjectEnvironmentVariables: () => ({ configs: [] }),
  CloudImport: ({ sourceCloudBackupId }: { sourceCloudBackupId: number }) => (
    <div>{sourceCloudBackupId}</div>
  ),
  useLogDeploymentEvent: () => () => {},
  TeamMemberLink: () => <div />,
  useTeamUsageState: () => "Default",
  teamsURI: "/",
  projectsURI: "/",
  deploymentsURI: "/",
  isSelfHosted: true,
};

const createEnvironmentVariable = (
  name: string,
  value: string,
): EnvironmentVariable => ({ name, value }) as EnvironmentVariable;

describe("DeploymentEnvironmentVariables", () => {
  describe("diffEnvironmentVariables", () => {
    it("exaclty same", () => {
      const projectLevelEnvVarDefaults: { configs: ProjectEnvVarConfig[] } = {
        configs: [
          {
            name: "name1",
            value: "value1",
            deploymentTypes: ["prod", "dev", "preview"],
          },
          {
            name: "name2",
            value: "value2",
            deploymentTypes: ["prod", "dev", "preview"],
          },
        ],
      };

      const deploymentEnvVariables = [
        createEnvironmentVariable("name1", "value1"),
        createEnvironmentVariable("name2", "value2"),
      ];

      const result = diffEnvironmentVariables(
        projectLevelEnvVarDefaults,
        deploymentEnvVariables,
        "dev",
      );
      expect(result).toEqual({ status: "same" });
    });

    it("deployment has more", () => {
      const projectLevelEnvVarDefaults: { configs: ProjectEnvVarConfig[] } = {
        configs: [
          {
            name: "name1",
            value: "value1",
            deploymentTypes: ["prod", "dev", "preview"],
          },
          {
            name: "name2",
            value: "value2",
            deploymentTypes: ["prod", "dev", "preview"],
          },
        ],
      };

      const deploymentEnvVariables = [
        createEnvironmentVariable("name1", "value1"),
        createEnvironmentVariable("name2", "value2"),
        createEnvironmentVariable("name3", "value3"),
      ];

      const result = diffEnvironmentVariables(
        projectLevelEnvVarDefaults,
        deploymentEnvVariables,
        "dev",
      );
      expect(result).toEqual({ status: "same" });
    });

    it("different", () => {
      const projectLevelEnvVarDefaults: { configs: ProjectEnvVarConfig[] } = {
        configs: [
          {
            name: "name1",
            value: "value1",
            deploymentTypes: ["prod", "dev", "preview"],
          },
          {
            name: "name2",
            value: "value2",
            deploymentTypes: ["prod", "dev", "preview"],
          },
        ],
      };

      const deploymentEnvVariables = [
        createEnvironmentVariable("name1", "value1"),
        createEnvironmentVariable("name2", "value3"),
      ];

      const result = diffEnvironmentVariables(
        projectLevelEnvVarDefaults,
        deploymentEnvVariables,
        "dev",
      );
      expect(result).toEqual({
        status: "different",
        projectEnvVariables: new Map([
          ["name1", "value1"],
          ["name2", "value2"],
        ]),
      });
    });

    it("respects deployment type", () => {
      const projectLevelEnvVarDefaults: { configs: ProjectEnvVarConfig[] } = {
        configs: [
          {
            name: "name1",
            value: "value1",
            deploymentTypes: ["prod", "dev", "preview"],
          },
          {
            name: "name2",
            value: "value2",
            deploymentTypes: ["preview"],
          },
        ],
      };

      const deploymentEnvVariables = [
        createEnvironmentVariable("name1", "value1"),
        createEnvironmentVariable("name2", "value3"),
      ];

      const result = diffEnvironmentVariables(
        projectLevelEnvVarDefaults,
        deploymentEnvVariables,
        "dev",
      );
      expect(result).toEqual({ status: "same" });
    });
  });
});

jest.mock("next/router", () => jest.requireActual("next-router-mock"));
describe("Prefilling env var name", () => {
  const mockClient = mockConvexReactClient().registerQueryFake(
    udfs.listEnvironmentVariables.default,
    () => [],
  );

  it("prefills one variable", async () => {
    mockRouter.setCurrentUrl("/some-url");
    mockRouter.query = { var: "MICHAL" };
    renderUI();
    const inputs = await screen.findAllByRole("textbox");
    expect(inputs).toHaveLength(2);
    expect(inputs[0]).toHaveValue("MICHAL");
    expect(inputs[1]).toHaveValue("");
  });

  it("prefills two variables", async () => {
    mockRouter.setCurrentUrl("/some-url");
    mockRouter.query = { var: ["MICHAL", "JAMES"] };
    renderUI();
    const inputs = await screen.findAllByRole("textbox");
    expect(inputs).toHaveLength(4);
    expect(inputs[0]).toHaveValue("MICHAL");
    expect(inputs[1]).toHaveValue("");
    expect(inputs[2]).toHaveValue("JAMES");
    expect(inputs[3]).toHaveValue("");
  });

  function renderUI() {
    render(
      <DeploymentInfoContext.Provider value={deploymentInfo}>
        <ConvexProvider client={mockClient}>
          <ConnectedDeploymentContext.Provider
            value={{ deployment: {} } as any}
          >
            <DeploymentEnvironmentVariables />
          </ConnectedDeploymentContext.Provider>
        </ConvexProvider>
        ,
      </DeploymentInfoContext.Provider>,
    );
  }
});
